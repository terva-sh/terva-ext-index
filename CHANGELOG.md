# Changelog

Public releases of terva-ext-index. Install with
`terva ext install https://github.com/terva-sh/terva-ext-index`.

## 0.8.1 — the ext-data declaration (2026-08-04)

- **`"data_secrets": false` in the manifest.** terva now treats an absent
  declaration as *unknown is not clean*: undeclared extensions are named
  by `terva secret status`, and their `ext-data/` directory goes
  agent-dark once enforcement flips. This extension writes nothing to
  `ext-data/`, so `false` is the honest declaration — and it keeps the
  directory usable as a debugging surface instead of losing it to a
  default.
- Built against **terva-sdk-rust v0.2.1** (0.8.0 shipped pinned at
  v0.1.0). No behavior change here: the extension declares no secrets
  and uses none of the new secret verbs. The SDK's extension runtime did
  move tool dispatch off the run-loop thread, which this extension rides
  unchanged.
- Note for anyone watching build times: the SDK's connector half gained
  at-rest sealing, so `age` and its dependencies now compile
  transitively even for an extension that seals nothing.

## 0.8.0 — onto the SDK (2026-08-03)

- The hand-rolled protocol loop and `sandbox.rs` are gone, replaced by
  **terva-extsdk**. That closed three known gaps by construction: no
  4 MiB read cap, an `expect()` in the send path, and an unvalidated
  `min_protocol`.
- `main.rs` shrank to the `Extension` declaration plus the index domain
  itself (path resolution, miss reporting, directory mode).
- One deliberate wire delta: the SDK omits `is_error: false` (Go
  `omitempty` parity), so three test assertions became "not an error".
- `run.sh` refuses to build a tree whose SDK path dependency cannot
  resolve — an installed copy — rather than silently falling back to a
  stale prebuilt binary.

## 0.7.0 and earlier

Pre-SDK releases, on the hand-rolled protocol loop: the outline language
set growing out from the original grammars (through XML, HTML, CSS,
shell, Makefiles and Dockerfiles), a bounded read budget for the map,
and directory mode. See the git history on the `release` branch.
