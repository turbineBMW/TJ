//! Project notes: the features, fixes, ideas and questions put off during a session, kept so
//! they aren't lost once the conversation scrolls away. Each project's notes are one JSON file
//! in TJ's data folder (`notes/<project id>.json`), outside the repository.
//!
//! The notes tab edits them, and so does Claude, through the `tj notes-mcp` server
//! ([`crate::notes_mcp`]) that every session TJ starts is given. Both read, change and write a
//! file under a lock, so neither loses the other's edits; [`NotesStore`] notices changes from
//! outside and reloads.
//!
//! Removing a project leaves its notes behind. They show on the notes tab's cleanup page, and
//! adding the same folder again brings them back.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gpui_kit::{Context, Task};
use serde::{Deserialize, Serialize};

use crate::tracked::TrackedPr;

/// A pull request's address the way it's stored: tidied, or as given if it isn't a web address.
fn tidy_url(url: &str) -> String {
    crate::tracked::parse_url(url).map_or_else(|| url.trim().to_string(), |l| l.url)
}

/// What Claude is told about the notes in every session that has the notes tools.
pub const INSTRUCTIONS: &str = "TJ keeps a list of notes for this project (the `tj` MCP server's \
notes_list, note_read, note_add, note_update and note_delete tools). The notes are how deferred work \
survives a long session:
- When you and the user decide to put something off (a feature, fix, follow-up, idea or open question \
that won't be done now), add a note with note_add and tell the user you did. Write a short title and a \
body that a fresh session could act on without this conversation: what, why, where in the code, and \
anything already decided.
- When the user asks what's pending, left over or deferred, check notes_list.
- When you finish work a note describes, close it with note_update (status \"done\") and a one-line \
resolution (what was done, and the PR if there is one).
- Prefer closing a note over deleting it; delete only duplicates or notes that were wrong.

TJ also keeps track of the pull requests this project sends to other repositories and hosts (the \
pr_track, pr_list, pr_update and pr_untrack tools), and shows them on the GitHub tab's Other repos page:
- After you open a pull request or merge request on a repository other than this project's own, or mail \
a patch series, call pr_track with its web address (for a patch, its lore.kernel.org or patchwork link), \
the local folder its branch is in, and a one-line note on what it's for. TJ also catches `gh pr create` \
output itself; tracking a PR twice is harmless.
- When the user asks how their PRs are doing, or what's waiting on review, call pr_list (refresh: true \
reads the latest state from GitHub and GitLab).
- Set a mailed patch's or other link's status by hand with pr_update when you learn it was merged or \
dropped.";

/// What a note is about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Work put off for later.
    #[default]
    Deferred,
    Bug,
    Idea,
    Question,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Deferred, Kind::Bug, Kind::Idea, Kind::Question];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Deferred => "deferred",
            Kind::Bug => "bug",
            Kind::Idea => "idea",
            Kind::Question => "question",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Deferred => "Deferred",
            Kind::Bug => "Bug",
            Kind::Idea => "Idea",
            Kind::Question => "Question",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// Where a note is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteStatus {
    #[default]
    Open,
    /// Someone is working on it (a thread was started for it).
    Doing,
    Done,
}

impl NoteStatus {
    pub fn name(self) -> &'static str {
        match self {
            NoteStatus::Open => "open",
            NoteStatus::Doing => "doing",
            NoteStatus::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NoteStatus::Open => "Open",
            NoteStatus::Doing => "In progress",
            NoteStatus::Done => "Done",
        }
    }

    pub fn parse(name: &str) -> Option<NoteStatus> {
        match name.trim().to_ascii_lowercase().as_str() {
            "open" => Some(NoteStatus::Open),
            "doing" | "in_progress" | "in progress" => Some(NoteStatus::Doing),
            "done" | "closed" => Some(NoteStatus::Done),
            _ => None,
        }
    }

    /// Still to be done: open, or being worked on.
    pub fn is_pending(self) -> bool {
        self != NoteStatus::Done
    }
}

/// Who wrote a note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    #[default]
    User,
    Agent,
}

/// Where a note was written: the thread, which of its messages, and the git state then.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// How many messages the user had sent in the thread when the note was written, so the
    /// note can lead back to the last of them (1 is the first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Numbered from 1 in each project, never reused, so "#3" stays the same note.
    pub number: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub status: NoteStatus,
    #[serde(default)]
    pub author: Author,
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// How it was resolved, once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// The thread started to work on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_thread: Option<String>,
}

/// A project's notes, with the project's name and folder so they can be recognised after the
/// project is removed from TJ.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Notebook {
    pub project_name: String,
    pub project_path: PathBuf,
    /// The number the next note gets.
    #[serde(default = "first_number")]
    pub next_number: u64,
    #[serde(default)]
    pub notes: Vec<Note>,
    /// The pull requests the project's sessions sent out, anywhere ([`crate::tracked`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<TrackedPr>,
}

fn first_number() -> u64 {
    1
}

/// A note to add.
#[derive(Clone, Debug, Default)]
pub struct NewNote {
    pub title: String,
    pub body: String,
    pub kind: Kind,
    pub author: Author,
    pub source: Option<Source>,
}

impl Notebook {
    pub fn new(name: &str, path: &Path) -> Self {
        Self {
            project_name: name.to_string(),
            project_path: path.to_path_buf(),
            next_number: 1,
            notes: Vec::new(),
            prs: Vec::new(),
        }
    }

    pub fn get(&self, number: u64) -> Option<&Note> {
        self.notes.iter().find(|n| n.number == number)
    }

    pub fn get_mut(&mut self, number: u64) -> Option<&mut Note> {
        self.notes.iter_mut().find(|n| n.number == number)
    }

    /// Add a note and return its number.
    pub fn add(&mut self, new: NewNote) -> u64 {
        let number = self.next_number.max(self.notes.iter().map(|n| n.number + 1).max().unwrap_or(1));
        self.next_number = number + 1;
        let now = crate::store::now();
        self.notes.push(Note {
            number,
            title: new.title.trim().to_string(),
            body: new.body.trim().to_string(),
            kind: new.kind,
            status: NoteStatus::Open,
            author: new.author,
            created: now,
            updated: now,
            source: new.source,
            resolution: None,
            work_thread: None,
        });
        number
    }

    pub fn remove(&mut self, number: u64) -> Option<Note> {
        let ix = self.notes.iter().position(|n| n.number == number)?;
        Some(self.notes.remove(ix))
    }

    /// Notes still to be done.
    pub fn pending(&self) -> usize {
        self.notes.iter().filter(|n| n.status.is_pending()).count()
    }

    /// Take in another project's notes, numbered after this one's.
    pub fn absorb(&mut self, other: Notebook) {
        for mut note in other.notes {
            note.number = self.next_number.max(self.notes.iter().map(|n| n.number + 1).max().unwrap_or(1));
            self.next_number = note.number + 1;
            self.notes.push(note);
        }
        for pr in other.prs {
            if self.tracked(&pr.url).is_none() {
                self.prs.push(pr);
            }
        }
    }

    /// The tracked pull request at `url` (as given or tidied).
    pub fn tracked(&self, url: &str) -> Option<&TrackedPr> {
        let url = tidy_url(url);
        self.prs.iter().find(|p| p.url == url)
    }

    pub fn tracked_mut(&mut self, url: &str) -> Option<&mut TrackedPr> {
        let url = tidy_url(url);
        self.prs.iter_mut().find(|p| p.url == url)
    }

    /// Start tracking a pull request. One already tracked keeps its state, and takes the new
    /// note, folder and source where it had none. True if it's new.
    pub fn track(&mut self, pr: TrackedPr) -> bool {
        match self.tracked_mut(&pr.url) {
            Some(old) => {
                if !pr.note.is_empty() {
                    old.note = pr.note;
                }
                if old.folder.is_none() {
                    old.folder = pr.folder;
                }
                if old.source.is_none() {
                    old.source = pr.source;
                }
                false
            }
            None => {
                self.prs.push(pr);
                true
            }
        }
    }

    pub fn untrack(&mut self, url: &str) -> Option<TrackedPr> {
        let url = tidy_url(url);
        let ix = self.prs.iter().position(|p| p.url == url)?;
        Some(self.prs.remove(ix))
    }

    /// The notes as Markdown, newest first, for copying out of TJ.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# Notes for {}\n\n`{}`\n", self.project_name, self.project_path.display());
        let mut notes: Vec<&Note> = self.notes.iter().collect();
        notes.sort_by_key(|n| std::cmp::Reverse(n.number));
        for note in notes {
            let check = if note.status == NoteStatus::Done { "x" } else { " " };
            out.push_str(&format!("\n## [{check}] #{} {} ({})\n", note.number, note.title, note.kind.name()));
            if !note.body.is_empty() {
                out.push_str(&format!("\n{}\n", note.body));
            }
            if let Some(resolution) = &note.resolution {
                out.push_str(&format!("\nResolved: {resolution}\n"));
            }
        }
        if !self.prs.is_empty() {
            out.push_str("\n## Pull requests\n\n");
            for pr in &self.prs {
                out.push_str(&format!("- [{}] {} ({})", pr.status.name(), pr.display_title(), pr.url));
                if !pr.note.is_empty() {
                    out.push_str(&format!(" — {}", pr.note));
                }
                out.push('\n');
            }
        }
        out
    }
}

/// What a thread started from a note is asked to do.
pub fn work_prompt(note: &Note) -> String {
    let mut prompt = format!("Let's work on project note #{}: {}", note.number, note.title);
    if !note.body.is_empty() {
        prompt.push_str(&format!("\n\n{}", note.body));
    }
    prompt.push_str(&format!(
        "\n\nWhen it's done, close note #{} with note_update (status done, and a one-line resolution).",
        note.number
    ));
    prompt
}

/// Where the notes files are.
pub fn notes_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("notes")
}

/// A project's notes file.
pub fn notes_file(data_dir: &Path, project_id: &str) -> PathBuf {
    notes_dir(data_dir).join(format!("{project_id}.json"))
}

/// Read a notes file; `None` when there isn't one or it can't be read.
pub fn load(path: &Path) -> Option<Notebook> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Change a project's notes under the file's lock: read them (an empty notebook when there's
/// no file yet), apply `change`, and write them back. The project's name and folder are kept
/// up to date in the file. Several TJ windows and Claude sessions can do this at once.
pub fn update<R>(path: &Path, name: &str, folder: &Path, change: impl FnOnce(&mut Notebook) -> R) -> anyhow::Result<R> {
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no folder for {}", path.display()))?;
    fs::create_dir_all(dir)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut book = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|err| anyhow::anyhow!("{} isn't a notes file TJ can read: {err}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Notebook::new(name, folder),
        Err(err) => return Err(err.into()),
    };
    if !name.is_empty() {
        book.project_name = name.to_string();
    }
    if !folder.as_os_str().is_empty() {
        book.project_path = folder.to_path_buf();
    }
    let result = change(&mut book);
    write(path, &book)?;
    drop(lock);
    Ok(result)
}

/// Write a notebook in one step: a reader never sees half a file.
fn write(path: &Path, book: &Notebook) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(book)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Delete a project's notes file (and its lock).
pub fn delete(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let _ = fs::remove_file(path.with_extension("lock"));
    Ok(())
}

/// The checked-out branch and short commit in `dir`, if it's a git repository.
pub fn git_state(dir: &Path) -> (Option<String>, Option<String>) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    (branch, git(&["rev-parse", "--short", "HEAD"]))
}

/// How many messages the user has sent in a thread, from its transcript.
pub fn prompts_in(transcript: &Path) -> Option<usize> {
    let text = fs::read_to_string(transcript).ok()?;
    let count = text.lines().filter(|line| line.starts_with("{\"kind\":\"prompt\"")).count();
    (count > 0).then_some(count)
}

/// How often the notes folder is checked for changes made outside this window.
const POLL: Duration = Duration::from_millis(1500);

/// Every project's notes, kept in step with the files.
pub struct NotesStore {
    dir: PathBuf,
    books: HashMap<String, Notebook>,
    stamps: HashMap<String, SystemTime>,
    /// The last problem reading or writing a notes file.
    pub error: Option<String>,
    _poll: Option<Task<()>>,
}

impl NotesStore {
    pub fn new(data_dir: &Path, cx: &mut Context<Self>) -> Self {
        let mut this =
            Self { dir: notes_dir(data_dir), books: HashMap::new(), stamps: HashMap::new(), error: None, _poll: None };
        this.rescan();
        this._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let rescanned = this.update(cx, |s: &mut NotesStore, cx| {
                    if s.rescan() {
                        cx.notify();
                    }
                });
                if rescanned.is_err() {
                    break;
                }
            }
        }));
        this
    }

    /// Read the files that changed since the last look. True if anything did.
    fn rescan(&mut self) -> bool {
        let mut seen: HashMap<String, SystemTime> = HashMap::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
                let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
                seen.insert(id, modified);
            }
        }
        let mut changed = false;
        for (id, modified) in &seen {
            if self.stamps.get(id) != Some(modified) {
                if let Some(book) = load(&self.dir.join(format!("{id}.json"))) {
                    self.books.insert(id.clone(), book);
                }
                changed = true;
            }
        }
        let gone: Vec<String> = self.books.keys().filter(|id| !seen.contains_key(*id)).cloned().collect();
        for id in gone {
            self.books.remove(&id);
            changed = true;
        }
        self.stamps = seen;
        changed
    }

    fn file(&self, project_id: &str) -> PathBuf {
        self.dir.join(format!("{project_id}.json"))
    }

    pub fn book(&self, project_id: &str) -> Option<&Notebook> {
        self.books.get(project_id)
    }

    /// How many of a project's notes are still to be done.
    pub fn pending(&self, project_id: &str) -> usize {
        self.books.get(project_id).map_or(0, Notebook::pending)
    }

    /// Change a project's notes, saving them at once.
    pub fn edit<R>(
        &mut self,
        project_id: &str,
        name: &str,
        folder: &Path,
        change: impl FnOnce(&mut Notebook) -> R,
        cx: &mut Context<Self>,
    ) -> Option<R> {
        let result = match update(&self.file(project_id), name, folder, change) {
            Ok(result) => {
                self.error = None;
                Some(result)
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                None
            }
        };
        self.rescan();
        cx.notify();
        result
    }

    /// Notes whose project isn't in TJ any more, by the id they were saved under.
    pub fn orphans<'a>(&'a self, live: &[&str]) -> Vec<(&'a str, &'a Notebook)> {
        let mut orphans: Vec<(&str, &Notebook)> = self
            .books
            .iter()
            .filter(|(id, book)| !live.contains(&id.as_str()) && !(book.notes.is_empty() && book.prs.is_empty()))
            .map(|(id, book)| (id.as_str(), book))
            .collect();
        orphans.sort_by_key(|a| a.1.project_name.to_lowercase());
        orphans
    }

    /// Move one project's notes into another's (numbered after its own), and forget the first.
    pub fn merge_into(&mut self, from: &str, to: &str, name: &str, folder: &Path, cx: &mut Context<Self>) {
        if from == to {
            return;
        }
        let Some(book) = self.books.get(from).cloned() else { return };
        if self.edit(to, name, folder, |target| target.absorb(book), cx).is_some() {
            self.delete_book(from, cx);
        }
    }

    pub fn delete_book(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Err(err) = delete(&self.file(project_id)) {
            self.error = Some(format!("{err:#}"));
        }
        self.rescan();
        cx.notify();
    }

    /// A project was added at `folder`: bring back notes left from when a project there was
    /// removed. True if any came back.
    pub fn reattach(
        &mut self,
        project_id: &str,
        name: &str,
        folder: &Path,
        live: &[&str],
        cx: &mut Context<Self>,
    ) -> bool {
        let found: Vec<String> = self
            .orphans(live)
            .into_iter()
            .filter(|(id, book)| *id != project_id && book.project_path == folder)
            .map(|(id, _)| id.to_string())
            .collect();
        for id in &found {
            self.merge_into(id, project_id, name, folder, cx);
        }
        !found.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tj-notes-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn notes_are_numbered_and_numbers_are_never_reused() {
        let mut book = Notebook::new("TJ", Path::new("/p"));
        let a = book.add(NewNote { title: " First ".into(), ..Default::default() });
        let b = book.add(NewNote { title: "Second".into(), ..Default::default() });
        assert_eq!((a, b), (1, 2));
        assert_eq!(book.get(1).unwrap().title, "First");
        book.remove(2);
        assert_eq!(book.add(NewNote { title: "Third".into(), ..Default::default() }), 3);
        assert_eq!(book.pending(), 2);
    }

    #[test]
    fn update_creates_the_file_and_keeps_edits_from_each_writer() {
        let dir = temp_dir("update");
        let path = notes_file(&dir, "p1");
        let folder = Path::new("/home/me/proj");
        update(&path, "proj", folder, |b| b.add(NewNote { title: "One".into(), ..Default::default() })).unwrap();
        update(&path, "", Path::new(""), |b| b.add(NewNote { title: "Two".into(), ..Default::default() })).unwrap();
        let book = load(&path).unwrap();
        assert_eq!(book.notes.len(), 2);
        assert_eq!(book.project_name, "proj", "an empty name leaves the saved one");
        assert_eq!(book.project_path, folder);
        delete(&path).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn absorbing_renumbers_after_the_target() {
        let mut target = Notebook::new("a", Path::new("/a"));
        target.add(NewNote { title: "Mine".into(), ..Default::default() });
        let mut other = Notebook::new("b", Path::new("/b"));
        other.add(NewNote { title: "Theirs".into(), ..Default::default() });
        target.absorb(other);
        assert_eq!(target.notes.iter().map(|n| n.number).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(target.get(2).unwrap().title, "Theirs");
    }

    #[test]
    fn kinds_and_statuses_parse_their_names() {
        assert_eq!(Kind::parse("Bug"), Some(Kind::Bug));
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(NoteStatus::parse("in progress"), Some(NoteStatus::Doing));
        assert_eq!(NoteStatus::parse("closed"), Some(NoteStatus::Done));
    }

    #[test]
    fn prompts_are_counted_from_the_transcript() {
        let dir = temp_dir("prompts");
        let path = dir.join("t.jsonl");
        fs::write(
            &path,
            "{\"kind\":\"prompt\",\"text\":\"a\"}\n{\"kind\":\"event\"}\n{\"kind\":\"prompt\",\"text\":\"b\"}\n",
        )
        .unwrap();
        assert_eq!(prompts_in(&path), Some(2));
        assert_eq!(prompts_in(&dir.join("missing.jsonl")), None);
    }

    #[test]
    fn markdown_lists_notes_newest_first() {
        let mut book = Notebook::new("TJ", Path::new("/p"));
        book.add(NewNote { title: "Old".into(), ..Default::default() });
        book.add(NewNote { title: "New".into(), body: "Details".into(), kind: Kind::Bug, ..Default::default() });
        let md = book.to_markdown();
        assert!(md.find("#2 New").unwrap() < md.find("#1 Old").unwrap());
        assert!(md.contains("(bug)") && md.contains("Details"));
    }
}
