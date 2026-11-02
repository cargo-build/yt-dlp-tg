//! Pure per-resolution availability planning from yt-dlp metadata; planned
//! sizes guide the menu while the download-time guard stays authoritative. @req DL-5

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// Resolutions above this height are never offered. @req DL-5
const MAX_HEIGHT: u32 = 1080;
/// Menu heights always listed, even when the source lacks them. @req DL-5
const STANDARD_HEIGHTS: [u32; 6] = [1080, 720, 480, 360, 240, 144];
/// A kilobit is 125 bytes; yt-dlp reports bitrates in kbps.
const BYTES_PER_KILOBIT: f64 = 125.0;
/// f64 integers are exact only to 2^53; larger sizes are overflow, not metadata.
const MAX_PLANNED_BYTES: f64 = 9_007_199_254_740_992.0;
/// Selector-embedded IDs beyond this length are metadata abuse.
const MAX_ID_LENGTH: usize = 100;

/// A byte size pinned by metadata or conservatively estimated. @req DL-5
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Size {
    Exact(u64),
    Estimated(u64),
}

impl Size {
    /// The planned size in bytes regardless of confidence. @req DL-5
    pub fn bytes(&self) -> u64 {
        match self {
            Size::Exact(bytes) | Size::Estimated(bytes) => *bytes,
        }
    }
}

/// A pinned yt-dlp selector and the size it is planned to produce. @req DL-5
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Selection {
    pub selector: String,
    pub size: Size,
}

/// One resolution's outcome against the configured upload limit. @req DL-5
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available(Selection),
    TooLarge(Size),
    Unknown,
    Missing,
}

/// One menu row: a resolution and its planned availability. @req DL-5
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub height: u32,
    pub availability: Availability,
}

/// Plan descending per-resolution choices for one already validated
/// single-item metadata root; pure metadata inspection, no I/O. @req DL-5
pub fn choices(metadata: &serde_json::Value, max_bytes: u64) -> Result<Vec<Choice>> {
    let formats = format_list(metadata)?;
    let duration = metadata.get("duration").and_then(positive_f64);
    let parsed: Vec<PlanFormat<'_>> = formats
        .iter()
        .enumerate()
        .filter_map(|(position, value)| parse_format(value, duration, position))
        .collect();
    let audios: Vec<&PlanFormat<'_>> = parsed
        .iter()
        .filter(|format| matches!(format.stream, Stream::Audio(..)))
        .collect();
    let has_audio = formats.iter().any(|format| !proves_no_audio(format));
    let mut heights = STANDARD_HEIGHTS.to_vec();
    heights.extend(actual_heights(&formats));
    heights.sort_unstable_by(|left, right| right.cmp(left));
    heights.dedup();
    Ok(heights
        .into_iter()
        .map(|height| Choice {
            height,
            availability: plan_height(&parsed, &audios, has_audio, height, max_bytes),
        })
        .collect())
}

#[derive(Debug, Clone, Copy)]
struct PlanFormat<'a> {
    position: usize,
    id: &'a str,
    height: Option<u32>,
    stream: Stream,
    size: Option<Size>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Video(VideoCodec),
    Audio(AudioCodec),
    Muxed(VideoCodec, AudioCodec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoCodec {
    H264,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCodec {
    Aac,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// H.264 video with AAC audio, or H.264 alone when the source is silent.
    Preferred,
    /// Any other classifiable combination, remuxable to MKV without re-encoding.
    Alternative,
}

fn format_list(metadata: &Value) -> Result<Vec<&Value>> {
    let Some(root) = metadata.as_object() else {
        return Err(Error::YtDlp("media metadata must be an object".into()));
    };
    match root.get("formats") {
        Some(Value::Array(formats)) => Ok(formats.iter().collect()),
        Some(Value::Null) | None => match root.get("format_id") {
            Some(_) => Ok(vec![metadata]),
            None => Ok(Vec::new()),
        },
        Some(_) => Err(Error::YtDlp("media formats must be an array".into())),
    }
}

fn parse_format(value: &Value, duration: Option<f64>, position: usize) -> Option<PlanFormat<'_>> {
    let object = value.as_object()?;
    let id = object.get("format_id").and_then(Value::as_str)?;
    if !safe_id(id) || drm(object.get("has_drm")) {
        return None;
    }
    let vcodec = object.get("vcodec").and_then(Value::as_str)?;
    if storyboard(object, vcodec) {
        return None;
    }
    let acodec = object.get("acodec").and_then(Value::as_str)?;
    Some(PlanFormat {
        position,
        id,
        height: object.get("height").and_then(format_height),
        stream: stream(vcodec, acodec)?,
        size: format_size(object, duration),
    })
}

/// Any `has_drm` value other than absent/null/false disqualifies the format. @req DL-5
fn drm(has_drm: Option<&Value>) -> bool {
    match has_drm {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

fn safe_id(id: &str) -> bool {
    let allowed =
        |character: char| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.');
    id.len() <= MAX_ID_LENGTH
        && id
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && id
            .chars()
            .last()
            .is_some_and(|last| last.is_ascii_alphanumeric())
        && id.chars().all(allowed)
}

fn storyboard(object: &Map<String, Value>, vcodec: &str) -> bool {
    let image = ["mjpeg", "jpeg", "jpg", "png"]
        .iter()
        .any(|codec| vcodec.eq_ignore_ascii_case(codec));
    let mhtml = object
        .get("protocol")
        .and_then(Value::as_str)
        .is_some_and(|protocol| protocol.eq_ignore_ascii_case("mhtml"));
    let note = object
        .get("format_note")
        .and_then(Value::as_str)
        .is_some_and(|note| note.to_ascii_lowercase().contains("storyboard"));
    image || mhtml || note
}

fn video_codec(vcodec: &str) -> Option<VideoCodec> {
    let codec = vcodec.to_ascii_lowercase();
    match codec.as_str() {
        "" | "none" | "unknown" => None,
        _ => Some(
            match codec.starts_with("avc1") || codec.starts_with("avc3") || codec == "h264" {
                true => VideoCodec::H264,
                false => VideoCodec::Other,
            },
        ),
    }
}

fn audio_codec(acodec: &str) -> Option<AudioCodec> {
    let codec = acodec.to_ascii_lowercase();
    match codec.as_str() {
        "" | "none" | "unknown" => None,
        _ => Some(match codec.starts_with("mp4a") || codec == "aac" {
            true => AudioCodec::Aac,
            false => AudioCodec::Other,
        }),
    }
}

/// A format is only classifiable when its codec fields are explicit: `none`
/// marks a missing track, while absent or `unknown` codecs leave pairing
/// unsafe, so the format is skipped instead of guessed. @req DL-5
fn stream(vcodec: &str, acodec: &str) -> Option<Stream> {
    match (video_codec(vcodec), audio_codec(acodec)) {
        (Some(video), Some(audio)) => Some(Stream::Muxed(video, audio)),
        (Some(video), None) => absent(acodec).then_some(Stream::Video(video)),
        (None, Some(audio)) => absent(vcodec).then_some(Stream::Audio(audio)),
        (None, None) => None,
    }
}

fn absent(codec: &str) -> bool {
    codec.eq_ignore_ascii_case("none")
}

/// Exact filesize wins over `filesize_approx`, then a duration×bitrate
/// ceiling estimate; nonfinite or overflowing values stay unknown. @req DL-5
fn format_size(object: &Map<String, Value>, duration: Option<f64>) -> Option<Size> {
    if let Some(exact) = object.get("filesize").and_then(positive_u64) {
        return Some(Size::Exact(exact));
    }
    if let Some(approx) = object.get("filesize_approx").and_then(positive_u64) {
        return Some(Size::Estimated(approx));
    }
    let bitrate = object
        .get("tbr")
        .and_then(positive_f64)
        .or_else(|| object.get("abr").and_then(positive_f64))?;
    let duration = duration?;
    let bytes = bitrate * duration * BYTES_PER_KILOBIT;
    match bytes.is_finite() && bytes <= MAX_PLANNED_BYTES {
        true => Some(Size::Estimated(bytes.ceil() as u64)),
        false => None,
    }
}

fn positive_u64(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    if let Some(bytes) = number.as_u64() {
        return (bytes > 0).then_some(bytes);
    }
    if let Some(signed) = number.as_i64() {
        return u64::try_from(signed).ok().filter(|bytes| *bytes > 0);
    }
    let parsed = number.as_f64()?;
    (parsed.is_finite() && parsed > 0.0 && parsed <= MAX_PLANNED_BYTES)
        .then(|| parsed.ceil() as u64)
}

fn positive_f64(value: &Value) -> Option<f64> {
    let parsed = value.as_number()?.as_f64()?;
    (parsed.is_finite() && parsed > 0.0).then_some(parsed)
}

fn format_height(value: &Value) -> Option<u32> {
    let number = value.as_number()?;
    if let Some(height) = number.as_u64() {
        return u32::try_from(height)
            .ok()
            .filter(|height| (1..=MAX_HEIGHT).contains(height));
    }
    let height = number.as_f64()?;
    match height.is_finite()
        && height.fract() == 0.0
        && (1.0..=f64::from(MAX_HEIGHT)).contains(&height)
    {
        true => Some(height as u32),
        false => None,
    }
}

fn actual_heights(formats: &[&Value]) -> impl Iterator<Item = u32> {
    formats.iter().filter_map(|value| {
        let object = value.as_object()?;
        let vcodec = object.get("vcodec").and_then(Value::as_str)?;
        match video_row(object, vcodec) {
            true => object.get("height").and_then(format_height),
            false => None,
        }
    })
}

/// Rows come from any real video track, even DRM-protected or unclassifiable
/// ones: the menu must show such heights as unavailable, not hide them. @req DL-5
fn video_row(object: &Map<String, Value>, vcodec: &str) -> bool {
    !(storyboard(object, vcodec) || vcodec.eq_ignore_ascii_case("none") || vcodec.is_empty())
}

/// An entry proves the source has no audio only when its `acodec` explicitly
/// reads `none`; missing or `unknown` values stay conservative, and so do
/// raw formats the planner itself cannot select (DRM, unsafe IDs). @req DL-5
fn proves_no_audio(format: &Value) -> bool {
    format
        .get("acodec")
        .and_then(Value::as_str)
        .is_some_and(|acodec| absent(acodec) && !acodec.is_empty())
}

/// Scan one height's candidates: preferred H.264/AAC first, then alternative
/// codecs, best quality first inside each tier, and never a muted selection
/// while any raw format still shows audio. @req DL-5
fn plan_height(
    formats: &[PlanFormat<'_>],
    audios: &[&PlanFormat<'_>],
    has_audio: bool,
    height: u32,
    max_bytes: u64,
) -> Availability {
    let mut video_like: Vec<&PlanFormat<'_>> = formats
        .iter()
        .filter(|format| {
            format.height == Some(height)
                && matches!(format.stream, Stream::Video(..) | Stream::Muxed(..))
        })
        .collect();
    video_like.sort_by_key(|format| std::cmp::Reverse(format.position));

    let mut budget = Budget {
        max_bytes,
        smallest_over: None,
        saw_unknown: false,
    };
    for tier in [Tier::Preferred, Tier::Alternative] {
        for format in &video_like {
            match format.stream {
                Stream::Muxed(video, audio) => {
                    if pair_tier(video, audio) != tier {
                        continue;
                    }
                    if let Some(size) = budget.offer(format.size) {
                        return single_selection(format, height, size);
                    }
                }
                Stream::Video(video) => match audios.is_empty() {
                    true => {
                        if has_audio || single_tier(video) != tier {
                            continue;
                        }
                        if let Some(size) = budget.offer(format.size) {
                            return single_selection(format, height, size);
                        }
                    }
                    false => {
                        for audio in audios.iter().rev() {
                            let Stream::Audio(codec) = audio.stream else {
                                continue;
                            };
                            if pair_tier(video, codec) != tier {
                                continue;
                            }
                            if let Some(size) = budget.offer(combine(format.size, audio.size)) {
                                return pair_selection(format, audio, height, size);
                            }
                        }
                    }
                },
                // Excluded from `video_like` by the filter above.
                Stream::Audio(_) => {}
            }
        }
    }
    budget.availability()
}

/// Streaming fit scan: the first fitting candidate wins; otherwise an unknown
/// marker outranks the smallest known-oversized size. @req DL-5
struct Budget {
    max_bytes: u64,
    smallest_over: Option<Size>,
    saw_unknown: bool,
}

impl Budget {
    fn offer(&mut self, size: Option<Size>) -> Option<Size> {
        match size {
            None => {
                self.saw_unknown = true;
                None
            }
            Some(size) => match size.bytes() <= self.max_bytes {
                true => Some(size),
                false => {
                    self.smallest_over = match self.smallest_over {
                        Some(current) if current.bytes() <= size.bytes() => Some(current),
                        _ => Some(size),
                    };
                    None
                }
            },
        }
    }

    fn availability(self) -> Availability {
        match self.saw_unknown {
            true => Availability::Unknown,
            false => match self.smallest_over {
                Some(size) => Availability::TooLarge(size),
                None => Availability::Missing,
            },
        }
    }
}

/// Video-only plus audio sums both components; an exact sum needs both inputs
/// exact, and any overflow stays unknown. @req DL-5
fn combine(video: Option<Size>, audio: Option<Size>) -> Option<Size> {
    let video = video?;
    let audio = audio?;
    let total = video.bytes().checked_add(audio.bytes())?;
    Some(match (video, audio) {
        (Size::Exact(_), Size::Exact(_)) => Size::Exact(total),
        _ => Size::Estimated(total),
    })
}

fn pair_tier(video: VideoCodec, audio: AudioCodec) -> Tier {
    match (video, audio) {
        (VideoCodec::H264, AudioCodec::Aac) => Tier::Preferred,
        _ => Tier::Alternative,
    }
}

fn single_tier(video: VideoCodec) -> Tier {
    match video {
        VideoCodec::H264 => Tier::Preferred,
        VideoCodec::Other => Tier::Alternative,
    }
}

fn single_selection(format: &PlanFormat<'_>, height: u32, size: Size) -> Availability {
    Availability::Available(Selection {
        selector: format!("all[format_id={}][height={}]", format.id, height),
        size,
    })
}

fn pair_selection(
    video: &PlanFormat<'_>,
    audio: &PlanFormat<'_>,
    height: u32,
    size: Size,
) -> Availability {
    Availability::Available(Selection {
        selector: format!(
            "all[format_id={}][height={}]+all[format_id={}]",
            video.id, height, audio.id
        ),
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selector_like_ids_are_pinned_as_literal_metadata_ids() {
        let metadata = json!({"formats":[{"format_id":"best","height":720,"vcodec":"avc1","acodec":"mp4a","filesize":1000}]});
        let menu = choices(&metadata, 2000).expect("known format");
        let choice = menu
            .iter()
            .find(|choice| choice.height == 720)
            .expect("720 row");
        assert!(
            matches!(&choice.availability, Availability::Available(selection) if selection.selector == "all[format_id=best][height=720]")
        );
    }

    fn video_format(id: &str, height: u64, filesize: u64) -> Value {
        json!({
            "format_id": id,
            "height": height,
            "vcodec": "avc1.640028",
            "acodec": "none",
            "filesize": filesize,
        })
    }

    fn audio_format(id: &str, filesize: u64) -> Value {
        json!({
            "format_id": id,
            "vcodec": "none",
            "acodec": "mp4a.40.2",
            "filesize": filesize,
        })
    }

    fn muxed_format(id: &str, height: u64, filesize: u64) -> Value {
        json!({
            "format_id": id,
            "height": height,
            "vcodec": "avc1.77.30",
            "acodec": "mp4a.40.2",
            "filesize": filesize,
        })
    }

    fn metadata(formats: Vec<Value>) -> Value {
        json!({"title": "source", "duration": 60.0, "formats": formats})
    }

    fn row(choices: &[Choice], height: u32) -> &Availability {
        &choices
            .iter()
            .find(|choice| choice.height == height)
            .expect("standard height row exists")
            .availability
    }

    #[test]
    fn marks_oversized_heights_too_large_and_fitting_heights_available() {
        let metadata = metadata(vec![
            muxed_format("22", 720, 900_000),
            video_format("137", 1080, 1_900_000),
            video_format("136", 720, 800_000),
            audio_format("140", 200_000),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        let heights: Vec<u32> = choices.iter().map(|choice| choice.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360, 240, 144]);
        // The 1080 aggregate video+audio size busts the limit.
        assert_eq!(
            row(&choices, 1080),
            &Availability::TooLarge(Size::Exact(2_100_000))
        );
        // The 720 pair hits the boundary exactly and fits.
        assert_eq!(
            row(&choices, 720),
            &Availability::Available(Selection {
                selector: "all[format_id=136][height=720]+all[format_id=140]".into(),
                size: Size::Exact(1_000_000),
            })
        );
        assert_eq!(row(&choices, 480), &Availability::Missing);
    }

    #[test]
    fn keeps_progressive_size_without_double_counting_audio() {
        let metadata = metadata(vec![muxed_format("22", 720, 1_000_000)]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 720),
            &Availability::Available(Selection {
                selector: "all[format_id=22][height=720]".into(),
                size: Size::Exact(1_000_000),
            })
        );
    }

    #[test]
    fn falls_back_to_a_smaller_video_at_the_same_height() {
        let metadata = metadata(vec![
            video_format("135-small", 480, 700_000),
            video_format("135-large", 480, 5_000_000),
            audio_format("140", 200_000),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 480),
            &Availability::Available(Selection {
                selector: "all[format_id=135-small][height=480]+all[format_id=140]".into(),
                size: Size::Exact(900_000),
            })
        );
    }

    #[test]
    fn tries_smaller_audio_combinations_when_the_best_pair_is_over_budget() {
        let metadata = metadata(vec![
            video_format("137", 1080, 1_800_000),
            audio_format("139", 100_000),
            audio_format("140", 300_000),
        ]);
        let choices = choices(&metadata, 2_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 1080),
            &Availability::Available(Selection {
                selector: "all[format_id=137][height=1080]+all[format_id=139]".into(),
                size: Size::Exact(1_900_000),
            })
        );
    }

    #[test]
    fn reports_unknown_when_usable_sizes_cannot_be_determined() {
        let metadata = metadata(vec![
            json!({"format_id": "43", "height": 360, "vcodec": "vp9", "acodec": "vorbis"}),
            video_format("134", 360, 4_000_000),
            audio_format("140", 100_000),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        // Unknown outranks TooLarge: the unsized candidate might still fit.
        assert_eq!(row(&choices, 360), &Availability::Unknown);
    }

    #[test]
    fn never_offers_unknown_heights_or_resolutions_above_1080() {
        let metadata = metadata(vec![
            json!({"format_id": "999", "vcodec": "avc1.640028", "acodec": "none", "filesize": 100}),
            json!({"format_id": "998", "height": 2160, "vcodec": "vp9", "acodec": "none", "filesize": 100}),
            video_format("135", 480, 100),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        let heights: Vec<u32> = choices.iter().map(|choice| choice.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360, 240, 144]);
        assert_eq!(row(&choices, 1080), &Availability::Missing);
        assert_eq!(row(&choices, 720), &Availability::Missing);
    }

    #[test]
    fn lists_nonstandard_heights_below_the_cap_in_descending_order() {
        let metadata = metadata(vec![
            json!({"format_id": "616", "height": 1440, "vcodec": "vp9", "acodec": "none", "filesize": 100}),
            json!({"format_id": "612", "height": 900, "vcodec": "vp9", "acodec": "none", "filesize": 100}),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        let heights: Vec<u32> = choices.iter().map(|choice| choice.height).collect();
        assert_eq!(heights, vec![1080, 900, 720, 480, 360, 240, 144]);
    }

    #[test]
    fn skips_unsafe_ids_drm_and_storyboard_formats() {
        let metadata = metadata(vec![
            json!({
                "format_id": "sb0",
                "height": 480,
                "vcodec": "mjpeg",
                "acodec": "none",
                "protocol": "mhtml",
                "filesize": 100,
            }),
            json!({
                "format_id": "18;bestaudio",
                "height": 360,
                "vcodec": "avc1.77.30",
                "acodec": "mp4a.40.2",
                "filesize": 100,
            }),
            json!({
                "format_id": "drm",
                "height": 240,
                "vcodec": "avc1.77.30",
                "acodec": "mp4a.40.2",
                "has_drm": true,
                "filesize": 100,
            }),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        let heights: Vec<u32> = choices.iter().map(|choice| choice.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360, 240, 144]);
        assert_eq!(row(&choices, 480), &Availability::Missing);
        assert_eq!(row(&choices, 360), &Availability::Missing);
        assert_eq!(row(&choices, 240), &Availability::Missing);
    }

    #[test]
    fn rejects_nonfinite_and_overflowing_estimates() {
        let metadata = json!({
            "title": "source",
            "duration": 1e308,
            "formats": [
                {"format_id": "137", "height": 1080, "vcodec": "avc1.640028", "acodec": "none", "tbr": 1e308},
                {"format_id": "136", "height": 720, "vcodec": "avc1.640028", "acodec": "none", "filesize": 2e19},
            ],
        });
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(row(&choices, 1080), &Availability::Unknown);
        assert_eq!(row(&choices, 720), &Availability::Unknown);
    }

    #[test]
    fn estimates_from_positive_duration_and_bitrate_with_ceiling() {
        let metadata = json!({
            "title": "source",
            "duration": 1.0,
            "formats": [
                {"format_id": "160", "vcodec": "none", "acodec": "mp4a.40.2", "tbr": 0.5},
                {"format_id": "133", "height": 240, "vcodec": "avc1.77.30", "acodec": "none", "tbr": 1000.0},
            ],
        });
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        // 0.5 kbit × 125 bytes/kbit = 62.5 → 63; 1000 kbit × 125 = 125000.
        assert_eq!(
            row(&choices, 240),
            &Availability::Available(Selection {
                selector: "all[format_id=133][height=240]+all[format_id=160]".into(),
                size: Size::Estimated(125_063),
            })
        );
    }

    #[test]
    fn never_mutes_known_audio_to_make_a_pair_fit() {
        let metadata = metadata(vec![
            video_format("137", 1080, 900_000),
            audio_format("140", 200_000),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 1080),
            &Availability::TooLarge(Size::Exact(1_100_000))
        );
    }

    #[test]
    fn does_not_offer_muted_video_when_raw_audio_is_unselectable() {
        let metadata = metadata(vec![
            video_format("135", 480, 500_000),
            json!({
                "format_id": "140",
                "vcodec": "none",
                "acodec": "mp4a.40.2",
                "has_drm": true,
                "filesize": 100_000,
            }),
            video_format("134", 360, 400_000),
            json!({"format_id": "250", "vcodec": "none", "acodec": "unknown"}),
        ]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        // DRM-protected and unclassifiable audio still prove the source has
        // sound, so neither height may degrade to a muted Available row.
        assert_eq!(row(&choices, 480), &Availability::Missing);
        assert_eq!(row(&choices, 360), &Availability::Missing);
    }

    #[test]
    fn allows_silent_video_only_when_audio_is_proven_absent() {
        let metadata = metadata(vec![video_format("135", 480, 500_000)]);
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 480),
            &Availability::Available(Selection {
                selector: "all[format_id=135][height=480]".into(),
                size: Size::Exact(500_000),
            })
        );
    }

    #[test]
    fn consumes_a_usable_top_level_single_format() {
        let metadata = json!({
            "format_id": "22",
            "height": 720,
            "vcodec": "avc1.77.30",
            "acodec": "mp4a.40.2",
            "filesize": 100,
        });
        let choices = choices(&metadata, 1_000_000).expect("planner output");
        assert_eq!(
            row(&choices, 720),
            &Availability::Available(Selection {
                selector: "all[format_id=22][height=720]".into(),
                size: Size::Exact(100),
            })
        );
    }

    #[test]
    fn rejects_malformed_roots_and_skips_invalid_entries() {
        assert!(
            choices(&json!("not an object"), 1_000).is_err(),
            "metadata must be an object"
        );
        assert!(
            choices(&json!({"formats": "nope"}), 1_000).is_err(),
            "formats must be an array"
        );
        let metadata = metadata(vec![json!(null), json!(42), muxed_format("22", 720, 100)]);
        let choices = choices(&metadata, 1_000).expect("planner output");
        assert_eq!(
            row(&choices, 720),
            &Availability::Available(Selection {
                selector: "all[format_id=22][height=720]".into(),
                size: Size::Exact(100),
            })
        );
    }
}
