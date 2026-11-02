//! Bounded child pipes, deadlines and process-group cleanup. @req DL-8 DL-12

use crate::error::{Error, Result};
use compio::{BufResult, io::AsyncRead, process::Command};
use futures_util::future::{Either, join3, select};
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Clone, Copy)]
pub struct DirectoryLimit<'a> {
    pub path: &'a Path,
    pub file_bytes: u64,
    pub total_bytes: u64,
}

/// Drain a child with bounded output; kill and reap its group on timeout. @req DL-8 DL-12
pub async fn run(command: &mut Command, deadline: Duration, limit: usize) -> Result<Vec<u8>> {
    run_inner(command, deadline, limit, None, None).await
}

/// Bound individual files and remux scratch while retaining child-group cleanup. @req DL-5 DL-8
pub async fn run_guarded(
    command: &mut Command,
    deadline: Duration,
    limit: usize,
    directory: DirectoryLimit<'_>,
    observer: Option<&dyn Fn(&[u8])>,
) -> Result<Vec<u8>> {
    run_inner(command, deadline, limit, Some(directory), observer).await
}

async fn run_inner(
    command: &mut Command,
    deadline: Duration,
    limit: usize,
    directory: Option<DirectoryLimit<'_>>,
    observer: Option<&dyn Fn(&[u8])>,
) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .expect("Stdio conversion is infallible");
    command
        .stdout(Stdio::piped())
        .expect("Stdio conversion is infallible");
    command
        .stderr(Stdio::piped())
        .expect("Stdio conversion is infallible");
    command.process_group(0);
    let mut child = command.spawn()?;
    let process_id = i32::try_from(child.id())
        .map_err(|_| Error::Process("invalid subprocess identifier".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Process("subprocess stdout is unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Process("subprocess stderr is unavailable".into()))?;
    let collect = join3(
        child.wait(),
        read_pipe(stdout, limit, observer),
        read_pipe(stderr, 0, None),
    );
    let collect = std::pin::pin!(collect);
    let stop = async {
        let watch = async {
            match directory {
                Some(directory) => watch_directory(directory).await,
                None => futures_util::future::pending::<Error>().await,
            }
        };
        match select(
            std::pin::pin!(compio::time::sleep(deadline)),
            std::pin::pin!(watch),
        )
        .await
        {
            Either::Left(_) => Error::Timeout,
            Either::Right((error, _)) => error,
        }
    };
    let (status, stdout, stderr) = match select(collect, std::pin::pin!(stop)).await {
        Either::Left((output, _)) => output,
        Either::Right((error, collect)) => {
            let killed = kill_group(process_id);
            let _reaped = collect.await;
            killed?;
            return Err(error);
        }
    };
    let status = status?;
    let (stdout, overflow) = stdout?;
    stderr?;
    match (status.success(), overflow) {
        (false, _) => Err(Error::ChildExit(status.code())),
        (true, true) => Err(Error::Process("subprocess output exceeds its limit".into())),
        (true, false) => Ok(stdout),
    }
}

async fn watch_directory(limit: DirectoryLimit<'_>) -> Error {
    loop {
        let path = limit.path.to_owned();
        let maximum = limit.file_bytes;
        let total = limit.total_bytes;
        match compio::runtime::spawn_blocking(move || check_directory(&path, maximum, total)).await
        {
            Ok(Ok(())) => compio::time::sleep(Duration::from_millis(100)).await,
            Ok(Err(error)) => return error,
            Err(_) => return Error::Process("download disk check failed".into()),
        }
    }
}

fn check_directory(root: &Path, file_bytes: u64, total_bytes: u64) -> Result<()> {
    let mut directories: Vec<PathBuf> = vec![root.to_owned()];
    let mut total = 0_u64;
    while let Some(path) = directories.pop() {
        let mut entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        entries.try_for_each(|entry| -> Result<()> {
            let entry = entry?;
            let metadata = match entry.path().symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if metadata.is_symlink() {
                return Err(Error::Process("unexpected download symlink".into()));
            }
            if metadata.is_dir() {
                directories.push(entry.path());
                return Ok(());
            }
            total = total.checked_add(metadata.len()).ok_or(Error::TooLarge)?;
            if metadata.len() > file_bytes || total > total_bytes {
                return Err(Error::TooLarge);
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn kill_group(process_id: i32) -> io::Result<()> {
    // SAFETY: a positive child PID names our dedicated process group; no pointers are used.
    let result = unsafe { libc::kill(-process_id, libc::SIGKILL) };
    match result {
        0 => Ok(()),
        _ => {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => Ok(()),
                _ => Err(error),
            }
        }
    }
}

struct Lines<'a> {
    observer: &'a dyn Fn(&[u8]),
    pending: Vec<u8>,
    dropping: bool,
}

impl<'a> Lines<'a> {
    fn new(observer: &'a dyn Fn(&[u8])) -> Self {
        Self { observer, pending: Vec::new(), dropping: false }
    }

    fn push(&mut self, bytes: &[u8]) {
        bytes.iter().for_each(|byte| match byte {
            b'\n' => {
                self.finish();
                self.dropping = false;
            }
            _ if self.dropping => {}
            _ if self.pending.len() < 1024 => self.pending.push(*byte),
            _ => {
                self.pending.clear();
                self.dropping = true;
            }
        });
    }

    fn finish(&mut self) {
        if !self.dropping && !self.pending.is_empty() {
            (self.observer)(self.pending.strip_suffix(b"\r").unwrap_or(&self.pending));
        }
        self.pending.clear();
    }
}

async fn read_pipe(
    mut reader: impl AsyncRead,
    limit: usize,
    observer: Option<&dyn Fn(&[u8])>,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut overflow = false;
    let mut lines = observer.map(Lines::new);
    loop {
        let BufResult(read, buffer) = reader.read([0_u8; 8192]).await;
        let read = read?;
        match read {
            0 => {
                if let Some(lines) = &mut lines { lines.finish(); }
                return Ok((output, overflow));
            }
            _ => {
                if let Some(lines) = &mut lines { lines.push(&buffer[..read]); }
                let kept = read.min(limit.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..kept]);
                overflow |= kept < read;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn download_guard_aborts_an_oversized_child_and_its_descendant() {
        let directory = tempfile::tempdir().expect("private test directory");
        let mut command = Command::new("python3");
        command.args(["-c", "import pathlib,subprocess,sys,time; p=pathlib.Path(sys.argv[1]); child=subprocess.Popen(['sleep','30']); (p/'pid').write_text(str(child.pid)); (p/'large').write_bytes(b'x'*65); time.sleep(30)"])
            .arg(directory.path());
        let started = std::time::Instant::now();
        assert!(matches!(
            run_guarded(
                &mut command,
                Duration::from_secs(5),
                1024,
                DirectoryLimit {
                    path: directory.path(),
                    file_bytes: 64,
                    total_bytes: 192
                },
                None,
            )
            .await,
            Err(Error::TooLarge)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "disk guard should not wait for the child deadline"
        );
        let pid =
            std::fs::read_to_string(directory.path().join("pid")).expect("descendant PID recorded");
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            assert_eq!(
                stat.split_whitespace().nth(2),
                Some("Z"),
                "descendant must not remain running"
            );
        }
    }

    #[compio::test]
    async fn download_guard_allows_inputs_and_remux_output_to_coexist() {
        let directory = tempfile::tempdir().expect("private test directory");
        let mut command = Command::new("python3");
        command.args(["-c", "import pathlib,sys,time; p=pathlib.Path(sys.argv[1]); [(p/str(i)).write_bytes(b'x'*768) for i in range(3)]; sys.stderr.write('x'*100000); time.sleep(.15); print('done')"])
            .arg(directory.path());
        let output = run_guarded(
            &mut command,
            Duration::from_secs(5),
            1024,
            DirectoryLimit {
                path: directory.path(),
                file_bytes: 1024,
                total_bytes: 3072,
            },
            None,
        )
        .await
        .expect("valid temporary copies fit the disk budget");
        assert_eq!(output, b"done\n");
        std::fs::write(directory.path().join("extra"), vec![0_u8; 1024]).expect("extra file");
        assert!(
            matches!(
                check_directory(directory.path(), 1024, 3072),
                Err(Error::TooLarge)
            ),
            "aggregate temporary space must also be bounded"
        );
        assert!(
            matches!(
                check_directory(directory.path(), 3072, 3072),
                Err(Error::TooLarge)
            ),
            "room for extraction inputs must not increase the aggregate budget"
        );
    }

    #[compio::test]
    async fn observes_bounded_stdout_lines_before_child_exit_without_exposing_stderr() {
        use std::cell::RefCell;
        let directory = tempfile::tempdir().expect("private child fixture");
        let lines = RefCell::new(Vec::new());
        let marker = directory.path().join("continue");
        let observe = |line: &[u8]| {
            lines.borrow_mut().push(line.to_vec());
            if line == b"frame|1" { std::fs::write(&marker, b"continue").expect("release fixture"); }
        };
        let mut command = Command::new("python3");
        command.args(["-c", "import sys,time,pathlib; print('frame|1',flush=True)\nwhile not pathlib.Path(sys.argv[1]).exists(): time.sleep(.01)\nprint('x'*2048); sys.stderr.write('private\\n'); print(chr(34)+'/tmp/media.m4a'+chr(34))"]).arg(&marker);
        let output = run_guarded(
            &mut command,
            Duration::from_secs(3),
            4096,
            DirectoryLimit { path: directory.path(), file_bytes: 4096, total_bytes: 4096 },
            Some(&observe),
        ).await.expect("observer releases child before its deadline");
        assert_eq!(lines.borrow().first(), Some(&b"frame|1".to_vec()));
        assert!(output.ends_with(b"\"/tmp/media.m4a\"\n"));
        assert_eq!(lines.borrow().len(), 2, "oversized line and stderr are excluded");
    }

    #[compio::test]
    async fn drains_stderr_and_bounds_stdout() {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import sys; sys.stderr.write('x'*100000); sys.stdout.write('y'*100000)",
        ]);
        assert!(matches!(
            run(&mut command, Duration::from_secs(5), 64).await,
            Err(Error::Process(_))
        ));
    }

    #[compio::test]
    async fn preserves_exit_class_without_returning_stderr() {
        let mut command = Command::new("python3");
        command.args(["-c", "import sys; sys.stderr.write('private'); sys.exit(2)"]);
        assert!(matches!(
            run(&mut command, Duration::from_secs(5), 64).await,
            Err(Error::ChildExit(Some(2)))
        ));
    }

    #[compio::test]
    async fn subprocess_preserves_literal_arguments() {
        let mut command = Command::new("printf");
        command.args(["%s", "--literal; $(never-executed)"]);
        let output = run(&mut command, Duration::from_secs(2), 1024)
            .await
            .expect("printf succeeds");
        assert_eq!(output, b"--literal; $(never-executed)");
    }

    #[compio::test]
    async fn subprocess_timeout_kills_and_reaps() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = std::time::Instant::now();
        assert!(matches!(
            run(&mut command, Duration::from_millis(20), 1024).await,
            Err(Error::Timeout)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must not wait for the original process duration"
        );
    }
}
