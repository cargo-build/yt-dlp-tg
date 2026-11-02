//! Environment-driven configuration. Every required value must be present;
//! optional values get explicit documented defaults.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// How updates are received from Telegram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Long polling via `getUpdates` (default; works behind NAT).
    Polling,
    /// Webhook served by the built-in HTTP server (reverse proxy with TLS in front).
    Webhook,
}

/// Explicit public access or a private allowlist; absence never grants access. @req DL-1
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAccess {
    All,
    Allowlist(Vec<i64>),
}

impl UserAccess {
    /// Keep invalid identities rejected even when the bot is public. @req DL-1
    pub fn allows(&self, user: u64) -> bool {
        match i64::try_from(user) {
            Ok(id) if id > 0 => match self {
                Self::All => true,
                Self::Allowlist(ids) => ids.contains(&id),
            },
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub bot_token: String,
    /// Explicitly selected public or restricted Telegram access.
    pub user_access: UserAccess,
    pub mode: Mode,
    /// Bot API base URL; point at a local Bot API server to lift the 50 MiB limit.
    pub tg_api_base: String,
    /// Webhook server bind address (MODE=webhook).
    pub listen_addr: String,
    /// Public HTTPS root the webhook URL is built from, e.g. https://bot.example.com
    pub webhook_public_url: Option<String>,
    /// Path segment of the webhook route.
    pub webhook_path: String,
    /// Per-job download scratch directory.
    pub download_dir: PathBuf,
    pub cache_path: PathBuf,
    pub ytdlp_path: String,
    /// Optional `--cookies <file>` passthrough.
    pub ytdlp_cookies: Option<String>,
    /// Optional `--proxy <url>` passthrough.
    pub ytdlp_proxy: Option<String>,
    pub tg_proxy: Option<String>,
    pub process_timeout_secs: u64,
    pub download_timeout_secs: u64,
    /// Upload guard for the Bot API instance in use.
    pub max_upload_bytes: u64,
    pub ocr: Option<OcrConfig>,
    pub iggy: Option<IggyConfig>,
}

#[derive(Clone)]
pub enum IggyConnection {
    Value(String),
    File(PathBuf),
}

#[derive(Clone)]
pub struct IggyConfig {
    pub connection: IggyConnection,
    pub stream: String,
    pub topic: String,
    pub max_pending: usize,
}

impl std::fmt::Debug for IggyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IggyConfig")
            .field("stream", &self.stream)
            .field("topic", &self.topic)
            .field("max_pending", &self.max_pending)
            .finish_non_exhaustive()
    }
}

impl IggyConfig {
    pub fn from_env() -> Result<Option<Self>> {
        iggy_config(&|name| std::env::var(name).ok())
    }

    /// Resolve private credentials only at the native connection boundary. @req DL-8 DL-14
    pub async fn connection_string(&self) -> Result<String> {
        let connection = match &self.connection {
            IggyConnection::Value(value) => value.trim().to_owned(),
            IggyConnection::File(path) => {
                let metadata = compio::fs::metadata(path)
                    .await
                    .map_err(|_| Error::Config("cannot inspect Iggy credential file".into()))?;
                if !metadata.is_file() || metadata.len() > 4096 {
                    return Err(Error::Config("invalid Iggy credential file".into()));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(Error::Config(
                            "Iggy credential file must be owner-only".into(),
                        ));
                    }
                }
                let bytes = compio::fs::read(path)
                    .await
                    .map_err(|_| Error::Config("cannot read Iggy credential file".into()))?;
                String::from_utf8(bytes)
                    .map_err(|_| Error::Config("Iggy credential file must contain UTF-8".into()))?
                    .trim()
                    .to_owned()
            }
        };
        let url = url::Url::parse(&connection)
            .map_err(|_| Error::Config("invalid Iggy connection setting".into()))?;
        let loopback = match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Domain(host)) => host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|host| host.is_loopback()),
            Some(url::Host::Ipv4(host)) => host.is_loopback(),
            Some(url::Host::Ipv6(host)) => host.is_loopback(),
            None => false,
        };
        if connection.len() > 4096
            || url.scheme() != "iggy"
            || !loopback
            || url.username().is_empty()
            || url.password().is_none_or(str::is_empty)
            || url.fragment().is_some()
        {
            return Err(Error::Config(
                "Iggy requires authenticated loopback plain TCP".into(),
            ));
        }
        Ok(connection)
    }
}

#[derive(Clone, Debug)]
pub struct OcrConfig {
    pub python: String,
    pub script: PathBuf,
    pub profile: Option<PathBuf>,
    pub secrets_file: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

const DEFAULT_API_BASE: &str = "https://api.telegram.org";
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";
const DEFAULT_WEBHOOK_PATH: &str = "tg-webhook";
const DEFAULT_DOWNLOAD_DIR: &str = "/tmp/yt-dlp-tg";
const DEFAULT_YTDLP: &str = "yt-dlp";
const DEFAULT_MAX_UPLOAD_MB: u64 = 50;
/// Hard cap of the Bot API (`MAX_UPLOAD_MB` cannot exceed this).
const HARD_MAX_UPLOAD_MB: u64 = 2000;

impl Config {
    pub async fn from_env() -> Result<Self> {
        let token = match non_empty(std::env::var("BOT_TOKEN").ok()) {
            Some(token) => token,
            None => {
                let path = non_empty(std::env::var("BOT_TOKEN_FILE").ok())
                    .unwrap_or_else(|| "/run/secrets/GET_MUSIC_VIDEO_BOT".into());
                let bytes = compio::fs::read(path)
                    .await
                    .map_err(|_| Error::Config("cannot read BOT_TOKEN_FILE".into()))?;
                String::from_utf8(bytes)
                    .map_err(|_| Error::Config("BOT_TOKEN_FILE must contain UTF-8".into()))?
            }
        };
        Self::from_map(|name| match name {
            "BOT_TOKEN" => Some(token.clone()),
            _ => std::env::var(name).ok(),
        })
    }

    /// `from_map` keeps parsing unit-testable without touching the process env.
    pub fn from_map(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let bot_token = non_empty(get("BOT_TOKEN"))
            .ok_or_else(|| Error::Config("BOT_TOKEN is required".into()))?;

        let allow_raw = non_empty(get("ALLOWED_USER_IDS")).ok_or_else(|| {
            Error::Config("ALLOWED_USER_IDS is required (comma-separated Telegram user IDs or *)".into())
        })?;
        let user_access = match allow_raw.trim() {
            "*" => UserAccess::All,
            raw => UserAccess::Allowlist(parse_user_ids(raw)?),
        };

        let mode = match non_empty(get("MODE")).as_deref() {
            None | Some("polling") => Mode::Polling,
            Some("webhook") => Mode::Webhook,
            Some(other) => {
                return Err(Error::Config(format!(
                    "MODE must be 'polling' or 'webhook', got {other:?}"
                )));
            }
        };

        let webhook_public_url = non_empty(get("WEBHOOK_PUBLIC_URL"));
        if mode == Mode::Webhook && webhook_public_url.is_none() {
            return Err(Error::Config(
                "WEBHOOK_PUBLIC_URL is required when MODE=webhook".into(),
            ));
        }

        let max_upload_mb = match non_empty(get("MAX_UPLOAD_MB")) {
            Some(v) => {
                let mb = v.parse::<u64>().map_err(|_| {
                    Error::Config(format!("MAX_UPLOAD_MB must be an integer, got {v:?}"))
                })?;
                if !(1..=HARD_MAX_UPLOAD_MB).contains(&mb) {
                    return Err(Error::Config(format!(
                        "MAX_UPLOAD_MB must be 1..={HARD_MAX_UPLOAD_MB}, got {mb}"
                    )));
                }
                mb
            }
            None => DEFAULT_MAX_UPLOAD_MB,
        };

        Ok(Self {
            bot_token: bot_token.trim().to_owned(),
            user_access,
            mode,
            tg_api_base: non_empty(get("TG_API_BASE")).unwrap_or_else(|| DEFAULT_API_BASE.into()),
            listen_addr: non_empty(get("LISTEN_ADDR"))
                .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.into()),
            webhook_public_url,
            webhook_path: non_empty(get("WEBHOOK_PATH"))
                .unwrap_or_else(|| DEFAULT_WEBHOOK_PATH.into()),
            download_dir: non_empty(get("DOWNLOAD_DIR"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_DOWNLOAD_DIR)),
            cache_path: cache_path(&get)?,
            ytdlp_path: non_empty(get("YTDLP_PATH")).unwrap_or_else(|| DEFAULT_YTDLP.into()),
            ytdlp_cookies: non_empty(get("YTDLP_COOKIES")),
            ytdlp_proxy: non_empty(get("YTDLP_PROXY")),
            tg_proxy: non_empty(get("TG_PROXY")),
            process_timeout_secs: seconds(get("PROCESS_TIMEOUT_SECS"), 180)?,
            download_timeout_secs: seconds(get("DOWNLOAD_TIMEOUT_SECS"), 900)?,
            max_upload_bytes: max_upload_mb * 1024 * 1024,
            ocr: ocr_config(&get)?,
            iggy: iggy_config(&get)?,
        })
        .and_then(Self::validate)
    }

    fn validate(self) -> Result<Self> {
        let api = url::Url::parse(&self.tg_api_base)
            .map_err(|_| Error::Config("TG_API_BASE must be an HTTP(S) URL".into()))?;
        if !matches!(api.scheme(), "http" | "https") || api.host_str().is_none() {
            return Err(Error::Config("TG_API_BASE must be an HTTP(S) URL".into()));
        }
        if api.host_str() == Some("api.telegram.org")
            && self.max_upload_bytes > DEFAULT_MAX_UPLOAD_MB * 1024 * 1024
        {
            return Err(Error::Config(
                "MAX_UPLOAD_MB cannot exceed 50 on the cloud Bot API".into(),
            ));
        }
        if self.mode == Mode::Webhook {
            let root = self
                .webhook_public_url
                .as_deref()
                .ok_or_else(|| Error::Config("WEBHOOK_PUBLIC_URL is required".into()))?;
            let url = url::Url::parse(root)
                .map_err(|_| Error::Config("invalid WEBHOOK_PUBLIC_URL".into()))?;
            if url.scheme() != "https" || url.host_str().is_none() {
                return Err(Error::Config("WEBHOOK_PUBLIC_URL must use HTTPS".into()));
            }
            self.listen_addr
                .parse::<std::net::SocketAddr>()
                .map_err(|_| Error::Config("invalid LISTEN_ADDR".into()))?;
        }
        if self.webhook_path.trim_matches('/').is_empty() {
            return Err(Error::Config("WEBHOOK_PATH cannot be empty".into()));
        }
        Ok(self)
    }
}

fn ocr_config(get: &impl Fn(&str) -> Option<String>) -> Result<Option<OcrConfig>> {
    let profile = non_empty(get("OCR_CONFIG")).map(PathBuf::from);
    let secrets_file = non_empty(get("OCR_SECRETS_FILE")).map(PathBuf::from);
    if secrets_file.is_some() && profile.is_none() {
        return Err(Error::Config("OCR_SECRETS_FILE requires OCR_CONFIG".into()));
    }
    match (non_empty(get("OCR_PYTHON")), non_empty(get("OCR_SCRIPT"))) {
        (None, None) if profile.is_none() => Ok(None),
        (Some(python), Some(script)) => {
            let timeout_secs = seconds(get("OCR_TIMEOUT_SECS"), 60)?;
            match timeout_secs <= 300 {
                true => Ok(Some(OcrConfig {
                    python,
                    script: script.into(),
                    profile,
                    secrets_file,
                    timeout_secs,
                })),
                false => Err(Error::Config("OCR_TIMEOUT_SECS must be 1..=300".into())),
            }
        }
        _ => Err(Error::Config(
            "OCR_PYTHON and OCR_SCRIPT must be configured together".into(),
        )),
    }
}

fn iggy_config(get: &impl Fn(&str) -> Option<String>) -> Result<Option<IggyConfig>> {
    let value = get("IGGY_CONNECTION_STRING");
    let file = get("IGGY_CONNECTION_STRING_FILE");
    let stream = get("IGGY_STREAM");
    let topic = get("IGGY_TOPIC");
    let pending = get("IGGY_MAX_PENDING");
    let configured = [&value, &file, &stream, &topic, &pending]
        .into_iter()
        .any(|value| value.is_some());
    if value.is_some() && file.is_some() {
        return Err(Error::Config(
            "Iggy credential sources are mutually exclusive".into(),
        ));
    }
    let connection =
        match (non_empty(value), non_empty(file)) {
            (None, None) if !configured => return Ok(None),
            (Some(value), None) => IggyConnection::Value(value),
            (None, Some(file)) if PathBuf::from(&file).is_absolute() => {
                IggyConnection::File(file.into())
            }
            _ => return Err(Error::Config(
                "configure exactly one nonempty Iggy connection string or absolute private file"
                    .into(),
            )),
        };
    let max_pending = match pending {
        Some(value) => value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=crate::jobs::MAX_PENDING).contains(count))
            .ok_or_else(|| Error::Config("IGGY_MAX_PENDING must be 1..=256".into()))?,
        None => crate::jobs::MAX_PENDING,
    };
    let stream = iggy_name(stream, "yt_dlp_tg")?;
    let topic = iggy_name(topic, "downloads")?;
    if [&stream, &topic].into_iter().any(|name| {
        name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }) {
        return Err(Error::Config("invalid Iggy stream or topic name".into()));
    }
    Ok(Some(IggyConfig {
        connection,
        stream,
        topic,
        max_pending,
    }))
}

fn iggy_name(value: Option<String>, default: &str) -> Result<String> {
    match value {
        None => Ok(default.into()),
        Some(value) if !value.trim().is_empty() => Ok(value),
        Some(_) => Err(Error::Config("Iggy resource names cannot be empty".into())),
    }
}

fn cache_path(get: &impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
    match non_empty(get("CACHE_PATH")) {
        Some(path) => Ok(PathBuf::from(path)),
        None => {
            let root = non_empty(get("XDG_STATE_HOME"))
                .map(PathBuf::from)
                .or_else(|| {
                    non_empty(get("HOME")).map(|home| PathBuf::from(home).join(".local/state"))
                })
                .ok_or_else(|| {
                    Error::Config(
                        "CACHE_PATH requires XDG_STATE_HOME or HOME for its default".into(),
                    )
                })?;
            Ok(root.join("yt-dlp-tg/files.redb"))
        }
    }
}

fn seconds(value: Option<String>, default: u64) -> Result<u64> {
    match non_empty(value) {
        None => Ok(default),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Config("process timeouts must be positive seconds".into())),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn parse_user_ids(raw: &str) -> Result<Vec<i64>> {
    raw.split(',')
        .map(|part| {
            part.trim()
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| {
                    Error::Config(format!("invalid user ID {part:?} in ALLOWED_USER_IDS"))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| match name {
            "HOME" => Some("/tmp/yt-dlp-tg-tests".into()),
            _ => owned.get(name).cloned(),
        }
    }

    fn valid() -> Vec<(&'static str, &'static str)> {
        vec![
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42, 7"),
            ("MODE", "webhook"),
            ("WEBHOOK_PUBLIC_URL", "https://bot.example.com"),
        ]
    }

    #[test]
    fn iggy_is_explicit_and_namespace_settings_never_silently_disable_it() {
        assert!(iggy_config(&map(&[])).expect("standalone").is_none());
        for pairs in [
            vec![("IGGY_TOPIC", "downloads")],
            vec![("IGGY_CONNECTION_STRING", "")],
            vec![("IGGY_CONNECTION_STRING_FILE", "relative")],
            vec![
                ("IGGY_CONNECTION_STRING", "iggy://a:b@127.0.0.1:8090"),
                ("IGGY_MAX_PENDING", "257"),
            ],
            vec![
                ("IGGY_CONNECTION_STRING", "iggy://a:b@127.0.0.1:8090"),
                ("IGGY_TOPIC", ""),
            ],
            vec![
                ("IGGY_CONNECTION_STRING", "iggy://a:b@127.0.0.1:8090"),
                ("IGGY_CONNECTION_STRING_FILE", "/private/key"),
            ],
        ] {
            assert!(iggy_config(&map(&pairs)).is_err());
        }
        let config = iggy_config(&map(&[(
            "IGGY_CONNECTION_STRING",
            "iggy://a:secret@127.0.0.1:8090",
        )]))
        .expect("valid")
        .expect("configured");
        assert_eq!(config.max_pending, 256);
        assert!(!format!("{config:?}").contains("secret"));
    }

    #[compio::test]
    async fn iggy_requires_private_credentials_and_authenticated_loopback() {
        let config = |value: &str| IggyConfig {
            connection: IggyConnection::Value(value.into()),
            stream: "test".into(),
            topic: "test".into(),
            max_pending: 1,
        };
        for value in ["iggy://a:b@127.0.0.1:8090", "iggy://a:b@[::1]:8090"] {
            assert!(config(value).connection_string().await.is_ok());
        }
        for value in [
            "iggy://a:b@example.com:8090",
            "iggy://a@127.0.0.1:8090",
            "http://a:b@127.0.0.1:8090",
            "iggy://a:b@127.0.0.1:8090#secret",
        ] {
            assert!(config(value).connection_string().await.is_err());
        }
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("connection");
        std::fs::write(&path, "iggy://a:private@127.0.0.1:8090\n").expect("credential");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
        let private = IggyConfig {
            connection: IggyConnection::File(path.clone()),
            ..config("unused")
        };
        assert!(private.connection_string().await.is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("public");
        assert!(private.connection_string().await.is_err());
    }

    #[test]
    fn missing_token_is_rejected() {
        let err = Config::from_map(map(&[("ALLOWED_USER_IDS", "42")])).unwrap_err();
        assert!(err.to_string().contains("BOT_TOKEN"));
    }

    #[test]
    fn cloud_upload_limit_cannot_be_raised_by_configuration() {
        let mut pairs = valid();
        pairs.push(("MAX_UPLOAD_MB", "51"));
        assert!(Config::from_map(map(&pairs)).is_err());
        pairs.push(("TG_API_BASE", "http://127.0.0.1:8081"));
        assert!(Config::from_map(map(&pairs)).is_ok());
    }

    #[test]
    fn ocr_requires_both_explicit_paths_and_a_bounded_deadline() {
        let base = [("BOT_TOKEN", "123:abc"), ("ALLOWED_USER_IDS", "42")];
        let mut pairs = base.to_vec();
        pairs.push(("OCR_PYTHON", "python3"));
        assert!(Config::from_map(map(&pairs)).is_err());
        pairs.push(("OCR_SCRIPT", "/app/ocr.py"));
        let settings = Config::from_map(map(&pairs))
            .expect("paired OCR settings")
            .ocr
            .expect("OCR enabled");
        assert_eq!(settings.timeout_secs, 60);
        assert!(settings.profile.is_none());
        let mut configured = pairs.clone();
        configured.push(("OCR_CONFIG", "/app/policy.json"));
        configured.push(("OCR_SECRETS_FILE", "/private/credentials.json"));
        assert!(Config::from_map(map(&configured)).is_ok());
        assert!(
            Config::from_map(map(&[
                ("BOT_TOKEN", "test"),
                ("ALLOWED_USER_IDS", "42"),
                ("OCR_CONFIG", "/app/policy.json")
            ]))
            .is_err()
        );
        assert!(
            Config::from_map(map(&[
                ("BOT_TOKEN", "test"),
                ("ALLOWED_USER_IDS", "42"),
                ("OCR_SECRETS_FILE", "/private/credentials.json")
            ]))
            .is_err()
        );
        pairs.push(("OCR_TIMEOUT_SECS", "301"));
        assert!(Config::from_map(map(&pairs)).is_err());
        assert!(
            Config::from_map(map(&base))
                .expect("OCR is optional")
                .ocr
                .is_none()
        );
    }

    #[test]
    fn missing_allowlist_is_rejected() {
        let err = Config::from_map(map(&[("BOT_TOKEN", "123:abc")])).unwrap_err();
        assert!(err.to_string().contains("ALLOWED_USER_IDS"));
    }

    #[test]
    fn invalid_user_id_is_rejected() {
        let err = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42,bob"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("user ID"));
    }

    #[test]
    fn public_access_requires_an_explicit_standalone_wildcard() {
        let cfg = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", " * "),
        ]))
        .expect("explicit public access");
        assert_eq!(cfg.user_access, UserAccess::All);
        [1, 42, 99, i64::MAX as u64]
            .into_iter()
            .for_each(|id| assert!(cfg.user_access.allows(id)));
        [0, u64::MAX]
            .into_iter()
            .for_each(|id| assert!(!cfg.user_access.allows(id)));
        ["", " ", "*,42", "42,*", "all", "0", "-1"]
            .into_iter()
            .for_each(|setting| {
                assert!(
                    Config::from_map(map(&[
                        ("BOT_TOKEN", "123:abc"),
                        ("ALLOWED_USER_IDS", setting),
                    ]))
                    .is_err(),
                    "rejected setting {setting:?}"
                );
            });
    }

    #[test]
    fn webhook_mode_requires_public_url() {
        let err = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42"),
            ("MODE", "webhook"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("WEBHOOK_PUBLIC_URL"));
    }

    #[test]
    fn defaults_are_applied_in_polling_mode() {
        let cfg = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42,7"),
        ]))
        .unwrap();
        assert_eq!(cfg.mode, Mode::Polling);
        assert_eq!(cfg.user_access, UserAccess::Allowlist(vec![42, 7]));
        assert!(cfg.user_access.allows(42));
        assert!(cfg.user_access.allows(7));
        assert!(!cfg.user_access.allows(99));
        assert_eq!(cfg.tg_api_base, "https://api.telegram.org");
        assert_eq!(cfg.listen_addr, "127.0.0.1:8080");
        assert_eq!(cfg.webhook_path, "tg-webhook");
        assert_eq!(cfg.download_dir, PathBuf::from("/tmp/yt-dlp-tg"));
        assert_eq!(cfg.ytdlp_path, "yt-dlp");
        assert_eq!(cfg.max_upload_bytes, 50 * 1024 * 1024);
        assert_eq!(cfg.ytdlp_cookies, None);
    }

    #[test]
    fn max_upload_mb_is_validated() {
        let err = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42"),
            ("MAX_UPLOAD_MB", "9000"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("MAX_UPLOAD_MB"));
    }

    #[test]
    fn unknown_mode_is_rejected() {
        let err = Config::from_map(map(&[
            ("BOT_TOKEN", "123:abc"),
            ("ALLOWED_USER_IDS", "42"),
            ("MODE", "grpc"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("MODE"));
    }

    #[test]
    fn webhook_configuration_and_debug_redaction() {
        let cfg = Config::from_map(map(&valid())).expect("valid webhook settings");
        assert_eq!(cfg.mode, Mode::Webhook);
        assert!(!format!("{cfg:?}").contains("123:abc"));
    }
}
