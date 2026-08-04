# Architecture

`terva-ext-index` is a single read-only tool, `index`, that turns a source file
(or a directory) into a tree-sitter **skeleton** — imports plus signatures with
exact line ranges — so the model can understand structure before paying for a
full `read`. Since 0.8.0 it runs on **terva-extsdk**, the Rust extension
runtime this repo's hand-rolled loop was extracted into (see
[rust-sdk-extraction.md](rust-sdk-extraction.md) for that history).

## Pure logic + thin glue

The same split the Go extensions use: keep the domain logic unit-testable
**without** starting the extension or talking to a host.

```
src/outline.rs   PURE   tree-sitter walk -> skeleton string. No IO, no protocol.
src/main.rs      GLUE   declare the extension (schema, guidance, one handler);
                        path resolution, miss reporting, directory dispatch.
tests/wire.rs    E2E    spawns the built binary, drives the real protocol over stdio.
```

`outline.rs` is stdlib-plus-tree-sitter only and carries most of the
interesting invariants; that is where the bulk of the unit tests live.
`main.rs` holds only the `index` domain: the `Extension` declaration and
`handle_index` (resolve → jail → bounded read → outline, plus the path-miss
and directory-mode rendering). The wire itself — handshake, LF-JSON framing
with the 4 MiB cap, `session_start` cwd-following, panic-isolated dispatch,
the self-`Jail` — lives in the SDK, and `tests/wire.rs` still validates the
composed binary end-to-end over a pipe.

## The reuse boundary (realized as terva-extsdk)

This table was drawn when the loop was hand-rolled, as the lens for
[pulling out a Rust SDK](rust-sdk-extraction.md). The extraction happened:
every **SDK** row below now lives in the `terva-extsdk` crate
(`terva-sdk-rust` repo), and this extension kept only the **stays** rows.

| Code (pre-0.8.0) | Concern | Where it lives now |
|---|---|---|
| handshake (hello / register_tool / register_context / subscribe / ready) | protocol | **SDK** — `Extension::run` registers eagerly |
| read loop + frame parse + `hello_ack` / `event` / `tool_call` / `shutdown` dispatch | protocol | **SDK** — typed `extproto` frames, malformed calls answered |
| `send()` (newline-framed JSON, flush, all chatter to stderr) | protocol | **SDK** — plus the 4 MiB cap both directions, closing the old gap |
| `HostDirs` (capture `cwd`/`data_dir`/`extension_dir`, refresh on `session_start`) | host | **SDK** — `Host`, the Go SDK's `Host()` analog |
| `read_bounded` (TOCTOU-safe capped read) | host I/O | **SDK** helper |
| `sandbox.rs` `Jail` (canonicalize + confine to cwd/data/ext dirs) | host policy | **SDK** — lifted wholesale, tests included |
| tool **schema**, `TOOL_DESC`, `STANDING_CONTEXT`, `depth`/directory dispatch | `index` domain | **stays** (`main.rs`) |
| `src/outline.rs` (the whole tree-sitter walk) | `index` domain | **stays** |

## Data flow of a `tool_call`

```
host                              extension (this binary, on terva-extsdk)
 │  hello ───────────────────────◄ SDK emits hello, register_tool, register_context,
 │                                  subscribe(session_start), ready  (eager, pre-ack)
 ├─ hello_ack(cwd,data_dir,ext_dir) ─► SDK Host captures the dirs
 ├─ event:session_start(cwd) ──────► SDK refreshes Host.cwd (follows /cd)
 ├─ tool_call(path, depth) ────────► handler: resolve_path(cwd) → Jail.resolve
 │                                    (canonicalize, confine) → read_bounded →
 │                                    outline_with_depth
 │  tool_result(skeleton) ◄────────  one text block (is_error on any refusal)
 ├─ shutdown ──────────────────────► SDK answers shutdown_ack, exits
```

Directory requests branch at the `is_dir` check into `handle_directory`: a
bounded, deterministic walk (`Walk::collect`) then a scale-based renderer
— full skeletons for a package, a flat file map for a larger tree, or a
child-directory rollup for a very large one (progressive disclosure, so a big
root is a cheap complete map rather than a firehose of skeletons). Everything
else about the loop is identical.

## The SDK

This was the **first Rust terva extension**, and its hand-rolled loop plus the
table above became the extraction blueprint for `terva-extsdk`. Migrating back
onto the SDK closed the loop's known gaps by construction: the 4 MiB read cap,
`min_protocol` validated against the ack, error answers for malformed and
unknown tool calls, and panic-isolated dispatch. See
[wire-protocol.md](wire-protocol.md) for the exact contract and
[rust-sdk-extraction.md](rust-sdk-extraction.md) for how the extraction was
planned (kept as the historical record).
