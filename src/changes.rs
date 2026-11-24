//! What changed in a project, and a review of it: git's view of the working tree against
//! HEAD (new files included), shown as a list of files and as stacked diffs, each file with a
//! short summary of the change written by a cheap model.

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, bail};
use gpui_kit::component::diff::{DiffFile, DiffFileStatus, DiffState};
use gpui_kit::*;
use serde_json::{Value, json};

/// Git's hash of the empty tree: the base for a repository with no commits yet.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// New files bigger than this are listed by git but left out of the diff.
const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;
const MAX_UNTRACKED_FILES: usize = 200;
/// How much of each file's patch, and of all of them, goes to the summarizer.
const SUMMARY_FILE_CHARS: usize = 6_000;
const SUMMARY_TOTAL_CHARS: usize = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeKind {
    pub fn letter(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub additions: usize,
    pub deletions: usize,
    /// This file's part of the patch, for summaries.
    pub patch: String,
}

impl FileChange {
    /// Identifies this version of the change, so a summary is reused until the file changes again.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.path.hash(&mut hasher);
        self.patch.hash(&mut hasher);
        hasher.finish()
    }
}

/// A snapshot of a project's uncommitted changes.
#[derive(Clone, Default)]
pub struct Changes {
    pub is_repo: bool,
    pub files: Vec<FileChange>,
    /// Parsed for the diff view, in the same order as `files`.
    pub diff_files: Vec<DiffFile>,
}

impl Changes {
    pub fn additions(&self) -> usize {
        self.files.iter().map(|f| f.additions).sum()
    }
    pub fn deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Command::new("git").arg("-C").arg(root).args(args).stdin(Stdio::null()).output().context("could not run git")
}

/// Read the project's changes. Blocking: run it off the UI thread.
pub fn collect(root: &Path) -> anyhow::Result<Changes> {
    let inside = git(root, &["rev-parse", "--is-inside-work-tree"])?;
    if !inside.status.success() {
        return Ok(Changes::default());
    }
    let has_head = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?.status.success();
    let base = if has_head { "HEAD" } else { EMPTY_TREE };

    let tracked = git(root, &["diff", "--no-color", "--no-ext-diff", "-M", base, "--"])?;
    if !tracked.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&tracked.stderr).trim());
    }
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();

    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in
        String::from_utf8_lossy(&untracked.stdout).split('\0').filter(|p| !p.is_empty()).take(MAX_UNTRACKED_FILES)
    {
        let size = std::fs::metadata(root.join(path)).map(|m| m.len()).unwrap_or(0);
        if size > MAX_UNTRACKED_BYTES {
            continue;
        }
        // Exit status 1 means "there are differences", which is always the case here.
        let out = git(root, &["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null", path])?;
        patch.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    from_patch(&patch)
}

/// Build [`Changes`] from a unified git patch.
pub fn from_patch(patch: &str) -> anyhow::Result<Changes> {
    let diff_files = if patch.trim().is_empty() {
        Vec::new()
    } else {
        DiffFile::parse(patch)
            .map_err(|e| anyhow::anyhow!("could not read the diff (line {}): {}", e.line(), e.message()))?
    };
    let chunks = split_patch(patch);
    let files = diff_files
        .iter()
        .enumerate()
        .map(|(ix, file)| FileChange {
            path: file.path().to_string(),
            kind: match file.status() {
                DiffFileStatus::Added => ChangeKind::Added,
                DiffFileStatus::Deleted => ChangeKind::Deleted,
                DiffFileStatus::Renamed | DiffFileStatus::Copied => ChangeKind::Renamed,
                _ => ChangeKind::Modified,
            },
            additions: file.additions(),
            deletions: file.deletions(),
            patch: chunks.get(ix).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(Changes { is_repo: true, files, diff_files })
}

/// Split a multi-file git patch at each `diff --git` header.
pub fn split_patch(patch: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") || chunks.is_empty() {
            chunks.push(String::new());
        }
        chunks.last_mut().unwrap().push_str(line);
    }
    chunks.retain(|c| c.starts_with("diff --git "));
    chunks
}

// ---------- summaries ----------

const SUMMARY_SYSTEM_PROMPT: &str = "You summarize code changes for a developer reviewing an AI coding agent's work. \
For each file in the diff, write one or two plain sentences: what changed, and why it matters or what it's for. \
Be concrete (name functions, settings, behavior). No filler, no praise, no markdown headings.";

/// The input for one summary call: the user's recent requests, then each file's patch.
pub fn summary_prompt(requests: &[String], files: &[&FileChange]) -> String {
    let mut prompt = String::new();
    if !requests.is_empty() {
        prompt.push_str("What the user asked the agent for, most recent last:\n");
        for request in requests {
            prompt.push_str(&format!("- {}\n", truncate(request, 1_000)));
        }
        prompt.push('\n');
    }
    let mut budget = SUMMARY_TOTAL_CHARS;
    for file in files {
        let patch = truncate(&file.patch, SUMMARY_FILE_CHARS.min(budget));
        budget = budget.saturating_sub(patch.len());
        prompt.push_str(&format!("=== FILE: {} ({:?}) ===\n{patch}\n", file.path, file.kind));
        if budget == 0 {
            break;
        }
    }
    prompt
}

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "files": { "type": "array", "items": {
            "type": "object",
            "properties": { "path": { "type": "string" }, "summary": { "type": "string" } },
            "required": ["path", "summary"]
        }}},
        "required": ["files"]
    })
}

/// Read `claude -p --output-format json` output into path -> summary.
#[cfg(test)]
pub fn parse_summaries(output: &str) -> anyhow::Result<HashMap<String, String>> {
    Ok(summaries_from(&crate::agent::claude::parse_oneshot(output)?))
}

fn summaries_from(structured: &Value) -> HashMap<String, String> {
    structured["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f.get("path")?.as_str()?.to_string(), f.get("summary")?.as_str()?.trim().to_string())))
        .collect()
}

/// Ask Claude (Haiku, no tools, nothing saved) for a summary of each file. Blocking.
pub fn summarize(
    cwd: &Path,
    config_dir: Option<&Path>,
    requests: &[String],
    files: &[&FileChange],
) -> anyhow::Result<HashMap<String, String>> {
    let structured = crate::agent::claude::oneshot(
        cwd,
        config_dir,
        "haiku",
        SUMMARY_SYSTEM_PROMPT,
        &summary_schema(),
        &summary_prompt(requests, files),
    )?;
    Ok(summaries_from(&structured))
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[…truncated]", &text[..end])
}

// ---------- the model the UI shows ----------

/// Who writes the summaries.
#[derive(Clone)]
pub enum Summarizer {
    /// Haiku on this account (`None`: Claude's default config directory).
    Claude { config_dir: Option<PathBuf> },
    /// No summaries (tests).
    Off,
}

/// A project's changes, kept fresh, with the diff view's state and the summaries.
pub struct ProjectChanges {
    root: PathBuf,
    pub changes: Changes,
    pub diff: Entity<DiffState>,
    /// Keyed by [`FileChange::fingerprint`].
    summaries: HashMap<u64, String>,
    pub summarizing: bool,
    pub error: Option<String>,
    refreshing: bool,
    refresh_again: bool,
    /// While a review is open: who writes summaries, and the user's requests for context.
    auto_summarize: Option<(Summarizer, Vec<String>)>,
}

impl ProjectChanges {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new(Vec::new(), cx));
        let mut this = Self {
            root,
            changes: Changes::default(),
            diff,
            summaries: HashMap::new(),
            summarizing: false,
            error: None,
            refreshing: false,
            refresh_again: false,
            auto_summarize: None,
        };
        this.refresh(cx);
        this
    }

    pub fn summary(&self, file: &FileChange) -> Option<&str> {
        self.summaries.get(&file.fingerprint()).map(String::as_str)
    }

    /// Re-read git in the background. Calls made while a refresh runs fold into one more.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            self.refresh_again = true;
            return;
        }
        self.refreshing = true;
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { collect(&root) }).await;
            let _ = this.update(cx, |this: &mut ProjectChanges, cx| {
                this.refreshing = false;
                match result {
                    Ok(changes) => {
                        this.error = None;
                        this.diff.update(cx, |diff, cx| diff.set_files(changes.diff_files.clone(), cx));
                        this.changes = changes;
                    }
                    Err(err) => this.error = Some(format!("{err:#}")),
                }
                if std::mem::take(&mut this.refresh_again) {
                    this.refresh(cx);
                } else if let Some((summarizer, requests)) = this.auto_summarize.clone() {
                    this.summarize_missing(summarizer, requests, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// While set, summaries are written for new and changed files after every refresh.
    pub fn set_auto_summarize(&mut self, auto: Option<(Summarizer, Vec<String>)>, cx: &mut Context<Self>) {
        self.auto_summarize = auto.clone();
        if let Some((summarizer, requests)) = auto {
            self.summarize_missing(summarizer, requests, cx);
        }
    }

    /// Write summaries for files that don't have one for their current version.
    pub fn summarize_missing(&mut self, summarizer: Summarizer, requests: Vec<String>, cx: &mut Context<Self>) {
        let Summarizer::Claude { config_dir } = summarizer else { return };
        let missing: Vec<FileChange> =
            self.changes.files.iter().filter(|f| !self.summaries.contains_key(&f.fingerprint())).cloned().collect();
        if missing.is_empty() || self.summarizing {
            return;
        }
        self.summarizing = true;
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let files = missing.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let refs: Vec<&FileChange> = files.iter().collect();
                    summarize(&root, config_dir.as_deref(), &requests, &refs)
                })
                .await;
            let _ = this.update(cx, |this: &mut ProjectChanges, cx| {
                this.summarizing = false;
                match result {
                    Ok(by_path) => {
                        for file in &missing {
                            if let Some(summary) = by_path.get(&file.path) {
                                this.summaries.insert(file.fingerprint(), summary.clone());
                            }
                        }
                    }
                    Err(err) => this.error = Some(format!("Summaries failed: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

/// Paths (relative to `root`) of files the agent wrote or edited, from tool inputs.
pub fn touched_relative(root: &Path, touched: &HashSet<String>) -> HashSet<String> {
    touched
        .iter()
        .map(|p| Path::new(p).strip_prefix(root).map(|r| r.to_string_lossy().to_string()).unwrap_or_else(|_| p.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ChangeKind, FileChange, SUMMARY_FILE_CHARS, collect, from_patch, git, parse_summaries, summarize,
        summary_prompt,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tj-changes-{}", crate::store::new_id()));
        fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Test"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(&dir, args).unwrap().status.success());
        }
        dir
    }

    fn commit_all(dir: &Path) {
        assert!(git(dir, &["add", "-A"]).unwrap().status.success());
        assert!(git(dir, &["commit", "-q", "-m", "base"]).unwrap().status.success());
    }

    #[test]
    fn lists_added_modified_deleted_and_renamed_files() {
        let dir = repo();
        fs::write(dir.join("keep.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        fs::write(dir.join("gone.rs"), "old\n").unwrap();
        fs::write(dir.join("old_name.rs"), "line 1\nline 2\nline 3\nline 4\n").unwrap();
        commit_all(&dir);

        fs::write(dir.join("keep.rs"), "fn a() {}\nfn c() {}\n").unwrap();
        fs::remove_file(dir.join("gone.rs")).unwrap();
        fs::rename(dir.join("old_name.rs"), dir.join("new_name.rs")).unwrap();
        assert!(git(&dir, &["add", "-A", "--", "old_name.rs", "new_name.rs"]).unwrap().status.success());
        fs::write(dir.join("fresh.rs"), "brand new\n").unwrap();

        let changes = collect(&dir).unwrap();
        let mut seen: Vec<(String, ChangeKind, usize, usize)> =
            changes.files.iter().map(|f| (f.path.clone(), f.kind, f.additions, f.deletions)).collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("fresh.rs".into(), ChangeKind::Added, 1, 0),
                ("gone.rs".into(), ChangeKind::Deleted, 0, 1),
                ("keep.rs".into(), ChangeKind::Modified, 1, 1),
                ("new_name.rs".into(), ChangeKind::Renamed, 0, 0),
            ]
        );
        assert!(changes.files.iter().all(|f| f.patch.starts_with("diff --git ")));
        assert_eq!(changes.diff_files.len(), changes.files.len());
    }

    #[test]
    fn a_repo_without_commits_shows_every_file_as_added() {
        let dir = repo();
        fs::write(dir.join("first.txt"), "hello\n").unwrap();
        let changes = collect(&dir).unwrap();
        assert!(changes.is_repo);
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].kind, ChangeKind::Added);
    }

    #[test]
    fn a_folder_outside_git_has_no_changes() {
        let dir = std::env::temp_dir().join(format!("tj-nogit-{}", crate::store::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let changes = collect(&dir).unwrap();
        assert!(!changes.is_repo);
        assert!(changes.files.is_empty());
    }

    #[test]
    fn a_clean_repo_has_no_changes() {
        let dir = repo();
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        commit_all(&dir);
        assert!(collect(&dir).unwrap().files.is_empty());
    }

    #[test]
    fn fingerprints_follow_the_patch() {
        let a =
            FileChange { path: "x".into(), kind: ChangeKind::Modified, additions: 1, deletions: 0, patch: "p1".into() };
        let b = FileChange { patch: "p2".into(), ..a.clone() };
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn summary_output_is_read_from_structured_output_or_result() {
        let structured = r#"{"is_error":false,"structured_output":{"files":[{"path":"a.rs","summary":" Adds a. "}]},"result":"ignored"}"#;
        assert_eq!(parse_summaries(structured).unwrap().get("a.rs").map(String::as_str), Some("Adds a."));
        let text = r#"{"is_error":false,"result":"{\"files\":[{\"path\":\"b.rs\",\"summary\":\"Drops b.\"}]}"}"#;
        assert_eq!(parse_summaries(text).unwrap().get("b.rs").map(String::as_str), Some("Drops b."));
        assert!(parse_summaries(r#"{"is_error":true,"result":"rate limited"}"#).is_err());
    }

    #[test]
    fn summary_prompt_includes_requests_and_caps_size() {
        let big = FileChange {
            path: "big.rs".into(),
            kind: ChangeKind::Modified,
            additions: 1,
            deletions: 0,
            patch: "x".repeat(50_000),
        };
        let prompt = summary_prompt(&["Make it faster".into()], &[&big]);
        assert!(prompt.contains("- Make it faster"));
        assert!(prompt.contains("=== FILE: big.rs (Modified) ==="));
        assert!(prompt.len() < SUMMARY_FILE_CHARS + 500);
    }

    /// Runs the real claude on Haiku (a fraction of a cent): `cargo test -- --ignored live_`
    #[test]
    #[ignore]
    fn live_summaries_come_back_per_file() {
        let changes = from_patch(
            "diff --git a/src/greet.rs b/src/greet.rs\n--- a/src/greet.rs\n+++ b/src/greet.rs\n@@ -1,3 +1,3 @@\n pub fn greet(name: &str) -> String {\n-    format!(\"Hello {name}\")\n+    format!(\"Hi there, {name}!\")\n }\n",
        )
        .unwrap();
        let files: Vec<&FileChange> = changes.files.iter().collect();
        let summaries =
            summarize(&std::env::temp_dir(), None, &["Make the greeting friendlier".into()], &files).unwrap();
        assert!(summaries.get("src/greet.rs").is_some_and(|s| !s.is_empty()), "{summaries:?}");
    }
}
