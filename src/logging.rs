use anyhow::{Result, bail};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init(log_level: &str, log_format: &str) -> Result<()> {
    let filter = EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match log_format {
        "json" => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .init();
        }
        "pretty" => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().pretty())
                .init();
        }
        other => bail!("unsupported log format: '{}' (expected 'json' or 'pretty')", other),
    }

    Ok(())
}
