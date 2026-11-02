//! Bounded normalization of one downloaded video into a playable MP4. @req DL-4 DL-5 DL-8

use std::{
    ffi::OsString,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use compio::process::Command;
use serde::Deserialize;

use crate::{
    config::Config,
    error::{Error, Result},
    process,
};

/// Pinned video height cap, shared with the selection planner. @req DL-5
const MAX_HEIGHT: u32 = 1080;

/// ffprobe stdout bound; stream metadata is small. @req DL-8
const MAX_CHILD_STDOUT_BYTES: usize = 64 * 1024;

/// Probe entries that decide stream handling and validate the final file. @req DL-4
const PROBE_FIELDS: &str = "stream=index,codec_type,codec_name,pix_fmt,width,height,channels,color_transfer:stream_disposition=attached_pic:stream_side_data=side_data_type,rotation:format=format_name,duration";

/// Side-data name carrying the display matrix that rotates the visible frame.
const DISPLAY_MATRIX: &str = "Display Matrix";

/// Deliverable stream contract: H.264 8-bit 4:2:0 video and AAC audio. @req DL-4
const VIDEO_CODEC: &str = "h264";
const PIXEL_FORMAT: &str = "yuv420p";
const AUDIO_CODEC: &str = "aac";
const MP4_CONTAINER: &str = "mp4";

/// CPU encoder settings for video that is not already deliverable. @req DL-4
const ENCODER: &str = "libx264";
const PRESET: &str = "veryfast";
const CRF: &str = "20";
const AUDIO_BITRATE: &str = "192k";
const THREADS: &str = "1";

/// Transfer characteristics that 8-bit yuv420p output would wash out. @req DL-4
const HDR_TRANSFERS: [&str; 2] = ["smpte2084", "arib-std-b67"];

/// Names owned by the downloader's one-file job-directory invariant.
const SOURCE_STEM: &str = "media";
const SOURCE_EXTENSIONS: [&str; 2] = ["mp4", "mkv"];
const TEMPORARY_FILE: &str = "normalized.mp4";
const FINAL_FILE: &str = "media.mp4";
const FINAL_EXTENSIONS: &[&str] = &["mp4"];

/// Real MP4 files carry a handful of top-level boxes; the scan is capped. @req DL-4
const MAX_TOP_LEVEL_BOXES: usize = 256;

// Safe, data-free diagnostics: none carries a path, URL, cookie or child stderr. @req DL-8
const PROBE_INVALID: &str = "media probe data is incomplete or invalid";
const SOURCE_INVALID: &str = "normalization input is not a downloaded media file";
const VIDEO_HEIGHT: &str = "media video height is outside the selected resolution";
const VIDEO_SIZE: &str = "media dimensions are unsupported for MP4 output";
const HDR_UNSUPPORTED: &str = "HDR media is unsupported";
const OUTPUT_INVALID: &str = "normalized media does not satisfy the playable MP4 contract";
const FASTSTART_MISSING: &str = "normalized media is not streamable";
const FASTSTART_INVALID: &str = "normalized media index is unreadable";
const DISK_INVALID: &str = "normalization disk limit is invalid";
const BLOCKING_FAILED: &str = "media normalization filesystem task failed";

/// One probed media file; parsing proves it is finite media with one video. @req DL-4
#[derive(Debug)]
struct Probe {
    /// ffprobe format name list, for example `mov,mp4,m4a,3gp,3g2,mj2`.
    container: String,
    video: VideoStream,
    audio: Vec<AudioStream>,
}

/// The single real (not cover-art) video stream of a probed file.
#[derive(Debug)]
struct VideoStream {
    index: u32,
    codec: String,
    pix_fmt: String,
    width: u32,
    height: u32,
    /// Display rotation in degrees; no display matrix is reported as none.
    rotation: i64,
    color_transfer: Option<String>,
}

/// One audio stream with the channel count an encoded replacement must keep.
#[derive(Debug)]
struct AudioStream {
    index: u32,
    codec: String,
    channels: u32,
}

/// Planned ffmpeg streams and paths for the one normalization child.
struct Conversion<'a> {
    input: &'a Path,
    output: &'a Path,
    video: &'a VideoStream,
    audio: &'a [AudioStream],
}

/// A top-level ISO-BMFF box header; a zero size spans the rest of the file.
struct BoxHeader {
    kind: [u8; 4],
    size: u64,
}

/// Turn the downloaded source into the single validated `media.mp4`. @req DL-4 DL-5
///
/// The source stays in place until the normalized file passes the stream,
/// dimension, size and faststart checks; any failure returns to the caller's
/// whole-job cleanup and never exposes a partial file.
pub(super) async fn ensure_playable(
    config: &Config,
    directory: &Path,
    path: PathBuf,
    height: u32,
    deadline: Instant,
) -> Result<PathBuf> {
    if !(1..=MAX_HEIGHT).contains(&height) {
        return Err(Error::YtDlp(VIDEO_HEIGHT.into()));
    }
    let probe_timeout = Duration::from_secs(config.process_timeout_secs);
    let directory = {
        let directory = directory.to_owned();
        let source = path.clone();
        blocking(move || -> Result<PathBuf> {
            let directory = directory.canonicalize()?;
            validate_source(&directory, &source)?;
            Ok(directory)
        })
        .await?
    };
    let input = inspect(&path, child_deadline(deadline, Some(probe_timeout))?).await?;
    validate_video(&input.video, height)?;

    let temporary = directory.join(TEMPORARY_FILE);
    run_conversion(config, &directory, &path, &temporary, &input, deadline).await?;

    let output = inspect(&temporary, child_deadline(deadline, Some(probe_timeout))?).await?;
    validate_output(&input, &output, height)?;

    let limit = config.max_upload_bytes;
    let checked = temporary.clone();
    let (size, streamable) = blocking(move || -> Result<(u64, bool)> {
        let size = std::fs::symlink_metadata(&checked)?.len();
        Ok((size, faststart(&checked)?))
    })
    .await?;
    if size == 0 {
        return Err(Error::YtDlp(OUTPUT_INVALID.into()));
    }
    if size > limit {
        return Err(Error::TooLarge);
    }
    if !streamable {
        return Err(Error::YtDlp(FASTSTART_MISSING.into()));
    }

    let final_path = directory.join(FINAL_FILE);
    blocking(move || -> Result<PathBuf> {
        std::fs::remove_file(&path)?;
        std::fs::rename(&temporary, &final_path)?;
        super::validate_download(&directory, &final_path, FINAL_EXTENSIONS, limit)
    })
    .await
}

/// Keep the normalizer on the downloader's one-file `media.*` source. @req DL-5
///
/// The caller supplies the canonical private job directory the downloader
/// validated before extraction.
fn validate_source(directory: &Path, path: &Path) -> Result<()> {
    let canonical = path.canonicalize()?;
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str());
    let metadata = std::fs::symlink_metadata(&canonical)?;
    let valid = canonical.parent() == Some(directory)
        && canonical
            .file_stem()
            .is_some_and(|stem| stem == SOURCE_STEM)
        && extension.is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
        && metadata.is_file();
    if !valid {
        return Err(Error::YtDlp(SOURCE_INVALID.into()));
    }
    Ok(())
}

/// Reject sizes, heights and HDR that the pinned MP4 contract cannot preserve. @req DL-4 DL-5
fn validate_video(video: &VideoStream, height: u32) -> Result<()> {
    let (_, display_height) = display_dimensions(video.width, video.height, video.rotation)?;
    if display_height > height || display_height > MAX_HEIGHT {
        return Err(Error::YtDlp(VIDEO_HEIGHT.into()));
    }
    if !video.width.is_multiple_of(2) || !video.height.is_multiple_of(2) {
        return Err(Error::YtDlp(VIDEO_SIZE.into()));
    }
    if video
        .color_transfer
        .as_deref()
        .is_some_and(|transfer| HDR_TRANSFERS.contains(&transfer))
    {
        return Err(Error::YtDlp(HDR_UNSUPPORTED.into()));
    }
    Ok(())
}

/// Dimensions a player shows after applying the display matrix.
fn display_dimensions(width: u32, height: u32, rotation: i64) -> Result<(u32, u32)> {
    match rotation.rem_euclid(360) {
        0 | 180 => Ok((width, height)),
        90 | 270 => Ok((height, width)),
        _ => Err(Error::YtDlp(VIDEO_SIZE.into())),
    }
}

/// Require the normalized file to be the deliverable MP4 of the same picture. @req DL-4
fn validate_output(input: &Probe, output: &Probe, height: u32) -> Result<()> {
    if !output
        .container
        .split(',')
        .any(|name| name == MP4_CONTAINER)
    {
        return Err(Error::YtDlp(OUTPUT_INVALID.into()));
    }
    validate_video(&output.video, height)?;
    if output.video.codec != VIDEO_CODEC || output.video.pix_fmt != PIXEL_FORMAT {
        return Err(Error::YtDlp(OUTPUT_INVALID.into()));
    }
    let source = display_dimensions(input.video.width, input.video.height, input.video.rotation)?;
    let result = display_dimensions(
        output.video.width,
        output.video.height,
        output.video.rotation,
    )?;
    if source != result {
        return Err(Error::YtDlp(OUTPUT_INVALID.into()));
    }
    validate_audio(&input.audio, &output.audio)
}

/// Every source audio track survives as one AAC track with the same channels. @req DL-4
fn validate_audio(source: &[AudioStream], result: &[AudioStream]) -> Result<()> {
    let preserved = source.len() == result.len()
        && source.iter().zip(result).all(|(source, result)| {
            result.codec == AUDIO_CODEC && result.channels == source.channels
        });
    if !preserved {
        return Err(Error::YtDlp(OUTPUT_INVALID.into()));
    }
    Ok(())
}

/// Run one bounded ffprobe child and require the parsed contract. @req DL-4 DL-8
async fn inspect(path: &Path, deadline: Duration) -> Result<Probe> {
    let mut command = Command::new("ffprobe");
    command.args(probe_arguments(path));
    let output = process::run(&mut command, deadline, MAX_CHILD_STDOUT_BYTES).await?;
    parse_probe(&output)
}

fn probe_arguments(path: &Path) -> Vec<OsString> {
    let mut arguments = ["-v", "error", "-show_entries", PROBE_FIELDS, "-of", "json"]
        .map(OsString::from)
        .to_vec();
    arguments.push(path.as_os_str().to_owned());
    arguments
}

/// Parse and validate one ffprobe document. @req DL-4
///
/// The format duration is required to prove finite media and is validated here
/// rather than retained; a live or unprobeable input fails explicitly.
fn parse_probe(output: &[u8]) -> Result<Probe> {
    let document: ProbeDocument = serde_json::from_slice(output).map_err(|_| probe_invalid())?;
    let format = document.format.ok_or_else(probe_invalid)?;
    let container = format.format_name.ok_or_else(probe_invalid)?;
    let duration = format
        .duration
        .and_then(DurationValue::seconds)
        .ok_or_else(probe_invalid)?;
    if container.is_empty() || !duration.is_finite() || duration <= 0.0 {
        return Err(probe_invalid());
    }
    let mut video = None;
    let mut audio = Vec::new();
    for mut stream in document.streams {
        let attached_pic = stream.attached_pic();
        let codec_type = stream.codec_type.take();
        match codec_type.as_deref() {
            Some("video") if !attached_pic => {
                if video.is_some() {
                    return Err(probe_invalid());
                }
                video = Some(video_stream(stream)?);
            }
            Some("audio") => audio.push(audio_stream(stream)?),
            _ => {}
        }
    }
    Ok(Probe {
        container,
        video: video.ok_or_else(probe_invalid)?,
        audio,
    })
}

fn probe_invalid() -> Error {
    Error::YtDlp(PROBE_INVALID.into())
}

fn video_stream(stream: StreamDocument) -> Result<VideoStream> {
    let rotation = stream.display_rotation();
    let color_transfer = stream
        .color_transfer
        .filter(|transfer| !transfer.is_empty());
    Ok(VideoStream {
        index: stream_index(stream.index)?,
        codec: required(stream.codec_name)?,
        pix_fmt: required(stream.pix_fmt)?,
        width: positive(stream.width)?,
        height: positive(stream.height)?,
        rotation,
        color_transfer,
    })
}

fn audio_stream(stream: StreamDocument) -> Result<AudioStream> {
    Ok(AudioStream {
        index: stream_index(stream.index)?,
        codec: required(stream.codec_name)?,
        channels: positive(stream.channels)?,
    })
}

fn required(value: Option<String>) -> Result<String> {
    match value.filter(|value| !value.is_empty()) {
        Some(value) => Ok(value),
        None => Err(probe_invalid()),
    }
}

fn stream_index(index: Option<u64>) -> Result<u32> {
    index
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(probe_invalid)
}

fn positive(value: Option<u64>) -> Result<u32> {
    value
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(probe_invalid)
}

#[derive(Deserialize)]
struct ProbeDocument {
    format: Option<FormatDocument>,
    #[serde(default)]
    streams: Vec<StreamDocument>,
}

#[derive(Deserialize)]
struct FormatDocument {
    format_name: Option<String>,
    duration: Option<DurationValue>,
}

/// ffprobe reports most numeric fields as strings; both forms are accepted.
#[derive(Deserialize)]
#[serde(untagged)]
enum DurationValue {
    Number(f64),
    Text(String),
}

impl DurationValue {
    fn seconds(self) -> Option<f64> {
        match self {
            Self::Number(seconds) => Some(seconds),
            Self::Text(text) => text.parse::<f64>().ok(),
        }
    }
}

#[derive(Deserialize)]
struct StreamDocument {
    index: Option<u64>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    channels: Option<u64>,
    color_transfer: Option<String>,
    rotation: Option<i64>,
    disposition: Option<DispositionDocument>,
    #[serde(default)]
    side_data_list: Vec<SideDataDocument>,
}

impl StreamDocument {
    fn attached_pic(&self) -> bool {
        self.disposition
            .as_ref()
            .is_some_and(|disposition| disposition.attached_pic != 0)
    }

    /// A missing display matrix means no rotation, which is the normal case.
    fn display_rotation(&self) -> i64 {
        self.rotation
            .or_else(|| {
                self.side_data_list
                    .iter()
                    .find(|side| side.side_data_type.as_deref() == Some(DISPLAY_MATRIX))
                    .and_then(|side| side.rotation)
            })
            .unwrap_or(0)
    }
}

#[derive(Deserialize)]
struct DispositionDocument {
    #[serde(default)]
    attached_pic: u64,
}

#[derive(Deserialize)]
struct SideDataDocument {
    side_data_type: Option<String>,
    rotation: Option<i64>,
}

/// Remux or re-encode one source into the temporary MP4 under the disk guard. @req DL-4
async fn run_conversion(
    config: &Config,
    directory: &Path,
    input: &Path,
    output: &Path,
    probe: &Probe,
    deadline: Instant,
) -> Result<()> {
    let total_bytes = config
        .max_upload_bytes
        .checked_mul(3)
        .ok_or_else(|| Error::Process(DISK_INVALID.into()))?;
    let limit = process::DirectoryLimit {
        path: directory,
        file_bytes: config.max_upload_bytes,
        total_bytes,
    };
    let mut command = Command::new("ffmpeg");
    command.args(conversion_arguments(&Conversion {
        input,
        output,
        video: &probe.video,
        audio: &probe.audio,
    }));
    process::run_guarded(
        &mut command,
        child_deadline(deadline, None)?,
        MAX_CHILD_STDOUT_BYTES,
        limit,
        None,
    )
    .await?;
    Ok(())
}

/// Map the probed streams explicitly: no default selection, cover or subtitle. @req DL-4
fn conversion_arguments(conversion: &Conversion<'_>) -> Vec<OsString> {
    let mut arguments = ["-nostdin", "-n", "-v", "error", "-xerror", "-threads", THREADS, "-i"].map(OsString::from).to_vec();
    arguments.push(conversion.input.as_os_str().to_owned());
    map_stream(&mut arguments, conversion.video.index);
    for audio in conversion.audio {
        map_stream(&mut arguments, audio.index);
    }
    match copy_video(conversion.video) {
        true => arguments.extend(["-c:v", "copy"].map(OsString::from)),
        false => arguments.extend(
            [
                "-c:v",
                ENCODER,
                "-preset",
                PRESET,
                "-crf",
                CRF,
                "-pix_fmt",
                PIXEL_FORMAT,
            ]
            .map(OsString::from),
        ),
    }
    for (ordinal, audio) in conversion.audio.iter().enumerate() {
        arguments.push(format!("-c:a:{ordinal}").into());
        match audio.codec == AUDIO_CODEC {
            true => arguments.push("copy".into()),
            false => {
                arguments.push(AUDIO_CODEC.into());
                arguments.push(format!("-b:a:{ordinal}").into());
                arguments.push(AUDIO_BITRATE.into());
                arguments.push(format!("-ac:a:{ordinal}").into());
                arguments.push(audio.channels.to_string().into());
            }
        }
    }
    if conversion.audio.is_empty() {
        arguments.push("-an".into());
    }
    arguments.extend(
        [
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-threads",
            THREADS,
            "-movflags",
            "+faststart",
            "-f",
            MP4_CONTAINER,
        ]
        .map(OsString::from),
    );
    arguments.push(conversion.output.as_os_str().to_owned());
    arguments
}

fn map_stream(arguments: &mut Vec<OsString>, index: u32) {
    arguments.extend([OsString::from("-map"), format!("0:{index}").into()]);
}

fn copy_video(video: &VideoStream) -> bool {
    video.codec == VIDEO_CODEC && video.pix_fmt == PIXEL_FORMAT
}

/// Every child is bounded by the remaining shared media deadline. @req DL-5
fn child_deadline(deadline: Instant, cap: Option<Duration>) -> Result<Duration> {
    let remaining = match deadline.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => remaining,
        _ => return Err(Error::Timeout),
    };
    Ok(cap.map_or(remaining, |cap| remaining.min(cap)))
}

/// Run one filesystem step on the shared std blocking pool, off the executor. @req DL-4
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    compio::runtime::spawn_blocking(work)
        .await
        .map_err(|_| Error::YtDlp(BLOCKING_FAILED.into()))?
}

/// Bounded top-level ISO-BMFF scan: `moov` must precede `mdat`. @req DL-4
fn faststart(path: &Path) -> Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    top_level_order(&mut file, length)
}

fn top_level_order<R: Read + Seek>(reader: &mut R, length: u64) -> Result<bool> {
    let mut offset = 0_u64;
    let mut scanned = 0_usize;
    let mut moov = None;
    let mut mdat = None;
    while offset < length && (moov.is_none() || mdat.is_none()) {
        scanned += 1;
        if scanned > MAX_TOP_LEVEL_BOXES {
            return Err(Error::YtDlp(FASTSTART_INVALID.into()));
        }
        reader.seek(SeekFrom::Start(offset))?;
        let header = read_box_header(reader, length - offset)?;
        if header.kind == *b"moov" {
            moov.get_or_insert(offset);
        }
        if header.kind == *b"mdat" {
            mdat.get_or_insert(offset);
        }
        match header.size {
            0 => break,
            size => offset += size,
        }
    }
    Ok(match (moov, mdat) {
        (Some(moov), Some(mdat)) => moov < mdat,
        (Some(_), None) => true,
        (None, _) => false,
    })
}

fn read_box_header<R: Read>(reader: &mut R, remaining: u64) -> Result<BoxHeader> {
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let kind = [header[4], header[5], header[6], header[7]];
    let size = u64::from(u32::from_be_bytes([
        header[0], header[1], header[2], header[3],
    ]));
    match size {
        0 => Ok(BoxHeader { kind, size }),
        1 => {
            let mut extended = [0_u8; 8];
            reader.read_exact(&mut extended)?;
            let size = u64::from_be_bytes(extended);
            if !(16..=remaining).contains(&size) {
                return Err(Error::YtDlp(FASTSTART_INVALID.into()));
            }
            Ok(BoxHeader { kind, size })
        }
        size => {
            if !(8..=remaining).contains(&size) {
                return Err(Error::YtDlp(FASTSTART_INVALID.into()));
            }
            Ok(BoxHeader { kind, size })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP4_FAMILY: &str = "mov,mp4,m4a,3gp,3g2,mj2";
    const SOURCE: &str = r#"{
        "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "vp9", "pix_fmt": "yuv420p",
             "width": 540, "height": 960, "color_transfer": "bt709",
             "disposition": {"attached_pic": 0}},
            {"index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2,
             "disposition": {"attached_pic": 0}},
            {"index": 2, "codec_type": "video", "codec_name": "mjpeg",
             "pix_fmt": "yuvj444p", "width": 100, "height": 100,
             "disposition": {"attached_pic": 1}}
        ],
        "format": {"format_name": "matroska,webm", "duration": "35.918413"}
    }"#;

    const ROTATED: &str = r#"{
        "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p",
             "width": 1920, "height": 1080, "color_transfer": "bt709",
             "disposition": {"attached_pic": 0},
             "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}
        ],
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "4.731956"}
    }"#;

    fn video_stream(width: u32, height: u32) -> VideoStream {
        VideoStream {
            index: 0,
            codec: VIDEO_CODEC.into(),
            pix_fmt: PIXEL_FORMAT.into(),
            width,
            height,
            rotation: 0,
            color_transfer: Some("bt709".into()),
        }
    }

    fn audio_stream(index: u32, codec: &str, channels: u32) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            channels,
        }
    }

    fn probe(container: &str, video: VideoStream, audio: Vec<AudioStream>) -> Probe {
        Probe {
            container: container.into(),
            video,
            audio,
        }
    }

    fn arguments(video: &VideoStream, audio: &[AudioStream]) -> Vec<String> {
        let conversion = Conversion {
            input: Path::new("/tmp/job/media.mkv"),
            output: Path::new("/tmp/job/normalized.mp4"),
            video,
            audio,
        };
        conversion_arguments(&conversion)
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn rejected(result: Result<()>, expected: &str) {
        match result {
            Err(Error::YtDlp(message)) => assert_eq!(message, expected),
            other => panic!("expected rejection {expected:?}, got {other:?}"),
        }
    }

    fn elementary_box(kind: &[u8; 4], payload: usize) -> Vec<u8> {
        let size = payload + 8;
        let mut bytes = u32::try_from(size)
            .expect("box size fits")
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.resize(size, 0);
        bytes
    }

    fn extended_box(kind: &[u8; 4], payload: usize) -> Vec<u8> {
        let size = payload + 16;
        let mut bytes = 1_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&u64::try_from(size).expect("box size fits").to_be_bytes());
        bytes.resize(size, 0);
        bytes
    }

    fn order(bytes: &[u8]) -> Result<bool> {
        let length = u64::try_from(bytes.len()).expect("fixture length fits");
        top_level_order(&mut std::io::Cursor::new(bytes), length)
    }

    #[test]
    fn probe_selects_the_real_video_and_keeps_audio_identity() {
        let probe = parse_probe(SOURCE.as_bytes()).expect("a real source probe");
        assert_eq!(probe.container, "matroska,webm");
        assert_eq!(probe.video.index, 0);
        assert_eq!(probe.video.codec, "vp9");
        assert_eq!(probe.video.width, 540);
        assert_eq!(probe.video.height, 960);
        assert_eq!(
            probe.video.rotation, 0,
            "a missing display matrix is no rotation"
        );
        assert_eq!(probe.audio.len(), 1);
        assert_eq!(probe.audio[0].index, 1);
        assert_eq!(probe.audio[0].channels, 2);
    }

    #[test]
    fn probe_reads_the_display_matrix_rotation() {
        let probe = parse_probe(ROTATED.as_bytes()).expect("a rotated source probe");
        assert_eq!(probe.video.rotation, -90);
        assert_eq!(
            display_dimensions(probe.video.width, probe.video.height, probe.video.rotation)
                .expect("a quarter-turn matrix"),
            (1080, 1920)
        );
        rejected(validate_video(&probe.video, 1080), VIDEO_HEIGHT);
    }

    #[test]
    fn probe_accepts_a_numeric_duration() {
        let document = r#"{
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "pix_fmt": "yuv420p", "width": 540, "height": 960}
            ],
            "format": {"format_name": "matroska,webm", "duration": 12.5}
        }"#;
        let probe = parse_probe(document.as_bytes()).expect("numeric durations are accepted");
        assert_eq!(probe.audio.len(), 0, "a silent source stays silent");
    }

    #[test]
    fn probe_rejects_missing_required_data() {
        [
            r#"{}"#.to_owned(),
            r#"{"format": {"format_name": "mp4", "duration": "1.0"}, "streams": []}"#.to_owned(),
            r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p", "width": 540, "height": 960}], "format": {"duration": "1.0"}}"#.to_owned(),
            r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "width": 540, "height": 960}], "format": {"format_name": "mp4", "duration": "1.0"}}"#.to_owned(),
            r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p", "width": 540, "height": 960}, {"index": 1, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p", "width": 540, "height": 960}], "format": {"format_name": "mp4", "duration": "1.0"}}"#.to_owned(),
            r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p", "width": 540, "height": 960}, {"index": 1, "codec_type": "audio", "codec_name": "aac"}], "format": {"format_name": "mp4", "duration": "1.0"}}"#.to_owned(),
            r#"{"streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p", "width": 540, "height": 960}], "format": {"format_name": "mp4", "duration": "N/A"}}"#.to_owned(),
        ]
        .into_iter()
        .for_each(|document| {
            assert!(
                parse_probe(document.as_bytes()).is_err(),
                "accepted invalid probe {document}"
            )
        });
    }

    #[test]
    fn display_rotation_uses_the_visible_dimensions() {
        assert_eq!(
            display_dimensions(1920, 1080, 0).expect("unrotated"),
            (1920, 1080)
        );
        assert_eq!(
            display_dimensions(1920, 1080, -90).expect("quarter turn"),
            (1080, 1920)
        );
        assert_eq!(
            display_dimensions(540, 960, 270).expect("three quarter turns"),
            (960, 540)
        );
        assert_eq!(
            display_dimensions(640, 360, 180).expect("inverted"),
            (640, 360)
        );
        assert!(display_dimensions(640, 360, 45).is_err());
    }

    #[test]
    fn validation_rejects_sizes_heights_and_hdr_outside_the_contract() {
        validate_video(&video_stream(540, 960), 960).expect("a capped SDR video");
        rejected(
            validate_video(&video_stream(1080, 1920), 1920),
            VIDEO_HEIGHT,
        );
        rejected(validate_video(&video_stream(608, 1081), 1081), VIDEO_HEIGHT);
        rejected(validate_video(&video_stream(541, 961), 1080), VIDEO_SIZE);
        rejected(
            validate_video(
                &VideoStream {
                    color_transfer: Some("smpte2084".into()),
                    ..video_stream(540, 960)
                },
                960,
            ),
            HDR_UNSUPPORTED,
        );
        rejected(
            validate_video(
                &VideoStream {
                    color_transfer: Some("arib-std-b67".into()),
                    ..video_stream(540, 960)
                },
                960,
            ),
            HDR_UNSUPPORTED,
        );
    }

    #[test]
    fn output_validation_requires_the_deliverable_contract() {
        let source = probe(
            "matroska,webm",
            video_stream(540, 960),
            vec![audio_stream(1, AUDIO_CODEC, 2)],
        );
        let deliverable = || {
            probe(
                MP4_FAMILY,
                video_stream(540, 960),
                vec![audio_stream(1, AUDIO_CODEC, 2)],
            )
        };
        validate_output(&source, &deliverable(), 960).expect("a matching normalized file");
        rejected(
            validate_output(
                &source,
                &probe(
                    MP4_CONTAINER,
                    VideoStream {
                        codec: "vp9".into(),
                        ..video_stream(540, 960)
                    },
                    vec![audio_stream(1, AUDIO_CODEC, 2)],
                ),
                960,
            ),
            OUTPUT_INVALID,
        );
        rejected(
            validate_output(
                &source,
                &probe(
                    MP4_CONTAINER,
                    VideoStream {
                        pix_fmt: "yuv420p10le".into(),
                        ..video_stream(540, 960)
                    },
                    vec![audio_stream(1, AUDIO_CODEC, 2)],
                ),
                960,
            ),
            OUTPUT_INVALID,
        );
        rejected(
            validate_output(
                &source,
                &probe(MP4_CONTAINER, video_stream(480, 854), Vec::new()),
                960,
            ),
            OUTPUT_INVALID,
        );
        rejected(
            validate_output(
                &source,
                &probe(
                    MP4_CONTAINER,
                    video_stream(540, 960),
                    vec![audio_stream(1, "opus", 2)],
                ),
                960,
            ),
            OUTPUT_INVALID,
        );
        rejected(
            validate_output(
                &source,
                &probe(
                    MP4_CONTAINER,
                    video_stream(540, 960),
                    vec![audio_stream(1, AUDIO_CODEC, 1)],
                ),
                960,
            ),
            OUTPUT_INVALID,
        );
        rejected(
            validate_output(
                &probe("matroska,webm", video_stream(540, 960), Vec::new()),
                &deliverable(),
                960,
            ),
            OUTPUT_INVALID,
        );
    }

    #[test]
    fn normalization_copies_compatible_streams_and_encodes_the_rest() {
        let compatible = audio_stream(1, AUDIO_CODEC, 2);
        let arguments = arguments(
            &video_stream(540, 960),
            &[compatible, audio_stream(3, "opus", 6)],
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-nostdin", "-n"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-map", "0:0"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-map", "0:1"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-map", "0:3"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-c:a:0", "copy"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-c:a:1", AUDIO_CODEC])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-b:a:1", AUDIO_BITRATE])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-ac:a:1", "6"]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-map_metadata", "-1"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-movflags", "+faststart"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "mp4"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-threads", "1"]));
        assert!(arguments.iter().any(|argument| argument == "-sn"));
        assert!(arguments.iter().any(|argument| argument == "-dn"));
        assert!(!arguments.iter().any(|argument| argument == "libx264"));
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("/tmp/job/normalized.mp4")
        );
    }

    #[test]
    fn normalization_encodes_incompatible_video_and_keeps_silent_sources_silent() {
        let video = VideoStream {
            codec: "vp9".into(),
            pix_fmt: "yuv420p10le".into(),
            ..video_stream(540, 960)
        };
        let arguments = arguments(&video, &[audio_stream(2, "opus", 2)]);
        assert!(arguments.windows(2).any(|pair| pair == ["-c:v", ENCODER]));
        assert!(arguments.windows(2).any(|pair| pair == ["-preset", PRESET]));
        assert!(arguments.windows(2).any(|pair| pair == ["-crf", CRF]));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-pix_fmt", PIXEL_FORMAT])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-c:a:0", AUDIO_CODEC])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-ac:a:0", "2"]));

        let silent = self::arguments(&video_stream(540, 960), &[]);
        assert!(silent.iter().any(|argument| argument == "-an"));
        assert!(!silent.iter().any(|argument| argument.starts_with("-c:a:")));
        assert_eq!(
            silent
                .iter()
                .filter(|argument| argument.as_str() == "-map")
                .count(),
            1,
            "a silent source maps only its video stream"
        );
    }

    #[test]
    fn probe_requests_every_contract_field() {
        let arguments = probe_arguments(Path::new("/tmp/job/media.mkv"));
        let fields: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let entries = fields
            .iter()
            .find(|argument| argument.starts_with("stream="))
            .expect("stream entries");
        [
            "index",
            "codec_type",
            "codec_name",
            "pix_fmt",
            "width",
            "height",
            "channels",
            "color_transfer",
            "attached_pic",
            "stream_side_data=side_data_type,rotation",
            "format=format_name,duration",
        ]
        .into_iter()
        .for_each(|field| assert!(entries.contains(field), "missing probe field {field}"));
        assert_eq!(MAX_CHILD_STDOUT_BYTES, 64 * 1024);
        assert_eq!(
            fields.last().map(String::as_str),
            Some("/tmp/job/media.mkv")
        );
    }

    #[test]
    fn child_deadlines_only_receive_remaining_time() {
        let capped = child_deadline(
            Instant::now() + Duration::from_secs(30),
            Some(Duration::from_secs(5)),
        )
        .expect("a live deadline");
        assert!(capped <= Duration::from_secs(5) && capped >= Duration::from_secs(4));
        let remaining =
            child_deadline(Instant::now() + Duration::from_secs(2), None).expect("a live deadline");
        assert!(remaining <= Duration::from_secs(2) && remaining > Duration::ZERO);
        assert!(matches!(
            child_deadline(Instant::now(), None),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn faststart_boxes_require_moov_before_mdat() {
        let mut deliverable = elementary_box(b"ftyp", 8);
        deliverable.extend(elementary_box(b"moov", 32));
        deliverable.extend(elementary_box(b"mdat", 64));
        assert!(order(&deliverable).expect("a faststart layout"));

        let mut interleaved = elementary_box(b"ftyp", 8);
        interleaved.extend(elementary_box(b"mdat", 64));
        interleaved.extend(elementary_box(b"moov", 32));
        assert!(!order(&interleaved).expect("a trailing index"));
    }

    #[test]
    fn faststart_scan_handles_extended_sizes_and_missing_indexes() {
        let mut extended = elementary_box(b"ftyp", 8);
        extended.extend(elementary_box(b"moov", 16));
        extended.extend(extended_box(b"mdat", 1024));
        assert!(order(&extended).expect("an extended mdat"));

        assert!(!order(&elementary_box(b"ftyp", 8)).expect("a missing index"));
        let mut zero_size = elementary_box(b"ftyp", 4);
        zero_size.extend(0_u32.to_be_bytes());
        zero_size.extend_from_slice(b"mdat");
        assert!(!order(&zero_size).expect("a zero-size trailing box"));
    }

    #[test]
    fn faststart_scan_rejects_truncated_and_unbounded_layouts() {
        let mut truncated = 16_u32.to_be_bytes().to_vec();
        truncated.extend_from_slice(b"moov");
        truncated.extend_from_slice(b"short");
        assert!(order(&truncated).is_err());

        let mut unbounded = Vec::new();
        (0..=MAX_TOP_LEVEL_BOXES).for_each(|_| unbounded.extend(elementary_box(b"free", 0)));
        assert!(order(&unbounded).is_err());
    }
}
