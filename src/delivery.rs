//! Typed Telegram delivery and durable reuse of successful uploads. @req DL-4 DL-9

use std::{
    future::Future,
    path::Path,
    time::{Duration, Instant},
};

use futures_util::future::{Either, select};

use teloxide::{
    Bot, RequestError,
    errors::ApiError,
    payloads::{
        EditMessageTextSetters, SendAudioSetters, SendDocumentSetters, SendVideoSetters,
        SendVoiceSetters,
    },
    requests::Requester,
    types::{ChatId, FileId, InlineKeyboardMarkup, InputFile, Message, MessageId},
};

use crate::{
    cache::{Cache, CachedFile, FileKind, Key},
    config::Config,
    error::{Error, Result},
    i18n::{Language, Text},
    media::{self, Format, Media},
    progress::Progress,
    upload,
};

pub struct Request<'a> {
    pub bot_id: u64,
    pub chat: ChatId,
    pub status: MessageId,
    pub media: &'a Media,
    pub format: Format,
    pub language: Language,
}

pub async fn deliver(
    bot: &Bot,
    cache: &Cache,
    config: &Config,
    request: Request<'_>,
) -> Result<()> {
    let key = Key {
        bot_id: request.bot_id,
        source: request.media.url.clone(),
        variant: variant(&request.format),
    };
    if let Some(cached) = cache.get(key.clone()).await? {
        if !matches_format(&request.format, &cached.kind) {
            return Err(Error::Cache(
                "cached media type does not match requested output".into(),
            ));
        }
        let feedback = Progress::cached();
        match tracked(
            bot,
            request.chat,
            request.status,
            request.language,
            &feedback,
            send_typed(
                bot,
                request.chat,
                &cached.kind,
                InputFile::file_id(FileId(cached.file_id)),
                &request.media.title,
            ),
        )
        .await
        {
            Ok(_) => {
                tracing::info!(kind = ?cached.kind, "Reused Telegram media");
                return Ok(());
            }
            Err(error) if invalid_file_id(&error) => {
                cache.remove(key.clone()).await?;
                tracing::info!("Telegram rejected a cached file ID; retrying one cold delivery");
            }
            Err(_) => return Err(Error::Telegram("cached media send failed".into())),
        }
    }
    let total = match &request.format {
        Format::Video { selection, .. } => Some(selection.size),
        Format::Opus | Format::M4a => None,
    };
    let feedback = Progress::new(total);
    let directory = config
        .download_dir
        .join(format!("job-{:016x}", rand::random::<u64>()));
    let result = async {
        compio::fs::create_dir_all(&config.download_dir).await?;
        compio::fs::create_dir(&directory).await?;
        let observe = |line: &[u8]| feedback.observe(line);
        let path = tracked(
            bot,
            request.chat,
            request.status,
            request.language,
            &feedback,
            media::download(config, request.media, &request.format, &directory, &observe),
        )
        .await?;
        feedback.download_finished();
        let size = compio::fs::metadata(&path).await?.len();
        if size > config.max_upload_bytes {
            return Err(Error::TooLarge);
        }
        let (kind, extension) = upload_kind(&request.format, &path)?;
        feedback.sending(Some(size));
        let file = InputFile::read(upload::counted(
            compio::fs::File::open(&path).await?,
            feedback.clone(),
        ))
        .file_name(upload_name(&request.media.title, extension));
        let sent = tracked(
            bot,
            request.chat,
            request.status,
            request.language,
            &feedback,
            send_typed(bot, request.chat, &kind, file, &request.media.title),
        )
        .await
        .map_err(|_| Error::Telegram("media upload failed".into()))?;
        let cached = response_file(&sent, kind)?;
        let kind = cached.kind.clone();
        cache.put(key, cached).await?;
        tracing::info!(kind = ?kind, "Uploaded Telegram media and saved its reusable ID");
        Ok(())
    }
    .await;
    match compio::runtime::spawn_blocking(move || std::fs::remove_dir_all(directory)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => tracing::warn!("Cannot remove download job directory"),
    }
    result
}

/// Cosmetic UI failure must not retire an accepted media request. @req DL-11 DL-14
pub(crate) async fn progress(
    bot: &Bot,
    chat: ChatId,
    status: MessageId,
    language: Language,
    text: Text,
) {
    if bot
        .edit_message_text(chat, status, language.text(text))
        .reply_markup(InlineKeyboardMarkup::default())
        .await
        .is_err()
    {
        tracing::warn!(stage = ?text, "Cannot update delivery status");
    }
}

/// Poll media work during cosmetic edits and finish pending edits before terminal UI. @req DL-11 DL-14
async fn tracked<F: Future>(
    bot: &Bot,
    chat: ChatId,
    status: MessageId,
    language: Language,
    feedback: &Progress,
    work: F,
) -> F::Output {
    let mut work = std::pin::pin!(work);
    let mut next = Instant::now();
    let mut previous = None;
    loop {
        let tick = compio::time::sleep(next.saturating_duration_since(Instant::now()));
        match select(work.as_mut(), std::pin::pin!(tick)).await {
            Either::Left((output, _)) => return output,
            Either::Right(_) => {}
        }
        next = Instant::now() + Duration::from_millis(3500);
        let text = feedback.render(language);
        if previous.as_ref() == Some(&text) {
            continue;
        }
        previous = Some(text.clone());
        let edit = async {
            compio::time::timeout(Duration::from_secs(2), async {
                bot.edit_message_text(chat, status, text)
                    .reply_markup(InlineKeyboardMarkup::default())
                    .await
            })
            .await
            .is_ok_and(|result| result.is_ok())
        };
        let edit = std::pin::pin!(edit);
        let succeeded = match select(work.as_mut(), edit).await {
            Either::Left((output, edit)) => {
                if !edit.await && feedback.warn_once() {
                    tracing::warn!("Cannot update media progress");
                }
                return output;
            }
            Either::Right((succeeded, _)) => succeeded,
        };
        if !succeeded && feedback.warn_once() {
            tracing::warn!("Cannot update media progress");
        }
    }
}

pub(crate) fn variant(format: &Format) -> String {
    match format {
        Format::Opus => "voice-opus-cover-v2".into(),
        Format::M4a => "music-native-aac-v2".into(),
        Format::Video { height, .. } => format!("video-{height}p-v2"),
    }
}

fn matches_format(format: &Format, kind: &FileKind) -> bool {
    matches!(
        (format, kind),
        (Format::Opus, FileKind::Voice)
            | (Format::M4a, FileKind::Audio)
            | (Format::Video { .. }, FileKind::Video | FileKind::Document)
    )
}

fn upload_kind(format: &Format, path: &Path) -> Result<(FileKind, &'static str)> {
    match (format, path.extension().and_then(|ext| ext.to_str())) {
        (Format::Opus, Some("opus")) => Ok((FileKind::Voice, "ogg")),
        (Format::M4a, Some("m4a")) => Ok((FileKind::Audio, "m4a")),
        (Format::Video { .. }, Some("mp4")) => Ok((FileKind::Video, "mp4")),
        (Format::Video { .. }, Some("mkv")) => Ok((FileKind::Document, "mkv")),
        _ => Err(Error::YtDlp(
            "download format does not match its requested delivery".into(),
        )),
    }
}

fn upload_name(title: &str, extension: &str) -> String {
    let name: String = title
        .chars()
        .filter(|character| character.is_alphanumeric() || matches!(character, '-' | '_'))
        .take(100)
        .collect();
    match name.is_empty() {
        true => format!("media.{extension}"),
        false => format!("{name}.{extension}"),
    }
}

async fn send_typed(
    bot: &Bot,
    chat: ChatId,
    kind: &FileKind,
    file: InputFile,
    title: &str,
) -> std::result::Result<Message, RequestError> {
    let title: String = title.chars().take(200).collect();
    match kind {
        FileKind::Voice => bot.send_voice(chat, file).caption(title).await,
        FileKind::Audio => {
            bot.send_audio(chat, file)
                .title(title.clone())
                .caption(title)
                .await
        }
        FileKind::Video => {
            bot.send_video(chat, file)
                .supports_streaming(true)
                .caption(title)
                .await
        }
        FileKind::Document => bot.send_document(chat, file).caption(title).await,
    }
}

fn response_file(message: &Message, kind: FileKind) -> Result<CachedFile> {
    let file = match kind {
        FileKind::Voice => message.voice().map(|voice| &voice.file.id),
        FileKind::Audio => message.audio().map(|audio| &audio.file.id),
        FileKind::Video => message.video().map(|video| &video.file.id),
        FileKind::Document => message.document().map(|document| &document.file.id),
    }
    .ok_or_else(|| Error::Telegram("upload response lacks the requested media type".into()))?;
    if file.0.trim().is_empty() {
        return Err(Error::Telegram(
            "upload response lacks a reusable file ID".into(),
        ));
    }
    Ok(CachedFile {
        kind,
        file_id: file.0.clone(),
    })
}

fn invalid_file_id(error: &RequestError) -> bool {
    matches!(
        error,
        RequestError::Api(
            ApiError::WrongFileId | ApiError::WrongFileIdOrUrl | ApiError::FileIdInvalid
        )
    )
}

#[cfg(test)]
mod tests {
    use super::{invalid_file_id, upload_kind, upload_name, variant};
    use crate::{cache::FileKind, media::Format};
    use std::path::Path;
    use teloxide::{RequestError, errors::ApiError};

    #[test]
    fn media_type_and_filename_preserve_playback_contract() {
        assert_eq!(
            upload_kind(&Format::Opus, Path::new("media.opus")).expect("Opus is Ogg"),
            (FileKind::Voice, "ogg")
        );
        assert_eq!(
            upload_kind(&Format::M4a, Path::new("media.m4a")).expect("M4A is music"),
            (FileKind::Audio, "m4a")
        );
        assert_eq!(
            upload_kind(
                &Format::Video {
                    height: 720,
                    selection: crate::video::Selection {
                        selector: "all[format_id=v720][height=720]+all[format_id=a]".into(),
                        size: crate::video::Size::Exact(1024)
                    }
                },
                Path::new("media.mp4")
            )
            .expect("MP4 is playable video"),
            (FileKind::Video, "mp4")
        );
        assert!(upload_kind(&Format::M4a, Path::new("media.opus")).is_err());
        assert_eq!(
            upload_name("../HOME - Resonance", "m4a"),
            "HOME-Resonance.m4a"
        );
    }

    #[test]
    fn video_cache_policies_isolate_explicit_resolutions() {
        let video = |height| Format::Video {
            height,
            selection: crate::video::Selection {
                selector: format!("v{height}[height={height}]+a"),
                size: crate::video::Size::Exact(1024),
            },
        };
        assert_ne!(variant(&video(720)), variant(&video(1080)));
        assert_ne!(variant(&video(1080)), "video-1080-v1");
        assert_ne!(variant(&Format::Opus), "voice-opus-v1");
        assert_eq!(variant(&Format::M4a), "music-native-aac-v2");
    }

    #[test]
    fn only_explicit_invalid_ids_allow_a_cold_retry() {
        assert!(invalid_file_id(&RequestError::Api(ApiError::FileIdInvalid)));
        assert!(!invalid_file_id(&RequestError::Api(ApiError::InvalidToken)));
    }
}
