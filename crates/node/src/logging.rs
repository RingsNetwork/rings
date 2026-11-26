//! Logging configuration contains both `node` and `browser`.
use std::fmt;
use std::panic::Location;
use std::panic::PanicHookInfo;

use backtrace::Backtrace;
use clap::ValueEnum;
use tracing::Level;
use tracing_log::LogTracer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

#[cfg(all(feature = "browser", target_family = "wasm"))]
pub use self::browser::init_logging;
#[cfg(feature = "node")]
pub use self::node::rings_node_init_logging as init_logging;
use crate::prelude::wasm_export;

/// Logging verbosity accepted by native CLI and browser bindings.
#[repr(C)]
#[wasm_export]
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum LogLevel {
    /// Debug-level diagnostic output.
    Debug,
    /// Informational runtime output.
    Info,
    /// Warning-level runtime output.
    Warn,
    /// Error-level runtime output.
    #[default]
    Error,
    /// Trace-level diagnostic output.
    Trace,
}

impl From<LogLevel> for Level {
    fn from(val: LogLevel) -> Self {
        match val {
            LogLevel::Trace => Level::TRACE,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "TRACE" => Ok(LogLevel::Trace),
            "DEBUG" => Ok(LogLevel::Debug),
            "INFO" => Ok(LogLevel::Info),
            "WARN" => Ok(LogLevel::Warn),
            "ERROR" => Ok(LogLevel::Error),
            x => Err(crate::error::Error::InvalidLoggingLevel(x.to_string())),
        }
    }
}

/// Panic location
#[derive(Debug, Clone)]
pub struct PanicLocation {
    /// Source file path where the panic occurred.
    file: String,
    /// Source line number where the panic occurred.
    line: String,
    /// Source column number where the panic occurred.
    column: String,
}

impl<'a, T> From<T> for PanicLocation
where T: Into<Location<'a>>
{
    fn from(lo: T) -> Self {
        let lo: Location = lo.into();
        Self {
            file: lo.file().to_string(),
            line: lo.line().to_string(),
            column: lo.column().to_string(),
        }
    }
}

/// Necessary information for recording panic
#[derive(Debug, Clone)]
pub struct PanicData<'a> {
    /// Panic hook metadata and payload message.
    message: &'a PanicHookInfo<'a>,
    /// Formatted stack backtrace captured at the panic site.
    backtrace: String,
    /// Source location of the panic, if reported by the runtime.
    location: Option<PanicLocation>,
}

impl<'a, T> From<T> for PanicData<'a>
where T: Into<&'a PanicHookInfo<'a>>
{
    fn from(panic: T) -> PanicData<'a> {
        let panic = panic.into();
        let backtrace = Backtrace::new();
        let backtrace = format!("{backtrace:?}");
        let location: Option<PanicLocation> = panic.location().map(|l| PanicLocation::from(*l));
        PanicData {
            message: panic,
            backtrace,
            location,
        }
    }
}

impl fmt::Display for PanicLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

impl<'a> fmt::Display for PanicData<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.location {
            Some(l) => write!(f, "{}, {} \n\n {}", self.message, l, self.backtrace),
            None => write!(f, "{} \n\n {}", self.message, self.backtrace),
        }
    }
}

/// Formats and emits a panic hook event via `tracing::error!`.
fn log_panic(panic: &PanicHookInfo) {
    let data: PanicData = panic.into();
    tracing::error!("{}", data)
}

/// Setup hooks for panic, this function works for both wasm and native.
pub fn set_panic_hook() {
    // Set a panic hook that records the panic as a `tracing` event at the
    // `ERROR` verbosity level.
    //
    // If we are currently in a span when the panic occurred, the logged event
    // will include the current span, allowing the context in which the panic
    // occurred to be recorded.
    std::panic::set_hook(Box::new(|panic| {
        log_panic(panic);
    }));
}

#[cfg(feature = "node")]
/// logging configuration about node.
pub mod node {
    use tracing_subscriber::filter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;

    use super::*;

    /// Build the default `EnvFilter` directive string for `level` when `RINGS_LOG_FILTER` is unset
    /// (#1316): keeps Rings crates at `level` while clamping noisy third-party `webrtc-rs` / `turn`
    /// (`target = "log"`, `webrtc*`, `turn`) diagnostics to `off`.
    pub(crate) fn default_log_filter_spec(level: LogLevel) -> String {
        let level_filter = filter::LevelFilter::from_level(level.into());
        format!(
            "{level_filter},log=off,turn=off,webrtc=off,webrtc_ice=off,webrtc_dtls=off,webrtc_sctp=off,webrtc_mdns=off,webrtc_data=off,webrtc_srtp=off,webrtc_util=off"
        )
    }

    /// Initializes native tracing and panic logging for the requested level.
    #[no_mangle]
    pub extern "C" fn rings_node_init_logging(level: LogLevel) {
        set_panic_hook();

        let subscriber = Registry::default();
        let default_spec = default_log_filter_spec(level);
        let filter = match std::env::var("RINGS_LOG_FILTER") {
            Ok(spec) if !spec.trim().is_empty() => {
                EnvFilter::try_new(spec.trim()).unwrap_or_else(|err| {
                    eprintln!(
                        "invalid RINGS_LOG_FILTER '{}': {}; falling back to {}",
                        spec, err, default_spec
                    );
                    EnvFilter::new(default_spec)
                })
            }
            _ => EnvFilter::new(default_spec),
        };

        // Stderr
        let subscriber = subscriber.with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter),
        );
        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this functions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
/// Browser console logging configuration.
pub mod browser {
    use tracing_wasm::ConsoleConfig;
    use tracing_wasm::WASMLayer;
    use tracing_wasm::WASMLayerConfigBuilder;

    use super::*;
    /// Initializes browser console tracing and panic logging for the requested level.
    #[wasm_export]
    pub fn init_logging(level: LogLevel) {
        set_panic_hook();

        let subscriber = Registry::default();

        // Browser console and profiler
        let subscriber = subscriber.with(WASMLayer::new(
            WASMLayerConfigBuilder::new()
                .set_max_level(level.into())
                .set_console_config(ConsoleConfig::ReportWithoutConsoleColor)
                .build(),
        ));

        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this functions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(test)]
mod tests {
    use super::LogLevel;

    #[test]
    fn test_default_log_level_is_error() {
        assert_eq!(LogLevel::default(), LogLevel::Error);
    }

    #[cfg(feature = "node")]
    #[test]
    fn test_default_log_filter_spec_clamps_third_party_webrtc_and_turn_targets() {
        use tracing_subscriber::EnvFilter;

        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            let spec = super::node::default_log_filter_spec(level);
            assert!(
                EnvFilter::try_new(&spec).is_ok(),
                "default log filter spec must parse: {spec}"
            );
            assert!(spec.contains("log=off"));
            assert!(spec.contains("turn=off"));
            assert!(spec.contains("webrtc_ice=off"));
        }
    }
}
