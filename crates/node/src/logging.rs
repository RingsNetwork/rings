//! Logging configuration contains both `node` and `browser`.
use std::fmt;
use std::panic::Location;
use std::panic::PanicInfo;

use backtrace::Backtrace;
use clap::ValueEnum;
use tracing::Level;
use tracing_log::LogTracer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

#[cfg(feature = "browser")]
pub use self::browser::init_logging;
#[cfg(feature = "node")]
pub use self::node::init_logging;
use crate::prelude::wasm_export;

#[repr(C)]
#[wasm_export]
#[derive(ValueEnum, Debug, Clone)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
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
    file: String,
    line: String,
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
            column: lo.file().to_string(),
        }
    }
}

/// Necessary information for recording panic
#[derive(Debug, Clone)]
pub struct PanicData<'a> {
    message: &'a PanicInfo<'a>,
    backtrace: String,
    location: Option<PanicLocation>,
}

impl<'a, T> From<T> for PanicData<'a>
where T: Into<&'a PanicInfo<'a>>
{
    fn from(panic: T) -> PanicData<'a> {
        let panic = panic.into();
        let backtrace = Backtrace::new();
        let backtrace = format!("{:?}", backtrace);
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

fn log_panic(panic: &PanicInfo) {
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
    use tracing_subscriber::Layer;

    use super::*;

    #[no_mangle]
    pub extern "C" fn init_logging(level: LogLevel) {
        set_panic_hook();

        let subscriber = Registry::default();
        let level_filter = filter::LevelFilter::from_level(level.into());

        // Stderr
        let subscriber = subscriber.with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(level_filter),
        );
        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this functions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(feature = "browser")]
pub mod browser {
    use tracing_wasm::ConsoleConfig;
    use tracing_wasm::WASMLayer;
    use tracing_wasm::WASMLayerConfigBuilder;

    use super::*;
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
