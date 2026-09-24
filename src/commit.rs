use std::{
    fmt::Write as _,
    io::{IsTerminal, Read},
};

use owo_colors::{OwoColorize, Stream};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cli::{CommitArgs, GlobalArgs},
    config::Config,
    git::{self, CommitOptions, Diff, GitError},
    model::{self, Model, ModelError},
    output::{Event, Reporter},
    picker, staged, Error,
};

/// One proposed commit message. The subject and body stay apart until the
/// moment the message is handed to git, so the picker and the length warning
/// can both work on the subject alone.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub subject: String,
    pub body: String,
}

impl Message {
    pub fn render(&self) -> String {
        if self.body.trim().is_empty() {
            self.subject.trim().to_owned()
        } else {
            format!("{}\n\n{}", self.subject.trim(), self.body.trim())
        }
    }

    fn parse(text: &str) -> Self {
        let text = text.trim();

        match text.split_once("\n\n") {
            Some((subject, body)) => Self {
                subject: subject.trim().to_owned(),
                body: body.trim().to_owned(),
            },
            None => Self {
                subject: text.lines().next().unwrap_or_default().to_owned(),
                body: text.lines().skip(1).collect::<Vec<_>>().join("\n"),
            },
        }
    }
}

#[derive(Deserialize)]
struct Candidates {
    messages: Vec<Message>,
}

pub async fn run(
    args: &CommitArgs,
    global: &GlobalArgs,
    reporter: &mut Reporter,
) -> Result<(), Error> {
    let root = git::repo_root().await?;
    let config = Config::load(Some(&root), global)?;

    reporter.set_show_thinking(config.show_thinking);

    // No spinner yet. Reading the diff is instant, and starting one here made
    // the tool look like it was working before it had even found a change.
    let mut diff = if args.stdin_diff {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        git::diff_from_text(&config, &text)?
    } else {
        match git::staged_diff(&config).await {
            Ok(diff) => diff,
            Err(GitError::NothingStaged) => return Err(nothing_staged(reporter).await),
            Err(error) => return Err(error.into()),
        }
    };

    // Nothing is sent until you say so, so the staging area can go on
    // changing while the screen is up. `--yes` and `--print` have already
    // said go, and a diff read from stdin cannot change.
    if interactive(reporter) && !args.yes && !global.print {
        diff = staged::watch(&config, diff).await?;
    }

    let model = Model::new(&config)?;
    let mut review = args.review;

    // A reload starts here rather than in the round below, because what is
    // staged has changed: the context, the prompt and the conversation all
    // have to be built again from the diff that is there now.
    let chosen = 'session: loop {
        let paths = diff.paths();

        let (recent, touching, template, branch) = tokio::join!(
            git::recent_commits(config.context_commits, &config.self_emails),
            git::commits_touching(&paths, config.context_commits),
            git::commit_template(&root),
            git::branch_name(),
        );

        // How many commit examples actually reached the prompt, which is not
        // the configured count when the repository is younger than that.
        let commits_used = recent.len() + touching.as_deref().map_or(0, |log| log.lines().count());

        let (added, deleted) = diff.totals();

        reporter.event(Event::Context {
            staged_files: diff.files.len(),
            included: diff.included_paths().len(),
            withheld: diff.withheld_paths(),
            added,
            deleted,
            diff_bytes: diff.included.len(),
            commits_used,
            model: config.model.clone(),
        });

        // Each rejected round stays in the conversation, so a second attempt
        // knows what it already proposed and why you did not want it.
        let mut messages = vec![
            model::system(system_prompt(&config, args, body_allowed(&config, &recent))),
            model::user(context_prompt(
                &recent,
                touching.as_deref(),
                template.as_deref(),
                branch.as_deref(),
                &diff,
            )),
        ];

        loop {
            reporter.start("writing the message");

            let candidates = generate(&model, &config, messages.clone(), &diff, reporter).await?;

            reporter.settle();

            // An empty list is the model failing to answer, not the user
            // declining, so it must not be reported as an abort.
            if candidates.is_empty() {
                return Err(ModelError::Empty.into());
            }

            // Rejecting the whole round and rejecting the one you picked mean
            // the same thing to the model, so both come back here.
            let feedback = match choose(&candidates, reporter)? {
                Pick::Cancel => return Err(Error::Aborted),
                Pick::Feedback(feedback) => feedback,
                Pick::Reload => {
                    diff = reload(&config, reporter).await?;
                    continue 'session;
                }
                // An edited message goes straight through. It is yours now, so
                // there is nothing left to confirm about the wording.
                Pick::Edit(index) => match edit(&candidates[index]).await? {
                    Some(edited) => break 'session edited,
                    None => return Err(Error::Aborted),
                },
                Pick::Message(index) => {
                    let chosen = candidates[index].clone();

                    if global.print || args.yes {
                        break 'session chosen;
                    }

                    match confirm(&chosen, &config, reporter)? {
                        Decision::Commit => break 'session chosen,
                        Decision::Edit => {
                            review = true;
                            break 'session chosen;
                        }
                        Decision::Cancel => return Err(Error::Aborted),
                        Decision::Reload => {
                            diff = reload(&config, reporter).await?;
                            continue 'session;
                        }
                        Decision::Feedback(feedback) => feedback,
                    }
                }
            };

            messages.push(model::assistant(proposal_summary(&candidates)));
            messages.push(model::user(format!(
                "None of those are right. {feedback}\n\nPropose {} new ones that answer that.",
                config.candidates.max(1)
            )));
        }
    };

    reporter.event(Event::Message {
        subject: chosen.subject.clone(),
        body: chosen.body.clone(),
    });

    let message = chosen.render();

    if global.print {
        reporter.payload(&message);
        return Ok(());
    }

    let sha = git::commit(
        &message,
        &CommitOptions {
            review,
            amend: args.amend,
            signoff: args.signoff,
            gpg_sign: args.gpg_sign,
            no_verify: args.no_verify,
        },
    )
    .await?;

    reporter.event(Event::Committed { sha });

    Ok(())
}

/// Reads the staging area again for another round. What is staged now is what
/// the next messages describe, so nothing of the last round is carried over.
async fn reload(config: &Config, reporter: &mut Reporter) -> Result<Diff, Error> {
    match git::staged_diff(config).await {
        Ok(diff) => Ok(diff),
        Err(GitError::NothingStaged) => Err(nothing_staged(reporter).await),
        Err(error) => Err(error.into()),
    }
}

/// Nothing staged is the case the tool is reached for most often, so it says
/// what is there rather than only what is missing.
///
/// It does not stage anything. Choosing what goes in a commit is the user's
/// job, and a tool that runs `git add` on their behalf is doing the one part
/// of the work they should not delegate.
async fn nothing_staged(reporter: &mut Reporter) -> Error {
    let dirty = git::unstaged_files().await;

    // The list only, with no heading of its own. The error printed after it
    // says what the list means, so a heading here would say it twice.
    if !dirty.is_empty() && reporter.is_human() {
        eprintln!();

        for (status, path) in &dirty {
            eprintln!(
                "  {:<2}  {path}",
                status.if_supports_color(Stream::Stderr, |text| text.yellow())
            );
        }

        eprintln!();
    }

    GitError::NothingStaged.into()
}

fn interactive(reporter: &Reporter) -> bool {
    reporter.is_human() && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// What to do with the message that was chosen.
enum Decision {
    Commit,
    Edit,
    Feedback(String),
    Reload,
    Cancel,
}

/// What the reader did with the proposed messages.
enum Pick {
    Message(usize),
    Edit(usize),
    Feedback(String),
    Reload,
    Cancel,
}

/// Opens the message in the user's editor and reads back what they saved.
/// Written where git keeps its own edited message, so it is inside the
/// repository's git directory and never in the working tree.
async fn edit(message: &Message) -> Result<Option<Message>, Error> {
    let editor = crate::editor().ok_or(Error::NoEditor)?;
    let path = git::git_dir().await?.join("AUTO_COMMIT_EDITMSG");

    std::fs::write(
        &path,
        format!(
            "{}\n\n# Lines starting with # are dropped. Save an empty message to cancel.\n",
            message.render()
        ),
    )?;

    let status = std::process::Command::new(&editor).arg(&path).status()?;

    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }

    let edited = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);

    let kept: Vec<&str> = edited
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();

    let text = kept.join("\n");

    Ok((!text.trim().is_empty()).then(|| Message::parse(&text)))
}

/// Takes a non-empty list; the caller has already rejected an empty one.
///
/// The body of the highlighted message shows inside the list rather than
/// above it. Listing every body first and then listing the subjects again to
/// choose from prints the same thing twice, and choosing from subjects alone
/// means the body arrives in the commit unseen.
fn choose(candidates: &[Message], reporter: &Reporter) -> Result<Pick, Error> {
    // One proposal needs no list. The confirmation that follows shows it and
    // carries the same feedback option, so a picker here would be a keystroke
    // that decides nothing.
    if candidates.len() == 1 || !interactive(reporter) {
        return Ok(Pick::Message(0));
    }

    let items: Vec<picker::Item> = candidates
        .iter()
        .map(|candidate| picker::Item {
            title: candidate.subject.clone(),
            detail: candidate.body.clone(),
        })
        .collect();

    let chosen = picker::select(
        "Which message?",
        &items,
        reporter.thinking(),
        crate::editor().is_some(),
    )?;

    Ok(match chosen {
        picker::Choice::Item(index) => Pick::Message(index),
        picker::Choice::Edit(index) => Pick::Edit(index),
        picker::Choice::Feedback(text) => Pick::Feedback(text),
        picker::Choice::Reload => Pick::Reload,
        picker::Choice::Cancel => Pick::Cancel,
    })
}

/// What the model proposed last round, written back as its own turn so the
/// feedback that follows has something to refer to.
fn proposal_summary(candidates: &[Message]) -> String {
    candidates
        .iter()
        .map(|candidate| format!("- {}", candidate.subject))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shows exactly what git is about to store, then asks. The message is
/// rendered the same way whether it came from a picker or straight through,
/// so nothing reaches a commit that was not on screen first.
fn confirm(message: &Message, config: &Config, reporter: &Reporter) -> Result<Decision, Error> {
    if !interactive(reporter) {
        return Ok(Decision::Commit);
    }

    eprintln!();
    eprintln!(
        "  {}",
        message
            .subject
            .if_supports_color(Stream::Stderr, |text| text.bold())
    );

    let over = message
        .subject
        .chars()
        .count()
        .saturating_sub(config.subject_max_len);

    if over > 0 {
        eprintln!(
            "  {}",
            format!(
                "{} characters, {over} over the {} you configured",
                message.subject.chars().count(),
                config.subject_max_len
            )
            .if_supports_color(Stream::Stderr, |text| text.yellow())
        );
    }

    for line in body_lines(message) {
        if line.is_empty() {
            eprintln!();
        } else {
            eprintln!("  {line}");
        }
    }

    eprintln!();

    let chosen = inquire::Select::new(
        "Commit this?",
        vec![
            "commit",
            "edit first",
            "try again, with feedback",
            "reload the staged change and try again",
            "cancel",
        ],
    )
    .raw_prompt()
    .map_err(|_| Error::Aborted)?;

    Ok(match chosen.index {
        0 => Decision::Commit,
        1 => Decision::Edit,
        2 => match inquire::Text::new("What would you rather it said?").prompt() {
            Ok(feedback) if !feedback.trim().is_empty() => Decision::Feedback(feedback),
            _ => Decision::Cancel,
        },
        3 => Decision::Reload,
        _ => Decision::Cancel,
    })
}

/// The body as it will be stored, with a blank leading line when there is one,
/// so a message with a body reads the way git will show it.
fn body_lines(message: &Message) -> Vec<String> {
    let body = message.body.trim();

    if body.is_empty() {
        return Vec::new();
    }

    std::iter::once(String::new())
        .chain(body.lines().map(str::to_owned))
        .collect()
}

/// A history that almost never carries a body outranks the `body` setting.
/// Asked to explain itself, a model writes a body every time, so the
/// instruction to match the examples is not enough on its own.
fn body_allowed(config: &Config, recent: &[git::PastCommit]) -> bool {
    let with_body = recent
        .iter()
        .filter(|commit| !commit.body.is_empty())
        .count();

    config.body && with_body * 5 >= recent.len()
}

/// The two paths ask for different shapes, so they need different
/// instructions. Telling a model to "reply with the message and nothing else"
/// while handing it a schema that wants an array of them is a contradiction,
/// and a model that resolves it by returning an empty array is not wrong.
fn system_prompt(config: &Config, args: &CommitArgs, body: bool) -> String {
    let mut prompt = String::from(
        "You are an experienced programmer writing the commit message for a staged change.\n\n",
    );

    if config.candidates <= 1 {
        prompt.push_str(
            "Reply with the message and nothing else. No preamble, no code fences, no quotes. \
             The first line is the subject: imperative mood, no trailing full stop.",
        );
    } else {
        let _ = write!(
            prompt,
            "Propose {} different messages for this one change, in the `messages` array. Each \
             carries a `subject` and a `body`. Make them genuinely different in what they \
             emphasise, not rewordings of each other. The subject is in the imperative mood, \
             with no trailing full stop.",
            config.candidates
        );
    }

    let _ = write!(
        prompt,
        " Keep the subject under {} characters.",
        config.subject_max_len
    );

    match (body, config.candidates <= 1) {
        (true, true) => prompt.push_str(
            "\nThen a blank line, then a body explaining why the change was made, wrapped at 72 \
             characters. Leave the body out when the subject already says everything.",
        ),
        (true, false) => prompt.push_str(
            "\nThe body explains why the change was made, wrapped at 72 characters. Leave it as \
             an empty string when the subject already says everything.",
        ),
        (false, true) => prompt.push_str("\nWrite the subject only. No body."),
        (false, false) => prompt.push_str("\nLeave every body as an empty string."),
    }

    prompt.push_str(
        "\n\nMatch the conventions of the previous commits you are shown: their mood, their \
         prefixes, their width, whether they carry a body. Those examples outrank any habit of \
         your own. Do not invent a convention they do not use.",
    );

    if config.conventional_commits {
        prompt.push_str(
            "\n\nUse the conventional commits format: `type(scope): subject`, where type is one \
             of feat, fix, docs, style, refactor, perf, test, build, ci, chore or revert. The \
             scope is optional.",
        );
    }

    if let Some(kind) = &args.kind {
        let _ = write!(prompt, "\n\nThe type must be `{kind}`.");
    }

    if let Some(scope) = &args.scope {
        let _ = write!(prompt, "\n\nThe scope must be `{scope}`.");
    }

    if let Some(issue) = &args.issue {
        let _ = write!(prompt, "\n\nReference issue {issue} in the message.");
    }

    prompt
}

/// Context first, diff last. The stable part of the prompt sits at the front
/// so a provider that caches prefixes can reuse it between runs.
fn context_prompt(
    recent: &[git::PastCommit],
    touching: Option<&str>,
    template: Option<&str>,
    branch: Option<&str>,
    diff: &Diff,
) -> String {
    let mut prompt = String::new();

    if !recent.is_empty() {
        prompt.push_str("# Recent commits in this repository\n\n");

        for commit in recent {
            let _ = writeln!(prompt, "{}", commit.subject);

            if !commit.body.is_empty() {
                let _ = writeln!(prompt, "{}", commit.body);
            }

            prompt.push_str("---\n");
        }

        prompt.push('\n');
    }

    if let Some(touching) = touching {
        let _ = write!(
            prompt,
            "# Recent commits touching these same files\n\n{touching}\n\n"
        );
    }

    if let Some(branch) = branch {
        let _ = write!(prompt, "# Current branch\n\n{branch}\n\n");
    }

    if let Some(template) = template {
        let _ = write!(
            prompt,
            "# This repository's commit template, which states its convention outright\n\n{template}\n\n"
        );
    }

    let _ = write!(
        prompt,
        "# Every file in the staged change\n\n{}\n",
        diff.manifest()
    );

    // How hard to push depends on what is left. A model with some hunks can
    // judge whether the withheld ones matter; a model with none cannot, and
    // guessing from file names alone is how "update" gets written.
    if diff.included.trim().is_empty() {
        prompt.push_str(
            "\nNone of these hunks are below: every file was withheld. You cannot describe this \
             change from the file names, so call `search_diff` or `read_diff` and read enough of \
             it to say what actually changed before you answer.\n",
        );
    } else if diff.has_withheld() {
        prompt.push_str(
            "\nThe hunks of the withheld files are not below. Call `search_diff` or `read_diff` \
             if one of them looks like it might be the point of this commit rather than noise.\n",
        );
    }

    let _ = write!(prompt, "\n# The staged diff\n\n{}", diff.included);

    prompt
}

/// One round of generation. The single message path streams so the subject
/// appears early; the multi message path cannot, because a half-written JSON
/// object is not readable.
async fn generate(
    model: &Model,
    config: &Config,
    messages: Vec<async_openai::types::chat::ChatCompletionRequestMessage>,
    diff: &Diff,
    reporter: &mut Reporter,
) -> Result<Vec<Message>, Error> {
    if config.candidates <= 1 {
        let text = model.text(messages, Some(diff), reporter).await?;

        return Ok(vec![Message::parse(&text)]);
    }

    let schema = json!({
        "type": "object",
        "properties": {
            "messages": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string"},
                        "body": {"type": "string"}
                    },
                    "required": ["subject", "body"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["messages"],
        "additionalProperties": false
    });

    Ok(model
        .structured::<Candidates>(messages, "commit_messages", schema, Some(diff), reporter)
        .await?
        .messages)
}

#[cfg(test)]
mod tests {
    use super::Message;

    #[test]
    fn a_subject_and_body_split_on_the_blank_line() {
        let message = Message::parse(
            "fix(git): read the staged diff\n\nThe old code sent\n`git diff HEAD`.\n",
        );

        assert_eq!(message.subject, "fix(git): read the staged diff");
        assert_eq!(message.body, "The old code sent\n`git diff HEAD`.");
        assert_eq!(
            message.render(),
            "fix(git): read the staged diff\n\nThe old code sent\n`git diff HEAD`."
        );
    }

    #[test]
    fn a_subject_on_its_own_renders_without_a_trailing_blank_line() {
        let message = Message::parse("  bump the lockfile  ");

        assert_eq!(message.subject, "bump the lockfile");
        assert_eq!(message.render(), "bump the lockfile");
    }
}
