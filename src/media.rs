//! Bounded yt-dlp processes and validated media metadata. @req DL-2 DL-4 DL-5 DL-8

mod size;

use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use compio::process::Command;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    config::Config,
    error::{Error, Result},
    process, progress, video,
};

/// Search results per menu page. @req DL-3
pub const PAGE_SIZE: usize = 5;
const MAX_PAGES: usize = 10;
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;
const AUDIO_SELECTOR: &str = "bestaudio[acodec=opus]/best[acodec=opus]/bestaudio/best";
const MUSIC_SELECTOR: &str =
    r"bestaudio[acodec~='^(aac|mp4a\.40\..+)$']/best[acodec~='^(aac|mp4a\.40\..+)$']";

/// An installed extractor with an explicit text-search prefix. @req DL-2
#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub key: String,
}

/// A validated source title and HTTP(S) webpage. @req DL-3
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Media {
    pub title: String,
    pub url: String,
}

/// The requested output workflow. @req DL-4 DL-5
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum Format {
    Opus,
    M4a,
    Video {
        height: u32,
        selection: video::Selection,
    },
}

#[derive(Debug, Deserialize)]
struct Extraction {
    #[serde(rename = "_type")]
    kind: Option<String>,
    title: Option<String>,
    webpage_url: Option<String>,
    original_url: Option<String>,
    url: Option<String>,
    id: Option<String>,
    ie_key: Option<String>,
    extractor_key: Option<String>,
    is_live: Option<bool>,
    live_status: Option<String>,
    entries: Option<Vec<Option<Self>>>,
}

/// Discover installed search extractors without maintaining a provider list. @req DL-2
pub async fn providers(config: &Config) -> Result<Vec<Provider>> {
    let mut command = base_command(config);
    command.arg("--extractor-descriptions");
    let output = process::run(
        &mut command,
        Duration::from_secs(config.process_timeout_secs),
        MAX_STDOUT_BYTES,
    )
    .await?;
    let descriptions = std::str::from_utf8(&output)
        .map_err(|_| Error::YtDlp("invalid extractor descriptions".into()))?;
    let providers = parse_providers(descriptions);
    match providers.is_empty() {
        true => Err(Error::YtDlp("no installed text-search providers".into())),
        false => Ok(providers),
    }
}

fn parse_providers(descriptions: &str) -> Vec<Provider> {
    let mut keys = HashSet::new();
    let mut providers: Vec<_> = descriptions
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once(": ")?;
            match name.to_ascii_lowercase().contains("search") {
                false => None,
                true => {
                    let key = description
                        .split('"')
                        .skip(1)
                        .step_by(2)
                        .find_map(|quoted| {
                            let prefix = quoted.strip_suffix(':')?;
                            valid_prefix(prefix).then_some(prefix)
                        })?;
                    keys.insert(key.to_owned()).then(|| Provider {
                        name: name.to_owned(),
                        key: key.to_owned(),
                    })
                }
            }
        })
        .collect();
    providers.sort_by(|left, right| {
        (left.key != "ytsearch", left.name.to_ascii_lowercase())
            .cmp(&(right.key != "ytsearch", right.name.to_ascii_lowercase()))
    });
    providers
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Normalize a single bare HTTP(S) URL, including a leading `www.`. @req DL-2
pub fn http_url(input: &str) -> Option<String> {
    let input = input.trim();
    match input.is_empty() || input.chars().any(char::is_whitespace) {
        true => None,
        false => {
            let normalized = match input.starts_with("www.") {
                true => format!("https://{input}"),
                false => input.to_owned(),
            };
            let url = Url::parse(&normalized).ok()?;
            match matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
            {
                true => Some(url.into()),
                false => None,
            }
        }
    }
}

/// Extract one source; playlist and active-stream responses are rejected. @req DL-2 DL-8
pub async fn probe(config: &Config, url: &str) -> Result<Media> {
    parse_probe(&metadata(config, url).await?)
}

/// Inspect resolution/size choices without downloading media. @req DL-5
pub async fn video_choices(config: &Config, media: &Media) -> Result<Vec<video::Choice>> {
    let output = metadata(config, &media.url).await?;
    parse_probe(&output)?;
    let mut info = serde_json::from_slice(&output)?;
    size::supplement(config, &mut info).await?;
    video::choices(&info, config.max_upload_bytes)
}

async fn metadata(config: &Config, url: &str) -> Result<Vec<u8>> {
    let url = http_url(url).ok_or_else(|| Error::YtDlp("invalid media URL".into()))?;
    let mut command = base_command(config);
    command.args(["--dump-single-json", "--skip-download", "--", &url]);
    process::run(
        &mut command,
        Duration::from_secs(config.process_timeout_secs),
        MAX_STDOUT_BYTES,
    )
    .await
}

fn parse_probe(output: &[u8]) -> Result<Media> {
    let extraction: Extraction = serde_json::from_slice(output)?;
    validate_single(&extraction)?;
    extraction_media(extraction)
}

#[expect(
    clippy::single_match,
    reason = "Explicit match arms follow the project branch style."
)]
fn validate_single(extraction: &Extraction) -> Result<()> {
    match extraction.kind.as_deref() {
        Some("playlist" | "multi_video") => {
            return Err(Error::YtDlp("a single media item is required".into()));
        }
        _ => {}
    }
    match extraction.entries.is_some() {
        true => return Err(Error::YtDlp("a single media item is required".into())),
        false => {}
    }
    match active_stream(extraction) {
        true => Err(Error::YtDlp(
            "active or upcoming streams are unsupported".into(),
        )),
        false => Ok(()),
    }
}

fn active_stream(extraction: &Extraction) -> bool {
    extraction.is_live == Some(true)
        || matches!(
            extraction.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        )
}

fn extraction_media(extraction: Extraction) -> Result<Media> {
    let url = youtube_url(&extraction)?
        .or_else(|| match extraction.webpage_url.as_deref() {
            Some(url) => http_url(url),
            None => extraction
                .url
                .as_deref()
                .and_then(http_url)
                .or_else(|| extraction.original_url.as_deref().and_then(http_url)),
        })
        .ok_or_else(|| Error::YtDlp("media webpage URL is missing or invalid".into()))?;
    let title = extraction
        .title
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| Error::YtDlp("media title is missing".into()))?;
    Ok(Media { title, url })
}

/// Keep one cache source for every URL form of an extracted YouTube video. @req DL-9
fn youtube_url(extraction: &Extraction) -> Result<Option<String>> {
    let youtube = extraction
        .ie_key
        .as_deref()
        .or(extraction.extractor_key.as_deref())
        .is_some_and(|key| key.eq_ignore_ascii_case("youtube"));
    match youtube {
        false => Ok(None),
        true => {
            let id = extraction
                .id
                .as_deref()
                .or(extraction.url.as_deref())
                .filter(|id| {
                    id.len() == 11
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .ok_or_else(|| Error::YtDlp("YouTube video ID is missing or invalid".into()))?;
            Ok(Some(format!("https://www.youtube.com/watch?v={id}")))
        }
    }
}

/// Search a bounded page using the selected extractor's pseudo-URL. @req DL-2 DL-3
pub async fn search(
    config: &Config,
    provider: &Provider,
    query: &str,
    page: usize,
) -> Result<Vec<Media>> {
    let (start, end) = page_range(page)?;
    match valid_prefix(&provider.key) && !query.trim().is_empty() {
        true => {}
        false => return Err(Error::YtDlp("invalid search request".into())),
    }
    let operand = format!("{}{end}:{}", provider.key, query.trim());
    let mut command = base_command(config);
    command.args([
        "--dump-single-json",
        "--flat-playlist",
        "--skip-download",
        "--playlist-items",
        &format!("{start}:{end}"),
        "--",
        &operand,
    ]);
    let output = process::run(
        &mut command,
        Duration::from_secs(config.process_timeout_secs),
        MAX_STDOUT_BYTES,
    )
    .await?;
    parse_search(&output)
}

fn page_range(page: usize) -> Result<(usize, usize)> {
    match page < MAX_PAGES {
        true => Ok((page * PAGE_SIZE + 1, (page + 1) * PAGE_SIZE)),
        false => Err(Error::YtDlp("search page is out of range".into())),
    }
}

/// Preserve YouTube order while requiring both recognized names in the title. @req DL-13
pub fn matching_track<'a>(results: &'a [Media], artist: &str, title: &str) -> Option<&'a Media> {
    let artist = words(artist);
    let title = words(title);
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    results.iter().find(|media| {
        let candidate = words(&media.title);
        artist.iter().all(|word| {
            candidate.iter().filter(|value| *value == word).count()
                >= artist.iter().filter(|value| *value == word).count()
        }) && candidate.windows(title.len()).any(|phrase| phrase == title)
    })
}

fn words(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn parse_search(output: &[u8]) -> Result<Vec<Media>> {
    let extraction: Extraction = serde_json::from_slice(output)?;
    extraction
        .entries
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !active_stream(entry))
        .take(PAGE_SIZE)
        .map(|entry| {
            validate_single(&entry)?;
            extraction_media(entry)
        })
        .collect()
}

/// Download into an empty caller-owned job directory and validate the final file. @req DL-4 DL-5 DL-8
pub async fn download(
    config: &Config,
    media: &Media,
    format: &Format,
    directory: &Path,
    observer: &dyn Fn(&[u8]),
) -> Result<PathBuf> {
    if let Format::Video { height, selection } = format {
        if !(1..=1080).contains(height) {
            return Err(Error::YtDlp("invalid video height".into()));
        }
        if selection.size.bytes() > config.max_upload_bytes {
            return Err(Error::TooLarge);
        }
    }
    let root = config.download_dir.clone();
    let requested_directory = directory.to_owned();
    let directory =
        compio::runtime::spawn_blocking(move || validate_directory(&root, &requested_directory))
            .await
            .map_err(|_| Error::YtDlp("download directory check failed".into()))??;

    let metadata = metadata(config, &media.url).await?;
    let source = parse_probe(&metadata)?;
    if matches!(format, Format::M4a) && !native_aac(&serde_json::from_slice(&metadata)?) {
        return Err(Error::AacUnavailable);
    }
    let mut command = base_command(config);
    download_arguments(&mut command, config, format, &directory, &source.url);
    let total_bytes = config
        .max_upload_bytes
        .checked_mul(3)
        .ok_or_else(|| Error::Process("invalid download disk limit".into()))?;
    let file_bytes = match format {
        Format::Opus | Format::M4a => total_bytes,
        Format::Video { .. } => config.max_upload_bytes,
    };
    let output = process::run_guarded(
        &mut command,
        Duration::from_secs(config.download_timeout_secs),
        MAX_STDOUT_BYTES,
        process::DirectoryLimit {
            path: &directory,
            file_bytes,
            total_bytes,
        },
        Some(observer),
    )
    .await?;
    let final_path = final_path(&output)?;
    let limit = config.max_upload_bytes;
    let extensions: &'static [&'static str] = match format {
        Format::Opus => &["opus"],
        Format::M4a => &["m4a"],
        Format::Video { .. } => &["mp4", "mkv"],
    };
    let path = compio::runtime::spawn_blocking(move || {
        validate_download(&directory, Path::new(&final_path), extensions, limit)
    })
    .await
    .map_err(|_| Error::YtDlp("download file check failed".into()))??;
    if matches!(format, Format::M4a) {
        verify_aac(config, &path).await?;
    }
    Ok(path)
}

fn native_aac(metadata: &serde_json::Value) -> bool {
    let audio = |format: &serde_json::Value| {
        format
            .get("acodec")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|codec| codec == "aac" || codec.starts_with("mp4a.40."))
    };
    match metadata
        .get("formats")
        .and_then(serde_json::Value::as_array)
    {
        Some(formats) if !formats.is_empty() => formats.iter().any(audio),
        _ => audio(metadata),
    }
}

async fn verify_aac(config: &Config, path: &Path) -> Result<()> {
    let mut command = Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type:stream_disposition=attached_pic",
            "-of",
            "json",
        ])
        .arg(path);
    let output = process::run(
        &mut command,
        Duration::from_secs(config.process_timeout_secs),
        64 * 1024,
    )
    .await?;
    let metadata: serde_json::Value = serde_json::from_slice(&output)?;
    match metadata
        .get("streams")
        .and_then(serde_json::Value::as_array)
    {
        Some(streams)
            if streams.iter().any(|stream| {
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("audio")
            }) && streams.iter().all(|stream| {
                match stream.get("codec_type").and_then(serde_json::Value::as_str) {
                    Some("audio") => {
                        stream.get("codec_name").and_then(serde_json::Value::as_str) == Some("aac")
                    }
                    Some("video") => {
                        stream
                            .pointer("/disposition/attached_pic")
                            .and_then(serde_json::Value::as_u64)
                            == Some(1)
                    }
                    _ => false,
                }
            }) =>
        {
            Ok(())
        }
        _ => Err(Error::AacUnavailable),
    }
}

fn final_path(output: &[u8]) -> Result<String> {
    let mut paths = output.split(|byte| *byte == b'\n').filter_map(|line| {
        std::str::from_utf8(line)
            .ok()
            .and_then(|line| serde_json::from_str::<String>(line).ok())
    });
    match (paths.next(), paths.next()) {
        (Some(path), None) if Path::new(&path).is_absolute() => Ok(path),
        _ => Err(Error::YtDlp(
            "download must return one absolute output path".into(),
        )),
    }
}

fn download_arguments(
    command: &mut Command,
    config: &Config,
    format: &Format,
    directory: &Path,
    url: &str,
) {
    command
        .args([
            "--progress",
            "--newline",
            "--progress-delta",
            "1",
            "--progress-template",
            progress::DOWNLOAD_TEMPLATE,
            "--no-simulate",
            "--no-overwrites",
            "--match-filters",
            "!is_live",
            "--abort-on-unavailable-fragments",
            "--max-filesize",
            &config.max_upload_bytes.to_string(),
            "--print",
            "after_move:%(filepath)j",
            "--paths",
        ])
        .arg(directory)
        .args(["--output", "media.%(ext)s"]);
    match format {
        Format::Opus => {
            command.args([
                "--format",
                AUDIO_SELECTOR,
                "--extract-audio",
                "--audio-format",
                "opus",
                "--audio-quality",
                "0",
                "--embed-thumbnail",
                "--convert-thumbnails",
                "jpg",
                "--embed-metadata",
            ]);
        }
        Format::M4a => {
            command.args([
                "--format",
                MUSIC_SELECTOR,
                "--extract-audio",
                "--audio-format",
                "best",
                "--remux-video",
                "m4a",
            ]);
        }
        Format::Video { selection, .. } => {
            command.args([
                "--format",
                &selection.selector,
                "--merge-output-format",
                "mp4/mkv",
                "--remux-video",
                "mp4>mp4/mov>mp4/mkv",
            ]);
        }
    }
    command.args(["--", url]);
}

#[expect(
    clippy::single_match,
    reason = "Explicit match arms follow the project branch style."
)]
fn validate_directory(root: &Path, directory: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let directory = directory.canonicalize()?;
    match directory != root && directory.starts_with(&root) && directory.is_dir() {
        false => return Err(Error::YtDlp("invalid download directory".into())),
        true => {}
    }
    match std::fs::read_dir(&directory)?.next().transpose()? {
        Some(_) => Err(Error::YtDlp("download directory must be empty".into())),
        None => Ok(directory),
    }
}

#[expect(
    clippy::single_match,
    reason = "Explicit match arms follow the project branch style."
)]
fn validate_download(
    directory: &Path,
    path: &Path,
    extensions: &[&str],
    limit: u64,
) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(path)?;
    match metadata.len() > limit {
        true => return Err(Error::TooLarge),
        false => {}
    }
    let canonical = path.canonicalize()?;
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str());
    let valid_extension = extension.is_some_and(|extension| extensions.contains(&extension));
    match path.is_absolute()
        && canonical.parent() == Some(directory)
        && canonical.file_stem().is_some_and(|stem| stem == "media")
        && metadata.is_file()
        && valid_extension
        && metadata.len() > 0
    {
        false => return Err(Error::YtDlp("downloaded file failed validation".into())),
        true => {}
    }
    let entries = std::fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    match entries.len() == 1 && entries[0].path() == canonical {
        true => Ok(canonical),
        false => Err(Error::YtDlp(
            "download must produce exactly one file".into(),
        )),
    }
}

fn base_command(config: &Config) -> Command {
    let mut command = Command::new(&config.ytdlp_path);
    common_arguments(&mut command, config);
    command
}

#[expect(
    clippy::single_match,
    reason = "Explicit match arms follow the project branch style."
)]
fn common_arguments(command: &mut Command, config: &Config) {
    command.args([
        "--ignore-config",
        "--no-playlist",
        "--no-warnings",
        "--no-progress",
        "--no-plugin-dirs",
        "--no-cache-dir",
        "--socket-timeout",
        "20",
        "--retries",
        "2",
        "--fragment-retries",
        "2",
        "--extractor-retries",
        "2",
        "--file-access-retries",
        "1",
    ]);
    match &config.ytdlp_cookies {
        Some(path) => {
            command.args(["--cookies", path]);
        }
        None => {}
    }
    match &config.ytdlp_proxy {
        Some(proxy) => {
            command.args(["--proxy", proxy]);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_aliases_share_the_extracted_video_identity() {
        let canonical = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        [
            "https://youtu.be/dQw4w9WgXcQ?si=mobile&t=60",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=playlist&index=2",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ#start",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
        ]
        .into_iter()
        .for_each(|webpage_url| {
            let metadata = serde_json::json!({
                "extractor_key": "Youtube", "id": "dQw4w9WgXcQ",
                "title": "Source title", "webpage_url": webpage_url,
            });
            let direct =
                parse_probe(metadata.to_string().as_bytes()).expect("valid YouTube metadata");
            let search = parse_search(
                serde_json::json!({"entries": [metadata]})
                    .to_string()
                    .as_bytes(),
            )
            .expect("valid search metadata");
            assert_eq!(direct.url, canonical, "alias {webpage_url}");
            assert_eq!(search[0].url, canonical, "search alias {webpage_url}");
        });
    }

    #[test]
    fn youtube_identity_is_validated_and_scoped_to_its_extractor() {
        let canonical = parse_probe(br#"{"extractor_key":"Youtube","id":"VP4sb57YFwQ","title":"Other video","webpage_url":"https://youtu.be/VP4sb57YFwQ"}"#)
            .expect("a distinct valid video");
        assert_eq!(canonical.url, "https://www.youtube.com/watch?v=VP4sb57YFwQ");
        [None, Some(""), Some("bad"), Some("abcdefghij!")]
            .into_iter()
            .for_each(|id| {
                let metadata = serde_json::json!({"extractor_key":"Youtube", "id":id,
                    "title":"Invalid identity", "webpage_url":"https://youtu.be/dQw4w9WgXcQ"});
                assert!(
                    parse_probe(metadata.to_string().as_bytes()).is_err(),
                    "invalid ID {id:?}"
                );
            });
        let other = parse_probe(br#"{"extractor_key":"Vimeo","id":"dQw4w9WgXcQ","title":"Other provider","webpage_url":"https://vimeo.com/123?source=share"}"#)
            .expect("other providers keep their webpage identity");
        assert_eq!(other.url, "https://vimeo.com/123?source=share");
    }

    #[test]
    fn video_download_pins_the_planned_height_without_a_broader_fallback() {
        let config = config();
        let mut command = Command::new("yt-dlp");
        let video = Format::Video {
            height: 720,
            selection: video::Selection {
                selector: "all[format_id=v720][height=720]+all[format_id=a]".into(),
                size: video::Size::Exact(1024),
            },
        };
        download_arguments(
            &mut command,
            &config,
            &video,
            Path::new("/tmp/job"),
            "https://example.com/video",
        );
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(arguments.windows(2).any(|pair| pair
            == [
                "--format",
                "all[format_id=v720][height=720]+all[format_id=a]"
            ]));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.contains("bestvideo"))
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--recode-video")
        );
    }

    #[test]
    fn matching_preserves_rank_but_skips_unrelated_titles() {
        let results = vec![
            Media {
                title: "A different Atlas".into(),
                url: "https://example.com/wrong".into(),
            },
            Media {
                title: "Unfound & FORHILL — Atlas (Official Audio)".into(),
                url: "https://example.com/match".into(),
            },
            Media {
                title: "Forhill & Unfound - Atlas".into(),
                url: "https://example.com/later".into(),
            },
        ];
        assert_eq!(
            matching_track(&results, "Forhill & Unfound", "Atlas")
                .expect("matching names")
                .url,
            "https://example.com/match"
        );
        assert!(matching_track(&results, "Forhill & Unfound", "Resonance").is_none());
        assert!(matching_track(&results, "ForhillRemix & Unfound", "Atlas").is_none());
        assert!(matching_track(&results, "&", "Atlas").is_none());
        assert!(matching_track(&results, "Forhill", "").is_none());
    }

    #[test]
    fn matching_requires_the_complete_title_phrase() {
        let results = vec![
            Media {
                title: "SKYKOT - White Noise and Shades".into(),
                url: "https://example.com/wrong".into(),
            },
            Media {
                title: "SKYKOT - White Shades".into(),
                url: "https://example.com/match".into(),
            },
        ];
        assert_eq!(
            matching_track(&results, "skykot", "white shades")
                .expect("complete title")
                .url,
            "https://example.com/match"
        );
        assert!(matching_track(&[], "skykot", "white shades").is_none());
    }

    #[test]
    fn matching_preserves_repeated_artist_words() {
        let results = vec![
            Media {
                title: "Duran - Ordinary World".into(),
                url: "https://example.com/wrong".into(),
            },
            Media {
                title: "Duran Duran - Ordinary World".into(),
                url: "https://example.com/match".into(),
            },
        ];
        assert_eq!(
            matching_track(&results, "Duran Duran", "Ordinary World")
                .expect("complete artist name")
                .url,
            "https://example.com/match"
        );
    }

    fn config() -> Config {
        Config::from_map(|name| match name {
            "BOT_TOKEN" => Some("test-token".into()),
            "ALLOWED_USER_IDS" => Some("42".into()),
            "HOME" => Some("/tmp/yt-dlp-tg-tests".into()),
            _ => None,
        })
        .expect("minimal test configuration is valid")
    }

    #[test]
    fn native_aac_requires_source_codec_and_never_requests_aac_encoding() {
        ["mp4a.40.2", "mp4a.40.5", "aac"]
            .into_iter()
            .for_each(|codec| {
                assert!(native_aac(
                    &serde_json::json!({"formats":[{"acodec":codec}]})
                ));
            });
        ["opus", "mp3", "mp4a.69", "unknown"]
            .into_iter()
            .for_each(|codec| {
                assert!(!native_aac(
                    &serde_json::json!({"formats":[{"acodec":codec}]})
                ));
            });
        let config = config();
        let mut command = Command::new("yt-dlp");
        download_arguments(
            &mut command,
            &config,
            &Format::M4a,
            Path::new("/tmp/job"),
            "https://media.invalid",
        );
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--audio-format", "best"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--remux-video", "m4a"])
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--audio-quality")
        );
        assert!(!MUSIC_SELECTOR.ends_with("/bestaudio/best"));
    }

    #[test]
    fn final_path_is_unique_absolute_and_separate_from_optional_frames() {
        assert_eq!(
            final_path(b"yt-dlp-tg-progress1|a|finished|NA|100|NA\n\"/tmp/media.m4a\"\n")
                .expect("progress and final path"),
            "/tmp/media.m4a"
        );
        assert!(final_path(b"yt-dlp-tg-progress1|bad\n").is_err());
        assert!(final_path(b"\"relative.m4a\"\n").is_err());
        assert!(final_path(b"\"/tmp/one\"\n\"/tmp/two\"\n").is_err());
        assert_eq!(
            final_path(b"\xff\n\"/tmp/media.opus\"\n").expect("bad optional output"),
            "/tmp/media.opus"
        );
    }

    #[test]
    fn discovers_only_explicit_safe_search_prefixes() {
        let providers = parse_providers(
            "BiliBiliSearch: Bilibili search; \"bilisearch:\" prefix\n\
             youtube:search: [youtube] YouTube search; \"ytsearch:\" prefix\n\
             youtube:search_url: YouTube search URL\n\
             prxseries:search: PRX Series Search; \"prxseries:\" prefix\n\
             evil:search: \"bad-prefix:\" prefix\n\
             random: \"random:\" prefix\n\
             duplicate:search: \"bilisearch:\" prefix\n",
        );
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.key.as_str())
                .collect::<Vec<_>>(),
            ["ytsearch", "bilisearch", "prxseries"],
            "URL-only, malformed, unrelated and duplicate providers must be excluded"
        );
    }

    #[test]
    fn accepts_only_one_bare_http_url() {
        assert_eq!(
            http_url(" www.example.com/watch?v=1 "),
            Some("https://www.example.com/watch?v=1".into())
        );
        [
            "look https://example.com",
            "https://example.com two",
            "file:///tmp/video",
            "https://user:secret@example.com",
            "https://",
            "ytsearch:cats",
        ]
        .into_iter()
        .for_each(|input| assert!(http_url(input).is_none(), "rejected input {input:?}"));
    }

    #[test]
    fn rejects_playlist_live_and_missing_required_metadata() {
        [
            br#"{"_type":"playlist","entries":[]}"#.as_slice(),
            br#"{"title":"live","webpage_url":"https://example.com/","is_live":true}"#,
            br#"{"title":"soon","webpage_url":"https://example.com/","live_status":"is_upcoming"}"#,
            br#"{"title":" ","webpage_url":"https://example.com/"}"#,
            br#"{"title":"title","webpage_url":"file:///tmp/video"}"#,
        ]
        .into_iter()
        .for_each(|output| assert!(parse_probe(output).is_err(), "rejected output {output:?}"));
        let media = parse_probe(br#"{"title":"recording","webpage_url":"https://example.com/video","live_status":"was_live"}"#)
            .expect("finished recordings are finite media");
        assert_eq!(media.title, "recording");
    }

    #[test]
    fn maps_flat_youtube_entries_without_inventing_missing_titles() {
        let media = parse_search(br#"{"_type":"playlist","entries":[null,{"ie_key":"Youtube","id":"dQw4w9WgXcQ","url":"dQw4w9WgXcQ","title":"Source title"}]}"#)
            .expect("canonical YouTube IDs map to watch URLs");
        assert_eq!(media[0].url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert!(parse_search(br#"{"entries":[{"url":"https://example.com"}]}"#).is_err());
        assert!(
            parse_search(br#"{"entries":[]}"#)
                .expect("empty results are valid")
                .is_empty()
        );
    }

    #[test]
    fn pagination_is_bounded_and_contiguous() {
        assert_eq!(page_range(0).expect("first page exists"), (1, 5));
        assert_eq!(page_range(1).expect("second page exists"), (6, 10));
        assert_eq!(page_range(9).expect("last page exists"), (46, 50));
        assert!(page_range(10).is_err());
        assert!(page_range(usize::MAX).is_err());
    }

    #[test]
    fn live_search_hits_do_not_hide_finite_results() {
        let items = parse_search(br#"{"entries":[{"title":"Live","url":"https://example.com/live","is_live":true},{"title":"Recording","url":"https://example.com/recording"}]}"#)
            .expect("a live hit does not invalidate the whole page");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Recording");
    }

    #[test]
    fn formats_keep_video_capped_and_user_operand_separate() {
        let config = config();
        let mut command = Command::new("yt-dlp");
        common_arguments(&mut command, &config);
        download_arguments(
            &mut command,
            &config,
            &Format::Opus,
            Path::new("/tmp/job"),
            "https://example.com/?a=1&b=2",
        );
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--no-plugin-dirs")
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--format", AUDIO_SELECTOR])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--audio-quality", "0"])
        );
        assert_eq!(
            &arguments[arguments.len() - 2..],
            ["--", "https://example.com/?a=1&b=2"]
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--recode-video")
        );
    }
}
