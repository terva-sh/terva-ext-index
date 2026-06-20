# Architecture

`terva-ext-index` is a single read-only tool, `index`, that turns a source file
(or a directory) into a tree-sitter **skeleton** — imports plus signatures with
exact line ranges — so the model can understand structure before paying for a
full `read`. It speaks the terva extension wire protocol directly from Rust,
with no SDK (there isn't a Rust one yet — see
[rust-sdk-extraction.md](rust-sdk-extraction.md)).

## Pure logic + thin glue

The same split the Go extensions use: keep the domain logic unit-testable
**without** starting the extension or talking to a host.

```
src/outline.rs   PURE   tree-sitter walk -> skeleton string. No IO, no protocol.
src/sandbox.rs   PURE   path confinement (the self-jail). No protocol.
src/main.rs      GLUE   wire loop: handshake, frame dispatch, tool_call -> result.
tests/wire.rs    E2E    spawns the built binary, drives the real protocol over stdio.
```

`outline.rs` and `sandbox.rs` are stdlib-plus-tree-sitter only and carry all the
interesting invariants; that is where the unit tests live (38 of them).
`main.rs` is deliberately thin — parse a frame, call into the pure modules, emit
a frame — and is validated end-to-end by `tests/wire.rs` driving the real binary
over a pipe.

## The reuse boundary (what a Rust SDK would lift)

This is the lens that matters for [pulling out a Rust SDK](rust-sdk-extraction.md):
which code is **protocol/host boilerplate** (identical in any extension) versus
**`index`-specific domain logic**.

| Code | Concern | Reusable? |
|---|---|---|
| `main.rs` handshake (hello / register_tool / register_context / subscribe / ready) | protocol | **SDK** — every extension does this |
| `main.rs` read loop + frame parse + `hello_ack` / `event` / `tool_call` / `shutdown` dispatch | protocol | **SDK** |
| `main.rs` `send()` (newline-framed JSON, flush, all chatter to stderr) | protocol | **SDK** (plus the 4 MiB frame cap, which we don't yet enforce — see wire-protocol.md) |
| `main.rs` `HostDirs` (capture `cwd`/`data_dir`/`extension_dir`, refresh `cwd` on `session_start`) | host | **SDK** — this is the Go SDK's `Host()` |
| `main.rs` `read_bounded` (TOCTOU-safe capped read) | host I/O | **SDK** helper |
| `src/sandbox.rs` `Jail` (canonicalize + confine to cwd/data/ext dirs) | host policy | **SDK** — every file-touching extension needs the self-jail |
| `main.rs` tool **schema**, `TOOL_DESC`, `STANDING_CONTEXT`, `depth`/directory dispatch | `index` domain | **stays** |
| `src/outline.rs` (the whole tree-sitter walk) | `index` domain | **stays** |

Roughly: `sandbox.rs` lifts wholesale, `main.rs` splits ~70/30 boilerplate vs.
`index` wiring, and `outline.rs` is entirely the extension's own.

## Data flow of a `tool_call`

```
host                              extension (this binary)
 │  hello ───────────────────────◄ emit hello, register_tool, register_context,
 │                                  subscribe(session_start), ready  (eager, pre-ack)
 ├─ hello_ack(cwd,data_dir,ext_dir) ─► HostDirs captures the dirs
 ├─ event:session_start(cwd) ──────► HostDirs.cwd refreshed (follows /cd)
 ├─ tool_call(path, depth) ────────► resolve_path(cwd) → Jail.resolve (canonicalize,
 │                                    confine) → read_bounded → outline_with_depth
 │  tool_result(skeleton) ◄────────  emit one text block (is_error on any refusal)
 ├─ shutdown ──────────────────────► shutdown_ack, exit
```

Directory requests branch at the `is_dir` check into a bounded, deterministic
walk (`collect_source_files` → per-file `outline_with_depth`); everything else
about the loop is identical.

## Why no SDK (yet)

This is the **first Rust terva extension**. The Go extensions share
`terva.sh/terva/packages/agent/ext`; there is no Rust equivalent, so the
protocol loop is hand-rolled here. That is fine for one small read-only tool —
but the table above is most of an SDK already. See
[wire-protocol.md](wire-protocol.md) for the exact contract and
[rust-sdk-extraction.md](rust-sdk-extraction.md) for the extraction plan.
