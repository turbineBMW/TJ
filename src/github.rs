//! GitHub for a project, through the `gh` command on the right account.
//!
//! `gh` keeps one token per account in the keyring, but only one account is "active" at a
//! time. TJ never switches it: each call gets the token of the account the project uses
//! (`gh auth token --user <login>`) in `GH_TOKEN`, so two projects on two accounts work side by
//! side. Which account a project uses is picked from the remote (an owner or an SSH host alias
//! that names a login), else the first account that can push to the repository, and can be
//! changed per project.
//!
//! Local git work (fetch, pull, push, switching branches, checking out a pull request) runs
//! `git` in the project folder against the project's own remote URL, so an SSH host alias keeps
//! its key. For an HTTPS remote, git asks `gh` for credentials with the same token.
//!
//! Everything here blocks: run it off the UI thread.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

pub const HOST: &str = "github.com";

// ---------- running gh and git ----------

/// The `gh` binary, found once on `PATH` (or in `~/.local/bin`, where the installer puts it).
fn gh_binary() -> PathBuf {
    static FOUND: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let mut dirs: Vec<PathBuf> =
                std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
            dirs.push(crate::store::home_dir().join(".local/bin"));
            dirs.into_iter().map(|d| d.join("gh")).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("gh"))
        })
        .clone()
}

/// Whether `gh` is installed.
pub fn gh_installed() -> bool {
    Command::new(gh_binary()).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn gh_command(token: Option<&str>) -> Command {
    let mut cmd = Command::new(gh_binary());
    cmd.env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::null());
    match token {
        Some(token) => cmd.env("GH_TOKEN", token),
        None => cmd.env_remove("GH_TOKEN"),
    };
    cmd
}

/// The first useful line of a failed command's output.
fn failure(what: &str, out: &std::process::Output) -> anyhow::Error {
    let text = String::from_utf8_lossy(&out.stderr);
    let text = if text.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { text };
    let message = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    let message = message.strip_prefix("error: ").or_else(|| message.strip_prefix("fatal: ")).unwrap_or(&message);
    if message.is_empty() { anyhow::anyhow!("{what} failed") } else { anyhow::anyhow!("{message}") }
}

fn run_gh(token: Option<&str>, args: &[&str], input: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = gh_command(token);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().context("could not run gh (is the GitHub CLI installed?)")?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(failure("gh", &out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `gh api` with a JSON body; returns the parsed response (`null` for an empty one).
fn api(token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let mut args = vec!["api", "-X", method, path, "-H", "Accept: application/vnd.github+json"];
    let input = body.map(Value::to_string);
    if input.is_some() {
        args.extend(["--input", "-"]);
    }
    let out = run_gh(Some(token), &args, input.as_deref())?;
    if out.trim().is_empty() { Ok(Value::Null) } else { Ok(serde_json::from_str(&out)?) }
}

fn git_command(root: &Path, token: Option<&str>) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
    if let Some(token) = token {
        // Only HTTPS remotes ask for credentials; SSH ones keep their host alias's key.
        let helper = format!("credential.helper=!'{}' auth git-credential", gh_binary().display());
        cmd.args(["-c", "credential.helper=", "-c", &helper]).env("GH_TOKEN", token);
    }
    cmd
}

fn git(root: &Path, token: Option<&str>, args: &[&str]) -> anyhow::Result<String> {
    let out = git_command(root, token).args(args).output().context("could not run git")?;
    if !out.status.success() {
        return Err(failure("git", &out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------- accounts ----------

/// The GitHub accounts `gh` is logged in to, the active one first.
pub fn accounts() -> anyhow::Result<Vec<String>> {
    let out = run_gh(None, &["auth", "status", "--hostname", HOST, "--json", "hosts"], None)?;
    parse_accounts(&out)
}

pub fn parse_accounts(json: &str) -> anyhow::Result<Vec<String>> {
    let value: Value = serde_json::from_str(json)?;
    let mut entries: Vec<(bool, String)> = value["hosts"][HOST]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["state"].as_str().is_none_or(|s| s == "success"))
        .filter_map(|a| Some((a["active"].as_bool().unwrap_or(false), a["login"].as_str()?.to_string())))
        .collect();
    entries.sort_by_key(|(active, _)| !active);
    Ok(entries.into_iter().map(|(_, login)| login).collect())
}

/// The account's token, from gh's keyring. It never leaves this process except as `GH_TOKEN`
/// for the `gh` and `git` TJ runs.
pub fn token(login: &str) -> anyhow::Result<String> {
    let out = run_gh(None, &["auth", "token", "--hostname", HOST, "--user", login], None)?;
    let token = out.trim().to_string();
    if token.is_empty() {
        bail!("gh has no token for {login}; run `gh auth login`");
    }
    Ok(token)
}

/// The account most likely meant for a repository, from its remote alone: the one named like
/// the owner, else one named in the SSH host alias (`github_fieldhook` -> `fieldhook`).
pub fn guess_account(accounts: &[String], remote: &Remote) -> Option<String> {
    let owner = remote.owner.to_lowercase();
    let alias = remote.host.to_lowercase();
    accounts
        .iter()
        .find(|a| a.to_lowercase() == owner)
        .or_else(|| accounts.iter().find(|a| alias != HOST && alias.contains(&a.to_lowercase())))
        .cloned()
}

/// Pick the account for a repository: the guess from the remote if it can see the repository,
/// else the first that can push to it, else the first that can see it.
pub fn pick_account(accounts: &[String], remote: &Remote) -> anyhow::Result<(String, String, RepoInfo)> {
    let guess = guess_account(accounts, remote);
    let mut order: Vec<&String> = guess.iter().collect();
    order.extend(accounts.iter().filter(|a| Some(*a) != guess.as_ref()));
    let mut readable = None;
    let mut last_error = None;
    for login in order {
        let token = match token(login) {
            Ok(token) => token,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };
        match repo_info(&token, remote) {
            Ok(info) if info.can_push || guess.as_ref() == Some(login) => return Ok((login.clone(), token, info)),
            Ok(info) => {
                readable.get_or_insert((login.clone(), token, info));
            }
            Err(err) => last_error = Some(err),
        }
    }
    readable.ok_or_else(|| last_error.unwrap_or_else(|| anyhow::anyhow!("no GitHub account can see {}", remote.slug())))
}

// ---------- the repository ----------

/// A project's GitHub remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// The git remote's name (`origin`).
    pub name: String,
    /// The host part of the URL: `github.com`, or an SSH alias for it.
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl Remote {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn web_url(&self) -> String {
        format!("https://{HOST}/{}", self.slug())
    }
}

/// Read `host`, `owner` and `repo` from a remote URL: `git@host:owner/repo.git`,
/// `host:owner/repo`, `ssh://git@host[:port]/owner/repo.git` or `https://host/owner/repo`.
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    let (host, path) = if let Some(rest) = url.split_once("://").map(|(_, rest)| rest) {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host.to_string(), path.to_string())
    } else {
        // scp-like: [user@]host:path
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority.rsplit('@').next()?.to_string(), path.to_string())
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() || host.is_empty() {
        return None;
    }
    Some((host, owner.to_string(), repo.to_string()))
}

/// Whether a host from a remote URL is GitHub: github.com itself, or an SSH alias that ssh
/// resolves to it.
fn is_github_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case(HOST) || host.eq_ignore_ascii_case("ssh.github.com") {
        return true;
    }
    Command::new("ssh")
        .args(["-G", host])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                l.strip_prefix("hostname ").is_some_and(|h| {
                    let h = h.trim();
                    h.eq_ignore_ascii_case(HOST) || h.eq_ignore_ascii_case("ssh.github.com")
                })
            })
        })
        .unwrap_or(false)
}

/// The project's GitHub remote: `origin` if it's on GitHub, else the first that is.
pub fn remote(root: &Path) -> anyhow::Result<Option<Remote>> {
    let out = git_command(root, None).args(["remote", "-v"]).output().context("could not run git")?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut remotes: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.ends_with("(fetch)"))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect();
    remotes.sort_by_key(|(name, _)| name != "origin");
    for (name, url) in remotes {
        if let Some((host, owner, repo)) = parse_remote_url(&url)
            && is_github_host(&host)
        {
            return Ok(Some(Remote { name, host, owner, repo }));
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoInfo {
    pub default_branch: String,
    pub description: Option<String>,
    pub private: bool,
    pub can_push: bool,
    pub allow_squash: bool,
    pub allow_merge: bool,
    pub allow_rebase: bool,
    pub delete_branch_on_merge: bool,
}

pub fn repo_info(token: &str, remote: &Remote) -> anyhow::Result<RepoInfo> {
    let v = api(token, "GET", &format!("repos/{}", remote.slug()), None)?;
    Ok(parse_repo_info(&v))
}

pub fn parse_repo_info(v: &Value) -> RepoInfo {
    let flag = |key: &str, default: bool| v[key].as_bool().unwrap_or(default);
    RepoInfo {
        default_branch: v["default_branch"].as_str().unwrap_or("main").to_string(),
        description: v["description"].as_str().filter(|d| !d.trim().is_empty()).map(str::to_owned),
        private: flag("private", false),
        can_push: v["permissions"]["push"].as_bool().unwrap_or(false),
        allow_squash: flag("allow_squash_merge", true),
        allow_merge: flag("allow_merge_commit", true),
        allow_rebase: flag("allow_rebase_merge", true),
        delete_branch_on_merge: flag("delete_branch_on_merge", false),
    }
}

// ---------- pull requests ----------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrFilter {
    #[default]
    Open,
    Merged,
    Closed,
}

impl PrFilter {
    pub const ALL: [PrFilter; 3] = [PrFilter::Open, PrFilter::Merged, PrFilter::Closed];

    pub fn label(self) -> &'static str {
        match self {
            PrFilter::Open => "Open",
            PrFilter::Merged => "Merged",
            PrFilter::Closed => "Closed",
        }
    }

    fn state(self) -> &'static str {
        match self {
            PrFilter::Open => "open",
            PrFilter::Merged => "merged",
            PrFilter::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            PrState::Open => "Open",
            PrState::Draft => "Draft",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
        }
    }
}

/// How a pull request's checks stand, rolled up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksState {
    None,
    Pending,
    Passing,
    Failing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub state: ChecksState,
    /// What it says now: "Success", "In progress", "Failure".
    pub status: String,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub state: PrState,
    pub cross_repo: bool,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    pub review_decision: String,
    pub checks: Vec<Check>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub comments: usize,
    pub labels: Vec<String>,
    pub updated_at: String,
    pub url: String,
}

impl PullRequest {
    pub fn checks_state(&self) -> ChecksState {
        roll_up(&self.checks)
    }
}

pub fn roll_up(checks: &[Check]) -> ChecksState {
    if checks.is_empty() {
        ChecksState::None
    } else if checks.iter().any(|c| c.state == ChecksState::Failing) {
        ChecksState::Failing
    } else if checks.iter().any(|c| c.state == ChecksState::Pending) {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    }
}

const LIST_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,state,isCrossRepository,reviewDecision,statusCheckRollup,additions,deletions,changedFiles,comments,labels,updatedAt,url";

pub fn pull_requests(token: &str, remote: &Remote, filter: PrFilter) -> anyhow::Result<Vec<PullRequest>> {
    let slug = remote.slug();
    let out = run_gh(
        Some(token),
        &["pr", "list", "-R", &slug, "--state", filter.state(), "-L", "60", "--json", LIST_FIELDS],
        None,
    )?;
    let list: Vec<Value> = serde_json::from_str(&out)?;
    Ok(list.iter().map(parse_pull_request).collect())
}

fn login(v: &Value) -> String {
    v["login"].as_str().unwrap_or("ghost").to_string()
}

pub fn parse_pull_request(v: &Value) -> PullRequest {
    let state = match (v["state"].as_str().unwrap_or("OPEN"), v["isDraft"].as_bool().unwrap_or(false)) {
        ("MERGED", _) => PrState::Merged,
        ("CLOSED", _) => PrState::Closed,
        (_, true) => PrState::Draft,
        _ => PrState::Open,
    };
    PullRequest {
        number: v["number"].as_u64().unwrap_or(0),
        title: v["title"].as_str().unwrap_or("").to_string(),
        author: login(&v["author"]),
        head: v["headRefName"].as_str().unwrap_or("").to_string(),
        base: v["baseRefName"].as_str().unwrap_or("").to_string(),
        state,
        cross_repo: v["isCrossRepository"].as_bool().unwrap_or(false),
        review_decision: v["reviewDecision"].as_str().unwrap_or("").to_string(),
        checks: v["statusCheckRollup"].as_array().into_iter().flatten().map(parse_check).collect(),
        additions: v["additions"].as_u64().unwrap_or(0),
        deletions: v["deletions"].as_u64().unwrap_or(0),
        changed_files: v["changedFiles"].as_u64().unwrap_or(0),
        comments: v["comments"].as_array().map(Vec::len).unwrap_or(0),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str())
            .map(str::to_owned)
            .collect(),
        updated_at: v["updatedAt"].as_str().unwrap_or("").to_string(),
        url: v["url"].as_str().unwrap_or("").to_string(),
    }
}

/// A check run (Actions) or a commit status (other CI).
fn parse_check(v: &Value) -> Check {
    let upper = |key: &str| v[key].as_str().unwrap_or("").to_uppercase();
    let (name, state, status, url) = if v["__typename"] == "StatusContext" {
        let s = upper("state");
        let state = match s.as_str() {
            "SUCCESS" => ChecksState::Passing,
            "PENDING" | "EXPECTED" => ChecksState::Pending,
            _ => ChecksState::Failing,
        };
        (v["context"].as_str().unwrap_or("status"), state, s, v["targetUrl"].as_str())
    } else {
        let (status, conclusion) = (upper("status"), upper("conclusion"));
        let state = if status != "COMPLETED" {
            ChecksState::Pending
        } else {
            match conclusion.as_str() {
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => ChecksState::Passing,
                _ => ChecksState::Failing,
            }
        };
        let shown = if status == "COMPLETED" { conclusion } else { status };
        (v["name"].as_str().unwrap_or("check"), state, shown, v["detailsUrl"].as_str())
    };
    Check { name: name.to_string(), state, status: title_case(&status), url: url.map(str::to_owned) }
}

/// `IN_PROGRESS` -> `In progress`.
pub fn title_case(text: &str) -> String {
    let lower = text.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Something said on a pull request: a comment, or a review with its verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineItem {
    pub author: String,
    pub body: String,
    pub at: String,
    /// For a review: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`.
    pub review: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub headline: String,
    pub author: String,
    pub at: String,
}

/// A comment on a line of the diff.
#[derive(Clone, Debug, PartialEq)]
pub struct LineComment {
    pub id: u64,
    pub path: String,
    /// The line on its side, or `None` when the code has moved on (an outdated comment).
    pub line: Option<u64>,
    /// `LEFT` (the base) or `RIGHT` (the change).
    pub side: String,
    pub author: String,
    pub body: String,
    pub at: String,
    pub reply_to: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PrDetail {
    pub pr: PullRequest,
    pub body: String,
    pub created_at: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    /// `CLEAN`, `BLOCKED`, `BEHIND`, `DIRTY`, `UNSTABLE`, `HAS_HOOKS`, `DRAFT`, `UNKNOWN`.
    pub merge_state: String,
    pub head_sha: String,
    pub timeline: Vec<TimelineItem>,
    pub commits: Vec<Commit>,
    pub line_comments: Vec<LineComment>,
    pub patch: String,
}

const DETAIL_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,state,isCrossRepository,reviewDecision,statusCheckRollup,additions,deletions,changedFiles,comments,labels,updatedAt,url,body,createdAt,mergeable,mergeStateStatus,reviews,commits,headRefOid";

pub fn pull_request(token: &str, remote: &Remote, number: u64) -> anyhow::Result<PrDetail> {
    let (slug, n) = (remote.slug(), number.to_string());
    let out = run_gh(Some(token), &["pr", "view", &n, "-R", &slug, "--json", DETAIL_FIELDS], None)?;
    let v: Value = serde_json::from_str(&out)?;
    let patch = run_gh(Some(token), &["pr", "diff", &n, "-R", &slug, "--color", "never"], None).unwrap_or_default();
    let comments = api(token, "GET", &format!("repos/{slug}/pulls/{number}/comments?per_page=100"), None)?;
    Ok(parse_detail(&v, &comments, patch))
}

pub fn parse_detail(v: &Value, line_comments: &Value, patch: String) -> PrDetail {
    let mut timeline: Vec<TimelineItem> = v["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| TimelineItem {
            author: login(&c["author"]),
            body: c["body"].as_str().unwrap_or("").to_string(),
            at: c["createdAt"].as_str().unwrap_or("").to_string(),
            review: None,
        })
        .collect();
    timeline.extend(v["reviews"].as_array().into_iter().flatten().filter_map(|r| {
        let state = r["state"].as_str().unwrap_or("COMMENTED").to_string();
        let body = r["body"].as_str().unwrap_or("").to_string();
        // A review that's only line comments says nothing here; its comments show on the diff.
        (state != "COMMENTED" || !body.trim().is_empty() || state == "PENDING").then(|| TimelineItem {
            author: login(&r["author"]),
            body,
            at: r["submittedAt"].as_str().unwrap_or("").to_string(),
            review: Some(state),
        })
    }));
    timeline.sort_by(|a, b| a.at.cmp(&b.at));
    let commits = v["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Commit {
            sha: c["oid"].as_str().unwrap_or("").to_string(),
            headline: c["messageHeadline"].as_str().unwrap_or("").to_string(),
            author: c["authors"][0]["login"]
                .as_str()
                .filter(|l| !l.is_empty())
                .or_else(|| c["authors"][0]["name"].as_str())
                .unwrap_or("")
                .to_string(),
            at: c["committedDate"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    PrDetail {
        pr: parse_pull_request(v),
        body: v["body"].as_str().unwrap_or("").to_string(),
        created_at: v["createdAt"].as_str().unwrap_or("").to_string(),
        mergeable: v["mergeable"].as_str().unwrap_or("UNKNOWN").to_string(),
        merge_state: v["mergeStateStatus"].as_str().unwrap_or("UNKNOWN").to_string(),
        head_sha: v["headRefOid"].as_str().unwrap_or("").to_string(),
        timeline,
        commits,
        line_comments: parse_line_comments(line_comments),
        patch,
    }
}

pub fn parse_line_comments(v: &Value) -> Vec<LineComment> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|c| LineComment {
            id: c["id"].as_u64().unwrap_or(0),
            path: c["path"].as_str().unwrap_or("").to_string(),
            line: c["line"].as_u64(),
            side: c["side"].as_str().unwrap_or("RIGHT").to_string(),
            author: login(&c["user"]),
            body: c["body"].as_str().unwrap_or("").to_string(),
            at: c["created_at"].as_str().unwrap_or("").to_string(),
            reply_to: c["in_reply_to_id"].as_u64(),
        })
        .collect()
}

/// What a review says overall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn api_name(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// A line comment written in TJ, sent with the review.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftComment {
    pub path: String,
    /// `LEFT` or `RIGHT`.
    pub side: String,
    pub start_line: u64,
    pub line: u64,
    pub body: String,
}

pub fn review_body(event: ReviewEvent, body: &str, commit: &str, drafts: &[DraftComment]) -> Value {
    let comments: Vec<Value> = drafts
        .iter()
        .map(|d| {
            let mut c = json!({ "path": d.path, "side": d.side, "line": d.line, "body": d.body });
            if d.start_line < d.line {
                c["start_line"] = json!(d.start_line);
                c["start_side"] = json!(d.side);
            }
            c
        })
        .collect();
    let mut review = json!({ "event": event.api_name(), "comments": comments });
    if !body.trim().is_empty() {
        review["body"] = json!(body);
    }
    if !commit.is_empty() {
        review["commit_id"] = json!(commit);
    }
    review
}

pub fn submit_review(
    token: &str,
    remote: &Remote,
    number: u64,
    event: ReviewEvent,
    body: &str,
    commit: &str,
    drafts: &[DraftComment],
) -> anyhow::Result<()> {
    if event != ReviewEvent::Approve && body.trim().is_empty() && drafts.is_empty() {
        bail!("Write something first: a review needs a comment or a line comment.");
    }
    let path = format!("repos/{}/pulls/{number}/reviews", remote.slug());
    api(token, "POST", &path, Some(&review_body(event, body, commit, drafts)))?;
    Ok(())
}

pub fn comment(token: &str, remote: &Remote, number: u64, body: &str) -> anyhow::Result<()> {
    if body.trim().is_empty() {
        bail!("The comment is empty.");
    }
    api(token, "POST", &format!("repos/{}/issues/{number}/comments", remote.slug()), Some(&json!({ "body": body })))?;
    Ok(())
}

/// Reply on a line comment's thread.
pub fn reply(token: &str, remote: &Remote, number: u64, to: u64, body: &str) -> anyhow::Result<()> {
    if body.trim().is_empty() {
        bail!("The reply is empty.");
    }
    let path = format!("repos/{}/pulls/{number}/comments/{to}/replies", remote.slug());
    api(token, "POST", &path, Some(&json!({ "body": body })))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] = [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase];

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Merge => "Create a merge commit",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }

    pub fn allowed(self, info: &RepoInfo) -> bool {
        match self {
            MergeMethod::Squash => info.allow_squash,
            MergeMethod::Merge => info.allow_merge,
            MergeMethod::Rebase => info.allow_rebase,
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        }
    }
}

/// Merge on GitHub (nothing local changes), then delete the head branch if asked and it lives
/// in this repository.
pub fn merge(
    token: &str,
    remote: &Remote,
    pr: &PullRequest,
    method: MergeMethod,
    head_sha: &str,
    delete_branch: bool,
) -> anyhow::Result<()> {
    let slug = remote.slug();
    let mut body = json!({ "merge_method": method.api_name() });
    if !head_sha.is_empty() {
        // Refuse if someone pushed since the page was loaded.
        body["sha"] = json!(head_sha);
    }
    api(token, "PUT", &format!("repos/{slug}/pulls/{}/merge", pr.number), Some(&body))?;
    if delete_branch && !pr.cross_repo && !pr.head.is_empty() {
        // Already gone (the repository deletes merged branches) is fine.
        let _ = api(token, "DELETE", &format!("repos/{slug}/git/refs/heads/{}", pr.head), None);
    }
    Ok(())
}

pub fn set_ready(token: &str, remote: &Remote, number: u64) -> anyhow::Result<()> {
    run_gh(Some(token), &["pr", "ready", &number.to_string(), "-R", &remote.slug()], None)?;
    Ok(())
}

/// Close a pull request without merging it, or reopen a closed one.
pub fn set_open(token: &str, remote: &Remote, number: u64, open: bool) -> anyhow::Result<()> {
    let body = json!({ "state": if open { "open" } else { "closed" } });
    api(token, "PATCH", &format!("repos/{}/pulls/{number}", remote.slug()), Some(&body))?;
    Ok(())
}

/// The last line the patch shows for its last file: `(on the old side, line number)`. That's
/// where the diff ends, so the review box goes under it. `None` when the last file has no
/// hunks (binary, or only renamed).
pub fn last_patch_line(patch: &str) -> Option<(bool, u64)> {
    let section = match patch.rfind("\ndiff --git ") {
        Some(at) => &patch[at + 1..],
        None => patch,
    };
    let (mut old, mut new, mut last, mut in_hunk) = (0u64, 0u64, None, false);
    for line in section.lines() {
        if let Some(rest) = line.strip_prefix("@@ -") {
            // "@@ -a,b +c,d @@": the first line of each side.
            let mut parts = rest.split(' ');
            let start = |part: Option<&str>| -> Option<u64> { part?.split(',').next()?.parse().ok() };
            old = start(parts.next())?;
            new = start(parts.next().map(|p| p.trim_start_matches('+')))?;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                last = Some((false, new));
                new += 1;
            }
            Some(b'-') => {
                last = Some((true, old));
                old += 1;
            }
            Some(b'\\') => {}
            _ => {
                last = Some((false, new));
                old += 1;
                new += 1;
            }
        }
    }
    last
}

// ---------- the local clone ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalStatus {
    /// The checked-out branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    /// Files with uncommitted changes.
    pub dirty: usize,
}

pub fn local_status(root: &Path) -> anyhow::Result<LocalStatus> {
    Ok(parse_status(&git(root, None, &["status", "--porcelain=v2", "--branch"])?))
}

pub fn parse_status(text: &str) -> LocalStatus {
    let mut status = LocalStatus::default();
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = (head != "(detached)").then(|| head.to_string());
        } else if let Some(up) = line.strip_prefix("# branch.upstream ") {
            status.upstream = Some(up.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut parts = ab.split_whitespace();
            status.ahead = parts.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
            status.behind = parts.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
        } else if !line.starts_with('#') && !line.is_empty() && !line.starts_with("! ") {
            status.dirty += 1;
        }
    }
    status
}

#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// The local name (for a remote-only branch, its name without the remote).
    pub name: String,
    /// Only on the remote; switching to it creates a local branch that tracks it.
    pub remote_only: bool,
    pub updated: i64,
    pub subject: String,
}

/// Local branches and the remote's, newest first; a remote branch with a local one is listed once.
pub fn branches(root: &Path, remote: &str) -> anyhow::Result<Vec<Branch>> {
    let out = git(
        root,
        None,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(committerdate:unix)%00%(subject)",
            "refs/heads",
            &format!("refs/remotes/{remote}"),
        ],
    )?;
    Ok(parse_branches(&out, remote))
}

pub fn parse_branches(text: &str, remote: &str) -> Vec<Branch> {
    let remote_prefix = format!("refs/remotes/{remote}/");
    let mut local = Vec::new();
    let mut remote_branches = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\0');
        let (Some(refname), Some(time), subject) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else {
            continue;
        };
        let updated = time.parse().unwrap_or(0);
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            local.push(Branch { name: name.into(), remote_only: false, updated, subject: subject.into() });
        } else if let Some(name) = refname.strip_prefix(&remote_prefix)
            && name != "HEAD"
        {
            remote_branches.push(Branch { name: name.into(), remote_only: true, updated, subject: subject.into() });
        }
    }
    remote_branches.retain(|r| !local.iter().any(|l| l.name == r.name));
    local.extend(remote_branches);
    local.sort_by_key(|b| std::cmp::Reverse(b.updated));
    local
}

pub fn fetch(root: &Path, token: Option<&str>, remote: &str) -> anyhow::Result<()> {
    git(root, token, &["fetch", "--prune", "--quiet", remote])?;
    Ok(())
}

/// Fast-forward the checked-out branch to its upstream. A branch that has diverged is left
/// alone, with a message saying so.
pub fn pull(root: &Path, token: Option<&str>) -> anyhow::Result<()> {
    match git(root, token, &["pull", "--ff-only", "--quiet"]) {
        Ok(_) => Ok(()),
        Err(err) if err.to_string().contains("ast-forward") || err.to_string().contains("diverg") => {
            bail!(
                "Your branch and its upstream have both moved on; pull can't fast-forward. Rebase or merge in a terminal."
            )
        }
        Err(err) => Err(err),
    }
}

/// Push the checked-out branch, setting its upstream on the first push.
pub fn push(root: &Path, token: Option<&str>, remote: &str, status: &LocalStatus) -> anyhow::Result<()> {
    let Some(branch) = &status.branch else { bail!("Nothing to push from a detached HEAD.") };
    if status.upstream.is_some() {
        git(root, token, &["push", "--quiet"])?;
    } else {
        git(root, token, &["push", "--quiet", "-u", remote, branch])?;
    }
    Ok(())
}

pub fn switch(root: &Path, remote: &str, branch: &Branch) -> anyhow::Result<()> {
    if branch.remote_only {
        git(root, None, &["switch", "--quiet", "--track", &format!("{remote}/{}", branch.name)])?;
    } else {
        git(root, None, &["switch", "--quiet", &branch.name])?;
    }
    Ok(())
}

/// The local branch a pull request is checked out as: its own name when it's in this
/// repository, `pr-<n>` for a fork's.
pub fn local_branch_for(pr: &PullRequest) -> String {
    if pr.cross_repo { format!("pr-{}", pr.number) } else { pr.head.clone() }
}

/// Check out a pull request's branch from the project's remote, fast-forwarding a local copy.
pub fn checkout(root: &Path, token: Option<&str>, remote: &str, pr: &PullRequest) -> anyhow::Result<String> {
    let local = local_branch_for(pr);
    let tracking = if pr.cross_repo {
        let tracking = format!("refs/remotes/{remote}/pr/{}", pr.number);
        git(root, token, &["fetch", "--quiet", remote, &format!("+refs/pull/{}/head:{tracking}", pr.number)])?;
        format!("{remote}/pr/{}", pr.number)
    } else {
        git(
            root,
            token,
            &["fetch", "--quiet", remote, &format!("+refs/heads/{0}:refs/remotes/{remote}/{0}", pr.head)],
        )?;
        format!("{remote}/{}", pr.head)
    };
    let exists = git(root, None, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{local}")]).is_ok();
    if exists {
        git(root, None, &["switch", "--quiet", &local])?;
        if git(root, None, &["merge", "--ff-only", "--quiet", &tracking]).is_err() {
            bail!("Switched to {local}, but it has commits the pull request doesn't, so it wasn't updated.");
        }
    } else {
        git(root, None, &["switch", "--quiet", "-c", &local, "--track", &tracking])?;
    }
    Ok(local)
}

// ---------- small helpers for the view ----------

/// "3m", "5h", "2d", "Nov 3": how long ago an ISO-8601 time was.
pub fn ago(iso: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let secs = (now - at.with_timezone(&chrono::Utc)).num_seconds().max(0);
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..2_592_000 => format!("{}d", secs / 86_400),
        _ => at.format("%b %-d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn last_patch_line_is_the_end_of_the_last_file() {
        let patch = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n\
diff --git a/y.rs b/y.rs\n--- a/y.rs\n+++ b/y.rs\n@@ -10,3 +12,4 @@ fn f()\n ctx\n+new\n ctx\n ctx\n";
        assert_eq!(super::last_patch_line(patch), Some((false, 15)));
        let deleted_last =
            "diff --git a/z b/z\n--- a/z\n+++ b/z\n@@ -4,2 +4,1 @@\n keep\n-gone\n\\ No newline at end of file\n";
        assert_eq!(super::last_patch_line(deleted_last), Some((true, 5)));
        let binary = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/i.png b/i.png\nBinary files differ\n";
        assert_eq!(super::last_patch_line(binary), None);
    }

    use super::*;

    fn remote(host: &str, owner: &str) -> Remote {
        Remote { name: "origin".into(), host: host.into(), owner: owner.into(), repo: "r".into() }
    }

    #[test]
    fn remote_urls_of_every_shape_are_read() {
        let cases = [
            ("git@github_personal:turbineBMW/TJ.git", ("github_personal", "turbineBMW", "TJ")),
            (
                "github_fieldhook:Liftwerx-Solutions-Inc/FieldHook.git",
                ("github_fieldhook", "Liftwerx-Solutions-Inc", "FieldHook"),
            ),
            ("https://github.com/owner/repo", ("github.com", "owner", "repo")),
            ("https://user@github.com/owner/repo.git/", ("github.com", "owner", "repo")),
            ("ssh://git@github.com:22/owner/repo.git", ("github.com", "owner", "repo")),
        ];
        for (url, (host, owner, repo)) in cases {
            assert_eq!(parse_remote_url(url), Some((host.into(), owner.into(), repo.into())), "{url}");
        }
        assert_eq!(parse_remote_url("/local/path/repo"), None);
        assert_eq!(parse_remote_url("https://github.com/only-owner"), None);
    }

    #[test]
    fn the_account_follows_the_owner_then_the_ssh_alias() {
        let accounts = vec!["fieldhook".to_string(), "turbineBMW".to_string()];
        assert_eq!(guess_account(&accounts, &remote("github_personal", "turbinebmw")).as_deref(), Some("turbineBMW"));
        assert_eq!(
            guess_account(&accounts, &remote("github_fieldhook", "Liftwerx-Solutions-Inc")).as_deref(),
            Some("fieldhook")
        );
        assert_eq!(guess_account(&accounts, &remote("github.com", "someone-else")), None);
    }

    #[test]
    fn accounts_list_the_active_one_first() {
        let json = r#"{"hosts":{"github.com":[
            {"state":"success","active":false,"login":"b"},
            {"state":"success","active":true,"login":"a"},
            {"state":"error","active":false,"login":"broken"}]}}"#;
        assert_eq!(parse_accounts(json).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn pull_requests_read_state_checks_and_reviews() {
        let v: Value = serde_json::from_str(
            r#"{"number":7,"title":"Add it","author":{"login":"me"},"headRefName":"feat","baseRefName":"main",
            "isDraft":true,"state":"OPEN","isCrossRepository":false,"reviewDecision":"APPROVED",
            "statusCheckRollup":[
              {"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"},
              {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":""},
              {"__typename":"StatusContext","context":"deploy","state":"SUCCESS"}],
            "additions":5,"deletions":2,"changedFiles":1,"comments":[{},{}],"labels":[{"name":"bug"}],
            "updatedAt":"2026-11-25T10:00:00Z","url":"u"}"#,
        )
        .unwrap();
        let pr = parse_pull_request(&v);
        assert_eq!(pr.state, PrState::Draft);
        assert_eq!(pr.checks_state(), ChecksState::Pending);
        assert_eq!(pr.checks[1].status, "In progress");
        assert_eq!(pr.comments, 2);
        assert_eq!(pr.labels, vec!["bug"]);
        let merged: Value = serde_json::from_str(r#"{"state":"MERGED","isDraft":true}"#).unwrap();
        assert_eq!(parse_pull_request(&merged).state, PrState::Merged);
    }

    #[test]
    fn a_failing_check_wins_the_roll_up() {
        let check = |state| Check { name: "c".into(), state, status: String::new(), url: None };
        assert_eq!(roll_up(&[]), ChecksState::None);
        assert_eq!(roll_up(&[check(ChecksState::Passing), check(ChecksState::Pending)]), ChecksState::Pending);
        assert_eq!(roll_up(&[check(ChecksState::Pending), check(ChecksState::Failing)]), ChecksState::Failing);
    }

    #[test]
    fn the_timeline_mixes_comments_and_reviews_in_order() {
        let v: Value = serde_json::from_str(
            r#"{"comments":[{"author":{"login":"a"},"body":"second","createdAt":"2026-01-02T00:00:00Z"}],
            "reviews":[
              {"author":{"login":"b"},"body":"","state":"APPROVED","submittedAt":"2026-01-03T00:00:00Z"},
              {"author":{"login":"c"},"body":"first","state":"COMMENTED","submittedAt":"2026-01-01T00:00:00Z"},
              {"author":{"login":"d"},"body":"","state":"COMMENTED","submittedAt":"2026-01-04T00:00:00Z"}],
            "commits":[{"oid":"abc","messageHeadline":"Do it","authors":[{"login":"","name":"Someone"}],"committedDate":"x"}]}"#,
        )
        .unwrap();
        let detail = parse_detail(&v, &Value::Null, String::new());
        let order: Vec<&str> = detail.timeline.iter().map(|t| t.author.as_str()).collect();
        assert_eq!(order, vec!["c", "a", "b"], "a bare COMMENTED review only has line comments");
        assert_eq!(detail.commits[0].author, "Someone");
    }

    #[test]
    fn a_review_sends_line_comments_and_ranges() {
        let drafts = vec![
            DraftComment { path: "a.rs".into(), side: "RIGHT".into(), start_line: 3, line: 3, body: "one".into() },
            DraftComment { path: "b.rs".into(), side: "LEFT".into(), start_line: 1, line: 4, body: "range".into() },
        ];
        let body = review_body(ReviewEvent::RequestChanges, "", "sha", &drafts);
        assert_eq!(body["event"], "REQUEST_CHANGES");
        assert!(body.get("body").is_none());
        assert_eq!(body["commit_id"], "sha");
        assert!(body["comments"][0].get("start_line").is_none());
        assert_eq!(body["comments"][1]["start_line"], 1);
        assert_eq!(body["comments"][1]["start_side"], "LEFT");
    }

    #[test]
    fn status_reads_branch_tracking_and_dirt() {
        let text = "# branch.oid abc\n# branch.head feat\n# branch.upstream origin/feat\n# branch.ab +2 -5\n1 .M N... 100644 100644 100644 a b src/x.rs\n? new.txt\n";
        let s = parse_status(text);
        assert_eq!(s.branch.as_deref(), Some("feat"));
        assert_eq!(s.upstream.as_deref(), Some("origin/feat"));
        assert_eq!((s.ahead, s.behind, s.dirty), (2, 5, 2));
        assert_eq!(parse_status("# branch.head (detached)\n").branch, None);
    }

    #[test]
    fn branches_list_remote_ones_once() {
        let text = "refs/heads/main\x00100\x00m\nrefs/remotes/origin/main\x00100\x00m\nrefs/remotes/origin/HEAD\x00100\x00m\nrefs/remotes/origin/feat\x00200\x00f\nrefs/remotes/upstream/x\x00300\x00x\n";
        let list = parse_branches(text, "origin");
        assert_eq!(
            list.iter().map(|b| (b.name.as_str(), b.remote_only)).collect::<Vec<_>>(),
            vec![("feat", true), ("main", false)]
        );
    }

    #[test]
    fn times_read_as_ago() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-25T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(ago("2026-11-25T11:58:00Z", now), "2m");
        assert_eq!(ago("2026-11-25T07:00:00Z", now), "5h");
        assert_eq!(ago("2026-11-22T12:00:00Z", now), "3d");
        assert_eq!(ago("2026-01-03T12:00:00Z", now), "Jan 3");
        assert_eq!(ago("garbage", now), "");
    }

    /// Reads a real repository through gh: `TJ_LIVE_REPO=~/Projects/X cargo test -- --ignored live_github`
    #[test]
    #[ignore]
    fn live_github_picks_an_account_and_reads_pull_requests() {
        let root = PathBuf::from(std::env::var("TJ_LIVE_REPO").expect("set TJ_LIVE_REPO to a clone"));
        let remote = super::remote(&root).unwrap().expect("a GitHub remote");
        let accounts = accounts().unwrap();
        let (login, token, info) = pick_account(&accounts, &remote).unwrap();
        let prs = pull_requests(&token, &remote, PrFilter::Merged).unwrap();
        eprintln!("{} -> {login} (push: {}, {} merged PRs)", remote.slug(), info.can_push, prs.len());
        if let Some(pr) = prs.first() {
            let detail = pull_request(&token, &remote, pr.number).unwrap();
            eprintln!("#{}: {} commits, {} bytes of patch", pr.number, detail.commits.len(), detail.patch.len());
            assert!(!detail.patch.is_empty());
        }
        let status = local_status(&root).unwrap();
        eprintln!("branch {:?}, ahead {}, behind {}", status.branch, status.ahead, status.behind);
    }

    /// Checking out and pulling against a real (local) remote.
    #[test]
    fn checkout_and_pull_follow_the_remote() {
        let base = std::env::temp_dir().join(format!("tj-gh-{}", crate::store::new_id()));
        let (origin, clone) = (base.join("origin.git"), base.join("clone"));
        std::fs::create_dir_all(&base).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&base, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        sh(&base, &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        for args in [
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "T"],
            &["config", "commit.gpgsign", "false"],
        ] {
            sh(&clone, args);
        }
        sh(&clone, &["commit", "-q", "--allow-empty", "-m", "one"]);
        sh(&clone, &["push", "-q", "-u", "origin", "main"]);
        sh(&clone, &["switch", "-q", "-c", "feat"]);
        sh(&clone, &["commit", "-q", "--allow-empty", "-m", "feature"]);
        sh(&clone, &["push", "-q", "origin", "feat"]);
        sh(&clone, &["switch", "-q", "main"]);
        sh(&clone, &["branch", "-q", "-D", "feat"]);

        let pr = PullRequest {
            number: 1,
            title: "t".into(),
            author: "a".into(),
            head: "feat".into(),
            base: "main".into(),
            state: PrState::Open,
            cross_repo: false,
            review_decision: String::new(),
            checks: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            comments: 0,
            labels: vec![],
            updated_at: String::new(),
            url: String::new(),
        };
        assert_eq!(checkout(&clone, None, "origin", &pr).unwrap(), "feat");
        let status = local_status(&clone).unwrap();
        assert_eq!(status.branch.as_deref(), Some("feat"));
        assert_eq!(status.upstream.as_deref(), Some("origin/feat"));

        // Someone else pushes to main; switching back and pulling brings it in.
        sh(&clone, &["switch", "-q", "main"]);
        sh(&clone, &["commit", "-q", "--allow-empty", "-m", "two"]);
        sh(&clone, &["push", "-q", "origin", "main"]);
        sh(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
        fetch(&clone, None, "origin").unwrap();
        assert_eq!(local_status(&clone).unwrap().behind, 1);
        pull(&clone, None).unwrap();
        assert_eq!(local_status(&clone).unwrap().behind, 0);

        let list = branches(&clone, "origin").unwrap();
        assert!(list.iter().any(|b| b.name == "feat" && !b.remote_only));
        let _ = std::fs::remove_dir_all(&base);
    }
}
