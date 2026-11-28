//! Tracked pull requests: the PRs, merge requests and mailed patches a project's sessions sent
//! out, wherever they went. A project like a folder of forks sends them to several repositories
//! and hosts at once (GitHub, a GitLab, a mailing list), so the GitHub tab, which shows one
//! repository, can't keep sight of them all.
//!
//! They are kept in the project's notes file ([`crate::notes::Notebook::prs`]), so they share its
//! lock, its reloading and its cleanup when a project is removed. Claude adds them through the
//! notes server's `pr_track` tool ([`crate::notes_mcp`]), and TJ adds any it sees `gh pr create`
//! print. The GitHub tab's Other repos page ([`crate::prs_view`]) lists them and keeps their state fresh: GitHub
//! through `gh`, GitLab through its API, and anything else by hand.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::notes::{Author, Source};

/// Where a pull request lives, which decides how its state is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forge {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    /// A patch series on a mailing list (lore.kernel.org, patchwork, ...).
    MailingList,
    /// Any other link; its state is set by hand.
    #[default]
    Other,
}

impl Forge {
    pub fn label(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
            Forge::MailingList => "Mailing list",
            Forge::Other => "Link",
        }
    }

    /// Whether TJ can read its state; the others are set by hand.
    pub fn refreshes(self) -> bool {
        matches!(self, Forge::GitHub | Forge::GitLab)
    }
}

/// Where a pull request is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    #[default]
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrStatus {
    pub const ALL: [PrStatus; 4] = [PrStatus::Open, PrStatus::Draft, PrStatus::Merged, PrStatus::Closed];

    pub fn name(self) -> &'static str {
        match self {
            PrStatus::Open => "open",
            PrStatus::Draft => "draft",
            PrStatus::Merged => "merged",
            PrStatus::Closed => "closed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PrStatus::Open => "Open",
            PrStatus::Draft => "Draft",
            PrStatus::Merged => "Merged",
            PrStatus::Closed => "Closed",
        }
    }

    pub fn parse(name: &str) -> Option<PrStatus> {
        match name.trim().to_ascii_lowercase().as_str() {
            "open" | "opened" | "sent" | "pending" => Some(PrStatus::Open),
            "draft" => Some(PrStatus::Draft),
            "merged" | "accepted" | "applied" => Some(PrStatus::Merged),
            "closed" | "rejected" | "abandoned" | "superseded" => Some(PrStatus::Closed),
            _ => None,
        }
    }

    /// Still waiting on someone: open or draft.
    pub fn is_active(self) -> bool {
        matches!(self, PrStatus::Open | PrStatus::Draft)
    }
}

/// How a pull request's checks stand, rolled up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    #[default]
    None,
    Pending,
    Passing,
    Failing,
}

impl Checks {
    pub fn label(self) -> &'static str {
        match self {
            Checks::None => "No checks",
            Checks::Pending => "Checks running",
            Checks::Passing => "Checks passing",
            Checks::Failing => "Checks failing",
        }
    }
}

/// Where its review stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Review {
    #[default]
    None,
    Required,
    Approved,
    ChangesRequested,
}

impl Review {
    pub fn label(self) -> &'static str {
        match self {
            Review::None => "",
            Review::Required => "Review required",
            Review::Approved => "Approved",
            Review::ChangesRequested => "Changes requested",
        }
    }
}

/// A pull request (or merge request, or patch) being kept track of.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackedPr {
    /// Its web address, tidied ([`parse_url`]); no two tracked PRs share one.
    pub url: String,
    #[serde(default)]
    pub forge: Forge,
    /// `github.com`, `gitlab.freedesktop.org`, `lore.kernel.org`.
    #[serde(default)]
    pub host: String,
    /// `omacom/omarchy`, `upower/power-profiles-daemon`, a list's name, or empty.
    #[serde(default)]
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: PrStatus,
    #[serde(default)]
    pub checks: Checks,
    #[serde(default)]
    pub review: Review,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// The branch it merges, and the one it merges into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// What it's for, or what it's waiting on, in a line or two.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The local checkout its branch is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// The thread that sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// Who started tracking it.
    #[serde(default)]
    pub added_by: Author,
    pub added: u64,
    /// When the host last saw it change (seconds since the epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<u64>,
    /// When TJ last read its state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<u64>,
    /// Why the last read failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TrackedPr {
    /// A new tracked PR for `url`, or `None` if it isn't a web address.
    pub fn new(url: &str, added_by: Author) -> Option<Self> {
        let link = parse_url(url)?;
        Some(Self {
            url: link.url,
            forge: link.forge,
            host: link.host,
            repo: link.repo,
            number: link.number,
            added_by,
            added: crate::store::now(),
            ..Default::default()
        })
    }

    /// `omacom/omarchy#12`, `upower/power-profiles-daemon!45`, `linux-bluetooth`, or the host.
    pub fn reference(&self) -> String {
        match (self.forge, self.number) {
            (Forge::GitHub, Some(n)) => format!("{}#{n}", self.repo),
            (Forge::GitLab, Some(n)) => format!("{}!{n}", self.repo),
            _ if !self.repo.is_empty() => self.repo.clone(),
            _ => self.host.clone(),
        }
    }

    /// Its title, or its reference until the title is known.
    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() { self.reference() } else { self.title.clone() }
    }

    /// Take in what the host says now.
    pub fn apply(&mut self, fresh: Fresh) {
        self.title = fresh.title;
        self.status = fresh.status;
        self.checks = fresh.checks;
        self.review = fresh.review;
        self.author = fresh.author.or(self.author.take());
        self.head = fresh.head.or(self.head.take());
        self.base = fresh.base.or(self.base.take());
        self.changed = fresh.changed.or(self.changed);
        self.checked = Some(crate::store::now());
        self.error = None;
    }

    /// The read failed: keep what was known, and say why.
    pub fn failed(&mut self, error: String) {
        self.checked = Some(crate::store::now());
        self.error = Some(error);
    }
}

/// What a link says about where it points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    pub number: Option<u64>,
}

/// Read a pull request's link: `https://github.com/<owner>/<repo>/pull/<n>`, a GitLab
/// `https://<host>/<group>/<project>/-/merge_requests/<n>`, a mailing list archive, or any
/// other web address. The address comes back tidied (no query, fragment or trailing parts after
/// the number), so the same PR is always the same address. `None` if it isn't http(s).
pub fn parse_url(url: &str) -> Option<Link> {
    let url = url.trim().trim_matches(|c| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')'));
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let number_at = |ix: usize| parts.get(ix).and_then(|n| n.parse::<u64>().ok());
    // GitHub: owner/repo/pull/N[/files...]
    if (host == "github.com" || host == "www.github.com")
        && parts.len() >= 4
        && parts[2] == "pull"
        && let Some(n) = number_at(3)
    {
        let repo = format!("{}/{}", parts[0], parts[1]);
        return Some(Link {
            url: format!("https://github.com/{repo}/pull/{n}"),
            forge: Forge::GitHub,
            host: "github.com".into(),
            repo,
            number: Some(n),
        });
    }
    // GitLab: group/sub/project/-/merge_requests/N[/diffs...]
    if let Some(dash) = parts.iter().position(|p| *p == "-")
        && dash >= 2
        && parts.get(dash + 1) == Some(&"merge_requests")
        && let Some(n) = number_at(dash + 2)
    {
        let repo = parts[..dash].join("/");
        return Some(Link {
            url: format!("https://{host}/{repo}/-/merge_requests/{n}"),
            forge: Forge::GitLab,
            host,
            repo,
            number: Some(n),
        });
    }
    let tidy =
        format!("https://{host}{}", if parts.is_empty() { String::new() } else { format!("/{}", parts.join("/")) });
    // Mailing lists: lore.kernel.org/<list>/<message id>, patchwork's project/<name>/...,
    // lists.*/... and the like.
    let list = if host == "lore.kernel.org" || host.starts_with("lore.") {
        parts.first().map(|l| l.to_string())
    } else if host.starts_with("patchwork.") {
        parts.iter().position(|p| *p == "project").and_then(|ix| parts.get(ix + 1)).map(|l| l.to_string())
    } else if host.starts_with("lists.") || host.contains("mail-archive") || host == "marc.info" {
        Some(String::new())
    } else {
        None
    };
    if let Some(list) = list {
        return Some(Link { url: tidy, forge: Forge::MailingList, host, repo: list, number: None });
    }
    Some(Link { url: tidy, forge: Forge::Other, host, repo: String::new(), number: None })
}

/// Pull request links in some text (a command's output), tidied, each once, in order. Only
/// GitHub pull requests and GitLab merge requests: a bare link could be anything.
pub fn pr_links(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '`')) {
        if !word.starts_with("http") {
            continue;
        }
        if let Some(link) = parse_url(word)
            && matches!(link.forge, Forge::GitHub | Forge::GitLab)
            && !found.contains(&link.url)
        {
            found.push(link.url);
        }
    }
    found
}

/// Whether a shell command opens a pull request, so the links it prints are ones it made (and
/// not, say, `gh pr list`'s).
pub fn opens_pr(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let has = |seq: &[&str]| words.windows(seq.len()).any(|w| w == seq);
    has(&["gh", "pr", "create"])
        || has(&["glab", "mr", "create"])
        || (words.contains(&"push") && command.contains("merge_request.create"))
}

/// The folder a shell command works in, when it starts with `cd <absolute path>`.
pub fn command_folder(command: &str) -> Option<PathBuf> {
    let rest = command.trim_start().strip_prefix("cd ")?;
    let path = rest.split(['&', ';', '|', '\n']).next()?.trim();
    let path = path.trim_matches(|c| c == '"' || c == '\'');
    path.starts_with('/').then(|| PathBuf::from(path))
}

/// Whether a folder has git repositories (or worktrees) in its subfolders, a few levels down:
/// a folder of forks and clones rather than one repository. Hidden folders, build output and
/// dependencies aren't looked in, and the search gives up after a few hundred folders.
pub fn has_nested_repos(root: &std::path::Path) -> bool {
    const DEPTH: usize = 3;
    const MAX_DIRS: usize = 400;
    let skip = |name: &str| name.starts_with('.') || matches!(name, "target" | "node_modules" | "build" | "dist");
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0)]);
    let mut seen = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if skip(&name.to_string_lossy()) {
                continue;
            }
            let path = entry.path();
            if path.join(".git").exists() {
                return true;
            }
            seen += 1;
            if seen >= MAX_DIRS {
                return false;
            }
            if depth + 1 < DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    false
}

/// A pull request's state as its host gives it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fresh {
    pub title: String,
    pub status: PrStatus,
    pub checks: Checks,
    pub review: Review,
    pub author: Option<String>,
    pub head: Option<String>,
    pub base: Option<String>,
    pub changed: Option<u64>,
}

/// Read a tracked PR's state from its host; `None` for one whose state is set by hand. This
/// runs `gh` or `curl` and waits, so call it off the main thread.
pub fn fetch(pr: &TrackedPr) -> anyhow::Result<Option<Fresh>> {
    match pr.forge {
        Forge::GitHub => Ok(Some(from_github(&crate::github::pull_request_at(&pr.url)?))),
        Forge::GitLab => {
            let number = pr.number.ok_or_else(|| anyhow::anyhow!("no merge request number in {}", pr.url))?;
            Ok(Some(from_gitlab(&gitlab_merge_request(&pr.host, &pr.repo, number)?)))
        }
        Forge::MailingList | Forge::Other => Ok(None),
    }
}

/// Read several at once, in parallel; each result is paired with its PR's address.
pub fn fetch_all(prs: Vec<TrackedPr>) -> Vec<(String, anyhow::Result<Option<Fresh>>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = prs.iter().map(|pr| scope.spawn(move || (pr.url.clone(), fetch(pr)))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

fn from_github(pr: &crate::github::PullRequest) -> Fresh {
    use crate::github::{ChecksState, PrState};
    Fresh {
        title: pr.title.clone(),
        status: match pr.state {
            PrState::Open => PrStatus::Open,
            PrState::Draft => PrStatus::Draft,
            PrState::Merged => PrStatus::Merged,
            PrState::Closed => PrStatus::Closed,
        },
        checks: match pr.checks_state() {
            ChecksState::None => Checks::None,
            ChecksState::Pending => Checks::Pending,
            ChecksState::Passing => Checks::Passing,
            ChecksState::Failing => Checks::Failing,
        },
        review: match pr.review_decision.as_str() {
            "APPROVED" => Review::Approved,
            "CHANGES_REQUESTED" => Review::ChangesRequested,
            "REVIEW_REQUIRED" => Review::Required,
            _ => Review::None,
        },
        author: Some(pr.author.clone()).filter(|a| !a.is_empty()),
        head: Some(pr.head.clone()).filter(|h| !h.is_empty()),
        base: Some(pr.base.clone()).filter(|b| !b.is_empty()),
        changed: parse_time(&pr.updated_at),
    }
}

/// A merge request from GitLab's API. Public projects need no token.
fn gitlab_merge_request(host: &str, repo: &str, number: u64) -> anyhow::Result<Value> {
    let project: String = repo
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    let url = format!("https://{host}/api/v4/projects/{project}/merge_requests/{number}");
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "20", "-H", "Accept: application/json", "-w", "\n%{http_code}", &url])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| anyhow::anyhow!("could not run curl: {err}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, ""));
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", err.trim().trim_start_matches("curl: "));
    }
    match code.trim() {
        "200" => Ok(serde_json::from_str(body)?),
        "401" | "403" | "404" => anyhow::bail!("{host} doesn't show {repo}!{number} without signing in (HTTP {code})"),
        code => anyhow::bail!("{host} answered HTTP {code}"),
    }
}

fn from_gitlab(v: &Value) -> Fresh {
    let text = |key: &str| v[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let draft = v["draft"].as_bool().or(v["work_in_progress"].as_bool()).unwrap_or(false);
    let status = match v["state"].as_str().unwrap_or("opened") {
        "merged" => PrStatus::Merged,
        "closed" | "locked" => PrStatus::Closed,
        _ if draft => PrStatus::Draft,
        _ => PrStatus::Open,
    };
    let pipeline = v["head_pipeline"]["status"].as_str().or(v["pipeline"]["status"].as_str());
    let checks = match pipeline {
        Some("success") => Checks::Passing,
        Some("failed" | "canceled") => Checks::Failing,
        Some("skipped") | None => Checks::None,
        Some(_) => Checks::Pending,
    };
    let review = match v["detailed_merge_status"].as_str() {
        Some("not_approved") => Review::Required,
        _ => Review::None,
    };
    Fresh {
        title: v["title"].as_str().unwrap_or_default().to_string(),
        status,
        checks,
        review,
        author: v["author"]["username"].as_str().map(str::to_owned),
        head: text("source_branch"),
        base: text("target_branch"),
        changed: v["updated_at"].as_str().and_then(parse_time),
    }
}

/// An RFC 3339 time ("2026-11-27T10:00:00Z") as seconds since the epoch.
fn parse_time(text: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().and_then(|t| u64::try_from(t.timestamp()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_links_are_tidied() {
        let link = parse_url("https://github.com/omacom/omarchy/pull/123/files?w=1#diff").unwrap();
        assert_eq!(link.url, "https://github.com/omacom/omarchy/pull/123");
        assert_eq!((link.forge, link.repo.as_str(), link.number), (Forge::GitHub, "omacom/omarchy", Some(123)));
    }

    #[test]
    fn gitlab_links_keep_their_group_path() {
        let link =
            parse_url("https://gitlab.freedesktop.org/upower/power-profiles-daemon/-/merge_requests/45/diffs").unwrap();
        assert_eq!(link.url, "https://gitlab.freedesktop.org/upower/power-profiles-daemon/-/merge_requests/45");
        assert_eq!(link.forge, Forge::GitLab);
        assert_eq!(
            (link.host.as_str(), link.repo.as_str()),
            ("gitlab.freedesktop.org", "upower/power-profiles-daemon")
        );
        let nested = parse_url("https://gitlab.com/a/b/c/-/merge_requests/7").unwrap();
        assert_eq!(nested.repo, "a/b/c");
    }

    #[test]
    fn mailing_lists_and_other_links() {
        let lore = parse_url("https://lore.kernel.org/linux-bluetooth/20261101.123@x.org/").unwrap();
        assert_eq!((lore.forge, lore.repo.as_str()), (Forge::MailingList, "linux-bluetooth"));
        assert_eq!(lore.url, "https://lore.kernel.org/linux-bluetooth/20261101.123@x.org");
        let pw = parse_url("https://patchwork.kernel.org/project/bluetooth/patch/123/").unwrap();
        assert_eq!((pw.forge, pw.repo.as_str()), (Forge::MailingList, "bluetooth"));
        let other = parse_url("https://codeberg.org/a/b/pulls/3").unwrap();
        assert_eq!(other.forge, Forge::Other);
        assert!(parse_url("not a url").is_none());
        assert!(parse_url("ftp://x.org/a").is_none());
    }

    #[test]
    fn references_name_the_repo_and_number() {
        let gh = TrackedPr::new("https://github.com/o/r/pull/9", Author::Agent).unwrap();
        assert_eq!(gh.reference(), "o/r#9");
        assert_eq!(gh.display_title(), "o/r#9", "the reference stands in for an unknown title");
        let gl = TrackedPr::new("https://gitlab.com/g/p/-/merge_requests/4", Author::User).unwrap();
        assert_eq!(gl.reference(), "g/p!4");
    }

    #[test]
    fn links_are_found_in_command_output() {
        let out = "Creating pull request for me:fix into main in o/r\n\nhttps://github.com/o/r/pull/12\n\
                   see also https://github.com/o/r/pull/12 and <https://gitlab.com/g/p/-/merge_requests/3>";
        assert_eq!(pr_links(out), ["https://github.com/o/r/pull/12", "https://gitlab.com/g/p/-/merge_requests/3"]);
        assert!(pr_links("https://github.com/o/r/issues/1").is_empty());
    }

    #[test]
    fn commands_that_open_prs() {
        assert!(opens_pr("cd /x && gh pr create --fill --base main"));
        assert!(opens_pr("glab mr create -t x"));
        assert!(opens_pr("git push -o merge_request.create origin fix"));
        assert!(!opens_pr("gh pr list"));
        assert!(!opens_pr("gh pr view 3"));
        assert_eq!(command_folder("cd /mnt/data/repo && gh pr create"), Some(PathBuf::from("/mnt/data/repo")));
        assert_eq!(command_folder("cd \"/a b\"; ls"), Some(PathBuf::from("/a b")));
        assert_eq!(command_folder("cd rel && ls"), None);
    }

    #[test]
    fn nested_repos_are_found_a_few_levels_down() {
        let root = std::env::temp_dir().join(format!("tj-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/repo/.git")).unwrap();
        assert!(!has_nested_repos(&root), "hidden folders aren't looked in");
        std::fs::create_dir_all(root.join("upstream/omarchy/.git")).unwrap();
        assert!(has_nested_repos(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn statuses_parse_their_names_and_synonyms() {
        assert_eq!(PrStatus::parse("Merged"), Some(PrStatus::Merged));
        assert_eq!(PrStatus::parse("applied"), Some(PrStatus::Merged));
        assert_eq!(PrStatus::parse("opened"), Some(PrStatus::Open));
        assert_eq!(PrStatus::parse("nope"), None);
        assert!(PrStatus::Draft.is_active() && !PrStatus::Closed.is_active());
    }

    #[test]
    fn gitlab_answers_are_read() {
        let v = serde_json::json!({
            "title": "Add cpufreq driver", "state": "opened", "draft": false,
            "author": {"username": "turbine.bmw"}, "source_branch": "cpufreq", "target_branch": "main",
            "updated_at": "2026-11-20T10:00:00.000Z", "head_pipeline": {"status": "running"},
            "detailed_merge_status": "not_approved"
        });
        let fresh = from_gitlab(&v);
        assert_eq!((fresh.status, fresh.checks, fresh.review), (PrStatus::Open, Checks::Pending, Review::Required));
        assert_eq!(fresh.author.as_deref(), Some("turbine.bmw"));
        assert!(fresh.changed.is_some());
        let merged = from_gitlab(&serde_json::json!({"state": "merged", "draft": true}));
        assert_eq!(merged.status, PrStatus::Merged, "merged wins over draft");
    }

    #[test]
    fn applying_a_read_keeps_what_it_lacks() {
        let mut pr = TrackedPr::new("https://github.com/o/r/pull/1", Author::Agent).unwrap();
        pr.head = Some("fix".into());
        pr.error = Some("old".into());
        pr.apply(Fresh { title: "Fix".into(), status: PrStatus::Merged, ..Default::default() });
        assert_eq!((pr.title.as_str(), pr.status, pr.head.as_deref()), ("Fix", PrStatus::Merged, Some("fix")));
        assert!(pr.error.is_none() && pr.checked.is_some());
    }
}
