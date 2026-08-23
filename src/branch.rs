use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cli::{BranchArgs, GlobalArgs},
    config::Config,
    git,
    model::{self, Model},
    output::{Event, Reporter},
    Error,
};

/// One proposed branch name, with the reason it fits this repository.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Suggestion {
    pub name: String,
    pub reason: String,
}

#[derive(Deserialize)]
struct Suggestions {
    candidates: Vec<Suggestion>,
}

pub async fn run(
    args: &BranchArgs,
    global: &GlobalArgs,
    reporter: &mut Reporter,
) -> Result<(), Error> {
    let root = git::repo_root().await?;
    let config = Config::load(Some(&root), global)?;

    let Some(current) = git::branch_name().await else {
        return Err(Error::DetachedHead);
    };

    let onto = match &args.onto {
        Some(onto) => onto.clone(),
        None => git::default_branch().await,
    };

    if onto.trim_start_matches("origin/") == current {
        return Err(Error::OnDefaultBranch(current));
    }

    reporter.start("reading the branch");

    let base = git::merge_base(&onto).await?;

    let (stat, commits, branches) = tokio::join!(
        git::diff_stat_since(&base),
        git::commits_since(&base),
        git::recent_branches(50),
    );

    let commits = commits?;

    if commits.is_empty() {
        return Err(Error::NoCommitsOnBranch(current, onto));
    }

    let model = Model::new(&config)?;

    let messages = vec![
        model::system(
            "You name git branches. You are given the branch's diff, its commits, and the branch \
             names already in use in this repository.\n\nPropose three names. Match the \
             convention the existing names show: their prefixes, their separators, their case, \
             whether they carry a ticket id. That convention outranks any habit of your own. Do \
             not propose a name that already exists. Give one short reason per name, naming the \
             convention it follows.",
        ),
        model::user(prompt(&current, &onto, &stat?, &commits, &branches)),
    ];

    reporter.step("thinking of names");

    let schema = json!({
        "type": "object",
        "properties": {
            "candidates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "reason": {"type": "string"}
                    },
                    "required": ["name", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["candidates"],
        "additionalProperties": false
    });

    let mut candidates = model
        .structured::<Suggestions>(messages, "branch_names", schema, None, reporter)
        .await?
        .candidates;

    reporter.stop();

    // A name that already exists cannot be used, whatever the model thought.
    let mut keep = Vec::with_capacity(candidates.len());

    for candidate in candidates.drain(..) {
        if !git::ref_exists(&candidate.name).await {
            keep.push(candidate);
        }
    }

    if keep.is_empty() {
        return Err(Error::NoUsableName);
    }

    reporter.event(Event::Branch {
        candidates: keep.clone(),
    });

    if global.print || !args.rename {
        for candidate in &keep {
            reporter.payload(&format!("{}  {}", candidate.name, candidate.reason));
        }

        if !args.rename {
            return Ok(());
        }
    }

    let chosen = choose(keep, reporter)?;

    git::rename_branch(&chosen.name).await?;

    reporter.event(Event::Renamed {
        from: current.clone(),
        to: chosen.name.clone(),
    });

    // The remote is never touched, so a pushed branch gets the commands
    // rather than a surprise.
    if let Some(upstream) = git::upstream().await {
        let remote = upstream.split_once('/').map_or("origin", |(name, _)| name);

        eprintln!(
            "\n{current} was pushed to {upstream}. To move it there as well:\n  \
             git push -u {remote} {}\n  git push {remote} --delete {current}",
            chosen.name
        );
    }

    Ok(())
}

fn choose(mut candidates: Vec<Suggestion>, reporter: &Reporter) -> Result<Suggestion, Error> {
    if candidates.len() == 1 || !reporter.is_human() {
        return Ok(candidates.remove(0));
    }

    let labels: Vec<String> = candidates
        .iter()
        .map(|candidate| format!("{}  ({})", candidate.name, candidate.reason))
        .collect();

    let chosen = inquire::Select::new("Rename the branch to?", labels)
        .with_page_size(candidates.len())
        .raw_prompt()
        .map_err(|_| Error::Aborted)?;

    Ok(candidates.remove(chosen.index))
}

fn prompt(
    current: &str,
    onto: &str,
    stat: &str,
    commits: &[String],
    branches: &[String],
) -> String {
    let mut prompt = String::new();

    let _ = write!(
        prompt,
        "# Branch names already in this repository\n\n{}\n\n",
        branches.join("\n")
    );

    let _ = write!(prompt, "# The current name\n\n{current}\n\n");

    let _ = write!(
        prompt,
        "# Commits on this branch, against {onto}\n\n{}\n\n",
        commits.join("\n")
    );

    let _ = write!(prompt, "# What the branch changes\n\n{stat}");

    prompt
}
