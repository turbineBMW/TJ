# TJ

A native GUI for coding agents, built in Rust on [gpui](https://github.com/zed-industries/zed) via [gpui-kit](https://github.com/longbridge/gpui-kit). In TJ, projects come first and threads live under them. Each project has a default agent and account, and you can switch temporarily. The window also has a terminal, a usage and task sidebar, and a diff review with a summary of each file. It's a GUI, but it's meant to be driven entirely from the keyboard when you want to (see "Keyboard rules" in [ROADMAP.md](ROADMAP.md)).

Named after Tony Jay, who voiced Judge Claude Frollo.

## Status

- **Projects and threads.** A full-height sidebar lists your projects with their threads. Add a project with **+** at the top; start a thread with a project's **+**. Each thread keeps its Claude session running in the background, and a dot shows threads that are working (amber) or waiting for you. At the end of each thread's row is the mark of the agent running it, taken from T3 Code (Claude, Codex, OpenCode, Cursor, Grok, Pi, Kiro, Muse, fx and Antigravity; hover it for the name). Claude is the only backend so far, so today every thread shows Claude's. Black-and-white marks are drawn in the theme's text colour; coloured ones keep their colours.
- **Managing threads and projects**: right-click a thread to rename (or double-click it), archive or delete it. A name you give stays; clearing it lets TJ name the thread again. Archived threads sit under "Archived (n)" at the bottom of their project. Archiving or deleting the thread on show moves the agent tab to the next thread in the same project. Right-click a project to remove it from TJ; its folder is never touched. Deleting asks first.
- **Project pictures.** Each project shows a small picture in the sidebar and the top bar. TJ looks through the project for its icon or logo (a file named like `icon`, `logo`, `favicon`, `ic_launcher` or the project itself, or one in an `icons` folder; big square images win, and dependencies, build output and tests are skipped). With no icon it shows the project's initials. Right-click the project, or the picture in the top bar, to **Choose image…** (TJ keeps a copy, so moving the original doesn't break it), **Use initials**, or go back to **Use the project's icon**.
- **`@file` mentions**: type `@` in the prompt box to pick a project file (Up/Down, then Enter or Tab; Escape closes the list). The file shows as a chip and is sent as `@path`, which Claude Code expands into the file's contents.
- **`/` commands**: type `/` at the start of the prompt for Claude Code's commands, your own commands and skills (the project's too), each with what it takes and what it does. Typing filters the list; **↑/↓** move, **Enter** or **Tab** puts the command in the prompt (Enter on a name typed in full sends it), **Escape** closes. TJ asks `claude` for the list when a thread opens (no prompt is sent, so it costs nothing), and a running session's own list replaces it. Commands go to Claude as typed. The ones Claude Code runs itself (`/context`, `/cost`, `/effort high`) show what they printed as a reply; `/clear` notes that Claude starts afresh from there (the messages above stay on screen), and compacting says so too.
- **Images**: paste one (Ctrl+V), drop it on the prompt box, or pick it with the paperclip. It goes with the next message; large images are scaled down first. Dropped or picked files that aren't images become `@file` mentions. Sent images are saved with the thread, so they show again when it's reopened.
- **Model and effort**, from a dropdown next to Send (for example "Opus 5.5 · High"). The list comes from `claude` itself for the thread's account (read once in the background, without using tokens), with each model's description and the effort levels it supports. Switching mid-session takes effect on the next reply; "Use for new threads in <project>" makes it the project's default.
- **Accounts and modes**, as dropdowns under the prompt box. Each thread runs on an account: the thread's own choice, else the project's default, else the global default. The account menu switches the thread for now, makes the account the project default, or starts `claude auth login` for it in a terminal; the mode menu picks Ask, Accept edits, Auto, Plan or Full access, or makes the current one the project's default. Switching a thread that has history starts a new Claude session on the other account, with the conversation so far as context (a session can't change accounts). Short conversations go over word for word. Longer ones go as a summary written by Haiku on the new account, plus the overview and the newest messages word for word. A message sent before the summary is ready waits for it, and if TJ restarts first, the handoff is prepared again.
- **Saved across restarts.** Projects, threads and transcripts are saved; reopening a thread shows its history and resumes its Claude conversation.
- **Overview and usage**, like the pi session sidebar, in a status column at the right of the window for the thread picked in the sidebar, whichever pane is focused. It sits in the window's frame under the top bar, mirroring the sidebar (see-through with it). The overview sits straight on the frame in the sidebar's colours; the changed files and the numbers are cards in the colour of the sidebar's selected thread, at half the frame's opacity. It shows for every thread, a new one too, folds away when it would leave the content narrower than about 640 px, and the **panel** button at the right of the top bar shows or hides it by hand. From the top: the session summary and tasks, then the changed files and the agent's numbers at the bottom. It shows the thread's **Goal**, **Now**, **Next** and **Tasks** (recent done ones struck through), each saying so when there's nothing yet ("No goal established yet", "Nothing in progress"). They're rewritten by Haiku on the thread's account after each turn and every minute during long ones, from the newest part of the conversation. Claude's own task list (TodoWrite) feeds into it. The thread's title in the sidebar comes from it too. **↻** updates it now. Pinned underneath, on every thread, is **Usage**, for the thread's account (shared by every thread on it), with the account and its plan in a badge. Each limit is a meter with an emoji, a bar, the percent used and when it resets. A subscription shows its **5-hour** and **Weekly** windows, plus **Monthly spend** when extra usage is on. A team or enterprise seat shows its spending limit ("$271 / $500"). A model's own weekly window shows only once Claude says it's getting close. Bars take a colour from the theme's terminal palette, then turn yellow from 75% (or when Claude warns) and red from 90%, with a glow. Then **Context** (tokens against the model's window; on a new thread, the window the model reported last time, kept across restarts, or "after the first message" if it never has) and what **This thread** has cost, marked ≈ "estimate" on a subscription. The model isn't repeated there; it's on the prompt card. Usage is asked for when `claude` starts and again after every turn. The last numbers heard, and each model's context window, are saved in `usage.json` beside the state file, so after a restart they show before any thread runs (a window that has reset since shows empty).
- **Changes and review.** Under the overview, the project's uncommitted changes (git's working tree against HEAD, new files included) with A/M/D/R, paths and line counts; a dot marks files the current thread edited. Click a file to open the review tab with just that file's diff (unified or side by side) and summary; with more than one file, **View all** under the list opens every diff stacked, each with its summary. Only the files on screen are summarized. Under each file's header is a short summary of what changed and why, written by Haiku on the thread's account with your recent requests as context; summaries are cached until the file changes again. The list refreshes as the agent works.
- **GitHub, per project, on the right account.** The GitHub button in the top bar (or **Ctrl+Shift+G**) opens the project's GitHub tab. TJ never switches `gh`'s active account: every call gets the token of the account this project uses (`gh auth token --user <login>`), so a personal project and a work project work side by side. The account is picked from the remote (the owner, or an SSH host alias naming a login, like `github_fieldhook`), else the first account that can push to the repository; the account menu in the tab's header picks another for the project, or goes back to picking from the remote, and the choice is saved. The header shows the repository, the checked-out branch (its menu switches to any local or remote branch), how far it is from its upstream, and **Pull** (fast-forward only, so nothing gets merged by surprise), **Push** (or **Publish** for a new branch) and **Refresh** (fetch and reload). Git runs against the project's own remote, so an SSH alias keeps its key; an HTTPS remote gets the account's token through `gh`. On the left are the open, merged or closed pull requests with their checks, review state and size. The pull request picked shows its **Conversation** (description and comments as markdown, reviews, a merge box with every check, the merge method the repository allows, **Delete branch**, **Ready for review** for a draft, and a comment box with **Close pull request**, or **Reopen** for a closed one, which posts the box's text first if there is any), its **Files changed** and its **Commits**. **Check out** puts its branch in the project folder (a fork's as `pr-<n>`).

  **Files changed** shows the diff, **Unified** or **Split** from the switch beside the section tabs. Hover a line and press **+**, or select lines, to write a line comment, kept for the review or posted on its own; existing comments show under their lines with **Reply**. The header of the file you're in stays pinned to the top as you scroll (click it to go to the file's start, or its arrow to collapse the file). A bar floating at the bottom moves through the files: back to the top, previous file, which file you're in ("File 3 of 16"), next file, and down to the review. Each file header's expand button opens the file in a modal with a short summary of its change, written by Haiku with the pull request's title and description as the intent; **Previous**/**Next** (or **←/→**) step through the files there, and summaries are kept until the file changes. At the end of the diff, the review box sends **Comment**, **Approve** or **Request changes** with the line comments, or closes the pull request; Approve and Request changes are off on your own pull request, which GitHub doesn't allow.

  When the tab is narrow, the list is a page of its own: picking a pull request opens it full width, and **Pull requests** at its top (or **Escape**) goes back. Keys in the tab: **↑/↓** move between pull requests, **Enter** opens the one picked when the tab is narrow, **Alt+1/2/3** pick the section, **F5** refreshes, **Ctrl+Alt+L** pulls, and **Ctrl+Enter** sends a comment, review or line comment. It needs the GitHub CLI, logged in to each account (`gh auth login`).
- **Omarchy theme, live.** TJ takes its colours from the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`, resolved with the same cascade as Bubo and Rustle, so legacy themes work too) and follows theme switches within two seconds. The window's surfaces are the ones Chromium gets from the same theme: Omarchy hands Chromium the theme's background, and Chrome builds Material "tonal spot" palettes from its hue. TJ does the same, then uses Chrome's frame colour for the sidebar, the top bar and the tabs not on show, its toolbar colour for the tab on show, and its address-bar colour for the prompt box and cards. Every tab floats on the top bar; the content below keeps the theme's own background, as its terminal does. Light themes get a readable "ink" for text, as in Bubo. The terminal follows the theme's `alacritty.toml`, like your other terminals, so a light theme that pins its terminals dark stays dark there.
- **Settings**, in a modal: ⚙ in the sidebar header, or **Ctrl+,**. Its tabs are **General** (the default account, for threads in projects that don't pick their own; written into `accounts.toml`, leaving the rest of that file alone), **Appearance** (**Follow Omarchy theme**, **Interface font** and **Monospace font**, **Transparent sidebar**, sidebar **Opacity** and **Cursor animation**), **Keybindings** and **About** (version, and where TJ keeps its files). It works from the keyboard: **Ctrl+PageUp/PageDown** switch tabs, **↑/↓** pick a row, **Enter** or **Space** toggles or changes it, **←/→** turn a switch off or on or step a slider, and **Escape** closes. A font row opens a list of installed families, each shown in itself: type to filter, **Enter** uses the one picked, **Escape** goes back; on the row, **←/→** step through the families and **Backspace** gives back the default. The monospace font is used for code, tool output and the terminal. Settings are saved in `~/.config/tj/settings.toml`.
- **Transparent sidebar**, as an app setting like the rest of the app family: off by default, with an opacity of 0.8.
- **Keybindings you can change.** Every app shortcut is listed on the Keybindings tab. Pick one and press **Enter** (or click its pencil), then press the new key; **Escape** cancels. **Backspace** leaves it without a key and **R** gives back its default. A key given to one command is taken from any other that had it, and the modal says so. Keys need Ctrl, Alt or Super (or are F-keys), so typing still works; it warns when a key such as Ctrl+R would no longer reach the terminal's shell. Only the changes are saved, under `[keybindings]` in `settings.toml`, and they apply at once. Tooltips show the current keys.
- **Moving between projects**, from the keyboard:

  | Keys | Action |
  |---|---|
  | `Ctrl+Alt+N` / `Ctrl+Alt+P` | Next / previous project (down / up the sidebar, wrapping round). A project comes back as you left it: the same thread, panes and tabs. |
  | `Ctrl+Shift+O` | A new thread in the project on show (its start screen) |
  | `Ctrl+Shift+G` | The project's GitHub tab: pull requests, reviews, merging, branches, pull and push, on the project's own account |
  | `Ctrl+Space` | Find a project or thread: a filtered list of every project with its threads under it. **↑/↓** move, **Enter** opens; a project opens a new thread's start screen in it, a thread opens that thread. A thread matches on its title or its project's name. |
  | `Ctrl+,` | Settings |

  These keys are taken from the terminal too, so `Ctrl+Space` no longer reaches the shell (change it in Keybindings if you need it there).
- **Smooth cursor**, in the terminal and in every text field (the prompt box, answers, renaming): it glides to each new place and blinks with a soft fade, solid while you type. Settings' Appearance tab has a **Cursor animation** slider (Off to 300 ms, 100 ms by default).
- **Panes and tabs**, like Vmux. The main area is the project's panes, split right or down, and every pane has its own tabs. A tab is the **agent**, a **terminal** or the **review**. Each project has one agent tab, and picking a thread in the sidebar swaps what it shows: the conversation and the status column change, and the terminals, splits and other tabs stay exactly where they were. So terminals belong to the project, not the thread, and a terminal can sit behind the agent as a background tab in the same pane. To work on two things at once, add a git worktree of the project as its own project. A new project starts with just its agent tab. While the project is a single pane its tabs sit in the top bar, like a browser's; once it's split, each pane shows its own tab strip when it has a second tab. Closing a tab goes back to the one used before; closing a pane's last tab folds its split; a shell that exits closes its tab. If the agent tab is closed, picking a thread opens it again in the focused pane. The layout and each terminal's directory are saved per project. Vmux's shortcuts work anywhere in the main area (all of them can be changed in Settings → Keybindings):

  | Keys | Action |
  |---|---|
  | `Ctrl+Shift+T` / `Ctrl+Shift+W` | New terminal tab / close tab |
  | `Ctrl+Alt+H` / `Ctrl+Alt+V` | Split right (side by side) / down |
  | `Ctrl+Alt+S/F/E/D` (or `Ctrl+Alt+←/→/↑/↓`) | Focus the pane left / right / above / below |
  | `Ctrl+Shift+A` | Focus the next pane |
  | `Ctrl+Shift+=` / `Ctrl+Shift+-` | Grow / shrink the pane (or drag the divider between panes; double-click it to even them out) |
  | `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab |
- **GNOME-style window**: no title bar; drag either header to move it, double-click to maximize. The sidebar header has **add project** on the left, **TJ** in the middle, and **settings** on the right; each project's **+** starts a new thread. The sidebar, the top bar and the status column are one frame, like a browser's (see-through with **Transparent sidebar**), with the content set into it under rounded top corners. The selected thread is a card at half the frame's opacity. In a single pane the top bar holds the pane's tabs: the one on show opens into the content below, the others float as pills, each with a little icon (the project's picture on the agent tab), and **+** after them opens a terminal. A split project's top bar shows `[badge] Project / Thread` instead. Every action is its own round button: at the far left, hide or show the sidebar (**Ctrl+Shift+S**, remembered across restarts); on the right, split right, split down, the status column and close. The conversation stays at a readable width in a wide window.
- **A start screen for new threads**, like T3 Code's: "What should we build in <project>?", a large prompt card with the account and mode menus, and the folder, branch and mode underneath. The first message switches to the conversation; the status column is there from the start, saying what it doesn't have yet.
- **Streaming replies** in markdown, with tool calls shown as cards. Each card's header has the tool's icon and name, a one-line summary (Bash's own description of its command, a path relative to the project, Read's line range) a button that adds the call to the prompt box, and a spinner while it runs, then a check or a cross. The button puts a chip above the prompt (× takes it out) and gives the prompt the keyboard; the next message then opens by naming that call (and, for Bash, its command), so you can ask Claude about it directly. Tools are coloured by family from the theme's terminal palette, so they follow Omarchy theme switches: reading and search blue, edits and writes yellow, web cyan, agents magenta, planning, scheduling, worktrees and the rest in the bright colours, and MCP tools as "server · tool". Shell calls (Bash, Monitor, ...) are drawn as a small grey terminal on near-black: the command after a `$`, then its output, red when it failed. Bash is syntax highlighted in the theme's terminal colours: the command as bash, JSON output as JSON, a diff by line (bold headers, cyan hunks, red and green changes), and otherwise error and warning lines in red and yellow. A Bash card shows at most 30 lines of command and output. When there's more, a line under it says how many lines are left out; click it ("Click to expand") to open all of it in a dialog over a shaded backdrop, where it scrolls (Escape or a click outside closes it). A long script keeps room for the first 10 lines of its output. Under the output the card says how long the command took ("Took 2.4s"), not counting the wait for your approval; the time is saved with the thread. Edit shows a line diff, Write the new file with line numbers, TodoWrite a checklist, a picture Claude reads (or an MCP tool returns) shows in its card at its own size up to 360 px tall, with its dimensions, and opens the card, and other tools their arguments and result (JSON laid out readably). Any other card's body scrolls once it reaches half the window. Pictures are saved beside the thread's transcript, like attached images, so the transcript stays small and they show again when the thread is reopened. Edit and Write cards start open; click a header to open or close its card. Bash cards are always open, and their header doesn't fold them.
- **Subagents.** When Claude hands work to a subagent (the Agent tool, which runs in the background), its row shows the agent type, a live "now doing" line, and its tool count and tokens; expand it for the subagent's tool calls and its report. The row stays "working" until the subagent finishes, and the turn Claude starts on its own to report back shows as working too.
- **Approval cards** when Claude wants to do something risky: **Approve**, **Approve for session**, **Decline** (with optional feedback that's passed to Claude) or **Cancel turn**.
- **Questions and plans.** When Claude asks you something (AskUserQuestion), a modal opens over the conversation with every question in it. The open question shows its options and an "Other" field for your own answer; answering it folds it to one line with your answer and opens the next. With several questions, a last step sends them all; a single question is sent as soon as you answer it. It all works from the keyboard: **↑/↓** move (down past the options into "Other"), **1–9** pick an option, **Space** ticks one in a multi-select, **Enter** chooses (or finishes a multi-select, or sends), **←/→** go to the previous or next question, and **Escape** leaves the "Other" field or closes the modal. A closed modal leaves the question waiting; its card in the conversation has **Answer…** to open it again and **Skip**. The modal takes the keyboard only when the prompt box had it (or nothing did), so a question never grabs keys you're typing into a terminal; click it to answer. When Claude presents a plan in plan mode, the card shows it formatted, with **Start, accepting edits**, **Start, asking first** (both leave plan mode) or **Keep planning** (with optional feedback).
- **Permission modes** for the thread: Ask, Accept edits, Auto, Plan and Full access.
- **Usage and cost**: the account's limits as meters (5-hour, weekly, monthly spend, or an enterprise spending limit), plus the cost of each turn and the whole thread.
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
- `--github` opens the project's GitHub tab at launch.
- `TJ_LIVE_REPO=~/Projects/X cargo test -- --ignored live_github` reads a real repository through `gh` (picks its account, lists merged pull requests, loads one).
- `--type 'text\r'` sends text to the terminal.
- `TJ_UI_FONT="Family"` sets the default UI font (an **Interface font** picked in Settings still wins).
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
