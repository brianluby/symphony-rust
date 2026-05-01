//! Structured logging setup using `tracing`.
//! Maps to SPEC.md §13 (Observability).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

#[derive(Debug, Clone, Copy)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub enum LogVerbosity {
    Normal,
    Verbose,
}

#[derive(Debug, Clone, Copy)]
pub struct LoggingConfig {
    pub format: LogFormat,
    pub verbosity: LogVerbosity,
}

/// Initialize structured JSON logging.
pub fn init_logging(config: LoggingConfig) {
    let env_filter = if matches!(config.verbosity, LogVerbosity::Verbose) {
        EnvFilter::new("symphony=trace,info")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("symphony=info,warn"))
    };

    match config.format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_target(true)
                .with_thread_ids(true)
                .with_span_events(FmtSpan::CLOSE)
                .init();
        }
        LogFormat::Text => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
        }
    }
}
