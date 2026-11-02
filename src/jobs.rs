//! Durable per-recipient request outbox and completion tracking backed by redb. @req DL-14

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{
    cache,
    error::Result,
    i18n::Language,
    media::{self, Format, Media},
};

/// Single versioned jobs table: 32-hex job ID string key to serialized JSON record bytes.
const JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("jobs_v1");

/// Metadata table storing sequence counter and store version invariants.
const META: TableDefinition<&str, u64> = TableDefinition::new("jobs_meta_v1");

/// Monotonically increasing sequence counter key in the metadata table.
const SEQUENCE_KEY: &str = "sequence";

/// Maximum allowed serialized request byte size (16KiB).
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Upper bound for stored JSON record byte length.
const MAX_RECORD_BYTES: usize = 32 * 1024;

/// Bound for random ID collision retry attempts in a write transaction.
const MAX_ID_ATTEMPTS: usize = 16;

/// Keep durable intake within the existing bounded conversation budget. @req DL-14
pub const MAX_PENDING: usize = 256;

/// Validated recipient request payload accepted for durable execution. @req DL-14
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub bot_id: u64,
    pub user_id: u64,
    pub chat_id: i64,
    pub status_message: i32,
    pub language: Language,
    pub media: Media,
    pub format: Format,
    pub policy: String,
}

/// Lifecycle state of a durable download request. @req DL-14
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Pending,
    Published,
    Completed,
}

/// Durable job record tracking a single recipient request. @req DL-14
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub sequence: u64,
    pub submitted_at_ms: u64,
    pub request: Request,
    pub state: State,
}

/// Durable request outbox and completion store on the shared redb instance. @req DL-14
#[derive(Clone, Debug)]
pub struct Store {
    database: Arc<Database>,
    capacity: usize,
}

impl Store {
    /// Opens the request store tables on the shared database handle. @req DL-14
    pub async fn open(database: Arc<Database>, capacity: usize) -> Result<Self> {
        if !(1..=MAX_PENDING).contains(&capacity) {
            return Err(cache::cache_error(
                "open",
                format!("capacity {capacity} out of allowed range 1..=256"),
            ));
        }

        let db = Arc::clone(&database);
        cache::run_blocking("open", move || {
            let write_txn = db
                .begin_write()
                .map_err(|error| cache::cache_error("open", error))?;

            let jobs_len = {
                let jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("open", error))?;
                jobs_table
                    .len()
                    .map_err(|error| cache::cache_error("open", error))?
            };

            {
                let mut meta_table = write_txn
                    .open_table(META)
                    .map_err(|error| cache::cache_error("open", error))?;
                let current_seq = meta_table
                    .get(SEQUENCE_KEY)
                    .map_err(|error| cache::cache_error("open", error))?
                    .map(|guard| guard.value());

                match (jobs_len > 0, current_seq) {
                    (true, None) => {
                        return Err(cache::cache_error(
                            "open",
                            "sequence metadata is missing while jobs already exist",
                        ));
                    }
                    (true, Some(0)) => {
                        return Err(cache::cache_error(
                            "open",
                            "sequence metadata is zero while jobs already exist",
                        ));
                    }
                    (false, None) => {
                        meta_table
                            .insert(SEQUENCE_KEY, 0u64)
                            .map_err(|error| cache::cache_error("open", error))?;
                    }
                    (_, Some(_)) => {}
                }
            }

            write_txn
                .commit()
                .map_err(|error| cache::cache_error("open", error))?;
            Ok(())
        })
        .await?;

        Ok(Self { database, capacity })
    }

    /// Submits a request if capacity allows, returning the accepted job. @req DL-14
    pub async fn submit(&self, request: Request) -> Result<Option<Job>> {
        validate_request(&request)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                cache::cache_error("submit", format!("system clock before Unix epoch: {error}"))
            })?;
        let submitted_at_ms = u64::try_from(now.as_millis()).map_err(|error| {
            cache::cache_error("submit", format!("timestamp overflow: {error}"))
        })?;

        let db = Arc::clone(&self.database);
        let capacity = self.capacity;

        cache::run_blocking("submit", move || {
            let write_txn = db
                .begin_write()
                .map_err(|error| cache::cache_error("submit", error))?;

            // Count all retained rows (including completed-but-unacked) to enforce capacity.
            let current_count = {
                let jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("submit", error))?;
                jobs_table
                    .len()
                    .map_err(|error| cache::cache_error("submit", error))?
            };

            if current_count >= capacity as u64 {
                return Ok(None);
            }

            let next_seq = {
                let mut meta_table = write_txn
                    .open_table(META)
                    .map_err(|error| cache::cache_error("submit", error))?;
                let current_seq = match meta_table
                    .get(SEQUENCE_KEY)
                    .map_err(|error| cache::cache_error("submit", error))?
                {
                    Some(guard) => guard.value(),
                    None => {
                        return Err(cache::cache_error(
                            "submit",
                            "sequence metadata missing in store",
                        ));
                    }
                };
                let next_seq = current_seq
                    .checked_add(1)
                    .ok_or_else(|| cache::cache_error("submit", "job sequence overflow"))?;
                meta_table
                    .insert(SEQUENCE_KEY, next_seq)
                    .map_err(|error| cache::cache_error("submit", error))?;
                next_seq
            };

            let job = {
                let mut jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("submit", error))?;

                let mut unique_id = None;
                for _ in 0..MAX_ID_ATTEMPTS {
                    let candidate = generate_id();
                    let exists = jobs_table
                        .get(candidate.as_str())
                        .map_err(|error| cache::cache_error("submit", error))?
                        .is_some();
                    if !exists {
                        unique_id = Some(candidate);
                        break;
                    }
                }

                let id = match unique_id {
                    Some(id) => id,
                    None => {
                        return Err(cache::cache_error(
                            "submit",
                            "failed to generate unique job ID after retries",
                        ));
                    }
                };

                let job = Job {
                    id: id.clone(),
                    sequence: next_seq,
                    submitted_at_ms,
                    request,
                    state: State::Pending,
                };
                let encoded = encode_job(&job)?;
                jobs_table
                    .insert(id.as_str(), encoded.as_slice())
                    .map_err(|error| cache::cache_error("submit", error))?;
                job
            };

            write_txn
                .commit()
                .map_err(|error| cache::cache_error("submit", error))?;

            Ok(Some(job))
        })
        .await
    }

    /// Returns all pending jobs in ascending FIFO sequence order. @req DL-14
    pub async fn pending(&self) -> Result<Vec<Job>> {
        Ok(self
            .retained()
            .await?
            .into_iter()
            .filter(|job| job.state == State::Pending)
            .collect())
    }

    /// Replay retained rows once after startup, including completed acknowledgements. @req DL-14
    pub async fn retained(&self) -> Result<Vec<Job>> {
        let db = Arc::clone(&self.database);
        cache::run_blocking("pending", move || {
            let read_txn = db
                .begin_read()
                .map_err(|error| cache::cache_error("pending", error))?;
            let jobs_table = read_txn
                .open_table(JOBS)
                .map_err(|error| cache::cache_error("pending", error))?;

            let mut pending_jobs = jobs_table
                .iter()
                .map_err(|error| cache::cache_error("pending", error))?
                .map(|entry_result| {
                    let (key, value) =
                        entry_result.map_err(|error| cache::cache_error("pending", error))?;
                    decode_job(key.value(), value.value())
                })
                .collect::<Result<Vec<Job>>>()?;

            pending_jobs.sort_by_key(|job| job.sequence);
            Ok(pending_jobs)
        })
        .await
    }

    /// Retrieves and validates a stored job record by its identifier. @req DL-14
    pub async fn get(&self, id: String) -> Result<Option<Job>> {
        validate_id(&id)?;
        let db = Arc::clone(&self.database);
        cache::run_blocking("get", move || {
            let read_txn = db
                .begin_read()
                .map_err(|error| cache::cache_error("get", error))?;
            let jobs_table = read_txn
                .open_table(JOBS)
                .map_err(|error| cache::cache_error("get", error))?;
            let record = jobs_table
                .get(id.as_str())
                .map_err(|error| cache::cache_error("get", error))?;
            match record {
                Some(guard) => decode_job(&id, guard.value()).map(Some),
                None => Ok(None),
            }
        })
        .await
    }

    /// Advances a pending job to published without regressing completed state. @req DL-14
    pub async fn mark_published(&self, id: String) -> Result<()> {
        validate_id(&id)?;
        let db = Arc::clone(&self.database);
        cache::run_blocking("mark_published", move || {
            let write_txn = db
                .begin_write()
                .map_err(|error| cache::cache_error("mark_published", error))?;
            {
                let mut jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("mark_published", error))?;
                let record = jobs_table
                    .get(id.as_str())
                    .map_err(|error| cache::cache_error("mark_published", error))?
                    .map(|guard| decode_job(&id, guard.value()))
                    .transpose()?;
                match record {
                    None => {
                        // Missing row is harmless; return Ok without manufacturing a job.
                    }
                    Some(mut job) => {
                        match job.state {
                            State::Pending => {
                                job.state = State::Published;
                                let encoded = encode_job(&job)?;
                                jobs_table
                                    .insert(id.as_str(), encoded.as_slice())
                                    .map_err(|error| cache::cache_error("mark_published", error))?;
                            }
                            State::Published => {
                                // Already published; idempotent.
                            }
                            State::Completed => {
                                // Must not regress Completed!
                            }
                        }
                    }
                }
            }
            write_txn
                .commit()
                .map_err(|error| cache::cache_error("mark_published", error))?;
            Ok(())
        })
        .await
    }

    /// Transitions a job to completed idempotently. @req DL-14
    pub async fn complete(&self, id: String) -> Result<()> {
        validate_id(&id)?;
        let db = Arc::clone(&self.database);
        cache::run_blocking("complete", move || {
            let write_txn = db
                .begin_write()
                .map_err(|error| cache::cache_error("complete", error))?;
            {
                let mut jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("complete", error))?;
                let record = jobs_table
                    .get(id.as_str())
                    .map_err(|error| cache::cache_error("complete", error))?
                    .map(|guard| decode_job(&id, guard.value()))
                    .transpose()?;
                match record {
                    None => {
                        // Missing row is harmless; return Ok without manufacturing a job.
                    }
                    Some(mut job) => {
                        match job.state {
                            State::Pending | State::Published => {
                                job.state = State::Completed;
                                let encoded = encode_job(&job)?;
                                jobs_table
                                    .insert(id.as_str(), encoded.as_slice())
                                    .map_err(|error| cache::cache_error("complete", error))?;
                            }
                            State::Completed => {
                                // Already completed; idempotent.
                            }
                        }
                    }
                }
            }
            write_txn
                .commit()
                .map_err(|error| cache::cache_error("complete", error))?;
            Ok(())
        })
        .await
    }

    /// Removes a job record idempotently after acknowledged offset commit. @req DL-14
    pub async fn remove(&self, id: String) -> Result<()> {
        validate_id(&id)?;
        let db = Arc::clone(&self.database);
        cache::run_blocking("remove", move || {
            let write_txn = db
                .begin_write()
                .map_err(|error| cache::cache_error("remove", error))?;
            {
                let mut jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|error| cache::cache_error("remove", error))?;
                drop(
                    jobs_table
                        .remove(id.as_str())
                        .map_err(|error| cache::cache_error("remove", error))?,
                );
            }
            write_txn
                .commit()
                .map_err(|error| cache::cache_error("remove", error))?;
            Ok(())
        })
        .await
    }

    /// Returns the total number of retained job records in the store. @req DL-14
    pub async fn len(&self) -> Result<u64> {
        let db = Arc::clone(&self.database);
        cache::run_blocking("len", move || {
            let read_txn = db
                .begin_read()
                .map_err(|error| cache::cache_error("len", error))?;
            let jobs_table = read_txn
                .open_table(JOBS)
                .map_err(|error| cache::cache_error("len", error))?;
            jobs_table
                .len()
                .map_err(|error| cache::cache_error("len", error))
        })
        .await
    }

    /// Returns true if the store has no retained job records. @req DL-14
    #[cfg(test)]
    pub async fn is_empty(&self) -> Result<bool> {
        Ok(self.len().await? == 0)
    }
}

fn encode_job(job: &Job) -> Result<Vec<u8>> {
    validate_job(job)?;
    serde_json::to_vec(job).map_err(|error| cache::cache_error("job encode", error))
}

fn decode_job(expected_id: &str, bytes: &[u8]) -> Result<Job> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(cache::cache_error(
            "job decode",
            format!(
                "record byte size {} exceeds limit {MAX_RECORD_BYTES}",
                bytes.len()
            ),
        ));
    }
    let job: Job =
        serde_json::from_slice(bytes).map_err(|error| cache::cache_error("job decode", error))?;
    if job.id != expected_id {
        return Err(cache::cache_error(
            "job decode",
            format!(
                "job ID mismatch: table key {expected_id} != payload {}",
                job.id
            ),
        ));
    }
    validate_job(&job)?;
    Ok(job)
}

fn validate_job(job: &Job) -> Result<()> {
    validate_id(&job.id)?;
    if job.sequence == 0 {
        return Err(cache::cache_error(
            "validate",
            "job sequence must be positive",
        ));
    }
    if job.submitted_at_ms == 0 {
        return Err(cache::cache_error(
            "validate",
            "job submitted_at_ms must be positive",
        ));
    }
    validate_request(&job.request)?;
    Ok(())
}

fn validate_request(request: &Request) -> Result<()> {
    match (
        request.bot_id,
        request.user_id,
        request.chat_id,
        request.status_message,
    ) {
        (0, _, _, _) => Err(cache::cache_error(
            "validate",
            "request bot ID must be positive",
        )),
        (_, 0, _, _) => Err(cache::cache_error(
            "validate",
            "request user ID must be positive",
        )),
        (_, _, 0, _) => Err(cache::cache_error(
            "validate",
            "request chat ID must be non-zero",
        )),
        (_, _, _, s) if s <= 0 => Err(cache::cache_error(
            "validate",
            "request status message ID must be positive",
        )),
        _ => Ok(()),
    }?;

    if request.media.title.trim().is_empty() {
        return Err(cache::cache_error(
            "validate",
            "request media title must not be empty",
        ));
    }
    if media::http_url(&request.media.url).is_none() {
        return Err(cache::cache_error(
            "validate",
            "request media URL must be a valid bare HTTP(S) URL",
        ));
    }
    if request.policy.trim().is_empty() {
        return Err(cache::cache_error(
            "validate",
            "request policy must not be empty",
        ));
    }

    let serialized =
        serde_json::to_vec(request).map_err(|error| cache::cache_error("request encode", error))?;
    if serialized.len() > MAX_REQUEST_BYTES {
        return Err(cache::cache_error(
            "validate",
            format!(
                "serialized request exceeds 16KiB limit: {} bytes > {MAX_REQUEST_BYTES}",
                serialized.len()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.len() != 32 {
        return Err(cache::cache_error(
            "validate",
            format!("job ID length must be exactly 32, got {}", id.len()),
        ));
    }
    if !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(cache::cache_error(
            "validate",
            "job ID must contain only lowercase hex digits",
        ));
    }
    if id.bytes().all(|b| b == b'0') {
        return Err(cache::cache_error("validate", "job ID must be non-zero"));
    }
    Ok(())
}

fn generate_id() -> String {
    let mut val: u128 = rand::random();
    while val == 0 {
        val = rand::random();
    }
    format!("{val:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, CachedFile, FileKind, Key};
    use crate::error::Error;
    use tempfile::TempDir;

    fn sample_request(
        bot_id: u64,
        user_id: u64,
        chat_id: i64,
        status_message: i32,
        title: &str,
        url: &str,
        policy: &str,
    ) -> Request {
        Request {
            bot_id,
            user_id,
            chat_id,
            status_message,
            language: Language::from_code(Some("en")),
            media: Media {
                title: title.to_string(),
                url: url.to_string(),
            },
            format: Format::Opus,
            policy: policy.to_string(),
        }
    }

    async fn open_test_cache(dir: &TempDir) -> Cache {
        let path = dir.path().join("files.redb");
        Cache::open(path).await.expect("temporary cache opens")
    }

    async fn open_test_store(cache: &Cache, capacity: usize) -> Store {
        Store::open(cache.shared_database(), capacity)
            .await
            .expect("temporary store opens")
    }

    #[compio::test]
    async fn atomic_capacity_rejection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 2).await;

        let req1 = sample_request(1, 10, 100, 1, "Track 1", "https://example.com/1", "opus-v1");
        let req2 = sample_request(1, 20, 200, 2, "Track 2", "https://example.com/2", "opus-v1");
        let req3 = sample_request(1, 30, 300, 3, "Track 3", "https://example.com/3", "opus-v1");

        let job1 = store
            .submit(req1)
            .await
            .expect("submit succeeds")
            .expect("accepted");
        assert_eq!(store.len().await.expect("len read"), 1);

        let job2 = store
            .submit(req2)
            .await
            .expect("submit succeeds")
            .expect("accepted");
        assert_eq!(store.len().await.expect("len read"), 2);

        // Queue is at capacity (2), next submit must return None without writing.
        let full = store.submit(req3.clone()).await.expect("submit check");
        assert!(full.is_none(), "at capacity submit must return None");
        assert_eq!(store.len().await.expect("len read"), 2);

        // Completed jobs still count toward retained capacity until removed.
        store
            .complete(job1.id.clone())
            .await
            .expect("complete succeeds");
        let still_full = store.submit(req3.clone()).await.expect("submit check");
        assert!(
            still_full.is_none(),
            "completed-but-retained job still occupies capacity"
        );
        assert_eq!(store.len().await.expect("len read"), 2);

        // After removal, slot is available.
        store.remove(job1.id).await.expect("remove succeeds");
        assert_eq!(store.len().await.expect("len read"), 1);

        let job3 = store
            .submit(req3)
            .await
            .expect("submit succeeds")
            .expect("accepted");
        assert_eq!(store.len().await.expect("len read"), 2);
        assert!(
            job3.sequence > job2.sequence,
            "sequence monotonically advances"
        );
    }

    #[compio::test]
    async fn fifo_policy_and_recipient_preservation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let req1 = sample_request(
            1,
            101,
            -1001,
            10,
            "Title A",
            "https://example.com/a",
            "policy-a",
        );
        let req2 = sample_request(
            2,
            202,
            2002,
            20,
            "Title B",
            "https://example.com/b",
            "policy-b",
        );
        let req3 = sample_request(
            3,
            303,
            -3003,
            30,
            "Title C",
            "https://example.com/c",
            "policy-c",
        );

        let job1 = store.submit(req1).await.expect("submit 1").expect("job 1");
        let job2 = store.submit(req2).await.expect("submit 2").expect("job 2");
        let job3 = store.submit(req3).await.expect("submit 3").expect("job 3");

        let pending = store.pending().await.expect("pending jobs");
        assert_eq!(pending.len(), 3);

        // Ascending FIFO sequence preservation.
        assert_eq!(pending[0].id, job1.id);
        assert_eq!(pending[0].sequence, job1.sequence);
        assert_eq!(pending[0].request.bot_id, 1);
        assert_eq!(pending[0].request.user_id, 101);
        assert_eq!(pending[0].request.chat_id, -1001);
        assert_eq!(pending[0].request.status_message, 10);
        assert_eq!(pending[0].request.policy, "policy-a");

        assert_eq!(pending[1].id, job2.id);
        assert_eq!(pending[1].sequence, job2.sequence);
        assert_eq!(pending[1].request.bot_id, 2);
        assert_eq!(pending[1].request.user_id, 202);
        assert_eq!(pending[1].request.chat_id, 2002);
        assert_eq!(pending[1].request.policy, "policy-b");

        assert_eq!(pending[2].id, job3.id);
        assert_eq!(pending[2].sequence, job3.sequence);
        assert_eq!(pending[2].request.bot_id, 3);
        assert_eq!(pending[2].request.user_id, 303);
        assert_eq!(pending[2].request.chat_id, -3003);
        assert_eq!(pending[2].request.policy, "policy-c");

        assert!(pending[0].sequence < pending[1].sequence);
        assert!(pending[1].sequence < pending[2].sequence);
    }

    #[compio::test]
    async fn publish_after_complete_race() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let req = sample_request(
            1,
            10,
            100,
            1,
            "Race Song",
            "https://example.com/race",
            "opus-v1",
        );
        let job = store.submit(req).await.expect("submit").expect("job");
        assert_eq!(job.state, State::Pending);

        // Complete before publish.
        store
            .complete(job.id.clone())
            .await
            .expect("complete succeeds");
        let completed = store
            .get(job.id.clone())
            .await
            .expect("get")
            .expect("present");
        assert_eq!(completed.state, State::Completed);

        // A racing or delayed publish call MUST NOT regress Completed to Published.
        store
            .mark_published(job.id.clone())
            .await
            .expect("mark_published succeeds");
        let still_completed = store
            .get(job.id.clone())
            .await
            .expect("get")
            .expect("present");
        assert_eq!(still_completed.state, State::Completed);

        // Calling complete again is idempotent.
        store
            .complete(job.id.clone())
            .await
            .expect("repeat complete succeeds");
        let idempotent = store.get(job.id).await.expect("get").expect("present");
        assert_eq!(idempotent.state, State::Completed);
    }

    #[compio::test]
    async fn remove_and_missing_idempotency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let missing_id = "0123456789abcdef0123456789abcdef".to_string();
        assert!(store.get(missing_id.clone()).await.expect("get").is_none());

        // Operations on missing rows must succeed without manufacturing a job.
        store
            .mark_published(missing_id.clone())
            .await
            .expect("missing publish ok");
        store
            .complete(missing_id.clone())
            .await
            .expect("missing complete ok");
        store
            .remove(missing_id.clone())
            .await
            .expect("missing remove ok");
        assert!(store.get(missing_id).await.expect("get").is_none());

        // Real job lifecycle then removal idempotency.
        let req = sample_request(1, 10, 100, 1, "Song", "https://example.com/s", "opus-v1");
        let job = store.submit(req).await.expect("submit").expect("job");
        store.complete(job.id.clone()).await.expect("complete");
        store.remove(job.id.clone()).await.expect("first remove");
        assert!(store.get(job.id.clone()).await.expect("get").is_none());

        store
            .remove(job.id.clone())
            .await
            .expect("second remove is idempotent");
        assert!(store.get(job.id).await.expect("get").is_none());
    }

    #[compio::test]
    async fn reopen_with_pending_published_completed_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("files.redb");

        let (id1, id2, id3, seq3) = {
            let cache = Cache::open(path.clone()).await.expect("open cache");
            let store = Store::open(cache.shared_database(), 10)
                .await
                .expect("open store");

            let j1 = store
                .submit(sample_request(
                    1,
                    10,
                    100,
                    1,
                    "Song 1",
                    "https://example.com/1",
                    "p1",
                ))
                .await
                .expect("submit")
                .expect("j1");
            let j2 = store
                .submit(sample_request(
                    1,
                    20,
                    200,
                    2,
                    "Song 2",
                    "https://example.com/2",
                    "p2",
                ))
                .await
                .expect("submit")
                .expect("j2");
            let j3 = store
                .submit(sample_request(
                    1,
                    30,
                    300,
                    3,
                    "Song 3",
                    "https://example.com/3",
                    "p3",
                ))
                .await
                .expect("submit")
                .expect("j3");

            store
                .mark_published(j2.id.clone())
                .await
                .expect("publish j2");
            store.complete(j3.id.clone()).await.expect("complete j3");

            (j1.id, j2.id, j3.id, j3.sequence)
        };

        // Reopen cache and store on the existing file.
        let cache = Cache::open(path).await.expect("reopen cache");
        let store = Store::open(cache.shared_database(), 10)
            .await
            .expect("reopen store");

        assert_eq!(store.len().await.expect("len read"), 3);
        assert_eq!(store.retained().await.expect("startup replay").len(), 3);

        // pending returns only Pending jobs.
        let pending = store.pending().await.expect("pending read");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id1);
        assert_eq!(pending[0].state, State::Pending);

        let rec1 = store.get(id1).await.expect("get 1").expect("rec 1");
        assert_eq!(rec1.state, State::Pending);

        let rec2 = store.get(id2).await.expect("get 2").expect("rec 2");
        assert_eq!(rec2.state, State::Published);

        let rec3 = store.get(id3).await.expect("get 3").expect("rec 3");
        assert_eq!(rec3.state, State::Completed);

        // Sequence continues monotonically after reopen.
        let j4 = store
            .submit(sample_request(
                1,
                40,
                400,
                4,
                "Song 4",
                "https://example.com/4",
                "p4",
            ))
            .await
            .expect("submit 4")
            .expect("j4");
        assert!(
            j4.sequence > seq3,
            "sequence monotonically continues across reopen"
        );
    }

    #[compio::test]
    async fn shared_database_preserves_file_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;

        let cache_key = Key {
            bot_id: 42,
            source: "https://example.com/watch?v=cache-test".to_string(),
            variant: "voice-opus-v1".to_string(),
        };
        let cached_file = CachedFile {
            kind: FileKind::Voice,
            file_id: "telegram-file-id-voice-42".to_string(),
        };

        cache
            .put(cache_key.clone(), cached_file.clone())
            .await
            .expect("cache put");

        // Operate store on the same database.
        let store = open_test_store(&cache, 5).await;
        let job = store
            .submit(sample_request(
                42,
                99,
                999,
                1,
                "Video",
                "https://example.com/watch?v=cache-test",
                "voice-opus-v1",
            ))
            .await
            .expect("submit")
            .expect("job");
        store.mark_published(job.id.clone()).await.expect("publish");
        store.complete(job.id.clone()).await.expect("complete");
        store.remove(job.id).await.expect("remove");

        // File cache is untouched.
        let retrieved = cache
            .get(cache_key)
            .await
            .expect("cache get")
            .expect("file present");
        assert_eq!(retrieved, cached_file);
    }

    #[compio::test]
    async fn rejects_invalid_required_data_and_oversized_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let base = sample_request(1, 1, 1, 1, "Title", "https://example.com/valid", "policy");

        // bot_id must be > 0.
        let mut req = base.clone();
        req.bot_id = 0;
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // user_id must be > 0.
        let mut req = base.clone();
        req.user_id = 0;
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // chat_id must be != 0.
        let mut req = base.clone();
        req.chat_id = 0;
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // status_message must be > 0.
        let mut req = base.clone();
        req.status_message = 0;
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));
        let mut req = base.clone();
        req.status_message = -5;
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // Title must not be empty.
        let mut req = base.clone();
        req.media.title = "   ".into();
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // Policy must not be empty.
        let mut req = base.clone();
        req.policy = "   ".into();
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // URL must be valid bare HTTP(S).
        let mut req = base.clone();
        req.media.url = "ftp://example.com/file".into();
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));
        let mut req = base.clone();
        req.media.url = "https://user:pass@example.com/file".into();
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));
        let mut req = base.clone();
        req.media.url = "not a url".into();
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // Oversized request payload (>16KiB).
        let mut req = base.clone();
        req.policy = "x".repeat(17 * 1024);
        assert!(matches!(store.submit(req).await, Err(Error::Cache(_))));

        // Capacity bounds on Store::open.
        assert!(matches!(
            Store::open(cache.shared_database(), 0).await,
            Err(Error::Cache(_))
        ));
        assert!(matches!(
            Store::open(cache.shared_database(), 257).await,
            Err(Error::Cache(_))
        ));

        // Invalid job IDs on get/mark_published/complete/remove.
        let invalid_ids = [
            "".to_string(),
            "short".to_string(),
            "0123456789abcdef0123456789abcdef0".to_string(), // 33 chars
            "0123456789ABCDEF0123456789abcdef".to_string(),  // uppercase hex
            "0123456789xyzdef0123456789abcdef".to_string(),  // non-hex
            "00000000000000000000000000000000".to_string(),  // all zero
        ];

        for invalid in invalid_ids {
            assert!(matches!(
                store.get(invalid.clone()).await,
                Err(Error::Cache(_))
            ));
            assert!(matches!(
                store.mark_published(invalid.clone()).await,
                Err(Error::Cache(_))
            ));
            assert!(matches!(
                store.complete(invalid.clone()).await,
                Err(Error::Cache(_))
            ));
            assert!(matches!(store.remove(invalid).await, Err(Error::Cache(_))));
        }
    }

    #[compio::test]
    async fn divergent_state_rejected_when_sequence_metadata_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let job = store
            .submit(sample_request(
                1,
                1,
                1,
                1,
                "Song",
                "https://example.com/song",
                "p1",
            ))
            .await
            .expect("submit")
            .expect("job");
        assert_eq!(job.sequence, 1);

        // Delete sequence metadata directly while jobs exist in the table.
        let db = cache.shared_database();
        cache::run_blocking("test_corrupt", move || {
            let write_txn = db
                .begin_write()
                .map_err(|e| cache::cache_error("test", e))?;
            {
                let mut meta_table = write_txn
                    .open_table(META)
                    .map_err(|e| cache::cache_error("test", e))?;
                meta_table
                    .remove(SEQUENCE_KEY)
                    .map_err(|e| cache::cache_error("test", e))?;
            }
            write_txn
                .commit()
                .map_err(|e| cache::cache_error("test", e))?;
            Ok(())
        })
        .await
        .expect("metadata removed");

        // Store::open must reject divergent state.
        let reopen = Store::open(cache.shared_database(), 10).await;
        assert!(
            matches!(reopen, Err(Error::Cache(ref msg)) if msg.contains("sequence metadata is missing")),
            "divergent state must be rejected on open: {reopen:?}"
        );
    }

    #[compio::test]
    async fn corrupt_jobs_record_rejected_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = open_test_cache(&dir).await;
        let store = open_test_store(&cache, 10).await;

        let valid_id = "0123456789abcdef0123456789abcdef";
        let db = cache.shared_database();
        cache::run_blocking("test_inject_corrupt", move || {
            let write_txn = db
                .begin_write()
                .map_err(|e| cache::cache_error("test", e))?;
            {
                let mut jobs_table = write_txn
                    .open_table(JOBS)
                    .map_err(|e| cache::cache_error("test", e))?;
                jobs_table
                    .insert(valid_id, b"not valid json" as &[u8])
                    .map_err(|e| cache::cache_error("test", e))?;
            }
            write_txn
                .commit()
                .map_err(|e| cache::cache_error("test", e))?;
            Ok(())
        })
        .await
        .expect("corrupt row inserted");

        assert!(matches!(
            store.get(valid_id.to_string()).await,
            Err(Error::Cache(_))
        ));
        assert!(matches!(store.pending().await, Err(Error::Cache(_))));
    }
}
