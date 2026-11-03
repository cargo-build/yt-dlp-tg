//! Load one capped output into RAM and count subsequent body demand. @req DL-11/DL-8

use std::io::Cursor;

use compio::{BufResult, buf::{IntoInner, IoBuf, IoBufMut}, fs::File, io::{AsyncRead, AsyncReadAt, AsyncReadAtExt}};

use crate::{error::{Error, Result}, progress::Progress, telemetry::transfer::Upload};

/// A bounded exact read avoids geometric growth and rejects changed input sizes. @req DL-8 DL-11
pub(crate) async fn load(file: File, expected: u64, limit: u64) -> Result<Vec<u8>> {
    match expected {
        0 => return Err(Error::YtDlp("upload input is empty".into())),
        bytes if bytes > limit => return Err(Error::TooLarge),
        _ => {}
    }
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() != expected {
        return Err(Error::YtDlp("upload input changed before buffering".into()));
    }
    let size = usize::try_from(expected)
        .map_err(|_| Error::Process("upload buffer exceeds address space".into()))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size)
        .map_err(|_| Error::Process("cannot allocate upload buffer".into()))?;
    let BufResult(read, bytes) = file.read_exact_at(bytes.slice(..size), 0).await;
    read?;
    let bytes = bytes.into_inner();
    let BufResult(extra, _) = file.read_at(Vec::with_capacity(1), expected).await;
    if extra? != 0 || file.metadata().await?.len() != expected {
        return Err(Error::YtDlp("upload input changed while buffering".into()));
    }
    Ok(bytes)
}

/// Moving the buffer keeps chunked progress without another file read or clone. @req DL-11/DL-8
pub(crate) fn counted(
    bytes: Vec<u8>,
    progress: Progress,
    diagnostics: Upload,
) -> impl AsyncRead + 'static {
    Counted {
        reader: Cursor::new(bytes),
        progress,
        diagnostics,
    }
}

struct Counted {
    reader: Cursor<Vec<u8>>,
    progress: Progress,
    diagnostics: Upload,
}

impl AsyncRead for Counted {
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        self.diagnostics.read_started();
        let BufResult(result, buffer) = self.reader.read(buffer).await;
        self.diagnostics.read_finished(&result);
        BufResult(
            result.inspect(|bytes| self.progress.uploaded(*bytes)),
            buffer,
        )
    }
}

#[cfg(test)]
mod tests {
    use compio::io::AsyncReadExt;

    use super::*;
    use crate::i18n::Language;

    #[compio::test]
    async fn full_buffer_survives_source_deletion_and_counts_actual_body_reads() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("input");
        let content = b"native upload bytes";
        std::fs::write(&path, content).expect("write fixture");
        let bytes = load(File::open(&path).await.expect("open fixture"), content.len() as u64, content.len() as u64)
            .await.expect("load exact cap");
        std::fs::remove_file(&path).expect("remove original before any body demand");
        let progress = Progress::new(None);
        progress.sending(Some(content.len() as u64));
        let mut reader = counted(
            bytes,
            progress.clone(),
            Upload::new(content.len() as u64),
        );
        let BufResult(read, first) = reader.read(Vec::with_capacity(4)).await;
        assert_eq!(read.expect("first read"), 4);
        assert_eq!(first, b"nati");
        assert!(
            progress
                .render(Language::from_code(Some("en")))
                .contains("4 B / 19 B")
        );
        let BufResult(read, rest) = reader.read_to_end(Vec::new()).await;
        assert_eq!(read.expect("remaining reads"), content.len() - first.len());
        assert_eq!([first, rest].concat(), content);
        let rendered = progress.render(Language::from_code(Some("en")));
        assert!(rendered.contains("100% · 19 B / 19 B"));
        let BufResult(read, buffer) = reader.read(Vec::with_capacity(4)).await;
        assert_eq!(read.expect("EOF read"), 0);
        assert!(buffer.is_empty());
        assert!(
            progress
                .render(Language::from_code(Some("en")))
                .contains("19 B / 19 B")
        );
    }

    #[compio::test]
    async fn rejects_empty_oversized_changed_and_non_file_inputs() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("input");
        std::fs::write(&path, b"12345").expect("write fixture");
        for (expected, limit) in [(0, 5), (6, 5), (4, 8), (6, 8)] {
            let result = load(File::open(&path).await.expect("open fixture"), expected, limit).await;
            assert!(result.is_err(), "invalid expected length {expected}/{limit}");
            if expected > limit { assert!(matches!(result, Err(Error::TooLarge))); }
        }
        let file = File::open(directory.path()).await.expect("open directory fixture");
        assert!(load(file, 1, 5).await.is_err());
    }
}
