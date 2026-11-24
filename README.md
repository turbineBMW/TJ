# TJ

A native GUI for coding agents, built in Rust on [gpui](https://github.com/zed-industries/zed) via [gpui-kit](https://github.com/longbridge/gpui-kit). In TJ, projects come first and threads live under them. Each project has a default agent and account, and you can switch temporarily. The window also has a terminal, a usage and task sidebar, and a diff review with a summary of each file.

Named after Tony Jay, who voiced Judge Claude Frollo.

## Status

- **Projects and threads.** A full-height sidebar lists your projects with their threads. Add a project with **+** at the top; start a thread with a project's **+**. Each thread keeps its Claude session running in the background, and a dot shows threads that are working (amber) or waiting for you.
- **Accounts and modes**, as dropdowns under the prompt box. Each thread runs on an account: the thread's own choice, else the project's default, else the global default. The account menu switches the thread for now, makes the account the project default, or starts `claude auth login` for it in a terminal; the mode menu picks Ask, Accept edits, Auto, Plan or Full access, or makes the current one the project's default. Switching a thread that has history starts a new Claude session on the other account, with the conversation so far as context (a session can't change accounts).
- **Saved across restarts.** Projects, threads and transcripts are saved; reopening a thread shows its history and resumes its Claude conversation.
- **Overview and usage**, like the pi session sidebar, in its own pane on the right that appears only when there's something to show (an overview or uncommitted changes). From the top: the session summary and tasks fill the space, the changed files sit below them, and the agent's numbers are pinned at the bottom. It shows the thread's **Goal**, **Now**, **Next** and **Tasks** (recent done ones struck through), rewritten by Haiku on the thread's account after each turn and every minute during long ones, from the newest part of the conversation. Claude's own task list (TodoWrite) feeds into it. The thread's title in the sidebar comes from it too. Underneath: context used against the model's window, subscription usage windows, cost (marked ≈ on a subscription, where it's an estimate) and the model. **↻** updates it now.
- **Changes and review.** Under the overview, the project's uncommitted changes (git's working tree against HEAD, new files included) with A/M/D/R, paths and line counts; a dot marks files the current thread edited. Click a file, or **Review**, to open the review tab: every diff stacked (unified or side by side), scrolled to that file. Under each file's header is a short summary of what changed and why, written by Haiku on the thread's account with your recent requests as context; summaries are cached until the file changes again. The list refreshes as the agent works.
- **Omarchy theme, live.** TJ takes its colours from the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`, resolved with the same cascade as Bubo and Rustle, so legacy themes work too) and follows theme switches within two seconds. Light themes get a readable "ink" for text, as in Bubo. The terminal follows the theme's `alacritty.toml`, like your other terminals, so a light theme that pins its terminals dark stays dark there.
- **Transparent sidebar**, as an app setting like the rest of the app family: ⚙ in the sidebar header has **Transparent sidebar** (off by default) and an **Opacity** slider (0.8 by default), plus **Follow Omarchy theme**. Settings are saved in `~/.config/tj/settings.toml`.
- **Panes and tabs**, like Vmux. The main area is the project's panes, split right or down, and every pane has its own tabs. A tab is a **conversation**, a **terminal** or the **review**, so you can put a thread beside a shell, or two threads side by side. Picking a thread in the sidebar focuses its tab, or opens it in the focused pane. A new project starts with just its conversation; the tab strip appears once a pane has a second tab. Closing a tab goes back to the one used before; closing a pane's last tab folds its split; a shell that exits closes its tab. The layout, each terminal's directory and which threads are open are saved per project. Vmux's shortcuts work anywhere in the main area:

  | Keys | Action |
  |---|---|
  | `Ctrl+Shift+T` / `Ctrl+Shift+W` | New terminal tab / close tab |
  | `Ctrl+Shift+E` / `Ctrl+Shift+O` | Split right / down |
  | `Ctrl+Alt+←/→/↑/↓` | Focus the pane in that direction |
  | `Ctrl+Shift+A` | Focus the next pane |
  | `Ctrl+Shift+=` / `Ctrl+Shift+-` | Grow / shrink the pane |
  | `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |
- **GNOME-style window**: no title bar; drag either header to move it, double-click to maximize. The sidebar header has **new thread** on the left, **TJ** in the middle, and **settings** and **add project** on the right. The top bar shows `[badge] Project / Thread`, like T3 Code, with the pane actions (new terminal, split right, split down) on the right. The conversation stays at a readable width in a wide window.
- **Streaming replies** in markdown, with tool calls shown as cards you can expand to see their output.
- **Approval cards** when Claude wants to do something risky: **Approve**, **Approve for session**, **Decline** (with optional feedback that's passed to Claude) or **Cancel turn**.
- **Permission modes** for the thread: Ask, Accept edits, Auto, Plan and Full access.
- **Usage and cost**: subscription usage windows (for example "7d 4%"), plus the cost of each turn and the whole thread.
- **Stop** interrupts a running turn. If `claude` exits, the next message resumes the conversation.

Next ideas are in [ROADMAP.md](ROADMAP.md); see [ROADMAP.md](ROADMAP.md).

## Run

```bash
cargo run                                # reopen where you left off (the current folder on first run)
cargo run -- --project ~/Projects/Vord   # open (and add) a project
cargo run -- --model haiku               # pick a model
```

The prompt box sends on Enter; Shift+Enter adds a line. Tab never moves focus. Click the terminal to focus it; Tab, Shift+Tab and Ctrl+C go to the shell, and Ctrl+Shift+V pastes.

Test hooks:

- `--prompt "text"` sends a prompt in a new thread at launch.
- Demos and replays use a throwaway folder, so they never touch your saved projects.
- `--replay tests/fixtures/claude/2.1.331-approve.jsonl` plays a recorded session through the real parser, without running a model.
- `--demo stream|firehose` streams a canned reply.
- `--review` opens the review of the project's changes at launch.
- `--type 'text\r'` sends text to the terminal.
- `TJ_UI_FONT="Family"` overrides the UI font.
- `TJ_HOME=/some/dir` keeps all of TJ's files in one folder (handy for trial runs).

## Where things are kept

- `~/.config/tj/accounts.toml`: your Claude accounts. Created on first run from the config directories TJ finds (`~/.claude`, plus any `~/.claude-<name>` or `~/.claude_<name>` with a login). Edit names or add accounts here:

  ```toml
  default = "personal"

  [[account]]
  id = "personal"
  name = "Personal"          # no config_dir: Claude's default, ~/.claude

  [[account]]
  id = "work"
  name = "Work"
  config_dir = "~/.claude-work"
  ```

- `~/.config/tj/settings.toml`: follow the Omarchy theme, transparent sidebar, opacity.
- `~/.local/share/tj/state.json`: projects, threads, defaults, and what was open.
- `~/.local/share/tj/threads/<id>.jsonl`: each thread's transcript.

## How TJ talks to Claude

TJ runs the `claude` binary and speaks the stdio protocol that Anthropic's Agent SDKs use: stream-json in both directions, plus control messages. Permission prompts come back to TJ as `can_use_tool` requests because it passes `--permission-prompt-tool=stdio`. This is the same mechanism T3 Code uses through the TypeScript SDK, without Node.

The protocol isn't a documented public API, so:

- Everything that depends on its shape is in `src/agent/claude.rs`, behind the `AgentSession` interface in `src/agent/mod.rs`.
- Unknown messages are skipped, not treated as errors.
- TJ warns when `claude` is newer than the version it was tested with (`TESTED_VERSION`).
- Recorded sessions in `tests/fixtures/claude/` pin the message shapes. After a `claude` update, re-record them:

  ```bash
  python3 -I scripts/record-claude-session.py /tmp/raw.jsonl allow   # uses about a cent on Haiku
  python3 -I scripts/scrub-claude-fixture.py /tmp/raw.jsonl tests/fixtures/claude/<version>-approve.jsonl
  ```

  The scrubber removes account details, MCP servers, plugins, hooks and local paths, and fails if anything personal survives. Never commit a raw recording.

## Tests

```bash
cargo test                          # parser, decisions, terminal keys, and UI tests in headless windows
cargo test -- --ignored live_       # runs the real claude once (needs a login; uses a little usage)
```

## Notes from the spike

- **gpui on Linux ignores the weight axis of variable fonts.** Adwaita Sans and gpui-kit's `.SystemUIFont` render **bold** as regular. TJ uses the desktop font only when it has a separate bold file, and falls back to Noto Sans otherwise.
- **gpui runs key bindings before a view's key handler.** gpui-kit's root view binds Tab to focus traversal and Ctrl+C to Copy; `src/keymap.rs` and `terminal::init` unbind what TJ needs.
- **A hidden window doesn't repaint** until something asks for a frame. The model keeps updating, so nothing is lost.
- **gpui-kit's root paints the theme background over the whole window**, which would hide a see-through sidebar. `main` clears the root's own background right after opening the window (the root's style is applied after its plugins'), and the content area paints its own.
- gpui-kit 0.11 pins `gpui-pre` 0.3.14, a snapshot of Zed's gpui published by the gpui-kit author, not Zed's `gpui` 0.2.x on crates.io.
- The terminal handles colours, bold, underline, inverse, Nerd Font glyphs, resize, scrollback, the alt screen and bracketed paste. Not done yet: mouse selection and copy, mouse reporting, a blinking cursor, and images.
