//! Logging set-up: `tracing` with a text or JSON formatter at the configured level.

use rusty_core::config::{LogFormat, LogLevel};
use rusty_core::{Error, Result};
use tracing::level_filters::LevelFilter;
use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;

fn filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// A subscriber writing to `writer` (tests capture the output with it).
pub fn subscriber<W>(level: LogLevel, format: LogFormat, writer: W) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => Box::new(
            tracing_subscriber::fmt()
                .with_max_level(filter(level))
                .with_writer(writer)
                .finish(),
        ),
        LogFormat::Json => Box::new(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(filter(level))
                .with_writer(writer)
                .finish(),
        ),
    }
}

/// Install the process-wide subscriber, writing to stderr. Errors if one is already installed.
pub fn init_logging(level: LogLevel, format: LogFormat) -> Result<()> {
    tracing::subscriber::set_global_default(subscriber(level, format, std::io::stderr))
        .map_err(|_| Error::Invalid("logging is already initialised".into()))
}
