# Zmux Shortcut Reference

## Prefix Key

The default prefix key is `Ctrl+a`. All prefix shortcuts require pressing the
prefix key first, then the action key. Pressing `Ctrl+a` twice sends a literal
`Ctrl+a` to the current pane.

---

## Pane Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + %` | Split the current pane horizontally into left and right panes |
| `Prefix + "` | Split the current pane vertically into top and bottom panes |
| `Prefix + x` | Close the current pane |
| `Prefix + z` | Maximize the current pane, or restore it when pressed again |
| `Prefix + K` | Completely clear the current pane output history, including copy mode history |
| `Prefix + b` | Toggle pane borders on or off |
| `Prefix + H` | Set the current pane's current directory as the working directory for future splits |
| `Prefix + h` | Move focus to the pane on the left, in Vim style |
| `Prefix + j` | Move focus to the pane below, in Vim style |
| `Prefix + k` | Move focus to the pane above, in Vim style |
| `Prefix + l` | Move focus to the pane on the right, in Vim style |
| `Prefix + ←` | Move focus to the pane on the left |
| `Prefix + ↓` | Move focus to the pane below |
| `Prefix + ↑` | Move focus to the pane above |
| `Prefix + →` | Move focus to the pane on the right |

---

## Window Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + c` | Create a new window |
| `Prefix + n` | Switch to the next window |
| `Prefix + p` | Switch to the previous window |
| `Prefix + ,` | Rename the current window, then press Enter to confirm or Esc to cancel |

---

## Session Operations

| Shortcut | Action |
|----------|--------|
| `Prefix + d` | Detach the current client. The server keeps running in the background and all panes stay alive |
| `Prefix + $` | Rename the current session, then press Enter to confirm or Esc to cancel |
| `Prefix + (` | Switch to the previous session |
| `Prefix + )` | Switch to the next session |
| `Prefix + s` | Open the interactive tree view of all sessions and windows. Use Enter to select, `j` or `k` to navigate, `l` to expand, `h` to collapse, and `q` or `Esc` to close |
| `Prefix + :` | Enter command mode. Type a zmux command and press Enter to execute it, or Esc to cancel |

---

## Command-Line Usage (`zmux` executable)

| Command | Action |
|---------|--------|
| `zmux` | Start zmux. If a background server already exists, it attaches automatically |
| `zmux a` / `zmux attach` | Attach to an existing background server |
| `zmux ls` / `zmux list-sessions` | List all current sessions |
| `zmux -L <name>` | Specify the socket name, defaulting to `default`, to run multiple independent servers at the same time |
| `zmux -s <name>` | Specify the name of the new session |
| `zmux server` | Start the server in daemon mode. This is usually invoked automatically by zmux and does not need to be run manually |

### Commands supported in command mode (`Prefix + :`)

| Command | Action |
|---------|--------|
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
| `set-pane-start-dir` | Save the current pane's current directory as the working directory for future splits |

---

## Copy Mode

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
| `g` / `G` | Jump to the top or bottom |
| `Ctrl+b` / `PageUp` | Scroll up one page |
| `Ctrl+f` / `PageDown` | Scroll down one page |
| `/` / `?` | Search forward or backward |
| `n` / `N` | Jump to the next or previous search result |
| `Space` / `v` | Start character selection |
| `V` | Start line selection |
| `Ctrl+v` | Start rectangular selection |
| `Enter` / `y` | Copy the current selection and exit copy mode |

---

## Keys Passed Through to the Shell

New panes and windows inherit the current working directory by default. Splits
follow the current pane's cwd unless one is explicitly set. After pressing
`Prefix + H`, future splits use the pinned working directory instead. The
following keys are not intercepted by zmux and are passed directly to the shell
or program running in the active pane. By default, zsh panes start with the
Emacs line editor keymap. If your shell configuration switches to another keymap,
your own configuration takes precedence.

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
| `Ctrl+z` | Suspend the current foreground process with `SIGTSTP` |
| Any other character or key combination | Pass through to the shell unchanged |

> Type `exit` or press `Ctrl+d` inside a pane to close that pane.  
> After the last pane is closed, the server daemon exits automatically and the client exits with it.  
> `Prefix + d` only detaches the current client. The server and all panes continue running in the background, and you can reconnect with `zmux a`.

---

## Notes

- If you press the prefix key and do not follow it with an action key, prefix mode stays active until the next key press.
- The prefix key itself will be configurable through a config file in the future.
- Mouse support and more configurable key bindings will be improved in future versions, and this document will be updated accordingly.
