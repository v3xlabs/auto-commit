use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cli::{GlobalArgs, SquashArgs},
    config::Config,
    git,
    model::{self, Model},
    output::{Event, Reporter},
    Error,
};

/// One group of commits that belong together, with the message the squashed
/// commit would carry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Group {
    pub commits: Vec<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Deserialize)]
struct Plan {
    groups: Vec<Group>,
}

pub async fn run(
    args: &SquashArgs,
    global: &GlobalArgs,
    reporter: &mut Reporter,
) -> Result<(), Error> {
    let root = git::repo_root().await?;
    let config = Config::load(Some(&root), global)?;

    let onto = match &args.onto {
        Some(onto) => onto.clone(),
        None => git::default_branch().await,
    };

    reporter.start("reading the branch");

    let base = git::merge_base(&onto).await?;
    let commits = git::commits_since(&base).await?;

    if commits.len() < 2 {
        return Err(Error::NothingToSquash(commits.len()));
    }

    let stat = git::diff_stat_since(&base).await?;

    let model = Model::new(&config)?;

    let messages = vec![
        model::system(
            "You group a branch's commits into the smallest number of commits that still tell \
             the story of the change.\n\nEach group lists the commits it absorbs, in order, and \
             carries one message: an imperative subject, then a body. Commits that only fix an \
             earlier commit on this branch belong in that commit's group. Do not merge two \
             groups that change unrelated things just to reach a smaller number.\n\nMatch the \
             style of the commit subjects you are shown.",
        ),
        model::user(prompt(&onto, &commits, &stat)),
    ];

    reporter.step("grouping the commits");

    let schema = json!({
        "type": "object",
        "properties": {
            "groups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "commits": {"type": "array", "items": {"type": "string"}},
                        "subject": {"type": "string"},
                        "body": {"type": "string"}
                    },
                    "required": ["commits", "subject", "body"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["groups"],
        "additionalProperties": false
    });

    let groups = model
        .structured::<Plan>(messages, "squash_plan", schema, None, reporter)
        .await?
        .groups;

    reporter.stop();

    reporter.event(Event::Squash {
        groups: groups.clone(),
    });

    reporter.payload(&render(&groups, &base, commits.len()));

    Ok(())
}

/// Read only, by design. The command prints what it would do and the command
/// that would do it, and never rewrites history itself.
fn render(groups: &[Group], base: &str, commits: usize) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "{commits} commits would become {}.\n", groups.len());

    for (index, group) in groups.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", index + 1, group.subject);

        for commit in &group.commits {
            let _ = writeln!(out, "     {commit}");
        }

        if !group.body.trim().is_empty() {
            let _ = writeln!(out, "\n   {}", group.body.trim().replace('\n', "\n   "));
        }

        let _ = writeln!(out);
    }

    if groups.len() == 1 {
        let _ = writeln!(
            out,
            "To apply:\n  git reset --soft {base}\n  git commit -m {:?}",
            groups[0].subject
        );
    } else {
        let _ = writeln!(out, "To apply:\n  git rebase -i {base}");
    }

    out
}

fn prompt(onto: &str, commits: &[String], stat: &str) -> String {
    let mut prompt = String::new();

    let _ = write!(
        prompt,
        "# Commits on this branch, against {onto}, oldest last\n\n{}\n\n",
        commits.join("\n")
    );

    let _ = write!(prompt, "# What the branch changes overall\n\n{stat}");

    prompt
}
