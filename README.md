# zmux

`zmux` is a cross-platform terminal multiplexer written in Rust. It is designed for fast, keyboard-driven terminal workflows with a lightweight client/server architecture and a focused feature set for everyday interactive use.

It currently supports pane, window, and session management, detach and attach workflows, command mode, copy mode, cwd-aware splits, and Vim-style pane navigation. The project aims to stay compact, predictable, and easy to extend while providing the core ergonomics expected from a modern terminal multiplexer.

## Highlights

- Cross-platform terminal multiplexer implemented in Rust
- Keyboard-first workflow for panes, windows, and sessions
- Background server model with clean attach and detach behavior
- Built-in command mode and copy mode for interactive terminal work
- Working-directory-aware splits and practical default shortcuts

## Documentation

- [Shortcut Reference](docs/SHORTCUT.md)
