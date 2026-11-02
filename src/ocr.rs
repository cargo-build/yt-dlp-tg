//! Private bounded image acquisition and configured recognition. @req DL-12 DL-13

use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    time::Duration,
};

use compio::process::Command;
use futures_util::StreamExt;
use serde::Deserialize;
use teloxide::{
    Bot,
    net::Download,
    payloads::EditMessageTextSetters,
    requests::Requester,
    types::{ChatId, FileMeta, InlineKeyboardMarkup, Message, MessageId},
};

use crate::{
    config::{Config, OcrConfig},
    error::{Error, Result},
    i18n::{Language, Text},
    process,
};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Image {
    file: FileMeta,
    pixels: Option<u64>,
}

pub struct Request {
    pub chat: ChatId,
    pub status: MessageId,
    pub language: Language,
    pub image: Image,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub plain_query: String,
    pub quoted_query: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recognition {
    #[serde(skip_deserializing)]
    Text {
        query: String,
    },
    Track(Track),
    Rejected {
        message: String,
    },
}

#[derive(Deserialize)]
struct LocalRecognition {
    query: String,
}

/// Select the largest photo or a supported image document. @req DL-12
pub fn image(message: &Message) -> Option<Image> {
    match message.photo() {
        Some(photos) => photos
            .iter()
            .max_by_key(|photo| u64::from(photo.width) * u64::from(photo.height))
            .map(|photo| Image {
                file: photo.file.clone(),
                pixels: Some(u64::from(photo.width) * u64::from(photo.height)),
            }),
        None => message
            .document()
            .filter(|document| {
                document.mime_type.as_ref().is_some_and(|mime| {
                    matches!(
                        mime.essence_str(),
                        "image/png" | "image/jpeg" | "image/webp"
                    )
                })
            })
            .map(|document| Image {
                file: document.file.clone(),
                pixels: None,
            }),
    }
}

/// Recognize one private image and remove its scratch on every result. @req DL-12
pub async fn recognize(bot: &Bot, config: &Config, request: Request) -> Result<Recognition> {
    let settings = config
        .ocr
        .as_ref()
        .ok_or_else(|| Error::Config("OCR is disabled".into()))?;
    check_size(request.image.file.size)?;
    match request.image.pixels {
        Some(0) => return Err(Error::Image),
        Some(pixels) if pixels > MAX_IMAGE_PIXELS => return Err(Error::ImageTooLarge),
        _ => {}
    }
    let root = config.download_dir.clone();
    let directory = compio::runtime::spawn_blocking(move || {
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let metadata = std::fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
            return Err(std::io::Error::other("image scratch root is not private"));
        }
        let directory = root.join(format!("image-{:016x}", rand::random::<u64>()));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(directory)
    })
    .await
    .map_err(|_| Error::Image)??;
    let result = async {
        status(bot, &request, Text::ImageDownloading).await?;
        let path = directory.join("input-image");
        compio::time::timeout(
            Duration::from_secs(60),
            download(bot, &request.image, &path),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        status(bot, &request, Text::Recognizing).await?;
        recognize_file(settings, &path, request.language).await
    }
    .await;
    match compio::runtime::spawn_blocking(move || std::fs::remove_dir_all(directory)).await {
        Ok(Ok(())) => {}
        _ => tracing::warn!("Cannot remove image job directory"),
    }
    result
}

fn check_size(size: u32) -> Result<()> {
    match size != u32::MAX && u64::from(size) > MAX_IMAGE_BYTES as u64 {
        true => Err(Error::ImageTooLarge),
        false => Ok(()),
    }
}

async fn download(bot: &Bot, image: &Image, path: &Path) -> Result<()> {
    if image.file.id.0.trim().is_empty() {
        return Err(Error::Image);
    }
    let file = bot
        .get_file(image.file.id.clone())
        .await
        .map_err(|_| Error::Image)?;
    check_size(file.size)?;
    if file.path.trim().is_empty() {
        return Err(Error::Image);
    }
    let stream = bot.download_file_stream(&file.path);
    let mut stream = std::pin::pin!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::Image)?;
        match chunk.len() > MAX_IMAGE_BYTES.saturating_sub(bytes.len()) {
            true => return Err(Error::ImageTooLarge),
            false => bytes.extend_from_slice(&chunk),
        }
    }
    if bytes.is_empty() {
        return Err(Error::Image);
    }
    let compio::BufResult(written, _) = compio::fs::write(path, bytes).await;
    written?;
    Ok(())
}

async fn recognize_file(
    settings: &OcrConfig,
    path: &Path,
    language: Language,
) -> Result<Recognition> {
    let mut command = helper_command(settings, language);
    command
        .arg("--image")
        .arg(path)
        .args([
            "--max-bytes",
            &MAX_IMAGE_BYTES.to_string(),
            "--max-pixels",
            &MAX_IMAGE_PIXELS.to_string(),
        ])
        .env("OMP_NUM_THREADS", "1")
        .env("OPENBLAS_NUM_THREADS", "1");
    helper_output(settings, &mut command).await
}

/// Render corrected fields with the configured templates, without inference. @req DL-13
pub async fn render(
    settings: &OcrConfig,
    language: Language,
    artist: &str,
    title: &str,
) -> Result<Recognition> {
    if settings.profile.is_none() || !valid_fields(artist, title) {
        return Err(Error::Image);
    }
    let mut command = helper_command(settings, language);
    command
        .arg("--render")
        .arg(format!("--artist={artist}"))
        .arg(format!("--title={title}"));
    helper_output(settings, &mut command).await
}

fn helper_command(settings: &OcrConfig, language: Language) -> Command {
    let mut command = Command::new(&settings.python);
    command.arg("-I").arg(&settings.script);
    if let Some(profile) = &settings.profile {
        command
            .arg("--config")
            .arg(profile)
            .arg("--language")
            .arg(language.code());
        if let Some(path) = &settings.secrets_file {
            command.arg("--secrets-file").arg(path);
        }
    }
    command
}

async fn helper_output(settings: &OcrConfig, command: &mut Command) -> Result<Recognition> {
    let output = match process::run(
        command,
        Duration::from_secs(settings.timeout_secs),
        MAX_OUTPUT_BYTES,
    )
    .await
    {
        Ok(output) => output,
        Err(Error::ChildExit(Some(2))) => return Err(Error::ImageEmpty),
        Err(Error::ChildExit(Some(4))) => return Err(Error::ImageTooLarge),
        Err(Error::Timeout) => return Err(Error::Timeout),
        Err(_) => return Err(Error::Image),
    };
    parse(&output, settings.profile.is_some())
}

/// Validate editable fields independently of the recognition backend. @req DL-13
pub fn valid_fields(artist: &str, title: &str) -> bool {
    valid_text(artist, 240)
        && valid_text(title, 240)
        && artist.chars().count() + title.chars().count() < 240
}

fn valid_text(text: &str, limit: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= limit && !text.chars().any(char::is_control)
}

fn parse(output: &[u8], configured: bool) -> Result<Recognition> {
    let recognition = match configured {
        true => serde_json::from_slice::<Recognition>(output).map_err(|_| Error::Image)?,
        false => {
            let local: LocalRecognition =
                serde_json::from_slice(output).map_err(|_| Error::Image)?;
            Recognition::Text {
                query: local.query.trim().to_owned(),
            }
        }
    };
    let valid = match &recognition {
        Recognition::Track(track) => {
            valid_fields(&track.artist, &track.title)
                && valid_text(&track.plain_query, 1024)
                && valid_text(&track.quoted_query, 1024)
        }
        Recognition::Rejected { message } => valid_text(message, 512),
        Recognition::Text { query } => !configured && valid_text(query, 240),
    };
    match valid {
        true => Ok(recognition),
        false => Err(Error::Image),
    }
}

async fn status(bot: &Bot, request: &Request, text: Text) -> Result<()> {
    bot.edit_message_text(request.chat, request.status, request.language.text(text))
        .reply_markup(InlineKeyboardMarkup::default())
        .await
        .map_err(|_| Error::Image)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_IMAGE_BYTES, Recognition, check_size, parse, valid_fields};

    #[test]
    fn validates_required_query_and_size_without_inventing_text() {
        assert!(
            matches!(parse(br#"{"query":"  FORHILL & JNFOUND ATLAS  "}"#, false).expect("valid local query"), Recognition::Text { query } if query == "FORHILL & JNFOUND ATLAS")
        );
        assert!(parse(br#"{"query":" "}"#, false).is_err());
        assert!(parse(br#"{}"#, false).is_err());
        assert!(parse(br#"{"query":"one\ntwo"}"#, false).is_err());
        assert!(
            parse(
                format!("{{\"query\":\"{}\"}}", "x".repeat(241)).as_bytes(),
                false
            )
            .is_err()
        );
        assert!(
            check_size(u32::MAX).is_ok(),
            "unknown metadata size still uses streamed cap"
        );
        assert!(check_size((MAX_IMAGE_BYTES + 1) as u32).is_err());
    }

    #[test]
    fn configured_mode_requires_structured_validated_results() {
        assert!(parse(br#"{"query":"bypass"}"#, true).is_err());
        assert!(parse(br#"{"kind":"track","artist":"A","title":"T","plain_query":"A T","quoted_query":"\"A\" \"T\""}"#, true).is_ok());
        assert!(
            parse(
                br#"{"kind":"rejected","message":"No track identified"}"#,
                true
            )
            .is_ok()
        );
        assert!(
            parse(
                br#"{"kind":"rejected","message":"","query":"bypass"}"#,
                true
            )
            .is_err()
        );
        assert!(!valid_fields("A\nB", "T"));
        assert!(!valid_fields(&"a".repeat(240), "T"));
        assert!(valid_fields(&"a".repeat(238), "T"));
        assert!(!valid_fields(&"a".repeat(239), "T"));
    }
}
