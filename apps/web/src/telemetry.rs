//! Tracing (structured logging) setup.

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::LogFormat;

const DEFAULT_FILTER: &str = "info,library_web=debug,tower_http=info";

/// Install the global subscriber. `RUST_LOG` overrides the default filter.
pub fn init(format: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Pretty => registry.with(fmt::layer().pretty()).init(),
        LogFormat::Json => registry.with(fmt::layer().json()).init(),
    }
}
