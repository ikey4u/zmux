# zmux

`zmux` is a cross-platform terminal multiplexer written in Rust. It brings local
and SSH terminal work into one keyboard-driven workspace explorer, backed by
lightweight client/server connections that keep sessions alive after detach.

The new UI replaces the flat tab bar with a left navigation tree:
`Machine → Workspace → Session → Window → Pane`. Former tabs live on as
Workspace nodes; they are not flattened into machines or confused with windows.

## Highlights

- Cross-platform terminal multiplexer implemented in Rust
- One navigation tree for local and SSH machines, with independent Workspaces
- NERDTree-inspired navigation, node operations, and built-in shortcut help
- Automatic remote session-tree discovery after connecting through zmux's SSH workflow
- Show/hide the sidebar without leaving the current terminal workflow
- Persistent machine and Workspace display names, separate from connection identities
- Background servers with clean attach/detach behavior and surviving sessions
- Built-in command mode and copy mode for interactive terminal work
- Working-directory-aware splits and practical default shortcuts

## Design: a workspace explorer, not a tab strip

Each level has a distinct responsibility:

| Level | Responsibility |
|-------|----------------|
| Machine | A local host or an explicitly connected SSH destination |
| Workspace | An independent zmux server/socket, containing one or more sessions; replaces the former client Tab |
| Session | A named collection of windows within a Workspace |
| Window | A terminal layout within a session |
| Pane | An individual terminal/shell in that layout (sometimes called a panel) |

For example, an expanded tree can look like this; labels are illustrative:

```text
MACHINE-01 (local)
├── WORKSPACE-01
│   └── SESSION-01
│       └── WINDOW-01
│           ├── PANE-01
│           └── PANE-02
└── WORKSPACE-02
    └── SESSION-02
        └── WINDOW-02
            └── PANE-01
MACHINE-02 (SSH)
└── WORKSPACE-01
    └── SESSION-01
        └── WINDOW-01
            └── PANE-01
```

Names may repeat across machines or Workspaces without identifying the same
session or pane. Opening a node targets its owning Workspace;
renaming a machine or Workspace changes its display label, not its SSH address
or server socket. Closing a Workspace detaches this client from it, while its
server and sessions remain available for reattachment. Closing a session,
window, or pane is a different operation and can end the processes inside it.

The sidebar is a navigation surface, not another terminal pane, and is **hidden
by default** on client startup. `Prefix+m` opens the same tree in a centered
popup without resizing terminal panes. `Esc`, `q`, a click outside, or
`Prefix+m` closes the popup; opening a node closes it and activates that target.
`Prefix+M` toggles the fixed left sidebar across the current client's Workspaces.
These shortcuts work globally, including when connected through SSH. Dismissing
the popup or toggling the fixed sidebar preserves unfinished command input.

In the fixed sidebar, `q` or `Esc` hides it and returns focus to the terminal.
Hiding either navigation surface does not disconnect machines
or close sessions. Only fixed-sidebar visibility changes resize the server viewport;
popup navigation overlays it and restores covered panes when dismissed.

Navigation borrows NERDTree's hierarchy-oriented workflow: `j/k` to select,
`l` to expand, `h`/`←` to collapse or select the parent, and `r`/`K` to rename
or delete a node (confirmation required). `H` opens the navigation shortcut reference.
See
[Machine Navigation](#machine-navigation) for the full key map.

## SSH: connect once, discover remote sessions automatically

The default Prefix is `Ctrl+a`. Start a local client with `zmux`, press
`Prefix+:`, then enter this **zmux command** (not a shell command):

```text
new -m user@server
```

An SSH config alias works too, for example `new -m production`.

1. zmux adds the destination as a peer Machine root and probes it asynchronously.
2. The system SSH client reads `zmux protocol-info` and checks the wire-version
   contract and required capabilities, including the stdio bridge.
3. zmux attaches through the bridge and negotiates again with the **running
   server** before sending commands or resizing panes. If the socket is absent,
   the bridge starts a server with an initial session named `0`.
4. Once attached, zmux automatically loads and displays that Workspace's
   sessions, windows, and panes under the remote Machine. There is no separate
   nested remote TUI to open or list of sessions to enter manually.
5. The tree refreshes in the background as sessions and layouts change. Select
   a remote node and use the same navigation, rename, close, split, and focus
   controls as for local work.

`Prefix+m` opens the floating explorer; `Prefix+M` shows the fixed sidebar.
`R` on the remote Machine
re-probes the connection. Unexpected disconnections retry with bounded
exponential backoff. Missing zmux/PATH, invalid protocol declarations, incompatible
wire versions, and missing capabilities stop automatic retries; fix the cause,
then press `R` or run `new -m` again. Deliberately closing a connection detaches it.
Local and remote machine/Workspace labels are saved in the same
[display-name configuration](#display-name-persistence).

### Requirements and discovery scope

Compatibility is independent of the application version. Both endpoints must
use the same protocol major, meet each other's minimum minor, and support each
other's required capabilities. Every socket/stdio connection enforces a handshake,
including control, frames, tree queries, `ls`, and `kill-server`. The current wire
protocol is **2.0**; unnegotiated legacy clients/servers are rejected, not silently
downgraded. Inspect the declaration with `zmux protocol-info` (or
`ssh host zmux protocol-info`). Updating the binary does not update an already
running server: save its work and restart it safely using its compatible old
client, or use a fresh Workspace socket. No automatic installation, server
replacement, or session termination occurs on incompatibility; `--clean` also
refuses a live socket. See [protocol design and upgrade rules](docs/protocol.md).

- The remote host needs a POSIX-compatible shell and a compatible `zmux` on
  the **non-interactive SSH command PATH**. zmux does not install itself remotely.
- SSH uses batch mode: authentication must already work without interactive
  password prompts, for example through keys or an SSH agent. Host-key trust
  must also be established. OpenSSH handles aliases, `ProxyJump`, `ProxyCommand`,
  `IdentityFile`, authentication, and host-key checking.
- The remote Workspace uses the client's base socket name: `default` for
  `zmux`, or `dev` when the client was started with `zmux -L dev`. Discovery
  includes all sessions in that remote Workspace, **not every server socket
  on the remote host**. Local `zmux a` can discover multiple local Workspaces.
- Automatic discovery begins after `new -m` connects. A plain `ssh host`
  typed inside a pane remains a normal shell command; it does **not** currently
  create a Machine node automatically. zmux also does not import every host
  from `~/.ssh/config` or modify remote shell startup files.
- Display names persist, but the remote connection list is not restored on
  client restart. Run `new -m` again to reconnect and reuse the saved labels.

If the remote node reports unavailable, verify authentication and the remote
command environment from your shell:

```sh
ssh user@server 'command -v zmux && zmux mux --help'
```

## Shortcuts

### Prefix Key

The default prefix key is `Ctrl+a`. All prefix shortcuts require pressing the
prefix key first, then the action key. Pressing `Ctrl+a` twice sends a literal
`Ctrl+a` to the current pane.

Enter command mode with `Prefix+:`, then type `h` or `help` (`:h` / `:help`)
to open the full zmux shortcut reference in a popup, including pane,
window, session, sidebar, copy-mode, prompt, mouse, and options controls.
Use `j/k`, arrows, PageUp/PageDown, or the mouse wheel to scroll, and
`g/G` or Home/End to jump to either end. `Esc`, `q`, or `H`
closes it and restores the terminal view.
The sidebar's bare `H` still opens its navigation-only help.

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
| `Prefix + H` | Set the current pane's cwd as Home for this Workspace only |
| `Prefix + ]` | Paste clipboard text, image, or files into the focused pane. Uses the OS clipboard when available, otherwise `zsync` |
| `Cmd+V` / `Super+V` | Paste through the same clipboard path when the terminal forwards the key to zmux (see Remote Clipboard below) |
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

### Machine Navigation

The local Machine is the first root, followed by explicitly connected SSH
machines. Workspace labels default to socket names. `zmux -L <name>` starts or
attaches a Workspace; `zmux a` discovers running local Workspaces, including
legacy tab server sockets. All levels use the same tree controls below.

| Shortcut / Action | Action |
|-------------------|--------|
| `Prefix + m` | Open/close the floating sidebar tree without resizing panes |
| `Prefix + M` | Toggle the fixed sidebar, hidden by default |
| `H` (sidebar focused) | Show the complete shortcut reference; `j/k`, PageUp/PageDown or mouse wheel scroll, `Esc` or `H` returns |
| `Prefix + h` | Move to the pane on the left; when already at the left edge, enter the machine tree |
| `Prefix + l` (tree focused) | Return focus to the terminal |
| `↑`/`↓` or `j`/`k` | Move through visible tree nodes |
| `←`/`h` | Collapse the selected node, or move to its parent |
| `→`/`l` | Expand the selected node, move to its first child, or open a leaf pane |
| `Enter` | Open the selected machine, workspace, session, window, or pane |
| `Home`/`g`, `End`/`G` | Jump to the first or last visible node |
| `r` | Rename any selected machine (including local/offline machines), workspace, session, window, or pane |
| `K` (also `d` / `Delete`) | Delete the selected node after confirmation; only `y/Y` confirms, `n`, `Esc`, or Enter cancels |
| `R` | Re-probe the selected machine, or refresh the active workspace |
| `q` or `Esc` | Close the navigation popup, or hide the focused fixed sidebar |
| Click `▸`/`▾` | Expand or collapse that branch |
| Click a node name | Connect or open that machine, workspace, session, window, or pane |

Deletion applies to every node level. Removing an SSH Machine removes its tree
node and detaches its Workspace connections without killing remote servers.
Removing a Workspace detaches it; sessions remain available for reattachment.
Deleting a Session, Window, or Panel ends the processes it owns. Existing guards
protect the local Machine root and the last Session/Window/Panel.

The tab bar and old tab commands remain removed; workspace nodes replace their
organization role. The old `Prefix+t/T/S` shortcuts are removed. Sidebar navigation
no longer binds `T`, `p/P`, `J`, Space, `o`, Tab, or `Prefix+j/k`; bare `K` now deletes.
Pane-direction shortcuts outside the sidebar are unchanged. Direction keys also work if Ctrl is still
held after the prefix (including Ctrl+h / Backspace).

#### Display-name persistence

Machine and workspace display names are persisted in `~/.config/zmux/machines.json` (or
`$XDG_CONFIG_HOME/zmux/machines.json`). When `ZMUX_CONFIG` is set, `machines.json`
is stored beside that configuration file. Renaming changes only the display
name, never the SSH destination. Names survive detach/restart; remote roots
are added again with `:new -m <SSH_HOST>` and reuse their saved names.

---

### Session Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + d` | Detach the current client. The server keeps running in the background and all panes stay alive |
| `Prefix + $` | Rename the current session, then press Enter to confirm or Esc to cancel |
| `Prefix + (` | Switch to the previous session |
| `Prefix + )` | Switch to the next session |
| `Prefix + :` | Enter command mode. Type a zmux command and press Enter to execute it, or Esc to cancel |

---

### Command-Line Usage (`zmux` executable)

| Command | Action |
|---------|--------|
| `zmux` | Start zmux. If a background server already exists, it attaches automatically |
| `zmux a` / `zmux attach` | Attach to running servers and show their sessions in the machine tree; use `--single` for only the selected socket |
| `zmux ls` / `zmux list-sessions` | List sessions for the base socket and any legacy derived server sockets |
| `zmux -L <name>` | Specify the server socket name, defaulting to `default` |
| `zmux -s <name>` | Specify the name of the new session |
| `zmux server` | Start the server in daemon mode. This is usually invoked automatically by zmux and does not need to be run manually |
| `zmux kill-server [SOCKET]...` | Stop one or more background servers. Without arguments it stops the current `-L` socket |
| `zmux kill-server --all` | Stop all discoverable background servers |

#### Commands supported in command mode (`Prefix + :`)

| Command | Action |
|---------|--------|
| `new -m <SSH_HOST>` | Add/connect an SSH machine in the navigation tree |
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
| `set-workspace-home` | Set the current pane's cwd as Home for this Workspace (same as `Prefix+H`) |
| `h`, `help` | Open the full shortcut popup in client command mode |
| `paste-cloud` | Same as `Prefix + ]`: paste clipboard contents into the focused pane |

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

### Remote Clipboard (zsync)

A headless Linux host has no OS clipboard. zmux uses [zsync](https://github.com/ikey4u/zsync) so copy and paste still work across the laptop and the server.

On both machines, run `zsync daemon`, then pair once (`zsync pair` on one side, `zsync connect <ticket>` on the other). After that:

- Copy mode `y` / `Enter` writes into zsync (and into the local OS clipboard when one exists). Over a nested SSH session without a display, this is the path that reaches the laptop.
- `Prefix + ]` reads the OS clipboard first; if that is empty (typical on the server), it runs `zsync p` in `~/.zmux/drop`.
- Synced text is pasted as text. For a synced image or file, zsync materializes the bytes under `~/.zmux/drop` and zmux pastes its shell-quoted, absolute Linux path. The private drop directory is also covered by zmux's 24-hour/1-GiB cleanup policy.
- `Cmd+V` / `Super+V` invokes the same operation when the terminal forwards that key to zmux using the enhanced keyboard protocol. `Prefix + ]` remains the terminal-independent fallback.
- Programs in a pane that emit OSC 52 (for example Neovim's osc52 provider) are still relayed to the attached terminal, and the decoded text is also copied into zsync.

macOS terminal applications normally reserve `Cmd+V` for their own text paste
action. That action cannot represent an image on the terminal byte stream, so
the remote TUI never sees a key or a paste event. Override the terminal binding
to forward `Cmd+V` to zmux instead. For example:

WezTerm (`~/.wezterm.lua`, merge the entry into your existing `config.keys`):

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

config.keys = config.keys or {}
table.insert(config.keys, {
  key = 'v',
  mods = 'CMD',
  action = act.SendKey { key = 'v', mods = 'SUPER' },
})
```

kitty (`kitty.conf`):

```conf
map cmd+v send_key cmd+v
```

For another terminal, map `Cmd+V` to the kitty keyboard-protocol sequence
`ESC [ 118 ; 9 u` (bytes `1b 5b 31 31 38 3b 39 75`). This represents
Super+V. Without such a mapping, normal text still follows the terminal's
native bracketed-paste path, while images should be pasted with `Prefix + ]`.

OSC 52 remains a fallback when zsync is not installed or the daemon is down. It needs a terminal that implements OSC 52 (WezTerm does; macOS Terminal.app does not). See the [OSC 52 terminal compatibility list](https://can-i-use-terminal.github.io/features/osc52copy.html).

For Neovim on the remote host, point the `+` register at zsync (daemon must be running):

```lua
vim.g.clipboard = {
  name = "zsync",
  copy = { ["+"] = { "zsync", "c" }, ["*"] = { "zsync", "c" } },
  paste = {
    ["+"] = { "zsync", "p", "--content" },
    ["*"] = { "zsync", "p", "--content" },
  },
  cache_enabled = 0,
}
vim.opt.clipboard = "unnamedplus"
```

`zsync p` prints a file path on a headless host; editors must use `--content`. Restart Neovim after changing the setting and confirm with `:checkhealth clipboard` / `"+y`.

---

### Keys Passed Through to the Shell

`Prefix+H` sets the active pane's current working directory as **Workspace Home**.
New sessions, windows, and split panes in that Workspace start there, even after
an existing shell changes directory. Other Workspaces (local or SSH) are unaffected;
existing shells are not moved and their `HOME` environment variable is unchanged.
Home is held by the Workspace server: it survives client detach/reattach, but not
server shutdown. Without Workspace Home, the existing cwd/window-start-directory
inheritance applies. The legacy `:set-pane-start-dir` remains window-scoped;
Workspace Home takes precedence when set. Global help is available via `:h` or
`:help`, while bare `H` inside the sidebar shows only navigation help.

The following keys are not intercepted by zmux and are passed directly to the shell
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

### Automated regression tests

Run `cargo test` for the full suite, or `cargo test --test tui` for the Unix
PTY integration tests. These drive the compiled client and server, decode ANSI
frames, and cover command editing, tree focus, machine/Workspace-name persistence,
Workspace switching and detach, default-hidden sidebar, floating tree node
operations/mouse/resize, global toggles and command-input restoration, help, window/session
switching, repeated split/close, refresh, and terminal resizing.
They check for lost pane content at completed synchronized frames and avoid
global screen clears. Test servers and configuration live in isolated temporary
directories; the runner must allow local PTYs and Unix sockets.

Remote tests replace SSH with a local shim while exercising the real stdio
bridge and a second isolated server. They do not validate real SSH networking,
authentication, Linux execution, font rendering, or every terminal emulator's
synchronized-update support.
