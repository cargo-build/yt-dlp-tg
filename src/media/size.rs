//! Header-only size discovery for direct streams omitted by extractor metadata. @req DL-5 DL-8

use std::time::Duration;

use serde_json::Value;

use crate::{
    config::Config,
    error::{Error, Result},
    media::http_url,
    video,
};

const MAX_PROBES: usize = 16;
const MAX_SECONDS: u64 = 20;

/// Missing headers remain unknown; never request a media body to build the menu. @req DL-5
pub(super) async fn supplement(config: &Config, info: &mut Value) -> Result<()> {
    let positions = video::missing_sizes(info)?;
    if positions.is_empty() {
        return Ok(());
    }
    let mut builder = cyper::Client::builder()
        .redirect(cyper::redirect::Policy::none())
        .referer(false);
    if let Some(proxy) = &config.ytdlp_proxy {
        builder = builder.proxy(
            cyper::proxy::Proxy::all(proxy)
                .map_err(|_| Error::Config("invalid YTDLP_PROXY for size inspection".into()))?,
        );
    }
    let client = builder
        .build()
        .map_err(|_| Error::YtDlp("cannot construct media size client".into()))?;
    let inspect = async {
        for position in positions.into_iter().take(MAX_PROBES) {
            let format = match info.get("formats").and_then(Value::as_array) {
                Some(formats) => &formats[position],
                None => &*info,
            };
            match size(&client, format, info.get("http_headers")).await {
                Ok(Some(length)) => match info.get_mut("formats").and_then(Value::as_array_mut) {
                    Some(formats) => formats[position]["filesize"] = length.into(),
                    None => info["filesize"] = length.into(),
                },
                Ok(None) => {}
                Err(_) => tracing::warn!("Media size header request failed"),
            }
        }
    };
    let deadline = Duration::from_secs(config.process_timeout_secs.min(MAX_SECONDS));
    if compio::time::timeout(deadline, inspect).await.is_err() {
        tracing::warn!("Media size inspection timed out; unresolved sizes remain unavailable");
    }
    Ok(())
}

async fn size(
    client: &cyper::Client,
    format: &Value,
    root_headers: Option<&Value>,
) -> Result<Option<u64>> {
    let Some(url) = direct_url(format) else {
        return Ok(None);
    };
    let invalid = || Error::YtDlp("invalid media size request".into());
    let mut request = client.head(url).map_err(|_| invalid())?;
    if let Some(headers) = format.get("http_headers").or(root_headers) {
        let headers = headers.as_object().ok_or_else(invalid)?;
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "range" | "if-range" => return Ok(None),
                "host" | "content-length" | "transfer-encoding" | "accept-encoding" => continue,
                _ => {}
            }
            request = request
                .header(name.as_str(), value.as_str().ok_or_else(invalid)?)
                .map_err(|_| invalid())?;
        }
    }
    let response = request
        .header("accept-encoding", "identity")
        .map_err(|_| invalid())?
        .send()
        .await
        .map_err(|_| Error::YtDlp("media size header request failed".into()))?;
    let headers = response.headers();
    let media_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase);
    let encoded = headers.get("content-encoding").is_some_and(|value| {
        value
            .to_str()
            .map_or(true, |value| !value.eq_ignore_ascii_case("identity"))
    });
    match response.status().as_u16() == 200
        && !encoded
        && !headers.contains_key("content-range")
        && media_type.is_some_and(|value| {
            value.starts_with("video/")
                || value.starts_with("audio/")
                || value.split(';').next() == Some("application/octet-stream")
        })
    {
        false => Ok(None),
        true => Ok(headers
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|length| *length > 0)),
    }
}

fn direct_url(format: &Value) -> Option<String> {
    let protocol = format.get("protocol").and_then(Value::as_str)?;
    if !matches!(protocol, "http" | "https")
        || format.get("fragments").is_some_and(|value| !value.is_null())
    {
        return None;
    }
    let url = http_url(format.get("url").and_then(Value::as_str)?)?;
    let parsed = url::Url::parse(&url).ok()?;
    match parsed.query_pairs().any(|(key, _)| {
        matches!(key.to_ascii_lowercase().as_str(), "range" | "bytestart" | "byteend")
    }) {
        true => None,
        false => Some(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragmented_or_ranged_media_cannot_supply_a_whole_file_size() {
        let direct = json!({"protocol":"https", "url":"https://media.invalid/file.mp4"});
        assert!(direct_url(&direct).is_some());
        [
            json!({"protocol":"m3u8_native", "url":"https://media.invalid/stream"}),
            json!({"protocol":"https", "url":"https://media.invalid/file?ByteEnd=1024"}),
            json!({"protocol":"https", "url":"https://media.invalid/file", "fragments":[{}]}),
            json!({"protocol":"https", "url":"file:///tmp/media"}),
        ]
        .iter()
        .for_each(|format| assert!(direct_url(format).is_none()));
    }
}
