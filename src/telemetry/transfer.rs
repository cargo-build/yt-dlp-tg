//! Application upload boundaries; none measures socket acknowledgement. @req DL-8 DL-11

use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    io::{self, Read},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use futures_util::future::{Either, select};

const TICK: Duration = Duration::from_secs(1);
const REPORT: Duration = Duration::from_secs(30);

/// Shared local state follows the single owned streaming request. @req DL-7 DL-11
#[derive(Clone)]
pub(crate) struct Upload(Rc<RefCell<State>>);

struct State {
    started: Instant,
    expected: u64,
    bytes: u64,
    reads: u64,
    errors: u64,
    cancelled_reads: u64,
    cancelled_time: Duration,
    eof: u64,
    first_read: Option<Instant>,
    reading: Option<Instant>,
    last_read: Option<Instant>,
    body_complete: Option<Instant>,
    read_time: Duration,
    read_max: Duration,
    gap_time: Duration,
    gap_max: Duration,
    polls: u64,
    poll_time: Duration,
    poll_max: Duration,
    timer_samples: u64,
    timer_lag_max: Duration,
}

impl Upload {
    pub(crate) fn new(expected: u64) -> Self {
        Self(Rc::new(RefCell::new(State {
            started: Instant::now(),
            expected,
            bytes: 0,
            reads: 0,
            errors: 0,
            cancelled_reads: 0,
            cancelled_time: Duration::ZERO,
            eof: 0,
            first_read: None,
            reading: None,
            last_read: None,
            body_complete: None,
            read_time: Duration::ZERO,
            read_max: Duration::ZERO,
            gap_time: Duration::ZERO,
            gap_max: Duration::ZERO,
            polls: 0,
            poll_time: Duration::ZERO,
            poll_max: Duration::ZERO,
            timer_samples: 0,
            timer_lag_max: Duration::ZERO,
        })))
    }

    pub(crate) fn read_started(&self) {
        let now = Instant::now();
        let mut state = self.0.borrow_mut();
        if let Some(abandoned) = state.reading.take() {
            state.cancelled_reads = state.cancelled_reads.saturating_add(1);
            state.cancelled_time = state.cancelled_time.saturating_add(now.saturating_duration_since(abandoned));
            state.last_read = None;
        }
        if let Some(previous) = state.last_read {
            let gap = now.saturating_duration_since(previous);
            state.gap_time = state.gap_time.saturating_add(gap);
            state.gap_max = state.gap_max.max(gap);
        }
        state.first_read.get_or_insert(now);
        state.reading = Some(now);
    }

    pub(crate) fn read_finished(&self, result: &io::Result<usize>) {
        let now = Instant::now();
        let mut state = self.0.borrow_mut();
        if let Some(started) = state.reading.take() {
            let elapsed = now.saturating_duration_since(started);
            state.read_time = state.read_time.saturating_add(elapsed);
            state.read_max = state.read_max.max(elapsed);
        }
        state.last_read = Some(now);
        state.reads = state.reads.saturating_add(1);
        match result {
            Ok(0) => state.eof = state.eof.saturating_add(1),
            Ok(bytes) => {
                state.bytes = state.bytes.saturating_add(*bytes as u64);
                if state.bytes >= state.expected && state.body_complete.is_none() {
                    state.body_complete = Some(now);
                }
            }
            Err(_) => state.errors = state.errors.saturating_add(1),
        }
    }

    /// The watchdog yields alongside work; dropping the request drops the watchdog. @req DL-11
    pub(crate) async fn run<T, E>(&self, work: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        self.run_with_tick(work, TICK).await
    }

    async fn run_with_tick<T, E>(
        &self,
        work: impl Future<Output = Result<T, E>>,
        tick: Duration,
    ) -> Result<T, E> {
        let mut report = FinalReport {
            upload: self,
            outcome: "cancelled",
        };
        self.emit("started");
        let mut work = std::pin::pin!(work);
        let polled = poll_fn(|context| {
            let started = Instant::now();
            let result = work.as_mut().poll(context);
            let elapsed = started.elapsed();
            let mut state = self.0.borrow_mut();
            state.polls = state.polls.saturating_add(1);
            state.poll_time = state.poll_time.saturating_add(elapsed);
            state.poll_max = state.poll_max.max(elapsed);
            result
        });
        let watchdog = self.watch(tick);
        let result = match select(std::pin::pin!(polled), std::pin::pin!(watchdog)).await {
            Either::Left((result, _)) => result,
            Either::Right((never, _)) => match never {},
        };
        report.outcome = if result.is_ok() { "ok" } else { "failed" };
        result
    }

    async fn watch(&self, tick: Duration) -> std::convert::Infallible {
        let mut next = self.0.borrow().started + tick;
        let mut report_at = next;
        loop {
            compio::time::sleep(next.saturating_duration_since(Instant::now())).await;
            let now = Instant::now();
            {
                let mut state = self.0.borrow_mut();
                state.timer_samples = state.timer_samples.saturating_add(1);
                state.timer_lag_max = state.timer_lag_max.max(now.saturating_duration_since(next));
            }
            next = now + tick;
            if now >= report_at {
                self.emit("periodic");
                if tracing::enabled!(tracing::Level::INFO) {
                    let sample = compio::runtime::spawn_blocking(rings).await;
                    match sample {
                        Ok(Ok(sample)) if !sample.is_empty() => {
                            sample.into_iter().for_each(|ring| ring.emit())
                        }
                        _ => tracing::info!(
                            stage = "ring_diagnostics",
                            available = false,
                            "Process ring sample"
                        ),
                    }
                }
                report_at = now + REPORT;
            }
        }
    }

    fn emit(&self, outcome: &'static str) {
        let state = self.0.borrow();
        let now = Instant::now();
        let elapsed = |then| now.saturating_duration_since(then).as_secs_f64() * 1000.0;
        let since_start =
            |then: Instant| then.saturating_duration_since(state.started).as_secs_f64() * 1000.0;
        tracing::info!(stage = "upload_diagnostics", outcome,
            elapsed_ms = elapsed(state.started), expected_bytes = state.expected, body_bytes = state.bytes,
            file_reads = state.reads, file_read_errors = state.errors, file_eof = state.eof,
            file_read_cancelled = state.cancelled_reads,
            file_read_cancelled_us = state.cancelled_time.as_secs_f64() * 1_000_000.0,
            first_read_ms = ?state.first_read.map(since_start),
            body_complete_ms = ?state.body_complete.map(since_start),
            after_body_ms = ?state.body_complete.map(elapsed),
            file_read_await_us = state.read_time.as_secs_f64() * 1_000_000.0,
            file_read_max_us = state.read_max.as_secs_f64() * 1_000_000.0,
            file_read_pending = state.reading.is_some(), file_read_pending_ms = ?state.reading.map(elapsed),
            body_gap_ms = state.gap_time.as_secs_f64() * 1000.0,
            body_gap_max_ms = state.gap_max.as_secs_f64() * 1000.0,
            request_polls = state.polls, request_poll_us = state.poll_time.as_secs_f64() * 1_000_000.0,
            request_poll_max_us = state.poll_max.as_secs_f64() * 1_000_000.0,
            timer_samples = state.timer_samples, timer_lag_max_ms = state.timer_lag_max.as_secs_f64() * 1000.0,
            "Native upload application boundaries");
    }
}

struct FinalReport<'a> {
    upload: &'a Upload,
    outcome: &'static str,
}

impl Drop for FinalReport<'_> {
    fn drop(&mut self) {
        self.upload.emit(self.outcome);
    }
}

struct Ring {
    descriptor: u32,
    sq_head: Option<u32>,
    sq_tail: Option<u32>,
    cq_head: Option<u32>,
    cq_tail: Option<u32>,
}

impl Ring {
    fn emit(&self) {
        tracing::info!(
            stage = "ring_diagnostics",
            available = true,
            scope = "process",
            descriptor = self.descriptor,
            sq_head = self.sq_head,
            sq_tail = self.sq_tail,
            cq_head = self.cq_head,
            cq_tail = self.cq_tail,
            "Process ring sample"
        );
    }
}

fn rings() -> io::Result<Vec<Ring>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")?.take(128) {
        let entry = match entry { Ok(entry) => entry, Err(_) => continue };
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(_) => continue,
        };
        if target != Path::new("anon_inode:[io_uring]") {
            continue;
        }
        let descriptor = match entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            Some(descriptor) => descriptor,
            None => continue,
        };
        let mut text = String::new();
        let read = std::fs::File::open(Path::new("/proc/self/fdinfo").join(entry.file_name()))
            .and_then(|file| file.take(64 * 1024).read_to_string(&mut text));
        if read.is_err() { continue; }
        found.push(parse_ring(descriptor, &text));
        if found.len() == 4 {
            break;
        }
    }
    Ok(found)
}

fn parse_ring(descriptor: u32, text: &str) -> Ring {
    let field = |name| {
        text.lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| (key == name).then(|| value.trim().parse().ok()).flatten())
    };
    Ring {
        descriptor,
        sq_head: field("SqHead"),
        sq_tail: field("SqTail"),
        cq_head: field("CqHead"),
        cq_tail: field("CqTail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn separates_read_await_body_demand_and_response_tail() {
        let upload = Upload::new(8);
        let result: Result<(), ()> = upload
            .run(async {
                upload.read_started();
                compio::time::sleep(Duration::from_millis(12)).await;
                upload.read_finished(&Ok(4));
                compio::time::sleep(Duration::from_millis(18)).await;
                upload.read_started();
                upload.read_finished(&Ok(4));
                upload.read_started();
                upload.read_finished(&Ok(0));
                compio::time::sleep(Duration::from_millis(25)).await;
                Ok(())
            })
            .await;
        assert_eq!(result, Ok(()));
        let state = upload.0.borrow();
        assert_eq!(
            (state.bytes, state.reads, state.eof, state.errors),
            (8, 3, 1, 0)
        );
        assert!(state.read_time >= Duration::from_millis(10));
        assert!(state.gap_time >= Duration::from_millis(15));
        assert!(state.body_complete.expect("body complete").elapsed() >= Duration::from_millis(20));
        assert!(state.polls >= 3);
    }

    #[compio::test]
    async fn records_slow_poll_timer_delay_and_preserves_failure() {
        let upload = Upload::new(5);
        let result = upload
            .run_with_tick(
                async {
                    std::thread::sleep(Duration::from_millis(20));
                    compio::time::sleep(Duration::from_millis(20)).await;
                    upload.read_started();
                    upload.read_finished(&Err(io::Error::other("PRIVATE")));
                    Err::<(), _>(17)
                },
                Duration::from_millis(2),
            )
            .await;
        assert_eq!(result, Err(17));
        let state = upload.0.borrow();
        assert_eq!((state.bytes, state.errors), (0, 1));
        assert!(state.body_complete.is_none());
        assert!(state.poll_max >= Duration::from_millis(18));
        assert!(state.timer_lag_max >= Duration::from_millis(15));
        assert!(state.timer_samples >= 1);
    }

    #[compio::test]
    async fn cancellation_drops_work_without_a_detached_monitor() {
        let upload = Upload::new(5);
        let result = compio::time::timeout(
            Duration::from_millis(5),
            upload.run(async {
                upload.read_started();
                std::future::pending::<Result<(), ()>>().await
            }),
        )
        .await;
        assert!(result.is_err());
        assert!(upload.0.borrow().reading.is_some());
        assert_eq!(Rc::strong_count(&upload.0), 1);
    }

    #[compio::test]
    async fn resumed_abandoned_read_is_not_charged_as_body_demand_gap() {
        let upload = Upload::new(8);
        upload.read_started();
        upload.read_finished(&Ok(4));
        upload.read_started();
        let gap = upload.0.borrow().gap_time;
        compio::time::sleep(Duration::from_millis(15)).await;
        upload.read_started();
        upload.read_finished(&Ok(4));
        let state = upload.0.borrow();
        assert_eq!(state.cancelled_reads, 1);
        assert!(state.cancelled_time >= Duration::from_millis(12));
        assert_eq!(state.gap_time, gap);
        assert_eq!(state.bytes, 8);
    }

    #[test]
    fn ring_parser_whitelists_numeric_fields_and_leaves_missing_unknown() {
        let ring = parse_ring(
            12,
            "SqHead:\t23\nSqTail:\t24\nCqHead:\t45\nCqTail:\tPRIVATE\nSQEs:\tPRIVATE\n",
        );
        assert_eq!(
            (ring.sq_head, ring.sq_tail, ring.cq_head, ring.cq_tail),
            (Some(23), Some(24), Some(45), None)
        );
    }
}
