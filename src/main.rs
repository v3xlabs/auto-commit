mod branch;
mod cli;
mod commit;
mod config;
mod git;
mod model;
mod output;
mod picker;
mod squash;

use std::process::ExitCode;

use clap::Parser;

use crate::{
    cli::{Cli, Command, ConfigAction},
    config::{Config, ConfigError},
    git::GitError,
    model::ModelError,
    output::{Event, Reporter},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Model(#[from] ModelError),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("aborted")]
    Aborted,

    #[error("HEAD is detached, so there is no branch to name")]
    DetachedHead,

    #[error("{0} is the default branch, so there is nothing to rename")]
    OnDefaultBranch(String),

    #[error("{0} has no commits that {1} does not already have")]
    NoCommitsOnBranch(String, String),

    #[error("every suggested name is already taken")]
    NoUsableName,

    #[error("this branch has {0} commits that the base does not, so there is nothing to squash")]
    NothingToSquash(usize),

    #[error("$EDITOR is not set")]
    NoEditor,
}

impl Error {
    /// The stable identifier a caller matches on. It is the same string in the
    /// JSON error event and in the exit code table.
    fn code(&self) -> &'static str {
        match self {
            Error::Git(GitError::NotARepository) => "not_a_repository",
            Error::Git(GitError::NothingStaged) => "no_staged_changes",
            Error::Git(_) => "git_failed",
            Error::Config(ConfigError::NoApiKey) => "no_api_key",
            Error::Config(_) => "bad_config",
            Error::Model(_) => "model_error",
            Error::Io(_) => "io_error",
            Error::Aborted => "aborted",
            Error::DetachedHead => "detached_head",
            Error::OnDefaultBranch(_) => "on_default_branch",
            Error::NoCommitsOnBranch(..) => "no_commits_on_branch",
            Error::NoUsableName => "no_usable_name",
            Error::NothingToSquash(_) => "nothing_to_squash",
            Error::NoEditor => "no_editor",
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Error::Git(GitError::NothingStaged) => 2,
            Error::Git(GitError::NotARepository) => 3,
            Error::Config(ConfigError::NoApiKey) => 4,
            Error::Model(_) => 5,
            Error::Aborted => 130,
            _ => 1,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut reporter = Reporter::new(&cli.global);

    let result = dispatch(&cli, &mut reporter).await;

    match result {
        Ok(()) => {
            reporter.finish();
            ExitCode::SUCCESS
        }
        Err(error) => {
            reporter.event(Event::Error {
                code: error.code(),
                message: error.to_string(),
            });

            reporter.finish();
            ExitCode::from(error.exit_code())
        }
    }
}

async fn dispatch(cli: &Cli, reporter: &mut Reporter) -> Result<(), Error> {
    match &cli.command {
        None => commit::run(&cli.commit, &cli.global, reporter).await,
        Some(Command::Commit(args)) => commit::run(args, &cli.global, reporter).await,
        Some(Command::Branch(args)) => branch::run(args, &cli.global, reporter).await,
        Some(Command::Squash(args)) => squash::run(args, &cli.global, reporter).await,
        Some(Command::Config { action }) => configure(action, cli, reporter).await,
    }
}

async fn configure(action: &ConfigAction, cli: &Cli, reporter: &mut Reporter) -> Result<(), Error> {
    match action {
        ConfigAction::Path => {
            reporter.payload(&config::user_path().display().to_string());
        }

        ConfigAction::Get { key } => {
            let root = git::repo_root().await.ok();
            let config = Config::load(root.as_ref(), &cli.global)?;

            match key {
                Some(key) => reporter.payload(&config.get(key)?),
                None => {
                    for key in Config::KEYS {
                        reporter.payload(&format!("{key} = {}", config.get(key)?));
                    }
                }
            }
        }

        ConfigAction::Set { key, value } => {
            let path = config::set(key, value)?;
            reporter.payload(&format!("{key} written to {}", path.display()));
        }

        ConfigAction::Edit => {
            let path = config::user_path();

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let editor = editor().ok_or(Error::NoEditor)?;

            std::process::Command::new(editor).arg(&path).status()?;
        }
    }

    Ok(())
}

/// The editor the user prefers, if they have said. `VISUAL` wins over
/// `EDITOR` because that is the order every other tool uses.
pub fn editor() -> Option<String> {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|editor| !editor.trim().is_empty())
}
