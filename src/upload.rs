//! Count file bytes at completed reads without buffering the upload. @req DL-11/DL-8

use std::io::Cursor;

use compio::{BufResult, buf::IoBufMut, fs::File, io::AsyncRead};

use crate::progress::Progress;

/// The local reader preserves owned InputFile streaming on compio. @req DL-11/DL-8
pub(crate) fn counted(file: File, progress: Progress) -> impl AsyncRead + 'static {
    Counted {
        reader: Cursor::new(file),
        progress,
    }
}

struct Counted {
    reader: Cursor<File>,
    progress: Progress,
}

impl AsyncRead for Counted {
    async fn read<B: IoBufMut>(&mut self, buffer: B) -> BufResult<usize, B> {
        let BufResult(result, buffer) = self.reader.read(buffer).await;
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
    async fn completed_reads_stream_the_original_file_and_count_actual_bytes() {
        let directory = tempfile::tempdir().expect("temporary input directory");
        let path = directory.path().join("input");
        let content = b"native upload bytes";
        std::fs::write(&path, content).expect("write fixture");
        let progress = Progress::new(None);
        progress.sending(Some(content.len() as u64));
        let mut reader = counted(
            File::open(&path).await.expect("open fixture"),
            progress.clone(),
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
    async fn failed_reads_preserve_the_buffer_and_do_not_advance_progress() {
        let directory = tempfile::tempdir().expect("temporary directory input");
        let progress = Progress::new(None);
        progress.sending(Some(100));
        let file = File::open(directory.path())
            .await
            .expect("open directory fixture");
        let mut reader = counted(file, progress.clone());
        let buffer = Vec::with_capacity(7);
        let capacity = buffer.capacity();
        let BufResult(read, buffer) = reader.read(buffer).await;
        assert!(read.is_err(), "directories are not readable file payloads");
        assert_eq!(buffer.capacity(), capacity);
        assert!(buffer.is_empty());
        assert!(
            progress
                .render(Language::from_code(Some("en")))
                .contains("0% · 0 B / 100 B")
        );
    }
}
