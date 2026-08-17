# zmux

`zmux` is a cross-platform terminal multiplexer written in Rust. It is designed for fast, keyboard-driven terminal workflows with a lightweight client/server architecture and a focused feature set for everyday interactive use.

It currently supports pane, window, and session management, detach and attach workflows, command mode, copy mode, cwd-aware splits, and Vim-style pane navigation. The project aims to stay compact, predictable, and easy to extend while providing the core ergonomics expected from a modern terminal multiplexer.

## Highlights

- Cross-platform terminal multiplexer implemented in Rust
- Keyboard-first workflow for panes, windows, and sessions
- Background server model with clean attach and detach behavior
- Built-in command mode and copy mode for interactive terminal work
- Working-directory-aware splits and practical default shortcuts

## Shortcuts

### Prefix Key

The default prefix key is `Ctrl+a`. All prefix shortcuts require pressing the
prefix key first, then the action key. Pressing `Ctrl+a` twice sends a literal
`Ctrl+a` to the current pane.

---

### Pane Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + %` | Split the current pane horizontally into left and right panes |
| `Prefix + "` | Split the current pane vertically into top and bottom panes |
| `Prefix + x` | Close the current pane |
| `Prefix + z` | Maximize the current pane, or restore it when pressed again |
| `Prefix + K` | Completely clear the current pane output history, including copy mode history |
| `Prefix + b` | Toggle pane borders on or off |
| `Prefix + H` | Set the current pane's current directory as the working directory for future splits |
| `Prefix + ]` | Paste from the OS clipboard (text, image, or files). In a remote pane this uploads files to `~/.zmux/drop/` and pastes the remote paths |
| `Prefix + h` | Move focus to the pane on the left, in Vim style |
| `Prefix + j` | Move focus to the pane below, in Vim style |
| `Prefix + k` | Move focus to the pane above, in Vim style |
| `Prefix + l` | Move focus to the pane on the right, in Vim style |
| `Prefix + ←` | Move focus to the pane on the left |
| `Prefix + ↓` | Move focus to the pane below |
| `Prefix + ↑` | Move focus to the pane above |
| `Prefix + →` | Move focus to the pane on the right |
| `Prefix + hold Alt/Option`, then press `h` `j` `k` `l` repeatedly | Resize the active pane left, down, up, or right while `Alt/Option` remains held. The first `Alt/Option+h` / `j` / `k` / `l` applies immediately. If there is no resize input for 500 ms, the sequence ends automatically |

---

### Window Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + c` | Create a new window |
| `Prefix + n` | Switch to the next window |
| `Prefix + p` | Switch to the previous window |
| `Prefix + ,` | Rename the current window, then press Enter to confirm or Esc to cancel |

---

### Tab Operations

Each tab is backed by an independent server. Sessions, windows, and panes are isolated between tabs.

| Shortcut / Action | Action |
|-------------------|--------|
| `Prefix + t` | Open the tab chooser. Use `↑`/`↓` or `j`/`k` to move, `/` or `?` to search by tab code/title, `Ctrl+j`/`Ctrl+k` to move within search results while search is active, `R` to rename the selected tab, `Enter` to switch, and `q` or `Esc` to close |
| `Prefix + /` | Open a centered quick-switch input. Enter a two-letter tab code, then press `Enter` to switch directly to that tab. Hidden tabs are shown automatically before switching. If the code is invalid or not found, the input stays open with an error so you can re-enter it. Press `Esc` to cancel |
| `Prefix + Tab` | Switch to the next tab |
| `Prefix + Shift+Tab` | Switch to the previous tab |
| `Prefix + T` | Open the tab rename dialog. Use `Tab` to switch between the code and title fields. The two-letter code must be unique; press `Enter` to move from code to title and press `Enter` again to save, or `Esc` to cancel |
| `Prefix + w` | Close the current tab |
| Click a visible tab in the top tab bar | Switch to that tab |
| Click `...` in the top tab bar | Open the searchable tab chooser |

---

### Session Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + d` | Detach the current client. The server keeps running in the background and all panes stay alive |
| `Prefix + $` | Rename the current session, then press Enter to confirm or Esc to cancel |
| `Prefix + (` | Switch to the previous session |
| `Prefix + )` | Switch to the next session |
| `Prefix + s` | Open the interactive tree view of all sessions and windows. Use Enter to select, `j` or `k` to navigate, `l` to expand, `h` to collapse, and `q` or `Esc` to close |
| `Prefix + :` | Enter command mode. Type a zmux command and press Enter to execute it, or Esc to cancel |

---

### Command-Line Usage (`zmux` executable)

| Command | Action |
|---------|--------|
| `zmux` | Start zmux. If a background server already exists, it attaches automatically |
| `zmux new -t <title>` | Start zmux with an initial client-side tab title |
| `zmux a` / `zmux attach` | Attach to an existing background server. Client-side tab layout is not restored from another client |
| `zmux ls` / `zmux list-sessions` | List sessions for the base socket and its tab server sockets such as `<socket>.tab.*` |
| `zmux -L <name>` | Specify the base socket name, defaulting to `default`. New tabs use derived socket names like `<name>.tab.<pid>.<id>` |
| `zmux -s <name>` | Specify the name of the new session |
| `zmux server` | Start the server in daemon mode. This is usually invoked automatically by zmux and does not need to be run manually |
| `zmux kill-server [SOCKET]...` | Stop one or more background servers. Without arguments it stops the current `-L` socket |
| `zmux kill-server --all` | Stop all discoverable background servers |
| `zmux ssh <host>` | Attach a remote zmux over SSH. Inside a pane this replaces that pane with the remote window; outside zmux it opens a dedicated client tab |

#### Commands supported in command mode (`Prefix + :`)

| Command | Action |
|---------|--------|
| `new -t <title>` / `new-tab -t <title>` | Create a new tab backed by an independent server and set its title. Use `new -t ""` for an empty title |
| `select-tab -t <code\|index\|title>` | Switch to a client-side tab by code, zero-based index, or exact title |
| `rename-tab -c <code> -t <title>` | Rename the active tab's code and title. The code must be unique two uppercase letters |
| `next-tab` / `prev-tab` | Switch to the next or previous client-side tab |
| `list-tabs` | Show a compact summary of client-side tabs |
| `new -s <name>` | Create a new session and switch to it |
| `new -s <name> -d` | Create a new session in the background without switching to it |
| `kill-session` | Close the current session |
| `kill-session -t <name>` | Close the specified session |
| `rename-session <name>` | Rename the current session |
| `switch-client -t <name>` | Switch to the specified session |
| `rename-window <name>` | Rename the current window |
| `new-window` | Create a new window |
| `kill-window` | Close the current window |
| `split-window -h` | Split horizontally |
| `split-window -v` | Split vertically |
| `zoom-pane` | Maximize or restore the current pane |
| `clear-pane` | Completely clear the current pane output history |
| `set-option -g history-limit <lines>` | Set the in-memory scrollback limit for existing and future panes (`0`–`100000`) |
| `show-options` | Show the current server options |
| `set-pane-start-dir` | Save the current pane's current directory as the working directory for future splits |
| `paste-cloud` | Same as `Prefix + ]`: read the OS clipboard and paste into the focused pane |

---

### Remote Domain (`zmux ssh`)

`zmux ssh linux` attaches the remote zmux on that SSH host as a first-class pane, not a nested TUI. Prefix stays on the local client.

- Inside an existing pane: the current leaf becomes a remote slot after SSH and the first frame succeed. Failure keeps the original local shell.
- Split (`Prefix + %` / `"`) and window commands follow the focused pane's machine. `Prefix + h/j/k/l` moves across local and remote panes.
- File and image paste uses `Prefix + ]` (not Cmd+V). Remote files land in `~/.zmux/drop/` on the SSH host.
- Optional host settings live in `~/.config/zmux/ssh.toml`. Do not nest `ssh` + `zmux a` inside a pane.

---

### Copy Mode

| Shortcut | Action |
|----------|--------|
| `Prefix + [` | Enter copy mode |
| `q` / `Esc` | Exit copy mode |
| `h` `j` `k` `l` / arrow keys | Move left, down, up, or right |
| `b` | Move back to the beginning of the current or previous word |
| `w` | Move forward to the beginning of the next word |
| `e` | Move forward to the end of the current or next word |
| `0` / `Home` | Move to the beginning of the line |
| `$` / `End` | Move to the end of the line |
| `g` / `G` | Jump to the top of the currently loaded history window or to the bottom. Move/page upward again to load the next older disk page |
| `Ctrl+b` / `PageUp` | Scroll up one page |
| `Ctrl+f` / `PageDown` | Scroll down one page |
| `/` / `?` | Search forward or backward |
| `n` / `N` | Jump to the next or previous search result |
| `Space` / `v` | Start character selection |
| `V` | Start line selection |
| `Ctrl+v` | Start rectangular selection |
| `Enter` / `y` | Copy the current selection and exit copy mode |

Each pane keeps 2,000 lines of parsed scrollback in memory by default. Once the
hot history reaches that watermark, completed primary-screen lines are moved to
one private shared SQLite database instead of being discarded. Records are
isolated by an opaque pane-instance key, so panes, sessions, and independent
zmux servers cannot read one another's history. The cold tier retains up to
1,000,000 logical lines per pane and their colours; copy mode rewraps them to
the pane's current width. Alternate-screen redraws from programs such as Neovim
and gitui are never archived. SQLite writes use a bounded per-pane write-behind
worker, so ordinary disk transactions do not stall rendering and a storage
failure cannot create an unbounded in-memory retry queue.

Copy mode initially loads the newest 1,000 cold lines alongside the in-memory
history. Moving or paging upward at the top loads older pages on demand, so a
long-running pane does not materialize its entire history at once. The copy
snapshot remains stable while live output continues; resizing only rewraps the
loaded snapshot. Search operates on the pages currently loaded into copy mode.
`clear-pane`, terminal saved-history clears, and terminal resets clear both
tiers. Closing a pane removes only its records; the shared `zmux.sqlite3` file
remains in zmux's private state directory. SQLite WAL mode may also create
`zmux.sqlite3-wal` and `zmux.sqlite3-shm` sidecar files in that same directory.
A later run reclaims
records and legacy per-pane database files left by a crashed server on platforms
where zmux can reliably check process liveness. On other platforms cleanup is
conservative and may retain orphaned records rather than risk removing a live
server's history. The cold tier is scrollback rather than a session backup. If
the private state directory or disk becomes unavailable, the pane continues with
its configured in-memory history (2,000 lines by default).

Use `Prefix + :`, then `set-option -g history-limit <lines>` to change the hot
in-memory watermark for the current server (`0`–`100000`). Lowering it moves
eligible older lines to the cold tier. A single unfinished soft-wrapped logical
line can temporarily use a bounded internal burst area above the watermark. If
a program keeps one line open beyond that safety bound, zmux drops the oldest
part of that line rather than allowing memory to grow without limit.

---

### Remote Clipboard (OSC 52)

When zmux runs on a remote Linux server over SSH, it cannot directly call the
clipboard on the client machine. Instead, zmux sends copied text to the
terminal emulator running on the SSH client using the OSC 52 escape sequence.
That terminal emulator must support OSC 52. For example, WezTerm supports it;
macOS Terminal.app does not. See the [OSC 52 terminal compatibility
list](https://can-i-use-terminal.github.io/features/osc52copy.html) for other
supported terminals.

This OSC 52 path does not require Linux graphical clipboard tools such as
`xsel`, `xclip`, `wl-copy`, or a display server. You may still install and use
them for other clipboard workflows on the remote host.

Programs running inside a pane, including Neovim, emit their own terminal
output. zmux relays valid OSC 52 sequences from that output to the attached
terminal. Make sure the remote host runs a zmux version with this support and
restart the remote zmux server after upgrading.

For Neovim on the remote Linux host, configure the OSC 52 clipboard provider
early in `~/.config/nvim/init.lua`:

```lua
vim.g.clipboard = "osc52"
vim.opt.clipboard:append("unnamedplus")
```

Restart Neovim after changing the setting. `unnamedplus` makes ordinary `y`
use the `+` register; `vim.g.clipboard = "osc52"` is what makes that register
send its copied text through the terminal. You can verify the active provider
with `:checkhealth clipboard` and test explicitly with `"+y`.

---

### Keys Passed Through to the Shell

New panes and windows inherit the current working directory by default. Splits
follow the current pane's cwd unless one is explicitly set. After pressing
`Prefix + H`, future splits use the pinned working directory instead. The
following keys are not intercepted by zmux and are passed directly to the shell
or program running in the active pane. By default, zsh panes start with the
Emacs line editor keymap. On Windows, default PowerShell panes initialize
PSReadLine in Emacs mode when PSReadLine is available for cross-platform
consistency. Explicit shell commands and interactive programs are not modified
and may define their own bindings.

| Key | Effect in the Shell |
|-----|----------------------|
| `Ctrl+a` `Ctrl+a` | Send a literal `Ctrl+a`, which usually moves to the beginning of the line in shell editing |
| `Ctrl+b` | Move backward by one character |
| `Ctrl+c` | Interrupt the current foreground process with `SIGINT` |
| `Ctrl+d` | Delete the character under the cursor. On an empty line, it usually means EOF |
| `Ctrl+e` | Move to the end of the line in shell editing |
| `Ctrl+f` | Move forward by one character |
| `Ctrl+k` | Delete to the end of the line |
| `Ctrl+l` | Clear the screen |
| `Ctrl+n` | Go to the next history entry |
| `Ctrl+p` | Go to the previous history entry |
| `Ctrl+r` | Search command history backward incrementally |
| `Ctrl+s` | Search command history forward incrementally |
| `Ctrl+t` | Transpose the two characters around the cursor |
| `Ctrl+u` | Delete to the beginning of the line |
| `Ctrl+z` | Suspend the current foreground process with `SIGTSTP` on Unix-like shells. Windows shells do not provide Unix job-control suspension, so the key is passed through |
| Any other character or key combination | Pass through to the shell unchanged |

> Type `exit` or press `Ctrl+d` inside a pane to close that pane.  
> After the last pane is closed, the server daemon exits automatically and the client exits with it.  
> `Prefix + d` only detaches the current client. The server and all panes continue running in the background, and you can reconnect with `zmux a`.

---

### Notes

- If you press the prefix key and do not follow it with an action key, prefix mode stays active until the next key press.
- The prefix key itself will be configurable through a config file in the future.
- Mouse support and more configurable key bindings will be improved in future versions, and this document will be updated accordingly.
