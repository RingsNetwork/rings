use clap::ArgEnum;
use log::Log;
use log::{Level, Metadata, Record};
use log::{LevelFilter, SetLoggerError};

pub struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Trace
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!("{} - {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

impl Logger {
    pub fn init(level: LevelFilter) -> Result<(), SetLoggerError> {
        log::set_boxed_logger(Box::new(Logger)).map(|()| log::set_max_level(level))
    }
}

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

impl From<LogLevel> for log::LevelFilter {
    fn from(val: LogLevel) -> Self {
        match val {
            LogLevel::Off => log::LevelFilter::Off,
            LogLevel::Debug => log::LevelFilter::Debug,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Trace => log::LevelFilter::Trace,
        }
    }
}
