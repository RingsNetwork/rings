use log::Log;
use log::{Level, Metadata, Record};
use log::{LevelFilter, SetLoggerError};

pub struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!("{} - {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

impl Logger {
    pub fn init() -> Result<(), SetLoggerError> {
        log::set_boxed_logger(Box::new(Logger)).map(|()| log::set_max_level(LevelFilter::Info))
    }
}
