# Proposal: outline SQL with a line scanner, not a grammar

Status: **proposed** — and deliberately not started. Prompted by the language-gap
review that added shell, Makefile, Dockerfile, XML, HTML and CSS; SQL was the one
candidate that did not pay for itself. **This is exploratory: nobody working on
this repo currently has SQL to index, so there is no evidence of what a real
schema file needs.** Do not build it on spec — pick it up when a session
actually hits `index: unsupported file type ".sql"`.

## 1. The claim

SQL belongs in `index` eventually. A migration directory or a `schema.sql` is
precisely the "hundreds of lines, a handful of things you want" shape the tool
exists for. But it should be a **line scanner** in the `LineFormat` family
(alongside Makefile, Dockerfile, INI and `.env`), **not** a tree-sitter grammar —
because the grammar costs 17% of the binary.

## 2. Evidence: the grammar is disproportionately expensive

Measured against the 13.90 MB release binary, each candidate wired into
`Lang::ts_language()` so the linker actually keeps it (an unreferenced
dependency is dead-code-eliminated and reads as free):

| Addition | Crate | Δ binary | Δ % |
|---|---|---|---|
| XML | `tree-sitter-xml` 0.7 | +34 KB | +0.2% |
| HTML + CSS | `tree-sitter-html` 0.23, `tree-sitter-css` 0.25 | +132 KB | +0.9% |
| **SQL** | **`tree-sitter-sequel` 0.3.11** | **+2.33 MB** | **+16.8%** |

The cause is not subtle: that grammar's generated `parser.c` is **16.6 MB** of
source, against 0.5 MB for CSS and 0.1 MB for HTML. SQL's grammar carries a huge
keyword set across several dialects and the LR tables explode. One grammar would
cost 18× the other three combined, on a binary that `run.sh` downloads
per-platform from GitHub releases.

There is no cheaper option in that family: `tree-sitter-sql` on crates.io is a
v0.0.2 stub, and `tree-sitter-sequel` is the maintained publication of the real
grammar.

## 3. Proposed shape

`LineFormat::Sql`, extensions `.sql` / `.ddl`, scanning for statements that
start a definition at column 0 (case-insensitive):

```
CREATE [OR REPLACE] TABLE|VIEW|MATERIALIZED VIEW|INDEX|FUNCTION|PROCEDURE|
       TRIGGER|TYPE|SCHEMA|EXTENSION|SEQUENCE
ALTER TABLE
DROP …
```

Each becomes one `DeclLine` whose range runs to the terminating `;` — the same
"span to the next sibling" rule Markdown headings, INI sections and Makefile
targets already use, so a follow-up `read` lands on the whole statement.

```
0003_add_sessions.sql
  [1-14]   CREATE TABLE sessions
  [16-16]  CREATE INDEX sessions_user_id_idx ON sessions
  [18-24]  ALTER TABLE users
```

Column lists inside a `CREATE TABLE` are **not** expanded — that is what `depth`
would mean here, and it is the natural follow-up once there is a real file to
judge it against (see §5).

## 4. Why a scanner is the right call, not just the cheap one

- **Dialect tolerance.** Postgres, MySQL, SQLite and T-SQL disagree about
  quoting, types and procedural blocks. A grammar either bloats to cover them or
  misparses; a keyword scan degrades to "found fewer statements", which is the
  failure mode this repo already prefers elsewhere.
- **It is the same trade already made twice.** Makefile and Dockerfile are line
  scanners for the same reason and produce genuinely useful outlines.
- **The lost precision is not the precision anyone wants.** A grammar buys exact
  ranges for nested expressions inside a `SELECT`. Nobody indexes a schema file
  to navigate an expression tree; they do it to find where a table is defined.

## 5. Open questions for whoever picks this up

1. **What does a real migration directory look like?** This is the question that
   blocks everything else, and it cannot be answered without one. Numbered
   migrations (`0003_add_sessions.sql`) are usually short and numerous — that is
   a *directory-mode* problem (does the file map read well?) more than a
   per-file one.
2. **Should `depth: 1` expand a `CREATE TABLE`'s columns?** Probably yes, by
   analogy with a struct's fields — but a wide table is 60 columns, so it needs
   the same judgement the JSON array cap got.
3. **Do `INSERT`/seed statements deserve anything?** A seed file is hundreds of
   `INSERT`s; a count (`142 INSERTs into 3 tables`) may beat a list, in the
   spirit of the CSV summary rather than the outline.
4. **Values.** A migration can contain a literal password in a seed row or a
   `CREATE USER … PASSWORD '…'`. The redaction rules in `outline.rs`
   (`must_redact`) already cover the value shapes; a scanner should route
   through them rather than inventing a second policy.

## 6. Non-goals

- No `tree-sitter-sequel` dependency, per §2, unless someone produces a case the
  scanner provably cannot serve.
- No SQL *parsing* — no attempt to resolve types, relations or references. This
  is an index, not a schema tool.
