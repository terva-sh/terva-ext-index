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

Pass a **directory** instead of a file and `index` maps it with **progressive
disclosure** — the same structure-before-body idea as the file level, one level
up. What comes back scales with the tree, so `index <root>` is always a cheap,
complete map and you drill in only where you look:

- **A package (≤ 30 supported files)** — the full skeleton of every file, each
  headed by its path relative to the directory plus a `(lines, size)` hint so
  you can tell which files are cheap to `read` without a separate `wc -l`/`ls -l`:

  ```
  $ index src
  src — 3 supported files

  main.rs (412 lines, 11.3 KB)
    imports: ...
    [59-65]  fn main()
    ...
  outline.rs (965 lines, 33.1 KB)
    imports: tree_sitter::{Node,Parser,Tree}
    [36-47]  pub enum Lang
    ...
  sandbox.rs (120 lines, 3.4 KB)
    ...
  ```

- **A larger tree (≤ 300 supported files)** — a **file map**: one line per file
  with its `(lines, size)`, no skeleton bodies. Every file is listed (nothing is
  silently dropped); `index` a file or subdirectory to expand it to skeletons.

  ```
  $ index .
  . — 118 supported files in 22 directories
  (map only — `index <file>` or `index <subdir>` for skeletons)

  cmd/terva/args.go (612 lines, 18.4 KB)
  cmd/terva/main.go (88 lines, 2.1 KB)
  ...
  ```

- **A very large tree (> 300 supported files)** — a **rollup**: one line per
  immediate child directory with its file count and total size, so a monorepo
  root is a ~1–2 KB index. `index <subdir>` descends into any group.

  ```
  $ index .
  . — 955 supported files in 48 directories
  (rolled up — `index <subdir>` to descend)

  cmd/                          14 files  120.0 KB
  docs/                         40 files  260.0 KB
  packages/                    620 files    5.9 MB
  ...
  ```

The map and rollup tiers ignore `depth` (a file/directory listing has no
per-file body to deepen), and the rollup reads no file bodies at all — its sizes
come from the walk's `stat`, so even a huge tree renders in a couple of KB.
`depth` keeps its per-file meaning on the skeleton tier.

The walk skips hidden entries (`.git`, `.venv`, …), heavy/generated trees
(`node_modules`, `target`, `vendor`, `dist`, `build`, `__pycache__`, …), and
symlinks (so it can't loop or escape the workspace), and is deterministic (files
are sorted). Only names that are *always* build output are skipped — `bin` is
deliberately not among them, since Rust binary crates live in `src/bin/`. CI
directories (`.github`, `.gitlab`, `.circleci`) are walked despite being hidden:
`.github/workflows/*.yml` is supported YAML and often the only description of
how a project builds and releases. The skeleton tier keeps a safety backstop of
at most 200 files / 256 KB / 20k directory entries visited; the map tier reads
at most 8 MB computing line counts (past that a file shows `?` lines, with a
note saying how many and why — its size is still exact, since sizes come from
the walk's `stat`), and the rollup tier reads nothing at all.

If the walk does exhaust its 20k-entry budget, the header says so and renders
its counts as `N+` — a partial index is never presented as a complete one:

```
. — 19999+ supported files in 1+ directories — walk stopped at 20000 entries,
so this is a LOWER BOUND; index a subdirectory for a complete view
```

## Supported languages

Picked by file extension — or by filename, for the formats that have no usable
one (`.env`, `Makefile`, `Dockerfile`):

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
| Shell      | `.sh`, `.bash`, `.zsh`, `.ksh`               |
| Makefile   | `Makefile`, `GNUmakefile`, `.mk`             |
| Dockerfile | `Dockerfile`, `Dockerfile.*`, `Containerfile`, `.dockerfile` |
| XML        | `.xml`, `.xsd`, `.xsl`, `.xslt`, `.plist`, `.csproj`, `.props`, `.targets`, `.wsdl` |
| HTML       | `.html`, `.htm`, `.xhtml`                    |
| CSS        | `.css`                                       |
| Markdown   | `.md`, `.markdown`                           |
| JSON       | `.json`, `.jsonc`                            |
| YAML       | `.yaml`, `.yml`                              |
| TOML       | `.toml`                                      |
| INI        | `.ini`, `.cfg`, `.conf`                      |
| dotenv     | `.env`, `.env.*`, `*.env`                    |
| CSV/TSV    | `.csv`, `.tsv`                               |
| logs/text  | `.log`, `.txt`                               |

Shell scripts (`.sh`, `.bash`, `.zsh`, `.ksh`, via tree-sitter-bash) outline to
their **function definitions**, with `source foo.sh` / `. foo.sh` treated as the
imports line. Top-level assignments are listed as **names only** — a shell
script is where `export API_TOKEN=…` actually lives — which also means a linear
script with no functions at all still outlines to something useful: its knobs.

```
deploy.sh
  imports: ./lib/common.sh, ./lib/log.sh
  [6-6]    REGISTRY=
  [7-7]    API_TOKEN=
  [10-12]  build()
  [14-16]  push()
```

For a **Makefile** the skeleton is its **targets**, each spanning to the line
before the next one so a follow-up `read` lands on that recipe — this is where
an index earns the most, since a long Makefile is a flat list of dozens of
targets that is otherwise a `grep`. Recipe bodies (tab-indented) are never
emitted, and variables show names only. For a **Dockerfile** the `FROM` lines
are the top level (a multi-stage build's stages) with each stage's instructions
nested under it; `ENV` and `ARG` show names only, since a build arg is a classic
place for a token.

**XML** and **HTML** outline to their **element tree** — the same shape as the
JSON/YAML key hierarchy, since a markup file's structure *is* its nesting. An
element shows its tag, its attributes and either a short text value or a child
count; text and attribute values follow the same elide-and-redact rules as
JSON/YAML, because a `pom.xml`, `settings.xml` or `web.config` is a place
credentials genuinely live.

```
pom.xml
  [1-14]   <project xmlns="http://maven.apache.org/POM/4.0.0"> (5 children)
    [2-2]    <artifactId> index
    [5-8]    <properties> (2 children)
      [6-6]    <db.password> <redacted>
```

HTML additionally lifts `<script src>` and `<link href>` into the imports line,
and looks *through* `<html>`, `<head>` and `<body>` — they carry no structure of
their own, and nesting everything under them would make `depth: 1` show nothing
but two wrappers. So a page's landmarks land at the top level:

```
index.html
  imports: /assets/app.css, /assets/app.js
  [4-4]    <title> Demo
  [9-11]   <header id="top" class="site-header"> (1 child)
  [12-14]  <main id="app"> (1 child)
```

For **CSS** the skeleton is its **selector list** — one line per rule set with
the range of the whole rule, so a follow-up `read` lands on that block, with
`@media` / `@supports` / `@keyframes` nesting their rules underneath and
`@import` going to the imports line. Property declarations are not emitted; the
selectors are what you look a stylesheet up by.

For Markdown the "skeleton" is the document's **headings** — a table of
contents. Each heading's range spans its whole section (down to the next
heading of equal-or-higher level), so a follow-up `read` lands on exactly that
section. `depth` controls how many heading levels deep the TOC goes (`depth: 0`
= top headings only, `depth: 1` = + one level, …), and `index docs/` gives a
project-wide doc map in one call.

For JSON (and JSONC) the skeleton is the **key hierarchy**: the root value, then
each key on its own line carrying the line range of its `key: value` entry — so
a follow-up `read` lands on exactly that entry. Arrays report their length
(`[array, N items]`) rather than expanding, a nested object collapses to
`{object, N keys}` until you raise `depth`, and a long string value is elided to
its size (`"<string, 1.2 KB>"`) so a base64 blob or inlined cert never enters the
skeleton. JSONC comments and trailing commas are tolerated.

YAML (`.yaml`/`.yml`) uses the same key-hierarchy shape over block/flow
mappings: a mapping value shows `{N keys}`, a sequence shows `[N items]`, and
multiple `---` documents are split with a `--- document N` line.

**Values under a secret-looking key name are redacted** — in JSON, YAML and XML
alike — whatever their length:

```
database: {3 keys}
  password: <redacted>
  aws_secret_access_key: <redacted>
  host: db.internal
```

The size-based elision above it is a token-budget heuristic, not a secret guard
— an AWS secret key is exactly 40 characters and slipped straight through it —
so the key *name* decides instead. Names are split on `_`, `-`, `.` and
camelCase and matched token-wise against `password`, `secret`, `token`, `key`,
`auth`, `credential`, … so `apiKey` and `AWS_SECRET_ACCESS_KEY` are caught while
`author` and `monkey` are not. Booleans, `null` and plain numbers are never
redacted (they hide nothing and `auth_enabled: <redacted>` helps no one), and a
redaction carries **no size**, since a length leaks entropy about the secret.
The line keeps its exact range, so a caller that genuinely needs the value can
`read` it. Matching is deliberately eager: a false positive costs one
uninformative line, a false negative puts a live credential in the transcript.

A value can also betray itself regardless of its key, because a connection
string carries its own credentials and nobody agrees on what to call it — .NET
says `connectionString`, Heroku says `DATABASE_URL`. So a value holding an
embedded `password=` / `secret=` / `token=` pair, or a URI with credentials in
its userinfo (`postgres://user:pw@host`), is redacted on shape alone. An
ordinary URL is not: `https://example.com/docs` and `postgres://db:5432/app`
both pass through, since redacting every URL would gut the outline.

For TOML the skeleton is its **sections + key names**: `[section]` and
`[[array.of.tables]]` headers at the top level, each section's keys nested one
level under it (and bare keys above the first section at the top level). Only
key **names** are shown, never values — so an embedded token or path can't leak,
and `index Cargo.toml` is a compact map of the manifest. INI / `.cfg` / `.conf`
use the same sections + key-names shape via a small line scanner (best-effort
for non-INI `.conf` dialects; the line ranges are exact regardless).

For `.env` / dotenv files the skeleton is the **variable names only** — the part
left of the first `=`, each with its line range. Values are **never** emitted
(not even a type or length), since a `.env` commonly holds secrets; a
`(values omitted …)` note marks the omission as deliberate. Matched by filename
(`.env`, `.env.local`, …) and by the `.env` extension, the dotfile members show
up in a directory walk despite the usual hidden-file skip. A `.env` that looks
binary is refused with a `use read` message rather than emitting noise.

CSV / TSV files get a **summary** rather than a per-line outline: the header row
with its column count, a newline-based `~N rows` estimate, and the detected
delimiter — enough to decide whether and where to `read` without materializing
the rows. (`~rows` is a raw line count, not an RFC-4180 row count.)

Logs and plain text (`.log`, `.txt`) are summarized too: the line and byte
counts, the first and last non-blank lines with their line numbers, and a scan
for a few severity tokens (`ERROR`, `WARN`, `FATAL`, `panic`, `Traceback`) with
counts and the first `ERROR` line — so the model can `read` straight to a
failure. A binary file (NUL bytes) is refused rather than summarized as noise.

For any other file type — a file over ~2 MB, one that can't be read, or one
**outside the workspace** (see [Sandbox](#sandbox)) — the tool returns an
**error** result telling the model to fall back to `read` (it does not try to
outline what it can't parse).

A path that **does not exist** is the one case where `read` cannot help: it
fails with the same `ENOENT` one call later. So `index` answers it instead —
it names the deepest directory above the path that does exist and lists what is
in it, capped at 50 entries with an `… and N more` tail:

```
index: no such path src/mainn.rs — `mainn.rs` is not in src/, which contains:
main.rs  outline.rs  sandbox.rs
```

Missed paths are usually structurally right and filename-wrong, so the listing
is normally the answer. It stays an `is_error` result — the request did not
produce an outline, and the host's stall detector keys off that flag.

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
emits `hello`, `register_tool` (read-only, `authority: local-read`, and
`essential` — so a host with lazy tool visibility keeps `index` advertised
instead of deferring it behind an activation round-trip the standing guidance
below would have already sent you past),
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
