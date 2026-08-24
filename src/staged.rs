use std::{collections::BTreeMap, io, time::Duration};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal, ExecutableCommand,
};

use crate::{
    config::Config,
    git::{self, Diff, FileChange, FileState, GitError},
    screen::{self, line, Ink, Line},
    Error,
};

/// How often the staging area is read again while the screen waits. Slow
/// enough to stay out of the way of a `git add -p` running in the next
/// terminal, fast enough that the panel has caught up by the time you look
/// back at it.
const REFRESH: Duration = Duration::from_millis(500);

/// Shows what is staged and waits for you to say go. The staging area is read
/// again on every beat, so the tree on screen is what the message would be
/// written from, not what was there when the tool started.
pub async fn watch(config: &Config, diff: Diff) -> Result<Diff, Error> {
    let mut stderr = io::stderr();
    let mut staged = Some(diff);
    let mut fingerprint = git::staged_fingerprint().await;
    let mut drawn = 0u16;

    terminal::enable_raw_mode()?;
    stderr.execute(cursor::Hide)?;

    let outcome = loop {
        screen::paint(&mut stderr, &frame(staged.as_ref()), &mut drawn)?;

        if let Some(KeyEvent {
            code, modifiers, ..
        }) = wait(REFRESH)?
        {
            if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                break Err(Error::Aborted);
            }

            match code {
                // Nothing staged has nothing to write about, so the key that
                // starts the run does nothing until something is.
                KeyCode::Char('y') | KeyCode::Enter => {
                    if let Some(diff) = staged.take() {
                        break Ok(diff);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => break Err(Error::Aborted),
                _ => {}
            }
        }

        let current = git::staged_fingerprint().await;

        if current != fingerprint {
            fingerprint = current;

            staged = match git::staged_diff(config).await {
                Ok(fresh) => Some(fresh),
                Err(GitError::NothingStaged) => None,
                Err(error) => break Err(error.into()),
            };
        }
    };

    screen::erase(&mut stderr, drawn)?;
    stderr.execute(cursor::Show)?;
    terminal::disable_raw_mode()?;

    outcome
}

/// Blocks for a keystroke until the next refresh falls due. Any other
/// terminal event also ends the wait, so a resize is redrawn at once rather
/// than half a second later.
fn wait(window: Duration) -> io::Result<Option<KeyEvent>> {
    if !event::poll(window)? {
        return Ok(None);
    }

    Ok(match event::read()? {
        Event::Key(key) => Some(key),
        _ => None,
    })
}

fn frame(staged: Option<&Diff>) -> Vec<Line> {
    let (width, height) = screen::size();

    // The header, the blank line under it, and the three rows of footer.
    let budget = height.saturating_sub(6);

    let mut lines = match staged {
        Some(diff) => {
            let (added, deleted) = diff.totals();

            let mut lines = vec![
                line(format!("  {} staged", count(diff.files.len())), Ink::Bold)
                    .then(format!("   +{added}"), Ink::Added)
                    .then(format!(" -{deleted}"), Ink::Removed),
                line("", Ink::Plain),
            ];

            lines.extend(tree(&diff.files, width, budget));
            lines
        }
        None => vec![
            line("  nothing staged", Ink::Bold),
            line("", Ink::Plain),
            line(
                "  stage what belongs in this commit with `git add -p`",
                Ink::Dim,
            ),
        ],
    };

    lines.push(line("", Ink::Plain));

    lines.push(match staged {
        Some(_) => line("  y  write the message   q  cancel", Ink::Accent),
        None => line("  q  cancel", Ink::Accent),
    });

    lines.push(line("  watching the staging area for changes", Ink::Dim));

    lines
}

/// One row per file and per directory that holds them, in the order they are
/// drawn. The count is kept rather than the path, because the row it lands on
/// needs the whole change to write its columns.
struct Row {
    label: String,
    change: Option<usize>,
}

#[derive(Default)]
struct Node {
    directories: BTreeMap<String, Node>,
    files: BTreeMap<String, usize>,
}

fn build(files: &[FileChange]) -> Node {
    let mut root = Node::default();

    for (index, file) in files.iter().enumerate() {
        let mut parts = file.path.split('/').peekable();
        let mut node = &mut root;

        while let Some(part) = parts.next() {
            match parts.peek() {
                Some(_) => node = node.directories.entry(part.to_owned()).or_default(),
                None => {
                    node.files.insert(part.to_owned(), index);
                }
            }
        }
    }

    root
}

fn walk(node: &Node, prefix: &str, out: &mut Vec<Row>) {
    let total = node.directories.len() + node.files.len();

    for (index, (name, child)) in node.directories.iter().enumerate() {
        let (branch, carry) = connectors(index + 1 == total);
        let (name, child) = collapse(name, child);

        out.push(Row {
            label: format!("{prefix}{branch}{name}/"),
            change: None,
        });

        walk(child, &format!("{prefix}{carry}"), out);
    }

    for (index, (name, change)) in node.files.iter().enumerate() {
        let (branch, _) = connectors(node.directories.len() + index + 1 == total);

        out.push(Row {
            label: format!("{prefix}{branch}{name}"),
            change: Some(*change),
        });
    }
}

/// A directory that holds one directory and nothing else is written as one
/// path. Without this, `src/main/java/com` spends four rows saying nothing.
fn collapse<'a>(name: &str, node: &'a Node) -> (String, &'a Node) {
    let mut name = name.to_owned();
    let mut node = node;

    while node.files.is_empty() && node.directories.len() == 1 {
        let Some((child, below)) = node.directories.iter().next() else {
            break;
        };

        name = format!("{name}/{child}");
        node = below;
    }

    (name, node)
}

fn connectors(last: bool) -> (&'static str, &'static str) {
    if last {
        ("└─ ", "   ")
    } else {
        ("├─ ", "│  ")
    }
}

/// The tree, trimmed to the rows it was given. A staged change of four
/// hundred files is not worth a screen, and scrolling one is not worth the
/// keys it would take.
fn tree(files: &[FileChange], width: usize, budget: usize) -> Vec<Line> {
    let mut rows = Vec::new();
    walk(&build(files), "", &mut rows);

    if rows.len() > budget {
        // One row goes to saying what is not shown.
        rows.truncate(budget.saturating_sub(1));
    }

    let shown = rows.iter().filter(|row| row.change.is_some()).count();

    // Long paths push the counts off the right edge, so the column they line
    // up on stops well before it.
    let column = rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(22));

    let mut lines: Vec<Line> = rows
        .iter()
        .map(|row| {
            let pad = " ".repeat(column.saturating_sub(row.label.chars().count()) + 2);
            let label = format!("  {}{pad}", row.label);

            let Some(change) = row.change.and_then(|index| files.get(index)) else {
                return line(label, Ink::Dim);
            };

            let counts = match (change.added, change.deleted) {
                (Some(added), Some(deleted)) => line(label, Ink::Plain)
                    .then(format!("{:>6}", format!("+{added}")), Ink::Added)
                    .then(format!(" {:<6}", format!("-{deleted}")), Ink::Removed),
                _ => line(label, Ink::Plain).then(format!("{:>13}", "binary"), Ink::Dim),
            };

            counts.then(note(&change.state), Ink::Dim)
        })
        .collect();

    if shown < files.len() {
        lines.push(line(
            format!("  and {} not shown", count(files.len() - shown)),
            Ink::Dim,
        ));
    }

    lines
}

/// Why a file's hunks will not reach the model, when they will not. A
/// withheld file still shapes the message, so it is worth a word here.
fn note(state: &FileState) -> &'static str {
    match state {
        FileState::Included | FileState::Binary => "",
        FileState::Truncated { .. } => "truncated",
        FileState::Withheld { .. } => "withheld",
    }
}

fn count(files: usize) -> String {
    if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{files} files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(paths: &[&str]) -> Vec<FileChange> {
        paths
            .iter()
            .map(|path| FileChange {
                path: (*path).to_owned(),
                added: Some(1),
                deleted: Some(0),
                state: FileState::Included,
            })
            .collect()
    }

    fn labels(files: &[FileChange]) -> Vec<String> {
        let mut rows = Vec::new();
        walk(&build(files), "", &mut rows);

        rows.into_iter().map(|row| row.label).collect()
    }

    #[test]
    fn files_sit_under_the_directory_that_holds_them() {
        assert_eq!(
            labels(&changed(&["src/git.rs", "src/main.rs", "README.md"])),
            vec![
                "├─ src/".to_owned(),
                "│  ├─ git.rs".to_owned(),
                "│  └─ main.rs".to_owned(),
                "└─ README.md".to_owned(),
            ]
        );
    }

    #[test]
    fn a_directory_holding_only_a_directory_is_written_as_one_path() {
        assert_eq!(
            labels(&changed(&["nix/modules/home.nix"])),
            vec!["└─ nix/modules/".to_owned(), "   └─ home.nix".to_owned()]
        );
    }

    /// A frame taller than the terminal scrolls, and the row count used to
    /// erase it is then wrong, so this bound is what keeps the screen still.
    #[test]
    fn the_tree_never_takes_more_rows_than_it_was_given() {
        let files = changed(&["a.rs", "src/b.rs", "src/c.rs", "src/deep/d.rs"]);

        for budget in 1..8 {
            assert!(tree(&files, 80, budget).len() <= budget, "budget {budget}");
        }
    }
}
