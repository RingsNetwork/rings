use clap::ArgEnum;
use tracing::error;
use tracing_log::LogTracer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;

#[derive(ArgEnum, Debug, Clone)]
#[clap(rename_all = "kebab-case")]
pub enum LogLevel {
    Off,
    Debug,
    Info,
    Warn,
    Error,
    Trace,
}

impl From<LogLevel> for LevelFilter {
    fn from(val: LogLevel) -> Self {
        match val {
            LogLevel::Off => LevelFilter::OFF,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Trace => LevelFilter::TRACE,
        }
    }
}

#[cfg(feature = "node")]
pub mod node {
    use std::backtrace::Backtrace;
    use std::panic::PanicInfo;

    use opentelemetry::global;
    use opentelemetry::sdk::propagation::TraceContextPropagator;

    use super::*;

    fn set_panic_hook() {
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

    fn log_panic(panic: &PanicInfo) {
        let backtrace = Backtrace::force_capture();
        let backtrace = format!("{:?}", backtrace);
        if let Some(location) = panic.location() {
            error!(
                message = %panic,
                backtrace = %backtrace,
                panic.file = location.file(),
                panic.line = location.line(),
                panic.column = location.column(),
            );
        } else {
            error!(message = %panic, backtrace = %backtrace);
        }
    }

    pub fn init_logging(level: LevelFilter) {
        set_panic_hook();

        let subscriber = Registry::default();

        // Stderr
        let subscriber =
            subscriber.with(fmt::layer().with_writer(std::io::stderr).with_filter(level));

        // Jaeger
        let subscriber = {
            if let Ok(endpoint) = std::env::var("RINGS_JAEGER_AGENT_ENDPOINT") {
                global::set_text_map_propagator(TraceContextPropagator::new());
                let jaeger = opentelemetry_jaeger::new_agent_pipeline()
                    .with_service_name("rings")
                    .with_endpoint(endpoint)
                    .with_auto_split_batch(true)
                    .install_batch(opentelemetry::runtime::Tokio)
                    .expect("opentelemetry_jaeger install");
                subscriber.with(Some(
                    tracing_opentelemetry::layer()
                        .with_tracer(jaeger)
                        .with_filter(level),
                ))
            } else {
                subscriber.with(None)
            }
        };

        // Enable log compatible layer to convert log record to tracing span.
        // We will ignore any errors that returned by this fucntions.
        let _ = LogTracer::init();

        // Ignore errors returned by set_global_default.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

#[cfg(feature = "browser")]
pub mod browser {
    fn set_panic_hook() {
        // When the `console_error_panic_hook` feature is enabled, we can call the
        // `set_panic_hook` function at least once during initialization, and then
        // we will get better error messages if our code ever panics.
        //
        // For more details see
        // https://github.com/rustwasm/console_error_panic_hook#readme
        // This is not needed for tracing_wasm to work, but it is a common tool for getting proper error line numbers for panics.
        #[cfg(feature = "console_error_panic_hook")]
        console_error_panic_hook::set_once();
    }

    pub fn init_logging() {
        set_panic_hook();

        tracing_wasm::set_as_global_default();

        //TODO: Jaeger in browser. How to setup agent endpoint?
    }
}
