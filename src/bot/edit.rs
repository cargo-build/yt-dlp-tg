//! Owned, bounded replies for correcting recognized track fields. @req DL-1 DL-3 DL-13

use std::{collections::HashMap, time::Instant};

use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    requests::Requester,
    types::{ChatId, ForceReply, Message, MessageId, UserId},
};

use super::{App, MAX_SESSIONS, SESSION_TTL, Session};
use crate::{
    i18n::{Language, Text},
    ocr,
};

#[derive(Clone, Copy)]
pub(super) enum Field {
    Artist,
    Title,
}

impl Field {
    pub(super) fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Artist),
            1 => Some(Self::Title),
            _ => None,
        }
    }

    fn prompt(self) -> Text {
        match self {
            Self::Artist => Text::EnterArtist,
            Self::Title => Text::EnterTitle,
        }
    }

    fn value(self, track: &ocr::Track) -> &str {
        match self {
            Self::Artist => &track.artist,
            Self::Title => &track.title,
        }
    }
}

#[derive(Clone, Copy)]
struct Request {
    owner: UserId,
    session: u64,
    version: u32,
    field: Field,
    created: Instant,
    used: bool,
}

enum Claim {
    Unrelated,
    Expired,
    Owned(Request),
}

#[derive(Default)]
pub(super) struct Pending {
    requests: HashMap<(ChatId, MessageId), Request>,
}

impl Pending {
    fn insert(&mut self, key: (ChatId, MessageId), request: Request) {
        self.requests
            .retain(|_, value| value.created.elapsed() < SESSION_TTL);
        if self.requests.len() >= MAX_SESSIONS
            && let Some(oldest) = self
                .requests
                .iter()
                .min_by_key(|(_, value)| value.created)
                .map(|(key, _)| *key)
        {
            self.requests.remove(&oldest);
        }
        self.requests.insert(key, request);
    }

    fn claim(&mut self, key: (ChatId, MessageId), owner: UserId) -> Claim {
        match self.requests.get_mut(&key) {
            None => Claim::Unrelated,
            Some(request)
                if request.owner != owner
                    || request.used
                    || request.created.elapsed() >= SESSION_TTL =>
            {
                Claim::Expired
            }
            Some(request) => {
                request.used = true;
                Claim::Owned(*request)
            }
        }
    }
}

pub(super) async fn begin(
    bot: &Bot,
    app: &App,
    session: &Session,
    field: Field,
    note: Option<&str>,
) -> Result<(), teloxide::RequestError> {
    let Some(track) = &session.track else {
        return Ok(());
    };
    let prompt = format!(
        "{}\n\n{}",
        session.language.text(field.prompt()),
        field.value(track)
    );
    let text = match note {
        Some(note) => format!("{note}\n\n{prompt}"),
        None => prompt,
    };
    let sent = bot
        .send_message(session.chat, text)
        .reply_markup(ForceReply::new())
        .await?;
    app.edits.borrow_mut().insert(
        (session.chat, sent.id),
        Request {
            owner: session.owner,
            session: session.id,
            version: session.version,
            field,
            created: Instant::now(),
            used: false,
        },
    );
    Ok(())
}

pub(super) async fn reply(
    bot: &Bot,
    app: &App,
    message: &Message,
    owner: UserId,
    language: Language,
) -> Result<bool, teloxide::RequestError> {
    let Some(parent) = message.reply_to_message() else {
        return Ok(false);
    };
    let claim = app
        .edits
        .borrow_mut()
        .claim((message.chat.id, parent.id), owner);
    let request = match claim {
        Claim::Unrelated => return Ok(false),
        Claim::Expired => {
            bot.send_message(message.chat.id, language.text(Text::Expired))
                .await?;
            return Ok(true);
        }
        Claim::Owned(request) => request,
    };
    let session = {
        let mut sessions = app.sessions.borrow_mut();
        match sessions.get(&request.session) {
            Some(session)
                if session.matches(owner, message.chat.id, session.message, request.version)
                    && session.track.is_some() =>
            {
                sessions.remove(&request.session)
            }
            _ => None,
        }
    };
    let Some(mut session) = session else {
        bot.send_message(message.chat.id, language.text(Text::Expired))
            .await?;
        return Ok(true);
    };
    let track = session
        .track
        .as_ref()
        .expect("owned correction requires a track");
    let fields = message
        .text()
        .map(str::trim)
        .map(|value| match request.field {
            Field::Artist => (value.to_owned(), track.title.clone()),
            Field::Title => (track.artist.clone(), value.to_owned()),
        });
    let note = match fields {
        Some((artist, title)) if ocr::valid_fields(&artist, &title) => {
            let result = match &app.config.ocr {
                Some(settings) => ocr::render(settings, session.language, &artist, &title).await,
                None => Err(crate::error::Error::Image),
            };
            match result {
                Ok(ocr::Recognition::Track(track)) => {
                    session.track = Some(track);
                    session.version += 1;
                    session.quoted = false;
                    session.results.clear();
                    session.selected = None;
                    session.page = 0;
                    None
                }
                Ok(ocr::Recognition::Rejected { message }) => Some(message),
                _ => Some(session.language.text(Text::Failed).to_owned()),
            }
        }
        _ => Some(session.language.text(Text::InvalidEdit).to_owned()),
    };
    let displayed = match note {
        Some(note) => begin(bot, app, &session, request.field, Some(&note)).await,
        None => session.show_preview(bot).await,
    };
    app.remember(session);
    displayed?;
    Ok(true)
}
