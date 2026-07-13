//! Minimal command-line boundary for the Phase 0 executable.

use std::ffi::OsString;
use std::process::ExitCode;

use crate::VERSION;

const HELP: &str = "AkuSupervisor - local development service supervisor\n\n\
Usage:\n  aku-supervisor --help\n  aku-supervisor --version\n\n\
Service lifecycle commands will be added in Roadmap Phase 3.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Help,
    Version,
}

/// Runs the CLI using an argument iterator that excludes the executable name.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    match parse(arguments) {
        Ok(Command::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("aku-supervisor {VERSION}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(Command::Help),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [flag] if flag == "--version" || flag == "-V" => Ok(Command::Version),
        [argument] => Err(format!(
            "unsupported argument: {}",
            argument.to_string_lossy()
        )),
        _ => Err("expected at most one argument".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn no_argument_shows_help() {
        assert_eq!(parse(args(&[])), Ok(Command::Help));
    }

    #[test]
    fn version_flag_selects_version() {
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    }

    #[test]
    fn lifecycle_commands_are_not_silently_accepted() {
        let error = parse(args(&["start"])).expect_err("start must remain unavailable in Phase 0");
        assert!(error.contains("unsupported argument"));
    }
}
