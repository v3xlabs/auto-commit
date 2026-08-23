use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "auto-commit",
    version,
    about = "Automagically generate commit messages."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub commit: CommitArgs,

    #[command(flatten)]
    pub global: GlobalArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write a commit message for the staged changes. This is the default.
    Commit(CommitArgs),

    /// Suggest a better name for the current branch.
    Branch(BranchArgs),

    /// Suggest how the commits on this branch could be squashed.
    Squash(SquashArgs),

    /// Inspect and edit the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the path of the config file that would be written.
    Path,
    /// Print one resolved setting, or every setting when no key is given.
    Get { key: Option<String> },
    /// Set one setting in the user config file.
    Set { key: String, value: String },
    /// Open the user config file in $EDITOR.
    Edit,
}

/// Settings that apply to every command, including the config layer overrides.
#[derive(Args, Debug, Default)]
pub struct GlobalArgs {
    /// Print the result to stdout and change nothing.
    #[arg(short, long, alias = "dry-run", global = true)]
    pub print: bool,

    /// Print one JSON object to stdout when the run finishes.
    #[arg(long, global = true, conflicts_with = "json_stream")]
    pub json: bool,

    /// Print newline-delimited JSON events to stdout as they happen.
    #[arg(long, global = true)]
    pub json_stream: bool,

    /// Silence progress on stderr.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Report timings on stderr when the run finishes.
    #[arg(long, global = true)]
    pub timing: bool,

    /// Override the configured model.
    #[arg(long, global = true, value_name = "NAME")]
    pub model: Option<String>,

    /// Override the configured API endpoint.
    #[arg(long, global = true, value_name = "URL")]
    pub endpoint: Option<String>,

    /// Override how many previous commits are shown to the model.
    #[arg(long, global = true, value_name = "N")]
    pub context_commits: Option<usize>,

    /// Override how many times the model may read a withheld diff. 0 disables it.
    #[arg(long, global = true, value_name = "N")]
    pub max_tool_calls: Option<u32>,

    /// Override how many messages to generate. Above 1 opens a picker.
    #[arg(long, global = true, value_name = "N")]
    pub candidates: Option<usize>,

    /// Ask for conventional commit format regardless of the config.
    #[arg(long, global = true)]
    pub conventional: bool,

    /// Stream the model's thinking instead of collapsing it to a summary.
    #[arg(long, global = true)]
    pub show_thinking: bool,
}

#[derive(Args, Debug, Default)]
pub struct CommitArgs {
    /// Edit the message in $EDITOR before committing.
    #[arg(short, long)]
    pub review: bool,

    /// Do not ask for confirmation before committing.
    #[arg(short = 'y', long, visible_alias = "force", visible_short_alias = 'f')]
    pub yes: bool,

    /// Read the diff from stdin instead of from git.
    #[arg(long)]
    pub stdin_diff: bool,

    /// Replace the previous commit.
    #[arg(long)]
    pub amend: bool,

    /// Add a Signed-off-by trailer.
    #[arg(short, long)]
    pub signoff: bool,

    /// Sign the commit with GPG.
    #[arg(short = 'S', long = "gpg-sign")]
    pub gpg_sign: bool,

    /// Skip the pre-commit and commit-msg hooks.
    #[arg(long)]
    pub no_verify: bool,

    /// Force the conventional commit type, for example `feat`.
    #[arg(long = "type", value_name = "TYPE")]
    pub kind: Option<String>,

    /// Force the conventional commit scope.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,

    /// Issue to reference in the message.
    #[arg(long, value_name = "ID")]
    pub issue: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct BranchArgs {
    /// Rename the local branch to the chosen suggestion.
    #[arg(long)]
    pub rename: bool,

    /// Compare against this ref instead of the default branch.
    #[arg(long, value_name = "REF")]
    pub onto: Option<String>,
}

#[derive(Args, Debug, Default)]
pub struct SquashArgs {
    /// Compare against this ref instead of the default branch.
    #[arg(long, value_name = "REF")]
    pub onto: Option<String>,
}
