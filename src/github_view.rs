//! The GitHub tab: a project's pull requests on the GitHub account that project uses, with no
//! switching accounts in a browser. The header names the repository and the account (pick
//! another, or let TJ pick), shows the checked-out branch (switch it from a menu) and how far it
//! is from its upstream, with Pull, Push and Refresh. Under it, the pull requests (open, merged
//! or closed) and the one picked: its conversation with a comment box and the merge box, its
//! files with line comments that go out with a review (Comment, Approve, Request changes), and
//! its commits. Check out puts its branch in the project folder.
//!
//! Beside Pull requests, the Actions page lists the repository's workflow runs (all, running or
//! failed) and the one picked: its jobs and their steps, the failing part of a job's log, and
//! Re-run or Cancel. Runs still going are read again while the page is open.
//!
//! Other repos lists the pull requests the project's sessions sent to other repositories and
//! hosts ([`crate::prs_view`]). A project folder with no GitHub remote of its own, such as a
//! folder of forks, shows that page alone when it has repositories in its subfolders or tracked
//! pull requests.
//!
//! All the work is `gh` and `git` in the background (see [`crate::github`]).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    diff::{Diff, DiffAnnotation, DiffFileStatus, DiffLinePosition, DiffLineRange, DiffMode, DiffSide, DiffState},
    input::{InputEvent, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    spinner::Spinner,
    switch::Switch,
    text::TextView,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::github::{
    self, Branch, ChecksState, DraftComment, Job, LineComment, LocalStatus, MergeMethod, PrDetail, PrFilter, PrState,
    PullRequest, Remote, RepoInfo, ReviewEvent, RunFilter, RunState, WorkflowRun,
};
use crate::prs_view::PrsView;

pub const CONTEXT: &str = "GitHub";
const LIST_WIDTH: f32 = 340.0;
/// Narrower than this, the list and the pull request take turns: the list is a page of its own,
/// and the pull request has a way back to it.
const NARROW_WIDTH: f32 = 1000.0;
/// The local branch's state is read again this often while the tab is open.
const LOCAL_POLL: Duration = Duration::from_secs(15);
/// Showing the tab again after this long reloads the pull requests.
const STALE: Duration = Duration::from_secs(45);

gpui_kit::actions!(
    github,
    [
        NextPullRequest,
        PreviousPullRequest,
        OpenPullRequest,
        BackToList,
        ShowConversation,
        ShowFiles,
        ShowCommits,
        Refresh,
        Pull
    ]
);

/// The tab's own keys; they apply while it has focus and a text box doesn't take them.
pub fn bindings() -> Vec<KeyBinding> {
    let cx = Some(CONTEXT);
    vec![
        KeyBinding::new("down", NextPullRequest, cx),
        KeyBinding::new("up", PreviousPullRequest, cx),
        KeyBinding::new("enter", OpenPullRequest, cx),
        KeyBinding::new("escape", BackToList, cx),
        KeyBinding::new("alt-1", ShowConversation, cx),
        KeyBinding::new("alt-2", ShowFiles, cx),
        KeyBinding::new("alt-3", ShowCommits, cx),
        KeyBinding::new("f5", Refresh, cx),
        KeyBinding::new("ctrl-alt-l", Pull, cx),
    ]
}

/// The workspace saves these.
pub enum GitHubEvent {
    /// The account picked for this project (`None`: TJ picks).
    AccountChosen(Option<String>),
    /// The merge method picked from the merge button's menu.
    MergeMethodChosen(MergeMethod),
    /// Open a file of a pull request in the review modal, with its summary.
    ExpandFile(ExpandFile),
}

/// A pull request's file to show in the review modal.
#[derive(Clone, Debug)]
pub struct ExpandFile {
    pub number: u64,
    pub patch: String,
    pub path: String,
    /// What the pull request is for, so the summary knows the intent.
    pub context: String,
}

impl EventEmitter<GitHubEvent> for GitHubView {}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Nothing loaded yet; the first render starts it.
    Idle,
    Loading,
    NoGh,
    NotGitHub,
    NoAccounts,
    Failed(String),
    Ready,
}

/// The tab's pages, like the repository's tabs on github.com.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    PullRequests,
    Actions,
    /// Pull requests on other repositories.
    Tracked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Conversation,
    Files,
    Commits,
}

/// A line comment or reply being written on the diff.
struct Compose {
    path: String,
    side: String,
    start: u64,
    line: u64,
    /// Replying on this comment's thread instead of starting one.
    reply_to: Option<u64>,
    input: Entity<TextareaState>,
}

/// What the header says after an action: what happened, or what went wrong.
#[derive(Clone)]
struct Flash {
    ok: bool,
    text: String,
    at: Instant,
}

pub struct GitHubView {
    root: PathBuf,
    /// The pull requests sent to other repositories: the Other repos page.
    tracked: Entity<PrsView>,
    /// With no GitHub remote: there are git repositories in the project's subfolders.
    nested_repos: bool,
    /// With no GitHub remote: the user asked for the Other repos page all the same.
    tracked_wanted: bool,
    /// The account picked for the project in TJ; `None` lets TJ pick from the remote.
    chosen: Option<String>,
    phase: Phase,
    accounts: Vec<String>,
    /// The account in use, and its token.
    account: Option<(String, String)>,
    remote: Option<Remote>,
    info: RepoInfo,
    page: Page,
    filter: PrFilter,
    prs: Vec<PullRequest>,
    prs_loading: bool,
    loaded_at: Option<Instant>,
    selected: Option<u64>,
    detail: Option<PrDetail>,
    detail_loading: bool,
    section: Section,
    run_filter: RunFilter,
    runs: Vec<WorkflowRun>,
    runs_loading: bool,
    runs_loaded_at: Option<Instant>,
    run_selected: Option<u64>,
    /// The picked run's jobs, with the run they're for.
    jobs: Option<(u64, Vec<Job>)>,
    jobs_loading: bool,
    /// Job logs read so far (`None` while one is loading), and the jobs showing theirs.
    logs: HashMap<u64, Option<Vec<String>>>,
    logs_open: HashSet<u64>,
    runs_scroll: ScrollHandle,
    diff: Entity<DiffState>,
    drafts: Vec<DraftComment>,
    compose: Option<Compose>,
    comment_box: Entity<TextareaState>,
    review_box: Entity<TextareaState>,
    merge_method: MergeMethod,
    /// The merge method last picked for the project, which the merge button starts on.
    last_merge_method: Option<MergeMethod>,
    delete_branch: bool,
    local: Option<LocalStatus>,
    branches: Vec<Branch>,
    /// What's running, for the header's spinner ("Pulling…").
    busy: Option<&'static str>,
    flash: Option<Flash>,
    avatars: HashMap<String, Arc<Image>>,
    avatars_asked: HashSet<String>,
    /// Bumped when the account or repository changes, so late answers for the old one are dropped.
    generation: u64,
    /// Text boxes to empty on the next render (after their text was sent).
    to_clear: Vec<Entity<TextareaState>>,
    list_scroll: ScrollHandle,
    /// The tab's width at the last paint (`None` before the first), to tell when it's narrow.
    width: Option<Pixels>,
    /// When narrow, the list is showing instead of the pull request.
    list_page: bool,
    /// The file headers painted this frame (file index, top), to tell which file is at the top.
    headers_seen: Rc<RefCell<Vec<(usize, Pixels)>>>,
    /// Where the diff was painted.
    diff_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The file at the top of the diff, which Previous and Next file move from.
    current_file: usize,
    /// Set by Previous/Next file and kept until the diff is scrolled by hand: the last files
    /// can't reach the top, so where they sit doesn't say which one was picked.
    file_pinned: bool,
    /// The file the top of the diff is in, once its own header has scrolled out of view; its
    /// header is drawn over the top of the diff.
    sticky_file: Option<usize>,
    focus: FocusHandle,
    _poll: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl GitHubView {
    pub fn new(
        root: PathBuf,
        tracked: Entity<PrsView>,
        chosen: Option<String>,
        last_merge_method: Option<MergeMethod>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let textarea = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 12).placeholder(placeholder))
        };
        let comment_box = textarea("Leave a comment  (Ctrl+Enter sends)", window, cx);
        let review_box = textarea("Sum up your review (optional for an approval)", window, cx);
        let mut subscriptions = Vec::new();
        // Typing re-renders the view too: Close reads "Close with comment" while there's text.
        subscriptions.push(cx.subscribe_in(&comment_box, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::PressEnter { secondary: true, .. } => this.send_comment(cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        }));
        subscriptions.push(cx.subscribe_in(&review_box, window, |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::PressEnter { secondary: true, .. } => this.send_review(ReviewEvent::Comment, cx),
            InputEvent::Change => cx.notify(),
            _ => {}
        }));
        // The page's count, and whether a folder with no remote shows it, follow its PRs.
        subscriptions.push(cx.observe(&tracked, |_, _, cx| cx.notify()));
        let diff = cx.new(|cx| DiffState::new(Vec::new(), cx));
        Self {
            root,
            tracked,
            nested_repos: false,
            tracked_wanted: false,
            chosen,
            phase: Phase::Idle,
            accounts: Vec::new(),
            account: None,
            remote: None,
            info: RepoInfo::default(),
            page: Page::PullRequests,
            filter: PrFilter::Open,
            prs: Vec::new(),
            prs_loading: false,
            loaded_at: None,
            selected: None,
            detail: None,
            detail_loading: false,
            section: Section::Conversation,
            run_filter: RunFilter::All,
            runs: Vec::new(),
            runs_loading: false,
            runs_loaded_at: None,
            run_selected: None,
            jobs: None,
            jobs_loading: false,
            logs: HashMap::new(),
            logs_open: HashSet::new(),
            runs_scroll: ScrollHandle::new(),
            diff,
            drafts: Vec::new(),
            compose: None,
            comment_box,
            review_box,
            merge_method: MergeMethod::Squash,
            last_merge_method,
            delete_branch: true,
            local: None,
            branches: Vec::new(),
            busy: None,
            flash: None,
            avatars: HashMap::new(),
            avatars_asked: HashSet::new(),
            generation: 0,
            to_clear: Vec::new(),
            list_scroll: ScrollHandle::new(),
            width: None,
            list_page: true,
            headers_seen: Rc::new(RefCell::new(Vec::new())),
            diff_bounds: Rc::new(Cell::new(Bounds::default())),
            current_file: 0,
            file_pinned: false,
            sticky_file: None,
            focus: cx.focus_handle(),
            _poll: None,
            _subscriptions: subscriptions,
        }
    }

    /// The tab's title, with how many pull requests are open: the repository's, or with no
    /// remote, the ones on other repositories.
    pub fn title(&self, cx: &App) -> String {
        let open = if self.tracked_only(cx) {
            self.tracked.read(cx).active(cx)
        } else if self.remote.is_some() && self.phase == Phase::Ready && self.filter == PrFilter::Open {
            self.prs.len()
        } else {
            0
        };
        if open == 0 { "GitHub".into() } else { format!("GitHub · {open}") }
    }

    /// The project's own repository (`owner/repo`), once found.
    pub fn repo(&self) -> Option<String> {
        self.remote.as_ref().map(Remote::slug)
    }

    /// No GitHub remote, but pull requests on other repositories to show (or repositories in
    /// subfolders to send them from): the tab is the Other repos page alone.
    fn tracked_only(&self, cx: &App) -> bool {
        self.phase == Phase::NotGitHub
            && (self.nested_repos || self.tracked_wanted || self.tracked.read(cx).count(cx) > 0)
    }

    /// Whether the Other repos page is what's showing.
    fn showing_tracked(&self, cx: &App) -> bool {
        self.tracked_only(cx) || (self.phase == Phase::Ready && self.page == Page::Tracked)
    }

    #[cfg(test)]
    pub fn is_showing_tracked(&self, cx: &App) -> bool {
        self.showing_tracked(cx)
    }

    /// The tab came to the front: focus it and bring stale data up to date.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_tracked(cx) {
            self.tracked.update(cx, |t, cx| t.shown(window, cx));
            return;
        }
        window.focus(&self.focus, cx);
        let loaded_at = if self.page == Page::Actions { self.runs_loaded_at } else { self.loaded_at };
        if self.phase == Phase::Ready && loaded_at.is_none_or(|at| at.elapsed() > STALE) {
            self.refresh(false, cx);
        }
    }

    // ---------- loading ----------

    /// Find the remote, the accounts and the one to use, then load everything.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Loading;
        self.generation += 1;
        let generation = self.generation;
        let (root, chosen) = (self.root.clone(), self.chosen.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // No remote is known without gh, and it decides whether the Other repos
                    // page stands in for the tab.
                    let Some(remote) = github::remote(&root)? else {
                        let nested = crate::tracked::has_nested_repos(&root);
                        return Ok((Phase::NotGitHub, Vec::new(), None, None, RepoInfo::default(), nested));
                    };
                    if !github::gh_installed() {
                        return Ok((Phase::NoGh, Vec::new(), None, None, RepoInfo::default(), false));
                    }
                    let accounts = github::accounts()?;
                    if accounts.is_empty() {
                        return Ok((Phase::NoAccounts, accounts, Some(remote), None, RepoInfo::default(), false));
                    }
                    let (login, token, info) = match chosen.filter(|c| accounts.contains(c)) {
                        Some(login) => {
                            let token = github::token(&login)?;
                            let info = github::repo_info(&token, &remote)?;
                            (login, token, info)
                        }
                        None => github::pick_account(&accounts, &remote)?,
                    };
                    anyhow::Ok((Phase::Ready, accounts, Some(remote), Some((login, token)), info, false))
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((phase, accounts, remote, account, info, nested)) => {
                        this.phase = phase;
                        this.nested_repos = nested;
                        // The repository's own pull requests are on the Pull requests page.
                        let own = remote.as_ref().map(Remote::slug);
                        this.tracked.update(cx, |t, cx| t.set_own_repo(own, cx));
                        this.accounts = accounts;
                        this.remote = remote;
                        this.account = account;
                        this.delete_branch = true;
                        this.merge_method = MergeMethod::initial(this.last_merge_method, &info);
                        this.info = info;
                        if let Some((login, _)) = this.account.clone() {
                            this.want_avatars([login], cx);
                        }
                        if this.phase == Phase::Ready {
                            this.refresh(true, cx);
                            this.start_polling(cx);
                        }
                    }
                    Err(err) => this.phase = Phase::Failed(format!("{err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LOCAL_POLL).await;
                let polled = this.update(cx, |this: &mut Self, cx| {
                    this.load_local(cx);
                    this.poll_runs(cx);
                });
                if polled.is_err() {
                    break;
                }
            }
        }));
    }

    fn token(&self) -> Option<String> {
        self.account.as_ref().map(|(_, token)| token.clone())
    }

    /// Reload the pull requests, the picked one and the local branch; `fetch` first brings in
    /// what's new on the remote.
    fn refresh(&mut self, fetch: bool, cx: &mut Context<Self>) {
        if fetch && let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) {
            let root = self.root.clone();
            self.run(
                "Fetching…",
                move || github::fetch(&root, Some(&token), &remote.name),
                |this, _, cx| this.load_local(cx),
                cx,
            );
        } else {
            self.load_local(cx);
        }
        self.load_prs(cx);
        if let Some(number) = self.selected {
            self.load_detail(number, cx);
        }
        if self.page == Page::Actions {
            self.load_runs(cx);
        }
    }

    fn load_prs(&mut self, cx: &mut Context<Self>) {
        let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) else { return };
        self.prs_loading = true;
        let (filter, generation) = (self.filter, self.generation);
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor().spawn(async move { github::pull_requests(&token, &remote, filter) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.generation != generation || this.filter != filter {
                    return;
                }
                this.prs_loading = false;
                this.loaded_at = Some(Instant::now());
                match result {
                    Ok(prs) => {
                        this.want_avatars(prs.iter().map(|p| p.author.clone()), cx);
                        this.prs = prs;
                        // Open the first one, so the page is never empty when there's something.
                        if this.selected.is_none_or(|n| !this.prs.iter().any(|p| p.number == n))
                            && let Some(first) = this.prs.first().map(|p| p.number)
                        {
                            this.select(first, cx);
                        }
                    }
                    Err(err) => this.say(false, format!("Couldn't load pull requests: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_detail(&mut self, number: u64, cx: &mut Context<Self>) {
        let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) else { return };
        self.detail_loading = true;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor().spawn(async move { github::pull_request(&token, &remote, number) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.generation != generation || this.selected != Some(number) {
                    return;
                }
                this.detail_loading = false;
                match result {
                    Ok(detail) => {
                        let files = crate::changes::from_patch(&detail.patch).map(|c| c.diff_files).unwrap_or_default();
                        this.diff.update(cx, |d, cx| d.set_files(files, cx));
                        let people: Vec<String> = detail
                            .timeline
                            .iter()
                            .map(|t| t.author.clone())
                            .chain(detail.line_comments.iter().map(|c| c.author.clone()))
                            .chain([detail.pr.author.clone()])
                            .collect();
                        this.want_avatars(people, cx);
                        // GitHub works mergeability out lazily; ask again shortly.
                        let unknown = detail.pr.state == PrState::Open && detail.mergeable == "UNKNOWN";
                        this.detail = Some(detail);
                        if unknown {
                            cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(Duration::from_secs(3)).await;
                                let _ = this.update(cx, |this: &mut Self, cx| {
                                    if this.selected == Some(number) && !this.detail_loading {
                                        this.load_detail(number, cx);
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                    Err(err) => this.say(false, format!("Couldn't load #{number}: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_local(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.clone() else { return };
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = github::local_status(&root)?;
                    let branches = github::branches(&root, &remote.name)?;
                    anyhow::Ok((status, branches))
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if let Ok((status, branches)) = result
                    && (this.local.as_ref() != Some(&status) || this.branches != branches)
                {
                    this.local = Some(status);
                    this.branches = branches;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn load_runs(&mut self, cx: &mut Context<Self>) {
        let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) else { return };
        self.runs_loading = true;
        let (filter, generation) = (self.run_filter, self.generation);
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor().spawn(async move { github::workflow_runs(&token, &remote, filter) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.generation != generation || this.run_filter != filter {
                    return;
                }
                this.runs_loading = false;
                this.runs_loaded_at = Some(Instant::now());
                match result {
                    Ok(runs) => {
                        this.want_avatars(runs.iter().map(|r| r.actor.clone()), cx);
                        this.runs = runs;
                        // Open the first one, so the page is never empty when there's something.
                        match this.run_selected.filter(|id| this.runs.iter().any(|r| r.id == *id)) {
                            Some(id) => this.load_jobs(id, cx),
                            None => {
                                if let Some(first) = this.runs.first().map(|r| r.id) {
                                    this.select_run(first, cx);
                                }
                            }
                        }
                    }
                    Err(err) => this.say(false, format!("Couldn't load workflow runs: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_jobs(&mut self, run: u64, cx: &mut Context<Self>) {
        let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) else { return };
        self.jobs_loading = true;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { github::run_jobs(&token, &remote, run) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.generation != generation || this.run_selected != Some(run) {
                    return;
                }
                this.jobs_loading = false;
                match result {
                    Ok(jobs) => {
                        // A job that finished since its log was read (or failed to be) is read again.
                        for job in &jobs {
                            if job.state.active() {
                                this.logs.remove(&job.id);
                            }
                        }
                        this.jobs = Some((run, jobs));
                    }
                    Err(err) => this.say(false, format!("Couldn't load the run's jobs: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Show or hide a job's log, reading it the first time.
    fn toggle_log(&mut self, job: u64, cx: &mut Context<Self>) {
        if !self.logs_open.insert(job) {
            self.logs_open.remove(&job);
            cx.notify();
            return;
        }
        if self.logs.contains_key(&job) {
            cx.notify();
            return;
        }
        let (Some(remote), Some(token)) = (self.remote.clone(), self.token()) else { return };
        self.logs.insert(job, None);
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { github::job_log(&token, &remote, job) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                match result {
                    Ok(lines) => {
                        this.logs.insert(job, Some(lines));
                    }
                    Err(err) => {
                        this.logs.remove(&job);
                        this.logs_open.remove(&job);
                        this.say(false, format!("Couldn't read the log: {err:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// While the Actions page is open and something on it is still going, read it again.
    fn poll_runs(&mut self, cx: &mut Context<Self>) {
        if self.page != Page::Actions || self.runs_loading {
            return;
        }
        let jobs_going = self.jobs.as_ref().is_some_and(|(_, jobs)| jobs.iter().any(|j| j.state.active()));
        if jobs_going || self.runs.iter().any(|r| r.state.active()) {
            self.load_runs(cx);
        }
    }

    /// Fetch the avatars not asked for yet, from github.com (public pictures, by login).
    fn want_avatars(&mut self, logins: impl IntoIterator<Item = String>, cx: &mut Context<Self>) {
        let wanted: Vec<String> = logins
            .into_iter()
            .filter(|l| !l.is_empty() && l != "ghost")
            .filter(|l| self.avatars_asked.insert(l.clone()))
            .collect();
        if wanted.is_empty() || cfg!(test) {
            return;
        }
        cx.spawn(async move |this, cx| {
            for login in wanted {
                let image = cx.background_executor().spawn(async move { fetch_avatar(&login).map(|i| (login, i)) });
                if let Some((login, image)) = image.await {
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        this.avatars.insert(login, image);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    // ---------- actions ----------

    /// Run `work` in the background with the header saying `busy`; on success `done` runs on
    /// the UI thread, and a failure is shown in the header.
    fn run<T: Send + 'static>(
        &mut self,
        busy: &'static str,
        work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.busy = Some(busy);
        self.flash = None;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.busy == Some(busy) {
                    this.busy = None;
                }
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(value) => done(this, value, cx),
                    Err(err) => this.say(false, format!("{err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn say(&mut self, ok: bool, text: impl Into<String>) {
        self.flash = Some(Flash { ok, text: text.into(), at: Instant::now() });
    }

    fn select(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.selected != Some(number) {
            self.selected = Some(number);
            self.detail = None;
            self.drafts.clear();
            self.compose = None;
            self.current_file = 0;
            self.file_pinned = false;
            self.sticky_file = None;
            self.diff.update(cx, |d, cx| d.set_files(Vec::new(), cx));
        }
        self.load_detail(number, cx);
        if let Some(ix) = self.prs.iter().position(|p| p.number == number) {
            self.list_scroll.scroll_to_item(ix);
        }
        cx.notify();
    }

    fn select_run(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.run_selected != Some(id) {
            self.run_selected = Some(id);
            self.jobs = None;
            self.logs_open.clear();
        }
        self.load_jobs(id, cx);
        if let Some(ix) = self.runs.iter().position(|r| r.id == id) {
            self.runs_scroll.scroll_to_item(ix);
        }
        cx.notify();
    }

    fn set_run_filter(&mut self, filter: RunFilter, cx: &mut Context<Self>) {
        if self.run_filter != filter {
            self.run_filter = filter;
            self.list_page = true;
            self.runs.clear();
            self.run_selected = None;
            self.jobs = None;
            self.load_runs(cx);
        }
    }

    fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        self.list_page = true;
        let loaded_at = if page == Page::Actions { self.runs_loaded_at } else { self.loaded_at };
        if loaded_at.is_none_or(|at| at.elapsed() > STALE) {
            match page {
                Page::Actions => self.load_runs(cx),
                Page::PullRequests => self.load_prs(cx),
                Page::Tracked => {}
            }
        }
        cx.notify();
    }

    /// A pull request's section; the keys for one bring the Pull requests page back.
    fn show_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.show_page(Page::PullRequests, cx);
        cx.notify();
    }

    /// The item picked on the page showing, if any.
    fn has_selection(&self) -> bool {
        match self.page {
            Page::PullRequests => self.selected.is_some(),
            Page::Actions => self.run_selected.is_some(),
            Page::Tracked => false,
        }
    }

    /// Whether the tab is too narrow for the list beside the pull request.
    fn narrow(&self) -> bool {
        self.width.is_some_and(|w| w < px(NARROW_WIDTH))
    }

    /// The button back to the list, when the pull request has the narrow tab to itself.
    fn back_to_list(&self, cx: &mut Context<Self>) -> Option<Button> {
        self.narrow().then(|| {
            let label = if self.page == Page::Actions { "Workflow runs" } else { "Pull requests" };
            Button::new("github-back").small().ghost().icon(IconName::ArrowLeft).label(label).on_click(cx.listener(
                |this, _, _, cx| {
                    this.list_page = true;
                    cx.notify();
                },
            ))
        })
    }

    fn select_step(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.page == Page::Tracked {
            return;
        }
        if self.page == Page::Actions {
            let Some(next) = step_in(self.runs.iter().map(|r| r.id), self.run_selected, step) else { return };
            self.select_run(next, cx);
            return;
        }
        let Some(number) = step_in(self.prs.iter().map(|p| p.number), self.selected, step) else { return };
        self.select(number, cx);
    }

    fn set_filter(&mut self, filter: PrFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.list_page = true;
            self.prs.clear();
            self.selected = None;
            self.detail = None;
            self.load_prs(cx);
        }
    }

    fn choose_merge_method(&mut self, method: MergeMethod, cx: &mut Context<Self>) {
        self.merge_method = method;
        if self.last_merge_method != Some(method) {
            self.last_merge_method = Some(method);
            cx.emit(GitHubEvent::MergeMethodChosen(method));
        }
        cx.notify();
    }

    fn choose_account(&mut self, login: Option<String>, cx: &mut Context<Self>) {
        self.chosen = login.clone();
        cx.emit(GitHubEvent::AccountChosen(login));
        self.prs.clear();
        self.selected = None;
        self.detail = None;
        self.runs.clear();
        self.run_selected = None;
        self.jobs = None;
        self.logs.clear();
        self.runs_loaded_at = None;
        self.account = None;
        self.load(cx);
    }

    fn pull(&mut self, cx: &mut Context<Self>) {
        let (root, token) = (self.root.clone(), self.token());
        let Some(remote) = self.remote.clone() else { return };
        self.run(
            "Pulling…",
            move || {
                github::fetch(&root, token.as_deref(), &remote.name)?;
                let before = github::local_status(&root)?.behind;
                github::pull(&root, token.as_deref())?;
                Ok(before)
            },
            |this, behind, cx| {
                this.say(
                    true,
                    match behind {
                        0 => "Already up to date.".to_string(),
                        1 => "Pulled 1 commit.".to_string(),
                        n => format!("Pulled {n} commits."),
                    },
                );
                this.load_local(cx);
            },
            cx,
        );
    }

    fn push(&mut self, cx: &mut Context<Self>) {
        let (root, token) = (self.root.clone(), self.token());
        let (Some(remote), Some(status)) = (self.remote.clone(), self.local.clone()) else { return };
        self.run(
            "Pushing…",
            move || github::push(&root, token.as_deref(), &remote.name, &status),
            |this, _, cx| {
                this.say(true, "Pushed.");
                this.load_local(cx);
                this.load_prs(cx);
            },
            cx,
        );
    }

    fn switch_branch(&mut self, branch: Branch, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let Some(remote) = self.remote.clone() else { return };
        let name = branch.name.clone();
        self.run(
            "Switching branch…",
            move || github::switch(&root, &remote.name, &branch),
            move |this, _, cx| {
                this.say(true, format!("Switched to {name}."));
                this.load_local(cx);
            },
            cx,
        );
    }

    fn checkout(&mut self, cx: &mut Context<Self>) {
        let Some(pr) = self.detail.as_ref().map(|d| d.pr.clone()) else { return };
        let (root, token) = (self.root.clone(), self.token());
        let Some(remote) = self.remote.clone() else { return };
        self.run(
            "Checking out…",
            move || github::checkout(&root, token.as_deref(), &remote.name, &pr),
            |this, branch, cx| {
                this.say(true, format!("Checked out {branch}."));
                this.load_local(cx);
            },
            cx,
        );
    }

    fn send_comment(&mut self, cx: &mut Context<Self>) {
        let body = self.comment_box.read(cx).value().to_string();
        let (Some(remote), Some(token), Some(number)) = (self.remote.clone(), self.token(), self.selected) else {
            return;
        };
        if body.trim().is_empty() {
            return;
        }
        self.run(
            "Commenting…",
            move || github::comment(&token, &remote, number, &body),
            move |this, _, cx| {
                this.say(true, "Comment posted.");
                this.clear_box(this.comment_box.clone(), cx);
                this.load_detail(number, cx);
            },
            cx,
        );
    }

    fn send_review(&mut self, event: ReviewEvent, cx: &mut Context<Self>) {
        let body = self.review_box.read(cx).value().to_string();
        let (Some(remote), Some(token), Some(detail)) = (self.remote.clone(), self.token(), self.detail.as_ref())
        else {
            return;
        };
        let (number, sha, drafts) = (detail.pr.number, detail.head_sha.clone(), self.drafts.clone());
        self.run(
            match event {
                ReviewEvent::Approve => "Approving…",
                ReviewEvent::RequestChanges => "Requesting changes…",
                ReviewEvent::Comment => "Sending review…",
            },
            move || github::submit_review(&token, &remote, number, event, &body, &sha, &drafts),
            move |this, _, cx| {
                this.say(
                    true,
                    match event {
                        ReviewEvent::Approve => format!("Approved #{number}."),
                        ReviewEvent::RequestChanges => format!("Requested changes on #{number}."),
                        ReviewEvent::Comment => "Review sent.".into(),
                    },
                );
                this.drafts.clear();
                this.clear_box(this.review_box.clone(), cx);
                this.load_detail(number, cx);
                this.load_prs(cx);
            },
            cx,
        );
    }

    /// Close the pull request (or reopen it). Text in `with_comment` is posted first, as a
    /// plain comment, and the box emptied.
    fn set_open(&mut self, open: bool, with_comment: Option<Entity<TextareaState>>, cx: &mut Context<Self>) {
        let (Some(remote), Some(token), Some(number)) = (self.remote.clone(), self.token(), self.selected) else {
            return;
        };
        let body = with_comment.as_ref().map(|b| b.read(cx).value().to_string()).unwrap_or_default();
        let commented = !body.trim().is_empty();
        self.run(
            if open { "Reopening…" } else { "Closing…" },
            move || {
                if commented {
                    github::comment(&token, &remote, number, &body)?;
                }
                github::set_open(&token, &remote, number, open)
            },
            move |this, _, cx| {
                this.say(true, if open { format!("Reopened #{number}.") } else { format!("Closed #{number}.") });
                if let (true, Some(input)) = (commented, with_comment) {
                    this.clear_box(input, cx);
                }
                // The list keeps it until the next refresh, so the page doesn't jump to another one.
                if let Some(pr) = this.prs.iter_mut().find(|p| p.number == number) {
                    pr.state = if open { PrState::Open } else { PrState::Closed };
                }
                this.load_detail(number, cx);
            },
            cx,
        );
    }

    /// Whether the account in use opened this pull request; GitHub doesn't let an author
    /// approve or request changes on their own.
    fn is_author(&self, pr: &PullRequest) -> bool {
        self.account.as_ref().is_some_and(|(login, _)| login.eq_ignore_ascii_case(&pr.author))
    }

    /// Where the review box sits: under the diff's last line, or under its last file's
    /// header when that file shows no lines.
    fn review_anchor(&self, detail: &PrDetail, cx: &App) -> Option<DiffAnnotation> {
        let diff = self.diff.read(cx);
        let path = diff.files().last()?.path().clone();
        let line = github::last_patch_line(&detail.patch)
            .map(|(old, line)| {
                let side = if old { DiffSide::Original } else { DiffSide::Modified };
                DiffLinePosition::new(path.clone(), side, line as usize)
            })
            .filter(|at| diff.has_line(at));
        Some(match line {
            Some(at) => DiffAnnotation::line("review", at),
            None => DiffAnnotation::file("review", path),
        })
    }

    /// Open `path` in the review modal, with a summary, a file at a time.
    fn expand_file(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else { return };
        let pr = &detail.pr;
        let body: String = detail.body.chars().take(2_000).collect();
        cx.emit(GitHubEvent::ExpandFile(ExpandFile {
            number: pr.number,
            patch: detail.patch.clone(),
            path,
            context: format!("Pull request #{}: {}\n\n{}", pr.number, pr.title, body.trim()),
        }));
    }

    /// Scroll the diff to the header of the file `step` files from the one at the top.
    fn step_file(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.diff.read(cx).files().len();
        if count == 0 {
            return;
        }
        let to = (self.current_file as isize + step).clamp(0, count as isize - 1) as usize;
        self.scroll_to_file_ix(to, cx);
    }

    fn scroll_to_file_ix(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.diff.read(cx).files().get(ix).map(|f| f.path().clone()) else { return };
        self.current_file = ix;
        self.file_pinned = true;
        self.diff.update(cx, |d, cx| d.scroll_to_file(&path, cx));
        cx.notify();
    }

    /// Scroll the diff to its end, where the review box is.
    fn jump_to_review(&mut self, cx: &mut Context<Self>) {
        self.file_pinned = false;
        let Some(anchor) = self.detail.as_ref().and_then(|d| self.review_anchor(d, cx)) else { return };
        self.diff.update(cx, |d, cx| match anchor.position() {
            Some(at) => d.scroll_to_line(at.clone(), cx),
            None => d.scroll_to_file(&anchor.path().clone(), cx),
        });
    }

    fn merge(&mut self, cx: &mut Context<Self>) {
        let (Some(remote), Some(token), Some(detail)) = (self.remote.clone(), self.token(), self.detail.as_ref())
        else {
            return;
        };
        let (pr, sha, method, delete) =
            (detail.pr.clone(), detail.head_sha.clone(), self.merge_method, self.delete_branch);
        let number = pr.number;
        let (root, fetch_token) = (self.root.clone(), token.clone());
        self.run(
            "Merging…",
            move || {
                github::merge(&token, &remote, &pr, method, &sha, delete)?;
                // Bring the merge in locally, so Pull shows it straight away.
                let _ = github::fetch(&root, Some(&fetch_token), &remote.name);
                Ok(())
            },
            move |this, _, cx| {
                this.say(true, format!("Merged #{number}."));
                this.load_detail(number, cx);
                this.load_prs(cx);
                this.load_local(cx);
            },
            cx,
        );
    }

    fn mark_ready(&mut self, cx: &mut Context<Self>) {
        let (Some(remote), Some(token), Some(number)) = (self.remote.clone(), self.token(), self.selected) else {
            return;
        };
        self.run(
            "Marking ready…",
            move || github::set_ready(&token, &remote, number),
            move |this, _, cx| {
                this.say(true, format!("#{number} is ready for review."));
                this.load_detail(number, cx);
                this.load_prs(cx);
            },
            cx,
        );
    }

    /// Run the picked workflow run again: all of it, or (`failed_only`) the jobs that failed.
    fn rerun(&mut self, failed_only: bool, cx: &mut Context<Self>) {
        let (Some(remote), Some(token), Some(id)) = (self.remote.clone(), self.token(), self.run_selected) else {
            return;
        };
        self.run(
            if failed_only { "Re-running failed jobs…" } else { "Re-running…" },
            move || github::rerun(&token, &remote, id, failed_only),
            move |this, _, cx| {
                this.say(true, if failed_only { "Re-running the failed jobs." } else { "Re-running all jobs." });
                this.logs.clear();
                this.logs_open.clear();
                this.load_runs(cx);
            },
            cx,
        );
    }

    fn cancel_run(&mut self, cx: &mut Context<Self>) {
        let (Some(remote), Some(token), Some(id)) = (self.remote.clone(), self.token(), self.run_selected) else {
            return;
        };
        self.run(
            "Cancelling…",
            move || github::cancel_run(&token, &remote, id),
            |this, _, cx| {
                this.say(true, "Cancel requested.");
                this.load_runs(cx);
            },
            cx,
        );
    }

    fn clear_box(&mut self, input: Entity<TextareaState>, cx: &mut Context<Self>) {
        // Setting a value needs a window; the next render has one.
        self.to_clear.push(input);
        cx.notify();
    }

    /// Start a line comment on what was selected (or the hovered line) in the diff.
    fn start_comment(&mut self, range: &DiffLineRange, window: &mut Window, cx: &mut Context<Self>) {
        let side = range.end_side();
        let start = if range.side() == side { range.start().min(range.end()) } else { range.end() };
        self.open_compose(
            range.path().to_string(),
            side_name(side),
            start as u64,
            range.end() as u64,
            None,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn open_compose(
        &mut self,
        path: String,
        side: String,
        start: u64,
        line: u64,
        reply_to: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = if reply_to.is_some() { "Reply…" } else { "Comment on this line  (Ctrl+Enter adds it)" };
        let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 10).placeholder(placeholder));
        self._subscriptions.push(cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { secondary: true, .. } = event {
                this.finish_compose(false, cx);
            }
        }));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.compose = Some(Compose { path, side, start, line, reply_to, input });
        cx.notify();
    }

    /// Keep the line comment for the review, or (`now`) post it on its own straight away.
    /// A reply always goes straight away.
    fn finish_compose(&mut self, now: bool, cx: &mut Context<Self>) {
        let Some(compose) = self.compose.take() else { return };
        let body = compose.input.read(cx).value().trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }
        let draft = DraftComment {
            path: compose.path,
            side: compose.side,
            start_line: compose.start,
            line: compose.line,
            body,
        };
        let (Some(remote), Some(token), Some(detail)) = (self.remote.clone(), self.token(), self.detail.as_ref())
        else {
            return;
        };
        let (number, sha) = (detail.pr.number, detail.head_sha.clone());
        if let Some(to) = compose.reply_to {
            let text = draft.body.clone();
            self.run(
                "Replying…",
                move || github::reply(&token, &remote, number, to, &text),
                move |this, _, cx| {
                    this.say(true, "Reply posted.");
                    this.load_detail(number, cx);
                },
                cx,
            );
        } else if now {
            self.run(
                "Commenting…",
                move || github::submit_review(&token, &remote, number, ReviewEvent::Comment, "", &sha, &[draft]),
                move |this, _, cx| {
                    this.say(true, "Comment posted.");
                    this.load_detail(number, cx);
                },
                cx,
            );
        } else {
            self.drafts.push(draft);
        }
        cx.notify();
    }

    // ---------- rendering ----------

    fn colors(cx: &App) -> Colors {
        let (ansi, _, _) = crate::terminal::palette(cx);
        let theme = cx.theme();
        Colors {
            fg: theme.foreground,
            muted: theme.muted_foreground,
            border: theme.border,
            card: theme.secondary,
            bg: theme.background,
            accent: theme.primary,
            green: ansi[2],
            red: ansi[1],
            yellow: ansi[3],
            purple: ansi[5],
            blue: ansi[4],
            palette: [
                ansi[1], ansi[2], ansi[3], ansi[4], ansi[5], ansi[6], ansi[9], ansi[10], ansi[12], ansi[13], ansi[14],
            ],
            mono: theme.mono_font_family.clone(),
        }
    }

    fn avatar(&self, login: &str, size: f32, c: &Colors) -> AnyElement {
        match self.avatars.get(login) {
            Some(image) => img(image.clone()).size(px(size)).rounded_full().flex_none().into_any_element(),
            None => {
                let hue = c.palette[(login.bytes().map(u64::from).sum::<u64>() as usize) % c.palette.len()];
                div()
                    .size(px(size))
                    .flex_none()
                    .rounded_full()
                    .bg(hue.opacity(0.25))
                    .text_color(hue)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(size * 0.48))
                    .font_weight(FontWeight::BOLD)
                    .child(login.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default())
                    .into_any_element()
            }
        }
    }

    fn render_message(
        &self,
        icon: IconName,
        title: &str,
        body: &str,
        extra: Option<Button>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .size(px(64.))
                    .rounded_full()
                    .bg(c.accent.opacity(0.12))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).large().text_color(c.accent)),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
            .child(div().max_w(px(460.)).text_sm().text_color(c.muted).text_center().child(body.to_string()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("github-retry")
                            .small()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("Try again")
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    )
                    .children(extra),
            )
            .into_any_element()
    }

    /// The band across the top: repository, account, branch, and the sync buttons.
    fn render_header(&self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let remote = self.remote.clone();
        let slug_owner = remote.as_ref().map(|r| r.owner.clone()).unwrap_or_default();
        let slug_repo = remote.as_ref().map(|r| r.repo.clone()).unwrap_or_default();
        let login = self.account.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
        let this = cx.entity();

        let account_menu = {
            let (accounts, chosen, this) = (self.accounts.clone(), self.chosen.clone(), this.clone());
            Button::new("github-account")
                .small()
                .ghost()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(self.avatar(&login, 18., c))
                        .child(div().text_sm().child(format!("@{login}")))
                        .when(self.chosen.is_none(), |el| el.child(div().text_xs().text_color(c.muted).child("auto")))
                        .child(Icon::new(IconName::ChevronDown).xsmall().text_color(c.muted)),
                )
                .tooltip("The GitHub account this project uses")
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.label("GitHub account for this project");
                    for account in &accounts {
                        let (this, account) = (this.clone(), account.clone());
                        menu = menu.item(
                            PopupMenuItem::new(format!("@{account}"))
                                .checked(chosen.as_deref() == Some(account.as_str()))
                                .on_click(move |_, _, cx| {
                                    this.update(cx, |v, cx| v.choose_account(Some(account.clone()), cx))
                                }),
                        );
                    }
                    let this = this.clone();
                    menu.separator().item(
                        PopupMenuItem::new("Pick from the remote")
                            .checked(chosen.is_none())
                            .on_click(move |_, _, cx| this.update(cx, |v, cx| v.choose_account(None, cx))),
                    )
                })
        };

        let local = self.local.clone().unwrap_or_default();
        let branch_label = local.branch.clone().unwrap_or_else(|| "detached".into());
        let branch_menu = {
            let (branches, current, this) = (self.branches.clone(), local.branch.clone(), this.clone());
            Button::new("github-branch")
                .small()
                .ghost()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Icon::new(IconName::GitBranch).small().text_color(c.accent))
                        .child(div().text_sm().max_w(px(220.)).overflow_hidden().text_ellipsis().child(branch_label))
                        .when(local.dirty > 0, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .px_1p5()
                                    .rounded_full()
                                    .bg(c.yellow.opacity(0.18))
                                    .text_color(c.yellow)
                                    .child(format!("{} changed", local.dirty)),
                            )
                        })
                        .child(Icon::new(IconName::ChevronDown).xsmall().text_color(c.muted)),
                )
                .tooltip("Switch the project's branch")
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.min_w(px(300.)).max_h(px(420.)).scrollable(true).label("Switch branch");
                    for branch in &branches {
                        let (this, b) = (this.clone(), branch.clone());
                        let label = if branch.remote_only {
                            format!("{}  · remote", branch.name)
                        } else {
                            branch.name.clone()
                        };
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(current.as_deref() == Some(branch.name.as_str()))
                                .on_click(move |_, _, cx| this.update(cx, |v, cx| v.switch_branch(b.clone(), cx))),
                        );
                    }
                    menu
                })
        };

        let busy = self.busy;
        let behind = local.behind;
        let ahead = local.ahead;
        let can_push = local.branch.is_some() && (ahead > 0 || local.upstream.is_none());
        let pull = Button::new("github-pull")
            .small()
            .icon(IconName::ArrowDownToLine)
            .label(if behind > 0 { format!("Pull {behind}") } else { "Pull".into() })
            .map(|b| if behind > 0 { b.primary() } else { b.outline() })
            .disabled(busy.is_some() || local.upstream.is_none())
            .tooltip(if local.upstream.is_none() {
                "This branch isn't tracking a remote branch".to_string()
            } else {
                format!("Bring in the new commits on {} (Ctrl+Alt+L)", local.upstream.clone().unwrap_or_default())
            })
            .on_click(cx.listener(|this, _, _, cx| this.pull(cx)));
        let push = can_push.then(|| {
            Button::new("github-push")
                .small()
                .outline()
                .icon(IconName::ArrowUpFromLine)
                .label(if ahead > 0 { format!("Push {ahead}") } else { "Publish".into() })
                .disabled(busy.is_some())
                .on_click(cx.listener(|this, _, _, cx| this.push(cx)))
        });
        let web = remote.as_ref().map(Remote::web_url);

        let flash = self.flash.clone().filter(|f| !f.ok || f.at.elapsed() < Duration::from_secs(8));

        let page = self.page;
        let page_tab = |id: &'static str, p: Page, icon: IconName, label: &'static str, count: Option<usize>| {
            let on = page == p;
            h_flex()
                .id(id)
                .test_support()
                .gap_1p5()
                .px_3()
                .py_2()
                .text_sm()
                .cursor_pointer()
                .border_b_2()
                .map(|el| {
                    if on {
                        el.border_color(c.accent).text_color(c.fg).font_weight(FontWeight::MEDIUM)
                    } else {
                        el.border_color(transparent_black()).text_color(c.muted).hover(|el| el.text_color(c.fg))
                    }
                })
                .child(Icon::new(icon).small())
                .child(label)
                .when_some(count.filter(|n| *n > 0), |el, n| {
                    el.child(div().px_1p5().rounded_full().bg(c.fg.opacity(0.08)).text_xs().child(n.to_string()))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.show_page(p, cx);
                    if p == Page::Tracked {
                        this.tracked.update(cx, |t, cx| t.shown(window, cx));
                    } else {
                        window.focus(&this.focus, cx);
                    }
                }))
        };
        let open_prs = (self.filter == PrFilter::Open).then_some(self.prs.len());
        let running = self.runs.iter().filter(|r| r.state.active()).count();
        let tabs = h_flex()
            .gap_1()
            // The picked tab's underline sits on the band's bottom border.
            .mb(px(-1.))
            .child(page_tab("github-page-prs", Page::PullRequests, IconName::GitPullRequest, "Pull requests", open_prs))
            .child(page_tab("github-page-actions", Page::Actions, IconName::Workflow, "Actions", Some(running)))
            .child(page_tab(
                "github-page-tracked",
                Page::Tracked,
                IconName::FolderGit2,
                "Other repos",
                Some(self.tracked.read(cx).active(cx)),
            ));

        v_flex()
            .flex_none()
            .px_4()
            .pt_3()
            .gap_2()
            .border_b_1()
            .border_color(c.border)
            // The content's rounded top corners, so the tint follows them like the other tabs.
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .bg(linear_gradient(
                100.,
                linear_color_stop(c.accent.opacity(0.10), 0.),
                linear_color_stop(c.bg.opacity(0.0), 0.7),
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .size(px(34.))
                            .flex_none()
                            .rounded_lg()
                            .bg(c.fg.opacity(0.08))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Github).text_color(c.fg)),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .text_lg()
                                    .child(div().text_color(c.muted).child(format!("{slug_owner} /")))
                                    .child(div().font_weight(FontWeight::BOLD).child(slug_repo))
                                    .when(self.info.private, |el| el.child(pill("Private", c.muted))),
                            )
                            .children(self.info.description.clone().map(|d| {
                                div()
                                    .text_xs()
                                    .text_color(c.muted)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(d)
                            })),
                    )
                    .child(div().flex_1())
                    .when_some(busy, |el, busy| {
                        el.child(
                            h_flex()
                                .gap_1p5()
                                .text_xs()
                                .text_color(c.muted)
                                .child(Spinner::new().xsmall().color(c.accent))
                                .child(busy),
                        )
                    })
                    .child(account_menu),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(branch_menu)
                    .when(local.upstream.is_some(), |el| el.child(sync_counts(ahead, behind, c)))
                    .child(div().flex_1())
                    .child(pull)
                    .children(push)
                    .child(
                        Button::new("github-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .tooltip("Fetch and reload (F5)")
                            .disabled(busy.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(true, cx))),
                    )
                    .when_some(web, |el, url| {
                        el.child(
                            Button::new("github-open-web")
                                .small()
                                .ghost()
                                .icon(IconName::ExternalLink)
                                .tooltip("Open on GitHub")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .when_some(flash, |el, flash| {
                let color = if flash.ok { c.green } else { c.red };
                el.child(
                    h_flex()
                        .id("github-flash")
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(color.opacity(0.12))
                        .border_1()
                        .border_color(color.opacity(0.35))
                        .text_sm()
                        .child(
                            Icon::new(if flash.ok { IconName::CircleCheck } else { IconName::CircleAlert })
                                .small()
                                .text_color(color),
                        )
                        .child(div().flex_1().min_w_0().child(flash.text))
                        .child(
                            div()
                                .id("github-flash-close")
                                .cursor_pointer()
                                .text_color(c.muted)
                                .hover(|el| el.text_color(c.fg))
                                .child(Icon::new(IconName::X).xsmall())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.flash = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(tabs)
    }

    fn render_list(&self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let now = chrono::Utc::now();
        let filter = self.filter;
        let segments = PrFilter::ALL.map(|f| {
            let on = f == filter;
            div()
                .id(SharedString::from(format!("github-filter-{}", f.label())))
                .test_support()
                .flex_1()
                .py_1()
                .rounded_md()
                .text_center()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .map(|el| {
                    if on {
                        el.bg(c.bg).text_color(c.fg).shadow_sm()
                    } else {
                        el.text_color(c.muted).hover(|el| el.text_color(c.fg))
                    }
                })
                .child(f.label())
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
        });
        let rows = self.prs.iter().map(|pr| {
            let selected = self.selected == Some(pr.number);
            let number = pr.number;
            let (icon, color) = state_look(pr.state, c);
            div()
                .id(SharedString::from(format!("github-pr-{number}")))
                .test_support()
                .relative()
                .mx_2()
                .mb_1()
                .px_3()
                .py_2p5()
                .rounded_lg()
                .cursor_pointer()
                .map(|el| if selected { el.bg(c.card) } else { el.hover(|el| el.bg(c.card.opacity(0.5))) })
                .when(selected, |el| {
                    el.child(
                        div().absolute().left_0().top(px(10.)).bottom(px(10.)).w(px(3.)).rounded_full().bg(c.accent),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.list_page = false;
                    this.select(number, cx)
                }))
                .child(
                    h_flex()
                        .gap_2p5()
                        .items_start()
                        .child(div().pt_0p5().child(Icon::new(icon).small().text_color(color)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .line_clamp(2)
                                        .child(pr.title.clone()),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(c.muted)
                                        .child(format!("#{number}"))
                                        .child("·")
                                        .child(self.avatar(&pr.author, 14., c))
                                        .child(
                                            div()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(pr.author.clone()),
                                        )
                                        .child("·")
                                        .child(github::ago(&pr.updated_at, now))
                                        .child(div().flex_1())
                                        .when(pr.comments > 0, |el| {
                                            el.child(
                                                h_flex()
                                                    .gap_0p5()
                                                    .child(Icon::new(IconName::MessageSquare).xsmall())
                                                    .child(pr.comments.to_string()),
                                            )
                                        })
                                        .children(checks_icon(pr.checks_state(), c)),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .child(branch_chip(&pr.head, c))
                                        .child(div().text_color(c.muted).child("→"))
                                        .child(div().text_color(c.muted).child(pr.base.clone()))
                                        .child(div().flex_1())
                                        .children(review_pill(&pr.review_decision, c))
                                        .child(div().text_color(c.green).child(format!("+{}", pr.additions)))
                                        .child(div().text_color(c.red).child(format!("−{}", pr.deletions))),
                                )
                                .when(!pr.labels.is_empty(), |el| {
                                    el.child(h_flex().gap_1().flex_wrap().children(
                                        pr.labels.iter().map(|l| pill(l, c.palette[l.len() % c.palette.len()])),
                                    ))
                                }),
                        ),
                )
                .into_any_element()
        });
        let empty = self.prs.is_empty();
        v_flex()
            .map(|el| if self.narrow() { el.flex_1().min_w_0() } else { el.w(px(LIST_WIDTH)).flex_none().border_r_1() })
            .h_full()
            .border_color(c.border)
            .child(h_flex().m_2().p_0p5().gap_0p5().rounded_lg().bg(c.card).children(segments))
            .child(
                div()
                    .id("github-pr-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .pb_2()
                    .map(|el| {
                        if empty {
                            el.child(
                                v_flex()
                                    .items_center()
                                    .gap_2()
                                    .pt_10()
                                    .text_sm()
                                    .text_color(c.muted)
                                    .child(if self.prs_loading {
                                        Spinner::new().small().color(c.accent).into_any_element()
                                    } else {
                                        Icon::new(IconName::GitPullRequest)
                                            .large()
                                            .text_color(c.muted.opacity(0.5))
                                            .into_any_element()
                                    })
                                    .child(if self.prs_loading {
                                        "Loading pull requests…".to_string()
                                    } else {
                                        format!("No {} pull requests", filter.label().to_lowercase())
                                    }),
                            )
                        } else {
                            el.children(rows)
                        }
                    }),
            )
    }

    fn render_detail(&self, window: &mut Window, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            let back = self.back_to_list(cx);
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .when_some(back, |el, back| el.child(h_flex().px_3().pt_3().child(back)))
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_sm()
                        .text_color(c.muted)
                        .child(if self.detail_loading || self.selected.is_some() {
                            Spinner::new().color(c.accent).into_any_element()
                        } else {
                            Icon::new(IconName::GitPullRequestArrow)
                                .large()
                                .text_color(c.muted.opacity(0.5))
                                .into_any_element()
                        })
                        .when(self.selected.is_none() && !self.prs_loading, |el| el.child("Pick a pull request")),
                )
                .into_any_element();
        };
        let pr = &detail.pr;
        let (icon, color) = state_look(pr.state, c);
        let now = chrono::Utc::now();
        let checked_out = self.local.as_ref().and_then(|l| l.branch.clone()) == Some(github::local_branch_for(pr));
        let open = matches!(pr.state, PrState::Open | PrState::Draft);
        let section = self.section;
        let tab = |id: &'static str, s: Section, icon: IconName, label: String, cx: &mut Context<Self>| {
            let on = section == s;
            h_flex()
                .id(id)
                .test_support()
                .gap_1p5()
                .px_3()
                .py_2()
                .text_sm()
                .cursor_pointer()
                .border_b_2()
                .map(|el| {
                    if on {
                        el.border_color(c.accent).text_color(c.fg).font_weight(FontWeight::MEDIUM)
                    } else {
                        el.border_color(transparent_black()).text_color(c.muted).hover(|el| el.text_color(c.fg))
                    }
                })
                .child(Icon::new(icon).small())
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.section = s;
                    cx.notify();
                }))
        };
        let conversation_count = detail.timeline.len();
        let tabs = h_flex()
            .flex_wrap()
            .px_5()
            .gap_1()
            .border_b_1()
            .border_color(c.border)
            .child(tab(
                "github-tab-conversation",
                Section::Conversation,
                IconName::MessageSquare,
                format!("Conversation {conversation_count}"),
                cx,
            ))
            .child(tab(
                "github-tab-files",
                Section::Files,
                IconName::FileDiff,
                format!("Files changed {}", pr.changed_files),
                cx,
            ))
            .child(tab(
                "github-tab-commits",
                Section::Commits,
                IconName::GitCommitHorizontal,
                format!("Commits {}", detail.commits.len()),
                cx,
            ))
            .child(div().flex_1())
            .when(section == Section::Files, |el| {
                let mode = self.diff.read(cx).mode();
                el.child(h_flex().mr_3().p_0p5().gap_0p5().rounded_lg().bg(c.card).children(
                    [(DiffMode::Unified, "Unified"), (DiffMode::Split, "Split")].map(|(m, label)| {
                        let on = mode == m;
                        let state = self.diff.clone();
                        div()
                            .id(SharedString::from(format!("github-diff-mode-{label}")))
                            .test_support()
                            .px_2p5()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .map(|el| {
                                if on {
                                    el.bg(c.accent.opacity(0.18)).text_color(c.fg).shadow_sm()
                                } else {
                                    el.text_color(c.muted).hover(|el| el.text_color(c.fg))
                                }
                            })
                            .child(label)
                            .on_click(move |_, _, cx| state.update(cx, |d, cx| d.set_mode(m, cx)))
                    }),
                ))
            })
            .child(
                h_flex()
                    .gap_1()
                    .text_sm()
                    .child(div().text_color(c.green).child(format!("+{}", pr.additions)))
                    .child(div().text_color(c.red).child(format!("−{}", pr.deletions)))
                    .child(diff_blocks(pr.additions, pr.deletions, c)),
            );

        let url = pr.url.clone();
        let back = self.back_to_list(cx);
        let header = v_flex()
            .px_5()
            .pt_4()
            .pb_3()
            .gap_2()
            .when_some(back, |el, back| el.child(h_flex().ml(px(-10.)).child(back)))
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(pr.title.clone())
                            .child(
                                div()
                                    .text_color(c.muted)
                                    .font_weight(FontWeight::NORMAL)
                                    .child(format!(" #{}", pr.number)),
                            ),
                    )
                    .when(open, |el| {
                        el.child(
                            Button::new("github-checkout")
                                .small()
                                .map(|b| if checked_out { b.ghost() } else { b.outline() })
                                .icon(if checked_out { IconName::Check } else { IconName::GitBranch })
                                .label(if checked_out { "Checked out" } else { "Check out" })
                                .tooltip(format!("Switch the project folder to {}", github::local_branch_for(pr)))
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.checkout(cx))),
                        )
                    })
                    .child(
                        Button::new("github-pr-web")
                            .small()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .tooltip("Open on GitHub")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .text_sm()
                    .text_color(c.muted)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded_full()
                            .bg(color)
                            .text_color(c.bg)
                            .font_weight(FontWeight::MEDIUM)
                            .child(Icon::new(icon).small())
                            .child(pr.state.label()),
                    )
                    .child(self.avatar(&pr.author, 18., c))
                    .child(div().text_color(c.fg).font_weight(FontWeight::MEDIUM).child(pr.author.clone()))
                    .child(if pr.state == PrState::Merged { "merged" } else { "wants to merge" })
                    .child(format!(
                        "{} commit{} into",
                        detail.commits.len(),
                        if detail.commits.len() == 1 { "" } else { "s" }
                    ))
                    .child(branch_chip(&pr.base, c))
                    .child("from")
                    .child(branch_chip(&pr.head, c))
                    .child(format!("· opened {}", github::ago(&detail.created_at, now))),
            );

        let body = match self.section {
            Section::Conversation => self.render_conversation(detail, c, cx).into_any_element(),
            Section::Files => self.render_files(detail, window, c, cx),
            Section::Commits => self.render_commits(detail, c).into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_conversation(&self, detail: &PrDetail, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let now = chrono::Utc::now();
        let pr = &detail.pr;
        let entry =
            |id: String, author: &str, at: &str, badge: Option<AnyElement>, body: &str, accent: Option<Hsla>| {
                h_flex().gap_3().items_start().child(self.avatar(author, 32., c)).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .rounded_lg()
                        .border_1()
                        .border_color(accent.map(|a| a.opacity(0.45)).unwrap_or(c.border))
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .bg(accent.map(|a| a.opacity(0.10)).unwrap_or(c.card))
                                .text_sm()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(author.to_string()))
                                .child(div().text_color(c.muted).child(github::ago(at, now)))
                                .children(badge),
                        )
                        .when(!body.trim().is_empty(), |el| {
                            el.child(
                                div().px_3().py_2().text_sm().child(
                                    TextView::markdown(SharedString::from(id), body.to_string()).selectable(true),
                                ),
                            )
                        }),
                )
            };
        let description = entry(
            format!("github-body-{}", pr.number),
            &pr.author,
            &detail.created_at,
            Some(pill("Author", c.muted).into_any_element()),
            if detail.body.trim().is_empty() { "_No description provided._" } else { &detail.body },
            None,
        );
        let timeline = detail.timeline.iter().enumerate().map(|(ix, item)| {
            let (badge, accent) = match item.review.as_deref() {
                Some("APPROVED") => (Some(review_badge(IconName::CircleCheck, "approved", c.green)), Some(c.green)),
                Some("CHANGES_REQUESTED") => {
                    (Some(review_badge(IconName::FileDiff, "requested changes", c.red)), Some(c.red))
                }
                Some("DISMISSED") => (Some(review_badge(IconName::CircleSlash, "review dismissed", c.muted)), None),
                Some(_) => (Some(review_badge(IconName::Eye, "reviewed", c.blue)), None),
                None => (None, None),
            };
            entry(format!("github-item-{}-{ix}", pr.number), &item.author, &item.at, badge, &item.body, accent)
        });
        let open = matches!(pr.state, PrState::Open | PrState::Draft);
        div().id("github-conversation").size_full().overflow_y_scroll().child(
            v_flex()
                .max_w(px(920.))
                .px_5()
                .py_4()
                .gap_4()
                .child(description)
                .children(timeline)
                .child(self.render_merge_box(detail, c, cx))
                .when(open || pr.state == PrState::Closed, |el| {
                    el.child(
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(self.avatar(self.account.as_ref().map(|(l, _)| l.as_str()).unwrap_or(""), 32., c))
                            .child(
                                v_flex().flex_1().gap_2().child(Textarea::new(&self.comment_box)).child(
                                    h_flex()
                                        .gap_2()
                                        .justify_end()
                                        .child(
                                            Button::new("github-conversation-close")
                                                .small()
                                                .outline()
                                                .map(|b| {
                                                    let has_text = !self.comment_box.read(cx).value().trim().is_empty();
                                                    if open {
                                                        b.icon(IconName::GitPullRequestClosed).label(if has_text {
                                                            "Close with comment"
                                                        } else {
                                                            "Close pull request"
                                                        })
                                                    } else {
                                                        b.icon(IconName::GitPullRequest).label(if has_text {
                                                            "Reopen with comment"
                                                        } else {
                                                            "Reopen pull request"
                                                        })
                                                    }
                                                })
                                                .disabled(self.busy.is_some())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    let input = this.comment_box.clone();
                                                    this.set_open(!open, Some(input), cx)
                                                })),
                                        )
                                        .child(
                                            Button::new("github-comment")
                                                .small()
                                                .primary()
                                                .icon(IconName::MessageSquare)
                                                .label("Comment")
                                                .disabled(self.busy.is_some())
                                                .on_click(cx.listener(|this, _, _, cx| this.send_comment(cx))),
                                        ),
                                ),
                            ),
                    )
                }),
        )
    }

    fn render_merge_box(&self, detail: &PrDetail, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let pr = &detail.pr;
        let checks = &pr.checks;
        let rollup = pr.checks_state();
        let (tone, icon, headline, note): (Hsla, IconName, String, String) = match pr.state {
            PrState::Merged => {
                (c.purple, IconName::GitMerge, "Merged".into(), format!("{} is in {}.", pr.head, pr.base))
            }
            PrState::Closed => {
                (c.red, IconName::GitPullRequestClosed, "Closed".into(), "Closed without merging.".into())
            }
            PrState::Draft => (
                c.muted,
                IconName::GitPullRequestDraft,
                "This pull request is still a draft".into(),
                "Mark it ready for review to merge it.".into(),
            ),
            PrState::Open => match (detail.mergeable.as_str(), detail.merge_state.as_str()) {
                ("CONFLICTING", _) | (_, "DIRTY") => (
                    c.red,
                    IconName::GitMergeConflict,
                    "This branch has conflicts".into(),
                    format!("Resolve them against {} before merging.", pr.base),
                ),
                (_, "BEHIND") => (
                    c.yellow,
                    IconName::TriangleAlert,
                    "This branch is out of date".into(),
                    format!("{} has moved on; the repository wants it brought in first.", pr.base),
                ),
                (_, "BLOCKED") => (
                    c.yellow,
                    IconName::ShieldAlert,
                    "Merging is blocked".into(),
                    match pr.review_decision.as_str() {
                        "REVIEW_REQUIRED" => "A review is required.".into(),
                        "CHANGES_REQUESTED" => "Changes were requested.".into(),
                        _ => "Required checks or rules haven't passed.".into(),
                    },
                ),
                (_, "UNSTABLE") => {
                    (c.yellow, IconName::TriangleAlert, "Some checks failed".into(), "You can still merge.".into())
                }
                ("UNKNOWN", _) => (c.muted, IconName::LoaderCircle, "Checking mergeability…".into(), String::new()),
                _ => {
                    (c.green, IconName::CircleCheck, "Ready to merge".into(), format!("No conflicts with {}.", pr.base))
                }
            },
        };
        let check_rows = checks.iter().enumerate().map(|(ix, check)| {
            let url = check.url.clone();
            h_flex()
                .id(SharedString::from(format!("github-check-{ix}")))
                .gap_2()
                .px_4()
                .py_1p5()
                .text_sm()
                .border_t_1()
                .border_color(c.border)
                .when(url.is_some(), |el| el.cursor_pointer().hover(|el| el.bg(c.fg.opacity(0.04))))
                .on_click(move |_, _, cx| {
                    if let Some(url) = &url {
                        cx.open_url(url)
                    }
                })
                .children(checks_icon(check.state, c))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(check.name.clone()),
                )
                .child(div().text_xs().text_color(c.muted).child(check.status.clone()))
        });
        let summary = match rollup {
            ChecksState::None => None,
            ChecksState::Passing => Some(format!("All {} checks passed", checks.len())),
            ChecksState::Pending => Some(format!(
                "{} of {} checks still running",
                checks.iter().filter(|c| c.state == ChecksState::Pending).count(),
                checks.len()
            )),
            ChecksState::Failing => Some(format!(
                "{} of {} checks failed",
                checks.iter().filter(|c| c.state == ChecksState::Failing).count(),
                checks.len()
            )),
        };
        let methods: Vec<MergeMethod> = MergeMethod::ALL.into_iter().filter(|m| m.allowed(&self.info)).collect();
        let method = self.merge_method;
        let this = cx.entity();
        let merge_controls = (pr.state == PrState::Open).then(|| {
            h_flex()
                .gap_2()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(c.border)
                .child(
                    Button::new("github-merge")
                        .primary()
                        .icon(IconName::GitMerge)
                        .label(method.label())
                        .disabled(self.busy.is_some() || !self.info.can_push)
                        .on_click(cx.listener(|this, _, _, cx| this.merge(cx))),
                )
                .when(methods.len() > 1, |el| {
                    el.child(
                        Button::new("github-merge-method")
                            .outline()
                            .icon(IconName::ChevronDown)
                            .tooltip("Merge method")
                            .dropdown_menu(move |menu, _, _| {
                                let mut menu = menu.label("Merge method");
                                for m in &methods {
                                    let (this, m) = (this.clone(), *m);
                                    menu = menu.item(PopupMenuItem::new(m.label()).checked(m == method).on_click(
                                        move |_, _, cx| this.update(cx, |v, cx| v.choose_merge_method(m, cx)),
                                    ));
                                }
                                menu
                            }),
                    )
                })
                .child(div().flex_1())
                .when(!pr.cross_repo, |el| {
                    el.child(
                        Switch::new("github-delete-branch")
                            .checked(self.delete_branch)
                            .label(format!("Delete {}", pr.head))
                            .small()
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.delete_branch = *checked;
                                cx.notify();
                            })),
                    )
                })
        });
        let ready = (pr.state == PrState::Draft).then(|| {
            h_flex().px_4().py_3().border_t_1().border_color(c.border).child(
                Button::new("github-ready")
                    .outline()
                    .icon(IconName::Eye)
                    .label("Ready for review")
                    .disabled(self.busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.mark_ready(cx))),
            )
        });
        h_flex()
            .gap_3()
            .items_start()
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .rounded_full()
                    .bg(tone)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).small().text_color(c.bg)),
            )
            .child(
                v_flex()
                    .id("github-merge-box")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .rounded_lg()
                    .border_1()
                    .border_color(tone.opacity(0.55))
                    .overflow_hidden()
                    .bg(c.card)
                    .when_some(summary, |el, summary| {
                        el.child(
                            h_flex()
                                .gap_2()
                                .px_4()
                                .py_2p5()
                                .children(checks_icon(rollup, c))
                                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(summary)),
                        )
                        .children(check_rows)
                    })
                    .child(
                        v_flex()
                            .px_4()
                            .py_2p5()
                            .when(!checks.is_empty(), |el| el.border_t_1().border_color(c.border))
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(tone).child(headline))
                            .when(!note.is_empty(), |el| el.child(div().text_sm().text_color(c.muted).child(note))),
                    )
                    .children(merge_controls)
                    .children(ready),
            )
    }

    fn render_files(&self, detail: &PrDetail, window: &mut Window, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let _ = window;
        let this = cx.entity().downgrade();
        // Line comments in threads, under the line they're on (outdated ones under the file).
        let mut threads: Vec<(u64, Vec<LineComment>)> = Vec::new();
        for comment in &detail.line_comments {
            match comment.reply_to.and_then(|to| threads.iter_mut().find(|(root, _)| *root == to)) {
                Some((_, thread)) => thread.push(comment.clone()),
                None => threads.push((comment.id, vec![comment.clone()])),
            }
        }
        enum Note {
            Thread(Vec<LineComment>),
            Draft(usize, DraftComment),
            Compose(Entity<TextareaState>, bool),
            /// The review box, at the end of the diff.
            Review,
        }
        let mut notes: Vec<(DiffAnnotation, Note)> = Vec::new();
        for (root, thread) in threads {
            let first = &thread[0];
            let id = SharedString::from(format!("thread-{root}"));
            let annotation = match first.line {
                Some(line) => DiffAnnotation::line(
                    id,
                    DiffLinePosition::new(first.path.clone(), side_of(&first.side), line as usize),
                ),
                None => DiffAnnotation::file(id, first.path.clone()),
            };
            notes.push((annotation, Note::Thread(thread)));
        }
        for (ix, draft) in self.drafts.iter().enumerate() {
            let id = SharedString::from(format!("draft-{ix}"));
            let at = DiffLinePosition::new(draft.path.clone(), side_of(&draft.side), draft.line as usize);
            notes.push((DiffAnnotation::line(id, at), Note::Draft(ix, draft.clone())));
        }
        if let Some(compose) = &self.compose {
            let at = DiffLinePosition::new(compose.path.clone(), side_of(&compose.side), compose.line as usize);
            notes.push((
                DiffAnnotation::line("compose", at),
                Note::Compose(compose.input.clone(), compose.reply_to.is_some()),
            ));
        }
        let open = matches!(detail.pr.state, PrState::Open | PrState::Draft);
        // Last, so it comes after any comment on the same line.
        let review_anchor = if open { self.review_anchor(detail, cx) } else { None };
        if let Some(anchor) = review_anchor.clone() {
            notes.push((anchor, Note::Review));
        }
        let annotations: Vec<DiffAnnotation> = notes.iter().map(|(a, _)| a.clone()).collect();
        let avatars = self.avatars.clone();
        let palette = c.palette;
        let notes = std::rc::Rc::new(notes);
        let busy = self.busy.is_some();
        let review_card = {
            let this = this.clone();
            let input = self.review_box.clone();
            let pending = self.drafts.len();
            let author = self.is_author(&detail.pr);
            move |cx: &mut App| -> AnyElement {
                let c = GitHubView::colors(cx);
                let has_text = !input.read(cx).value().trim().is_empty();
                let (t1, t2, t3, t4) = (this.clone(), this.clone(), this.clone(), this.clone());
                let own = "You opened this pull request, and GitHub doesn't let an author review their own.";
                v_flex()
                    .mx_3()
                    .mt_2()
                    // Room for the file navigation floating over the bottom of the diff.
                    .mb(px(56.))
                    .p_3()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(c.accent.opacity(0.4))
                    .bg(c.card)
                    .font_family(cx.theme().font_family.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(IconName::Eye).small().text_color(c.accent))
                            .child("Finish your review")
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(if pending > 0 { c.yellow } else { c.muted })
                                    .child(match pending {
                                        0 => "Hover a line and press + to comment on it; select lines for a range."
                                            .to_string(),
                                        1 => "1 line comment will go with the review.".to_string(),
                                        n => format!("{n} line comments will go with the review."),
                                    }),
                            ),
                    )
                    .child(Textarea::new(&input))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("github-close")
                                    .small()
                                    .ghost()
                                    .icon(IconName::GitPullRequestClosed)
                                    .label(if has_text { "Close with comment" } else { "Close pull request" })
                                    .disabled(busy)
                                    .on_click({
                                        let input = input.clone();
                                        move |_, _, cx| {
                                            let input = input.clone();
                                            let _ = t1.update(cx, |v, cx| v.set_open(false, Some(input), cx));
                                        }
                                    }),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("github-review-comment")
                                    .small()
                                    .outline()
                                    .icon(IconName::MessageSquare)
                                    .label("Comment")
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        let _ = t2.update(cx, |v, cx| v.send_review(ReviewEvent::Comment, cx));
                                    }),
                            )
                            .child(
                                Button::new("github-request-changes")
                                    .small()
                                    .outline()
                                    .icon(IconName::FileDiff)
                                    .label("Request changes")
                                    .disabled(busy || author)
                                    .when(author, |b| b.tooltip(own))
                                    .on_click(move |_, _, cx| {
                                        let _ = t3.update(cx, |v, cx| v.send_review(ReviewEvent::RequestChanges, cx));
                                    }),
                            )
                            .child(
                                Button::new("github-approve")
                                    .small()
                                    .success()
                                    .icon(IconName::Check)
                                    .label("Approve")
                                    .disabled(busy || author)
                                    .when(author, |b| b.tooltip(own))
                                    .on_click(move |_, _, cx| {
                                        let _ = t4.update(cx, |v, cx| v.send_review(ReviewEvent::Approve, cx));
                                    }),
                            ),
                    )
                    .into_any_element()
            }
        };
        let review_card = Rc::new(review_card);
        let card_for_diff = review_card.clone();

        let paths: Vec<SharedString> = self.diff.read(cx).files().iter().map(|f| f.path().clone()).collect();
        let file_count = paths.len();
        let headers = self.headers_seen.clone();
        let expand = this.clone();
        let diff = Diff::new(&self.diff)
            .syntax_highlight(true)
            .render_header_suffix(move |file, _, _| {
                let (this, path) = (expand.clone(), file.path().to_string());
                Button::new("github-expand-file")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Maximize2)
                    .tooltip("Open with its summary")
                    .on_click(move |_, _, cx| {
                        let path = path.clone();
                        let _ = this.update(cx, |v, cx| v.expand_file(path, cx));
                    })
            })
            // An empty marker in each header says where it was painted.
            .render_header_prefix(move |file, _, _| {
                let (headers, ix) = (headers.clone(), paths.iter().position(|p| p == file.path()));
                canvas(
                    move |bounds, _, _| {
                        if let Some(ix) = ix {
                            headers.borrow_mut().push((ix, bounds.top()));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_0()
            })
            .annotations(annotations)
            .on_add_annotation({
                let this = this.clone();
                move |range, window, cx| {
                    let _ = this.update(cx, |v, cx| v.start_comment(range, window, cx));
                }
            })
            .render_annotation(move |annotation, _window, cx| {
                let c = GitHubView::colors(cx);
                let Some((_, note)) = notes.iter().find(|(a, _)| a.id() == annotation.id()) else {
                    return div().into_any_element();
                };
                if let Note::Review = note {
                    return card_for_diff(cx);
                }
                let avatar = |login: &str| match avatars.get(login) {
                    Some(image) => img(image.clone()).size(px(20.)).rounded_full().flex_none().into_any_element(),
                    None => {
                        let hue = palette[(login.bytes().map(u64::from).sum::<u64>() as usize) % palette.len()];
                        div()
                            .size(px(20.))
                            .flex_none()
                            .rounded_full()
                            .bg(hue.opacity(0.25))
                            .text_color(hue)
                            .text_xs()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(login.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default())
                            .into_any_element()
                    }
                };
                let frame = |accent: Hsla| {
                    v_flex()
                        .mx_3()
                        .my_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(accent.opacity(0.4))
                        .bg(c.card)
                        .overflow_hidden()
                        .font_family(cx.theme().font_family.clone())
                };
                match note {
                    Note::Thread(thread) => {
                        let root = thread[0].clone();
                        let outdated = root.line.is_none();
                        let this = this.clone();
                        frame(c.blue)
                            .children(thread.iter().map(|comment| {
                                v_flex()
                                    .px_3()
                                    .py_2()
                                    .gap_1()
                                    .border_b_1()
                                    .border_color(c.border)
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .text_xs()
                                            .child(avatar(&comment.author))
                                            .child(
                                                div().font_weight(FontWeight::SEMIBOLD).child(comment.author.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_color(c.muted)
                                                    .child(github::ago(&comment.at, chrono::Utc::now())),
                                            )
                                            .when(outdated, |el| el.child(pill("Outdated", c.yellow))),
                                    )
                                    .child(
                                        div().text_sm().child(
                                            TextView::markdown(
                                                SharedString::from(format!("lc-{}", comment.id)),
                                                comment.body.clone(),
                                            )
                                            .selectable(true),
                                        ),
                                    )
                            }))
                            .child(
                                h_flex().px_2().py_1().child(
                                    Button::new(SharedString::from(format!("reply-{}", root.id)))
                                        .xsmall()
                                        .ghost()
                                        .icon(IconName::Reply)
                                        .label("Reply")
                                        .on_click(move |_, window, cx| {
                                            let root = root.clone();
                                            let _ = this.update(cx, |v, cx| {
                                                let line = root.line.unwrap_or(1);
                                                v.open_compose(
                                                    root.path.clone(),
                                                    root.side.clone(),
                                                    line,
                                                    line,
                                                    Some(root.id),
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                ),
                            )
                            .into_any_element()
                    }
                    Note::Draft(ix, draft) => {
                        let (ix, this) = (*ix, this.clone());
                        crate::line_comment::pending_card(
                            ix,
                            crate::line_comment::lines_label(draft.start_line, draft.line),
                            draft.body.clone(),
                            move |_, _, cx| {
                                let _ = this.update(cx, |v, cx| {
                                    if ix < v.drafts.len() {
                                        v.drafts.remove(ix);
                                    }
                                    cx.notify();
                                });
                            },
                            cx,
                        )
                    }
                    Note::Compose(input, reply) => {
                        let reply = *reply;
                        let (t1, t2, t3) = (this.clone(), this.clone(), this.clone());
                        let cancel =
                            Button::new("compose-cancel").xsmall().ghost().label("Cancel").on_click(move |_, _, cx| {
                                let _ = t1.update(cx, |v, cx| {
                                    v.compose = None;
                                    cx.notify();
                                });
                            });
                        let now = (!reply).then(|| {
                            Button::new("compose-now")
                                .xsmall()
                                .outline()
                                .label("Add single comment")
                                .disabled(busy)
                                .on_click(move |_, _, cx| {
                                    let _ = t2.update(cx, |v, cx| v.finish_compose(true, cx));
                                })
                        });
                        let add = Button::new("compose-add")
                            .xsmall()
                            .primary()
                            .label(if reply { "Reply" } else { "Add to review" })
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                let _ = t3.update(cx, |v, cx| v.finish_compose(false, cx));
                            });
                        crate::line_comment::compose_card(
                            input,
                            None,
                            std::iter::once(cancel).chain(now).chain(std::iter::once(add)),
                            cx,
                        )
                    }
                    Note::Review => div().into_any_element(),
                }
            });

        // Each frame: the first canvas notes the diff's frame and forgets the headers, each
        // header painted notes where, and the last canvas works out which file is at the top.
        let (headers, frame_bounds) = (self.headers_seen.clone(), self.diff_bounds.clone());
        let before = canvas(
            move |bounds, _, _| {
                frame_bounds.set(bounds);
                headers.borrow_mut().clear();
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let (headers, frame_bounds, view) = (self.headers_seen.clone(), self.diff_bounds.clone(), cx.entity());
        let (was_current, was_sticky, pinned) = (self.current_file, self.sticky_file, self.file_pinned);
        let unpin = cx.entity();
        let input_bounds = self.diff_bounds.clone();
        let after = canvas(
            move |_, _, cx| {
                let frame = frame_bounds.get();
                let seen = headers.borrow();
                // A header within a header's height of the top is the file at the top; else the
                // file the top is inside: the one before the first header in view, or, with no
                // header in view, the same one as before.
                let at_top = seen
                    .iter()
                    .filter(|(_, at)| *at >= frame.top() - px(2.) && *at <= frame.top() + px(36.))
                    .map(|(ix, _)| *ix)
                    .max();
                let inside = seen
                    .iter()
                    .filter(|(_, at)| *at < frame.top() - px(2.))
                    .map(|(ix, _)| *ix)
                    .max()
                    .or_else(|| seen.iter().map(|(ix, _)| *ix).min().map(|ix| ix.saturating_sub(1)))
                    .or(was_sticky)
                    .unwrap_or(was_current);
                let sticky = if at_top.is_some() { None } else { Some(inside) };
                let current = if pinned { was_current } else { at_top.unwrap_or(inside) };
                if current != was_current || sticky != was_sticky {
                    cx.defer(move |cx| {
                        view.update(cx, |v, cx| {
                            v.current_file = current;
                            v.sticky_file = sticky;
                            cx.notify();
                        })
                    });
                }
            },
            // Scrolling or clicking in the diff ends a pick by Previous/Next file.
            move |_, _, window, _| {
                if !pinned {
                    return;
                }
                let (wheel, press) = (unpin.clone(), unpin.clone());
                let (b1, b2) = (input_bounds.clone(), input_bounds.clone());
                window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture && b1.get().contains(&event.position) {
                        wheel.update(cx, |v, cx| {
                            v.file_pinned = false;
                            cx.notify();
                        });
                    }
                });
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture && b2.get().contains(&event.position) {
                        press.update(cx, |v, cx| {
                            v.file_pinned = false;
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let sticky = self.sticky_file.and_then(|ix| {
            let file = self.diff.read(cx).files().get(ix)?;
            let path = file.path().clone();
            let name: SharedString = match (file.original_path(), file.modified_path()) {
                (Some(old), Some(new)) if old != new => format!("{old} → {new}").into(),
                _ => path.clone(),
            };
            let status = match file.status() {
                DiffFileStatus::Added => Some("Added file"),
                DiffFileStatus::Deleted => Some("Deleted file"),
                DiffFileStatus::Renamed => Some("Renamed file"),
                DiffFileStatus::Copied => Some("Copied file"),
                _ => None,
            };
            let (adds, dels) = (file.additions(), file.deletions());
            let (collapse_path, header_path, expand_path) = (path.clone(), path.clone(), path);
            Some(
                h_flex()
                    .id("github-sticky-file")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .pl_1()
                    .pr_3()
                    .py_1()
                    .gap_1()
                    .bg(c.bg)
                    .border_b_1()
                    .border_color(c.border)
                    .shadow_sm()
                    .text_sm()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = header_path.clone();
                        this.diff.update(cx, |d, cx| d.scroll_to_file(&path, cx));
                    }))
                    .child(
                        Button::new("github-sticky-collapse")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronDown)
                            .tooltip("Collapse this file")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let path = collapse_path.clone();
                                this.diff.update(cx, |d, cx| {
                                    d.set_file_collapsed(&path, true, cx);
                                    d.scroll_to_file(&path, cx);
                                });
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::MEDIUM)
                            .child(name),
                    )
                    .child(div().text_xs().text_color(c.muted).child(format!("+{adds} −{dels}")))
                    .children(status.map(|s| div().text_xs().text_color(c.muted).child(s)))
                    .child(
                        Button::new("github-sticky-expand")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Maximize2)
                            .tooltip("Open with its summary")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.expand_file(expand_path.to_string(), cx);
                            })),
                    ),
            )
        });
        let current = self.current_file.min(file_count.saturating_sub(1));
        let nav_button = |id: &'static str, icon: IconName, tip: &'static str, disabled: bool| {
            Button::new(id).ghost().small().icon(icon).tooltip(tip).rounded(cx.theme().radius_full()).disabled(disabled)
        };
        let nav = (file_count > 0).then(|| {
            div().absolute().left_0().right_0().bottom_4().flex().justify_center().child(
                h_flex()
                    .gap_0p5()
                    .p_1()
                    .rounded_full()
                    .border_1()
                    .border_color(c.border)
                    .bg(c.bg)
                    .shadow_md()
                    .child(
                        nav_button("github-files-top", IconName::ArrowUpToLine, "Back to the top", false)
                            .on_click(cx.listener(|this, _, _, cx| this.scroll_to_file_ix(0, cx))),
                    )
                    .child(
                        nav_button("github-files-previous", IconName::ChevronUp, "Previous file", current == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.step_file(-1, cx))),
                    )
                    .child(
                        div()
                            .px_2()
                            .text_xs()
                            .text_color(c.muted)
                            .child(format!("File {} of {file_count}", current + 1)),
                    )
                    .child(
                        nav_button("github-files-next", IconName::ChevronDown, "Next file", current + 1 >= file_count)
                            .on_click(cx.listener(|this, _, _, cx| this.step_file(1, cx))),
                    )
                    .child(
                        nav_button(
                            "github-files-bottom",
                            IconName::ArrowDownToLine,
                            if open { "Down to the review" } else { "Down to the bottom" },
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_review(cx))),
                    ),
            )
        });
        if detail.patch.trim().is_empty() {
            return v_flex()
                .size_full()
                .child(div().p_5().text_sm().text_color(c.muted).child("No changes to show."))
                .when(open, |el| el.child(review_card(cx)))
                .into_any_element();
        }
        div()
            .relative()
            .size_full()
            // Keys that scroll the diff end a pick by Previous/Next file too.
            .capture_key_down(cx.listener(|this, _, _, cx| {
                if this.file_pinned {
                    this.file_pinned = false;
                    cx.notify();
                }
            }))
            .child(before)
            .child(div().size_full().child(diff))
            .child(after)
            .children(sticky)
            .children(nav)
            .into_any_element()
    }

    fn render_commits(&self, detail: &PrDetail, c: &Colors) -> impl IntoElement {
        let now = chrono::Utc::now();
        let url = detail.pr.url.clone();
        div().id("github-commits").size_full().overflow_y_scroll().child(v_flex().max_w(px(920.)).px_5().py_4().child(
            v_flex().rounded_lg().border_1().border_color(c.border).overflow_hidden().children(
                detail.commits.iter().enumerate().map(|(ix, commit)| {
                    let link = format!("{url}/commits/{}", commit.sha);
                    h_flex()
                        .id(SharedString::from(format!("github-commit-{ix}")))
                        .gap_3()
                        .px_4()
                        .py_2p5()
                        .when(ix > 0, |el| el.border_t_1().border_color(c.border))
                        .cursor_pointer()
                        .hover(|el| el.bg(c.card))
                        .on_click(move |_, _, cx| cx.open_url(&link))
                        .child(Icon::new(IconName::GitCommitHorizontal).small().text_color(c.muted))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(commit.headline.clone()))
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(c.muted)
                                        .child(self.avatar(&commit.author, 14., c))
                                        .child(commit.author.clone())
                                        .child(format!("committed {}", github::ago(&commit.at, now))),
                                ),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(c.card)
                                .text_xs()
                                .font_family(c.mono.clone())
                                .text_color(c.muted)
                                .child(commit.sha.chars().take(7).collect::<String>()),
                        )
                }),
            ),
        ))
    }
}

impl GitHubView {
    /// The Actions page's list: workflow runs, newest first.
    fn render_runs(&self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let now = chrono::Utc::now();
        let filter = self.run_filter;
        let segments = RunFilter::ALL.map(|f| {
            let on = f == filter;
            div()
                .id(SharedString::from(format!("github-run-filter-{}", f.label())))
                .test_support()
                .flex_1()
                .py_1()
                .rounded_md()
                .text_center()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .map(|el| {
                    if on {
                        el.bg(c.bg).text_color(c.fg).shadow_sm()
                    } else {
                        el.text_color(c.muted).hover(|el| el.text_color(c.fg))
                    }
                })
                .child(f.label())
                .on_click(cx.listener(move |this, _, _, cx| this.set_run_filter(f, cx)))
        });
        let rows = self.runs.iter().map(|run| {
            let selected = self.run_selected == Some(run.id);
            let id = run.id;
            let end = if run.state.active() { "" } else { run.updated_at.as_str() };
            div()
                .id(SharedString::from(format!("github-run-{id}")))
                .test_support()
                .relative()
                .mx_2()
                .mb_1()
                .px_3()
                .py_2p5()
                .rounded_lg()
                .cursor_pointer()
                .map(|el| if selected { el.bg(c.card) } else { el.hover(|el| el.bg(c.card.opacity(0.5))) })
                .when(selected, |el| {
                    el.child(
                        div().absolute().left_0().top(px(10.)).bottom(px(10.)).w(px(3.)).rounded_full().bg(c.accent),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.list_page = false;
                    this.select_run(id, cx)
                }))
                .child(
                    h_flex().gap_2p5().items_start().child(div().pt_0p5().child(run_icon(run.state, false, c))).child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div().text_sm().font_weight(FontWeight::MEDIUM).line_clamp(2).child(run.title.clone()),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(c.muted)
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(format!("{} #{}", run.workflow, run.number)),
                                    )
                                    .child("·")
                                    .child(github::title_case(&run.event))
                                    .child(div().flex_1())
                                    .child(github::ago(&run.started_at, now)),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(c.muted)
                                    .when(!run.branch.is_empty(), |el| el.child(branch_chip(&run.branch, c)))
                                    .child(div().flex_1())
                                    .child(
                                        h_flex()
                                            .gap_0p5()
                                            .child(Icon::new(IconName::Timer).xsmall())
                                            .child(github::duration(&run.started_at, end, now)),
                                    ),
                            ),
                    ),
                )
                .into_any_element()
        });
        let empty = self.runs.is_empty();
        v_flex()
            .map(|el| if self.narrow() { el.flex_1().min_w_0() } else { el.w(px(LIST_WIDTH)).flex_none().border_r_1() })
            .h_full()
            .border_color(c.border)
            .child(h_flex().m_2().p_0p5().gap_0p5().rounded_lg().bg(c.card).children(segments))
            .child(
                div()
                    .id("github-run-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.runs_scroll)
                    .pb_2()
                    .map(|el| {
                        if !empty {
                            return el.children(rows);
                        }
                        el.child(
                            v_flex()
                                .items_center()
                                .gap_2()
                                .pt_10()
                                .text_sm()
                                .text_color(c.muted)
                                .child(if self.runs_loading {
                                    Spinner::new().small().color(c.accent).into_any_element()
                                } else {
                                    Icon::new(IconName::Workflow)
                                        .large()
                                        .text_color(c.muted.opacity(0.5))
                                        .into_any_element()
                                })
                                .child(match (self.runs_loading, filter) {
                                    (true, _) => "Loading workflow runs…",
                                    (false, RunFilter::All) => "No workflow runs yet",
                                    (false, RunFilter::Running) => "Nothing running",
                                    (false, RunFilter::Failed) => "No failed runs",
                                }),
                        )
                    }),
            )
    }

    /// The picked workflow run: what started it, its jobs and their steps, and logs.
    fn render_run(&self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_to_list(cx);
        let Some(run) = self.run_selected.and_then(|id| self.runs.iter().find(|r| r.id == id)) else {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .when_some(back, |el, back| el.child(h_flex().px_3().pt_3().child(back)))
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_sm()
                        .text_color(c.muted)
                        .child(Icon::new(IconName::Workflow).large().text_color(c.muted.opacity(0.5)))
                        .when(!self.runs_loading, |el| el.child("Pick a workflow run")),
                )
                .into_any_element();
        };
        let now = chrono::Utc::now();
        let (tone, _) = run_look(run.state, c);
        let busy = self.busy.is_some();
        let can_push = self.info.can_push;
        let no_push = "This account can't run workflows on this repository";
        let url = run.url.clone();
        let end = if run.state.active() { "" } else { run.updated_at.as_str() };
        let action = |id: &'static str, icon: IconName, label: &'static str| {
            Button::new(id)
                .small()
                .outline()
                .icon(icon)
                .label(label)
                .disabled(busy || !can_push)
                .when(!can_push, |b| b.tooltip(no_push))
        };
        let header = v_flex()
            .px_5()
            .pt_4()
            .pb_3()
            .gap_2()
            .border_b_1()
            .border_color(c.border)
            .when_some(back, |el, back| el.child(h_flex().ml(px(-10.)).child(back)))
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(run.title.clone())
                            .child(
                                div()
                                    .text_color(c.muted)
                                    .font_weight(FontWeight::NORMAL)
                                    .child(format!(" {} #{}", run.workflow, run.number)),
                            ),
                    )
                    .when(run.state.active(), |el| {
                        el.child(
                            action("github-run-cancel", IconName::CircleStop, "Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_run(cx))),
                        )
                    })
                    .when(matches!(run.state, RunState::Failed | RunState::Cancelled), |el| {
                        el.child(
                            action("github-run-rerun-failed", IconName::RotateCcw, "Re-run failed jobs")
                                .on_click(cx.listener(|this, _, _, cx| this.rerun(true, cx))),
                        )
                    })
                    .when(!run.state.active(), |el| {
                        el.child(
                            action("github-run-rerun", IconName::RefreshCw, "Re-run all jobs")
                                .on_click(cx.listener(|this, _, _, cx| this.rerun(false, cx))),
                        )
                    })
                    .child(
                        Button::new("github-run-web")
                            .small()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .tooltip("Open on GitHub")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .text_sm()
                    .text_color(c.muted)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded_full()
                            .bg(tone)
                            .text_color(c.bg)
                            .font_weight(FontWeight::MEDIUM)
                            .child(run_look(run.state, c).1.small())
                            .child(run.state.label()),
                    )
                    .child(github::title_case(&run.event))
                    .when(!run.branch.is_empty(), |el| el.child("on").child(branch_chip(&run.branch, c)))
                    .child("by")
                    .child(self.avatar(&run.actor, 18., c))
                    .child(div().text_color(c.fg).font_weight(FontWeight::MEDIUM).child(run.actor.clone()))
                    .child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .bg(c.card)
                            .text_xs()
                            .font_family(c.mono.clone())
                            .child(run.sha.chars().take(7).collect::<String>()),
                    )
                    .child(format!("· started {}", github::ago(&run.started_at, now)))
                    .child(format!(
                        "· {} {}",
                        if run.state.active() { "running for" } else { "took" },
                        github::duration(&run.started_at, end, now)
                    ))
                    .when(run.attempt > 1, |el| el.child(pill(&format!("Attempt {}", run.attempt), c.muted))),
            );

        let jobs = self.jobs.as_ref().filter(|(id, _)| *id == run.id).map(|(_, jobs)| jobs);
        let body = match jobs {
            None => v_flex().pt_10().items_center().child(Spinner::new().color(c.accent)).into_any_element(),
            Some(jobs) if jobs.is_empty() => {
                div().p_5().text_sm().text_color(c.muted).child("This run has no jobs yet.").into_any_element()
            }
            Some(jobs) => v_flex()
                .max_w(px(920.))
                .px_5()
                .py_4()
                .gap_3()
                .children(jobs.iter().map(|job| self.render_job(job, now, c, cx)))
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(div().id("github-run-jobs").flex_1().min_h_0().overflow_y_scroll().child(body))
            .into_any_element()
    }

    fn render_job(
        &self,
        job: &Job,
        now: chrono::DateTime<chrono::Utc>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = job.id;
        let open = self.logs_open.contains(&id);
        let log = self.logs.get(&id);
        let url = job.url.clone();
        let (tone, _) = run_look(job.state, c);
        let steps = job.steps.iter().enumerate().map(|(ix, step)| {
            let skipped = step.state == RunState::Skipped;
            h_flex()
                .gap_2()
                .px_4()
                .py_1p5()
                .text_sm()
                .border_t_1()
                .border_color(c.border)
                .when(skipped, |el| el.text_color(c.muted))
                .child(run_icon(step.state, true, c))
                .child(div().w(px(22.)).text_xs().text_color(c.muted).child((ix + 1).to_string()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(step.name.clone()),
                )
                .when(!skipped && !step.started_at.is_empty(), |el| {
                    el.child(div().text_xs().text_color(c.muted).child(github::duration(
                        &step.started_at,
                        &step.completed_at,
                        now,
                    )))
                })
        });
        let log_view = open.then(|| {
            let lines = log.cloned().flatten();
            v_flex().border_t_1().border_color(c.border).bg(c.bg).map(|el| match lines {
                None => el.p_3().items_center().child(Spinner::new().small().color(c.accent)),
                Some(lines) => {
                    let text = lines.join("\n");
                    el.child(
                        h_flex()
                            .px_4()
                            .py_1()
                            .gap_2()
                            .text_xs()
                            .text_color(c.muted)
                            .child(div().flex_1().child(if lines.iter().any(|l| l.starts_with("Error: ")) {
                                "The step that failed, up to its error"
                            } else {
                                "The end of the log"
                            }))
                            .child(
                                Button::new(SharedString::from(format!("github-log-copy-{id}")))
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .label("Copy")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("github-log-{id}")))
                            .max_h(px(420.))
                            .overflow_y_scroll()
                            .px_4()
                            .pb_3()
                            .font_family(c.mono.clone())
                            .text_xs()
                            .children(lines.into_iter().map(|line| {
                                let color = if line.starts_with("Error: ") {
                                    c.red
                                } else if line.starts_with("Warning: ") {
                                    c.yellow
                                } else if line.starts_with("▸ ") {
                                    c.accent
                                } else if line.starts_with("$ ") {
                                    c.muted
                                } else {
                                    c.fg
                                };
                                div().text_color(color).child(if line.is_empty() { " ".to_string() } else { line })
                            })),
                    )
                }
            })
        });
        v_flex()
            .rounded_lg()
            .border_1()
            .border_color(if job.state == RunState::Failed { tone.opacity(0.55) } else { c.border })
            .overflow_hidden()
            .child(
                h_flex()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .bg(c.card)
                    .child(run_icon(job.state, false, c))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(job.name.clone()),
                    )
                    .when(!job.started_at.is_empty(), |el| {
                        el.child(div().text_xs().text_color(c.muted).child(github::duration(
                            &job.started_at,
                            &job.completed_at,
                            now,
                        )))
                    })
                    .child(
                        Button::new(SharedString::from(format!("github-job-log-{id}")))
                            .xsmall()
                            .map(|b| if open { b.ghost() } else { b.outline() })
                            .icon(IconName::SquareTerminal)
                            .label(if open { "Hide log" } else { "Log" })
                            .disabled(job.state.active() || job.state == RunState::Skipped)
                            .when(job.state.active(), |b| b.tooltip("The log is ready when the job finishes"))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_log(id, cx))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("github-job-web-{id}")))
                            .xsmall()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .tooltip("Open on GitHub")
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .children(steps)
            .children(log_view)
            .into_any_element()
    }
}

impl Focusable for GitHubView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for GitHubView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.phase == Phase::Idle {
            self.load(cx);
        }
        for input in std::mem::take(&mut self.to_clear) {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        // Loading found the Other repos page stands in for the tab: it takes the keyboard.
        if self.showing_tracked(cx) && self.focus.is_focused(window) {
            self.tracked.update(cx, |t, cx| t.shown(window, cx));
        }
        let c = Self::colors(cx);
        let body = match self.phase.clone() {
            Phase::Idle | Phase::Loading => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .text_sm()
                .text_color(c.muted)
                .child(Spinner::new().large().color(c.accent))
                .child("Finding the repository and its GitHub account…")
                .into_any_element(),
            Phase::NoGh => self.render_message(
                IconName::Github,
                "The GitHub CLI isn't installed",
                "TJ talks to GitHub through gh. Install it (pacman -S github-cli), log in to each account with `gh auth login`, then try again.",
                None,
                &c,
                cx,
            ),
            Phase::NotGitHub if self.tracked_only(cx) => self.tracked.clone().into_any_element(),
            Phase::NotGitHub => {
                let other = Button::new("github-other-repos")
                    .small()
                    .outline()
                    .icon(IconName::FolderGit2)
                    .label("Pull requests on other repositories")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.tracked_wanted = true;
                        this.tracked.update(cx, |t, cx| t.shown(window, cx));
                        cx.notify();
                    }));
                self.render_message(
                    IconName::GitBranch,
                    "No GitHub remote",
                    "This project has no remote on GitHub. Add one (git remote add origin …) and try again, or keep track of pull requests it sends to other repositories.",
                    Some(other),
                    &c,
                    cx,
                )
            }
            Phase::NoAccounts => self.render_message(
                IconName::Users,
                "No GitHub accounts",
                "gh isn't logged in to any account. Run `gh auth login` in a terminal for each of your accounts, then try again.",
                None,
                &c,
                cx,
            ),
            Phase::Failed(err) => {
                self.render_message(IconName::CircleAlert, "Couldn't reach GitHub", &err, None, &c, cx)
            }
            Phase::Ready if self.page == Page::Tracked => v_flex()
                .size_full()
                .child(self.render_header(&c, cx))
                .child(div().flex_1().min_h_0().child(self.tracked.clone()))
                .into_any_element(),
            Phase::Ready => {
                let (list, detail) = match self.page {
                    Page::PullRequests => (
                        self.render_list(&c, cx).into_any_element(),
                        (!self.narrow() || !self.list_page).then(|| self.render_detail(window, &c, cx)),
                    ),
                    Page::Actions | Page::Tracked => (
                        self.render_runs(&c, cx).into_any_element(),
                        (!self.narrow() || !self.list_page).then(|| self.render_run(&c, cx)),
                    ),
                };
                v_flex()
                    .size_full()
                    .child(self.render_header(&c, cx))
                    .child(h_flex().flex_1().min_h_0().items_start().map(|el| {
                        if !self.narrow() {
                            el.child(list).children(detail)
                        } else if self.list_page || !self.has_selection() {
                            el.child(list)
                        } else {
                            el.children(detail)
                        }
                    }))
                    .into_any_element()
            }
        };
        let this = cx.entity();
        let width = self.width;
        let measure = canvas(
            move |bounds, _, cx| {
                if width != Some(bounds.size.width) {
                    cx.defer(move |cx| {
                        this.update(cx, |v, cx| {
                            v.width = Some(bounds.size.width);
                            cx.notify();
                        })
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .id("github")
            .key_context(CONTEXT)
            .relative()
            .track_focus(&self.focus)
            .size_full()
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .overflow_hidden()
            .bg(c.bg)
            .text_color(c.fg)
            .on_action(cx.listener(|this, _: &NextPullRequest, _, cx| this.select_step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousPullRequest, _, cx| this.select_step(-1, cx)))
            .on_action(cx.listener(|this, _: &OpenPullRequest, _, cx| {
                if this.has_selection() {
                    this.list_page = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &BackToList, _, cx| {
                if this.narrow() {
                    this.list_page = true;
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ShowConversation, _, cx| this.show_section(Section::Conversation, cx)))
            .on_action(cx.listener(|this, _: &ShowFiles, _, cx| this.show_section(Section::Files, cx)))
            .on_action(cx.listener(|this, _: &ShowCommits, _, cx| this.show_section(Section::Commits, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                if this.showing_tracked(cx) {
                    this.tracked.update(cx, |t, cx| t.refresh_all(cx))
                } else if this.phase == Phase::Ready {
                    this.refresh(true, cx)
                } else {
                    this.load(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Pull, _, cx| this.pull(cx)))
            .child(body)
            .child(measure)
    }
}

struct Colors {
    fg: Hsla,
    muted: Hsla,
    border: Hsla,
    card: Hsla,
    bg: Hsla,
    accent: Hsla,
    green: Hsla,
    red: Hsla,
    yellow: Hsla,
    purple: Hsla,
    blue: Hsla,
    palette: [Hsla; 11],
    mono: SharedString,
}

fn side_name(side: DiffSide) -> String {
    match side {
        DiffSide::Original => "LEFT".into(),
        _ => "RIGHT".into(),
    }
}

fn side_of(name: &str) -> DiffSide {
    if name == "LEFT" { DiffSide::Original } else { DiffSide::Modified }
}

fn state_look(state: PrState, c: &Colors) -> (IconName, Hsla) {
    match state {
        PrState::Open => (IconName::GitPullRequest, c.green),
        PrState::Draft => (IconName::GitPullRequestDraft, c.muted),
        PrState::Merged => (IconName::GitMerge, c.purple),
        PrState::Closed => (IconName::GitPullRequestClosed, c.red),
    }
}

fn pill(text: &str, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.45))
        .text_color(color)
        .text_xs()
        .child(text.to_string())
}

fn review_badge(icon: IconName, text: &'static str, color: Hsla) -> AnyElement {
    h_flex().gap_1().text_xs().text_color(color).child(Icon::new(icon).xsmall()).child(text).into_any_element()
}

fn branch_chip(name: &str, c: &Colors) -> Div {
    div()
        .px_1p5()
        .rounded_md()
        .bg(c.blue.opacity(0.14))
        .text_color(c.blue)
        .text_xs()
        .font_family(c.mono.clone())
        .max_w(px(200.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(name.to_string())
}

fn review_pill(decision: &str, c: &Colors) -> Option<Div> {
    match decision {
        "APPROVED" => Some(pill("Approved", c.green)),
        "CHANGES_REQUESTED" => Some(pill("Changes", c.red)),
        _ => None,
    }
}

fn checks_icon(state: ChecksState, c: &Colors) -> Option<AnyElement> {
    Some(match state {
        ChecksState::None => return None,
        ChecksState::Passing => Icon::new(IconName::CircleCheck).xsmall().text_color(c.green).into_any_element(),
        ChecksState::Failing => Icon::new(IconName::CircleX).xsmall().text_color(c.red).into_any_element(),
        ChecksState::Pending => Icon::new(IconName::CircleDot).xsmall().text_color(c.yellow).into_any_element(),
    })
}

/// A run's, job's or step's colour and icon.
fn run_look(state: RunState, c: &Colors) -> (Hsla, Icon) {
    let (color, icon) = match state {
        RunState::Queued => (c.yellow, IconName::CircleDashed),
        RunState::Running => (c.yellow, IconName::LoaderCircle),
        RunState::Passed => (c.green, IconName::CircleCheck),
        RunState::Failed => (c.red, IconName::CircleX),
        RunState::Cancelled => (c.muted, IconName::CircleStop),
        RunState::Skipped => (c.muted, IconName::CircleMinus),
    };
    (color, Icon::new(icon))
}

/// The icon for where a run, job or step stands; a spinner while it runs.
fn run_icon(state: RunState, xsmall: bool, c: &Colors) -> AnyElement {
    let (color, icon) = run_look(state, c);
    if state == RunState::Running {
        let spinner = Spinner::new().color(color);
        return if xsmall { spinner.xsmall() } else { spinner.small() }.into_any_element();
    }
    let icon = icon.text_color(color);
    if xsmall { icon.xsmall() } else { icon.small() }.into_any_element()
}

/// The id `step` places from `at` among `ids` (the first when nothing is picked), held at the ends.
fn step_in(ids: impl Iterator<Item = u64>, at: Option<u64>, step: isize) -> Option<u64> {
    let ids: Vec<u64> = ids.collect();
    let last = ids.len().checked_sub(1)?;
    let next = match at.and_then(|at| ids.iter().position(|id| *id == at)) {
        Some(ix) => (ix as isize + step).clamp(0, last as isize) as usize,
        None => 0,
    };
    Some(ids[next])
}

/// How far the branch is from its upstream: `↓ 3  ↑ 1`, or a check when level.
fn sync_counts(ahead: u64, behind: u64, c: &Colors) -> Div {
    let level = ahead == 0 && behind == 0;
    h_flex().gap_2().px_2().py_0p5().rounded_full().bg(c.card).text_xs().map(|el| {
        if level {
            el.text_color(c.green).child(Icon::new(IconName::Check).xsmall()).child("Up to date")
        } else {
            el.when(behind > 0, |el| el.child(div().text_color(c.yellow).child(format!("↓ {behind}"))))
                .when(ahead > 0, |el| el.child(div().text_color(c.accent).child(format!("↑ {ahead}"))))
        }
    })
}

/// GitHub's five little blocks: the share of additions to deletions.
fn diff_blocks(additions: u64, deletions: u64, c: &Colors) -> Div {
    let total = additions + deletions;
    let green = if total == 0 { 0 } else { ((additions as f64 / total as f64) * 5.0).round() as usize };
    h_flex().gap(px(2.)).ml_1().children((0..5).map(move |i| {
        let color = if total == 0 {
            c.muted.opacity(0.3)
        } else if i < green {
            c.green
        } else {
            c.red
        };
        div().size(px(8.)).rounded_sm().bg(color)
    }))
}

/// An account's picture, by login. Blocking.
fn fetch_avatar(login: &str) -> Option<Arc<Image>> {
    if !login.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '[' || ch == ']') {
        return None;
    }
    let out = std::process::Command::new("curl")
        .args(["-sfL", "--max-time", "10", &format!("https://avatars.githubusercontent.com/{login}?s=96")])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success() && !o.stdout.is_empty())?;
    let format = match image::guess_format(&out.stdout).ok()? {
        image::ImageFormat::Jpeg => ImageFormat::Jpeg,
        image::ImageFormat::Gif => ImageFormat::Gif,
        image::ImageFormat::WebP => ImageFormat::Webp,
        _ => ImageFormat::Png,
    };
    Some(Arc::new(Image::from_bytes(format, out.stdout)))
}
