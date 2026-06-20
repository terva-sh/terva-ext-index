# terva-ext-index

A [terva](https://github.com/terva-sh/terva) extension that emits a file's
**skeleton** — imports plus type/function/class signatures with their exact
line ranges — instead of its body, so the model can understand a file's
structure cheaply before spending tokens on a full `read`.

It registers a single read-only tool, `index`, and speaks the raw terva
extension wire protocol (newline-delimited JSON over stdin/stdout) directly from
Rust — no Go SDK. Parsing is done with [tree-sitter](https://tree-sitter.github.io/).

## Example

```
$ index ext.go
ext.go
  imports: terva.sh/.../extproto, time, sync, strings, fmt, encoding/json, ...
  [195-195]  func Text(s string) ToolContent
  [268-316]  type Extension struct
  [393-408]  func New(name, version string) *Extension
  [620-629]  func (e *Extension) Tool(name, description string, schema json.RawMessage, fn ToolHandler, opts ...ToolOption)
  [993-1248] func (e *Extension) Run() error
```

Line ranges are exact (1-based, inclusive — straight from tree-sitter node
rows), so you can follow up with `read offset/limit` on just the part you need.
On a real 1367-line Go file this skeleton is **~88% fewer tokens** than a full
read.

### Depth

By default `index` shows top-level declarations plus one level of members
(methods inside a type/impl/class). Pass `depth` to change that:

- `depth: 0` — top-level declarations only (the most compact map).
- `depth: 1` — the default.
- `depth: 2`+ — members of members, e.g. methods of an `impl` nested in a
  `mod`, or inner classes' methods.

The output cap still applies, so a deep descent on a large file truncates with
a marker rather than running away.

### Directory mode

Pass a **directory** instead of a file to map a whole package in one call —
every supported source file under it (recursively), each headed by its path
relative to the directory:

```
$ index src
src — 3 supported files

main.rs
  imports: ...
  [59-65]  fn main()
  ...
outline.rs
  imports: tree_sitter::{Node,Parser,Tree}
  [36-47]  pub enum Lang
  ...
sandbox.rs
  ...
```

The walk skips hidden entries (`.git`, `.venv`, …), heavy/generated trees
(`node_modules`, `target`, `vendor`, `dist`, `build`, `__pycache__`, …), and
symlinks (so it can't loop or escape the workspace). Output is deterministic
(files are sorted) and bounded: at most 200 files / 256 KB / 20k directory
entries visited, with a `(showing N; index a subdirectory for the rest)` marker
when a tree is larger. `depth` applies per file.

## Supported languages

Picked by file extension:

| Language   | Extensions                                  |
|------------|---------------------------------------------|
| Rust       | `.rs`                                        |
| Go         | `.go`                                        |
| Python     | `.py`, `.pyi`                                |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs`               |
| TypeScript | `.ts`, `.mts`, `.cts`                        |
| TSX        | `.tsx`                                       |
| Java       | `.java`                                      |
| C          | `.c`, `.h`                                   |
| C++        | `.cc`, `.cpp`, `.cxx`, `.hpp`, `.hh`, `.hxx` |
| Ruby       | `.rb`                                        |
| Markdown   | `.md`, `.markdown`                           |

For Markdown the "skeleton" is the document's **headings** — a table of
contents. Each heading's range spans its whole section (down to the next
heading of equal-or-higher level), so a follow-up `read` lands on exactly that
section. `depth` controls how many heading levels deep the TOC goes (`depth: 0`
= top headings only, `depth: 1` = + one level, …), and `index docs/` gives a
project-wide doc map in one call.

For any other file type — a file over ~2 MB, one that can't be read, or one
**outside the workspace** (see [Sandbox](#sandbox)) — the tool returns an
**error** result telling the model to fall back to `read` (it does not try to
outline what it can't parse).

Skeletons are also bounded: the imports list caps at 40 entries and the
declaration list at 500, each with a `(+N more)` / `… (output truncated …)`
marker, and any single signature line is clipped to 200 characters. A huge
generated file can't blow up the model's context (or brush the host's 1 MiB
tool-result / 4 MiB frame limits) — the marker tells the model to `read` the
rest.

## Build / test

```bash
cargo build --release   # produces target/release/terva-ext-index
cargo test              # unit tests (per-language fixtures + error paths) + a
                        # wire smoke test that drives the real protocol loop
```

### Toolchain note

Building from source needs a **Rust toolchain** (`cargo`, via
[rustup](https://rustup.rs)) and a **C compiler** (`cc`/`clang`/`gcc`) on `PATH`:
the tree-sitter grammar crates compile a small amount of C. `run.sh` builds the
binary on first launch (and after any source change).

If `cargo` isn't available — or the build fails — `run.sh` **falls back to
downloading** the prebuilt binary for the host from the latest GitHub release
(Linux and macOS, x86_64 and arm64), verified against its published `.sha256`.
So a toolchain-less host still works, as long as it can reach the release (or a
mirror set via `TERVA_EXT_INDEX_RELEASE_BASE`). The binaries are built by
[`.github/workflows/release.yml`](.github/workflows/release.yml).

The tree-sitter crate versions are pinned to a mutually ABI-compatible set
(`tree-sitter` 0.25 with grammar crates on the matching `LANGUAGE: LanguageFn`
const ABI). Bump them together.

## Install

```bash
terva ext install /path/to/terva-ext-index    # or a git URL once published
```

terva runs `./run.sh`, which builds the release binary on first launch (and
after any source change), or downloads the latest prebuilt binary if there's no
Rust toolchain, then execs it. No platform-specific binary is committed.

## Layout

```
extension.json   manifest (name "index", version, exec "./run.sh")
run.sh           launcher: build from source (or download the latest prebuilt
                 binary if cargo is unavailable) on first launch, then exec
Cargo.toml       pinned tree-sitter + grammar crate versions (ABI-matched)
src/main.rs      protocol loop: handshake, read frames, dispatch tool_call,
                 session_start cwd-refresh, shutdown (thin glue; stdout is the
                 wire, all chatter to stderr)
src/sandbox.rs   path confinement (the self-jail); pure + unit-tested
src/outline.rs   the pure tree-sitter walk -> skeleton (unit-tested in isolation)
tests/wire.rs    end-to-end: pipe hello_ack + tool_call into the built binary,
                 assert the tool_result frame (incl. jail + session_start cases)
```

The crate version, `extension.json`'s `version`, and the in-code `VERSION`
constant are kept equal by the `version_matches_manifests` unit test — bump all
three together on release.

## Sandbox

Like every terva extension, `index` **self-jails** — and unlike a host-side
tool, there is no `/unjail`. It only outlines files inside the workspace (the
session `cwd`) or the extension's own `data_dir` / `extension_dir`, all learned
from `hello_ack`. A path that resolves outside those roots — whether absolute,
via `..`, or through a symlink that escapes the workspace — comes back as an
error pointing the model at `read` instead.

The confinement mirrors terva's own `tools/sandbox.go`: the target and each
root are canonicalized (symlinks resolved), then the target must be a root or a
descendant. If the agent genuinely needs a file outside the project, that's a
deliberate act through the host `read` tool (which *can* be unjailed) or by
copying the file in — never something `index` does implicitly.

Because the jail follows `cwd`, the extension `subscribe`s to `session_start`
and updates its root on every `/cd`, so confinement tracks the live working
directory instead of the launch-time one.

## Protocol

Speaks terva extension protocol **v2+** (declares `min_protocol: 2` — the
lowest host that re-fires `session_start`, which the jail relies on to follow
`/cd`; `index` otherwise uses only base read-only-tool features). On startup it
emits `hello`, `register_tool` (read-only, `authority: local-read`),
`register_context` (the "prefer index before read" standing guidance),
`subscribe` (for `session_start`), and `ready` — all eagerly before
`hello_ack`, matching the Go SDK. It then reads frames and handles `hello_ack`
(capturing `cwd` / `data_dir` / `extension_dir`), `event` (`session_start`
cwd-refresh), `tool_call`, and `shutdown`.

## Documentation

- [docs/architecture.md](docs/architecture.md) — module map and the
  reusable-vs-`index`-specific boundary.
- [docs/wire-protocol.md](docs/wire-protocol.md) — the terva extension protocol
  as implemented here (the contract reference).
- [docs/rust-sdk-extraction.md](docs/rust-sdk-extraction.md) — sketch + plan for
  pulling a Rust SDK out of this first Rust extension.
