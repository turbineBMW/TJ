//! What "Fix in thread" and "Address in thread" put in the agent's prompt box: a failed CI job
//! with its log, or review feedback on a pull request, written so the agent can act on it alone.

use crate::github::{Job, LineComment, PullRequest, RunState, TimelineItem, WorkflowRun};
use crate::terminal::fenced;

/// Most log lines a chip carries; the end of the log is the part that says what went wrong.
const LOG_LINES: usize = 300;
/// Most diff lines quoted above a line comment.
const HUNK_LINES: usize = 12;

/// The chip's label: a prefix and a name cut to fit.
fn label(prefix: &str, name: &str) -> String {
    match name.char_indices().nth(40) {
        Some((cut, _)) => format!("{prefix} · {}…", &name[..cut]),
        None => format!("{prefix} · {name}"),
    }
}

/// A line saying the checkout isn't on the branch the work is for.
fn branch_note(local: Option<&str>, wanted: &str) -> Option<String> {
    let local = local?;
    (!wanted.is_empty() && local != wanted)
        .then(|| format!("This checkout is on `{local}`, not `{wanted}`. Ask me before switching branches."))
}

/// `text` as a Markdown quote.
fn quoted(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "> (no text)".into();
    }
    text.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
}

/// A failed job, with the log of the step that failed (`log`, as the Actions page shows it).
pub fn failed_job(
    slug: &str,
    run: &WorkflowRun,
    job: &Job,
    log: &[String],
    local_branch: Option<&str>,
) -> (String, String) {
    let mut out = vec!["A CI job failed on GitHub. Find out why and fix it.".to_string(), String::new()];
    out.push(format!("- Repository: {slug}"));
    let attempt = if run.attempt > 1 { format!(", attempt {}", run.attempt) } else { String::new() };
    let title = if run.title.is_empty() { String::new() } else { format!(": {}", run.title) };
    out.push(format!("- Workflow: {}, run #{}{attempt}{title}", run.workflow, run.number));
    let failed: Vec<String> = job
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.state == RunState::Failed)
        .map(|(ix, s)| format!("{} \"{}\"", ix + 1, s.name))
        .collect();
    match failed.as_slice() {
        [] => out.push(format!("- Job: {}", job.name)),
        [one] => out.push(format!("- Job: {}, failed at step {one}", job.name)),
        many => out.push(format!("- Job: {}, failed at steps {}", job.name, many.join(", "))),
    }
    if !run.branch.is_empty() {
        let sha: String = run.sha.chars().take(7).collect();
        out.push(format!("- Branch: `{}`, commit {sha}", run.branch));
    }
    if !job.url.is_empty() {
        out.push(format!("- Job page: {}", job.url));
    }
    if let Some(note) = branch_note(local_branch, &run.branch) {
        out.extend([String::new(), note]);
    }
    out.push(String::new());
    let skip = log.len().saturating_sub(LOG_LINES);
    out.push(
        if log.iter().any(|l| l.starts_with("Error: ")) {
            "The log of the step that failed, up to its error:"
        } else {
            "The end of the job's log:"
        }
        .to_string(),
    );
    let mut text = log[skip..].join("\n");
    if skip > 0 {
        text = format!("[{skip} earlier lines left out]\n{text}");
    }
    out.push(fenced(&text, ""));
    (label("CI", &format!("{} failed", job.name)), out.join("\n"))
}

/// Where a line comment is, and the diff lines above it.
fn comment_place(comment: &LineComment) -> String {
    let at = match comment.line {
        Some(line) => format!("`{}` line {line}", comment.path),
        None => format!("`{}` (outdated: the code has changed since the comment)", comment.path),
    };
    let lines: Vec<&str> = comment.diff_hunk.lines().collect();
    if lines.is_empty() {
        return at;
    }
    let hunk = lines[lines.len().saturating_sub(HUNK_LINES)..].join("\n");
    format!("{at}:\n{}", fenced(&hunk, "diff"))
}

fn thread_text(thread: &[LineComment]) -> String {
    let mut out = vec![comment_place(&thread[0])];
    for comment in thread {
        out.push(String::new());
        out.push(format!("{} wrote:", comment.author));
        out.push(quoted(&comment.body));
    }
    out.join("\n")
}

fn pr_intro(pr: &PullRequest) -> String {
    format!("pull request #{}, \"{}\" (branch `{}`)", pr.number, pr.title, pr.head)
}

/// One line comment's thread on a pull request.
pub fn review_thread(pr: &PullRequest, thread: &[LineComment], local_branch: Option<&str>) -> (String, String) {
    let first = &thread[0];
    let mut out = vec![format!("Address this review comment on {}.", pr_intro(pr)), String::new(), thread_text(thread)];
    if let Some(note) = branch_note(local_branch, &pr.head) {
        out.extend([String::new(), note]);
    }
    let file = first.path.rsplit('/').next().unwrap_or(&first.path);
    let place = match first.line {
        Some(line) => format!("{file}:{line}"),
        None => file.to_string(),
    };
    (label("Review", &place), out.join("\n"))
}

fn verdict(item: &TimelineItem) -> &'static str {
    match item.review.as_deref() {
        Some("CHANGES_REQUESTED") => "requested changes",
        Some("APPROVED") => "approved",
        Some(_) => "reviewed",
        None => "commented",
    }
}

/// A review or comment from the pull request's conversation.
pub fn review(pr: &PullRequest, item: &TimelineItem, local_branch: Option<&str>) -> (String, String) {
    let mut out = vec![
        format!("Address this feedback on {}.", pr_intro(pr)),
        String::new(),
        format!("{} {}:", item.author, verdict(item)),
        quoted(&item.body),
    ];
    if let Some(note) = branch_note(local_branch, &pr.head) {
        out.extend([String::new(), note]);
    }
    (label("Review", &item.author), out.join("\n"))
}

/// All the feedback on a pull request from people other than `me`: reviews and comments with
/// text, then the line comment threads. `None` when there's none.
pub fn all_feedback(
    pr: &PullRequest,
    timeline: &[TimelineItem],
    threads: &[Vec<LineComment>],
    me: Option<&str>,
    local_branch: Option<&str>,
) -> Option<(String, String)> {
    let others = |author: &str| Some(author) != me;
    let items: Vec<&TimelineItem> =
        timeline.iter().filter(|i| others(&i.author) && !i.body.trim().is_empty()).collect();
    let threads: Vec<&Vec<LineComment>> =
        threads.iter().filter(|t| t.first().is_some_and(|c| others(&c.author))).collect();
    let count = items.len() + threads.len();
    if count == 0 {
        return None;
    }
    let mut out = vec![format!(
        "Address the review feedback on {}. Skip anything already done, and tell me which.",
        pr_intro(pr)
    )];
    for item in items {
        out.extend([String::new(), format!("{} {}:", item.author, verdict(item)), quoted(&item.body)]);
    }
    for (ix, thread) in threads.iter().enumerate() {
        out.extend([String::new(), format!("## Line comment {}", ix + 1), String::new(), thread_text(thread)]);
    }
    if let Some(note) = branch_note(local_branch, &pr.head) {
        out.extend([String::new(), note]);
    }
    let what = if count == 1 { "1 comment".to_string() } else { format!("{count} comments") };
    Some((label("Review", &format!("#{} · {what}", pr.number)), out.join("\n")))
}

/// Line comments grouped into threads (the first comment and its replies), in order.
pub fn threads(comments: &[LineComment]) -> Vec<Vec<LineComment>> {
    let mut threads: Vec<(u64, Vec<LineComment>)> = Vec::new();
    for comment in comments {
        match comment.reply_to.and_then(|to| threads.iter_mut().find(|(root, _)| *root == to)) {
            Some((_, thread)) => thread.push(comment.clone()),
            None => threads.push((comment.id, vec![comment.clone()])),
        }
    }
    threads.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{PrState, Step};

    fn pr() -> PullRequest {
        PullRequest {
            number: 12,
            title: "Add a cache".into(),
            author: "me".into(),
            head: "cache".into(),
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
        }
    }

    fn comment(id: u64, author: &str, body: &str, reply_to: Option<u64>) -> LineComment {
        LineComment {
            id,
            path: "src/cache.rs".into(),
            line: Some(42),
            side: "RIGHT".into(),
            author: author.into(),
            body: body.into(),
            at: String::new(),
            reply_to,
            diff_hunk: "@@ -40,2 +40,3 @@\n fn get() {\n+    unwrap()".into(),
        }
    }

    fn run() -> WorkflowRun {
        WorkflowRun {
            id: 1,
            number: 126,
            attempt: 2,
            workflow: "CI".into(),
            title: "Add a cache".into(),
            event: "push".into(),
            branch: "cache".into(),
            sha: "abcdef1234".into(),
            actor: "me".into(),
            state: RunState::Failed,
            started_at: String::new(),
            updated_at: String::new(),
            url: String::new(),
        }
    }

    fn job() -> Job {
        let step = |name: &str, state| Step {
            name: name.into(),
            state,
            started_at: String::new(),
            completed_at: String::new(),
        };
        Job {
            id: 9,
            name: "test".into(),
            state: RunState::Failed,
            started_at: String::new(),
            completed_at: String::new(),
            url: "https://github.com/o/r/actions/runs/1/job/9".into(),
            steps: vec![step("Checkout", RunState::Passed), step("cargo test", RunState::Failed)],
        }
    }

    #[test]
    fn a_failed_job_names_the_run_step_and_branch_and_fences_its_log() {
        let log = vec!["$ cargo test".to_string(), "Error: 1 test failed".to_string()];
        let (label, text) = failed_job("o/r", &run(), &job(), &log, Some("main"));
        assert_eq!(label, "CI · test failed");
        assert!(text.contains("- Repository: o/r"), "{text}");
        assert!(text.contains("- Workflow: CI, run #126, attempt 2: Add a cache"), "{text}");
        assert!(text.contains("- Job: test, failed at step 2 \"cargo test\""), "{text}");
        assert!(text.contains("- Branch: `cache`, commit abcdef1"), "{text}");
        assert!(text.contains("This checkout is on `main`, not `cache`."), "{text}");
        assert!(text.contains("up to its error:\n```\n$ cargo test\nError: 1 test failed\n```"), "{text}");
    }

    #[test]
    fn a_long_log_keeps_its_end() {
        let log: Vec<String> = (0..LOG_LINES + 5).map(|i| format!("line {i}")).collect();
        let (_, text) = failed_job("o/r", &run(), &job(), &log, Some("cache"));
        assert!(text.contains("[5 earlier lines left out]\nline 5\n"), "{text}");
        assert!(!text.contains("not `cache`"), "on the run's branch there's no warning");
    }

    #[test]
    fn a_review_thread_quotes_its_lines_and_every_reply() {
        let thread =
            vec![comment(1, "ana", "Don't unwrap here.", None), comment(2, "me", "Why not?\nIt's fine.", Some(1))];
        let (label, text) = review_thread(&pr(), &thread, Some("cache"));
        assert_eq!(label, "Review · cache.rs:42");
        assert!(text.starts_with("Address this review comment on pull request #12, \"Add a cache\" (branch `cache`)."));
        assert!(text.contains("`src/cache.rs` line 42:\n```diff\n@@ -40,2 +40,3 @@"), "{text}");
        assert!(text.contains("ana wrote:\n> Don't unwrap here."), "{text}");
        assert!(text.contains("me wrote:\n> Why not?\n> It's fine."), "{text}");
    }

    #[test]
    fn all_feedback_leaves_out_my_own_and_counts_the_rest() {
        let timeline = vec![
            TimelineItem {
                author: "ana".into(),
                body: "A few things.".into(),
                at: String::new(),
                review: Some("CHANGES_REQUESTED".into()),
            },
            TimelineItem { author: "me".into(), body: "Thanks!".into(), at: String::new(), review: None },
            TimelineItem {
                author: "bo".into(),
                body: String::new(),
                at: String::new(),
                review: Some("APPROVED".into()),
            },
        ];
        let comments = vec![
            comment(1, "ana", "Don't unwrap here.", None),
            comment(2, "me", "Done.", Some(1)),
            comment(3, "me", "Note to self", None),
        ];
        let (label, text) = all_feedback(&pr(), &timeline, &threads(&comments), Some("me"), None).unwrap();
        assert_eq!(label, "Review · #12 · 2 comments");
        assert!(text.contains("ana requested changes:\n> A few things."), "{text}");
        assert!(!text.contains("Thanks!") && !text.contains("Note to self"), "{text}");
        assert!(text.contains("## Line comment 1") && text.contains("me wrote:\n> Done."), "replies stay: {text}");
        assert!(!text.contains("## Line comment 2"), "{text}");
        assert!(all_feedback(&pr(), &timeline[1..2], &[], Some("me"), None).is_none());
    }

    #[test]
    fn a_comment_with_backticks_gets_a_longer_fence() {
        let mut c = comment(1, "ana", "x", None);
        c.diff_hunk = "+let s = \"```\";".into();
        assert!(comment_place(&c).contains("````diff\n+let s = \"```\";\n````"));
    }
}
