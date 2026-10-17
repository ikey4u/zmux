# zmux-core

Reusable zmux terminal host. The default feature set is headless; `tui` adds the
standalone client's rendering and native clipboard support. The CLI and this API
share PTY creation, the terminal state engine, frame recovery and history storage.

```rust,no_run
use std::{path::Path, time::Duration};
use zmux_core::{Environment, TerminalRuntime};
# fn example() -> std::io::Result<()> {
let environment = Environment::at("/var/lib/my-app/zmux", "my-worker")?;
let runtime = TerminalRuntime::new(environment)?;
let id = runtime.open(Path::new("/var/lib/my-app/workspace"), 80, 24)?;
// No connected viewer is needed for the shell to keep running.
let output = runtime.pull(&id, 0, Duration::from_millis(500))?;
// output.data is binary ANSI painting data, not a JSON or Base64 envelope.
let next_offset = output.offset;
runtime.resize(&id, 100, 30)?;
// Input is ordered through a bounded queue; errors must be surfaced, not retried blindly.
runtime.input(&id, b"pwd\r")?;
runtime.close(&id);
# Ok(())
# }
```

`TerminalRuntime` holds an exclusive host lock in its environment. A second
runtime for the same root/ID is rejected; different environments run concurrently
in one process. Each opened terminal owns a separate zmux view, so its input,
active selection and dimensions cannot affect another terminal. IDs cannot be
used through a different runtime. Clients detaching/dropping subscriptions have
no effect on shell ownership. Closing the terminal or dropping the runtime ends
its processes. The embedded API does **not** promise survival across its host
process or device restart; a persistent application must keep a separately
supervised host process alive if that is required.

Output is derived from zmux's authoritative screen state. Missing frame history
uses a repaint snapshot, rather than replaying an arbitrary truncated raw ANSI
stream. `pull` exposes byte offsets, truncation and closed state for existing
adapters; retained output is bounded. It is a blocking API: asynchronous hosts
should call it on a blocking executor. Clipboard or terminal replies must not be
automatically echoed as shell input by display adapters.

`Environment::enter` adapts existing synchronous APIs and restores the previous
scope on return or unwind. `environment::spawn` / `spawn_named` propagate that
scope to library threads. Do not hold an `enter` scope across an async `.await`;
the headless runtime scopes each operation itself. `server::InProcessServer`
also offers `start_in` and uses its stored environment for subsequent calls.
Library code does not terminate the hosting process or install global signal/
panic handlers; those choices belong to the standalone executable.
