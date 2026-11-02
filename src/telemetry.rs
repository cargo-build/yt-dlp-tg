//! Correlated diagnostics with explicit source and error redaction. @req DL-8 DL-11

use std::{future::Future, time::Instant};

pub(crate) mod transfer;

use crate::{
    error::Error,
    video::{Availability, Choice},
};

/// Preserve the result and record safe completion or cancellation timing. @req DL-8 DL-11
pub(crate) async fn phase<T>(
    component: &'static str,
    work: impl Future<Output = crate::error::Result<T>>,
) -> crate::error::Result<T> {
    let mut report = Phase {
        component,
        started: Instant::now(),
        outcome: "cancelled",
        reason: None,
    };
    tracing::info!(
        stage = "transfer_phase",
        component,
        outcome = "started",
        "Media component"
    );
    let result = work.await;
    report.outcome = if result.is_ok() { "ok" } else { "failed" };
    report.reason = result.as_ref().err().map(reason);
    result
}

struct Phase {
    component: &'static str,
    started: Instant,
    outcome: &'static str,
    reason: Option<&'static str>,
}

impl Drop for Phase {
    fn drop(&mut self) {
        tracing::info!(
            stage = "transfer_phase",
            component = self.component,
            outcome = self.outcome,
            reason = self.reason,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            "Media component"
        );
    }
}

/// Preserve known video identities; unknown paths and share queries stay private. @req DL-8 DL-11
pub fn source(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let segments: Vec<_> = url
        .path()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let identity = match url.host_str()? {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            match segments.as_slice() {
                ["watch"] => url.query_pairs().find_map(|(key, value)| {
                    (key == "v" && value.len() == 11 && identifier(&value))
                        .then(|| format!("https://www.youtube.com/watch?v={value}"))
                }),
                ["shorts" | "embed" | "live", id] if id.len() == 11 && identifier(id) => {
                    Some(format!("https://www.youtube.com/watch?v={id}"))
                }
                _ => None,
            }
        }
        "youtu.be" => match segments.as_slice() {
            [id] if id.len() == 11 && identifier(id) => {
                Some(format!("https://www.youtube.com/watch?v={id}"))
            }
            _ => None,
        },
        "instagram.com" | "www.instagram.com" | "m.instagram.com" => match segments.as_slice() {
            [kind @ ("reel" | "reels" | "p" | "tv"), id] if identifier(id) => {
                Some(format!("https://www.instagram.com/{kind}/{id}/"))
            }
            _ => None,
        },
        _ => None,
    };
    match identity {
        Some(identity) => Some(identity),
        None => {
            url.set_username("").ok()?;
            url.set_password(None).ok()?;
            url.set_path("/");
            url.set_query(None);
            url.set_fragment(None);
            Some(url.into())
        }
    }
}

fn identifier(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Categorize failures without formatting their credential-bearing payloads. @req DL-8 DL-11
pub fn reason(error: &Error) -> &'static str {
    match error {
        Error::Config(_) => "configuration",
        Error::Telegram(_) => "telegram",
        Error::YtDlp(_) => "extractor",
        Error::Json(_) => "invalid_metadata",
        Error::Timeout => "timeout",
        Error::TooLarge => "upload_limit",
        Error::AacUnavailable => "native_aac_unavailable",
        Error::Cache(_) => "cache",
        Error::Queue(_) => "queue",
        Error::Process(_) | Error::ChildExit(_) => "subprocess",
        Error::Image => "image",
        Error::ImageEmpty => "image_empty",
        Error::ImageTooLarge => "image_limit",
        Error::Io(_) => "io",
    }
}

/// Include unavailable rows after successful extraction, without dumping metadata. @req DL-11
pub fn video_options(request_id: u64, url: &str, choices: &[Choice]) {
    let source = source(url);
    choices.iter().for_each(|choice| {
        let (availability, bytes) = match &choice.availability {
            Availability::Available(selection) => ("available", Some(selection.size.bytes())),
            Availability::TooLarge(size) => ("too_large", Some(size.bytes())),
            Availability::Unknown => ("unknown_size", None),
            Availability::Missing => ("missing", None),
        };
        tracing::info!(request_id, stage = "video_options", height = choice.height,
            availability, bytes, source = ?source, "Media request");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_diagnostics_keep_media_identity_without_private_url_components() {
        [
            (
                "https://www.instagram.com/reel/Fixture_123/?tracking=PRIVATE#PRIVATE",
                "https://www.instagram.com/reel/Fixture_123/",
            ),
            (
                "https://youtu.be/dQw4w9WgXcQ?tracking=PRIVATE",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            ),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&token=PRIVATE",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            ),
            (
                "https://user:PRIVATE@media.invalid/PRIVATE/file?key=PRIVATE#PRIVATE",
                "https://media.invalid/",
            ),
            (
                "https://api.telegram.org/bot123:PRIVATE/method",
                "https://api.telegram.org/",
            ),
            (
                "https://www.instagram.com/PRIVATE/profile",
                "https://www.instagram.com/",
            ),
            (
                "https://www.instagram.com/reel/has%3Asecret/",
                "https://www.instagram.com/",
            ),
        ]
        .into_iter()
        .for_each(|(input, expected)| assert_eq!(source(input).as_deref(), Some(expected)));
        assert!(source("file:///PRIVATE").is_none());
        assert!(source("not a URL").is_none());
        assert_eq!(reason(&Error::YtDlp("PRIVATE".into())), "extractor");
    }
}
