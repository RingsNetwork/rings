#![deny(missing_docs, warnings)]
//! Command-line boundary for generating or checking repository logo assets.

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rings_logo::{generate_assets, LogoError};

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rings-logo: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, CliError> {
    let action = parse_action(env::args_os())?;
    let current = env::current_dir().map_err(CliError::CurrentDirectory)?;
    let root = find_repository_root(&current).ok_or(CliError::RepositoryRoot(current))?;
    let assets = generate_assets()?;

    match action {
        Action::Generate => {
            for asset in &assets {
                let output = root.join(asset.path());
                if let Some(parent) = output.parent() {
                    fs::create_dir_all(parent).map_err(|source| CliError::Write {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                fs::write(&output, asset.contents()).map_err(|source| CliError::Write {
                    path: output,
                    source,
                })?;
            }
            Ok(format!("generated {} deterministic assets", assets.len()))
        }
        Action::Check => {
            let mut drift = Vec::new();
            for asset in &assets {
                let path = root.join(asset.path());
                let actual = fs::read_to_string(&path).map_err(|source| CliError::Read {
                    path: path.clone(),
                    source,
                })?;
                if actual != asset.contents() {
                    drift.push(asset.path());
                }
            }
            if drift.is_empty() {
                Ok(format!("verified {} deterministic assets", assets.len()))
            } else {
                Err(CliError::Drift(drift))
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Generate,
    Check,
}

fn parse_action(mut arguments: impl Iterator<Item = OsString>) -> Result<Action, CliError> {
    let _program = arguments.next();
    let action = match arguments.next() {
        Some(value) if value == "generate" => Action::Generate,
        Some(value) if value == "check" => Action::Check,
        Some(value) => return Err(CliError::UnknownAction(value)),
        None => return Err(CliError::MissingAction),
    };
    if let Some(extra) = arguments.next() {
        return Err(CliError::UnexpectedArgument(extra));
    }
    Ok(action)
}

fn find_repository_root(start: &Path) -> Option<PathBuf> {
    let mut candidate = start.to_path_buf();
    loop {
        if candidate.join("Cargo.toml").is_file()
            && candidate.join("assets/logo").is_dir()
            && candidate.join("crates").is_dir()
        {
            return Some(candidate);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

#[derive(Debug)]
enum CliError {
    MissingAction,
    UnknownAction(OsString),
    UnexpectedArgument(OsString),
    CurrentDirectory(std::io::Error),
    RepositoryRoot(PathBuf),
    Logo(LogoError),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    Drift(Vec<&'static str>),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAction => formatter.write_str("expected `generate` or `check`"),
            Self::UnknownAction(value) => {
                write!(formatter, "unknown action `{}`", value.to_string_lossy())
            }
            Self::UnexpectedArgument(value) => {
                write!(
                    formatter,
                    "unexpected argument `{}`",
                    value.to_string_lossy()
                )
            }
            Self::CurrentDirectory(error) => {
                write!(formatter, "cannot read current directory: {error}")
            }
            Self::RepositoryRoot(path) => {
                write!(
                    formatter,
                    "cannot find the Rings repository above {}",
                    path.display()
                )
            }
            Self::Logo(error) => write!(formatter, "{error}"),
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
            Self::Drift(paths) => write!(
                formatter,
                "generated assets differ: {}; run `cargo run -p rings-logo -- generate`",
                paths.join(", "),
            ),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDirectory(error) => Some(error),
            Self::Logo(error) => Some(error),
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::MissingAction
            | Self::UnknownAction(_)
            | Self::UnexpectedArgument(_)
            | Self::RepositoryRoot(_)
            | Self::Drift(_) => None,
        }
    }
}

impl From<LogoError> for CliError {
    fn from(error: LogoError) -> Self {
        Self::Logo(error)
    }
}
