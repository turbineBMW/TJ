# TJ

A native GUI for coding agents, built in Rust on [gpui](https://github.com/zed-industries/zed) via [gpui-kit](https://github.com/longbridge/gpui-kit). In TJ, projects come first and threads live under them. Each project has a default agent and account, and you can switch temporarily. The window also has a terminal, a usage and task sidebar, and a diff review with a summary of each file.

Named after Tony Jay, who voiced Judge Claude Frollo.

## Status: spike

This checks the two risky parts before committing to gpui:

1. **A streaming markdown transcript**: a virtual list that follows the newest text, with a long history.
2. **An embedded terminal**: `alacritty_terminal` for the PTY and VT parsing, drawn with gpui text runs.

No agent is connected yet. The transcript streams a canned reply from `src/sample.rs`.

## Run

```bash
cargo run
```

- **Stream reply** streams the sample at about claude.ai speed (6 words every 100 ms).
- **Firehose** streams it at about 4,500 characters a second.
- **+50 replies** adds 100 finished messages, to test a long history.
- Click the terminal to focus it. Tab, Shift+Tab and Ctrl+C go to the shell. Ctrl+Shift+V pastes.

Test hooks for running without clicks:

- `--demo stream|firehose|history` starts a stream at launch.
- `--type 'text\r'` sends text to the shell.
- `TJ_UI_FONT="Family"` overrides the UI font.

## What the spike found

- **Streaming works well.** gpui-kit's `MessageScroller` follows the newest text. `TextViewState::push_str` parses in the background and fades streamed text in. Code blocks are highlighted by tree-sitter, and tables render. 102 messages plus a firehose stream stayed smooth. CPU was about 5% while streaming at a realistic speed and 0% when idle, in a debug build.
- **The terminal took about 570 lines** and handles colours, bold, underline, inverse, Nerd Font glyphs, resize, scrollback, the alt screen, and bracketed paste. Not done yet: mouse selection and copy, mouse reporting, a blinking cursor, and images (SIXEL/kitty).
- **gpui on Linux ignores the weight axis of variable fonts.** Adwaita Sans and gpui-kit's `.SystemUIFont` render **bold** as regular. TJ uses the desktop font only when it has a separate bold file, and falls back to Noto Sans otherwise.
- **A hidden window doesn't repaint** until something asks for a frame. The model keeps updating, so nothing is lost.
- gpui-kit 0.11 pins `gpui-pre` 0.3.14, a snapshot of Zed's gpui published by the gpui-kit author, not Zed's `gpui` 0.2.x on crates.io.

## Next

The long-term layout (GNOME-style with no title bar, a full-height sidebar that can be transparent, and terminal splits and tabs like Vmux) is in [ROADMAP.md](ROADMAP.md).

- Connect a real agent: `claude` over stream-json, or ACP, with one `CLAUDE_CONFIG_DIR` per account.
- Lay out projects, then threads, in a sidebar.
- Follow the Omarchy theme, including the terminal palette, which is currently hard-coded Tokyo Night.
