//! Local, measured download/upload feedback without untrusted presentation text. @req DL-11/DL-8

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Instant,
};

use crate::{
    i18n::{Language, Text},
    video::Size,
};

/// The stdout frame namespace excludes ordinary downloader output. @req DL-11/DL-8
pub(crate) const FRAME_PREFIX: &[u8] = b"yt-dlp-tg-progress1|";
/// Only bounded identifiers, enumerated statuses and byte counters are consumed. @req DL-11/DL-8
pub(crate) const DOWNLOAD_TEMPLATE: &str = "download:yt-dlp-tg-progress1|%(info.format_id)s|%(progress.status)s|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s";

/// Progress cannot retain arbitrary extractor metadata or unlimited stream IDs.
const MAX_STREAMS: usize = 16;
const MAX_ID_LENGTH: usize = 100;
const MAX_FRAME_LENGTH: usize = 256;
/// A one-PiB ceiling rejects absurd counters while leaving media limits to their owner.
const MAX_FRAME_BYTES: u64 = 1 << 50;
const BAR_CELLS: u8 = 10;
const ANIMATION_INTERVAL_MS: u128 = 3500;
const EMPTY_BAR: &str = "░░░░░░░░░░";
const FULL_BAR: &str = "██████████";

/// Cheap clones share only ephemeral state on the native executor. @req DL-11
#[derive(Clone)]
pub(crate) struct Progress(Rc<State>);

struct State {
    started: Instant,
    download: RefCell<Download>,
    sending: Cell<Sending>,
    warned: Cell<bool>,
}

struct Download {
    phase: DownloadPhase,
    hint: Option<Size>,
    streams: HashMap<Vec<u8>, Stream>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DownloadPhase {
    Active,
    Preparing,
    Complete,
    Cached,
}

#[derive(Clone, Copy)]
enum Sending {
    Pending,
    Active { total: Option<u64>, done: u64 },
    Cached,
}

#[derive(Default)]
struct Stream {
    done: Option<u64>,
    total: Option<Size>,
    finished: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    done: Option<u64>,
    total: Option<Size>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Downloading,
    Finished,
}

#[derive(Debug, PartialEq, Eq)]
struct Frame<'a> {
    id: &'a [u8],
    status: Status,
    done: Option<u64>,
    total: Option<Size>,
}

#[derive(Clone, Copy)]
enum Transfer {
    Downloading,
    Downloaded,
    Sending,
}

impl Progress {
    /// Pinned sizes guide the denominator without inventing observed bytes. @req DL-11
    pub(crate) fn new(download_total: Option<Size>) -> Self {
        Self(Rc::new(State {
            started: Instant::now(),
            download: RefCell::new(Download {
                phase: DownloadPhase::Active,
                hint: download_total.filter(|total| total.bytes() > 0),
                streams: HashMap::new(),
            }),
            sending: Cell::new(Sending::Pending),
            warned: Cell::new(false),
        }))
    }

    /// File-ID reuse completes both rows without claiming a byte transfer. @req DL-11
    pub(crate) fn cached() -> Self {
        let progress = Self::new(None);
        progress.0.download.borrow_mut().phase = DownloadPhase::Cached;
        progress.0.sending.set(Sending::Cached);
        progress
    }

    /// Invalid optional frames are ignored without affecting media work. @req DL-11/DL-8
    pub(crate) fn observe(&self, line: &[u8]) {
        let Some(frame) = parse_frame(line) else {
            return;
        };
        let mut download = self.0.download.borrow_mut();
        match download.phase {
            DownloadPhase::Active => {}
            DownloadPhase::Preparing | DownloadPhase::Complete | DownloadPhase::Cached => return,
        }
        let has_room = download.streams.len() < MAX_STREAMS;
        match download.streams.get_mut(frame.id) {
            Some(stream) => stream.observe(frame),
            None if has_room => {
                let mut stream = Stream::default();
                let id = frame.id.to_vec();
                stream.observe(frame);
                download.streams.insert(id, stream);
            }
            None => {}
        }
    }

    /// Only the completed download/processing future may finish this row. @req DL-11
    pub(crate) fn download_finished(&self) {
        let mut download = self.0.download.borrow_mut();
        match download.phase {
            DownloadPhase::Active | DownloadPhase::Preparing => {
                download.phase = DownloadPhase::Complete
            }
            DownloadPhase::Complete | DownloadPhase::Cached => {}
        }
    }

    /// MP4 preparation has no invented encoder byte percentage. @req DL-4 DL-11
    pub(crate) fn preparing_video(&self) {
        let mut download = self.0.download.borrow_mut();
        if download.phase == DownloadPhase::Active {
            download.phase = DownloadPhase::Preparing;
        }
    }

    /// A new file upload starts at zero using its actual file length. @req DL-11
    pub(crate) fn sending(&self, total: Option<u64>) {
        match self.0.sending.get() {
            Sending::Cached => {}
            Sending::Pending | Sending::Active { .. } => self.0.sending.set(Sending::Active {
                total: total.filter(|total| *total > 0),
                done: 0,
            }),
        }
    }

    /// Count completed file reads, rather than acknowledged Telegram bytes. @req DL-11
    pub(crate) fn uploaded(&self, bytes: usize) {
        match self.0.sending.get() {
            Sending::Active { total, done } => {
                let bytes = match u64::try_from(bytes) {
                    Ok(bytes) => bytes,
                    Err(_) => return,
                };
                self.0.sending.set(Sending::Active {
                    total,
                    done: done.saturating_add(bytes),
                });
            }
            Sending::Pending | Sending::Cached => {}
        }
    }

    /// Both localized rows and real elapsed time remain visible throughout work. @req DL-11
    pub(crate) fn render(&self, language: Language) -> String {
        let elapsed = self.0.started.elapsed();
        let animation = (elapsed.as_millis() / ANIMATION_INTERVAL_MS) % u128::from(BAR_CELLS);
        let download = self.0.download.borrow();
        let (download_marker, download_value) = match download.phase {
            DownloadPhase::Preparing => (
                '▶',
                format!(
                    "{} {}",
                    animated_bar(animation),
                    language.text(Text::PreparingVideo)
                ),
            ),
            DownloadPhase::Active => (
                '▶',
                transfer_value(download.sample(), Transfer::Downloading, animation),
            ),
            DownloadPhase::Complete => (
                '✓',
                transfer_value(download.sample(), Transfer::Downloaded, animation),
            ),
            DownloadPhase::Cached => (
                '✓',
                format!("{EMPTY_BAR} {}", language.text(Text::ProgressCached)),
            ),
        };
        let (sending_marker, sending_value) = match self.0.sending.get() {
            Sending::Pending => (
                '○',
                format!("{EMPTY_BAR} {}", language.text(Text::ProgressPending)),
            ),
            Sending::Active { total, done } => (
                '▶',
                transfer_value(
                    Sample {
                        done: Some(done),
                        total: total.map(Size::Exact),
                    },
                    Transfer::Sending,
                    animation,
                ),
            ),
            Sending::Cached => (
                '▶',
                format!("{EMPTY_BAR} {}", language.text(Text::ProgressCached)),
            ),
        };
        let seconds = elapsed.as_secs();
        format!(
            "{download_marker} {}\n{download_value}\n\n{sending_marker} {}\n{sending_value}\n\n⏱ {:02}:{:02}:{:02}",
            language.text(Text::Downloading),
            language.text(Text::Sending),
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60,
        )
    }

    /// Cosmetic failures produce at most one warning for this delivery. @req DL-11/DL-8
    pub(crate) fn warn_once(&self) -> bool {
        !self.0.warned.replace(true)
    }
}

impl Download {
    fn sample(&self) -> Sample {
        let done = self
            .streams
            .values()
            .filter_map(|stream| stream.done)
            .reduce(u64::saturating_add);
        let observed_total = match self.streams.is_empty() {
            true => None,
            false => self
                .streams
                .values()
                .try_fold(Size::Exact(0), |sum, stream| {
                    let total = match (stream.finished, stream.total, stream.done) {
                        (true, None, Some(done)) if done > 0 => Size::Exact(done),
                        (_, Some(total), _) => total,
                        _ => return None,
                    };
                    let bytes = sum.bytes().checked_add(total.bytes())?;
                    Some(match (sum, total) {
                        (Size::Exact(_), Size::Exact(_)) => Size::Exact(bytes),
                        _ => Size::Estimated(bytes),
                    })
                }),
        };
        Sample {
            done,
            total: self.hint.or(observed_total),
        }
    }
}

impl Stream {
    fn observe(&mut self, frame: Frame<'_>) {
        if self.finished {
            return;
        }
        self.total = match (self.total, frame.total) {
            (Some(Size::Exact(total)), Some(Size::Estimated(_)) | None) => Some(Size::Exact(total)),
            (_, Some(total)) => Some(total),
            (total, None) => total,
        };
        let reported = match (frame.status, frame.done, frame.total) {
            (_, Some(done), _) => Some(done),
            (Status::Finished, None, Some(Size::Exact(total))) => Some(total),
            _ => None,
        };
        self.done = match (self.done, reported) {
            (Some(previous), Some(current)) => Some(previous.max(current)),
            (previous, None) => previous,
            (None, current) => current,
        };
        self.finished = frame.status == Status::Finished;
    }
}

fn parse_frame(line: &[u8]) -> Option<Frame<'_>> {
    if line.len() > MAX_FRAME_LENGTH {
        return None;
    }
    let mut fields = line.strip_prefix(FRAME_PREFIX)?.split(|byte| *byte == b'|');
    let id = fields.next()?;
    if id.is_empty()
        || id.len() > MAX_ID_LENGTH
        || !id
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'.' | b'-'))
    {
        return None;
    }
    let status = match fields.next()? {
        b"downloading" => Status::Downloading,
        b"finished" => Status::Finished,
        _ => return None,
    };
    let done = parse_number(fields.next()?)?;
    let exact = parse_number(fields.next()?)?;
    let estimate = parse_number(fields.next()?)?;
    match fields.next().is_some() || exact == Some(0) || estimate == Some(0) {
        true => None,
        false => Some(Frame {
            id,
            status,
            done,
            total: exact.map(Size::Exact).or(estimate.map(Size::Estimated)),
        }),
    }
}

fn parse_number(field: &[u8]) -> Option<Option<u64>> {
    match field {
        b"NA" => Some(None),
        _ if field.is_empty() || field.len() > 16 || !field.iter().all(u8::is_ascii_digit) => None,
        _ => {
            let bytes = field.iter().try_fold(0_u64, |value, byte| {
                value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
            })?;
            (bytes <= MAX_FRAME_BYTES).then_some(Some(bytes))
        }
    }
}

fn transfer_value(sample: Sample, transfer: Transfer, animation: u128) -> String {
    let complete = matches!(transfer, Transfer::Downloaded);
    match sample.total {
        Some(total) if sample.done.is_none() && !complete => {
            let estimate = match total {
                Size::Exact(_) => "",
                Size::Estimated(_) => "~",
            };
            format!(
                "{} — / {estimate}{}",
                animated_bar(animation),
                byte_amount(total.bytes())
            )
        }
        Some(total) => {
            let percent = match transfer {
                Transfer::Downloaded => 100,
                Transfer::Downloading | Transfer::Sending => {
                    let ceiling: u8 = match transfer {
                        Transfer::Downloading => 99,
                        Transfer::Sending | Transfer::Downloaded => 100,
                    };
                    let done = match sample.done {
                        Some(done) => u128::from(done),
                        None => 0,
                    };
                    ((done * 100 / u128::from(total.bytes())).min(u128::from(ceiling))) as u8
                }
            };
            let bar: String = (0..BAR_CELLS)
                .map(|cell| match cell < percent / 10 {
                    true => '█',
                    false => '░',
                })
                .collect();
            let estimate = match total {
                Size::Exact(_) => "",
                Size::Estimated(_) => "~",
            };
            match sample.done {
                Some(done) if complete => {
                    format!("{bar} {estimate}{percent}% · {}", byte_amount(done))
                }
                Some(done) => format!(
                    "{bar} {estimate}{percent}% · {} / {estimate}{}",
                    byte_amount(done),
                    byte_amount(total.bytes()),
                ),
                None => format!("{bar} {estimate}{percent}%"),
            }
        }
        None => {
            let bar = match complete {
                true => FULL_BAR.to_owned(),
                false => animated_bar(animation),
            };
            match sample.done {
                Some(done) => format!("{bar} · {}", byte_amount(done)),
                None => bar,
            }
        }
    }
}

fn animated_bar(animation: u128) -> String {
    (0..BAR_CELLS)
        .map(|cell| match u128::from(cell) == animation {
            true => '█',
            false => '░',
        })
        .collect()
}

fn byte_amount(bytes: u64) -> String {
    match [(1_u64 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")]
        .into_iter()
        .find(|(unit, _)| bytes >= *unit)
    {
        Some((unit, name)) => {
            let tenths = u128::from(bytes) * 10 / u128::from(unit);
            format!("{}.{} {name}", tenths / 10, tenths % 10)
        }
        None => format!("{bytes} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn english() -> Language {
        Language::from_code(Some("en"))
    }

    #[test]
    fn video_preparation_is_visible_without_claiming_encoded_bytes() {
        let progress = Progress::new(None);
        progress.observe(b"yt-dlp-tg-progress1|video|finished|100|100|NA");
        progress.preparing_video();
        let preparing = progress.render(english());
        assert!(preparing.contains("Preparing MP4"));
        assert!(preparing.contains("Sending"));
        assert!(!preparing.contains('%'));
        progress.download_finished();
        assert!(progress.render(english()).contains("100%"));
        let cached = Progress::cached();
        cached.preparing_video();
        assert!(!cached.render(english()).contains("Preparing MP4"));
    }

    #[test]
    fn parser_prefers_exact_totals_and_accepts_finished_na() {
        let frame = parse_frame(b"yt-dlp-tg-progress1|137|downloading|12|40|50")
            .expect("exact downloading frame");
        assert_eq!(frame.id, b"137");
        assert_eq!(frame.done, Some(12));
        assert_eq!(frame.total, Some(Size::Exact(40)));
        let frame = parse_frame(b"yt-dlp-tg-progress1|m4a.audio-1|finished|NA|40|NA")
            .expect("finished frames may omit downloaded_bytes");
        assert_eq!(frame.status, Status::Finished);
        assert_eq!(frame.done, None);
        assert_eq!(frame.total, Some(Size::Exact(40)));
    }

    #[test]
    fn parser_preserves_estimates_and_unknown_values() {
        assert_eq!(
            parse_frame(b"yt-dlp-tg-progress1|251|downloading|0|NA|100")
                .expect("estimated stream")
                .total,
            Some(Size::Estimated(100)),
        );
        let frame =
            parse_frame(b"yt-dlp-tg-progress1|251|finished|NA|NA|NA").expect("unknown stream size");
        assert_eq!(frame.done, None);
        assert_eq!(frame.total, None);
    }

    #[test]
    fn parser_rejects_untrusted_or_absurd_fields() {
        [
            "other|137|downloading|0|10|NA",
            "yt-dlp-tg-progress1||downloading|0|10|NA",
            "yt-dlp-tg-progress1|https://private|downloading|0|10|NA",
            "yt-dlp-tg-progress1|137|error|0|10|NA",
            "yt-dlp-tg-progress1|137|downloading|0|10",
            "yt-dlp-tg-progress1|137|downloading|0|10|NA|extra",
            "yt-dlp-tg-progress1|137|downloading|-1|10|NA",
            "yt-dlp-tg-progress1|137|downloading|1.0|10|NA",
            "yt-dlp-tg-progress1|137|downloading|١|10|NA",
            "yt-dlp-tg-progress1|137|downloading|0|0|NA",
            "yt-dlp-tg-progress1|137|downloading|0|10|0",
            "yt-dlp-tg-progress1|137|downloading|0|NA|NaN",
            "yt-dlp-tg-progress1|137|downloading|0|10|NA\n",
            "yt-dlp-tg-progress1|137|downloading|1125899906842625|NA|NA",
            "yt-dlp-tg-progress1|137|downloading|18446744073709551616|NA|NA",
        ]
        .into_iter()
        .for_each(|line| assert!(parse_frame(line.as_bytes()).is_none(), "accepted {line:?}"));
        let long_id = format!(
            "yt-dlp-tg-progress1|{}|downloading|0|10|NA",
            "a".repeat(101)
        );
        assert!(parse_frame(long_id.as_bytes()).is_none());
        assert!(parse_frame(&[b'a'; MAX_FRAME_LENGTH + 1]).is_none());
        assert!(parse_frame(b"yt-dlp-tg-progress1|\xff|downloading|0|10|NA").is_none());
    }

    #[test]
    fn initial_rows_are_localized_and_sending_is_pending() {
        ["en", "ru"].into_iter().for_each(|code| {
            let language = Language::from_code(Some(code));
            let rendered = Progress::new(None).render(language);
            assert!(rendered.starts_with(&format!("▶ {}\n", language.text(Text::Downloading))));
            assert!(rendered.contains(&format!("○ {}\n", language.text(Text::Sending))));
            assert!(rendered.contains(language.text(Text::ProgressPending)));
            assert!(rendered.contains("⏱ 00:00:00"));
            assert!(!rendered.contains('%'));
            assert!(!rendered.contains(" B"));
        });
    }

    #[test]
    fn split_streams_keep_completed_bytes_and_finished_is_idempotent() {
        let progress = Progress::new(None);
        progress.observe(b"yt-dlp-tg-progress1|137|finished|NA|100|NA");
        progress.observe(b"yt-dlp-tg-progress1|137|finished|NA|100|NA");
        progress.observe(b"yt-dlp-tg-progress1|137|downloading|0|100|NA");
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|20|100|NA");
        assert_eq!(
            progress.0.download.borrow().sample(),
            Sample {
                done: Some(120),
                total: Some(Size::Exact(200)),
            }
        );
        let rendered = progress.render(english());
        assert!(rendered.contains("██████░░░░ 60% · 120 B / 200 B"));
    }

    #[test]
    fn pinned_total_covers_both_streams_before_second_is_seen() {
        let progress = Progress::new(Some(Size::Estimated(200)));
        progress.observe(b"yt-dlp-tg-progress1|137|finished|NA|100|NA");
        assert!(progress.render(english()).contains("~50% · 100 B / ~200 B"));
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|20|100|NA");
        assert!(progress.render(english()).contains("~60% · 120 B / ~200 B"));
    }

    #[test]
    fn unknown_stream_never_acquires_an_invented_denominator() {
        let progress = Progress::new(None);
        progress.observe(b"yt-dlp-tg-progress1|137|finished|NA|100|NA");
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|20|NA|NA");
        let rendered = progress.render(english());
        assert!(rendered.contains("120 B"));
        assert!(!rendered.contains('%'));
        progress.download_finished();
        assert!(
            progress
                .render(english())
                .starts_with("✓ Downloading…\n██████████ · 120 B")
        );
    }

    #[test]
    fn active_download_waits_for_processing_before_one_hundred_percent() {
        let progress = Progress::new(None);
        progress.observe(b"yt-dlp-tg-progress1|137|finished|NA|100|NA");
        assert!(progress.render(english()).contains("99%"));
        assert!(!progress.render(english()).contains("100%"));
        progress.download_finished();
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|20|100|NA");
        assert!(
            progress
                .render(english())
                .starts_with("✓ Downloading…\n██████████ 100% · 100 B")
        );
    }

    #[test]
    fn completion_without_frames_never_invents_observed_bytes() {
        [None, Some(Size::Exact(100))].into_iter().for_each(|hint| {
            let progress = Progress::new(hint);
            progress.download_finished();
            let rendered = progress.render(english());
            assert!(rendered.starts_with("✓ Downloading…\n██████████"));
            assert!(!rendered.contains(" B"));
        });
    }

    #[test]
    fn cached_rows_claim_no_transfer_even_if_counters_are_called() {
        ["en", "ru"].into_iter().for_each(|code| {
            let language = Language::from_code(Some(code));
            let progress = Progress::cached();
            progress.observe(b"yt-dlp-tg-progress1|137|downloading|20|100|NA");
            progress.sending(Some(100));
            progress.uploaded(100);
            let rendered = progress.render(language);
            assert!(rendered.starts_with(&format!("✓ {}\n", language.text(Text::Downloading))));
            assert!(rendered.contains(&format!("▶ {}\n", language.text(Text::Sending))));
            assert_eq!(
                rendered
                    .matches(language.text(Text::ProgressCached))
                    .count(),
                2
            );
            assert!(!rendered.contains('%'));
            assert!(!rendered.contains(" B"));
        });
    }

    #[test]
    fn stream_storage_is_bounded_and_existing_streams_still_update() {
        let progress = Progress::new(None);
        (0..=MAX_STREAMS).for_each(|id| {
            progress.observe(format!("yt-dlp-tg-progress1|{id}|downloading|1|10|NA").as_bytes());
        });
        assert_eq!(progress.0.download.borrow().streams.len(), MAX_STREAMS);
        progress.observe(b"yt-dlp-tg-progress1|0|downloading|5|10|NA");
        assert_eq!(progress.0.download.borrow().sample().done, Some(20));
    }

    #[test]
    fn estimates_are_replaced_by_exact_totals_and_counters_do_not_regress() {
        let progress = Progress::new(None);
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|20|NA|200");
        assert!(progress.render(english()).contains("~10%"));
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|15|100|NA");
        progress.observe(b"yt-dlp-tg-progress1|140|downloading|10|NA|300");
        assert!(progress.render(english()).contains("20% · 20 B / 100 B"));
        assert!(!progress.render(english()).contains('~'));
    }

    #[test]
    fn sending_resets_and_saturates_without_overflow_or_zero_division() {
        let progress = Progress::new(None);
        progress.sending(Some(10));
        progress.uploaded(5);
        assert!(progress.render(english()).contains("50% · 5 B / 10 B"));
        progress.sending(Some(20));
        assert!(progress.render(english()).contains("0% · 0 B / 20 B"));
        progress.uploaded(usize::MAX);
        progress.uploaded(usize::MAX);
        assert!(progress.render(english()).contains("100%"));
        progress.sending(Some(0));
        progress.uploaded(3);
        let rendered = progress.render(english());
        assert!(!rendered.contains('%'));
        assert!(rendered.contains("3 B"));
    }

    #[test]
    fn elapsed_time_and_indeterminate_animation_advance_on_ticks() {
        let mut progress = Progress::new(Some(Size::Exact(100)));
        let initial = progress.render(english());
        Rc::get_mut(&mut progress.0)
            .expect("test owns the sole state reference")
            .started -= Duration::from_millis(3500);
        let later = progress.render(english());
        assert_ne!(initial, later);
        assert!(later.contains("⏱ 00:00:03"));
        assert_ne!(
            transfer_value(
                Sample {
                    done: None,
                    total: None
                },
                Transfer::Downloading,
                0
            ),
            transfer_value(
                Sample {
                    done: None,
                    total: None
                },
                Transfer::Downloading,
                1
            ),
        );
    }

    #[test]
    fn cloned_state_shares_only_one_cosmetic_warning() {
        let progress = Progress::new(None);
        assert!(progress.warn_once());
        assert!(!progress.clone().warn_once());
        assert!(!progress.warn_once());
    }
}
