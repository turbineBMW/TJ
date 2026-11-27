//! Claude Code backend: runs the `claude` binary and speaks its stdio protocol.
//!
//! This is the protocol Anthropic's Agent SDKs use (stream-json in both directions,
//! plus `control_request` / `control_response` messages). It is not a documented
//! public API, so everything that depends on its shape lives in this file, unknown
//! messages are skipped rather than treated as errors, and recorded sessions in
//! `tests/fixtures/` pin the shapes this code was written against.

use std::{
    collections::{HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use serde_json::{Value, json};

use super::{
    AgentEvent, AgentSession, Decision, ELICITATION_TOOL, ElicitAction, PermissionMode, PermissionRequest,
    SlashCommand, Spend, ToolImage, UsageWindow,
};

/// The `claude` version the protocol handling was written and recorded against.
pub const TESTED_VERSION: &str = "2.1.331";

const STDERR_LINES_KEPT: usize = 40;

/// Added to Claude's system prompt in every TJ session: how TJ shows commands, so the agent
/// writes them in a form its Copy and Run in terminal buttons can take whole.
pub const TJ_INSTRUCTIONS: &str = "You are running inside TJ, a desktop app. \
In your replies, every fenced code block gets a Copy button, and a shell block (```bash, ```sh, ```zsh) \
also gets a Run in terminal button that puts its contents on a new terminal's command line for the user \
to check and run. So when you ask the user to run something in a terminal:
- Put exactly one command in each code block. Never put two commands in one block, not even a variable \
assignment followed by the command that uses it: give each its own block, in the order to run them.
- One command may span lines with trailing backslashes or a pipeline; that still counts as one.
- Mark the block as ```bash (or the shell it is for). Leave out prompts such as `$ ` and leave output out \
of the block.
- When the user has to fill in a value, say which one in the text before the block.";

/// How to start a session.
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// The binary to run; `claude` on `PATH` when `None`.
    pub binary: Option<PathBuf>,
    pub cwd: PathBuf,
    /// The account's config directory (`CLAUDE_CONFIG_DIR`); Claude's default when `None`.
    pub config_dir: Option<PathBuf>,
    pub model: Option<String>,
    /// `--effort` (low ... max); the model's default when `None`.
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    /// Continue an earlier conversation.
    pub resume: Option<String>,
    /// Also ask for an 80/20 summary at the end of finished work (`summary::INSTRUCTIONS`).
    pub eighty_twenty: bool,
    /// Give the session TJ's notes server (`tj notes-mcp`), run by this binary, and tell
    /// Claude what the notes are for (`notes::INSTRUCTIONS`).
    pub notes: Option<Box<NotesServer>>,
}

/// The notes server a session is given.
#[derive(Clone, Debug)]
pub struct NotesServer {
    /// The `tj` binary that runs it.
    pub binary: PathBuf,
    pub context: crate::notes_mcp::Context,
}

impl NotesServer {
    /// The `--mcp-config` JSON naming the server `tj`, so its tools are `mcp__tj__…`.
    pub fn mcp_config(&self) -> String {
        serde_json::json!({
            "mcpServers": {
                "tj": {
                    "type": "stdio",
                    "command": self.binary.display().to_string(),
                    "args": self.context.to_args(),
                }
            }
        })
        .to_string()
    }
}

impl SessionOptions {
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            // Permission prompts come to us as `can_use_tool` control requests.
            "--permission-prompt-tool=stdio",
        ]
        .map(String::from)
        .to_vec();
        let mut instructions = TJ_INSTRUCTIONS.to_string();
        if self.notes.is_some() {
            instructions = format!("{instructions}\n\n{}", crate::notes::INSTRUCTIONS);
        }
        if self.eighty_twenty {
            instructions = format!("{instructions}\n\n{}", crate::summary::INSTRUCTIONS);
        }
        args.extend(["--append-system-prompt".into(), instructions]);
        if let Some(notes) = &self.notes {
            // Its tools only touch TJ's own notes file, so they don't ask for approval.
            args.extend(["--mcp-config".into(), notes.mcp_config(), "--allowedTools".into(), "mcp__tj".into()]);
        }
        args.extend(["--permission-mode".into(), self.permission_mode.claude_name().into()]);
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &self.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(session) = &self.resume {
            args.extend(["--resume".into(), session.clone()]);
        }
        args
    }
}

/// A running `claude` process.
pub struct ClaudeSession {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    next_id: AtomicU64,
}

impl ClaudeSession {
    /// Start `claude` and return the session plus the channel its events arrive on.
    pub fn spawn(options: &SessionOptions) -> anyhow::Result<(Self, smol::channel::Receiver<AgentEvent>)> {
        let binary = options.binary.clone().unwrap_or_else(|| "claude".into());
        let mut command = Command::new(&binary);
        command
            .args(options.args())
            .current_dir(&options.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &options.config_dir {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        let mut child = command.spawn().with_context(|| format!("could not start {}", binary.display()))?;
        let stdin = child.stdin.take().context("claude stdin")?;
        let stdout = child.stdout.take().context("claude stdout")?;
        let stderr = child.stderr.take().context("claude stderr")?;

        let (tx, rx) = smol::channel::unbounded();
        let session =
            Self { stdin: Arc::new(Mutex::new(stdin)), child: Arc::new(Mutex::new(child)), next_id: AtomicU64::new(1) };

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = tail.lock().unwrap();
                    if tail.len() == STDERR_LINES_KEPT {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }
        {
            let child = session.child.clone();
            std::thread::spawn(move || {
                let mut parser = Parser::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    for event in parser.parse_line(&line) {
                        if tx.send_blocking(event).is_err() {
                            return;
                        }
                    }
                }
                // stdout closed: the process is ending.
                let code = child.lock().unwrap().wait().ok().and_then(|status| status.code());
                let stderr = stderr_tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
                let _ = tx.send_blocking(AgentEvent::Exited { code, stderr });
            });
        }

        session.control("initialize", json!({}))?;
        Ok((session, rx))
    }

    fn write(&self, message: &Value) -> anyhow::Result<()> {
        let mut stdin = self.stdin.lock().unwrap();
        serde_json::to_writer(&mut *stdin, message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    /// Send a control request. Its id starts with `tj-` and the `subtype` names it.
    fn control(&self, subtype: &str, mut fields: Value) -> anyhow::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        fields["subtype"] = json!(subtype);
        self.write(&json!({
            "type": "control_request",
            "request_id": format!("tj-{subtype}-{id}"),
            "request": fields,
        }))
    }
}

impl AgentSession for ClaudeSession {
    fn send_message(&self, text: &str, images: &[super::PromptImage]) -> anyhow::Result<()> {
        self.write(&json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": { "role": "user", "content": message_content(text, images) },
        }))
    }

    fn respond(&self, request: &PermissionRequest, decision: Decision) -> anyhow::Result<()> {
        self.write(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request.id,
                "response": permission_result(request, &decision),
            },
        }))
    }

    fn interrupt(&self) -> anyhow::Result<()> {
        self.control("interrupt", json!({}))
    }

    fn set_permission_mode(&self, mode: PermissionMode) -> anyhow::Result<()> {
        self.control("set_permission_mode", json!({ "mode": mode.claude_name() }))
    }

    fn set_model(&self, model: Option<&str>) -> anyhow::Result<()> {
        self.control("set_model", json!({ "model": model }))
    }

    /// Effort is a flag-tier setting; `null` returns to the model's default effort.
    fn set_effort(&self, effort: Option<&str>) -> anyhow::Result<()> {
        self.control("apply_flag_settings", json!({ "settings": { "effortLevel": effort } }))
    }

    fn request_usage(&self) -> anyhow::Result<()> {
        self.control("get_usage", json!({}))
    }
}

/// A user message's content blocks: images first (as the API recommends), then the text.
pub fn message_content(text: &str, images: &[super::PromptImage]) -> Value {
    use base64::Engine as _;
    let mut blocks: Vec<Value> = images
        .iter()
        .map(|image| {
            json!({ "type": "image", "source": {
                "type": "base64",
                "media_type": image.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(image.data.as_slice()),
            }})
        })
        .collect();
    if !text.is_empty() || blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    Value::Array(blocks)
}

/// The models `claude` offers this account, read from the `initialize` reply of a session
/// that is stopped straight after (no prompt is sent, so it costs nothing). Blocking.
pub fn list_models(config_dir: Option<&std::path::Path>) -> anyhow::Result<Vec<super::ModelChoice>> {
    let reply = initialize_reply(config_dir, &std::env::temp_dir())?;
    let models = parse_models(&reply["models"]);
    anyhow::ensure!(!models.is_empty(), "claude listed no models");
    Ok(models)
}

/// The slash commands a session in `cwd` accepts on this account (its project's own commands
/// and skills too), the same way as [`list_models`]. Blocking.
pub fn list_commands(config_dir: Option<&std::path::Path>, cwd: &std::path::Path) -> anyhow::Result<Vec<SlashCommand>> {
    let reply = initialize_reply(config_dir, cwd)?;
    let commands = parse_commands(&reply["commands"]);
    anyhow::ensure!(!commands.is_empty(), "claude listed no commands");
    Ok(commands)
}

/// The `initialize` reply of a session in `cwd` that is stopped straight after.
fn initialize_reply(config_dir: Option<&std::path::Path>, cwd: &std::path::Path) -> anyhow::Result<Value> {
    let mut command = Command::new("claude");
    command
        .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
        // Only the lists are wanted: the user's SessionStart hooks needn't run for it.
        .args(["--settings", r#"{"disableAllHooks":true}"#])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = command.spawn().context("could not start claude")?;
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let stdout = child.stdout.take().context("claude stdout")?;
    writeln!(
        stdin,
        "{}",
        json!({ "type": "control_request", "request_id": "tj-models", "request": { "subtype": "initialize" } })
    )?;
    stdin.flush()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["response"]["request_id"] == "tj-models"
            {
                let _ = tx.send(message);
                return;
            }
        }
    });
    let reply = rx.recv_timeout(std::time::Duration::from_secs(20));
    let _ = child.kill();
    let _ = child.wait();
    let mut reply = reply.context("claude did not answer in time")?;
    Ok(reply["response"]["response"].take())
}

pub fn parse_commands(commands: &Value) -> Vec<SlashCommand> {
    let mut seen = std::collections::HashSet::new();
    commands
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?.trim_start_matches('/').to_string();
            Some(SlashCommand {
                description: text(c, "description").unwrap_or_default(),
                argument_hint: text(c, "argumentHint").unwrap_or_default(),
                name,
            })
        })
        // Claude Code's internal commands start with `__`; they aren't for people to type.
        .filter(|c| !c.name.is_empty() && !c.name.starts_with("__"))
        .filter(|c| seen.insert(c.name.to_lowercase()))
        .collect()
}

pub fn parse_models(models: &Value) -> Vec<super::ModelChoice> {
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(super::ModelChoice {
                value: m.get("value")?.as_str()?.to_string(),
                name: m.get("displayName").and_then(Value::as_str).unwrap_or_default().to_string(),
                description: m.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                resolved: m.get("resolvedModel").and_then(Value::as_str).map(str::to_owned),
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect(),
            })
        })
        .collect()
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

/// The `can_use_tool` answer for a decision.
pub fn permission_result(request: &PermissionRequest, decision: &Decision) -> Value {
    // An MCP server's request for input is answered in MCP's terms.
    if request.tool_name == ELICITATION_TOOL {
        let (action, content) = match decision {
            Decision::Elicit { action, content } => (*action, content.clone()),
            Decision::Approve | Decision::ApproveForSession => (ElicitAction::Accept, None),
            Decision::Decline { .. } => (ElicitAction::Decline, None),
            _ => (ElicitAction::Cancel, None),
        };
        let mut answer = json!({ "action": action.name() });
        if let (ElicitAction::Accept, Some(content)) = (action, content) {
            answer["content"] = content;
        }
        return answer;
    }
    match decision {
        Decision::Approve => json!({ "behavior": "allow", "updatedInput": request.input }),
        Decision::ApproveForSession => json!({
            "behavior": "allow",
            "updatedInput": request.input,
            "updatedPermissions": session_rules(request),
        }),
        Decision::Decline { feedback } => {
            let message = match feedback.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
                Some(feedback) => format!("The user declined this tool call and said:\n{feedback}"),
                None => "The user declined this tool call.".to_string(),
            };
            json!({ "behavior": "deny", "message": message })
        }
        Decision::Cancel => {
            json!({ "behavior": "deny", "message": "The user cancelled the turn.", "interrupt": true })
        }
        // Only an MCP server's request for input is answered this way; a tool call isn't run.
        Decision::Elicit { .. } => json!({ "behavior": "deny", "message": "The user declined this tool call." }),
        // The answers ride in the tool's own input, which is how Claude Code's dialog returns them.
        Decision::Answer { answers } => {
            let mut input = request.input.clone();
            if !input.is_object() {
                input = json!({});
            }
            input["answers"] = json!(answers);
            json!({ "behavior": "allow", "updatedInput": input })
        }
        Decision::StartPlan { accept_edits } => json!({
            "behavior": "allow",
            "updatedInput": request.input,
            "updatedPermissions": [{
                "type": "setMode",
                "mode": if *accept_edits { "acceptEdits" } else { "default" },
                "destination": "session",
            }],
        }),
    }
}

/// The rules an "approve for session" saves: the agent's own suggestions, kept to this
/// session. A suggestion to switch permission mode is left out so approving one call
/// never silently changes the mode the user picked.
fn session_rules(request: &PermissionRequest) -> Vec<Value> {
    let rules: Vec<Value> = request
        .suggestions
        .iter()
        .filter(|s| s.get("type").and_then(Value::as_str) != Some("setMode"))
        .cloned()
        .map(|mut s| {
            s["destination"] = json!("session");
            s
        })
        .collect();
    if rules.is_empty() {
        vec![json!({
            "type": "addRules",
            "rules": [{ "toolName": request.tool_name }],
            "behavior": "allow",
            "destination": "session",
        })]
    } else {
        rules
    }
}

/// Turns protocol lines into [`AgentEvent`]s. One parser reads one process.
#[derive(Default)]
pub struct Parser {
    /// The main agent's response being streamed: its model, its usage when it began, and what
    /// it has been priced at so far.
    response: Option<(String, Value, f64)>,
    /// Subagent responses already priced: each content block repeats the same usage.
    priced: HashSet<String>,
    /// Claude Code's running cost for this process at its last result.
    process_total: f64,
}

impl Parser {
    pub fn parse_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => self.parse(&message),
            Err(_) => Vec::new(),
        }
    }

    /// A subagent's response, priced the first time it's seen. Subagents aren't streamed, so its
    /// output is counted as it stood when the response began.
    fn price_subagent_response(&mut self, message: &Value) -> Option<AgentEvent> {
        let id = message.get("id").and_then(Value::as_str)?;
        if !self.priced.insert(id.to_string()) {
            return None;
        }
        let cost = super::pricing::cost(message.get("model").and_then(Value::as_str)?, &message["usage"])?;
        (cost > 0.0).then_some(AgentEvent::CostEstimate(cost))
    }

    pub fn parse(&mut self, m: &Value) -> Vec<AgentEvent> {
        let str_at = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        // Messages from subagents carry their parent's tool id; only the main thread is shown.
        let from_subagent = m.get("parent_tool_use_id").is_some_and(|p| !p.is_null());

        // A subagent's own messages carry the Agent tool call they belong to.
        if from_subagent {
            let parent = str_at(m, "parent_tool_use_id").unwrap_or_default();
            return match m.get("type").and_then(Value::as_str) {
                Some("assistant") => self
                    .price_subagent_response(&m["message"])
                    .into_iter()
                    .chain(blocks(m).filter_map(|b| {
                        match b["type"].as_str() {
                            Some("tool_use") => Some(AgentEvent::SubagentTool {
                                tool_use_id: parent.clone(),
                                name: str_at(b, "name")?,
                                input: b.get("input").cloned().unwrap_or(Value::Null),
                            }),
                            Some("text") => str_at(b, "text")
                                .filter(|t| !t.trim().is_empty())
                                .map(|text| AgentEvent::SubagentText { tool_use_id: parent.clone(), text }),
                            _ => None,
                        }
                    }))
                    .collect(),
                _ => Vec::new(),
            };
        }

        let subtype = str_at(m, "subtype");
        match m.get("type").and_then(Value::as_str) {
            Some("system") if subtype.as_deref() == Some("task_started") => match str_at(m, "tool_use_id") {
                Some(tool_use_id) => vec![AgentEvent::SubagentStarted {
                    tool_use_id,
                    description: str_at(m, "description").unwrap_or_default(),
                    agent_type: str_at(m, "subagent_type"),
                }],
                None => Vec::new(),
            },
            Some("system") if subtype.as_deref() == Some("task_progress") => match str_at(m, "tool_use_id") {
                Some(tool_use_id) => vec![AgentEvent::SubagentProgress {
                    tool_use_id,
                    description: str_at(m, "description").unwrap_or_default(),
                    tool_uses: m["usage"]["tool_uses"].as_u64(),
                    tokens: m["usage"]["total_tokens"].as_u64(),
                }],
                None => Vec::new(),
            },
            Some("system") if subtype.as_deref() == Some("task_notification") => match str_at(m, "tool_use_id") {
                Some(tool_use_id) => vec![AgentEvent::SubagentFinished {
                    tool_use_id,
                    status: str_at(m, "status").unwrap_or_else(|| "completed".into()),
                    summary: str_at(m, "summary"),
                    output_file: str_at(m, "output_file"),
                }],
                None => Vec::new(),
            },
            Some("system") if subtype.as_deref() == Some("compact_boundary") => {
                let meta = &m["compact_metadata"];
                vec![AgentEvent::Compacted {
                    pre_tokens: meta["pre_tokens"].as_u64(),
                    manual: meta["trigger"].as_str() == Some("manual"),
                }]
            }
            Some("conversation_reset") => vec![AgentEvent::ConversationReset],
            // A slash command Claude Code ran itself: its output, written as an assistant message.
            // Built-in commands (`/context`) name themselves in `local_command_run`; a skill run
            // as a local command (`/code-review`) only carries `local_command_source`.
            Some("assistant")
                if ["local_command_run", "local_command_source"]
                    .iter()
                    .any(|key| m.get(*key).is_some_and(|v| !v.is_null())) =>
            {
                let command = str_at(&m["local_command_run"], "command").unwrap_or_default();
                let text =
                    blocks(m).filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
                vec![AgentEvent::CommandOutput { command, text }]
            }
            Some("system") if subtype.as_deref() == Some("init") => match str_at(m, "session_id") {
                Some(session_id) => vec![AgentEvent::SessionInfo { session_id, model: str_at(m, "model") }],
                None => Vec::new(),
            },
            Some("stream_event") if !from_subagent => {
                let event = &m["event"];
                match event.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        let message = &event["message"];
                        let model = str_at(message, "model").unwrap_or_default();
                        let usage = message["usage"].clone();
                        let cost = super::pricing::cost(&model, &usage).unwrap_or(0.0);
                        self.response = Some((model, usage, cost));
                        estimate(cost)
                    }
                    // The response's final counts; what it added since it began is the difference.
                    Some("message_delta") => match (self.response.as_mut(), event["usage"].as_object()) {
                        (Some((model, usage, priced)), Some(delta)) => {
                            for (key, value) in delta.iter().filter(|(_, v)| v.is_number()) {
                                usage[key] = value.clone();
                            }
                            let cost = super::pricing::cost(model, usage).unwrap_or(0.0);
                            let added = cost - *priced;
                            *priced = cost;
                            estimate(added)
                        }
                        _ => Vec::new(),
                    },
                    Some("content_block_start") if event["content_block"]["type"] == "text" => {
                        vec![AgentEvent::TextStarted]
                    }
                    Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                        str_at(&event["delta"], "text").map(AgentEvent::TextDelta).into_iter().collect()
                    }
                    _ => Vec::new(),
                }
            }
            Some("assistant") if !from_subagent => {
                let mut events: Vec<AgentEvent> = blocks(m)
                    .filter(|b| b["type"] == "tool_use")
                    .filter_map(|b| {
                        Some(AgentEvent::ToolStarted {
                            id: str_at(b, "id")?,
                            name: str_at(b, "name")?,
                            input: b.get("input").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                // Messages Claude Code writes itself (errors) say "<synthetic>"; not a model.
                if let Some(model) = str_at(&m["message"], "model").filter(|m| !m.starts_with('<')) {
                    events.push(AgentEvent::ModelInUse(model));
                }
                let usage = &m["message"]["usage"];
                if usage.is_object() {
                    let tokens =
                        ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"]
                            .iter()
                            .filter_map(|k| usage.get(*k).and_then(Value::as_u64))
                            .sum::<u64>();
                    events.push(AgentEvent::Context { tokens: Some(tokens), window: None });
                }
                events
            }
            Some("user") if !from_subagent => blocks(m)
                .filter(|b| b["type"] == "tool_result")
                .filter_map(|b| {
                    Some(AgentEvent::ToolFinished {
                        id: str_at(b, "tool_use_id")?,
                        output: tool_output(b.get("content")),
                        images: tool_images(b.get("content")),
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        took_ms: None,
                    })
                })
                .collect(),
            Some("rate_limit_event") => {
                let windows = m["rate_limit_info"]["unifiedWindows"]
                    .as_object()
                    .map(|windows| {
                        windows
                            .iter()
                            .filter_map(|(id, w)| {
                                Some(UsageWindow {
                                    id: id.clone(),
                                    utilization: w.get("utilization")?.as_f64()?,
                                    resets_at: w.get("resetsAt").and_then(Value::as_i64),
                                    severity: match w.get("status").and_then(Value::as_str) {
                                        Some("allowed_warning") => Some("warning".into()),
                                        Some("rejected") => Some("critical".into()),
                                        _ => None,
                                    },
                                    ..Default::default()
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if windows.is_empty() { Vec::new() } else { vec![AgentEvent::Usage(windows)] }
            }
            Some("result") => {
                let is_error = m.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let window = m["modelUsage"]
                    .as_object()
                    .and_then(|models| models.values().filter_map(|u| u.get("contextWindow")?.as_u64()).max());
                let mut events = Vec::new();
                if window.is_some() {
                    events.push(AgentEvent::Context { tokens: None, window });
                }
                // A slash command Claude Code ran itself never reached a model: nothing to report.
                let local = m.get("local_command").is_some_and(|c| !c.is_null()) && !is_error;
                self.response = None;
                // `total_cost_usd` is what the whole process has cost so far, not this turn.
                let total = m.get("total_cost_usd").and_then(Value::as_f64);
                let since_last = m["model_usage_since_last_result"]
                    .as_object()
                    .map(|models| models.values().filter_map(|u| u.get("costUSD")?.as_f64()).sum::<f64>());
                let turn = since_last.or_else(|| total.map(|t| (t - self.process_total).max(0.0)));
                if let Some(total) = total {
                    self.process_total = total;
                }
                events.push(AgentEvent::TurnFinished {
                    cost_usd: turn.filter(|_| !local),
                    process_total_usd: None,
                    duration_ms: m.get("duration_ms").and_then(Value::as_u64).filter(|_| !local),
                    is_error,
                    error: is_error.then(|| str_at(m, "result").unwrap_or_else(|| "The turn failed.".into())),
                });
                events
            }
            Some("control_request") => {
                let request = &m["request"];
                match (str_at(request, "subtype").as_deref(), str_at(m, "request_id")) {
                    (Some("can_use_tool"), Some(id)) => vec![AgentEvent::PermissionRequested(PermissionRequest {
                        id,
                        tool_name: str_at(request, "tool_name").unwrap_or_else(|| "tool".into()),
                        tool_use_id: str_at(request, "tool_use_id"),
                        description: str_at(request, "description"),
                        input: request.get("input").cloned().unwrap_or(Value::Null),
                        suggestions: request["permission_suggestions"].as_array().cloned().unwrap_or_default(),
                        reason: str_at(request, "decision_reason").map(|r| strip_ansi(&r)),
                        no_standing_grant: request
                            .get("suppress_always_allow_rule")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })],
                    // An MCP server asking the user for input. Claude waits for the answer, so
                    // it's shown like an approval.
                    (Some("elicitation"), Some(id)) => vec![AgentEvent::PermissionRequested(PermissionRequest {
                        id,
                        tool_name: ELICITATION_TOOL.into(),
                        tool_use_id: None,
                        description: str_at(request, "message"),
                        input: request.clone(),
                        suggestions: Vec::new(),
                        reason: None,
                        no_standing_grant: true,
                    })],
                    _ => Vec::new(),
                }
            }
            Some("control_cancel_request") => {
                str_at(m, "request_id").map(|id| AgentEvent::PermissionWithdrawn { id }).into_iter().collect()
            }
            Some("control_response") => {
                let response = &m["response"];
                let request_id = str_at(response, "request_id").unwrap_or_default();
                if response["subtype"] == "error" {
                    let error = str_at(response, "error").unwrap_or_default();
                    return vec![AgentEvent::Error(format!("{request_id}: {error}"))];
                }
                let body = &response["response"];
                if request_id.starts_with("tj-initialize-") {
                    vec![AgentEvent::Started {
                        version: str_at(body, "claude_code_version"),
                        permission_mode: str_at(body, "current_permission_mode")
                            .and_then(|mode| PermissionMode::from_claude_name(&mode)),
                        commands: parse_commands(&body["commands"]),
                    }]
                } else if request_id.starts_with("tj-get_usage-") {
                    let mut events = Vec::new();
                    if let Some(plan) = str_at(body, "subscription_type") {
                        events.push(AgentEvent::Plan(plan));
                    }
                    let windows = usage_windows(&body["rate_limits"]);
                    if !windows.is_empty() {
                        events.push(AgentEvent::Usage(windows));
                    }
                    events
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

/// The account's limits from a `get_usage` reply's `rate_limits`: the server's meters
/// (`limits[]`) when it sends them, else the older named windows, plus extra-usage spend.
fn usage_windows(limits: &Value) -> Vec<UsageWindow> {
    let mut windows: Vec<UsageWindow> = match limits["limits"].as_array() {
        Some(rows) => rows
            .iter()
            .filter_map(|row| {
                let kind = row["kind"].as_str()?;
                let group = row["group"].as_str().unwrap_or_default();
                let scope = row["scope"]["model"]["display_name"]
                    .as_str()
                    .or_else(|| row["scope"]["surface"]["display_name"].as_str())
                    .map(str::to_owned);
                let id = match (kind, &scope) {
                    ("session", None) => "five_hour".to_string(),
                    ("weekly_all", None) => "seven_day".to_string(),
                    ("spend", _) => format!("spend_{group}"),
                    (_, Some(scope)) => format!("{kind}:{scope}"),
                    (_, None) => kind.to_string(),
                };
                Some(UsageWindow {
                    id,
                    utilization: row["percent"].as_f64()? / 100.0,
                    resets_at: row["resets_at"].as_str().and_then(unix_seconds),
                    severity: text(row, "severity"),
                    scope,
                    spend: None,
                })
            })
            .collect(),
        // A server that predates `limits[]`: its named windows, in percent.
        None => limits
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(id, _)| id.as_str() != "extra_usage")
            .filter_map(|(id, w)| {
                Some(UsageWindow {
                    id: id.clone(),
                    utilization: w.get("utilization")?.as_f64()? / 100.0,
                    resets_at: w.get("resets_at").and_then(Value::as_str).and_then(unix_seconds),
                    ..Default::default()
                })
            })
            .collect(),
    };
    // Extra usage (a subscription's overage, or a team or enterprise seat's spending limit) is
    // in minor units: cents for USD.
    let extra = &limits["extra_usage"];
    if extra["is_enabled"].as_bool() == Some(true) {
        let scale = 10f64.powi(extra["decimal_places"].as_i64().unwrap_or(2) as i32);
        let limit = extra["monthly_limit"].as_f64().map(|c| c / scale);
        let used = extra["used_credits"].as_f64().map(|c| c / scale);
        if let Some(used) = used {
            let spend = Spend { used, limit, currency: text(extra, "currency").unwrap_or_else(|| "USD".into()) };
            match windows.iter_mut().find(|w| w.id == "spend_monthly") {
                Some(window) => window.spend = Some(spend),
                None => windows.push(UsageWindow {
                    id: "spend_monthly".into(),
                    utilization: match (extra["utilization"].as_f64(), limit) {
                        (Some(percent), _) => percent / 100.0,
                        (None, Some(limit)) if limit > 0.0 => used / limit,
                        _ => 0.0,
                    },
                    spend: Some(spend),
                    ..Default::default()
                }),
            }
        }
    }
    windows
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// An ISO 8601 time as Unix seconds.
fn unix_seconds(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso).ok().map(|t| t.timestamp())
}

/// A running cost estimate, when there is anything to add.
fn estimate(dollars: f64) -> Vec<AgentEvent> {
    if dollars > 0.0 { vec![AgentEvent::CostEstimate(dollars)] } else { Vec::new() }
}

fn blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message["message"]["content"].as_array().into_iter().flatten()
}

/// The base64 image blocks in a tool result's content.
fn tool_images(content: Option<&Value>) -> Vec<ToolImage> {
    let Some(Value::Array(parts)) = content else { return Vec::new() };
    parts
        .iter()
        .filter(|p| p["type"] == "image" && p["source"]["type"] == "base64")
        .filter_map(|p| {
            let source = |key: &str| p["source"].get(key).and_then(Value::as_str).map(str::to_owned);
            Some(ToolImage { media_type: source("media_type")?, data: source("data")?, file: None })
        })
        .collect()
}

/// A tool result's content is either a string or a list of content blocks.
fn tool_output(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

/// Remove ANSI escape sequences; the agent's reason text may carry terminal colours.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One structured answer from a cheap, tool-less `claude -p` call: no tools, no MCP servers,
/// nothing saved. `schema` constrains the JSON that comes back. Blocking.
pub fn oneshot(
    cwd: &std::path::Path,
    config_dir: Option<&std::path::Path>,
    model: &str,
    system_prompt: &str,
    schema: &Value,
    input: &str,
) -> anyhow::Result<Value> {
    let mut command = Command::new("claude");
    command
        .args(["-p", "--model", model, "--output-format", "json", "--tools", "", "--no-session-persistence"])
        .args(["--strict-mcp-config", "--system-prompt", system_prompt])
        .args(["--json-schema", &schema.to_string()])
        // The user's CLAUDE.md files are for their sessions, not for TJ's helper calls: they
        // cost tokens on every call and leak into the answers (summaries quoted them).
        .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = command.spawn().context("could not start claude")?;
    child.stdin.take().context("claude stdin")?.write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() && output.stdout.is_empty() {
        anyhow::bail!("claude failed: {}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or(""));
    }
    parse_oneshot(&String::from_utf8_lossy(&output.stdout))
}

/// The structured answer in `claude -p --output-format json` output.
pub fn parse_oneshot(output: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(output.trim()).context("claude's output was not JSON")?;
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        anyhow::bail!("{}", value.get("result").and_then(Value::as_str).unwrap_or("the call failed"));
    }
    match value.get("structured_output") {
        Some(v) if !v.is_null() => Ok(v.clone()),
        _ => Ok(serde_json::from_str(value.get("result").and_then(Value::as_str).unwrap_or("{}"))?),
    }
}

/// Whether `version` is newer than [`TESTED_VERSION`], comparing dotted numbers.
pub fn is_newer_than_tested(version: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(version) > parse(TESTED_VERSION)
}

/// Read a recorded session (`{"dir": "out", "msg": ...}` per line) and return what the
/// agent sent, for replaying through [`Parser`].
pub fn read_fixture(reader: impl Read) -> Vec<Value> {
    BufReader::new(reader)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|record| record["dir"] == "out")
        .filter_map(|record| record.get("msg").cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay(name: &str) -> Vec<AgentEvent> {
        let path = format!("{}/tests/fixtures/claude/{name}", env!("CARGO_MANIFEST_DIR"));
        let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut parser = Parser::default();
        read_fixture(file).iter().flat_map(|m| parser.parse(m)).collect()
    }

    fn text(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn slash_commands_are_listed_and_local_ones_print_their_output() {
        let events = replay("2.1.332-slash.jsonl");

        let AgentEvent::Started { commands, .. } = &events[0] else { panic!("{:?}", events[0]) };
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"compact") && names.contains(&"clear"), "{names:?}");
        let effort = commands.iter().find(|c| c.name == "effort").unwrap();
        assert!(effort.argument_hint.starts_with("<low|"), "{effort:?}");

        let outputs: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::CommandOutput { command, text } => Some((command.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 2, "{outputs:?}");
        assert_eq!(outputs[0].0, "effort");
        assert!(outputs[0].1.starts_with("Set effort level to high"), "{outputs:?}");
        assert_eq!(outputs[1], ("compact", "Not enough messages to compact."));
        assert!(text(&events).is_empty(), "command output isn't streamed text");

        assert!(events.contains(&AgentEvent::ConversationReset), "/clear resets the conversation");
        let turns: Vec<_> = events.iter().filter(|e| matches!(e, AgentEvent::TurnFinished { .. })).collect();
        assert_eq!(turns.len(), 3);
        assert!(
            turns.iter().all(|t| matches!(
                t,
                AgentEvent::TurnFinished { cost_usd: None, duration_ms: None, is_error: false, .. }
            )),
            "a local command costs nothing and has no time worth showing: {turns:?}"
        );
    }

    #[test]
    fn a_skill_run_as_a_local_command_prints_its_output() {
        // What `/code-review` sends when it finishes (Claude Code 2.1.332): no
        // `local_command_run`, only `local_command_source`.
        let events = Parser::default().parse(&json!({
            "type": "assistant", "session_id": "s", "parent_tool_use_id": null,
            "local_command_source": "<local-command-stdout>2 findings</local-command-stdout>",
            "message": {
                "model": "<synthetic>", "role": "assistant", "type": "message",
                "content": [{ "type": "text", "text": "2 findings:\n\n1. src/a.rs:3 Off by one" }],
            },
        }));
        assert_eq!(
            events,
            vec![AgentEvent::CommandOutput {
                command: String::new(),
                text: "2 findings:\n\n1. src/a.rs:3 Off by one".into()
            }]
        );
    }

    #[test]
    fn compacting_is_reported() {
        let events = Parser::default().parse(&json!({
            "type": "system", "subtype": "compact_boundary", "session_id": "s",
            "compact_metadata": { "trigger": "manual", "pre_tokens": 48213 },
        }));
        assert_eq!(events, vec![AgentEvent::Compacted { pre_tokens: Some(48213), manual: true }]);
    }

    /// What each turn cost, and what was estimated for it while it ran.
    fn turn_costs(events: &[AgentEvent]) -> Vec<(f64, Option<f64>)> {
        let mut turns = Vec::new();
        let mut estimate = 0.0;
        for event in events {
            match event {
                AgentEvent::CostEstimate(dollars) => estimate += dollars,
                AgentEvent::TurnFinished { cost_usd, .. } => turns.push((std::mem::take(&mut estimate), *cost_usd)),
                _ => {}
            }
        }
        turns
    }

    #[test]
    fn cost_climbs_with_each_response_and_comes_to_what_claude_code_charged() {
        let events = replay("2.1.331-plan.jsonl");
        let estimates = events.iter().filter(|e| matches!(e, AgentEvent::CostEstimate(_))).count();
        assert!(estimates >= 4, "every response adds to the estimate as it streams: {estimates}");
        let turns = turn_costs(&events);
        assert_eq!(turns.len(), 1);
        let (estimated, charged) = turns[0];
        assert!((estimated - charged.unwrap()).abs() < 1e-9, "estimated {estimated}, charged {charged:?}");
    }

    #[test]
    fn each_turn_reports_its_own_cost_not_the_process_total() {
        let turns = turn_costs(&replay("2.1.331-subagent.jsonl"));
        let charged: Vec<f64> = turns.iter().map(|(_, c)| c.unwrap()).collect();
        // Claude Code says 0.0097 and then 0.0113, its total so far; the second turn cost 0.0016.
        assert!((charged[0] - 0.009692325).abs() < 1e-9, "{charged:?}");
        assert!((charged[1] - 0.00157015).abs() < 1e-9, "{charged:?}");
        // The subagent's responses aren't streamed, so its output is only counted as it began.
        assert!(turns[0].0 > 0.0 && turns[0].0 <= charged[0], "{turns:?}");

        // Without the per-result figure, the turn's cost is the growth in the running total.
        let mut parser = Parser::default();
        let result = |total: f64| json!({"type": "result", "is_error": false, "total_cost_usd": total});
        let costs: Vec<_> = [result(0.5), result(0.75)]
            .iter()
            .flat_map(|m| parser.parse(m))
            .filter_map(|e| match e {
                AgentEvent::TurnFinished { cost_usd, .. } => cost_usd,
                _ => None,
            })
            .collect();
        assert_eq!(costs, vec![0.5, 0.25]);
    }

    #[test]
    fn internal_and_repeated_commands_are_left_out() {
        let commands = parse_commands(&json!([
            { "name": "compact", "description": "Summarize", "argumentHint": "<instructions>" },
            { "name": "__remote-workflow", "description": "" },
            { "name": "Compact", "description": "again" },
            { "name": "plugin:skill" },
        ]));
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["compact", "plugin:skill"]);
        assert_eq!(commands[0].argument_hint, "<instructions>");
    }

    #[test]
    fn approved_tool_call_runs_and_the_turn_finishes() {
        let events = replay("2.1.331-approve.jsonl");

        assert!(
            matches!(&events[0], AgentEvent::Started { version: Some(v), permission_mode: Some(PermissionMode::Ask), .. } if v == TESTED_VERSION)
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::SessionInfo { model: Some(m), .. } if m == "claude-haiku-5-5"))
        );

        let tool_id = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolStarted { id, name, input } if name == "Bash" => {
                    assert_eq!(input["command"], "touch tj-probe.txt");
                    Some(id.clone())
                }
                _ => None,
            })
            .expect("a Bash tool call");
        let request = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::PermissionRequested(r) => Some(r.clone()),
                _ => None,
            })
            .expect("a permission request");
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(request.tool_use_id.as_deref(), Some(tool_id.as_str()));
        assert!(!request.suggestions.is_empty());

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { id, is_error: false, .. } if *id == tool_id))
        );
        assert!(text(&events).trim().ends_with("DONE"), "{}", text(&events));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Usage(w) if w.iter().any(|w| w.id == "seven_day"))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnFinished { cost_usd: Some(c), is_error: false, .. } if *c > 0.0))
        );
    }

    #[test]
    fn context_size_and_window_are_reported() {
        let events = replay("2.1.331-approve.jsonl");
        let tokens = events.iter().rev().find_map(|e| match e {
            AgentEvent::Context { tokens: Some(t), .. } => Some(*t),
            _ => None,
        });
        let window = events.iter().find_map(|e| match e {
            AgentEvent::Context { window: Some(w), .. } => Some(*w),
            _ => None,
        });
        assert!(tokens.is_some_and(|t| t > 1_000), "{tokens:?}");
        assert_eq!(window, Some(1_000_000));
    }

    #[test]
    fn questions_and_plans_arrive_as_permission_requests() {
        let question = replay("2.1.331-question.jsonl");
        let request = question
            .iter()
            .find_map(|e| match e {
                AgentEvent::PermissionRequested(r) => Some(r.clone()),
                _ => None,
            })
            .expect("a question");
        assert_eq!(request.tool_name, "AskUserQuestion");
        assert_eq!(request.input["questions"][0]["options"][1]["label"], "Blue");
        assert!(
            question
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { output, .. } if output.contains("have been answered")))
        );

        let plan = replay("2.1.331-plan.jsonl");
        let request = plan
            .iter()
            .find_map(|e| match e {
                AgentEvent::PermissionRequested(r) if r.tool_name == "ExitPlanMode" => Some(r.clone()),
                _ => None,
            })
            .expect("a plan");
        assert!(request.input["plan"].as_str().unwrap().starts_with("# Plan"));
        assert!(
            plan.iter()
                .any(|e| matches!(e, AgentEvent::ToolFinished { output, .. } if output.contains("approved your plan")))
        );
    }

    #[test]
    fn a_background_subagent_reports_through_its_tool_call() {
        let events = replay("2.1.331-subagent.jsonl");
        let agent_id = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolStarted { id, name, .. } if name == "Agent" => Some(id.clone()),
                _ => None,
            })
            .expect("an Agent call");
        let for_agent = |e: &&AgentEvent| match e {
            AgentEvent::SubagentStarted { tool_use_id, .. }
            | AgentEvent::SubagentProgress { tool_use_id, .. }
            | AgentEvent::SubagentTool { tool_use_id, .. }
            | AgentEvent::SubagentText { tool_use_id, .. }
            | AgentEvent::SubagentFinished { tool_use_id, .. } => *tool_use_id == agent_id,
            _ => false,
        };
        let sub: Vec<&AgentEvent> = events.iter().filter(for_agent).collect();
        assert!(
            matches!(sub.first(), Some(AgentEvent::SubagentStarted { agent_type: Some(t), .. }) if t == "general-purpose")
        );
        assert!(sub.iter().any(|e| matches!(e, AgentEvent::SubagentTool { name, .. } if name == "Bash")));
        assert!(sub.iter().any(|e| matches!(e, AgentEvent::SubagentProgress { tool_uses: Some(1), .. })));
        assert!(sub.iter().any(|e| matches!(e, AgentEvent::SubagentText { .. })));
        assert!(
            matches!(sub.last(), Some(AgentEvent::SubagentFinished { status, summary: Some(_), .. }) if status == "completed")
        );
        let turns = events.iter().filter(|e| matches!(e, AgentEvent::TurnFinished { .. })).count();
        assert_eq!(turns, 2, "Claude reports the subagent's result in a turn of its own");
    }

    #[test]
    fn a_background_shell_reports_where_its_output_went() {
        let events = replay("2.1.332-background.jsonl");
        let (id, input) = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolStarted { id, name, input } if name == "Bash" => Some((id.clone(), input.clone())),
                _ => None,
            })
            .expect("a Bash call");
        assert_eq!(input["run_in_background"], true);
        // The call returns at once, naming the file the output goes to.
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ToolFinished { id: i, output, .. } if *i == id && output.contains("Output is being written to: "))
        ));
        let finished = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::SubagentFinished { tool_use_id, status, summary, output_file } if *tool_use_id == id => {
                    Some((status.clone(), summary.clone(), output_file.clone()))
                }
                _ => None,
            })
            .expect("the shell's notification");
        assert_eq!(finished.0, "completed");
        assert!(finished.1.unwrap().contains("exit code 0"));
        assert!(finished.2.unwrap().ends_with(".output"));
    }

    #[test]
    fn images_in_a_tool_result_come_with_it() {
        // The shape Claude Code 2.1.331 sends for Read on a PNG.
        let message = json!({ "type": "user", "message": { "content": [{
            "type": "tool_result", "tool_use_id": "toolu_1",
            "content": [{ "type": "image", "source": { "type": "base64", "data": "iVBORw0K", "media_type": "image/png" } }]
        }]}});
        let events = Parser::default().parse(&message);
        let [AgentEvent::ToolFinished { id, output, images, .. }] = events.as_slice() else { panic!("{events:?}") };
        assert_eq!(id, "toolu_1");
        assert!(output.is_empty());
        assert_eq!(images, &vec![ToolImage { media_type: "image/png".into(), data: "iVBORw0K".into(), file: None }]);
    }

    #[test]
    fn declined_tool_call_comes_back_as_an_error_result() {
        let events = replay("2.1.331-decline.jsonl");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::PermissionRequested(_))));
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ToolFinished { is_error: true, output, .. } if output.contains("declined"))
        ));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnFinished { .. })));
    }

    #[test]
    fn usage_from_the_stream_and_from_get_usage_agree() {
        // The stream reports a fraction and get_usage a percent; both become a fraction.
        let events = replay("2.1.331-approve.jsonl");
        let windows: Vec<f64> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage(w) => w.iter().find(|w| w.id == "seven_day").map(|w| w.utilization),
                _ => None,
            })
            .collect();
        assert!(windows.len() >= 2, "expected stream and get_usage values, got {windows:?}");
        assert!(windows.iter().all(|u| (0.0..=1.0).contains(u)));
        assert!((windows[0] - windows[windows.len() - 1]).abs() < 0.02);
    }

    #[test]
    fn get_usage_reads_the_servers_meters_and_spend() {
        // A subscription: the session and weekly meters, and a model's own weekly one.
        let windows = usage_windows(&json!({
            "five_hour": null,
            "seven_day": { "utilization": 17, "resets_at": "2026-11-27T06:00:00+00:00" },
            "extra_usage": { "is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null },
            "limits": [
                { "kind": "session", "group": "session", "percent": 42, "severity": "normal", "resets_at": "2026-11-25T23:00:00+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_all", "group": "weekly", "percent": 17, "severity": "normal", "resets_at": "2026-11-27T06:00:00+00:00", "scope": null, "is_active": true },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 13, "severity": "normal", "resets_at": null, "scope": { "model": { "display_name": "Fable" } }, "is_active": false }
            ]
        }));
        let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day", "weekly_scoped:Fable"]);
        assert!((windows[0].utilization - 0.42).abs() < 1e-9);
        assert_eq!(windows[1].resets_at, Some(1795759200));
        assert_eq!(windows[2].scope.as_deref(), Some("Fable"));

        // An enterprise seat with a monthly spending limit, in cents.
        let windows = usage_windows(&json!({
            "extra_usage": { "is_enabled": true, "monthly_limit": 50000, "used_credits": 27140, "utilization": 54, "currency": "USD" },
            "limits": [{ "kind": "spend", "group": "monthly", "percent": 54, "severity": "normal", "resets_at": "2026-12-01T00:00:00+00:00", "is_active": true }]
        }));
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "spend_monthly");
        assert_eq!(windows[0].spend, Some(Spend { used: 271.4, limit: Some(500.0), currency: "USD".into() }));

        // Spend with no meter row still shows up; a server without `limits[]` gives its windows.
        let windows = usage_windows(&json!({
            "seven_day": { "utilization": 6, "resets_at": null },
            "extra_usage": { "is_enabled": true, "monthly_limit": 10000, "used_credits": 2500, "utilization": null, "currency": "USD" }
        }));
        let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["seven_day", "spend_monthly"]);
        assert!((windows[1].utilization - 0.25).abs() < 1e-9);
    }

    fn request() -> PermissionRequest {
        PermissionRequest {
            id: "req-1".into(),
            tool_name: "Bash".into(),
            tool_use_id: Some("toolu_1".into()),
            description: None,
            input: json!({ "command": "touch x" }),
            suggestions: vec![
                json!({ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "touch x" }], "behavior": "allow", "destination": "localSettings" }),
                json!({ "type": "setMode", "mode": "acceptEdits", "destination": "session" }),
            ],
            reason: None,
            no_standing_grant: false,
        }
    }

    #[test]
    fn decisions_become_the_answers_claude_expects() {
        let r = request();
        assert_eq!(
            permission_result(&r, &Decision::Approve),
            json!({ "behavior": "allow", "updatedInput": { "command": "touch x" } })
        );

        let session = permission_result(&r, &Decision::ApproveForSession);
        let rules = session["updatedPermissions"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "setMode must not ride along: {rules:?}");
        assert_eq!(rules[0]["type"], "addRules");
        assert_eq!(rules[0]["destination"], "session");

        let declined = permission_result(&r, &Decision::Decline { feedback: Some("use mktemp".into()) });
        assert_eq!(declined["behavior"], "deny");
        assert!(declined["message"].as_str().unwrap().ends_with("use mktemp"));

        let cancelled = permission_result(&r, &Decision::Cancel);
        assert_eq!(cancelled["interrupt"], true);
    }

    #[test]
    fn answers_and_plans_become_the_answers_claude_expects() {
        let mut r = request();
        r.tool_name = "AskUserQuestion".into();
        r.input = json!({ "questions": [{ "question": "Which colour?", "header": "Colour", "options": [], "multiSelect": false }] });
        let answers = std::collections::BTreeMap::from([("Which colour?".to_string(), "Red".to_string())]);
        let answered = permission_result(&r, &Decision::Answer { answers });
        assert_eq!(answered["behavior"], "allow");
        assert_eq!(answered["updatedInput"]["answers"]["Which colour?"], "Red");
        assert_eq!(answered["updatedInput"]["questions"][0]["header"], "Colour", "the questions stay in the input");

        r.tool_name = "ExitPlanMode".into();
        r.input = json!({ "plan": "# Plan" });
        let start = permission_result(&r, &Decision::StartPlan { accept_edits: true });
        assert_eq!(start["updatedPermissions"][0]["mode"], "acceptEdits");
        let ask = permission_result(&r, &Decision::StartPlan { accept_edits: false });
        assert_eq!(ask["updatedPermissions"][0]["mode"], "default");
    }

    #[test]
    fn an_mcp_servers_request_for_input_is_shown_and_answered_in_mcps_terms() {
        use crate::agent::{ELICITATION_TOOL, ElicitAction};
        let events = Parser::default().parse(&json!({
            "type": "control_request",
            "request_id": "e1",
            "request": {
                "subtype": "elicitation", "mcp_server_name": "fieldhook", "message": "Confirm it",
                "mode": "form", "requested_schema": { "type": "object", "properties": {} }
            },
        }));
        let [AgentEvent::PermissionRequested(r)] = events.as_slice() else { panic!("{events:?}") };
        assert_eq!((r.id.as_str(), r.tool_name.as_str()), ("e1", ELICITATION_TOOL));
        assert_eq!(r.description.as_deref(), Some("Confirm it"));
        assert_eq!(r.input["mcp_server_name"], "fieldhook", "the request rides along for the card");

        let accept = Decision::Elicit { action: ElicitAction::Accept, content: Some(json!({ "ok": true })) };
        assert_eq!(permission_result(r, &accept), json!({ "action": "accept", "content": { "ok": true } }));
        let link = Decision::Elicit { action: ElicitAction::Accept, content: None };
        assert_eq!(permission_result(r, &link), json!({ "action": "accept" }));
        let decline = Decision::Elicit { action: ElicitAction::Decline, content: Some(json!({ "ok": true })) };
        assert_eq!(permission_result(r, &decline), json!({ "action": "decline" }), "declining sends nothing");
        // A turn cancelled with the request still open cancels it.
        assert_eq!(permission_result(r, &Decision::Cancel), json!({ "action": "cancel" }));
    }

    #[test]
    fn approve_for_session_without_suggestions_allows_the_tool() {
        let mut r = request();
        r.suggestions.clear();
        let rules = permission_result(&r, &Decision::ApproveForSession)["updatedPermissions"].clone();
        assert_eq!(rules[0]["rules"][0]["toolName"], "Bash");
    }

    #[test]
    fn models_are_read_from_the_initialize_reply() {
        let models = parse_models(&json!([
            { "value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)",
              "description": "Opus 5.5 · Best for everyday, complex tasks", "supportedEffortLevels": ["low", "high"] },
            { "value": "claude-haiku-4-5-20251001", "displayName": "Haiku 4.5", "description": "Fastest" },
            { "displayName": "no value: skipped" }
        ]));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].value, "default");
        assert_eq!(models[0].efforts, vec!["low", "high"]);
        assert_eq!(models[0].resolved.as_deref(), Some("claude-opus-5-5"));
        assert!(models[1].efforts.is_empty(), "a model without effort levels offers none");
    }

    #[test]
    fn images_go_before_the_text_as_base64_blocks() {
        let image =
            crate::agent::PromptImage { media_type: "image/png".into(), data: std::sync::Arc::new(vec![1, 2, 3]) };
        let content = message_content("what is this?", std::slice::from_ref(&image));
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "AQID");
        assert_eq!(content[1], json!({ "type": "text", "text": "what is this?" }));
        assert_eq!(
            message_content("", &[image]).as_array().unwrap().len(),
            1,
            "an image alone has no empty text block"
        );
    }

    #[test]
    fn effort_becomes_a_flag() {
        let options = SessionOptions { model: Some("opus".into()), effort: Some("high".into()), ..Default::default() };
        let args = options.args().join(" ");
        assert!(args.contains("--model opus") && args.contains("--effort high"), "{args}");
    }

    #[test]
    fn sessions_are_told_how_tj_shows_commands() {
        let args = SessionOptions::default().args();
        let at = args.iter().position(|a| a == "--append-system-prompt").expect("flag");
        assert_eq!(args[at + 1], TJ_INSTRUCTIONS);
        assert!(TJ_INSTRUCTIONS.contains("exactly one command in each code block"));
        let args = SessionOptions { eighty_twenty: true, ..Default::default() }.args();
        let at = args.iter().position(|a| a == "--append-system-prompt").expect("flag");
        assert!(args[at + 1].starts_with(TJ_INSTRUCTIONS), "the 80/20 rule comes after TJ's own");
        assert!(args[at + 1].ends_with(crate::summary::INSTRUCTIONS));
    }

    #[test]
    fn sessions_get_the_notes_server_and_its_instructions() {
        let context = crate::notes_mcp::Context {
            file: "/data/notes/p1.json".into(),
            project_dir: "/home/me/proj".into(),
            project_name: "proj".into(),
            thread: Some("t1".into()),
            transcript: None,
        };
        let notes = NotesServer { binary: "/usr/bin/tj".into(), context };
        let args = SessionOptions { notes: Some(Box::new(notes)), eighty_twenty: true, ..Default::default() }.args();
        let at = args.iter().position(|a| a == "--append-system-prompt").expect("flag");
        assert!(args[at + 1].contains(crate::notes::INSTRUCTIONS));
        assert!(args[at + 1].ends_with(crate::summary::INSTRUCTIONS), "the 80/20 rule stays last");
        let at = args.iter().position(|a| a == "--mcp-config").expect("flag");
        let config: Value = serde_json::from_str(&args[at + 1]).unwrap();
        assert_eq!(config["mcpServers"]["tj"]["command"], "/usr/bin/tj");
        assert_eq!(config["mcpServers"]["tj"]["args"][0], "notes-mcp");
        assert!(args.windows(2).any(|w| w == ["--allowedTools", "mcp__tj"]));
        assert!(!SessionOptions::default().args().contains(&"--mcp-config".to_string()));
    }

    /// `cargo test -- --ignored live_` (no tokens: the session is stopped after its handshake)
    #[test]
    #[ignore]
    fn live_models_are_listed() {
        let models = list_models(None).unwrap();
        assert!(models.iter().any(|m| m.value == "haiku"), "{models:?}");
    }

    #[test]
    fn version_check() {
        assert!(!is_newer_than_tested(TESTED_VERSION));
        assert!(!is_newer_than_tested("2.1.300"));
        assert!(is_newer_than_tested("2.1.332"));
        assert!(is_newer_than_tested("2.2.0"));
    }

    #[test]
    fn unknown_messages_are_skipped() {
        let mut parser = Parser::default();
        assert!(parser.parse_line(r#"{"type":"something_new","x":1}"#).is_empty());
        assert!(parser.parse_line("not json").is_empty());
    }

    #[test]
    fn ansi_is_stripped_from_reasons() {
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m path"), "bold path");
    }

    /// Collect events until the turn ends; returns (session id, reply text).
    fn run_turn(events: &smol::channel::Receiver<AgentEvent>) -> (Option<String>, String) {
        let (mut session, mut text) = (None, String::new());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            match smol::block_on(events.recv()).expect("events") {
                AgentEvent::SessionInfo { session_id, .. } => session = Some(session_id),
                AgentEvent::TextDelta(t) => text.push_str(&t),
                AgentEvent::TurnFinished { is_error, error, .. } => {
                    assert!(!is_error, "{error:?}");
                    break;
                }
                AgentEvent::Exited { stderr, .. } => panic!("claude exited: {stderr}"),
                _ => {}
            }
        }
        (session, text)
    }

    /// A real AskUserQuestion, answered through TJ's own answer format.
    #[test]
    #[ignore]
    fn live_question_is_answered() {
        let dir = std::env::temp_dir().join(format!("tj-live-question-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let options = SessionOptions { cwd: dir.clone(), model: Some("haiku".into()), ..Default::default() };
        let (session, events) = ClaudeSession::spawn(&options).unwrap();
        session
            .send_prompt("Use the AskUserQuestion tool to ask me which fruit I prefer, with the options Apple and Pear. Then reply with only the fruit I chose.")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let (mut asked, mut text) = (false, String::new());
        while std::time::Instant::now() < deadline {
            match smol::block_on(events.recv()).expect("events") {
                AgentEvent::PermissionRequested(request) if request.tool_name == "AskUserQuestion" => {
                    asked = true;
                    let question = request.input["questions"][0]["question"].as_str().unwrap().to_string();
                    let answers = std::collections::BTreeMap::from([(question, "Pear".to_string())]);
                    session.respond(&request, Decision::Answer { answers }).unwrap();
                }
                AgentEvent::TextDelta(t) => text.push_str(&t),
                AgentEvent::TurnFinished { .. } => break,
                AgentEvent::Exited { stderr, .. } => panic!("claude exited: {stderr}"),
                _ => {}
            }
        }
        assert!(asked, "expected a question");
        assert!(text.to_lowercase().contains("pear"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Switching model and effort mid-session: the next reply comes from the new model, and
    /// neither control request is refused.
    #[test]
    #[ignore]
    fn live_model_and_effort_switch() {
        let dir = std::env::temp_dir().join(format!("tj-live-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let options = SessionOptions { cwd: dir.clone(), model: Some("haiku".into()), ..Default::default() };
        let (session, events) = ClaudeSession::spawn(&options).unwrap();
        session.set_model(Some("sonnet")).unwrap();
        session.set_effort(Some("low")).unwrap();
        session.send_prompt("Reply with just the word ok.").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut models = Vec::new();
        while std::time::Instant::now() < deadline {
            match smol::block_on(events.recv()).expect("events") {
                AgentEvent::ModelInUse(model) => models.push(model),
                AgentEvent::Error(err) => panic!("a control request was refused: {err}"),
                AgentEvent::TurnFinished { .. } => break,
                AgentEvent::Exited { stderr, .. } => panic!("claude exited: {stderr}"),
                _ => {}
            }
        }
        assert!(models.iter().any(|m| m.contains("sonnet")), "{models:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An attached image reaches the model: a plain red picture is described as red.
    #[test]
    #[ignore]
    fn live_image_is_seen() {
        let dir = std::env::temp_dir().join(format!("tj-live-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = Vec::new();
        image::RgbImage::from_pixel(64, 64, image::Rgb([220, 20, 20]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let image = crate::agent::PromptImage { media_type: "image/png".into(), data: std::sync::Arc::new(png) };
        let options = SessionOptions { cwd: dir.clone(), model: Some("haiku".into()), ..Default::default() };
        let (session, events) = ClaudeSession::spawn(&options).unwrap();
        session.send_message("What colour is this image? Reply with one word.", &[image]).unwrap();
        let mut reply = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while std::time::Instant::now() < deadline {
            match smol::block_on(events.recv()).expect("events") {
                AgentEvent::TextDelta(text) => reply.push_str(&text),
                AgentEvent::TurnFinished { .. } => break,
                AgentEvent::Exited { stderr, .. } => panic!("claude exited: {stderr}"),
                _ => {}
            }
        }
        assert!(reply.to_lowercase().contains("red"), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--resume` continues the same conversation in a new process.
    #[test]
    #[ignore]
    fn live_resume_continues_the_conversation() {
        let dir = std::env::temp_dir().join(format!("tj-live-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut options = SessionOptions { cwd: dir.clone(), model: Some("haiku".into()), ..Default::default() };
        let (first, events) = ClaudeSession::spawn(&options).unwrap();
        first.send_prompt("Remember this word for later: tangerine. Reply with just OK.").unwrap();
        let (session, _) = run_turn(&events);
        drop(first);

        options.resume = Some(session.expect("a session id"));
        let (second, events) = ClaudeSession::spawn(&options).unwrap();
        second.send_prompt("What word did I ask you to remember? Reply with just the word.").unwrap();
        let (_, reply) = run_turn(&events);
        assert!(reply.to_lowercase().contains("tangerine"), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs the real `claude` (uses a little of the logged-in account's usage):
    /// `cargo test -- --ignored live_`
    #[test]
    #[ignore]
    fn live_session_asks_and_runs_after_approval() {
        let dir = std::env::temp_dir().join(format!("tj-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let options = SessionOptions { cwd: dir.clone(), model: Some("haiku".into()), ..Default::default() };
        let (session, events) = ClaudeSession::spawn(&options).unwrap();
        session
            .send_prompt("Use the Bash tool to run exactly: touch tj-live.txt. Then reply with one word: DONE.")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut asked = false;
        while std::time::Instant::now() < deadline {
            match smol::block_on(events.recv()).expect("events") {
                AgentEvent::PermissionRequested(request) => {
                    asked = true;
                    session.respond(&request, Decision::Approve).unwrap();
                }
                AgentEvent::TurnFinished { is_error, .. } => {
                    assert!(!is_error);
                    break;
                }
                AgentEvent::Exited { stderr, .. } => panic!("claude exited: {stderr}"),
                _ => {}
            }
        }
        assert!(asked, "expected a permission request");
        assert!(dir.join("tj-live.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
