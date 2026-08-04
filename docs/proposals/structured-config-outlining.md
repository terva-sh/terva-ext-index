# Proposal: key-outlining for structured-config / data formats + per-file size hints in directory mode

Status: **implemented** — the whole proposal shipped. §4 (directory size hints)
in v0.4.0 alongside §3.1 JSON/YAML and §3.2 TOML; the §3 line scanners
(`.env`, INI, CSV/TSV, logs/text) in v0.5.0. Two later changes are worth knowing
about when reading the design below: JSON/YAML values under a secret-looking key
name — or values that betray themselves, like a connection string — are now
**redacted**, not merely elided by length (see `README.md`); and the backend fork
of §3.0 landed simpler than sketched, as a fallthrough in `outline_with_depth`
(try `Lang::from_extension`, else `LineFormat::from_extension`, else
unsupported) rather than a `Format` enum wrapper.
Audience: the agent maintaining `terva-ext-index`
Scope: `src/outline.rs` (the pure walk) and `src/main.rs` (directory mode + dispatch + `STANDING_CONTEXT`)

This note is self-contained: it carries the context an implementer needs without
reading the terva core repo or the harness test report that prompted it.

---

## 0. Background — how `index` works today (so the proposal lands in the real code)

`terva-ext-index` is a single read-only terva tool, `index`. It turns a file (or
a directory) into a compact **skeleton** — a header line plus one line per
declaration, each carrying its **exact 1-based inclusive line range** so the
model can follow up with `read offset/limit` on just the slice it needs. That
line-range-as-a-cheap-pointer is the whole point of the tool, and this proposal
preserves it for the new formats.

The pieces you will touch:

- **`src/outline.rs`** — pure, IO-free, unit-tested. The dispatch surface is the
  `enum Lang` (one variant per supported language, currently `Rust, Go, Python,
  JavaScript, TypeScript, Tsx, Java, C, Cpp, Ruby, Markdown`) and its three
  methods:
  - `Lang::from_extension(ext) -> Option<Lang>` — the **only** place file types
    are recognized. If it returns `None`, the file is "unsupported."
  - `Lang::name(self) -> &'static str` — human label for stderr diagnostics.
  - `Lang::ts_language(self) -> tree_sitter::Language` — the grammar.
  The entry point is `outline_with_depth(display_name, ext, source, max_depth)`,
  which checks `MAX_FILE_BYTES` (2 MB), resolves the `Lang`, parses with
  tree-sitter, and calls `render(...)`. `render` writes the header line, an
  optional `imports:` line, then iterates a `DeclSink` of `DeclLine { start_row,
  end_row, indent, text }` printing `  {[start-end]:<10} {text}`. `DeclSink`
  enforces `MAX_DECL_LINES` (500) and sets a `truncated` flag that prints a
  `… (output truncated …)` marker.

- **Markdown is the precedent to mirror.** Markdown is *not* outlined through the
  generic `collect_decls` body-descent path. `render()` special-cases it:

  ```rust
  if lang == Lang::Markdown {
      collect_markdown(root, src, max_depth, &mut sink);
  } else {
      // generic tree-sitter declaration walk
  }
  ```

  `collect_markdown` gathers headings, rebuilds the hierarchy from heading
  levels, and pushes one `DeclLine` per heading whose `end_row` spans the whole
  section (down to the next equal-or-higher heading) — so a follow-up `read`
  lands on that section's body. **The new structured formats follow this exact
  shape**: a dedicated collector that fills the same `DeclSink` with the same
  `DeclLine`s carrying real line ranges, behind the same per-format branch in
  `render()`. You get `MAX_DECL_LINES` capping, the truncation marker, the range
  column, and indentation for free.

- **`src/main.rs`** — the thin wire glue. `handle_index` resolves+jails the path,
  bounded-reads it, and calls `outline_with_depth`. Directory requests branch to
  `handle_directory`, which walks the tree (`collect_source_files`, gated on
  `Lang::from_extension(ext).is_some()`), then for each file **inlines its full
  skeleton** under a relative-path header. The standing guidance string
  `STANDING_CONTEXT` and the tool description `TOOL_DESC` both enumerate the
  supported types and must be updated when the set grows. `extension.json`,
  `Cargo.toml`, and `const VERSION` carry the version and are kept in lockstep by
  the `version_matches_manifests` test — bump all three on release.

---

## 1. Problem

`index` supports ~10 programming languages plus Markdown. It does **not** outline
the structured-config / data / log formats that fill a real repository:

- JSON (`.json`, also `package.json`, `tsconfig.json`, `*.jsonc`)
- YAML (`.yaml`, `.yml` — CI configs, k8s manifests, `docker-compose`, Helm)
- TOML (`.toml` — `Cargo.toml`, `pyproject.toml`, `netlify.toml`)
- INI / config (`.ini`, `.cfg`, `.conf`, `.toml`-adjacent)
- `.env` / dotenv files
- CSV / TSV data
- plain text (`.txt`) and log files (`.log`)

For a **coding agent** these are as common as code — they are the manifests,
lockfiles, pipeline definitions, and fixtures it reads constantly. Today
`Lang::from_extension` returns `None` for every one of them, so `index` refuses
with "unsupported file type … Fall back to `read`," and the model pays for a full
`read` (optionally paging with `offset/limit`, but it has to read first to know
where to page).

## 2. Why it matters (token economics)

The README measures the existing skeleton at **~70–90% fewer tokens** than a full
read on real source files. The same logic applies — often more strongly — to
config and data:

- A 400-line CI YAML or a 600-key `tsconfig`/`package.json` costs a few thousand
  tokens to read in full. A **key outline with line numbers** is a few hundred
  bytes, and it tells the model the one path it actually wants
  (`jobs.test.steps`, `compilerOptions.paths`) and the exact line to `read`.
- A 50k-line `.log` or a 10k-row `.csv` is *unreadable* in one `read` and forces
  blind `offset/limit` paging. A structural summary (line/row count, header,
  first/last lines, detected error markers) is a handful of lines and lets the
  model target its paging.
- For `.env`, a full `read` **leaks secrets into the transcript**. A
  **key-names-only** outline gives the model the variable inventory it needs to
  reason about config without ever surfacing a value.

The cost of *not* doing this is not just tokens: it is an extra `bash wc -l` /
`grep` round-trip to figure out *whether* a config is even worth reading, and a
secret-leak hazard on `.env`.

## 3. Design — one collector per format, all filling `DeclSink`

Each format gets a collector that mirrors `collect_markdown`: push `DeclLine`s
with **real `start_row`/`end_row`** (0-based, as the rest of the code uses;
`render` adds 1 when printing). Reuse `clip()` for over-long lines and let
`DeclSink`/`MAX_DECL_LINES` cap. The header line and range column are produced by
the existing `render`/`DeclLine` machinery in all cases.

**Two output contracts — name them, because they are not the same shape:**
- **Outline formats** — JSON, YAML, TOML, INI. One ranged line per key/section;
  every emitted line is a `read`-able pointer, preserving the §0 invariant.
- **Summary formats** — CSV, logs, plain text. A fixed structural digest (counts,
  header, first/last line) with **no** per-line range. This is a deliberate,
  different contract — reflected in the AC split in §6.

**Uniform read-pointer rule.** Every format's output — outline, summary, the
empty-file note, and the binary refusal (§5) — ends with a line/byte count and a
`read offset/limit` nudge, so a consumer never has to fall back to a `wc -l`
round-trip even when `index` declines to outline. Removing that round-trip is the
whole point (§2); a refusal that doesn't carry a pointer reintroduces it.

**Dispatch backend.** JSON/YAML/TOML are tree-sitter-backed and slot into the
existing `Lang` enum with a new `render()` branch (exactly how Markdown was
added) — no structural change. The line-oriented formats (INI, `.env`, CSV, logs,
text) have **no grammar**, and `outline_with_depth` currently builds a parser
unconditionally, so they require the backend split in §3.0 first.

### 3.0 Dispatch backend — resolve the `Lang` vs `DataLang` fork

`outline_with_depth` today *always* creates a parser and calls `set_language`/
`parse` (`outline.rs:169–175`), and `Lang::ts_language()` returns a real grammar
for every variant (`outline.rs:86–100`). That is correct for grammar-backed
formats and wrong for the line-oriented ones, which have no grammar to set.
Resolve it with a two-arm dispatch rather than overloading `Lang`:

```rust
enum Format {
    TreeSitter(Lang),   // Rust…Markdown, JSON, YAML, TOML — parsed
    Lines(LineFormat),  // INI, Env, Csv, Log, Text — scanned, no grammar
}
```

`outline_with_depth` branches on `Format` **before** building a parser:
`TreeSitter(lang)` takes the existing parse path; `Lines(fmt)` routes straight to
a hand-rolled scanner that fills the same `DeclSink`. This keeps `ts_language()`
total (no panicking/`unreachable!` arm) and confines the scanners to one place.

**Sequencing.** JSON/YAML/TOML do **not** need this refactor — they are
`TreeSitter` variants and land as additive `Lang` arms + a `render()` branch.
Introduce `Format`/`LineFormat` only when the first line-oriented format (`.env`)
arrives, as a standalone no-behaviour-change refactor so the existing-format
tests stay green, then build the scanners on top.

### 3.1 JSON / YAML → key hierarchy with array lengths

Emit the **key path tree** down to `max_depth`, each key on its own line with the
line range of its value. For arrays, show the length rather than enumerating
elements. Scalars are leaves (key + a short type/elided-value hint, never the
full value of a long string).

```
package.json
  [1-40]   {object, 6 keys}
  [2-2]      "name": "terva-ext-index"
  [3-3]      "version": "0.3.3"
  [5-12]     "dependencies": {object, 7 keys}
  [13-20]    "scripts": {object, 5 keys}
  [21-39]    "workspaces": [array, 3 items]
```

```
ci.yml
  [1-1]    name: CI
  [3-8]    on: {2 keys}
  [10-60]  jobs: {2 keys}
  [11-40]    build: {5 keys}
  [41-60]    test: {4 keys}
```

Rules:
- Depth: top-level keys at `depth 0`; nested keys appear as `depth` grows — same
  contract as Markdown headings and code members. Indent via `DeclLine.indent`.
- Arrays: `[array, N items]`. Do **not** descend into array elements by default
  (a 5000-element array would blow the cap); if the array holds objects, you may
  optionally show the first element's shape at `depth >= 2`, but keep it bounded.
- Scalars: print `key: <hint>` where the hint is a type word (`string`, `number`,
  `bool`, `null`) or a clipped value for short scalars only (`<= 40` chars).
  Long strings → `"<string, 1.2 KB>"`. This keeps a giant base64 blob or inlined
  cert out of the skeleton.
- YAML specifics: handle documents separated by `---` (emit one block per
  document, or a `--- document N` separator line). Anchors/aliases: show the key
  as written; do not attempt to resolve aliases.

### 3.2 TOML / INI → sections + keys with line numbers

Section headers become the top-level outline; keys under a section are the
nested level.

```
Cargo.toml
  [1-3]    [package]
  [2-2]      name
  [3-3]      version
  [5-20]   [dependencies]
  [6-6]      tree-sitter
  [7-7]      serde_json
  [22-25]  [dev-dependencies]
```

Rules:
- TOML: `[section]` and `[[array.of.tables]]` are the section level; bare
  `key = value` above the first section is a top-level group (label it
  `(root)` or emit the keys at `depth 0`). Show **key names only**, not values,
  to stay compact and avoid leaking embedded tokens.
- INI/`.cfg`/`.conf`: same model. `[section]` lines + `key = value` /
  `key: value` keys. Tolerate `;` and `#` comments. These have no canonical
  grammar; a tiny line scanner (regex-free, see §3.7) is enough and avoids a new
  dependency.
- Range for a section = its header line through the line before the next section
  (mirror the Markdown section-span computation).

### 3.3 CSV / TSV → header + column count + row count

```
data.csv
  [1-1]    header: id, name, email, created_at (4 columns)
  rows: 12,480 (delimiter: ",")
```

Rules:
- Parse only the **first line** for the header and column count; **count newlines**
  for the row total (do not materialize rows). Detect the delimiter from the first
  line (`,` vs `\t` vs `;`).
- If the first line does not look like a header (all-numeric), say
  `header: (none detected) — N columns` and still give the row count.
- This is O(file) on bytes but O(1) on memory — a newline count, not a parse. It
  stays well under `MAX_FILE_BYTES`.

### 3.4 `.env` → key names ONLY (never values)

```
.env
  [1-1]    DATABASE_URL
  [2-2]    REDIS_URL
  [4-4]    STRIPE_SECRET_KEY
  (values omitted — .env may contain secrets)
```

Rules — this is the **security-critical** format:
- Emit **only** the part left of the first `=`. NEVER the value, NEVER a value
  type hint, NEVER a value length (a length can leak entropy). The trailing
  `(values omitted …)` note tells the model the omission is deliberate so it does
  not "helpfully" `read` the file to recover them — but it still can if it has a
  genuine reason; we are reducing accidental exposure, not enforcing policy.
- Skip blank lines and `#` comments. Handle `export FOO=bar` (strip the `export`).
- Match by **filename**, not just extension: `.env`, `.env.local`,
  `.env.production`, etc. (files whose name is `.env` or starts with `.env.`).
  Extension-only matching misses them because `.env` has no extension after the
  dot in the usual sense. See §3.7 on filename-based dispatch.

### 3.5 Logs / plain text → cheap structural summary

```
build.log
  lines: 4,210   bytes: 612 KB
  first: [1] "=== build started 2026-06-19T10:02:11Z ==="
  last:  [4210] "BUILD SUCCESSFUL in 3m12s"
  matches: 3 ERROR, 11 WARN  (first ERROR at [1882])
```

Rules:
- Always cheap: line count, byte size, first non-blank and last non-blank line
  (clipped). These alone beat a blind `read`.
- Optional, high-value for logs: scan for a small set of severity tokens
  (`ERROR`, `WARN`, `FATAL`, `panic`, `Traceback`) and report counts + the line
  of the first match, so the model can `read` straight to the failure. Keep this
  to a fixed token list and a single pass; cap reported matches.
- Plain `.txt` with no structure → just the line/byte count and first/last
  lines. Do not try to invent sections. (If you later want it, a heuristic for
  `ALL-CAPS` or underlined headings could promote a `.txt` to a Markdown-like
  TOC — explicitly out of scope here.)

### 3.6 Directory mode for the new formats

`handle_directory` in `main.rs` currently inlines each file's full skeleton. For
config/data files the per-file outline is small, so inlining is fine and
desirable — but combined with §4 (size hints) the directory view becomes the
real win: a model can see *every* manifest and log with its size in one call. Two
required changes:

- `collect_source_files` gates on `Lang::from_extension(...).is_some()`. Extend
  the recognition predicate so the new formats are walked too (and so filename
  matches like `.env` are included — but note `.env` and other dotfiles are
  currently **skipped** by the `name.starts_with('.')` hidden-entry filter; see
  the edge case in §5).
- Keep the existing `DENY_DIRS` / hidden-dir pruning. Do **not** start walking
  `node_modules` just because it is full of `package.json`s.

### 3.7 Parsing strategy & dispatch (implementation choices)

Two questions: how to recognize these formats, and how to parse them.

**Recognition.** `Lang::from_extension` keys off the lowercased extension and is
the single dispatch point. That is sufficient for `.json/.yaml/.yml/.toml/.ini/
.cfg/.conf/.csv/.tsv/.txt/.log`. It is **not** sufficient for `.env` (no usable
extension) and for special-named JSON without an extension. Add a
companion `Lang::from_filename(name) -> Option<Lang>` (or fold filename handling
into the resolver in `main.rs`) and call it as a fallback when
`from_extension` misses. Wire it in at:
- `outline.rs` entry: `outline_with_depth` resolves the format — give it the
  **basename**, not just the extension. Do **not** reuse `display_name` for this:
  in directory mode it is a relative path *and now carries the `(N lines, X KB)`
  size hint* (§4), so it is not a filename. Pass `path.file_name()` explicitly.
- `main.rs` directory walk: the recognition predicate in `collect_source_files`.

Two `.env` gotchas the implementer will hit:
- **`Path::extension()` is inconsistent across the family.** `.env` → `None`, but
  `.env.local` → `Some("local")` and `.env.production` → `Some("production")`. So
  no extension rule matches the whole family — recognize it by filename:
  `name == ".env" || name.starts_with(".env.")`.
- **The hidden-entry filter hides it.** `collect_source_files` skips every entry
  whose name `starts_with('.')` (`main.rs:438`), so the filename predicate must be
  applied in **both** the dotfile allowlist (to let `.env*` past that filter) and
  the recognition gate (`main.rs:454`). See §5.

**Parsing.** Mix of approaches, chosen to minimize new dependencies and to keep
real line numbers:
- **JSON**: `serde_json` is **already a dependency**. But `serde_json::Value`
  discards line numbers, and you need ranges. Two options: (a) parse with a
  spanned/located JSON parser, or (b) the recommended path — add the
  `tree-sitter-json` grammar (consistent with every other format here, gives you
  node row ranges directly, matches the existing walk style). JSONC/comments come
  free with the tree-sitter grammar. Prefer (b) for line-range fidelity.
- **YAML**: add `tree-sitter-yaml`. Row ranges from nodes, same as code.
- **TOML**: add `tree-sitter-toml` (gives sections/keys with ranges).
- **INI / `.env` / CSV / logs / plain text**: **no grammar.** These are
  line-oriented; a tiny hand-rolled scanner over `source.split(|&b| b == b'\n')`
  with a running line index gives exact ranges and zero new dependencies. Keep
  the scanners in `outline.rs` next to `collect_markdown` so they stay
  unit-tested in isolation.

Adding grammars means new `Cargo.toml` entries pinned to ABI-matched versions
(the file already documents this constraint — keep the `tree-sitter` 0.25 ABI
alignment). Verify each grammar crate's `tree-sitter` major matches the others
before pinning; a mismatch fails `set_language`.

`Lang::name()` must gain an arm for each new variant (it is a non-exhaustive
`match`-by-value and will fail to compile otherwise — a useful forcing function).

---

## 4. Directory walk: per-file size hint

### Problem
`handle_directory` lists each file by its relative path header but emits **no
per-file line count or byte size**. The model cannot tell which files are cheap
to `read` without a separate `bash wc -l` / `ls -l` round-trip. For a directory
full of configs and logs this is exactly the information that decides *what to
read next*.

### Proposal
Annotate each file's header line in directory mode with its **line count and byte
size**. The numbers are already in hand: `handle_directory` calls `read_bounded`
on every file (so it has the bytes → `bytes.len()` and a newline count), and
`std::fs::metadata` gives the size without a read for files it skips.

Current header (from the README):
```
main.rs
  imports: ...
```
Proposed:
```
main.rs  (412 lines, 11.3 KB)
  imports: ...
```

And for the new config/data files the same hint makes the directory a one-call
manifest map:
```
config/  — 14 supported files

package.json  (40 lines, 1.1 KB)
  [1-40]   {object, 6 keys}
  ...
docker-compose.yml  (88 lines, 2.4 KB)
  [1-3]    version: "3.9"
  ...
build.log  (4210 lines, 612 KB)
  lines: 4,210  first: [1] "..."  last: [4210] "..."  matches: 3 ERROR
```

Implementation notes:
- Compute lines as a newline count over the bytes you already read (add 1 if the
  file does not end in `\n` and is non-empty, or just report the newline count —
  pick one and be consistent; document it).
- Format bytes human-readably (`KB`/`MB`) but keep it short.
- This is a directory-mode-only change in `main.rs`; the single-file skeleton
  header (`render` in `outline.rs`) is unchanged unless you also want the hint
  there (optional — a single-file call is already a deliberate read, so the
  marginal value is lower; recommend directory-mode only to keep the change
  small).
- For files the walk decided to count but **not** outline (e.g. it hit
  `MAX_DIR_FILES` / `MAX_DIR_BYTES`), consider still listing the name + size as a
  bare line so the "showing N of M" set is actionable. Optional; keep within the
  existing byte budget.

---

## 5. Edge cases

- **Huge files.** `MAX_FILE_BYTES` (2 MB) still applies — a 2 MB+ JSON/YAML/log
  is refused with the existing `TooLarge` message pointing at `read`. The CSV/log
  summaries are O(file) in bytes but O(1) in memory (newline counts, first/last
  line), so they stay safe right up to the cap. `MAX_DECL_LINES` (500) caps the
  key/section outline for a config with thousands of keys — the existing
  truncation marker fires. Good: a giant generated `*.json` does not produce a
  giant skeleton.
- **Binary / non-UTF-8.** Configs are normally UTF-8, but a `.log` or mis-named
  `.csv` can be binary. The code already uses `String::from_utf8_lossy`, which
  will not panic, but a binary `.log` would yield a garbage "first/last line."
  Add a cheap binary sniff (NUL byte in the first few KB) and refuse with a
  "looks binary — use `read`" message rather than emitting noise. The existing
  tree-sitter formats tolerate this implicitly via the parser; the hand-rolled
  scanners need the explicit guard.
- **Secrets in `.env`** — see §3.4. Values are **never** emitted, not even a
  type or length. This is the one format where the omission is a hard rule, not a
  size optimization. Mirror the same caution for any value that looks like a
  secret in other formats: the JSON/YAML scalar rule already elides long strings;
  do not special-case "show me the token."
- **`.env` and dotfiles vs. the hidden-entry filter.** `collect_source_files`
  skips every entry whose name `starts_with('.')` — which includes `.env`,
  `.env.local`, etc. A directory walk will therefore **not** surface dotfile
  configs unless you carve an exception. Recommendation: keep skipping hidden
  *directories* (`.git`, `.venv`), but allow an allowlist of hidden *files*
  (`.env*`, maybe `.npmrc`, `.editorconfig`) so the common dotfile configs show
  up. A direct `index .env` request (a file path, not a walk) already bypasses
  this filter and should just work once recognition handles `.env`.
- **JSONC / comments / trailing commas.** `tsconfig.json` and many `.json`
  configs are technically JSONC. The tree-sitter-json grammar tolerates this far
  better than strict `serde_json`; another reason to prefer the grammar (§3.7).
- **CSV with embedded newlines in quoted fields.** A pure newline count
  over-counts rows when a quoted field contains a newline. Acceptable for a
  *hint* (label it `~N rows` or note it is a raw line count, not an RFC-4180 row
  count) — do not pull in a full CSV parser for a summary.
- **Empty / whitespace-only files.** Emit the existing
  "(no … found)"-style note rather than an empty block. `render` already does
  this for code; replicate for the new collectors.
- **Ambiguous extensions.** `.conf` and `.cfg` are not a single format. Treat
  them as INI-style key/section and accept imperfect results — the line ranges
  are still correct even if the sectioning is approximate.

---

## 6. Acceptance criteria

A change implementing this proposal should satisfy:

1. `Lang::from_extension` (and a new filename fallback) recognizes
   `.json`, `.yaml`/`.yml`, `.toml`, `.ini`/`.cfg`/`.conf`, `.csv`/`.tsv`,
   `.txt`, `.log`, and `.env`/`.env.*`. `Lang::name()` has an arm for each.
2. **Outline formats** (JSON/YAML/TOML/INI) produce a skeleton whose lines carry
   **correct 1-based inclusive line ranges**, verifiable against fixture content
   (the same assertion style as the existing `[8-10]`/`[7-9]` tests in
   `outline.rs`). **Summary formats** (CSV/logs/text) instead emit a fixed
   structural digest with no per-line range; their tests assert the digest fields,
   not ranges. Every format — both contracts, plus the empty and binary paths —
   ends with a line/byte count and a `read` pointer (the §3 uniform read-pointer
   rule).
3. JSON/YAML outlines show the **key hierarchy** with array lengths and elide
   long scalar values. TOML/INI show **sections + keys** (key names, not values).
   CSV shows header + column count + row count. Logs/text show line/byte counts +
   first/last line (+ optional severity match summary).
4. `.env` emits **key names only**; a test asserts that a known value string does
   **not** appear anywhere in the output, and that the "values omitted" note is
   present.
5. `MAX_FILE_BYTES`, `MAX_DECL_LINES`, and the truncation marker continue to
   apply to the new formats (a test with > 500 keys truncates with the marker).
6. A binary `.log`/`.csv` is refused with a "use `read`" message, not garbled
   output.
7. Directory mode lists each file's header with `(N lines, X KB)` and includes
   the new config/data formats (and dotfile configs per the allowlist), while
   still pruning `DENY_DIRS` and hidden directories.
8. `TOOL_DESC` and `STANDING_CONTEXT` in `main.rs` are updated to advertise the
   new formats **and teach the two output shapes**. The current wording frames
   `index` as code-only ("imports + signatures"), so a config must be described as
   a key/section outline (and logs/CSV as a summary) or a consumer keeps reaching
   for `read` instead of `index` — the feature lands but goes unused. Treat this
   as part of the feature, not a trailing doc chore. The README "Supported
   languages" section is updated too.
9. `const VERSION`, `Cargo.toml` `version`, and `extension.json` `version` are
   bumped together (the `version_matches_manifests` test enforces this), and any
   new grammar crates are pinned ABI-compatibly.
10. Existing tests still pass; new unit tests live in `outline.rs` (collectors)
    and `main.rs`/`tests/wire.rs` (dispatch, directory size hints).

---

## 7. Suggested implementation order (highest value / lowest effort first)

Ship in small, independently releasable steps. Each step is a self-contained PR.

1. **Directory size hints (§4). ✅ Done — `feat/dir-size-hints`.** Lowest effort,
   immediate value, zero new dependencies, no new format logic — the byte/line
   data is already in hand in `handle_directory`. Shipped first; it improves the
   existing tool on day one and de-risks the rest. As built: each outlined file's
   header gains `(N lines, X KB)`, where lines = newline count + 1 for a final
   unterminated line (empty → 0) and bytes are human-readable (`B`/`KB`/`MB`).
2. **JSON (§3.1).** Highest value per the brief (`package.json`, `tsconfig.json`,
   lockfiles are everywhere). Add `tree-sitter-json`; reuse the existing
   tree-sitter walk style and the `DeclSink`/range machinery. Establishes the
   `render()` per-format branch pattern and the key-outline format that YAML/TOML
   reuse.
3. **YAML (§3.1).** Same outline shape as JSON, second-highest value (CI configs,
   k8s, compose). Add `tree-sitter-yaml`. Handle multi-document `---`.
4. **TOML (§3.2).** `Cargo.toml`/`pyproject.toml` are right in this repo's
   wheelhouse. Add `tree-sitter-toml`. Sections+keys outline.
5. **`.env` (§3.4).** Small, no grammar, line scanner — but do it as its own step
   because of the security rule and the filename-dispatch + dotfile-allowlist
   plumbing it introduces. Land the "never emit values" test with it.
6. **INI / `.cfg` / `.conf` (§3.2).** Reuses the TOML outline shape via a tiny
   line scanner; no grammar. Low value individually but cheap once §3.2 exists.
7. **CSV / TSV (§3.3).** No grammar; header parse + newline count. Cheap, useful
   for fixtures/data dirs.
8. **Logs / plain text (§3.5).** No grammar; the structural summary + optional
   severity scan. Most heuristic of the set — do it last, with the binary-sniff
   guard from §5.

Rationale: §1 ships value with no format risk; §2–4 are the
grammar-backed "real" outliners in descending commonality; §5–8 are
dependency-free line scanners ordered by value, with the two judgment-call
formats (`.env` security, log heuristics) bracketing the easy ones so their extra
review attention is isolated.

---

## Appendix: doc placement

This repo's `docs/` is otherwise **flat** (`architecture.md`, `wire-protocol.md`,
`rust-sdk-extraction.md`, `release-process.md`) and has no prior `proposals/`
convention — the Markdown-TOC feature (commit `19d165b`) shipped without an
in-repo proposal doc. This note is placed at **`docs/proposals/`** (a new
subdirectory) to keep forward-looking design notes separate from the existing
as-built reference docs. If the maintainer prefers the flat convention, move it
to `docs/structured-config-outlining.md` — nothing in the note depends on the
location.
