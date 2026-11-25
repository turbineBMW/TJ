# TJ

A native GUI for coding agents, built in Rust on [gpui](https://github.com/zed-industries/zed) via [gpui-kit](https://github.com/longbridge/gpui-kit). In TJ, projects come first and threads live under them. Each project has a default agent and account, and you can switch temporarily. The window also has a terminal, a usage and task sidebar, and a diff review with a summary of each file.

Named after Tony Jay, who voiced Judge Claude Frollo.

## Status

- **Projects and threads.** A full-height sidebar lists your projects with their threads. Add a project with **+** at the top; start a thread with a project's **+**. Each thread keeps its Claude session running in the background, and a dot shows threads that are working (amber) or waiting for you. At the end of each thread's row is the mark of the agent running it, taken from T3 Code (Claude, Codex, OpenCode, Cursor, Grok, Pi, Kiro, Muse, fx and Antigravity; hover it for the name). Claude is the only backend so far, so today every thread shows Claude's. Black-and-white marks are drawn in the theme's text colour; coloured ones keep their colours.
- **Managing threads and projects**: right-click a thread to rename (or double-click it), archive or delete it. A name you give stays; clearing it lets TJ name the thread again. Archived threads sit under "Archived (n)" at the bottom of their project. Archiving or deleting the thread on show moves the agent tab to the next thread in the same project. Right-click a project to remove it from TJ; its folder is never touched. Deleting asks first.
- **Project pictures.** Each project shows a small picture in the sidebar and the top bar. TJ looks through the project for its icon or logo (a file named like `icon`, `logo`, `favicon`, `ic_launcher` or the project itself, or one in an `icons` folder; big square images win, and dependencies, build output and tests are skipped). With no icon it shows the project's initials. Right-click the project, or the picture in the top bar, to **Choose image…** (TJ keeps a copy, so moving the original doesn't break it), **Use initials**, or go back to **Use the project's icon**.
- **`@file` mentions**: type `@` in the prompt box to pick a project file (Up/Down, then Enter or Tab; Escape closes the list). The file shows as a chip and is sent as `@path`, which Claude Code expands into the file's contents.
- **Images**: paste one (Ctrl+V), drop it on the prompt box, or pick it with the paperclip. It goes with the next message; large images are scaled down first. Dropped or picked files that aren't images become `@file` mentions. Sent images are saved with the thread, so they show again when it's reopened.
- **Model and effort**, from a dropdown next to Send (for example "Opus 5.5 · High"). The list comes from `claude` itself for the thread's account (read once in the background, without using tokens), with each model's description and the effort levels it supports. Switching mid-session takes effect on the next reply; "Use for new threads in <project>" makes it the project's default.
- **Accounts and modes**, as dropdowns under the prompt box. Each thread runs on an account: the thread's own choice, else the project's default, else the global default. The account menu switches the thread for now, makes the account the project default, or starts `claude auth login` for it in a terminal; the mode menu picks Ask, Accept edits, Auto, Plan or Full access, or makes the current one the project's default. Switching a thread that has history starts a new Claude session on the other account, with the conversation so far as context (a session can't change accounts). Short conversations go over word for word. Longer ones go as a summary written by Haiku on the new account, plus the overview and the newest messages word for word. A message sent before the summary is ready waits for it, and if TJ restarts first, the handoff is prepared again.
- **Saved across restarts.** Projects, threads and transcripts are saved; reopening a thread shows its history and resumes its Claude conversation.
- **Overview and usage**, like the pi session sidebar, in a status column on the right of the agent tab, so it stays with the thread it describes. It appears only when there's something to show (an overview or uncommitted changes), folds away when the agent tab is narrow (under about 760 px), and the **panel** button at the right of the top bar shows or hides it by hand. From the top: the session summary and tasks fill the space, the changed files sit below them, and the agent's numbers are pinned at the bottom. It shows the thread's **Goal**, **Now**, **Next** and **Tasks** (recent done ones struck through), rewritten by Haiku on the thread's account after each turn and every minute during long ones, from the newest part of the conversation. Claude's own task list (TodoWrite) feeds into it. The thread's title in the sidebar comes from it too. Underneath: context used against the model's window, subscription usage windows, cost (marked ≈ on a subscription, where it's an estimate) and the model. **↻** updates it now.
- **Changes and review.** Under the overview, the project's uncommitted changes (git's working tree against HEAD, new files included) with A/M/D/R, paths and line counts; a dot marks files the current thread edited. Click a file, or **Review**, to open the review tab: every diff stacked (unified or side by side), scrolled to that file. Under each file's header is a short summary of what changed and why, written by Haiku on the thread's account with your recent requests as context; summaries are cached until the file changes again. The list refreshes as the agent works.
- **Omarchy theme, live.** TJ takes its colours from the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`, resolved with the same cascade as Bubo and Rustle, so legacy themes work too) and follows theme switches within two seconds. Light themes get a readable "ink" for text, as in Bubo. The terminal follows the theme's `alacritty.toml`, like your other terminals, so a light theme that pins its terminals dark stays dark there.
- **Transparent sidebar**, as an app setting like the rest of the app family: ⚙ in the sidebar header has **Transparent sidebar** (off by default) and an **Opacity** slider (0.8 by default), plus **Follow Omarchy theme**. Settings are saved in `~/.config/tj/settings.toml`.
- **Smooth cursor**, in the terminal and in every text field (the prompt box, answers, renaming): it glides to each new place and blinks with a soft fade, solid while you type. With **Cursor trail** on it stretches along its path and snaps back, like Neovide. ⚙ has a **Cursor animation** slider (Off to 300 ms, 90 ms by default) and the **Cursor trail** switch (on by default).
- **Panes and tabs**, like Vmux. The main area is the project's panes, split right or down, and every pane has its own tabs. A tab is the **agent**, a **terminal** or the **review**. Each project has one agent tab, and picking a thread in the sidebar swaps what it shows: the conversation and its status column change, and the terminals, splits and other tabs stay exactly where they were. So terminals belong to the project, not the thread, and a terminal can sit behind the agent as a background tab in the same pane. To work on two things at once, add a git worktree of the project as its own project. A new project starts with just its agent tab; the tab strip appears once a pane has a second tab. Closing a tab goes back to the one used before; closing a pane's last tab folds its split; a shell that exits closes its tab. If the agent tab is closed, picking a thread opens it again in the focused pane. The layout and each terminal's directory are saved per project. Vmux's shortcuts work anywhere in the main area:

  | Keys | Action |
  |---|---|
  | `Ctrl+Shift+T` / `Ctrl+Shift+W` | New terminal tab / close tab |
  | `Ctrl+Shift+E` / `Ctrl+Shift+O` | Split right / down |
  | `Ctrl+Alt+←/→/↑/↓` | Focus the pane in that direction |
  | `Ctrl+Shift+A` | Focus the next pane |
  | `Ctrl+Shift+=` / `Ctrl+Shift+-` | Grow / shrink the pane (or drag the divider between panes; double-click it to even them out) |
  | `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |
- **GNOME-style window**: no title bar; drag either header to move it, double-click to maximize. The sidebar header has **add project** on the left, **TJ** in the middle, and **settings** on the right; each project's **+** starts a new thread. The top bar shows `[badge] Project / Thread`, like T3 Code, with the pane actions (new terminal, split right, split down) on the right. The conversation stays at a readable width in a wide window.
- **A start screen for new threads**, like T3 Code's: "What should we build in <project>?", a large prompt card with the account and mode menus, and the folder, branch and mode underneath. The first message switches to the conversation, and the status column appears once there's something in it.
- **Streaming replies** in markdown, with tool calls shown as cards. Each card's header has the tool's icon and name, a one-line summary (Bash's own description of its command, a path relative to the project, Read's line range) and a spinner while it runs, then a check or a cross. Tools are coloured by family from the theme's terminal palette, so they follow Omarchy theme switches: reading and search blue, edits and writes yellow, web cyan, agents magenta, planning, scheduling, worktrees and the rest in the bright colours, and MCP tools as "server · tool". Shell calls (Bash, Monitor, ...) are drawn as a small grey terminal on near-black: the command after a `$`, then its output, red when it failed. Bash is syntax highlighted in the theme's terminal colours: the command as bash, JSON output as JSON, a diff by line (bold headers, cyan hunks, red and green changes), and otherwise error and warning lines in red and yellow. A Bash card too long for its space opens in a dialog from its header, over a shaded backdrop, where all of it scrolls (Escape or a click outside closes it); the fold button next to its status folds it instead. Edit shows a line diff, Write the new file with line numbers, TodoWrite a checklist, a picture Claude reads (or an MCP tool returns) shows in its card at its own size up to 360 px tall, with its dimensions, and opens the card, and other tools their arguments and result (JSON laid out readably). A card's body scrolls once it reaches half the window. Pictures are saved beside the thread's transcript, like attached images, so the transcript stays small and they show again when the thread is reopened. Bash, Edit and Write cards start open; click any header to open or close it.
- **Subagents.** When Claude hands work to a subagent (the Agent tool, which runs in the background), its row shows the agent type, a live "now doing" line, and its tool count and tokens; expand it for the subagent's tool calls and its report. The row stays "working" until the subagent finishes, and the turn Claude starts on its own to report back shows as working too.
- **Approval cards** when Claude wants to do something risky: **Approve**, **Approve for session**, **Decline** (with optional feedback that's passed to Claude) or **Cancel turn**.
- **Questions and plans.** When Claude asks you something (AskUserQuestion), a card shows each question with its options: pick one (or several, for a multi-select) or type your own answer. When Claude presents a plan in plan mode, the card shows it formatted, with **Start, accepting edits**, **Start, asking first** (both leave plan mode) or **Keep planning** (with optional feedback).
- **Permission modes** for the thread: Ask, Accept edits, Auto, Plan and Full access.
- **Usage and cost**: subscription usage windows (for example "7d 4%"), plus the cost of each turn and the whole thread.
- **Stop** interrupts a running turn. If `claude` exits, the next message resumes the conversation.

Next ideas are in [ROADMAP.md](ROADMAP.md); see [ROADMAP.md](ROADMAP.md).

## Install

```sh
./install.sh            # build, then install to ~/.local: the tj command, a launcher entry and the icon
./uninstall.sh          # remove it again; projects, threads and settings stay
./uninstall.sh --purge  # and remove those too
```

`install.sh` installs any missing system packages with pacman (Vulkan loader, xkbcommon, xcb, git), builds a release binary and puts TJ in the app launcher. TJ runs agents through the `claude` command (Claude Code), which you install and log in to yourself; the script says so if it's missing. The window's app ID is `dev.turbinebmw.TJ`, matching the launcher entry, so the running window shows TJ's icon. The icon ships as PNGs from 16 to 512 px plus an SVG wrapping the 512 px art, so launchers that prefer SVG (Omarchy's does) draw it sharp instead of scaling up a small PNG.

## Run

```bash
cargo run                                # reopen where you left off (the current folder on first run)
cargo run -- --project ~/Projects/Vord   # open (and add) a project
cargo run -- --model haiku               # pick a model
```

The prompt box sends on Enter; Shift+Enter adds a line. Tab never moves focus. Click the terminal to focus it; Tab, Shift+Tab and Ctrl+C go to the shell, and Ctrl+Shift+V pastes.

In the terminal, drag to select text, double-click for a word and triple-click for a line. What you select goes to the primary selection, as in other Linux terminals: middle-click pastes it. Ctrl+Shift+C copies the selection to the clipboard. Apps that ask for the mouse (vim, htop, less with mouse on) get clicks, drags and the scroll wheel, in the SGR or classic encoding they ask for; hold Shift to select text in them instead.

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

- `~/.config/tj/settings.toml`: follow the Omarchy theme, transparent sidebar, opacity, and how long the cursor glides after a key (`cursor_glide_ms`).
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
- The terminal handles colours, bold, underline, inverse, Nerd Font glyphs, resize, scrollback, the alt screen and bracketed paste. It also does mouse selection and copy, and mouse reporting. Its cursor is the smooth one above; apps that ask for a steady cursor or a beam or underline get one, and an unfocused terminal shows a hollow block. It shows **inline images** sent with the kitty graphics protocol (`kitten icat`, yazi), SIXEL (`magick image.png sixel:-`, chafa) or iTerm2's inline images: they scroll with the text and clear with it, and text written over an image replaces that part of it. TJ answers the queries those tools send first (kitty support, SIXEL in the device attributes, the cell size in pixels), so they find it without configuration.
