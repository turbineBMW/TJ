# TJ

A native GUI for coding agents, built in Rust on [gpui](https://github.com/zed-industries/zed) via [gpui-kit](https://github.com/longbridge/gpui-kit). In TJ, projects come first and threads live under them. Each project has a default agent and account, and you can switch temporarily. The window also has a terminal, a usage and task sidebar, and a diff review with a summary of each file.

Named after Tony Jay, who voiced Judge Claude Frollo.

## Status

TJ talks to a real Claude Code session in one thread, next to a terminal:

- **Streaming replies** in markdown, with tool calls shown as cards you can expand to see their output.
- **Approval cards** when Claude wants to do something risky: **Approve**, **Approve for session**, **Decline** (with optional feedback that's passed to Claude) or **Cancel turn**.
- **Permission modes** for the thread: Ask, Accept edits, Auto, Plan and Full access.
- **Usage and cost**: subscription usage windows (for example "7d 4%"), plus the cost of each turn and the whole thread.
- **Stop** interrupts a running turn. If `claude` exits, the next message resumes the conversation.

Projects, the thread sidebar, accounts, the task panel and the diff review are next; see [ROADMAP.md](ROADMAP.md).

## Run

```bash
cargo run                        # Claude in the current directory, with your default account
cargo run -- --model haiku       # pick a model
```

The prompt box sends on Enter; Shift+Enter adds a line. Tab never moves focus. Click the terminal to focus it; Tab, Shift+Tab and Ctrl+C go to the shell, and Ctrl+Shift+V pastes.

Test hooks:

- `--prompt "text"` sends a prompt at launch.
- `--replay tests/fixtures/claude/2.1.331-approve.jsonl` plays a recorded session through the real parser, without running a model.
- `--demo stream|firehose|history` streams a canned reply (`history` adds 100 messages first).
- `--type 'text\r'` sends text to the terminal.
- `TJ_UI_FONT="Family"` overrides the UI font.

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
- gpui-kit 0.11 pins `gpui-pre` 0.3.14, a snapshot of Zed's gpui published by the gpui-kit author, not Zed's `gpui` 0.2.x on crates.io.
- The terminal handles colours, bold, underline, inverse, Nerd Font glyphs, resize, scrollback, the alt screen and bracketed paste. Not done yet: mouse selection and copy, mouse reporting, a blinking cursor, and images.
