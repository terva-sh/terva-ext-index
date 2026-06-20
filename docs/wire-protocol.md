# Wire protocol (as implemented here)

What `terva-ext-index` actually speaks, and where the contract lives in the host
repo. This is the subset a [Rust SDK](rust-sdk-extraction.md) would encapsulate;
it is **not** the full protocol (commands, panels, event interception,
`host_tool_call`, context cards — none of which `index` uses).

> **Source of truth:** the terva repo, `terva.sh/terva`
> ([github.com/terva-sh/terva](https://github.com/terva-sh/terva)).
> - `docs/extensions.md` — the prose spec.
> - `packages/agent/extproto/extproto.go` — frame structs + constants.
> - `packages/agent/ext/ext.go` — the Go SDK; `Run()` is the canonical loop.
> - `packages/agent/tools/sandbox.go` — host path confinement we mirror.
> - `packages/core/policy.go` — `Authority` values.
>
> Line numbers drift; cite files and symbols, not addresses.

## Transport

Newline-delimited JSON, one object per line, over the extension's stdin/stdout.
terva launches the manifest's `exec` (`./run.sh`) as a subprocess.

- **stdout is the wire.** Every diagnostic byte goes to **stderr** (terva
  captures it to `$TERVA_HOME/logs/ext-index.log`). One stray stdout write
  corrupts the JSON stream — this is the single most important rule.
- **Frame caps** (`extproto`): `MaxFrameBytes` = 4 MiB both directions;
  `MaxToolCallBytes` = 1 MiB for the args the host puts in one `tool_call`. An
  oversized inbound frame is **skipped and logged, never fatal**. We keep our
  own output well under these (see the caps in `outline.rs`).
  - ⚠️ Gap: our `main.rs` read loop uses `BufRead::read_line`, which does not
    enforce the 4 MiB read cap the Go SDK applies (`extproto.ReadFrame`). A
    hostile/buggy host could stream an unbounded line. Low risk (the host is
    trusted), but an SDK should match `ReadFrame`.

## Handshake (ordering matters)

The extension emits **all** registration frames eagerly, *before* reading
`hello_ack` — matching the Go SDK's `ext.go` `Run()`, which sends `hello` →
`register_*` → `subscribe` → `ready` and only then enters its read loop. The
host buffers registrations until `ready`, so emitting before the ack is correct,
not a race.

Startup frames we send, in order:

```jsonc
{"type":"hello","name":"index","version":"0.3.0","capabilities":["tools"],"min_protocol":2}
{"type":"register_tool","name":"index","description":"…","schema":{…},
 "read_only":true,"authority":"local-read"}
{"type":"register_context","text":"Prefer `index` before `read`: …"}
{"type":"subscribe","events":["session_start"],"intercept":[]}
{"type":"ready"}
```

- **`read_only` + `authority`** — `authority:"local-read"` is a real value
  (`core.AuthLocalRead`; classified read-only by `IsReadOnlyAuthority`). A
  declared authority wins over the legacy `read_only` bool. Read-only tools are
  auto-admitted in `plan` / `auto-edit` approval modes. Lying here only cheats
  your own user's policy.
- **`min_protocol:2`** — the lowest host we require. We use the protocol-2
  `session_start` event to follow `cwd`; we declare 2 (not 3) because nothing
  else here needs protocol 3. Declaring more than you use refuses hosts you'd
  otherwise work with.

## Frames the host sends us

### `hello_ack` (once, right after `hello`)

```jsonc
{"type":"hello_ack","protocol_version":3,"terva_version":"…",
 "provider":"…","model":"…","cwd":"/abs/workspace",
 "extension_dir":"/abs/install/dir","data_dir":"/abs/$TERVA_HOME/ext-data/index"}
```

We capture `cwd`, `data_dir`, `extension_dir` into `HostDirs`.
- `extension_dir` — the **read-only install dir** (our code/assets).
- `data_dir` — the **writable state dir** (`$TERVA_HOME/ext-data/<name>`). `index`
  is stateless so it writes nothing, but the jail still includes it.
- `cwd` here is **frozen at launch**; it moves only via `session_start`.

### `event` → `session_start` (re-fires on every `/cd`)

```jsonc
{"type":"event","event":"session_start","cwd":"/abs/new/cwd","project_id":"…"}
```

We update `HostDirs.cwd` so the jail tracks the live working directory instead of
going stale on the launch cwd. (We `subscribe`d to `session_start`; other event
kinds — `turn_start`, `tool_call`, etc. — we don't request and ignore.)

### `tool_call`

```jsonc
{"type":"tool_call","id":"corr-1","name":"index","args":{"path":"src/foo.rs","depth":1}}
```

We validate/coerce `args` ourselves (the host forwards whatever the model
produced). Reply within the host's tool timeout (default 60 s) with:

```jsonc
{"type":"tool_result","id":"corr-1","content":[{"type":"text","text":"…skeleton…"}],"is_error":false}
```

`content[]` blocks are `{"type":"text",…}` or `{"type":"image",…}`; we only emit
text. Every refusal (unsupported, oversized, out-of-workspace, unreadable) is a
normal `is_error:true` result telling the model to fall back to `read` — never a
crash.

### `shutdown`

Reply `{"type":"shutdown_ack"}` and exit. EOF on stdin is also a clean exit.

## Self-jail (host policy we enforce ourselves)

Extensions confine their own filesystem access — and, unlike host-side tools,
there is **no unjail**. We only resolve paths inside `cwd ∪ data_dir ∪
extension_dir`. The confinement mirrors `tools/sandbox.go`: canonicalize
(symlink-resolve) the target and each root, then require the target to be a root
or a descendant. See [architecture.md](architecture.md) and `src/sandbox.rs`.

## What we deliberately don't implement

`register_command` / `command_invoked`, panels (`open_panel` / `panel_*`),
event **interception** (`event_intercept` / `_response`), `host_tool_call`,
`list_sessions` / `read_session`, `context_card` / `refresh_context` /
`status_segment`. `index` is one read-only tool plus a static context blurb. A
Rust SDK would grow into these as later extensions need them.
