//! Owned video availability menus and guarded resolution selection. @req DL-3 DL-5

use teloxide::{
    Bot,
    types::{InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::Instrument;

use super::{App, Session};
use crate::{
    error::{Error, Result},
    i18n::Text,
    media::{self, Format},
    telemetry,
    video::{Availability, Choice},
};

const PER_PAGE: usize = 6;

pub(super) async fn prepare(
    bot: &Bot,
    app: &App,
    session: &mut Session,
) -> Result<(String, InlineKeyboardMarkup), teloxide::RequestError> {
    if session
        .selected
        .as_ref()
        .is_some_and(|selected| selected.video_choices.is_none())
    {
        session.status(bot, Text::Checking).await?;
        let selected = session
            .selected
            .as_ref()
            .expect("video callback has a selected source");
        match media::video_choices(&app.config, &selected.media)
            .instrument(tracing::info_span!("media_request", request_id = session.id)).await {
            Ok(choices) => {
                telemetry::video_options(session.id, &selected.media.url, &choices);
                session
                    .selected
                    .as_mut()
                    .expect("metadata keeps the selected source")
                    .video_choices = Some(choices)
            }
            Err(error) => {
                tracing::warn!(request_id = session.id, stage = "video_options", outcome = "failed",
                    reason = telemetry::reason(&error), source = ?telemetry::source(&selected.media.url),
                    "Video availability check failed");
                return Ok((
                    source(session, session.language.text(Text::VideoCheckFailed)),
                    InlineKeyboardMarkup::new([
                        vec![session.button(session.language.text(Text::VideoRetry), 'v', 0)],
                        audio_row(session),
                        vec![session.button(session.language.text(Text::Back), 'f', 0)],
                    ]),
                ));
            }
        }
    }
    Ok(menu(session, 0, app.config.max_upload_bytes))
}

pub(super) fn menu(session: &Session, page: usize, limit: u64) -> (String, InlineKeyboardMarkup) {
    let choices = match session
        .selected
        .as_ref()
        .and_then(|selected| selected.video_choices.as_deref())
    {
        Some(choices) => choices,
        None => {
            return (
                source(session, session.language.text(Text::VideoCheckFailed)),
                InlineKeyboardMarkup::new([
                    audio_row(session),
                    vec![session.button(session.language.text(Text::Back), 'f', 0)],
                ]),
            );
        }
    };
    let page = page.min(choices.len().saturating_sub(1) / PER_PAGE);
    let mut rows: Vec<_> = choices
        .iter()
        .enumerate()
        .skip(page * PER_PAGE)
        .take(PER_PAGE)
        .map(|(index, choice)| {
            vec![session.button(
                label(session, choice, limit),
                match choice.availability {
                    Availability::Available(_) => 'd',
                    _ => 'u',
                },
                index,
            )]
        })
        .collect();
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(session.button(session.language.text(Text::Previous), 'k', page - 1));
    }
    if (page + 1) * PER_PAGE < choices.len() {
        navigation.push(session.button(session.language.text(Text::Next), 'k', page + 1));
    }
    if !navigation.is_empty() {
        rows.push(navigation);
    }
    rows.push(audio_row(session));
    rows.push(vec![session.button(
        session.language.text(Text::Back),
        'f',
        0,
    )]);
    let heading = session
        .language
        .text(Text::VideoOptions)
        .replace("{limit}", &mb(limit));
    (source(session, &heading), InlineKeyboardMarkup::new(rows))
}

fn source(session: &Session, body: &str) -> String {
    match &session.selected {
        Some(selected) => session.context(&format!(
            "{}\n{}\n\n{body}",
            selected.media.title.chars().take(240).collect::<String>(),
            selected.media.url
        )),
        None => session.context(body),
    }
}

fn audio_row(session: &Session) -> Vec<InlineKeyboardButton> {
    vec![
        session.button(session.language.text(Text::Music), 'm', 0),
        session.button(session.language.text(Text::Audio), 'a', 0),
    ]
}

fn label(session: &Session, choice: &Choice, limit: u64) -> String {
    let unavailable = session.language.text(Text::VideoUnavailable);
    match &choice.availability {
        Availability::Available(selection) => format!(
            "{}p · ~{} MB",
            choice.height,
            size_mb(selection.size.bytes())
        ),
        Availability::TooLarge(_) => {
            format!("{}p — {unavailable} (>{} MB)", choice.height, mb(limit))
        }
        Availability::Unknown => format!(
            "{}p — {unavailable} ({})",
            choice.height,
            session.language.text(Text::VideoSizeUnknown)
        ),
        Availability::Missing => format!("{}p — {unavailable}", choice.height),
    }
}

fn mb(bytes: u64) -> String {
    (bytes / (1024 * 1024)).to_string()
}

fn size_mb(bytes: u64) -> String {
    let tenths = (u128::from(bytes) * 10).div_ceil(1024 * 1024);
    format!("{}.{}", tenths / 10, tenths % 10)
}

pub(super) fn blocked(session: &Session, action: char, index: usize, limit: u64) -> Option<String> {
    if !matches!(action, 'd' | 'u') {
        return None;
    }
    let choice = session
        .selected
        .as_ref()
        .and_then(|selected| selected.video_choices.as_ref())
        .and_then(|choices| choices.get(index));
    let (note, reason) = match choice {
        Some(Choice {
            height,
            availability: Availability::Available(selection),
        }) if action == 'd' && (1..=1080).contains(height) && selection.size.bytes() <= limit => {
            return None;
        }
        Some(Choice {
            availability: Availability::TooLarge(_),
            ..
        }) => (session.language.text(Text::TooLarge), "too_large"),
        Some(Choice {
            availability: Availability::Unknown,
            ..
        }) => (
            session.language.text(Text::VideoSizeUnknown),
            "unknown_size",
        ),
        Some(Choice {
            availability: Availability::Missing,
            ..
        }) => (session.language.text(Text::VideoMissing), "missing"),
        _ => (session.language.text(Text::Expired), "invalid_selection"),
    };
    tracing::info!(request_id = session.id, stage = "video_selection", outcome = "unavailable",
        reason, height = ?choice.map(|choice| choice.height), "Media request");
    Some(note.to_owned())
}

pub(super) fn format(session: &Session, index: usize) -> Result<Format> {
    match session
        .selected
        .as_ref()
        .and_then(|selected| selected.video_choices.as_ref())
        .and_then(|choices| choices.get(index))
    {
        Some(Choice {
            height,
            availability: Availability::Available(selection),
        }) if (1..=1080).contains(height) => Ok(Format::Video {
            height: *height,
            selection: selection.clone(),
        }),
        _ => Err(Error::YtDlp("video choice is unavailable".into())),
    }
}
