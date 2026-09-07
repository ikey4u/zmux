# zmux protocol compatibility

## Scope and ownership

Application releases and wire versions are independent. `zmux --version` is for
diagnostics; it is never used as a compatibility gate. `zmux protocol-info`
prints one bounded JSON declaration without opening a socket or starting a
server. `src/ipc/protocol.rs` is the single source of truth for both discovery
and live negotiation.

The enforced protocol begins at **2.0**. Historical `ZMUX 1` constants were not
used by the live command transport; those peers are legacy/unnegotiated and
are intentionally rejected. There is no silent fallback to the legacy format.

The declaration includes `product`, negotiation `schema`, wire `major`, `minor`,
`min_peer_minor`, supported `capabilities`, `required_capabilities`, and a
diagnostic `application_version`. Unknown JSON fields are allowed for additive
evolution; missing required fields, malformed declarations, duplicate capability
names, oversized messages, and unsupported schemas are rejected.

## Compatibility contract

Both endpoints independently apply the same checks:

1. Product and negotiation schema must match.
2. Wire major versions must match exactly.
3. Each endpoint's minor version must meet the other's `min_peer_minor`.
4. Each endpoint must support all capabilities required by the other.
5. The selected minor is the smaller offered minor. The capability set is the
   sorted intersection. A client rejects a server selection that differs from
   its independently calculated result.

Current required capabilities are `control-v1`, `frame-json-v1`,
`session-tree-v1`, and `workspace-home-v1`. SSH discovery additionally requires
`ssh-stdio-v1`. These names describe explicit wire contracts, not installed
third-party tools; clipboard availability is still checked at operation time.

Within a major version, minor additions must preserve old message meanings,
field defaults, and commands. Optional new behavior must be capability-gated;
raising `min_peer_minor` requires tests proving why older peers cannot operate.
Incompatible changes to framing, command semantics, or required JSON fields
require a **major bump**. Do not merely bump the application release or advertise
a capability before its implementation exists. This release implements only
wire 2.0; negotiation does not magically provide adapters for older formats.

## Live handshake

Every physical connection starts with a newline-terminated exchange:

```text
client -> server: ZMUX HELLO <ProtocolInfo JSON>
server -> client: ZMUX WELCOME {"peer": <ProtocolInfo>, "negotiated": <selection>}
              or ZMUX REJECT {"code": "...", "message": "..."}
```

Only after WELCOME may the client send ATTACH/size, FRAME?, INPUT, commands,
SESSION_TREE, OPTIONS, COPY_YANK, LIST, or KILL_SERVER. The server rejects an
unnegotiated first command before dispatching it or resizing panes. Control,
frame, read-only tree polling, and one-shot management connections all use this
gate, including reconnections. Headers are bounded at 8 KiB and reads have
transport timeouts. Buffered data after the handshake remains available to the
business-protocol reader. An open connection retains its negotiated contract
for its lifetime; its peer cannot change versions mid-stream.

Unix sockets and Windows named pipes use the same handshake. SSH stdio is a
transparent transport for that exchange, not an alternative protocol. The
remote executable is checked before launching the bridge, **and the running
server is checked again over the actual stream**. Replacing a remote binary
does not upgrade an already-running server.

Negotiation is a compatibility check, not authentication. Local IPC permissions
and SSH host-key verification/authentication remain the trust boundary.

## Failure handling and upgrades

Missing remote zmux/PATH, missing protocol-info, invalid declarations, version
mismatches, and missing capabilities are permanent until configuration or
software changes. The machine remains visible with an error, without automatic
probe retries. `R` or `:new -m host` explicitly retries after repair. SSH transport
failures retain capped exponential retries. Reconnecting tree streams stop
retrying on a protocol rejection.

A compatibility failure must never delete a live socket, start a replacement
server over it, kill sessions, or silently report “no server”. `--clean` also
refuses a live socket: use another `-L` name or explicitly stop the server after
saving work. Automatic launch is limited to absent/refused sockets, not protocol,
permission, or timeout errors. There is no automatic installation or upgrade.

To upgrade legacy deployments safely:

1. Save work in old sessions and arrange their shutdown using the old compatible
   client; the new client deliberately cannot issue legacy kill commands.
2. Install compatible client/server binaries (including SSH non-interactive PATH).
3. Restart the old server when safe, or create an independent Workspace with a
   fresh `-L` name. Do not unlink a live server's socket to force an upgrade.
4. Check `zmux protocol-info` on both machines, then reconnect. Live handshake
   errors, rather than binary metadata alone, determine final compatibility.

Regression coverage must include major/minor/capability matrices, malformed and
fragmented handshakes, read-ahead preservation, old/new peers in both directions,
unnegotiated mutation rejection, management channels, stale running servers,
SSH failure classification, and preservation of existing sockets/sessions.
