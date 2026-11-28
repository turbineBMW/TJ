//! `tj notes-mcp`: a small MCP server, over stdio, that lets Claude read and change a project's
//! notes ([`crate::notes`]) and the pull requests it keeps track of ([`crate::tracked`]). TJ gives it to every Claude session it starts, with the project's
//! notes file and the thread the session belongs to, so a note Claude adds knows where it was
//! written.
//!
//! It speaks just enough MCP for tools: `initialize`, `tools/list`, `tools/call` and `ping`, as
//! JSON-RPC messages one per line.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::notes::{self, Author, Kind, NewNote, NoteStatus, Source};
use crate::tracked::{self, PrStatus, TrackedPr};

/// What the server works on, from its command line.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub file: PathBuf,
    pub project_dir: PathBuf,
    pub project_name: String,
    pub thread: Option<String>,
    pub transcript: Option<PathBuf>,
}

impl Context {
    /// Read `--file`, `--project-dir`, `--project-name`, `--thread` and `--transcript`.
    pub fn from_args(args: &[String]) -> anyhow::Result<Self> {
        let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let file = value("--file").ok_or_else(|| anyhow::anyhow!("tj notes-mcp needs --file <notes file>"))?;
        Ok(Self {
            file: file.into(),
            project_dir: value("--project-dir").map(PathBuf::from).unwrap_or_default(),
            project_name: value("--project-name").unwrap_or_default(),
            thread: value("--thread"),
            transcript: value("--transcript").map(PathBuf::from),
        })
    }

    /// The command line that starts the server for this context.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "notes-mcp".to_string(),
            "--file".into(),
            self.file.display().to_string(),
            "--project-dir".into(),
            self.project_dir.display().to_string(),
            "--project-name".into(),
            self.project_name.clone(),
        ];
        if let Some(thread) = &self.thread {
            args.extend(["--thread".into(), thread.clone()]);
        }
        if let Some(transcript) = &self.transcript {
            args.extend(["--transcript".into(), transcript.display().to_string()]);
        }
        args
    }
}

/// Serve requests on stdin until it closes.
pub fn run(context: &Context) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message, context),
            Err(err) => Some(error(Value::Null, -32700, &format!("Parse error: {err}"))),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Answer one message; `None` for notifications.
pub fn handle(message: &Value, context: &Context) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18");
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "tj", "version": env!("CARGO_PKG_VERSION")},
                "instructions": notes::INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(name, &args, context) {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(err) => json!({"content": [{"type": "text", "text": format!("{err:#}")}], "isError": true}),
            }
        }
        _ => return Some(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tools() -> Value {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
    json!([
        {
            "name": "notes_list",
            "description": "List this project's notes: number, status, kind and title, with the first line of each body. By default only notes still to be done (open or in progress).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["pending", "open", "doing", "done", "all"], "description": "Which notes to list (default: pending)"}
                }
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "note_read",
            "description": "Read one note in full: its body, where it was written and how it was resolved.",
            "inputSchema": {
                "type": "object",
                "properties": {"number": {"type": "integer", "description": "The note's number"}},
                "required": ["number"]
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "note_add",
            "description": "Add a note for something put off for later, so it isn't lost when the session ends. The title is one short line; the body says what to do, why, and where, so a fresh session can pick it up without this conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string", "description": "Markdown"},
                    "kind": {"type": "string", "enum": kinds, "description": "Default: deferred"}
                },
                "required": ["title"]
            }
        },
        {
            "name": "note_update",
            "description": "Change a note. Set status to \"done\" with a resolution when the work is finished, \"doing\" while working on it, or \"open\" to reopen it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "number": {"type": "integer"},
                    "title": {"type": "string"},
                    "body": {"type": "string", "description": "Replaces the body (Markdown)"},
                    "kind": {"type": "string", "enum": kinds},
                    "status": {"type": "string", "enum": ["open", "doing", "done"]},
                    "resolution": {"type": "string", "description": "How it was resolved, e.g. what changed and the PR"}
                },
                "required": ["number"]
            }
        },
        {
            "name": "note_delete",
            "description": "Delete a note for good. Prefer closing it (note_update with status done); delete only duplicates or notes that were wrong.",
            "inputSchema": {
                "type": "object",
                "properties": {"number": {"type": "integer"}},
                "required": ["number"]
            },
            "annotations": {"destructiveHint": true}
        },
        {
            "name": "pr_track",
            "description": "Keep track of a pull request, merge request or mailed patch series this project sent out, on any repository or host (GitHub, GitLab, a mailing list). It shows on the GitHub tab's Other repos page, where TJ keeps its state fresh. Call it right after you open one. Tracking the same address again only updates its note and folder.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Its web address: the PR or MR page, or the patch's lore.kernel.org or patchwork link"},
                    "title": {"type": "string", "description": "For links TJ can't read (a mailed patch): its subject. GitHub and GitLab ones get theirs from the host"},
                    "note": {"type": "string", "description": "One line: what it's for, or what it's waiting on"},
                    "folder": {"type": "string", "description": "The local checkout its branch is in (absolute path)"},
                    "status": {"type": "string", "enum": ["open", "draft", "merged", "closed"], "description": "For links TJ can't read (mailing lists, other hosts). Default: open"}
                },
                "required": ["url"]
            }
        },
        {
            "name": "pr_list",
            "description": "List the pull requests this project is keeping track of, with their state, checks, review and note. By default only the ones still open (open or draft).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["active", "open", "draft", "merged", "closed", "all"], "description": "Which to list (default: active)"},
                    "refresh": {"type": "boolean", "description": "Read the latest state from GitHub and GitLab first (slower)"}
                }
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "pr_update",
            "description": "Change a tracked pull request: its note, its folder, or (for links TJ can't read, like a mailed patch) its status and title. GitHub and GitLab ones get their status from the host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "status": {"type": "string", "enum": ["open", "draft", "merged", "closed"]},
                    "title": {"type": "string"},
                    "note": {"type": "string", "description": "Replaces the note; empty clears it"},
                    "folder": {"type": "string"}
                },
                "required": ["url"]
            }
        },
        {
            "name": "pr_untrack",
            "description": "Stop keeping track of a pull request. Merged and closed ones can stay; untrack only ones tracked by mistake or no longer wanted.",
            "inputSchema": {
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            },
            "annotations": {"destructiveHint": true}
        }
    ])
}

fn string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number(args: &Value) -> anyhow::Result<u64> {
    let value = args.get("number").ok_or_else(|| anyhow::anyhow!("Give the note's number"))?;
    // Accept "3" and "#3" as well as 3.
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().trim_start_matches('#').parse().ok()))
        .ok_or_else(|| anyhow::anyhow!("The note's number must be a whole number"))
}

fn kind(args: &Value) -> anyhow::Result<Option<Kind>> {
    match string(args, "kind") {
        None => Ok(None),
        Some(name) => Kind::parse(&name)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Unknown kind {name:?}: use deferred, bug, idea or question")),
    }
}

fn call(name: &str, args: &Value, cx: &Context) -> anyhow::Result<String> {
    let edit = |change: &mut dyn FnMut(&mut notes::Notebook) -> anyhow::Result<String>| {
        notes::update(&cx.file, &cx.project_name, &cx.project_dir, change)?
    };
    match name {
        "notes_list" => {
            let which = string(args, "status").unwrap_or_else(|| "pending".into());
            let book = notes::load(&cx.file).unwrap_or_default();
            let keep = |status: NoteStatus| match which.as_str() {
                "all" => true,
                "pending" => status.is_pending(),
                other => NoteStatus::parse(other) == Some(status),
            };
            let lines: Vec<String> = book
                .notes
                .iter()
                .filter(|n| keep(n.status))
                .map(|n| {
                    let first = n.body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                    let first = if first.chars().count() > 120 {
                        format!(" — {}…", first.chars().take(120).collect::<String>())
                    } else if first.is_empty() {
                        String::new()
                    } else {
                        format!(" — {first}")
                    };
                    format!("#{} [{}] ({}) {}{first}", n.number, n.status.name(), n.kind.name(), n.title)
                })
                .collect();
            Ok(if lines.is_empty() { format!("No {which} notes.") } else { lines.join("\n") })
        }
        "note_read" => {
            let number = number(args)?;
            let book = notes::load(&cx.file).unwrap_or_default();
            let note = book.get(number).ok_or_else(|| anyhow::anyhow!("There's no note #{number}"))?;
            let mut out = format!(
                "#{} {}\nKind: {}\nStatus: {}\n",
                note.number,
                note.title,
                note.kind.name(),
                note.status.name()
            );
            if let Some(source) = &note.source {
                let at: Vec<String> = [source.branch.as_ref().map(|b| format!("branch {b}")), source.commit.clone()]
                    .into_iter()
                    .flatten()
                    .collect();
                if !at.is_empty() {
                    out.push_str(&format!("Written at: {}\n", at.join(", ")));
                }
            }
            if let Some(resolution) = &note.resolution {
                out.push_str(&format!("Resolution: {resolution}\n"));
            }
            if !note.body.is_empty() {
                out.push_str(&format!("\n{}\n", note.body));
            }
            Ok(out)
        }
        "note_add" => {
            let title = string(args, "title").filter(|t| !t.trim().is_empty());
            let title = title.ok_or_else(|| anyhow::anyhow!("A note needs a title"))?;
            let kind = kind(args)?.unwrap_or_default();
            let (branch, commit) = notes::git_state(&cx.project_dir);
            let prompt = cx.transcript.as_deref().and_then(notes::prompts_in);
            let source = Source { thread: cx.thread.clone(), prompt, branch, commit };
            let new = NewNote {
                title,
                body: string(args, "body").unwrap_or_default(),
                kind,
                author: Author::Agent,
                source: Some(source),
            };
            edit(&mut |book| {
                let number = book.add(new.clone());
                Ok(format!("Added note #{number}. The user sees it in the project's Notes tab (Ctrl+Shift+N)."))
            })
        }
        "note_update" => {
            let number = number(args)?;
            let kind = kind(args)?;
            let status = match string(args, "status") {
                None => None,
                Some(name) => Some(
                    NoteStatus::parse(&name)
                        .ok_or_else(|| anyhow::anyhow!("Unknown status {name:?}: use open, doing or done"))?,
                ),
            };
            let (title, body, resolution) = (string(args, "title"), string(args, "body"), string(args, "resolution"));
            edit(&mut |book| {
                let note = book.get_mut(number).ok_or_else(|| anyhow::anyhow!("There's no note #{number}"))?;
                if let Some(title) = title.clone().filter(|t| !t.trim().is_empty()) {
                    note.title = title.trim().to_string();
                }
                if let Some(body) = &body {
                    note.body = body.trim().to_string();
                }
                if let Some(kind) = kind {
                    note.kind = kind;
                }
                if let Some(status) = status {
                    note.status = status;
                }
                if let Some(resolution) = &resolution {
                    note.resolution = Some(resolution.trim().to_string()).filter(|r| !r.is_empty());
                }
                note.updated = crate::store::now();
                Ok(format!("Updated note #{number} ({}).", note.status.name()))
            })
        }
        "note_delete" => {
            let number = number(args)?;
            edit(&mut |book| {
                book.remove(number).ok_or_else(|| anyhow::anyhow!("There's no note #{number}"))?;
                Ok(format!("Deleted note #{number}."))
            })
        }
        "pr_track" => {
            let url = string(args, "url").unwrap_or_default();
            let mut pr = TrackedPr::new(&url, Author::Agent)
                .ok_or_else(|| anyhow::anyhow!("{url:?} isn't a web address (https://...)"))?;
            let folder = string(args, "folder").filter(|f| !f.trim().is_empty()).map(PathBuf::from);
            let (branch, commit) = notes::git_state(folder.as_deref().unwrap_or(&cx.project_dir));
            let prompt = cx.transcript.as_deref().and_then(notes::prompts_in);
            pr.source = Some(Source { thread: cx.thread.clone(), prompt, branch, commit });
            pr.folder = folder;
            pr.note = string(args, "note").unwrap_or_default().trim().to_string();
            pr.title = string(args, "title").unwrap_or_default().trim().to_string();
            if let Some(status) = status(args)? {
                pr.status = status;
            }
            // Read it now, so the answer says what it is (and a wrong address shows at once).
            let read = tracked::fetch(&pr);
            match &read {
                Ok(Some(fresh)) => pr.apply(fresh.clone()),
                Ok(None) => {}
                Err(err) => pr.failed(format!("{err:#}")),
            }
            let line = pr_line(&pr);
            let url = pr.url.clone();
            let new = notes::update(&cx.file, &cx.project_name, &cx.project_dir, |book| {
                let new = book.track(pr.clone());
                // Tracked before: it still takes what was just read.
                if !new && let (Some(old), Ok(Some(fresh))) = (book.tracked_mut(&url), &read) {
                    old.apply(fresh.clone());
                }
                new
            })?;
            let lead = if new { "Tracking" } else { "Already tracking" };
            let mut out =
                format!("{lead} {line}\nThe user sees it on the GitHub tab's Other repos page (Ctrl+Shift+G).");
            if let Err(err) = read {
                out.push_str(&format!("\nCouldn't read its state yet: {err:#}"));
            }
            Ok(out)
        }
        "pr_list" => {
            let which = string(args, "status").unwrap_or_else(|| "active".into());
            let keep = |status: PrStatus| match which.as_str() {
                "all" => true,
                "active" | "pending" => status.is_active(),
                other => PrStatus::parse(other) == Some(status),
            };
            if args.get("refresh").and_then(Value::as_bool).unwrap_or(false) {
                let book = notes::load(&cx.file).unwrap_or_default();
                let stale: Vec<TrackedPr> =
                    book.prs.into_iter().filter(|p| keep(p.status) && p.forge.refreshes()).collect();
                let results = tracked::fetch_all(stale);
                edit(&mut |book| {
                    for (url, result) in &results {
                        if let Some(pr) = book.tracked_mut(url) {
                            match result {
                                Ok(Some(fresh)) => pr.apply(fresh.clone()),
                                Ok(None) => {}
                                Err(err) => pr.failed(format!("{err:#}")),
                            }
                        }
                    }
                    Ok(String::new())
                })?;
            }
            let book = notes::load(&cx.file).unwrap_or_default();
            let lines: Vec<String> = book.prs.iter().filter(|p| keep(p.status)).map(pr_line).collect();
            Ok(if lines.is_empty() { format!("No {which} pull requests tracked.") } else { lines.join("\n") })
        }
        "pr_update" => {
            let url = string(args, "url").unwrap_or_default();
            let status = status(args)?;
            let (title, note, folder) = (string(args, "title"), string(args, "note"), string(args, "folder"));
            edit(&mut |book| {
                let pr = book.tracked_mut(&url).ok_or_else(|| anyhow::anyhow!("{url} isn't tracked"))?;
                if let Some(status) = status {
                    pr.status = status;
                }
                if let Some(title) = title.as_ref().filter(|t| !t.trim().is_empty()) {
                    pr.title = title.trim().to_string();
                }
                if let Some(note) = &note {
                    pr.note = note.trim().to_string();
                }
                if let Some(folder) = &folder {
                    pr.folder = Some(PathBuf::from(folder.trim())).filter(|f| !f.as_os_str().is_empty());
                }
                let mut out = format!("Updated {}", pr_line(pr));
                if status.is_some() && pr.forge.refreshes() {
                    out.push_str(&format!("\n{} sets its status itself; the next refresh replaces this one.", pr.host));
                }
                Ok(out)
            })
        }
        "pr_untrack" => {
            let url = string(args, "url").unwrap_or_default();
            edit(&mut |book| {
                let pr = book.untrack(&url).ok_or_else(|| anyhow::anyhow!("{url} isn't tracked"))?;
                Ok(format!("Stopped tracking {}.", pr.reference()))
            })
        }
        _ => anyhow::bail!("Unknown tool {name}"),
    }
}

fn status(args: &Value) -> anyhow::Result<Option<PrStatus>> {
    match string(args, "status") {
        None => Ok(None),
        Some(name) => PrStatus::parse(&name)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Unknown status {name:?}: use open, draft, merged or closed")),
    }
}

/// One tracked PR in a line: `[open] o/r#12 Fix it (checks passing, approved) — note — url`.
fn pr_line(pr: &TrackedPr) -> String {
    let mut line = format!("[{}] {}", pr.status.name(), pr.reference());
    if !pr.title.is_empty() {
        line.push_str(&format!(" {}", pr.title));
    }
    let facts: Vec<&str> = [
        (pr.checks != tracked::Checks::None).then(|| pr.checks.label()),
        Some(pr.review.label()).filter(|r| !r.is_empty()),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Checks and review only matter while it's open.
    if !facts.is_empty() && pr.status.is_active() {
        line.push_str(&format!(" ({})", facts.join(", ").to_lowercase()));
    }
    if !pr.note.is_empty() {
        line.push_str(&format!(" — {}", pr.note));
    }
    line.push_str(&format!(" — {}", pr.url));
    if let Some(error) = &pr.error {
        line.push_str(&format!(" (last read failed: {error})"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(name: &str) -> Context {
        let dir = std::env::temp_dir().join(format!("tj-notes-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t1.jsonl");
        std::fs::write(&transcript, "{\"kind\":\"prompt\",\"text\":\"hi\"}\n").unwrap();
        Context {
            file: dir.join("notes").join("p1.json"),
            project_dir: dir.clone(),
            project_name: "proj".into(),
            thread: Some("t1".into()),
            transcript: Some(transcript),
        }
    }

    fn call_tool(cx: &Context, name: &str, args: Value) -> (String, bool) {
        let reply = handle(
            &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}),
            cx,
        )
        .unwrap();
        let result = &reply["result"];
        (result["content"][0]["text"].as_str().unwrap().to_string(), result["isError"].as_bool().unwrap_or(false))
    }

    #[test]
    fn initialize_echoes_the_version_and_offers_tools() {
        let cx = context("init");
        let reply = handle(&json!({"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}), &cx);
        let reply = reply.unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        assert!(handle(&json!({"method": "notifications/initialized"}), &cx).is_none(), "notifications get no reply");
        let tools = handle(&json!({"id": 2, "method": "tools/list"}), &cx).unwrap();
        let names: Vec<&str> =
            tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "notes_list",
                "note_read",
                "note_add",
                "note_update",
                "note_delete",
                "pr_track",
                "pr_list",
                "pr_update",
                "pr_untrack"
            ]
        );
        let unknown = handle(&json!({"id": 3, "method": "resources/list"}), &cx).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn notes_can_be_added_listed_closed_and_deleted() {
        let cx = context("crud");
        let (text, failed) =
            call_tool(&cx, "note_add", json!({"title": "Prune list", "body": "Later.", "kind": "idea"}));
        assert!(!failed && text.starts_with("Added note #1"), "{text}");
        let book = notes::load(&cx.file).unwrap();
        let note = book.get(1).unwrap();
        assert_eq!(note.author, Author::Agent);
        let source = note.source.as_ref().unwrap();
        assert_eq!((source.thread.as_deref(), source.prompt), (Some("t1"), Some(1)));

        assert_eq!(call_tool(&cx, "notes_list", json!({})).0, "#1 [open] (idea) Prune list — Later.");
        let (text, _) = call_tool(&cx, "note_update", json!({"number": "#1", "status": "done", "resolution": "PR #9"}));
        assert_eq!(text, "Updated note #1 (done).");
        assert_eq!(call_tool(&cx, "notes_list", json!({})).0, "No pending notes.");
        assert!(call_tool(&cx, "note_read", json!({"number": 1})).0.contains("Resolution: PR #9"));

        assert_eq!(call_tool(&cx, "note_delete", json!({"number": 1})).0, "Deleted note #1.");
        let (text, failed) = call_tool(&cx, "note_read", json!({"number": 1}));
        assert!(failed && text.contains("no note #1"));
        let (_, failed) = call_tool(&cx, "note_add", json!({"title": "x", "kind": "chore"}));
        assert!(failed, "an unknown kind is refused");
    }

    #[test]
    fn prs_can_be_tracked_listed_updated_and_untracked() {
        let cx = context("prs");
        // A mailed patch: nothing to read, so nothing leaves the machine.
        let lore = "https://lore.kernel.org/linux-bluetooth/abc@x.org/";
        let (text, failed) = call_tool(
            &cx,
            "pr_track",
            json!({
                "url": lore,
                "title": "Bluetooth: quirk",
                "note": "SP11 battery quirk",
                "folder": cx.project_dir.display().to_string()
            }),
        );
        assert!(!failed && text.starts_with("Tracking [open] linux-bluetooth Bluetooth: quirk"), "{text}");
        let book = notes::load(&cx.file).unwrap();
        let pr = &book.prs[0];
        assert_eq!((pr.added_by, pr.source.as_ref().unwrap().thread.as_deref()), (Author::Agent, Some("t1")));
        assert_eq!(pr.folder.as_deref(), Some(cx.project_dir.as_path()));

        let (text, _) = call_tool(&cx, "pr_track", json!({"url": lore}));
        assert!(text.starts_with("Already tracking"), "{text}");
        assert_eq!(notes::load(&cx.file).unwrap().prs.len(), 1, "the same address is one PR");
        assert_eq!(notes::load(&cx.file).unwrap().prs[0].note, "SP11 battery quirk", "an empty note keeps the old");

        let listed = call_tool(&cx, "pr_list", json!({})).0;
        assert!(listed.contains("SP11 battery quirk") && listed.contains("lore.kernel.org"), "{listed}");
        let (text, _) = call_tool(&cx, "pr_update", json!({"url": lore, "status": "merged", "title": "Bluetooth fix"}));
        assert!(text.contains("[merged] linux-bluetooth Bluetooth fix"), "{text}");
        assert_eq!(call_tool(&cx, "pr_list", json!({})).0, "No active pull requests tracked.");
        assert!(call_tool(&cx, "pr_list", json!({"status": "merged"})).0.contains("Bluetooth fix"));

        let (_, failed) = call_tool(&cx, "pr_track", json!({"url": "not a link"}));
        assert!(failed);
        let (_, failed) = call_tool(&cx, "pr_update", json!({"url": lore, "status": "shipped"}));
        assert!(failed, "an unknown status is refused");
        assert_eq!(call_tool(&cx, "pr_untrack", json!({"url": lore})).0, "Stopped tracking linux-bluetooth.");
        assert!(call_tool(&cx, "pr_untrack", json!({"url": lore})).1);
    }

    #[test]
    fn args_round_trip() {
        let cx = context("args");
        let args = cx.to_args();
        assert_eq!(args[0], "notes-mcp");
        let back = Context::from_args(&args).unwrap();
        assert_eq!((back.file, back.thread, back.project_name), (cx.file, cx.thread, cx.project_name));
    }
}
