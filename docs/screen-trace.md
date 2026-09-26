# Screen flicker trace

New zmux processes record a metadata-only trailing trace by default. The trace
contains timestamps, pane IDs, alternate-screen and synchronized-output state,
server frame sizes and clear decisions, and client draw decisions. It does not
contain terminal text or pasted input.

Files are written under the process's temporary directory:

- `zmux-screen-trace-server-<pid>.log`
- `zmux-screen-trace-client-<pid>.log`

Each file rolls at 4 MiB, retaining the preceding segment as `.log.prev`.
Trace files older than three days are removed when a new trace starts. After a
flash, note the time and save the current and `.prev` files for the relevant
server and client before they roll again. The timestamp at the start of each
line is Unix time in microseconds; a `dropped=` field reports records skipped
when the background writer queue was full.

`ZMUX_TRACE_SCREEN_MODES=/path/to/trace.log` sets the server trace path. The
client writes `/path/to/trace.log.client-<pid>.log` so the processes can roll
independently. Set `ZMUX_TRACE_SCREEN_MODES=0` to disable tracing. These settings
take effect when the server or client process starts.
