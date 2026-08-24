use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::{io::AsyncWriteExt, process::Command};

use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run git: {0}")]
    Spawn(#[from] io::Error),

    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },

    #[error("not inside a git repository. Run this from a repository, or `git init` one")]
    NotARepository,

    #[error("nothing is staged. Stage what belongs in this commit with `git add -p`, then run auto-commit again")]
    NothingStaged,

    #[error("{0:?} is not a valid exclude pattern: {1}")]
    BadGlob(String, globset::Error),
}

/// Runs a git command and returns its stdout, or the error git printed.
async fn git<I, S>(args: I) -> Result<String, GitError>
where
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<OsStr>,
{
    let output = Command::new("git").args(args.clone()).output().await?;

    if !output.status.success() {
        let command = args
            .into_iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        return Err(GitError::Failed {
            command,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like `git`, but an exit code is an answer rather than a failure. Used for
/// the queries that are allowed to come back empty, such as a config lookup.
async fn git_opt<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S> + Clone,
    S: AsRef<OsStr>,
{
    let text = git(args).await.ok()?;
    let trimmed = text.trim();

    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub async fn repo_root() -> Result<PathBuf, GitError> {
    let root = git(["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| GitError::NotARepository)?;

    Ok(PathBuf::from(root.trim()))
}

pub async fn branch_name() -> Option<String> {
    git_opt(["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|name| name != "HEAD")
}

/// The commit message template this repository asks for, if it has one. A
/// template states the convention outright, so it outranks any example.
pub async fn commit_template(root: &Path) -> Option<String> {
    let configured = git_opt(["config", "commit.template"]).await;

    let path = match configured {
        Some(path) if path.starts_with('/') => PathBuf::from(path),
        Some(path) => root.join(path),
        None => root.join(".gitmessage"),
    };

    tokio::fs::read_to_string(path)
        .await
        .ok()
        .filter(|text| !text.trim().is_empty())
}

pub async fn recent_commits(count: usize) -> Option<String> {
    git_opt([
        "log".to_owned(),
        format!("-n{count}"),
        "--format=%s%n%b%n---".to_owned(),
    ])
    .await
}

/// Commits touching the same files as the staged change. These carry the
/// conventions of this part of the tree, which a repository-wide sample
/// dilutes.
pub async fn commits_touching(paths: &[String], count: usize) -> Option<String> {
    if paths.is_empty() {
        return None;
    }

    let mut args = vec![
        "log".to_owned(),
        format!("-n{count}"),
        "--format=%s".to_owned(),
        "--".to_owned(),
    ];
    args.extend(paths.iter().cloned());

    git_opt(args).await
}

/// Files changed in the working tree but not staged, each with its porcelain
/// status, so the "nothing staged" message can say what is there instead of
/// only what is missing.
///
/// Read without trimming: the first column of a porcelain line is a space for
/// an unstaged change, and trimming it shifts every field of that line.
pub async fn unstaged_files() -> Vec<(String, String)> {
    let Ok(porcelain) = git(["status", "--porcelain"]).await else {
        return Vec::new();
    };

    porcelain
        .lines()
        .filter_map(|line| {
            let (status, path) = line.split_at_checked(3)?;

            // A rename reads `R  old -> new`; the new name is the useful half.
            let path = path.rsplit(" -> ").next().unwrap_or(path);

            Some((status.trim().to_owned(), path.to_owned()))
        })
        .collect()
}

pub async fn default_branch() -> String {
    if let Some(head) = git_opt(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await {
        return head;
    }

    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if git(["rev-parse", "--verify", "--quiet", candidate])
            .await
            .is_ok()
        {
            return candidate.to_owned();
        }
    }

    "HEAD".to_owned()
}

pub async fn merge_base(onto: &str) -> Result<String, GitError> {
    Ok(git(["merge-base", "HEAD", onto]).await?.trim().to_owned())
}

/// Recent branch names, local and remote. Without these a suggestion invents
/// a convention instead of matching the one in use.
pub async fn recent_branches(count: usize) -> Vec<String> {
    let listing = git_opt([
        "for-each-ref".to_owned(),
        "--sort=-committerdate".to_owned(),
        format!("--count={count}"),
        "--format=%(refname:short)".to_owned(),
        "refs/heads".to_owned(),
        "refs/remotes".to_owned(),
    ])
    .await
    .unwrap_or_default();

    listing.lines().map(str::to_owned).collect()
}

pub async fn ref_exists(name: &str) -> bool {
    git(["rev-parse", "--verify", "--quiet", name])
        .await
        .is_ok()
}

pub async fn rename_branch(new: &str) -> Result<(), GitError> {
    git(["branch", "-m", new]).await.map(|_| ())
}

pub async fn upstream() -> Option<String> {
    git_opt(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).await
}

pub async fn commits_since(base: &str) -> Result<Vec<String>, GitError> {
    let log = git(["log", "--format=%H %s", &format!("{base}..HEAD")]).await?;

    Ok(log.lines().map(str::to_owned).collect())
}

pub async fn diff_stat_since(base: &str) -> Result<String, GitError> {
    git(["diff", "--stat", base, "HEAD"]).await
}

/// One entry per changed file. Every file reaches the model as one of these
/// lines, whether or not its hunks do.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub state: FileState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    Included,
    Truncated { kept: usize, total: usize },
    Withheld { bytes: usize },
    Binary,
}

impl FileChange {
    fn describe(&self) -> String {
        match &self.state {
            FileState::Included => "included".to_owned(),
            FileState::Truncated { kept, total } => {
                format!("included, first {} of {}", kb(*kept), kb(*total))
            }
            FileState::Withheld { bytes } => format!("withheld, {} of diff", kb(*bytes)),
            FileState::Binary => "binary, no diff".to_owned(),
        }
    }
}

fn kb(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// The staged change, split into what the model is shown up front and what it
/// has to ask for. Nothing is hidden: every changed file appears in the
/// manifest, so the model can see that a lockfile moved even when its hunks
/// were withheld.
#[derive(Debug)]
pub struct Diff {
    pub files: Vec<FileChange>,
    pub included: String,
    withheld: BTreeMap<String, String>,
}

impl Diff {
    pub fn manifest(&self) -> String {
        let width = self
            .files
            .iter()
            .map(|file| file.path.len())
            .max()
            .unwrap_or(0);

        let mut out = String::new();

        for file in &self.files {
            let counts = match (file.added, file.deleted) {
                (Some(added), Some(deleted)) => format!("+{added} -{deleted}"),
                _ => "binary".to_owned(),
            };

            let _ = writeln!(
                out,
                "{:width$}  {:>12}  {}",
                file.path,
                counts,
                file.describe()
            );
        }

        out
    }

    pub fn withheld_paths(&self) -> Vec<String> {
        self.withheld.keys().cloned().collect()
    }

    pub fn included_paths(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|file| !matches!(file.state, FileState::Withheld { .. }))
            .map(|file| file.path.clone())
            .collect()
    }

    /// Lines added and removed across the whole staged change. Binary files
    /// have no counts, so they contribute nothing rather than zero.
    pub fn totals(&self) -> (u64, u64) {
        self.files.iter().fold((0, 0), |(added, deleted), file| {
            (
                added + file.added.unwrap_or(0),
                deleted + file.deleted.unwrap_or(0),
            )
        })
    }

    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }

    pub fn has_withheld(&self) -> bool {
        !self.withheld.is_empty()
    }

    /// A slice of one withheld file's diff, so a large lockfile change can be
    /// read a few hundred lines at a time.
    pub fn read_withheld(&self, path: &str, offset: usize, limit: usize) -> String {
        let Some(text) = self.withheld.get(path) else {
            return format!(
                "no withheld diff for {path:?}. Withheld files: {}",
                self.withheld_paths().join(", ")
            );
        };

        let lines: Vec<&str> = text.lines().collect();
        let end = offset.saturating_add(limit).min(lines.len());

        if offset >= lines.len() {
            return format!("{path} has {} lines, {offset} is past the end", lines.len());
        }

        format!(
            "{path} lines {offset}..{end} of {}\n{}",
            lines.len(),
            lines[offset..end].join("\n")
        )
    }

    /// Dispatches one retrieval tool call. The model names the tool; the
    /// diff decides what it can see.
    pub fn call_tool(&self, name: &str, arguments: &serde_json::Value, budget: usize) -> String {
        match name {
            "read_diff" => self.read_withheld(
                arguments["path"].as_str().unwrap_or_default(),
                arguments["offset"].as_u64().unwrap_or(0) as usize,
                arguments["limit"].as_u64().unwrap_or(200) as usize,
            ),
            "search_diff" => {
                self.search_withheld(arguments["pattern"].as_str().unwrap_or_default(), budget)
            }
            other => format!("no such tool: {other}"),
        }
    }

    /// Matching lines across every withheld diff. A lockfile change that
    /// matters usually matters because of one named package, so a search finds
    /// it in one call where paging would take twenty.
    pub fn search_withheld(&self, pattern: &str, budget: usize) -> String {
        let regex = match regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => return format!("{pattern:?} is not a valid regular expression: {error}"),
        };

        let mut out = String::new();
        let mut matches = 0;

        for (path, text) in &self.withheld {
            for (number, line) in text.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }

                matches += 1;

                if out.len() < budget {
                    let _ = writeln!(out, "{path}:{number}: {}", line.trim_end());
                }
            }
        }

        if matches == 0 {
            return format!("no match for {pattern:?} in any withheld diff");
        }

        if out.len() >= budget {
            let _ = writeln!(out, "... {matches} matches in total, output truncated");
        }

        out
    }
}

fn glob_set(patterns: &[String]) -> Result<GlobSet, GitError> {
    let mut builder = GlobSetBuilder::new();

    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| GitError::BadGlob(pattern.clone(), error))?;

        builder.add(glob);
    }

    builder
        .build()
        .map_err(|error| GitError::BadGlob(patterns.join(","), error))
}

/// Splits a unified diff into one chunk per file, keyed by the path in the
/// `diff --git` header.
fn split_by_file(diff: &str) -> BTreeMap<String, String> {
    let mut chunks = BTreeMap::new();
    let mut path: Option<String> = None;
    let mut current = String::new();

    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            if let Some(previous) = path.take() {
                chunks.insert(previous, std::mem::take(&mut current));
            }

            path = header.split_once(" b/").map(|(_, to)| to.to_owned());
        }

        if path.is_some() {
            current.push_str(line);
            current.push('\n');
        }
    }

    if let Some(last) = path {
        chunks.insert(last, current);
    }

    chunks
}

fn parse_numstat(numstat: &str) -> Vec<(String, Option<u64>, Option<u64>)> {
    numstat
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let added = fields.next()?;
            let deleted = fields.next()?;
            let path = fields.next()?;

            Some((path.to_owned(), added.parse().ok(), deleted.parse().ok()))
        })
        .collect()
}

/// Reads the staged change and decides what fits in the prompt. Files are
/// dropped from the prompt body, never from the manifest.
pub async fn staged_diff(config: &Config) -> Result<Diff, GitError> {
    let (numstat, diff) = tokio::try_join!(
        git(["diff", "--staged", "--numstat"]),
        git(["diff", "--staged", "-U3"]),
    )?;

    build_diff(config, &numstat, &diff)
}

/// A cheap stand-in for the staged change, used to notice that the index has
/// moved without reading the whole diff again. `--raw` names both blob ids,
/// so a file edited in place changes this as surely as one newly added.
pub async fn staged_fingerprint() -> String {
    git(["diff", "--staged", "--raw"]).await.unwrap_or_default()
}

/// The same split applied to a diff that did not come from git, for
/// `--stdin-diff`. Callers that supply their own diff get no numstat, so the
/// counts come from the hunks.
pub fn diff_from_text(config: &Config, diff: &str) -> Result<Diff, GitError> {
    let numstat = split_by_file(diff)
        .into_iter()
        .map(|(path, chunk)| {
            let added = chunk
                .lines()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                .count();

            let deleted = chunk
                .lines()
                .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                .count();

            format!("{added}\t{deleted}\t{path}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    build_diff(config, &numstat, diff)
}

fn build_diff(config: &Config, numstat: &str, diff: &str) -> Result<Diff, GitError> {
    let entries = parse_numstat(numstat);

    if entries.is_empty() {
        return Err(GitError::NothingStaged);
    }

    let excluded = glob_set(&config.exclude)?;
    let mut chunks = split_by_file(diff);

    let mut files = Vec::with_capacity(entries.len());
    let mut withheld = BTreeMap::new();
    let mut included = String::new();

    for (path, added, deleted) in entries {
        let chunk = chunks.remove(&path).unwrap_or_default();

        // Binary files have no diff text at all, so there is nothing to
        // withhold and nothing to retrieve.
        if added.is_none() {
            files.push(FileChange {
                path,
                added,
                deleted,
                state: FileState::Binary,
            });

            continue;
        }

        // A single file larger than the whole budget is truncated rather than
        // withheld, so the model always sees the start of every change it is
        // meant to describe.
        let kept = if chunk.len() > config.max_file_diff_bytes {
            floor_to_line(&chunk, config.max_file_diff_bytes)
        } else {
            chunk.len()
        };

        if excluded.is_match(&path) || included.len() + kept > config.max_diff_bytes {
            files.push(FileChange {
                path: path.clone(),
                added,
                deleted,
                state: FileState::Withheld { bytes: chunk.len() },
            });

            withheld.insert(path, chunk);
            continue;
        }

        included.push_str(&chunk[..kept]);

        let state = if kept < chunk.len() {
            FileState::Truncated {
                kept,
                total: chunk.len(),
            }
        } else {
            FileState::Included
        };

        files.push(FileChange {
            path,
            added,
            deleted,
            state,
        });
    }

    Ok(Diff {
        files,
        included,
        withheld,
    })
}

/// Cutting a diff mid-line produces a hunk that reads as corrupt, so a
/// truncation always lands on a line boundary.
fn floor_to_line(text: &str, limit: usize) -> usize {
    text[..limit.min(text.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

pub struct CommitOptions {
    pub review: bool,
    pub amend: bool,
    pub signoff: bool,
    pub gpg_sign: bool,
    pub no_verify: bool,
}

/// Writes the commit, letting git run its own hooks and open its own editor.
pub async fn commit(message: &str, options: &CommitOptions) -> Result<String, GitError> {
    let mut args = vec!["commit".to_owned(), "-F".to_owned(), "-".to_owned()];

    if options.review {
        args.push("-e".to_owned());
    }
    if options.amend {
        args.push("--amend".to_owned());
    }
    if options.signoff {
        args.push("--signoff".to_owned());
    }
    if options.gpg_sign {
        args.push("--gpg-sign".to_owned());
    }
    if options.no_verify {
        args.push("--no-verify".to_owned());
    }

    let mut child = Command::new("git")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        return Err(GitError::Failed {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(git(["rev-parse", "--short", "HEAD"])
        .await?
        .trim()
        .to_owned())
}

/// The repository's git directory, which is not always `<root>/.git`: a
/// worktree or a submodule puts it elsewhere.
pub async fn git_dir() -> Result<PathBuf, GitError> {
    let dir = git(["rev-parse", "--absolute-git-dir"]).await?;

    Ok(PathBuf::from(dir.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            max_diff_bytes: 400,
            max_file_diff_bytes: 200,
            ..Config::default()
        }
    }

    fn diff_for(path: &str, lines: usize) -> String {
        let mut text =
            format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n");

        for index in 0..lines {
            text.push_str(&format!("+line {index}\n"));
        }

        text
    }

    #[test]
    fn a_withheld_file_still_reaches_the_manifest() {
        let diff = format!(
            "{}{}",
            diff_for("src/main.rs", 2),
            diff_for("pnpm-lock.yaml", 3)
        );
        let numstat = "2\t0\tsrc/main.rs\n3\t0\tpnpm-lock.yaml";

        let plan = build_diff(&config(), numstat, &diff).unwrap();
        let manifest = plan.manifest();

        assert!(manifest.contains("pnpm-lock.yaml"));
        assert!(manifest.contains("withheld"));
        assert!(manifest.contains("+3 -0"));

        assert!(plan.included.contains("src/main.rs"));
        assert!(!plan.included.contains("pnpm-lock.yaml"));
        assert_eq!(plan.withheld_paths(), vec!["pnpm-lock.yaml".to_owned()]);
    }

    #[test]
    fn a_withheld_file_can_be_searched_and_read() {
        let diff = diff_for("yarn.lock", 40);
        let plan = build_diff(&config(), "40\t0\tyarn.lock", &diff).unwrap();

        let hit = plan.search_withheld("line 7$", 4096);
        assert!(hit.contains("yarn.lock:"), "{hit}");
        assert!(hit.contains("+line 7"), "{hit}");

        let miss = plan.search_withheld("nothing here", 4096);
        assert!(miss.starts_with("no match"), "{miss}");

        let slice = plan.read_withheld("yarn.lock", 4, 2);
        assert!(slice.contains("+line 0"), "{slice}");
        assert!(!slice.contains("+line 3"), "{slice}");

        let wrong = plan.read_withheld("not-a-file", 0, 10);
        assert!(wrong.starts_with("no withheld diff"), "{wrong}");
    }

    #[test]
    fn an_oversized_file_is_truncated_on_a_line_boundary() {
        let plan = build_diff(&config(), "60\t0\tbig.rs", &diff_for("big.rs", 60)).unwrap();

        assert!(matches!(plan.files[0].state, FileState::Truncated { .. }));

        assert!(plan.included.ends_with('\n'));
        assert!(plan.included.len() <= 200);
        assert!(plan.manifest().contains("included, first"));
    }

    #[test]
    fn a_binary_file_is_named_but_has_nothing_to_retrieve() {
        let plan = build_diff(&config(), "-\t-\tlogo.png", "").unwrap();

        assert_eq!(plan.files[0].state, FileState::Binary);
        assert!(plan.manifest().contains("binary"));
        assert!(!plan.has_withheld());
    }

    #[test]
    fn an_empty_staging_area_is_an_error_rather_than_an_empty_prompt() {
        assert!(matches!(
            build_diff(&config(), "", ""),
            Err(GitError::NothingStaged)
        ));
    }
}
