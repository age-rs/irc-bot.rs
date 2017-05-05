extern crate clap;
extern crate irc_bot;

#[macro_use]
extern crate log;

use irc_bot::modules;
use std::io;
use std::io::Write as IoWrite;

const PROGRAM_NAME: &'static str = "bot74d";

fn main() {
    let args = clap::App::new(PROGRAM_NAME)
        .arg(clap::Arg::with_name("config-file")
                 .short("c")
                 .default_value("config.json"))
        .get_matches();

    let log_lvl = log::LogLevelFilter::Info;

    log::set_logger(|max_log_lvl| {
                        max_log_lvl.set(log_lvl);
                        Box::new(LogBackend { log_lvl: log_lvl })
                    })
            .expect("error: failed to initialize logging");

    irc_bot::run(args.value_of("config-file").expect("default missing?"),
                 |err| {
                     error!("{}", err);
                     irc_bot::ErrorReaction::Proceed
                 },
                 &[modules::default(), modules::test()]);
}


struct LogBackend {
    log_lvl: log::LogLevelFilter,
}

impl log::Log for LogBackend {
    fn enabled(&self, metadata: &log::LogMetadata) -> bool {
        metadata.level() <= self.log_lvl
    }

    fn log(&self, record: &log::LogRecord) {
        if !self.enabled(record.metadata()) {
            return;
        }
        writeln!(io::stderr(), "{}: {}", record.level(), record.args()).expect("stderr broken?");
    }
}