# Pulling a Rust SDK out of `index`

> **Status: implemented.** The SDK exists — `terva-extsdk`, in the
> `terva-sh/terva-sdk-rust` repo — and `index` migrated onto it in 0.8.0
> (`src/sandbox.rs` and the hand-rolled loop deleted; `tests/wire.rs` stayed
> green across the refactor). The trigger was not a second extension but the
> Matrix *connector*: extension protocol 5 carries a connector role, so the
> connproto and extproto SDKs belong to one family, and this repo's reuse
> boundary became the extension half's blueprint. The extraction landed
> almost exactly as sketched below (builder + closures, sync, MVP rows only,
> the protocol gaps closed by construction); the sketch's `ToolCall` helpers
> and `Host`/`Jail` shapes survived nearly verbatim. Kept as the historical
> record — section links to `sandbox.rs` and the loop describe the pre-0.8.0
> tree.

`terva-ext-index` is the **first Rust terva extension**. The Go extensions share
`terva.sh/terva/packages/agent/ext`; there is no Rust equivalent, so this one
hand-rolls the protocol loop. Most of that loop is not `index`-specific — see the
[reuse boundary](architecture.md#the-reuse-boundary-realized-as-terva-extsdk).
This doc sketches the SDK that boundary implies and a low-risk path to it.

Nothing here is committed to yet — it's the design we'll evaluate when a **second**
Rust extension makes the duplication real. One data point isn't a library.

## What lifts, what stays

From this repo, essentially unchanged:

- **`src/sandbox.rs`** — the `Jail` (canonicalize + confine to cwd/data/ext
  dirs). Every file-touching extension needs exactly this. Lifts wholesale.
- **`main.rs` protocol loop** — handshake, frame parse/dispatch, `send()`,
  `HostDirs` (capture dirs, follow `session_start`), `read_bounded`. ~70% of
  `main.rs`.

Stays in the extension:

- **`src/outline.rs`** — the tree-sitter walk. Pure `index` domain.
- The tool **schema**, `TOOL_DESC`, `STANDING_CONTEXT`, and the `depth`/directory
  dispatch in `handle_index` — the ~30% of `main.rs` that is actually `index`.

## Proposed shape: a `terva-ext` crate

Mirror the Go SDK's name (`ext`) and ergonomics. A sketch — **illustrative, not
final** — of what `index` would become:

```rust
use terva_ext::{Extension, Authority, ToolResult, Host};

fn main() -> std::io::Result<()> {
    Extension::new("index", env!("CARGO_PKG_VERSION"))
        .min_protocol(2)                       // RequireProtocol(2): session_start
        .context(STANDING_CONTEXT)             // register_context
        .follow_cwd()                          // subscribe(session_start) + refresh
        .tool("index", TOOL_DESC, schema(), Authority::LocalRead, index_tool)
        .run()                                 // blocks until EOF/shutdown
}

// host gives jailed file access + the live dirs; the handler is pure index logic.
fn index_tool(call: ToolCall, host: &Host) -> ToolResult {
    let depth = call.arg_u64("depth").unwrap_or(1).min(8) as usize;
    let path  = call.arg_str("path").unwrap_or_default();
    let canon = match host.resolve_in_jail(path) {       // Jail lives in the SDK
        Ok(p) => p,
        Err(e) => return ToolResult::error(e.fallback_message()),
    };
    // ... read_bounded + outline_with_depth (the index-specific part) ...
    ToolResult::text(skeleton)
}
```

The SDK owns: the wire loop, framed JSON I/O (with the 4 MiB `ReadFrame` cap the
current loop is missing — see [wire-protocol.md](wire-protocol.md)), the
handshake, `Host` (`cwd`/`data_dir`/`extension_dir`, refreshed on
`session_start`), `Jail`, `read_bounded`, and `ToolResult`. The extension owns
its schema, descriptions, and handler.

### Mapping to the Go SDK

The Go `ext.Extension` surface, and what the Rust SDK needs **now** vs. later:

| Go SDK | Rust SDK (MVP) | When |
|---|---|---|
| `ext.New(name, ver)` / `Run()` | `Extension::new().run()` | **now** (`index` uses it) |
| `e.Tool(name, desc, schema, fn, ReadOnly())` | `.tool(.., Authority, handler)` | **now** |
| `e.ContributeContext(text)` | `.context(text)` | **now** |
| `e.RequireProtocol(n)` | `.min_protocol(n)` | **now** |
| `Host().CWD / DataDir / ExtensionDir` + `session_start` refresh | `Host` + `.follow_cwd()` | **now** |
| `tools/sandbox.go` confinement | `Jail` / `host.resolve_in_jail` | **now** (already written) |
| `e.Command` / `command_invoked` | `.command(..)` | when an extension needs a slash command |
| `e.OnSession`, `Host().ProjectID`, `ProjectDataDir` | `.on_session(..)` | when an extension needs per-session/-project state |
| `PushContextCard` / `ClearContextCard`, `SetStatus` | context cards | when an extension wants live per-turn context |
| `OpenPanel` / `RenderPanel` / `OnPanelKey` | panels | when an extension ships UI |
| `InterceptToolCall` / `TurnStart` / `AssistantMessage` | interceptors | for a guardrail extension |
| `host_tool_call`, `list_sessions` / `read_session` | host-tool / session reads | for orchestration / indexing extensions |

`index` needs only the **now** rows. Build those first; add the rest when a real
consumer appears, not speculatively.

## Extraction plan

1. **Carve the crate.** Add a workspace member (e.g. `crates/terva-ext/`) holding
   `sandbox.rs` + framed JSON I/O + the handshake/run loop + `Host` + `ToolResult`.
   Typed frame structs (serde) mirroring `extproto`, rather than the ad-hoc
   `serde_json::Value` probing `main.rs` does today.
2. **Refactor `index` onto it.** `main.rs` shrinks to registration + the handler;
   `outline.rs` is untouched. The existing `tests/wire.rs` is the regression net —
   it must stay green across the refactor.
3. **Close the protocol gaps** the hand-rolled loop has: enforce the 4 MiB read
   cap (`ReadFrame`), don't `expect()` in `send()`, validate `min_protocol`.
4. **Grow by demand.** Add `command` / `on_session` / context cards / panels /
   interceptors only when a second extension needs them, keeping names aligned
   with the Go SDK so the mental model transfers.
5. **Promote + pin.** Once the API stabilizes, move the crate to its own repo and
   pin it (git rev or crates.io) the way the Go extensions pin `terva.sh/terva`;
   path-dependency for co-development, pinned for release.

## Open decisions

- **Home.** Start as a workspace member here (fastest, dogfooded immediately),
  then promote? Or live in the terva repo next to the Go SDK (versioned with the
  host protocol, but cross-language tooling in a Go module)? Or a standalone
  `terva-ext-rs` repo from day one? Recommendation: workspace-member → standalone.
- **API style.** Builder + closures (sketch above) vs. a `Tool` trait. Closures
  read well for small tools; a trait helps when handlers carry state.
- **Sync vs async.** The loop is blocking and `index` is happy sync. `host_tool_call`
  (an extension calling host tools mid-handler) is the first thing that might want
  async. Recommendation: sync until a consumer forces otherwise.
- **Typed frames.** Define serde structs mirroring `extproto` (vs. `Value`
  probing). Strongly recommended — it's where the wire contract gets enforced.
- **Versioning/dev loop.** Mirror the Go model (pin for release, path/replace for
  dev). A Rust `release.just` analog would pin the SDK crate and drop any path dep
  — deferred until we actually publish (see the repo's `import? 'release.just'`).

## See also

- [architecture.md](architecture.md) — the reuse boundary in this codebase.
- [wire-protocol.md](wire-protocol.md) — the exact contract the SDK codifies.
- terva's `packages/agent/ext/ext.go`
  ([github.com/terva-sh/terva](https://github.com/terva-sh/terva)) — the Go SDK
  to mirror.
- [terva-sh/terva-extension-template](https://github.com/terva-sh/terva-extension-template)
  — the Go extension template; a Rust template would follow once the SDK exists.
