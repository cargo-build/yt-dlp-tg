//! Authorized conversations, callback ownership and download delivery. @req DL-1 DL-3

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use futures_util::FutureExt;
use teloxide::{
    Bot,
    dispatching::Dispatcher,
    dispatching::UpdateFilterExt,
    dptree,
    payloads::{AnswerCallbackQuerySetters, EditMessageTextSetters},
    requests::Requester,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Me, Message, MessageId,
        Update, UserId,
    },
};
use tracing::Instrument;

use crate::{
    cache::Cache,
    config::{Config, Mode},
    delivery,
    error::{Error, Result},
    i18n::{Language, Text},
    jobs,
    media::{self, Format, Media, Provider},
    ocr,
    queue::Queue,
    telemetry,
};

mod edit;
mod quality;

const SESSION_TTL: Duration = Duration::from_secs(20 * 60);
const MAX_SESSIONS: usize = 256;
const PROVIDERS_PER_PAGE: usize = 5;
const SEND_TIMEOUT_SECS: u64 = 300;

#[derive(Clone)]
struct Selection {
    media: Media,
    video_choices: Option<Vec<crate::video::Choice>>,
}

impl From<Media> for Selection {
    fn from(media: Media) -> Self {
        Self {
            media,
            video_choices: None,
        }
    }
}

struct App {
    config: Config,
    providers: Vec<Provider>,
    youtube_provider: usize,
    sessions: RefCell<HashMap<u64, Session>>,
    edits: RefCell<edit::Pending>,
    media_slot: async_lock::Semaphore,
    cache: Cache,
    queue: Option<Rc<Queue>>,
    bot_id: Cell<Option<u64>>,
}

impl App {
    async fn current_bot_id(&self, bot: &Bot) -> Result<u64> {
        match self.bot_id.get() {
            Some(id) => Ok(id),
            None => {
                let me = bot
                    .get_me()
                    .await
                    .map_err(|_| Error::Telegram("getMe failed".into()))?;
                self.bot_id.set(Some(me.id.0));
                Ok(me.id.0)
            }
        }
    }

    fn remember(&self, session: Session) {
        let mut sessions = self.sessions.borrow_mut();
        sessions.retain(|_, session| session.created.elapsed() < SESSION_TTL);
        if sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = sessions
                .values()
                .min_by_key(|session| session.created)
                .map(|session| session.id)
        {
            sessions.remove(&oldest);
        }
        sessions.insert(session.id, session);
    }

    async fn acquire_media_slot(
        &self,
        bot: &Bot,
        session: &Session,
    ) -> std::result::Result<async_lock::SemaphoreGuard<'_>, teloxide::RequestError> {
        match self.media_slot.try_acquire() {
            Some(slot) => Ok(slot),
            None => {
                session.status(bot, Text::Waiting).await?;
                Ok(self.media_slot.acquire().await)
            }
        }
    }
}

#[derive(Clone)]
struct Session {
    id: u64,
    version: u32,
    owner: UserId,
    chat: ChatId,
    message: MessageId,
    language: Language,
    created: Instant,
    query: String,
    track: Option<ocr::Track>,
    quoted: bool,
    provider: usize,
    page: usize,
    results: Vec<Media>,
    selected: Option<Selection>,
}

impl Session {
    fn new(
        app: &App,
        owner: UserId,
        chat: ChatId,
        message: MessageId,
        language: Language,
        query: String,
    ) -> Self {
        Self {
            id: rand::random(),
            version: 0,
            owner,
            chat,
            message,
            language,
            created: Instant::now(),
            query,
            track: None,
            quoted: false,
            provider: app.youtube_provider,
            page: 0,
            results: Vec::new(),
            selected: None,
        }
    }

    async fn status(
        &self,
        bot: &Bot,
        text: Text,
    ) -> std::result::Result<Message, teloxide::RequestError> {
        bot.edit_message_text(self.chat, self.message, self.language.text(text))
            .reply_markup(InlineKeyboardMarkup::default())
            .await
    }

    fn button(&self, text: impl Into<String>, action: char, value: usize) -> InlineKeyboardButton {
        InlineKeyboardButton::callback(
            text,
            format!("{}:{}:{action}:{value}", self.id, self.version),
        )
    }

    fn matches(&self, owner: UserId, chat: ChatId, message: MessageId, version: u32) -> bool {
        self.owner == owner
            && self.chat == chat
            && self.message == message
            && self.version == version
            && self.created.elapsed() < SESSION_TTL
    }

    fn search_query(&self) -> &str {
        match &self.track {
            Some(track) if self.quoted => &track.quoted_query,
            Some(track) => &track.plain_query,
            None => &self.query,
        }
    }

    fn context(&self, body: &str) -> String {
        match &self.track {
            Some(track) => format!(
                "{}: {}\n{}: {}\n\n{body}",
                self.language.text(Text::Artist),
                track.artist,
                self.language.text(Text::Title),
                track.title
            ),
            None => body.to_owned(),
        }
    }

    fn track_controls(&self) -> Vec<Vec<InlineKeyboardButton>> {
        match self.track {
            Some(_) => vec![
                vec![
                    self.button(self.language.text(Text::SearchYouTube), 'q', 0),
                    self.button(self.language.text(Text::SearchNames), 'q', 1),
                ],
                vec![
                    self.button(self.language.text(Text::EditArtist), 'e', 0),
                    self.button(self.language.text(Text::EditTitle), 'e', 1),
                ],
            ],
            None => Vec::new(),
        }
    }

    async fn show_preview(&self, bot: &Bot) -> std::result::Result<(), teloxide::RequestError> {
        let (body, keyboard) = preview_menu(self);
        bot.edit_message_text(self.chat, self.message, body)
            .reply_markup(keyboard)
            .await?;
        Ok(())
    }
}

fn preview_menu(session: &Session) -> (String, InlineKeyboardMarkup) {
    match (&session.track, &session.selected) {
        (Some(_), Some(selected)) => {
            let media = &selected.media;
            let title: String = media.title.chars().take(240).collect();
            let body = format!(
                "{}\n{title}\n{}\n\n{}",
                session.language.text(Text::SuggestedMatch),
                media.url,
                session.language.text(Text::DownloadHint)
            );
            (
                session.context(&body),
                InlineKeyboardMarkup::new([
                    vec![
                        session.button(session.language.text(Text::DownloadMusic), 'm', 0),
                        session.button(session.language.text(Text::DownloadVideo), 'v', 0),
                    ],
                    vec![
                        session.button(session.language.text(Text::OtherFormats), 'f', 0),
                        session.button(session.language.text(Text::MoreResults), 'b', 0),
                    ],
                    vec![
                        session.button(session.language.text(Text::EditArtist), 'e', 0),
                        session.button(session.language.text(Text::EditTitle), 'e', 1),
                    ],
                ]),
            )
        }
        (Some(_), None) => (
            session.context(session.language.text(Text::TrackHint)),
            InlineKeyboardMarkup::new(session.track_controls()),
        ),
        (None, _) => (
            format!(
                "{}\n\n{}\n\n{}",
                session.language.text(Text::ImagePreview),
                session.query,
                session.language.text(Text::ImageHint)
            ),
            InlineKeyboardMarkup::new([vec![session.button(
                session.language.text(Text::SearchYouTube),
                'q',
                0,
            )]]),
        ),
    }
}

pub async fn run(config: Config) -> Result<()> {
    let providers = media::providers(&config).await?;
    let youtube_provider = providers
        .iter()
        .position(|provider| provider.key == "ytsearch")
        .ok_or_else(|| Error::Config("installed yt-dlp does not expose ytsearch".into()))?;
    let mut client = cyper::Client::builder();
    if let Some(proxy) = &config.tg_proxy {
        client = client.proxy(
            cyper::proxy::Proxy::all(proxy)
                .map_err(|_| Error::Config("invalid TG_PROXY".into()))?,
        );
    }
    let bot = Bot::with_client(
        config.bot_token.clone(),
        client
            .build()
            .map_err(|_| Error::Config("cannot construct Telegram client".into()))?,
    )
    .set_api_url(
        url::Url::parse(&config.tg_api_base)
            .map_err(|_| Error::Config("invalid TG_API_BASE".into()))?,
    )
    .set_request_timeout(Duration::from_secs(SEND_TIMEOUT_SECS));
    let cache = Cache::open(config.cache_path.clone()).await?;
    let queue = match &config.iggy {
        Some(iggy) => Some(Queue::connect(iggy, &cache).await?),
        None => None,
    };
    let app = Rc::new(App {
        config,
        providers,
        youtube_provider,
        sessions: RefCell::new(HashMap::new()),
        edits: RefCell::new(edit::Pending::default()),
        media_slot: async_lock::Semaphore::new(1),
        cache,
        queue,
        bot_id: Cell::new(None),
    });
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(message))
        .branch(Update::filter_callback_query().endpoint(callback));
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![Rc::clone(&app)])
        .worker_queue_size(4)
        .error_handler(Rc::new(|_: teloxide::RequestError| async {
            tracing::warn!("Telegram request failed");
        }))
        .build();
    let shutdown = dispatcher.shutdown_token();
    let queue_work = {
        let app = Rc::clone(&app);
        let bot = bot.clone();
        async move {
            match &app.queue {
                Some(queue) => {
                    Rc::clone(queue)
                        .run(|request| deliver_request(&bot, &app, request))
                        .await
                }
                None => std::future::pending().await,
            }
        }
        .map(|result: Result<()>| result.map_err(Rc::new))
        .boxed_local()
        .shared()
    };
    tracing::info!(
        providers = app.providers.len(),
        queue = if app.queue.is_some() {
            "iggy"
        } else {
            "direct"
        },
        access = match &app.config.user_access {
            crate::config::UserAccess::All => "all",
            crate::config::UserAccess::Allowlist(_) => "allowlist",
        },
        "Starting native compio downloader"
    );
    let running = Box::pin(async {
        match app.config.mode {
            Mode::Polling => {
                bot.delete_webhook()
                    .await
                    .map_err(|_| Error::Telegram("deleteWebhook failed".into()))?;
                let listener = teloxide::update_listeners::Polling::builder(bot.clone())
                    .timeout(Duration::from_secs(20))
                    .build();
                dispatcher
                    .try_dispatch_with_listener(
                        listener,
                        Rc::new(|_: teloxide::RequestError| async {
                            tracing::warn!("Telegram polling failed");
                        }),
                    )
                    .await
                    .map_err(|_| Error::Telegram("polling setup failed".into()))?;
            }
            Mode::Webhook => {
                let public = app
                    .config
                    .webhook_public_url
                    .as_deref()
                    .ok_or_else(|| Error::Config("missing webhook public URL".into()))?;
                let mut url = url::Url::parse(public)
                    .map_err(|_| Error::Config("invalid webhook public URL".into()))?;
                url.set_path(&format!("/{}", app.config.webhook_path.trim_matches('/')));
                let address = app
                    .config
                    .listen_addr
                    .parse()
                    .map_err(|_| Error::Config("invalid listen address".into()))?;
                let listener = teloxide::update_listeners::webhooks::tako(
                    bot.clone(),
                    teloxide::update_listeners::webhooks::Options::new(address, url),
                )
                .await
                .map_err(|_| Error::Telegram("webhook setup failed".into()))?;
                dispatcher
                    .try_dispatch_with_listener(
                        listener,
                        Rc::new(|_: std::convert::Infallible| async {}),
                    )
                    .await
                    .map_err(|_| Error::Telegram("webhook dispatch setup failed".into()))?;
            }
        }
        Ok(())
    });
    let control = Box::pin(futures_util::future::select(
        Box::pin(stop_signal()),
        queue_work.clone(),
    ));
    let result = match futures_util::future::select(running, control).await {
        futures_util::future::Either::Left((result, _)) => result,
        futures_util::future::Either::Right((reason, running)) => {
            if let Some(queue) = &app.queue {
                queue.stop();
            }
            let trigger = match reason {
                futures_util::future::Either::Left((signal, _)) => signal.map_err(Error::from),
                futures_util::future::Either::Right((result, _)) => {
                    result.map_err(|_| Error::Queue("queue driver failed"))
                }
            };
            let drained = match shutdown.shutdown() {
                Ok(drained) => {
                    let (result, ()) = futures_util::future::join(running, drained).await;
                    result
                }
                Err(_idle) => {
                    drop(running);
                    tracing::info!("Startup interrupted before dispatch began");
                    if app.config.mode == Mode::Webhook {
                        let _cleanup = compio::time::timeout(Duration::from_secs(5), async {
                            bot.delete_webhook().await
                        })
                        .await;
                    }
                    Ok(())
                }
            };
            trigger.and(drained)
        }
    };
    match &app.queue {
        Some(queue) => {
            queue.stop();
            let stopped = queue_work
                .await
                .map_err(|_| Error::Queue("queue driver failed"));
            result.and(stopped)
        }
        None => result,
    }
}

async fn stop_signal() -> std::io::Result<()> {
    let interrupt = compio::signal::ctrl_c();
    let terminate = compio::signal::unix::signal(libc::SIGTERM);
    match futures_util::future::select(std::pin::pin!(interrupt), std::pin::pin!(terminate)).await {
        futures_util::future::Either::Left((result, _))
        | futures_util::future::Either::Right((result, _)) => result,
    }
}

async fn message(
    bot: Bot,
    app: Rc<App>,
    message: Message,
) -> std::result::Result<(), teloxide::RequestError> {
    let Some(user) = message.from.as_ref() else {
        return Ok(());
    };
    let language = Language::from_code(user.language_code.as_deref());
    if message
        .text()
        .is_some_and(|text| text.trim().split('@').next() == Some("/id"))
    {
        bot.send_message(message.chat.id, user.id.0.to_string())
            .await?;
        return Ok(());
    }
    if !authorized(&app.config, user.id) {
        bot.send_message(message.chat.id, language.text(Text::Denied))
            .await?;
        return Ok(());
    }
    if edit::reply(&bot, &app, &message, user.id, language).await? {
        return Ok(());
    }
    if let Some(image) = ocr::image(&message) {
        return match app.config.ocr {
            Some(_) => image_message(&bot, &app, &message, user.id, language, image).await,
            None => {
                bot.send_message(message.chat.id, language.text(Text::ImageDisabled))
                    .await?;
                Ok(())
            }
        };
    }
    let text = match message.text() {
        Some(text) => text,
        None => {
            bot.send_message(message.chat.id, language.text(Text::Unsupported))
                .await?;
            return Ok(());
        }
    };
    let query = query_text(text);
    if query.is_empty() || matches!(query, "/start" | "/help") || query.chars().count() > 1024 {
        let help = match app.config.ocr {
            Some(_) => format!(
                "{}\n{}",
                language.text(Text::Help),
                language.text(Text::ImageHelp)
            ),
            None => language.text(Text::Help).into(),
        };
        let help = format!("{help}\n\n{}", limits(&app.config, language));
        bot.send_message(message.chat.id, help).await?;
        return Ok(());
    }
    let received = bot
        .send_message(message.chat.id, language.text(Text::Received))
        .await?;
    let mut session = Session::new(
        &app,
        user.id,
        message.chat.id,
        received.id,
        language,
        query.into(),
    );
    let input_url = media::http_url(query);
    tracing::info!(
        request_id = session.id,
        stage = "accepted",
        source = ?input_url.as_deref().and_then(telemetry::source),
        "Media request"
    );
    let started = Instant::now();
    let _slot = app.acquire_media_slot(&bot, &session).await?;
    tracing::info!(
        request_id = session.id,
        stage = "transfer_phase",
        component = "media_slot",
        outcome = "ok",
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "Media component"
    );
    session.created = Instant::now();
    if let Some(url) = input_url {
        session.status(&bot, Text::Checking).await?;
        match media::probe(&app.config, &url)
            .instrument(tracing::info_span!(
                "media_request",
                request_id = session.id
            ))
            .await
        {
            Ok(media) => {
                tracing::info!(
                    request_id = session.id,
                    stage = "probe",
                    outcome = "ok",
                    source = ?telemetry::source(&media.url),
                    "Media request"
                );
                session.selected = Some(media.into());
            }
            Err(error) => tracing::info!(
                request_id = session.id,
                stage = "probe",
                outcome = "search_fallback",
                reason = telemetry::reason(&error),
                source = ?telemetry::source(&url),
                "Media request"
            ),
        }
    }
    let (body, keyboard) = match session.selected {
        Some(_) => format_menu(&session),
        None => {
            session.status(&bot, Text::Searching).await?;
            refresh_results(&app, &mut session).await
        }
    };
    bot.edit_message_text(session.chat, session.message, body)
        .reply_markup(keyboard)
        .await?;
    app.remember(session);
    Ok(())
}

async fn image_message(
    bot: &Bot,
    app: &App,
    message: &Message,
    owner: UserId,
    language: Language,
    image: ocr::Image,
) -> std::result::Result<(), teloxide::RequestError> {
    let received = bot
        .send_message(message.chat.id, language.text(Text::ImageReceived))
        .await?;
    let mut session = Session::new(
        app,
        owner,
        message.chat.id,
        received.id,
        language,
        String::new(),
    );
    let _slot = app.acquire_media_slot(bot, &session).await?;
    let result = ocr::recognize(
        bot,
        &app.config,
        ocr::Request {
            chat: session.chat,
            status: session.message,
            language,
            image,
        },
    )
    .await;
    match result {
        Ok(ocr::Recognition::Rejected { message }) => {
            bot.edit_message_text(session.chat, session.message, message)
                .reply_markup(InlineKeyboardMarkup::default())
                .await?;
        }
        Ok(ocr::Recognition::Text { query }) => {
            session.query = query;
            session.created = Instant::now();
            session.show_preview(bot).await?;
            app.remember(session);
        }
        Ok(ocr::Recognition::Track(track)) => {
            session.track = Some(track);
            session.created = Instant::now();
            session.quoted = true;
            session.status(bot, Text::Searching).await?;
            let (body, keyboard) = track_results(app, &mut session).await;
            bot.edit_message_text(session.chat, session.message, body)
                .reply_markup(keyboard)
                .await?;
            app.remember(session);
        }
        Err(error) => {
            let text = match error {
                Error::ImageEmpty => Text::ImageEmpty,
                Error::ImageTooLarge => Text::ImageTooLarge,
                Error::Timeout => Text::ImageTimeout,
                _ => Text::ImageFailed,
            };
            tracing::warn!("Image recognition failed");
            session.status(bot, text).await?;
        }
    }
    Ok(())
}

async fn callback(
    bot: Bot,
    app: Rc<App>,
    query: CallbackQuery,
    me: Me,
) -> std::result::Result<(), teloxide::RequestError> {
    app.bot_id.set(Some(me.id.0));
    let language = Language::from_code(query.from.language_code.as_deref());
    if !authorized(&app.config, query.from.id) {
        bot.answer_callback_query(query.id)
            .text(language.text(Text::Denied))
            .await?;
        return Ok(());
    }
    let Some(message) = &query.message else {
        bot.answer_callback_query(query.id)
            .text(language.text(Text::Expired))
            .await?;
        return Ok(());
    };
    let callback = query.data.as_deref().and_then(parse_callback);
    let blocked = callback.and_then(|(id, version, action, value)| {
        let sessions = app.sessions.borrow();
        let session = sessions.get(&id)?;
        match session.matches(query.from.id, message.chat().id, message.id(), version) {
            true => quality::blocked(session, action, value, app.config.max_upload_bytes),
            false => None,
        }
    });
    if let Some(note) = blocked {
        bot.answer_callback_query(query.id).text(note).await?;
        return Ok(());
    }
    let session = callback.and_then(|(id, version, action, value)| {
        let mut sessions = app.sessions.borrow_mut();
        let session = sessions.get(&id)?;
        if !session.matches(query.from.id, message.chat().id, message.id(), version) {
            return None;
        }
        sessions.remove(&id).map(|mut s| {
            s.version += 1;
            (s, action, value)
        })
    });
    let Some((mut session, action, value)) = session else {
        bot.answer_callback_query(query.id)
            .text(language.text(Text::Expired))
            .await?;
        return Ok(());
    };
    bot.answer_callback_query(query.id).await?;
    if action == 'e' {
        match edit::Field::from_index(value) {
            Some(field) if session.track.is_some() => {
                session.selected = None;
                edit::begin(&bot, &app, &session, field, None).await?;
                session.show_preview(&bot).await?;
                app.remember(session);
            }
            _ => {
                session.status(&bot, Text::Expired).await?;
            }
        }
        return Ok(());
    }
    if matches!(action, 'a' | 'm' | 'd') {
        let request = match session.selected.as_ref() {
            Some(selected) => {
                let format = match action {
                    'a' => Ok(Format::Opus),
                    'm' => Ok(Format::M4a),
                    _ => quality::format(&session, value),
                };
                match format {
                    Ok(format) => Ok(jobs::Request {
                        bot_id: me.id.0,
                        user_id: session.owner.0,
                        chat_id: session.chat.0,
                        status_message: session.message.0,
                        language: session.language,
                        media: selected.media.clone(),
                        policy: delivery::variant(&format),
                        format,
                    }),
                    Err(error) => Err(error),
                }
            }
            None => Err(Error::Telegram("no selected media".into())),
        };
        let request = request.inspect(|request| {
            tracing::info!(
                request_id = session.id,
                stage = "download",
                outcome = "requested",
                source = ?telemetry::source(&request.media.url),
                "Media request"
            );
        });
        match request {
            Ok(request) => match &app.queue {
                Some(queue) => {
                    delivery::progress(
                        &bot,
                        session.chat,
                        session.message,
                        session.language,
                        Text::Queuing,
                    )
                    .await;
                    match queue.submit(request).await {
                        Ok(Some(job)) => tracing::info!(request_id = session.id, job_id = %job.id,
                            stage = "queue", outcome = "accepted", "Media request"),
                        Ok(None) => {
                            session.status(&bot, Text::QueueFull).await?;
                        }
                        Err(_) => {
                            session.status(&bot, Text::Failed).await?;
                        }
                    }
                }
                None => {
                    deliver_request(&bot, &app, request)
                        .instrument(tracing::info_span!(
                            "download_request",
                            request_id = session.id
                        ))
                        .await
                }
            },
            Err(_) => {
                session.status(&bot, Text::Failed).await?;
            }
        }
        return Ok(());
    }
    let _slot = app.acquire_media_slot(&bot, &session).await?;
    let menu = match action {
        'v' if session.selected.is_some() => {
            Some(quality::prepare(&bot, &app, &mut session).await?)
        }
        'k' if session
            .selected
            .as_ref()
            .is_some_and(|selected| selected.video_choices.is_some()) =>
        {
            Some(quality::menu(&session, value, app.config.max_upload_bytes))
        }
        'q' if value == 0 || value == 1 && session.track.is_some() => {
            session.quoted = value == 1;
            session.provider = app.youtube_provider;
            session.page = 0;
            session.status(&bot, Text::Searching).await?;
            Some(track_results(&app, &mut session).await)
        }
        'p' => Some(providers_menu(&session, &app.providers, value)),
        'h' if value < app.providers.len() => {
            session.provider = value;
            session.page = 0;
            session.status(&bot, Text::Searching).await?;
            Some(refresh_results(&app, &mut session).await)
        }
        's' if value < 10 => {
            session.page = value;
            session.status(&bot, Text::Searching).await?;
            Some(refresh_results(&app, &mut session).await)
        }
        'r' if value < session.results.len() => {
            session.selected = Some(session.results[value].clone().into());
            Some(format_menu(&session))
        }
        'f' if session.selected.is_some() => Some(format_menu(&session)),
        'b' => {
            session.selected = None;
            Some(results_menu(&session, &app.providers[session.provider]))
        }
        _ => None,
    };
    match menu {
        Some((body, keyboard)) => {
            bot.edit_message_text(session.chat, session.message, body)
                .reply_markup(keyboard)
                .await?;
            app.remember(session);
        }
        None => {
            bot.edit_message_text(
                session.chat,
                session.message,
                session.language.text(Text::Failed),
            )
            .await?;
        }
    }
    Ok(())
}

/// Recheck persisted recipient and output authority before sharing the media slot. @req DL-1 DL-14
async fn deliver_request(bot: &Bot, app: &App, request: jobs::Request) {
    tracing::info!(stage = "delivery", outcome = "started",
        source = ?telemetry::source(&request.media.url), "Media request");
    let chat = ChatId(request.chat_id);
    let status = MessageId(request.status_message);
    let result = async {
        let bot_id = app.current_bot_id(bot).await?;
        if request.bot_id != bot_id
            || !authorized(&app.config, UserId(request.user_id))
            || !delivery::accepts_policy(&request.policy, &request.format)
        {
            return Err(Error::Queue("stored request is no longer authorized"));
        }
        let started = Instant::now();
        let _slot = match app.media_slot.try_acquire() {
            Some(slot) => slot,
            None => {
                delivery::progress(bot, chat, status, request.language, Text::Waiting).await;
                app.media_slot.acquire().await
            }
        };
        tracing::info!(
            stage = "transfer_phase",
            component = "media_slot",
            outcome = "ok",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Media component"
        );
        delivery::deliver(
            bot,
            &app.cache,
            &app.config,
            delivery::Request {
                bot_id,
                chat,
                status,
                media: &request.media,
                format: request.format,
                language: request.language,
            },
        )
        .await
    }
    .await;
    if matches!(&result, Err(Error::AacUnavailable)) {
        tracing::info!(stage = "delivery", outcome = "native_aac_unavailable",
            source = ?telemetry::source(&request.media.url), "Media request");
        let mut session = Session::new(
            app,
            UserId(request.user_id),
            chat,
            status,
            request.language,
            request.media.url.clone(),
        );
        session.selected = Some(request.media.clone().into());
        let keyboard = InlineKeyboardMarkup::new([vec![session.button(
            request.language.text(Text::Audio),
            'a',
            0,
        )]]);
        let body = format!(
            "{}\n{}\n\n{}",
            request.media.title.chars().take(240).collect::<String>(),
            request.media.url,
            request.language.text(Text::NativeAacUnavailable)
        );
        app.remember(session);
        if bot
            .edit_message_text(chat, status, body)
            .reply_markup(keyboard)
            .await
            .is_err()
        {
            tracing::warn!("Cannot offer the Opus alternative");
        }
        return;
    }
    match &result {
        Ok(()) => {
            tracing::info!(stage = "delivery", outcome = "sent", "Media request")
        }
        Err(error) => {
            tracing::warn!(
                stage = "delivery",
                outcome = "failed",
                reason = telemetry::reason(error),
                "Media request"
            )
        }
    }
    let text = match result {
        Ok(()) => Text::Sent,
        Err(Error::TooLarge) => Text::TooLarge,
        Err(Error::AacUnavailable) => Text::NativeAacUnavailable,
        Err(_) => Text::Failed,
    };
    delivery::progress(bot, chat, status, request.language, text).await;
}

async fn refresh_results(app: &App, session: &mut Session) -> (String, InlineKeyboardMarkup) {
    session.results.clear();
    session.selected = None;
    match media::search(
        &app.config,
        &app.providers[session.provider],
        session.search_query(),
        session.page,
    )
    .await
    {
        Ok(results) => {
            session.results = results;
            session.selected = None;
            results_menu(session, &app.providers[session.provider])
        }
        Err(_) => {
            let mut rows = session.track_controls();
            rows.push(vec![session.button(
                session.language.text(Text::ChooseHost),
                'p',
                0,
            )]);
            (
                session.context(session.language.text(Text::Failed)),
                InlineKeyboardMarkup::new(rows),
            )
        }
    }
}

async fn track_results(app: &App, session: &mut Session) -> (String, InlineKeyboardMarkup) {
    let fallback = refresh_results(app, session).await;
    let matched = match (&session.track, session.provider == app.youtube_provider) {
        (Some(track), true) => media::matching_track(&session.results, &track.artist, &track.title),
        _ => None,
    };
    match matched {
        Some(media) => {
            session.selected = Some(media.clone().into());
            preview_menu(session)
        }
        None => fallback,
    }
}

fn authorized(config: &Config, user: UserId) -> bool {
    config.user_access.allows(user.0)
}

fn query_text(text: &str) -> &str {
    let trimmed = text.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((command, query))
            if matches!(
                command.split('@').next(),
                Some("/search" | "/yt_search" | "/yt-search")
            ) =>
        {
            query.trim()
        }
        _ if matches!(trimmed, "/search" | "/yt_search" | "/yt-search") => "",
        _ => trimmed,
    }
}

fn parse_callback(data: &str) -> Option<(u64, u32, char, usize)> {
    let mut parts = data.split(':');
    let id = parts.next()?.parse().ok()?;
    let version = parts.next()?.parse().ok()?;
    let action = parts.next()?;
    let value = parts.next()?.parse().ok()?;
    if parts.next().is_some() || action.len() != 1 {
        return None;
    }
    Some((id, version, action.chars().next()?, value))
}

fn results_menu(session: &Session, provider: &Provider) -> (String, InlineKeyboardMarkup) {
    let mut rows: Vec<_> = session
        .results
        .iter()
        .enumerate()
        .map(|(index, media)| {
            let mut row = vec![session.button(
                format!(
                    "{} · {}",
                    index + 1,
                    media.title.chars().take(48).collect::<String>()
                ),
                'r',
                index,
            )];
            if let Ok(url) = url::Url::parse(&media.url) {
                row.push(InlineKeyboardButton::url("↗", url));
            }
            row
        })
        .collect();
    let mut navigation = Vec::new();
    if session.page > 0 {
        navigation.push(session.button(
            session.language.text(Text::Previous),
            's',
            session.page - 1,
        ));
    }
    if session.page < 9 {
        navigation.push(session.button(session.language.text(Text::Next), 's', session.page + 1));
    }
    if !navigation.is_empty() {
        rows.push(navigation);
    }
    rows.push(vec![session.button(
        session.language.text(Text::ChooseHost),
        'p',
        0,
    )]);
    rows.extend(session.track_controls());
    let heading = session.language.text(if session.results.is_empty() {
        Text::Empty
    } else {
        Text::Results
    });
    (
        session.context(&format!(
            "{heading}\n{} · {}",
            provider.name,
            session.page + 1
        )),
        InlineKeyboardMarkup::new(rows),
    )
}

fn providers_menu(
    session: &Session,
    providers: &[Provider],
    page: usize,
) -> (String, InlineKeyboardMarkup) {
    let page = page.min(providers.len().saturating_sub(1) / PROVIDERS_PER_PAGE);
    let mut rows: Vec<_> = providers
        .iter()
        .enumerate()
        .skip(page * PROVIDERS_PER_PAGE)
        .take(PROVIDERS_PER_PAGE)
        .map(|(index, provider)| vec![session.button(provider.name.clone(), 'h', index)])
        .collect();
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(session.button(session.language.text(Text::Previous), 'p', page - 1));
    }
    if (page + 1) * PROVIDERS_PER_PAGE < providers.len() {
        navigation.push(session.button(session.language.text(Text::Next), 'p', page + 1));
    }
    if !navigation.is_empty() {
        rows.push(navigation);
    }
    rows.push(vec![session.button(
        session.language.text(Text::Back),
        'b',
        0,
    )]);
    (
        session.context(session.language.text(Text::Providers)),
        InlineKeyboardMarkup::new(rows),
    )
}

fn format_menu(session: &Session) -> (String, InlineKeyboardMarkup) {
    let title = session
        .selected
        .as_ref()
        .map(|selected| selected.media.title.chars().take(240).collect::<String>());
    let text = match title {
        Some(title) => format!("{title}\n{}", session.language.text(Text::ChooseFormat)),
        None => session.language.text(Text::ChooseFormat).into(),
    };
    (
        session.context(&text),
        InlineKeyboardMarkup::new([
            vec![
                session.button(session.language.text(Text::Music), 'm', 0),
                session.button(session.language.text(Text::Audio), 'a', 0),
            ],
            vec![session.button(session.language.text(Text::Video), 'v', 0)],
            vec![session.button(session.language.text(Text::Back), 'b', 0)],
        ]),
    )
}

fn limits(config: &Config, language: Language) -> String {
    language
        .text(Text::Limits)
        .replace(
            "{limit}",
            &(config.max_upload_bytes / (1024 * 1024)).to_string(),
        )
        .replace("{download}", &config.download_timeout_secs.to_string())
        .replace("{send}", &SEND_TIMEOUT_SECS.to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_callback, query_text};

    #[test]
    fn commands_and_callbacks_are_unambiguous() {
        assert_eq!(query_text("/search@music_bot test song"), "test song");
        assert_eq!(query_text(" test song "), "test song");
        assert_eq!(query_text("/search"), "");
        assert_eq!(parse_callback("123:0:r:2"), Some((123, 0, 'r', 2)));
        assert!(parse_callback("123:0:r:2:extra").is_none());
        assert!(parse_callback("123:0:rr:2").is_none());
    }
}
