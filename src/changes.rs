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
    /// The one file the review shows (and summarizes), or `None` for all of them.
    pub focus: Option<String>,
    /// Changes from a patch (a pull request's) rather than the working tree; refresh leaves
    /// them be.
    fixed: bool,
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
            focus: None,
            fixed: false,
        };
        this.refresh(cx);
        this
    }

    /// The changes in `patch` (a pull request's), summarized with `root` as the working folder.
    pub fn from_fixed_patch(root: PathBuf, patch: &str, cx: &mut Context<Self>) -> Self {
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
            focus: None,
            fixed: true,
        };
        this.set_patch(patch, cx);
        this
    }

    /// Show `patch` instead (the pull request moved on). Summaries of files that didn't change
    /// are kept.
    pub fn set_patch(&mut self, patch: &str, cx: &mut Context<Self>) {
        match from_patch(patch) {
            Ok(changes) => {
                self.error = None;
                self.changes = changes;
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
        self.show_diff(cx);
        cx.notify();
    }

    /// Where the focused file is among all of them: (index, count).
    pub fn focus_position(&self) -> Option<(usize, usize)> {
        let path = self.focus.as_ref()?;
        let ix = self.changes.files.iter().position(|f| &f.path == path)?;
        Some((ix, self.changes.files.len()))
    }

    /// Focus the file `step` files after (or before) the focused one.
    pub fn step_focus(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some((ix, count)) = self.focus_position() else { return };
        let to = (ix as isize + step).clamp(0, count as isize - 1) as usize;
        if to != ix {
            let path = self.changes.files[to].path.clone();
            self.set_focus(Some(path), cx);
        }
    }

    pub fn summary(&self, file: &FileChange) -> Option<&str> {
        self.summaries.get(&file.fingerprint()).map(String::as_str)
    }

    /// The files the review shows: the focused one, or all of them.
    pub fn shown(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.files.iter().filter(|f| self.focus.as_ref().is_none_or(|p| &f.path == p))
    }

    /// Show one file in the review (`None`: all of them). Only what's shown gets summarized.
    pub fn set_focus(&mut self, focus: Option<String>, cx: &mut Context<Self>) {
        self.focus = focus;
        self.show_diff(cx);
        if let Some((summarizer, requests)) = self.auto_summarize.clone() {
            self.summarize_missing(summarizer, requests, cx);
        }
        cx.notify();
    }

    fn show_diff(&self, cx: &mut Context<Self>) {
        let files: Vec<DiffFile> = self
            .changes
            .files
            .iter()
            .zip(&self.changes.diff_files)
            .filter(|(f, _)| self.focus.as_ref().is_none_or(|p| &f.path == p))
            .map(|(_, d)| d.clone())
            .collect();
        self.diff.update(cx, |diff, cx| diff.set_files(files, cx));
    }

    /// Re-read git in the background. Calls made while a refresh runs fold into one more.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.fixed {
            return;
        }
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
                        this.changes = changes;
                        this.show_diff(cx);
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

    /// Shown files without a summary for their current version.
    fn missing(&self) -> Vec<FileChange> {
        self.shown().filter(|f| !self.summaries.contains_key(&f.fingerprint())).cloned().collect()
    }

    /// Write summaries for the shown files that don't have one for their current version.
    pub fn summarize_missing(&mut self, summarizer: Summarizer, requests: Vec<String>, cx: &mut Context<Self>) {
        let Summarizer::Claude { config_dir } = summarizer else { return };
        let missing = self.missing();
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
                        // Files asked for while this ran (another file picked, say) are next.
                        let sent: HashSet<u64> = missing.iter().map(FileChange::fingerprint).collect();
                        if let Some((summarizer, requests)) = this.auto_summarize.clone()
                            && this.missing().iter().any(|f| !sent.contains(&f.fingerprint()))
                        {
                            this.summarize_missing(summarizer, requests, cx);
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

/// The text of lines `start..=end` on one side of `patch` (one file's part of a diff): the old
/// side's (context and removed lines) or the new side's (context and added lines). Lines the
/// patch doesn't carry are left out.
pub fn patch_lines(patch: &str, old: bool, start: u64, end: u64) -> String {
    let (mut old_no, mut new_no) = (0u64, 0u64);
    let mut in_hunk = false;
    let mut out: Vec<&str> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            // `@@ -12,3 +12,4 @@`: where each side's lines start.
            let mut parts = rest.split_whitespace();
            let first = |part: Option<&str>, sign: char| {
                part.and_then(|p| p.strip_prefix(sign)).and_then(|p| p.split(',').next()?.parse().ok()).unwrap_or(1)
            };
            old_no = first(parts.next(), '-');
            new_no = first(parts.next(), '+');
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let tag = line.chars().next();
        let text = tag.map_or("", |c| &line[c.len_utf8()..]);
        let mut take = |no: u64| {
            if (start..=end).contains(&no) {
                out.push(text);
            }
        };
        match tag {
            Some(' ') | None => {
                take(if old { old_no } else { new_no });
                old_no += 1;
                new_no += 1;
            }
            Some('-') => {
                if old {
                    take(old_no);
                }
                old_no += 1;
            }
            Some('+') => {
                if !old {
                    take(new_no);
                }
                new_no += 1;
            }
            // "\ No newline at end of file".
            _ => {}
        }
    }
    out.join("\n")
}

/// A comment on lines of the review's diff, for the agent.
#[derive(Clone, Debug, PartialEq)]
pub struct LineNote {
    pub path: String,
    /// On lines the change removed (numbered as before it), rather than the file as it is.
    pub old: bool,
    pub start: u64,
    pub line: u64,
    /// The lines it's on.
    pub quoted: String,
    pub body: String,
}

/// What the agent is sent for `notes`: each one's place (`path:line`), the lines it's on and
/// the comment.
pub fn notes_prompt(notes: &[LineNote]) -> String {
    let mut out = String::from("Review comments on the uncommitted changes. Please address each one:\n");
    for note in notes {
        let at = if note.start < note.line {
            format!("{}:{}-{}", note.path, note.start, note.line)
        } else {
            format!("{}:{}", note.path, note.line)
        };
        out.push_str(&format!(
            "\n{at}{}\n",
            if note.old { " (removed lines, numbered as before the change)" } else { "" }
        ));
        if !note.quoted.trim().is_empty() {
            // A fence longer than any run of backticks in the code, so the code can't end it.
            let longest = note.quoted.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            out.push_str(&format!("{fence}\n{}\n{fence}\n", note.quoted.trim_end_matches('\n')));
        }
        out.push_str(note.body.trim());
        out.push('\n');
    }
    out
}

pub const REVIEW_CONTEXT: &str = "Review";

gpui_kit::actions!(review, [CommentOnLines, RemoveComment, SendComments, CancelComment]);

/// The review's own keys, while its diff (or a comment box in it) has focus. Shift+↑/↓ in the
/// diff select lines (gpui-kit's), and these act on them.
pub fn bindings() -> Vec<KeyBinding> {
    let cx = Some(REVIEW_CONTEXT);
    vec![
        KeyBinding::new("enter", CommentOnLines, cx),
        KeyBinding::new("delete", RemoveComment, cx),
        KeyBinding::new("ctrl-enter", SendComments, cx),
        KeyBinding::new("escape", CancelComment, cx),
    ]
}

/// What the review asks of the workspace.
pub enum ReviewEvent {
    /// Put these comments in the project agent's prompt box, as a chip.
    SendToThread { label: String, text: String },
}

/// A comment being written on the diff.
struct Compose {
    note: LineNote,
    input: Entity<gpui_kit::component::input::TextareaState>,
}

/// The review of a project's changes, shown in a tab: every file's diff stacked, unified or
/// side by side, with each file's summary under its header. Lines can be commented on (hover
/// and +, or select and Enter), and the comments sent to the agent's prompt box.
pub struct ReviewView {
    pub changes: Entity<ProjectChanges>,
    no_summaries: bool,
    /// In a modal over a pull request: one file at a time, with Previous and Next file, and no
    /// comments (the GitHub tab takes those).
    navigator: bool,
    /// Comments waiting to go to the agent.
    pub notes: Vec<LineNote>,
    compose: Option<Compose>,
    focus: FocusHandle,
    _observe: Subscription,
    _compose_events: Option<Subscription>,
}

impl EventEmitter<ReviewEvent> for ReviewView {}

impl ReviewView {
    pub fn new(changes: Entity<ProjectChanges>, no_summaries: bool, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&changes, |_, _, cx| cx.notify());
        Self {
            changes,
            no_summaries,
            navigator: false,
            notes: Vec::new(),
            compose: None,
            focus: cx.focus_handle(),
            _observe: observe,
            _compose_events: None,
        }
    }

    /// The review in a modal, a file at a time, with Previous and Next file (also ← and →).
    pub fn with_navigator(changes: Entity<ProjectChanges>, no_summaries: bool, cx: &mut Context<Self>) -> Self {
        Self { navigator: true, ..Self::new(changes, no_summaries, cx) }
    }

    /// Put the keyboard in the review: in its diff, where the arrows scroll and Shift+↑/↓
    /// select lines (in the modal, on the review itself, where ← and → change file).
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        if self.navigator {
            window.focus(&self.focus, cx);
        } else if let Some(compose) = &self.compose {
            let handle = compose.input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        } else {
            let handle = self.changes.read(cx).diff.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// The tab's title: the file shown, or how many.
    pub fn title(&self, cx: &App) -> String {
        let model = self.changes.read(cx);
        match &model.focus {
            Some(path) => format!("Review · {}", path.rsplit('/').next().unwrap_or(path)),
            None => format!("Review · {}", model.changes.files.len()),
        }
    }

    /// Start a comment on `range` (lines picked in the diff).
    pub fn start_comment(
        &mut self,
        range: &gpui_kit::component::diff::DiffLineRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::diff::DiffSide;
        use gpui_kit::component::input::{InputEvent, TextareaState};
        if self.navigator {
            return;
        }
        // A range from a removed line to an added one (unified view) is taken on its end's side.
        let side = range.end_side();
        let (start, line) = if range.side() == side {
            (range.start().min(range.end()) as u64, range.start().max(range.end()) as u64)
        } else {
            (range.end() as u64, range.end() as u64)
        };
        let old = side == DiffSide::Original;
        let path = range.path().to_string();
        let quoted = self
            .changes
            .read(cx)
            .changes
            .files
            .iter()
            .find(|f| f.path == path)
            .map(|f| patch_lines(&f.patch, old, start, line))
            .unwrap_or_default();
        let placeholder = "Comment for the agent";
        let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 10).placeholder(placeholder));
        self._compose_events = Some(cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary: true, .. } = event {
                this.finish_comment(window, cx);
            }
        }));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.compose = Some(Compose { note: LineNote { path, old, start, line, quoted, body: String::new() }, input });
        cx.notify();
    }

    /// Keep the comment being written (if it says anything), and go back to the diff.
    pub fn finish_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Compose { mut note, input }) = self.compose.take() else { return };
        note.body = input.read(cx).value().trim().to_string();
        if !note.body.is_empty() {
            self.notes.push(note);
        }
        self._compose_events = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn cancel_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compose = None;
        self._compose_events = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn remove_note(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.notes.len() {
            self.notes.remove(ix);
            cx.notify();
        }
    }

    /// Remove the comments on the lines selected in the diff. False if there were none.
    fn remove_selected(&mut self, cx: &mut Context<Self>) -> bool {
        use gpui_kit::component::diff::DiffSide;
        let Some(range) = self.changes.read(cx).diff.read(cx).selected_lines() else { return false };
        let old = range.end_side() == DiffSide::Original;
        let (from, to) = (range.start().min(range.end()) as u64, range.start().max(range.end()) as u64);
        let before = self.notes.len();
        self.notes.retain(|n| !(n.path == range.path().as_ref() && n.old == old && n.start <= to && n.line >= from));
        cx.notify();
        self.notes.len() < before
    }

    /// Hand the comments to the agent's prompt box, and forget them here.
    pub fn send_notes(&mut self, cx: &mut Context<Self>) {
        if self.notes.is_empty() {
            return;
        }
        let notes = std::mem::take(&mut self.notes);
        let label = match notes.len() {
            1 => format!("Review comment · {}", notes[0].path.rsplit('/').next().unwrap_or(&notes[0].path)),
            n => format!("{n} review comments"),
        };
        cx.emit(ReviewEvent::SendToThread { label, text: notes_prompt(&notes) });
        cx.notify();
    }
}

impl Render for ReviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::assets::IconName;
        use gpui_kit::base::{h_flex, v_flex};
        use gpui_kit::component::{
            ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
            button::{Button, ButtonVariants as _},
            diff::{Diff, DiffAnnotation, DiffLinePosition, DiffMode, DiffSide},
        };
        use gpui_kit::prelude::FluentBuilder as _;

        let model = self.changes.read(cx);
        let theme = cx.theme();
        let (muted, border, danger, secondary, bg) =
            (theme.muted_foreground, theme.border, theme.danger, theme.secondary, theme.background);
        let summaries: HashMap<String, Option<String>> =
            model.shown().map(|f| (f.path.clone(), model.summary(f).map(str::to_owned))).collect();
        let summarizing = model.summarizing;
        let no_summaries = self.no_summaries;
        let mode = model.diff.read(cx).mode();
        let diff_state = model.diff.clone();
        let mut annotations: Vec<DiffAnnotation> = model
            .shown()
            .enumerate()
            .map(|(ix, f)| DiffAnnotation::file(SharedString::from(format!("summary-{ix}")), f.path.clone()))
            .collect();
        // Comments under their last line (under their file's header if the line is gone), then
        // the one being written.
        let commenting = !self.navigator;
        let place = |path: &str, old: bool, line: u64, id: SharedString| {
            let side = if old { DiffSide::Original } else { DiffSide::Modified };
            let at = DiffLinePosition::new(path.to_string(), side, line as usize);
            if diff_state.read(cx).has_line(&at) {
                DiffAnnotation::line(id, at)
            } else {
                DiffAnnotation::file(id, path.to_string())
            }
        };
        if commenting {
            annotations.extend(
                self.notes
                    .iter()
                    .enumerate()
                    .map(|(ix, n)| place(&n.path, n.old, n.line, SharedString::from(format!("note-{ix}")))),
            );
            if let Some(compose) = &self.compose {
                let n = &compose.note;
                annotations.push(place(&n.path, n.old, n.line, "compose".into()));
            }
        }
        let notes: HashMap<String, (usize, LineNote)> =
            self.notes.iter().cloned().enumerate().map(|(ix, n)| (format!("note-{ix}"), (ix, n))).collect();
        let compose_input = self.compose.as_ref().map(|c| c.input.clone());
        let shown: Vec<&FileChange> = model.shown().collect();
        let (additions, deletions): (usize, usize) =
            shown.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let heading = match &model.focus {
            Some(path) => format!("{path} · +{additions} −{deletions}"),
            None => format!("{} files changed · +{additions} −{deletions}", shown.len()),
        };
        let error = model.error.clone();
        let position = if self.navigator { model.focus_position() } else { None };
        let changes = self.changes.clone();
        let navigator = position.map(|(ix, count)| {
            let (back, next) = (changes.clone(), changes.clone());
            h_flex()
                .gap_1()
                .child(
                    Button::new("review-previous-file")
                        .xsmall()
                        .ghost()
                        .icon(IconName::ChevronLeft)
                        .label("Previous")
                        .tooltip("Previous file (←)")
                        .disabled(ix == 0)
                        .on_click(move |_, _, cx| back.update(cx, |c, cx| c.step_focus(-1, cx))),
                )
                .child(div().px_1().text_xs().text_color(muted).child(format!("File {} of {count}", ix + 1)))
                .child(
                    Button::new("review-next-file")
                        .xsmall()
                        .ghost()
                        .label("Next")
                        .icon(IconName::ChevronRight)
                        .tooltip("Next file (→)")
                        .disabled(ix + 1 >= count)
                        .on_click(move |_, _, cx| next.update(cx, |c, cx| c.step_focus(1, cx))),
                )
        });
        let empty = shown.is_empty();
        let empty_text = match &model.focus {
            Some(path) => format!("{path} has no uncommitted changes."),
            None => "No uncommitted changes.".into(),
        };

        // Under the header: how to comment, or the comments waiting and what to do with them.
        let note_count = self.notes.len();
        let comment_bar = (commenting && !empty).then(|| {
            let bar = h_flex().px_3().py_1().gap_2().border_b_1().border_color(border).text_xs().text_color(muted);
            if note_count == 0 {
                return bar.child(
                    "Comment for the agent: hover a line and press +, or select lines with Shift+↓ and press Enter.",
                );
            }
            bar.child(match note_count {
                1 => "1 comment for the agent.".to_string(),
                n => format!("{n} comments for the agent."),
            })
            .child("Ctrl+Enter sends them to the thread; Delete removes the selected line's.")
            .child(div().flex_1())
            .child(Button::new("review-discard-notes").xsmall().ghost().label("Discard").on_click(cx.listener(
                |this, _, _, cx| {
                    this.notes.clear();
                    cx.notify();
                },
            )))
            .child(
                Button::new("review-send-notes")
                    .xsmall()
                    .primary()
                    .icon(IconName::MessageSquarePlus)
                    .label("Send to thread")
                    .tooltip("Put them in the agent's prompt box (Ctrl+Enter)")
                    .on_click(cx.listener(|this, _, _, cx| this.send_notes(cx))),
            )
        });

        let this = cx.entity().downgrade();
        let diff = Diff::new(&diff_state)
            .syntax_highlight(true)
            .annotations(annotations)
            .when(commenting, |diff| {
                let this = this.clone();
                diff.on_add_annotation(move |range, window, cx| {
                    let _ = this.update(cx, |v, cx| v.start_comment(range, window, cx));
                })
            })
            .render_annotation(move |annotation, _window, cx| {
                let id = annotation.id().to_string();
                if let Some((ix, note)) = notes.get(&id) {
                    let (ix, this) = (*ix, this.clone());
                    return crate::line_comment::pending_card(
                        ix,
                        crate::line_comment::lines_label(note.start, note.line),
                        note.body.clone(),
                        move |_, _, cx| {
                            let _ = this.update(cx, |v, cx| v.remove_note(ix, cx));
                        },
                        cx,
                    );
                }
                if id == "compose"
                    && let Some(input) = &compose_input
                {
                    let (cancel, add) = (this.clone(), this.clone());
                    return crate::line_comment::compose_card(
                        input,
                        Some("Ctrl+Enter adds it · Esc cancels"),
                        [
                            Button::new("review-compose-cancel").xsmall().ghost().label("Cancel").on_click(
                                move |_, window, cx| {
                                    let _ = cancel.update(cx, |v, cx| v.cancel_comment(window, cx));
                                },
                            ),
                            Button::new("review-compose-add").xsmall().primary().label("Add comment").on_click(
                                move |_, window, cx| {
                                    let _ = add.update(cx, |v, cx| v.finish_comment(window, cx));
                                },
                            ),
                        ],
                        cx,
                    );
                }
                let summary = summaries.get(annotation.path().as_ref()).cloned().flatten();
                let text = match summary {
                    Some(summary) => summary,
                    None if no_summaries => String::new(),
                    None if summarizing => "Summarizing…".into(),
                    None => "No summary yet.".into(),
                };
                // Out over the padding the diff puts around each annotation, so the whole box is
                // the summary's colour.
                div()
                    .mx(rems(-0.75))
                    .my(rems(-0.75))
                    .px_6()
                    .py_4()
                    .bg(secondary)
                    .text_sm()
                    .child(text)
                    .into_any_element()
            });

        let (left, right) = (changes.clone(), changes);
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .when(self.navigator, |el| {
                el.on_key_down(move |event, _, cx| match event.keystroke.key.as_str() {
                    "left" if !event.keystroke.modifiers.modified() => {
                        left.update(cx, |c, cx| c.step_focus(-1, cx));
                    }
                    "right" if !event.keystroke.modifiers.modified() => {
                        right.update(cx, |c, cx| c.step_focus(1, cx));
                    }
                    _ => {}
                })
            })
            .when(commenting, |el| {
                el.key_context(REVIEW_CONTEXT)
                    .on_action(cx.listener(|this, _: &CommentOnLines, window, cx| {
                        let range = this.changes.read(cx).diff.read(cx).selected_lines();
                        match range {
                            Some(range) if this.compose.is_none() => this.start_comment(&range, window, cx),
                            _ => cx.propagate(),
                        }
                    }))
                    .on_action(cx.listener(|this, _: &RemoveComment, _, cx| {
                        if this.compose.is_some() || !this.remove_selected(cx) {
                            cx.propagate();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &SendComments, _, cx| {
                        if this.notes.is_empty() || this.compose.is_some() {
                            cx.propagate();
                        } else {
                            this.send_notes(cx);
                        }
                    }))
                    .on_action(cx.listener(|this, _: &CancelComment, window, cx| {
                        if this.compose.is_some() {
                            this.cancel_comment(window, cx);
                        } else {
                            cx.propagate();
                        }
                    }))
            })
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .bg(bg)
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(heading))
                    .when(summarizing, |el| el.child(div().text_xs().text_color(muted).child("Writing summaries…")))
                    .child(div().flex_1())
                    .children(navigator)
                    .children([DiffMode::Unified, DiffMode::Split].map(|m| {
                        let state = diff_state.clone();
                        Button::new(SharedString::from(format!("diff-mode-{m:?}")))
                            .xsmall()
                            .ghost()
                            .selected(mode == m)
                            .label(if m == DiffMode::Split { "Side by side" } else { "Unified" })
                            .on_click(move |_, _, cx| state.update(cx, |d, cx| d.set_mode(m, cx)))
                    })),
            )
            .children(comment_bar)
            .when_some(error, |el, error| el.child(div().px_3().py_1().text_sm().text_color(danger).child(error)))
            .child(if empty {
                div().p_4().text_sm().text_color(muted).child(empty_text).into_any_element()
            } else {
                div().flex_1().min_h_0().child(diff).into_any_element()
            })
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

    #[test]
    fn patch_lines_reads_either_side_of_a_hunk() {
        use super::patch_lines;
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,4 +10,4 @@ fn f() {\n keep\n-old one\n-old two\n+new one\n+new two\n \n\\ No newline at end of file\n";
        assert_eq!(patch_lines(patch, false, 11, 12), "new one\nnew two");
        assert_eq!(patch_lines(patch, true, 11, 12), "old one\nold two");
        assert_eq!(patch_lines(patch, false, 10, 10), "keep");
        assert_eq!(patch_lines(patch, true, 13, 13), "", "the blank context line");
        assert_eq!(patch_lines(patch, false, 40, 41), "", "lines the patch doesn't carry");
    }

    #[test]
    fn notes_prompt_places_and_quotes_each_comment() {
        use super::{LineNote, notes_prompt};
        let note = |path: &str, old, start, line, quoted: &str, body: &str| LineNote {
            path: path.into(),
            old,
            start,
            line,
            quoted: quoted.into(),
            body: body.into(),
        };
        let text = notes_prompt(&[
            note("src/a.rs", false, 3, 3, "let x = 1;", "Name this better."),
            note("README.md", true, 5, 6, "```sh\nrun\n```", "Why remove this?"),
        ]);
        assert!(text.contains("\nsrc/a.rs:3\n```\nlet x = 1;\n```\nName this better.\n"), "{text}");
        assert!(text.contains("README.md:5-6 (removed lines"), "{text}");
        assert!(text.contains("\n````\n```sh\nrun\n```\n````\nWhy remove this?"), "a longer fence: {text}");
    }

    #[gpui_kit::test]
    fn a_pull_requests_patch_is_shown_a_file_at_a_time(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+A\n\
diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-b\n+B\n";
        let changes = cx.new(|cx| super::ProjectChanges::from_fixed_patch(std::env::temp_dir(), patch, cx));
        changes.update(cx, |c, cx| {
            assert_eq!(c.changes.files.len(), 2);
            c.set_focus(Some("a.rs".into()), cx);
            assert_eq!(c.focus_position(), Some((0, 2)));
            c.step_focus(1, cx);
            assert_eq!(c.focus.as_deref(), Some("b.rs"));
            c.step_focus(1, cx);
            assert_eq!(c.focus_position(), Some((1, 2)), "stops at the last file");
            c.refresh(cx);
            assert_eq!(c.changes.files.len(), 2, "refresh doesn't swap in the working tree");
            assert_eq!(c.diff.read(cx).files().len(), 1, "only the focused file is in the diff");
        });
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
