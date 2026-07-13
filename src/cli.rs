//! User-visible command-line boundary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::VERSION;

const HELP: &str = "AkuSupervisor - local development service supervisor\n\n\
Usage:\n\
  aku-supervisor\n\
  aku-supervisor --config <path>\n\
  aku-supervisor run\n\
  aku-supervisor run --config <path>\n\
  aku-supervisor --help\n\
  aku-supervisor --version\n\n\
Without --config, AkuSupervisor checks AKU_SUPERVISOR_CONFIG and then the default user configuration.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Run { config: Option<PathBuf> },
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
        Ok(Command::Run { config }) => run_foreground(config),
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(Command::Run { config: None }),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [flag] if flag == "--version" || flag == "-V" => Ok(Command::Version),
        [config_flag, config] if config_flag == "--config" => Ok(Command::Run {
            config: Some(PathBuf::from(config)),
        }),
        [run] if run == "run" => Ok(Command::Run { config: None }),
        [run, config_flag, config] if run == "run" && config_flag == "--config" => {
            Ok(Command::Run {
                config: Some(PathBuf::from(config)),
            })
        }
        [argument] => Err(format!(
            "unsupported argument: {}",
            argument.to_string_lossy()
        )),
        _ => Err("expected run, run --config <path>, --help, or --version".to_owned()),
    }
}

#[cfg(windows)]
fn run_foreground(explicit_config: Option<PathBuf>) -> ExitCode {
    let resolved = match crate::adapters::config_path::resolve_config_path(explicit_config) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match crate::adapters::foreground::run(resolved.path()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn run_foreground(_explicit_config: Option<PathBuf>) -> ExitCode {
    eprintln!("error: no lifecycle platform adapter is implemented for this operating system");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Command, parse};

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn no_argument_starts_with_discovered_configuration() {
        assert_eq!(parse(args(&[])), Ok(Command::Run { config: None }));
        assert_eq!(parse(args(&["run"])), Ok(Command::Run { config: None }));
    }

    #[test]
    fn version_flag_selects_version() {
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    }

    #[test]
    fn lifecycle_commands_require_the_running_supervisor_boundary() {
        let error = parse(args(&["start"])).expect_err("start is an interactive or API action");
        assert!(error.contains("unsupported argument"));
    }

    #[test]
    fn explicit_configuration_path_is_supported() {
        let expected = Ok(Command::Run {
            config: Some(PathBuf::from("services.json")),
        });
        assert_eq!(parse(args(&["--config", "services.json"])), expected);
        assert_eq!(
            parse(args(&["run", "--config", "services.json"])),
            Ok(Command::Run {
                config: Some(PathBuf::from("services.json"))
            })
        );
    }
}
