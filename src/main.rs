//! Personal Telegram downloader on one compio/io_uring runtime. @req DL-7

mod bot;
mod broker;
mod cache;
mod config;
mod delivery;
mod error;
mod i18n;
mod jobs;
mod media;
mod ocr;
mod process;
mod queue;
mod telemetry;
mod video;

use crate::{config::Config, error::Result};

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| error::Error::Config("cannot select TLS crypto provider".into()))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "yt_dlp_tg=info".into()),
        )
        .init();
    let runtime = compio::runtime::Runtime::new()?;
    if !runtime.driver_type().is_iouring() {
        return Err(std::io::Error::other("io_uring is required").into());
    }
    runtime.block_on(async {
        match std::env::args().nth(1).as_deref() {
            Some("iggy-provision" | "iggy-probe") => {
                let config = config::IggyConfig::from_env()?
                    .ok_or_else(|| error::Error::Config("Iggy settings are required".into()))?;
                if std::env::args().nth(1).as_deref() == Some("iggy-provision") {
                    let path = std::env::var_os("IGGY_APP_CONNECTION_STRING_FILE")
                        .map(std::path::PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| {
                            error::Error::Config(
                                "absolute IGGY_APP_CONNECTION_STRING_FILE is required".into(),
                            )
                        })?;
                    broker::provision(&config, path).await
                } else {
                    let broker = broker::Broker::connect(&config).await?;
                    broker.shutdown().await?;
                    println!("Native io_uring Iggy connection and persisted queue verified");
                    Ok(())
                }
            }
            Some(_) => Err(error::Error::Config("unknown command".into())),
            None => bot::run(Config::from_env().await?).await,
        }
    })
}
