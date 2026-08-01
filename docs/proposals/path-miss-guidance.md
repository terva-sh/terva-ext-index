# Proposal: a path miss should answer, not just refuse

Status: **implemented** (§3.1 and §3.2 landed in `src/main.rs`; §3.3 was
considered and declined). Prompted by a live terva session review — a 17.5-hour
coding-agent session in an unrelated Go project, 2,205 tool calls, 246 of them
`index`. Evidence in §2. Paths below are anonymized; the shapes are verbatim.

Implementation notes / deviations from the sketch below:

- **§1's premise was wrong.** "The extension has already resolved the parent
  directory by the time it gives up" — it has not. `Jail::resolve`
  (`src/sandbox.rs:56`) is a single `std::fs::canonicalize`; the kernel walks the
  path internally but Rust surfaces a bare `io::Error` with no component
  information. Nothing about the parent is retained. This answers §5.1: the walk
  is redone in `nearest_existing_ancestor`, one `canonicalize` per component on
  the error path only, and it is cheap enough to be unconditional. It also
  **self-terminates** — above the jail root `resolve` returns `Outside`, so the
  climb stops at the workspace instead of marching to `/`. That property is what
  keeps the listing from ever naming a directory outside the jail.
- **§3.2 was one site, not three.** `:316` (`cannot stat`) and `:352`
  (`cannot read`) already gave correct advice — as §3.2 itself concedes — and
  `:316` is close to unreachable, since `canonicalize` succeeding means a
  `metadata` failure is a race or an ACL edge, never a missing file. Only the
  `NotFound` message changed; the other two carry a comment saying why they keep
  the `read` advice.
- **The empty-directory case got the same treatment** — an addition, not in the
  proposal. `handle_directory`'s "contains no supported source files" result had
  the identical shape: it held the walk results and told the caller to go `grep`
  blind. It now names what is in the directory, which also *explains* the result
  (everything is under `node_modules/`, or in a format we don't outline).
- Cap is `MISS_LIST_ENTRIES = 50`, not `MAP_DIR_FILES`. See §5.2.
- The listing does **not** filter hidden entries or `DENY_DIRS`, unlike the
  directory walk: it never descends, so `node_modules/` costs one line, and a
  listing that omits `.env` while the caller is hunting `.env` is worse than a
  long one.

Real output, from this repo:

```
index: no such path src/mainn.rs — `mainn.rs` is not in src/, which contains: main.rs  outline.rs  sandbox.rs
index: no such path src/nope/deep/x.rs — `nope` is not in src/, which contains: main.rs  outline.rs  sandbox.rs
```

**On the evidence:** one session, one repo, one model — the 8.5% below should not
be quoted as a general rate. The case does not rest on it. The mechanism is
deterministic: on `ENOENT`, `read` fails identically by construction, so "fall
back to `read`" was dead advice whatever the frequency. The session only showed
it happening twice.

## 1. The claim

`index` is asked for a path that does not exist about **one time in twelve**, and
when that happens it returns an error whose advice cannot work. The extension has
already resolved the parent directory by the time it gives up, so it is holding
the answer the caller needs and throws it away.

Three changes, in descending order of value:

1. When the missing path's **parent exists**, name the parent's entries.
2. Stop recommending `read` for a path that does not exist.
3. Consider returning a path miss as a **successful** result rather than
   `is_error: true`.

## 2. Evidence

One session, one repo:

| | |
|---|---|
| `index` calls | 246 |
| path misses (`is_error`, "cannot access") | **21 (8.5%)** |
| distinct paths missed | 17 |
| times the model followed the "fall back to `read`" advice | 2 |
| times that fallback then succeeded | **0** |

The two fallbacks both produced the identical failure one call later:

```
index: cannot access …/internal/store/save/save.go
       (No such file or directory (os error 2)). Fall back to `read` …
read:  stat …/internal/store/save/save.go: no such file or directory
```

(the other pair was `schemas/api/v1/entity-data.schema.json`.)

**The guesses were structurally right and filename-wrong.** Every missed path sat
under a directory that existed — `internal/store/save/`, `internal/model/spec/`,
`internal/runner/engine/`, `docs/`, `schemas/api/v1/`. The model knew the layout
and guessed the conventional filename:

| asked for | actually present |
|---|---|
| `internal/store/save/save.go` | `save_test.go` |
| `internal/model/spec/loader.go` | `loader_profile_test.go` |
| `internal/runner/engine/dispatch.go`, `event.go`, `events.go`, `advance.go`, `effect.go` | — |

**Misses chained inside one directory**, because nothing ever told the caller what
was in it:

- `internal/runner/engine/activity_test.go` (miss) → `scheduler_test.go` (miss)
- `docs/scheduling.md` (miss) → `docs/dispatcher.md` (miss)
- `internal/store/save/save.go` (miss) → `replay.go` (miss)

`schemas/api/v1/entity-data.schema.json` was missed **three separate times** over
the session. The recovery, when it came, was usually a `grep` or `glob` over the
same directory — the call the miss could have saved.

## 3. Proposed changes

### 3.1 Name the siblings (`src/main.rs:288–298`)

`JailError::NotFound` is returned from `dirs.jail().resolve(&resolved)`, which has
already walked to the failure point. When `resolved.parent()` resolves inside the
jail and is a directory, list its entries instead of stopping:

```
index: no such file …/internal/store/save/save.go
       internal/store/save/ contains: recorder.go  replay_test.go  save_test.go
       coarse_batch_test.go  memory_pool_test.go
```

Bound it the way directory mode already bounds itself — this is the same
"don't trade a token firehose for an I/O one" constraint as
`docs/proposals/directory-mode-scoping.md`; a stat-only listing with a cap
(reuse `MAP_DIR_FILES`, or a smaller cap since this is an error path) and an
`… and N more` tail. No file bodies are read.

For a missing **intermediate** component (`internal/nope/save.go`), name the
deepest parent that does exist and list that — it is the same answer one level up.

### 3.2 Fix the advice at all three sites

`src/main.rs:293`, `:316`, `:352` all end in "Fall back to `read`". That advice is
right for exactly one of the cases they cover:

- `cannot access` / `NotFound` → **`read` cannot help**; it fails identically.
  Proven twice above. Point at the sibling listing (§3.1), or at `glob` when the
  parent is gone too.
- `cannot stat` / `cannot read` (permissions, a device, a broken symlink) →
  `read` is a reasonable second opinion. Keep it here.

The `:293` message conflates "it may not exist or be unreadable" into one
sentence, and the caller cannot tell which it got. The `os error 2` in the payload
already distinguishes them; the prose should too.

### 3.3 Is a path miss an error? — **declined, stays `true`**

The original argument: *"that file doesn't exist; the directory contains …"* is
an **answer**, and returning it as a normal result would keep `is_error`
meaningful for genuine faults (jail violations, unreadable files, a malformed
request).

Checking the host settled it the other way. `is_error` is load-bearing in terva
core, not just cosmetic:

- `packages/core/stall.go:531` — `unproductiveResult()` keys the **churn axis**
  off `IsError`. Flip the flag and path misses stop feeding it entirely.
- `packages/core/stall.go:130` — `resultFingerprint()` hashes `IsError` *plus*
  the result text for **spin** detection. Today the three identical
  `actor-data.schema.json` misses in §2 fingerprint identically and the spin
  detector can fire. After §3.1 the text carries the missed filename, so two
  *different* wrong guesses in one directory no longer match the spin
  fingerprint — and with `is_error: false` they would be invisible to churn as
  well. That is precisely the flail §2 documents.

So the flag stays `true`: the guidance rides in the text either way, the request
genuinely did not produce an outline, and dropping the flag would remove the
host's only remaining signal for a caller that keeps missing.

(`packages/core/compact.go:516` also marks failed calls in the executed-actions
ledger so the agent isn't told "you already ran this" — harmless either way for
a read-only tool, but another consumer of the flag.)

## 4. Non-goals

- No fuzzy matching or did-you-mean scoring. Listing the directory is enough; the
  caller is a model that reads the list and picks. Ranking is a second system to
  get wrong.
- No caching of directory listings. Path misses are ~8% of calls and the listing
  is stat-only.
- No change to the jail. `JailError::Outside` (`:299`) is correct as written —
  it names the constraint and points at unjailed host `read`.

## 5. Open questions — answered

1. **Does `jail().resolve()` surface which component was missing?** No. It is one
   `canonicalize`; the error says only that *something* is missing. §3.1 does its
   own upward walk (`nearest_existing_ancestor`) — one `canonicalize` per
   component, error path only, terminating at the jail root because `resolve`
   returns `Outside` above it. Cheap enough to be unconditional.
2. **Cap?** Not `MAP_DIR_FILES`. `MISS_LIST_ENTRIES = 50`, with an
   `… and N more (T total)` tail and a pointer at `glob` when it truncates — a
   50-name listing renders in ~700 bytes, which is a message; 300 would be the
   firehose directory mode was built to avoid.
3. **Respect `depth`?** No — always one level, for the reason given in the
   question. `depth` describes a tree the caller asked to render, not the search
   for a file they missed.

## 6. What this did not do

Both non-goals in §4 held. No fuzzy matching or did-you-mean ranking: the
listing is sorted and complete-to-the-cap, and the caller picks. No caching. The
jail is untouched — `JailError::Outside` still reports exactly as it did, and the
new walk goes *through* `Jail::resolve` for every directory it considers listing,
so the confinement guarantee is unchanged rather than re-implemented.

Test coverage in `src/main.rs`: siblings named, the climb past a missing
intermediate component, the workspace-root wording, the cap and its tail, hidden
entries listed, and — the security-relevant one —
`path_miss_outside_the_workspace_lists_nothing`, which pins that a miss whose
parent lives outside the jail leaks no listing.
