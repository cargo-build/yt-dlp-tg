//! Durable typed Telegram file-ID index backed by redb. @req DL-9

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Single durable table: encoded key bytes to serialized [`CachedFile`] bytes.
const FILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("files");

/// Engine page-cache budget; the index holds only small metadata records.
const CACHE_SIZE_BYTES: usize = 8 * 1024 * 1024;

/// Telegram media type a cached file ID was produced for. @req DL-4
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileKind {
    /// Voice message from the Opus pipeline.
    Voice,
    /// Music audio message from the M4A pipeline.
    Audio,
    /// Playable video message within the resolution cap.
    Video,
    /// Legacy document IDs remain readable during an output-policy upgrade.
    Document,
}

/// Validated Telegram response identity for one delivered media file. @req DL-9
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedFile {
    /// Delivery type Telegram accepted.
    pub kind: FileKind,
    /// Reusable `file_id`; never the cross-bot `file_unique_id`.
    pub file_id: String,
}

/// Durable lookup key for one delivered output. @req DL-9
#[derive(Debug, Clone)]
pub struct Key {
    /// Numeric bot ID that uploaded the file: file IDs are bot-scoped.
    pub bot_id: u64,
    /// Canonical selected source URL.
    pub source: String,
    /// Output-policy version that produced the file.
    pub variant: String,
}

/// Durable index of successful typed uploads. @req DL-9
#[derive(Clone)]
pub struct Cache {
    database: Arc<Database>,
}

impl Cache {
    /// Share the already-open state file with the durable request store. @req DL-14
    pub(crate) fn shared_database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// Opens a private durable index without blocking the executor. @req DL-9
    pub async fn open(path: PathBuf) -> Result<Self> {
        let database = run_blocking("open", move || {
            if let Some(parent) = path.parent() {
                create_private_dirs(parent).map_err(|error| cache_error("open", error))?;
            }
            let file = open_database_file(&path).map_err(|error| cache_error("open", error))?;
            #[cfg(unix)]
            set_owner_only(&path).map_err(|error| cache_error("open", error))?;
            let mut builder = Database::builder();
            builder.set_cache_size(CACHE_SIZE_BYTES);
            let database = builder
                .create_file(file)
                .map_err(|error| cache_error("open", error))?;
            let write_txn = database
                .begin_write()
                .map_err(|error| cache_error("open", error))?;
            {
                write_txn
                    .open_table(FILES)
                    .map_err(|error| cache_error("open", error))?;
            }
            write_txn
                .commit()
                .map_err(|error| cache_error("open", error))?;
            Ok(Arc::new(database))
        })
        .await?;
        Ok(Self { database })
    }

    /// Distinguishes absent IDs from storage or decoding failures. @req DL-9
    pub async fn get(&self, key: Key) -> Result<Option<CachedFile>> {
        let encoded = encode_key(&key)?;
        let database = Arc::clone(&self.database);
        run_blocking("read", move || {
            let read_txn = database
                .begin_read()
                .map_err(|error| cache_error("read", error))?;
            let table = read_txn
                .open_table(FILES)
                .map_err(|error| cache_error("read", error))?;
            let record = table
                .get(encoded.as_slice())
                .map_err(|error| cache_error("read", error))?;
            match record {
                Some(record) => decode_file(record.value()).map(Some),
                None => Ok(None),
            }
        })
        .await
    }

    /// Commits a validated typed ID before returning success. @req DL-9
    pub async fn put(&self, key: Key, file: CachedFile) -> Result<()> {
        let encoded_key = encode_key(&key)?;
        let encoded_file = encode_file(&file)?;
        let database = Arc::clone(&self.database);
        run_blocking("write", move || {
            let write_txn = database
                .begin_write()
                .map_err(|error| cache_error("write", error))?;
            {
                let mut table = write_txn
                    .open_table(FILES)
                    .map_err(|error| cache_error("write", error))?;
                drop(
                    table
                        .insert(encoded_key.as_slice(), encoded_file.as_slice())
                        .map_err(|error| cache_error("write", error))?,
                );
            }
            write_txn
                .commit()
                .map_err(|error| cache_error("write", error))
        })
        .await
    }

    /// Removes a rejected ID; a missing key is harmless. @req DL-9
    pub async fn remove(&self, key: Key) -> Result<()> {
        let encoded = encode_key(&key)?;
        let database = Arc::clone(&self.database);
        run_blocking("remove", move || {
            let write_txn = database
                .begin_write()
                .map_err(|error| cache_error("remove", error))?;
            {
                let mut table = write_txn
                    .open_table(FILES)
                    .map_err(|error| cache_error("remove", error))?;
                drop(
                    table
                        .remove(encoded.as_slice())
                        .map_err(|error| cache_error("remove", error))?,
                );
            }
            write_txn
                .commit()
                .map_err(|error| cache_error("remove", error))
        })
        .await
    }
}

/// Runs one redb operation on compio's blocking pool, propagating worker failure.
pub(crate) async fn run_blocking<T: Send + 'static>(
    operation: &'static str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    match compio::runtime::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(Error::Cache(format!("{operation} worker failed: {error}"))),
    }
}

/// Builds the fixed operation-context cache error.
pub(crate) fn cache_error(operation: &'static str, error: impl std::fmt::Display) -> Error {
    Error::Cache(format!("{operation}: {error}"))
}

/// Validates a key and encodes it as an unambiguous JSON tuple of its parts.
fn encode_key(key: &Key) -> Result<Vec<u8>> {
    validate_key(key)?;
    serde_json::to_vec(&(key.bot_id, key.source.as_str(), key.variant.as_str()))
        .map_err(|error| cache_error("key encode", error))
}

/// Serializes a validated record for storage.
fn encode_file(file: &CachedFile) -> Result<Vec<u8>> {
    validate_file(file)?;
    serde_json::to_vec(file).map_err(|error| cache_error("record encode", error))
}

/// Decodes a stored record, rejecting data that no longer satisfies the contract.
fn decode_file(bytes: &[u8]) -> Result<CachedFile> {
    let file: CachedFile =
        serde_json::from_slice(bytes).map_err(|error| cache_error("record decode", error))?;
    validate_file(&file)?;
    Ok(file)
}

fn validate_key(key: &Key) -> Result<()> {
    if key.bot_id == 0 {
        return Err(Error::Cache("key bot ID must be positive".into()));
    }
    if key.source.trim().is_empty() {
        return Err(Error::Cache("key source must not be empty".into()));
    }
    if key.variant.trim().is_empty() {
        return Err(Error::Cache("key variant must not be empty".into()));
    }
    Ok(())
}

fn validate_file(file: &CachedFile) -> Result<()> {
    if file.file_id.trim().is_empty() {
        return Err(Error::Cache("record file ID must not be empty".into()));
    }
    Ok(())
}

/// Creates only the missing ancestors of `parent`, each owner-only.
///
/// An empty parent means the current directory and needs no creation.
fn create_private_dirs(parent: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = parent;
    while !current.as_os_str().is_empty() && !current.exists() {
        missing.push(current.to_path_buf());
        let Some(next) = current.parent() else { break };
        current = next;
    }
    for directory in missing.iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
    }
    Ok(())
}

/// Opens the database file for reading and writing, creating it owner-only.
fn open_database_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Restricts an existing database file to its owner.
#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use tempfile::TempDir;

    fn key(bot_id: u64, source: &str, variant: &str) -> Key {
        Key {
            bot_id,
            source: source.into(),
            variant: variant.into(),
        }
    }

    fn file(kind: FileKind, file_id: &str) -> CachedFile {
        CachedFile {
            kind,
            file_id: file_id.into(),
        }
    }

    async fn cache_in(directory: &TempDir) -> Cache {
        Cache::open(directory.path().join("files.redb"))
            .await
            .expect("temporary cache opens")
    }

    #[compio::test]
    async fn stores_and_replaces_a_typed_record() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cache = cache_in(&directory).await;
        let stored_key = key(7, "https://example.com/watch?v=1", "voice-opus-v1");
        let voice = file(FileKind::Voice, "voice-file-id");

        cache
            .put(stored_key.clone(), voice.clone())
            .await
            .expect("valid record is stored");
        assert_eq!(
            cache
                .get(stored_key.clone())
                .await
                .expect("valid read")
                .expect("stored record is present"),
            voice
        );

        let video = file(FileKind::Video, "video-file-id");
        cache
            .put(stored_key.clone(), video.clone())
            .await
            .expect("replacement record is stored");
        assert_eq!(
            cache
                .get(stored_key)
                .await
                .expect("valid read")
                .expect("replacement record is present"),
            video
        );
    }

    #[compio::test]
    async fn reopened_database_keeps_records() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory
            .path()
            .join("state")
            .join("nested")
            .join("files.redb");
        let stored_key = key(11, "https://example.com/track", "music-m4a-v1");
        let music = file(FileKind::Audio, "audio-file-id");

        {
            let cache = Cache::open(path.clone()).await.expect("cache opens");
            cache
                .put(stored_key.clone(), music.clone())
                .await
                .expect("record is stored");
        }

        let cache = Cache::open(path).await.expect("cache reopens");
        assert_eq!(
            cache
                .get(stored_key)
                .await
                .expect("valid read")
                .expect("record survived the reopen"),
            music
        );
    }

    #[compio::test]
    async fn keys_are_isolated_across_bots_variants_and_delimiter_looking_sources() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cache = cache_in(&directory).await;
        let entries = [
            (
                key(1, "https://example.com/a", "b|c"),
                file(FileKind::Voice, "first"),
            ),
            (
                key(1, "https://example.com/a|b", "c"),
                file(FileKind::Audio, "second"),
            ),
            (
                key(2, "https://example.com/a", "b|c"),
                file(FileKind::Video, "third"),
            ),
            (
                key(1, "https://example.com/a:b|c?x=1&y=2", "voice-opus-v1"),
                file(FileKind::Document, "fourth"),
            ),
        ];

        for (entry_key, entry_file) in entries.clone() {
            cache
                .put(entry_key, entry_file)
                .await
                .expect("distinct key is stored");
        }
        for (entry_key, entry_file) in entries {
            let stored = cache
                .get(entry_key.clone())
                .await
                .expect("valid read")
                .unwrap_or_else(|| panic!("record for {entry_key:?} is present"));
            assert_eq!(stored, entry_file, "record for {entry_key:?}");
        }
        assert!(
            cache
                .get(key(1, "https://example.com/a", "voice-opus-v1"))
                .await
                .expect("valid read")
                .is_none(),
            "another output variant stays absent"
        );
    }

    #[compio::test]
    async fn removal_is_idempotent_and_missing_keys_are_absent() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cache = cache_in(&directory).await;
        let present = key(5, "https://example.com/x", "music-m4a-v1");

        cache
            .put(present.clone(), file(FileKind::Audio, "audio-file-id"))
            .await
            .expect("record is stored");
        cache
            .remove(present.clone())
            .await
            .expect("existing record is removed");
        assert!(
            cache
                .get(present.clone())
                .await
                .expect("valid read")
                .is_none(),
            "removed record is absent"
        );
        cache
            .remove(present)
            .await
            .expect("removing a missing record succeeds");
        assert!(
            cache
                .get(key(5, "https://example.com/x", "video-1080-v1"))
                .await
                .expect("valid read")
                .is_none(),
            "never stored key is absent"
        );
    }

    #[compio::test]
    async fn rejects_invalid_required_data() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cache = cache_in(&directory).await;
        let valid = key(9, "https://example.com/y", "voice-opus-v1");

        for invalid in [
            key(0, "https://example.com/y", "voice-opus-v1"),
            key(9, "", "voice-opus-v1"),
            key(9, "https://example.com/y", ""),
        ] {
            assert!(
                matches!(cache.get(invalid.clone()).await, Err(Error::Cache(_))),
                "get rejects {invalid:?}"
            );
            assert!(
                matches!(
                    cache
                        .put(invalid.clone(), file(FileKind::Voice, "file-id"))
                        .await,
                    Err(Error::Cache(_))
                ),
                "put rejects {invalid:?}"
            );
            assert!(
                matches!(cache.remove(invalid.clone()).await, Err(Error::Cache(_))),
                "remove rejects {invalid:?}"
            );
        }
        assert!(
            matches!(
                cache.put(valid.clone(), file(FileKind::Voice, "")).await,
                Err(Error::Cache(_))
            ),
            "an empty file ID is rejected"
        );
        assert!(
            cache.get(valid).await.expect("valid read").is_none(),
            "rejected records are not stored"
        );
    }

    #[compio::test]
    async fn corrupt_records_are_errors_not_misses() {
        let directory = tempfile::tempdir().expect("temp directory");
        let cache = cache_in(&directory).await;
        let stored_key = key(3, "https://example.com/c", "voice-opus-v1");
        let encoded = encode_key(&stored_key).expect("valid key encodes");
        let database = Arc::clone(&cache.database);
        let garbage: &[u8] = b"not a cached file";

        run_blocking("test", move || {
            let write_txn = database
                .begin_write()
                .map_err(|error| cache_error("test", error))?;
            {
                let mut table = write_txn
                    .open_table(FILES)
                    .map_err(|error| cache_error("test", error))?;
                drop(
                    table
                        .insert(encoded.as_slice(), garbage)
                        .map_err(|error| cache_error("test", error))?,
                );
            }
            write_txn
                .commit()
                .map_err(|error| cache_error("test", error))
        })
        .await
        .expect("raw record is written");

        assert!(
            matches!(cache.get(stored_key).await, Err(Error::Cache(_))),
            "an undecodable record must not look absent"
        );
    }

    #[test]
    fn a_bare_file_name_needs_no_directory_creation() {
        assert!(
            create_private_dirs(Path::new("")).is_ok(),
            "an empty parent means the current directory"
        );
        assert!(
            create_private_dirs(Path::new(".")).is_ok(),
            "the current directory already exists"
        );
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path)
            .expect("path metadata is readable")
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    #[compio::test]
    async fn creates_private_directories_and_database_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let parent = directory.path().join("state").join("yt-dlp-tg");
        let path = parent.join("files.redb");
        let _cache = Cache::open(path.clone()).await.expect("cache opens");

        assert_eq!(
            mode(&directory.path().join("state")),
            0o700,
            "missing ancestors are owner-only"
        );
        assert_eq!(mode(&parent), 0o700, "nested ancestors are owner-only");
        assert_eq!(mode(&path), 0o600, "database file is owner-only");
    }
}
