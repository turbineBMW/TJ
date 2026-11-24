//! What TJ keeps on disk: accounts, projects and their threads, and each thread's transcript.
//!
//! - `$XDG_CONFIG_HOME/tj/accounts.toml`: the Claude accounts. Hand-editable; created on
//!   first run from the config directories found in the home folder.
//! - `$XDG_DATA_HOME/tj/state.json`: projects, threads and what was selected.
//! - `$XDG_DATA_HOME/tj/threads/<id>.jsonl`: one transcript per thread, replayed on open.
//!
//! Set `TJ_HOME` to keep all of it in one folder instead (tests and trial runs).

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::agent::{AgentEvent, PermissionMode};

/// Where TJ's files live.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        if let Some(home) = std::env::var_os("TJ_HOME") {
            let home = PathBuf::from(home);
            return Self { config_dir: home.join("config"), data_dir: home.join("data") };
        }
        let home = home_dir();
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback))
        };
        Self {
            config_dir: xdg("XDG_CONFIG_HOME", ".config").join("tj"),
            data_dir: xdg("XDG_DATA_HOME", ".local/share").join("tj"),
        }
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self { config_dir: dir.join("config"), data_dir: dir.join("data") }
    }

    pub fn accounts_file(&self) -> PathBuf {
        self.config_dir.join("accounts.toml")
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
    pub fn transcript(&self, thread_id: &str) -> PathBuf {
        self.data_dir.join("threads").join(format!("{thread_id}.jsonl"))
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/x` -> `/home/me/x`.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// ---------- accounts ----------

/// A Claude login, identified by its config directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// `CLAUDE_CONFIG_DIR`; Claude's own default (`~/.claude`) when absent. `~/` is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl Account {
    pub fn config_path(&self, home: &Path) -> Option<PathBuf> {
        self.config_dir.as_deref().map(|dir| expand_tilde(dir, home))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Accounts {
    /// The account used when neither the thread nor its project names one.
    pub default: String,
    #[serde(rename = "account")]
    pub accounts: Vec<Account>,
}

impl Accounts {
    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// The thread's own choice, else the project's, else the global default; unknown ids fall through.
    pub fn resolve(&self, thread: Option<&str>, project: Option<&str>) -> &Account {
        [thread, project, Some(self.default.as_str())]
            .into_iter()
            .flatten()
            .find_map(|id| self.get(id))
            .or(self.accounts.first())
            .expect("accounts are never empty")
    }

    /// One account per Claude config directory in `home`: `~/.claude` plus any `~/.claude-<name>`
    /// or `~/.claude_<name>` that holds a login. With more than one, `~/.claude` is "Personal".
    pub fn detect(home: &Path) -> Self {
        let mut extra: Vec<Account> = fs::read_dir(home)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let suffix = name.strip_prefix(".claude-").or_else(|| name.strip_prefix(".claude_"))?.to_string();
                let dir = entry.path();
                let has_login = dir.join(".credentials.json").exists() || dir.join(".claude.json").exists();
                (dir.is_dir() && has_login && !suffix.is_empty()).then(|| Account {
                    id: suffix.to_lowercase(),
                    name: capitalize(&suffix),
                    config_dir: Some(format!("~/{name}")),
                })
            })
            .collect();
        extra.sort_by(|a, b| a.id.cmp(&b.id));
        let default_name = if extra.is_empty() { "Default" } else { "Personal" };
        let mut accounts =
            vec![Account { id: default_name.to_lowercase(), name: default_name.into(), config_dir: None }];
        accounts.extend(extra.into_iter().filter(|a| a.id != default_name.to_lowercase()));
        Self { default: accounts[0].id.clone(), accounts }
    }

    /// Read `accounts.toml`, creating it from [`Accounts::detect`] the first time.
    pub fn load_or_create(paths: &Paths, home: &Path) -> anyhow::Result<Self> {
        let file = paths.accounts_file();
        if file.exists() {
            let text = fs::read_to_string(&file)?;
            let accounts: Self = toml::from_str(&text).with_context(|| format!("reading {}", file.display()))?;
            anyhow::ensure!(!accounts.accounts.is_empty(), "{} lists no accounts", file.display());
            return Ok(accounts);
        }
        let accounts = Self::detect(home);
        fs::create_dir_all(&paths.config_dir)?;
        let body = toml::to_string_pretty(&accounts)?;
        fs::write(
            &file,
            format!(
                "# TJ's Claude accounts. Each config_dir is a CLAUDE_CONFIG_DIR; leave it out for ~/.claude.\n\
                 # Log in to one with: CLAUDE_CONFIG_DIR=<dir> claude auth login\n\n{body}"
            ),
        )?;
        Ok(accounts)
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

// ---------- projects and threads ----------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadMeta {
    pub id: String,
    pub title: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
    /// The Claude conversation to resume. It belongs to `session_account`'s config directory.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_account: Option<String>,
    /// The thread's own account, overriding the project's (a temporary switch).
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default, with = "mode_name")]
    pub mode: Option<PermissionMode>,
    /// The thread's own model and effort, overriding the project's.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// The project's default account; the global default when absent.
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default, with = "mode_name")]
    pub mode: Option<PermissionMode>,
    /// The model and effort new threads start with; Claude Code's defaults when absent.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub threads: Vec<ThreadMeta>,
    /// The project's panes and their tabs (conversations, terminals, the review).
    #[serde(default)]
    pub layout: Option<PaneLayout>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Side by side; the new pane goes on the right.
    Right,
    /// Stacked; the new pane goes below.
    Down,
}

/// One saved tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabState {
    /// A conversation, by thread id.
    Thread { id: String },
    /// A shell, by its working directory.
    Terminal { cwd: PathBuf },
    /// The review of the project's changes.
    Review,
}

/// A saved layout: panes of tabs, split right or down.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout {
    Pane {
        tabs: Vec<TabState>,
        active: usize,
    },
    Split {
        direction: SplitDirection,
        /// The first pane's share, 0.15 to 0.85.
        ratio: f32,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    /// Thread ids anywhere in the layout.
    pub fn thread_ids(&self) -> Vec<String> {
        match self {
            PaneLayout::Pane { tabs, .. } => tabs
                .iter()
                .filter_map(|t| match t {
                    TabState::Thread { id } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            PaneLayout::Split { first, second, .. } => {
                let mut ids = first.thread_ids();
                ids.extend(second.thread_ids());
                ids
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub projects: Vec<Project>,
    #[serde(default)]
    pub selected_thread: Option<String>,
}

impl State {
    pub fn load(paths: &Paths) -> anyhow::Result<Self> {
        let file = paths.state_file();
        if !file.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&file)?;
        serde_json::from_str(&text).with_context(|| format!("reading {}", file.display()))
    }

    /// Write atomically: a crash mid-save never leaves a half-written state file.
    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        fs::create_dir_all(&paths.data_dir)?;
        let file = paths.state_file();
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, &file)?;
        Ok(())
    }

    /// The project for `path`, adding it if it's new.
    pub fn add_project(&mut self, path: &Path) -> &mut Project {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(ix) = self.projects.iter().position(|p| p.path == path) {
            return &mut self.projects[ix];
        }
        let name =
            path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
        self.projects.push(Project {
            id: new_id(),
            name,
            path,
            account: None,
            mode: None,
            model: None,
            effort: None,
            threads: Vec::new(),
            layout: None,
        });
        self.projects.last_mut().unwrap()
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }
    pub fn project_mut(&mut self, id: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    /// The project holding `thread_id`, and the thread.
    pub fn thread(&self, thread_id: &str) -> Option<(&Project, &ThreadMeta)> {
        self.projects.iter().find_map(|p| p.threads.iter().find(|t| t.id == thread_id).map(|t| (p, t)))
    }
    pub fn thread_mut(&mut self, thread_id: &str) -> Option<&mut ThreadMeta> {
        self.projects.iter_mut().flat_map(|p| p.threads.iter_mut()).find(|t| t.id == thread_id)
    }

    pub fn new_thread(&mut self, project_id: &str) -> Option<String> {
        let now = now();
        let project = self.project_mut(project_id)?;
        let id = new_id();
        project.threads.insert(
            0,
            ThreadMeta {
                id: id.clone(),
                title: None,
                created_at: now,
                updated_at: now,
                session_id: None,
                session_account: None,
                account: None,
                mode: None,
                model: None,
                effort: None,
            },
        );
        Some(id)
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Permission modes are stored by Claude's names ("default", "acceptEdits", ...).
mod mode_name {
    use super::PermissionMode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(mode: &Option<PermissionMode>, s: S) -> Result<S::Ok, S::Error> {
        match mode {
            Some(mode) => s.serialize_some(mode.claude_name()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PermissionMode>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.and_then(|name| PermissionMode::from_claude_name(&name)))
    }
}

// ---------- transcripts ----------

/// One line of a thread's transcript file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Prompt {
        text: String,
        /// Attached images, as file names in the thread's `<id>.files/` folder.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Event {
        event: AgentEvent,
    },
    Decision {
        request_id: String,
        outcome: String,
    },
    /// The thread moved to another account (shown as a notice when replayed).
    AccountSwitch {
        from: String,
        to: String,
    },
    /// A new overview, covering the conversation up to `mark` (an index into its plain text).
    Overview {
        overview: crate::overview::Overview,
        mark: usize,
    },
}

/// Appends records to a transcript file.
pub struct TranscriptWriter {
    file: fs::File,
}

impl TranscriptWriter {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    pub fn append(&mut self, record: &Record) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        Ok(())
    }
}

/// Read a transcript; lines that don't parse (a newer TJ, or a torn last line) are skipped.
pub fn read_transcript(path: &Path) -> Vec<Record> {
    let Ok(file) = fs::File::open(path) else { return Vec::new() };
    BufReader::new(file).lines().map_while(Result::ok).filter_map(|line| serde_json::from_str(&line).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tj-test-{name}-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_extra_config_dirs_with_a_login() {
        let home = temp_dir("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".claude-work")).unwrap();
        fs::write(home.join(".claude-work/.credentials.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".claude-empty")).unwrap(); // no login: ignored

        let accounts = Accounts::detect(&home);
        let names: Vec<_> = accounts.accounts.iter().map(|a| (a.id.as_str(), a.config_dir.as_deref())).collect();
        assert_eq!(names, vec![("personal", None), ("work", Some("~/.claude-work"))]);
        assert_eq!(accounts.default, "personal");
    }

    #[test]
    fn a_single_login_is_called_default() {
        let home = temp_dir("solo");
        let accounts = Accounts::detect(&home);
        assert_eq!(accounts.accounts.len(), 1);
        assert_eq!(accounts.accounts[0].name, "Default");
    }

    #[test]
    fn accounts_file_round_trips_and_is_created_once() {
        let dir = temp_dir("accounts");
        let paths = Paths::in_dir(&dir);
        let home = temp_dir("home2");
        let created = Accounts::load_or_create(&paths, &home).unwrap();
        let text = fs::read_to_string(paths.accounts_file()).unwrap();
        assert!(text.starts_with("# TJ's Claude accounts"));
        assert_eq!(Accounts::load_or_create(&paths, &home).unwrap(), created);
    }

    #[test]
    fn account_resolution_prefers_thread_then_project_then_default() {
        let accounts = Accounts {
            default: "personal".into(),
            accounts: vec![
                Account { id: "personal".into(), name: "Personal".into(), config_dir: None },
                Account { id: "work".into(), name: "Work".into(), config_dir: Some("~/.claude-work".into()) },
            ],
        };
        assert_eq!(accounts.resolve(None, None).id, "personal");
        assert_eq!(accounts.resolve(None, Some("work")).id, "work");
        assert_eq!(accounts.resolve(Some("personal"), Some("work")).id, "personal");
        assert_eq!(accounts.resolve(Some("gone"), Some("work")).id, "work");
        let home = PathBuf::from("/home/me");
        assert_eq!(accounts.get("work").unwrap().config_path(&home), Some(PathBuf::from("/home/me/.claude-work")));
    }

    #[test]
    fn state_round_trips_with_modes_by_claude_name() {
        let dir = temp_dir("state");
        let paths = Paths::in_dir(&dir);
        let mut state = State::default();
        let project_id = {
            let project = state.add_project(&dir);
            project.mode = Some(PermissionMode::AcceptEdits);
            project.id.clone()
        };
        assert_eq!(state.add_project(&dir).id, project_id, "adding the same folder twice reuses it");
        let thread = state.new_thread(&project_id).unwrap();
        state.thread_mut(&thread).unwrap().account = Some("work".into());
        state.save(&paths).unwrap();

        let text = fs::read_to_string(paths.state_file()).unwrap();
        assert!(text.contains("\"acceptEdits\""));
        assert_eq!(State::load(&paths).unwrap(), state);
    }

    #[test]
    fn transcripts_append_and_skip_bad_lines() {
        let dir = temp_dir("transcript");
        let path = dir.join("t.jsonl");
        let mut writer = TranscriptWriter::open(&path).unwrap();
        writer.append(&Record::Prompt { text: "hi".into(), images: vec![] }).unwrap();
        writer.append(&Record::Event { event: AgentEvent::TextDelta("hello".into()) }).unwrap();
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"kind\":\"torn").unwrap();
        assert_eq!(
            read_transcript(&path),
            vec![
                Record::Prompt { text: "hi".into(), images: vec![] },
                Record::Event { event: AgentEvent::TextDelta("hello".into()) }
            ]
        );
    }
}
