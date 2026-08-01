# Proposal: map-first directory mode — let `index` scope itself

Status: **implemented** (map-first three tiers landed in `src/main.rs`). Prompted by
a live terva review run (evidence in §5).

Implementation notes / deviations from the sketch below:
- Tiers are chosen by file **count**: `<= SKELETON_DIR_FILES` (30) → full
  skeletons; `<= MAP_DIR_FILES` (300) → flat file map; larger → child-dir rollup.
  The map/rollup boundary uses a count rather than a rendered-byte budget on
  purpose — a map/rollup line is a short size hint whose length is
  content-independent, so a count bounds the output accurately without
  rendering-then-discarding.
- The **rollup is stat-only**: it sums each group's file count and the byte size
  stat'd during the walk (`WalkFile.size`), and reads **no** file bodies — so a
  monorepo root renders in a couple hundred bytes and can't trade the token
  firehose for an I/O one. (The flat map still reads each file once for its line
  count, bounded by `MAP_DIR_FILES`.) This is the one place the sketch's rollup —
  which showed summed *lines* — would have read the whole tree; it does not.
- Map and rollup ignore `depth` (verified: a 330-file tree at `depth: 2` renders
  in ~280 bytes). `STANDING_CONTEXT` / `TOOL_DESC` updated. No `full: true` escape
  and no caching were added (both parked, per §3.5 / §5d).
- Not bumped: `VERSION` stays `0.5.0` until the next release (bump all three
  manifests together, per `version_matches_manifests`).

Original proposal follows.
Audience: the agent maintaining `terva-ext-index`.
Scope: `src/main.rs` (`handle_directory`, the dir caps, `STANDING_CONTEXT`, the tool
description) and a small renderer; `src/outline.rs` is untouched.

This note is self-contained: it carries the context an implementer needs without
reading the terva core repo or the harness run that prompted it.

---

## 0. Background — how directory mode works today (so the fix lands in real code)

`index <path>` turns a file into a tree-sitter **skeleton** (imports + signatures
with exact line ranges) so the model reads structure before paying for a full
`read`. Handed a **directory**, it maps the whole subtree in one call
(`handle_directory`, `src/main.rs:384`):

1. `collect_source_files` walks the tree (skipping hidden entries, `DENY_DIRS`
   like `node_modules`/`target`/`vendor`, and symlinks), bounded by
   `MAX_WALK_ENTRIES = 20_000` visited entries.
2. Files are sorted (deterministic order).
3. For **every** file, in sorted order, it emits the file's **full skeleton**
   (`outline::outline_with_depth(header, ext, bytes, depth)`), each headed by
   `rel (N lines, SIZE)`.
4. It stops when it has emitted `MAX_DIR_FILES = 200` skeletons **or**
   `MAX_DIR_BYTES = 256 * 1024` bytes, whichever comes first, and appends
   `(showing N; index a subdirectory for the rest)`.

The three directory caps live at `src/main.rs:363-365`.

This is exactly right for a **package**: `index src` over three files is the
canonical, delightful use — one call, the whole package's shape. The problem is
what happens when the directory isn't a package.

## 1. The problem — directory mode has no notion of scale

The caps are a **safety backstop** (don't brush the host's 1 MiB tool-result
limit), not an **ergonomic budget**. Between "3-file package" and "256 KB" there
is no middle behavior: `index` handed a large tree dumps up to 200 full
skeletons, and 200 skeletons is a firehose.

In the run that prompted this (§5), `index { path: ".", depth: 1 }` on a Go
monorepo returned a **116 KB** result — `. — 955 supported files (showing 200;
index a subdirectory for the rest)` — ~29k tokens in a single tool result. That
is not a "cheap map before reads"; it *is* the expensive thing the tool exists
to avoid, just aimed at a directory instead of a file. And because it rides in
the model's context for the rest of the session (re-sent every turn, re-billed on
any cache miss), one such call sets the cost floor for the whole conversation.

Two failure modes compound it:

- **Silent alphabetical truncation.** The cutoff is the 200th filename *in sort
  order*, so on a 955-file repo everything past `cmd/…`/early `packages/…` is
  dropped, and the model is never told which 755 files it can't see. The marker
  says "index a subdirectory for the rest" but not *which* — the model has to
  guess the tree it just failed to receive.
- **The default depth expands members.** `depth` defaults to 1 (top-level decls
  **plus** one level of members), so each of the 200 files carries its methods,
  not just its top-level shape — maximizing per-file bytes exactly when there are
  the most files.

The user's framing is the right one: **the scoping should come from the
extension.** Relying on the caller (a skill, or the model's judgment) to remember
"don't `index` the root, index a subdirectory" is a discipline that will be
forgotten, and the tool already knows more than the caller at the moment of the
call — it has walked the tree and counted the files. It can decide.

## 2. The fix — map-first directory mode (progressive disclosure)

Apply the tool's own core principle one level up. At the **file** level `index`
already gives structure-before-body (skeleton, then `read` the slice you want).
Do the same at the **directory** level: give **structure-before-skeletons** — a
file map, then `index` the file or small subtree you want expanded.

Concretely, `handle_directory` gains a scale check after the walk:

- **Small directory (`files.len() <= SKELETON_DIR_FILES`, e.g. 25):** unchanged —
  emit full skeletons for every file. The package case stays exactly as good as
  it is today.
- **Large directory (over the threshold):** emit a **map only** — one line per
  file with its `(lines, size, lang)` hint, no skeleton — headed by a line that
  tells the model how to expand:

  ```
  . — 955 supported files in 48 directories
  (map only — `index <file>` or `index <subdir>` for skeletons)

  cmd/terva/args.go            (612 lines, 18.4 KB)
  cmd/terva/main.go            (88 lines, 2.1 KB)
  packages/agent/swarm/agent.go   (243 lines, 7.9 KB)
  packages/agent/swarm/runner.go  (410 lines, 14.2 KB)
  …
  ```

  The model then does `index packages/agent/swarm` (18 files → under threshold →
  full skeletons) or `index packages/agent/swarm/runner.go` (one file →
  skeleton). Same drill-down the tool is built around, now reachable from the
  root without a 116 KB detour — and that drill-down is *proven* behavior, not a
  hope: in the observed runs every single `index <file>` was followed by a
  **targeted** `read` of the range the skeleton pointed at, never a full re-read
  (§5b). The fix must preserve that loop, and map-first does — it only changes
  what a *large-directory* call returns, and makes the file/package calls that
  feed the loop *more* reachable.

A bare file map of 955 entries at ~45 bytes/line is ~40 KB — already a big win
over 116 KB, and *complete* (every file is listed, nothing silently dropped). To
get a huge monorepo's root map down to a couple of KB, add an optional second
tier:

- **Very large tree — roll up by immediate child directory.** Instead of one
  line per file, one line per immediate subdirectory with a rollup:

  ```
  . — 955 supported files in 48 directories
  (rolled up — `index <subdir>` to descend)

  cmd/           14 files    3.9k lines
  packages/agent/ 310 files  92k lines
  packages/core/  71 files   28k lines
  docs/           40 files   …
  …
  ```

  `index packages/agent` then expands to *its* file map (or a further rollup),
  and so on down to a package small enough for skeletons. This makes the root of
  a monorepo a ~1-2 KB index, and every drill step stays bounded regardless of
  repo size — genuine progressive disclosure.

The map/rollup choice can itself be by scale: `<= SKELETON_DIR_FILES` →
skeletons; `<= MAP_DIR_FILES` (e.g. 300) → flat file map; larger → child-dir
rollup. All three tiers are cheap to render from the `files` vec the walk already
produces (the rollup just groups by first path component).

## 3. Implementation sketch

Everything is in `src/main.rs`; `src/outline.rs` (the pure per-file walk) is
untouched.

1. **Thresholds** next to the existing dir caps (`src/main.rs:363`):
   ```rust
   const SKELETON_DIR_FILES: usize = 25;  // <= this: full skeletons (a package)
   const MAP_DIR_FILES: usize = 300;      // <= this: flat file map; larger: rollup
   ```
   Keep `MAX_DIR_FILES` / `MAX_DIR_BYTES` as the safety backstop on the skeleton
   path.

2. **Branch in `handle_directory`** after `collect_source_files` + `files.sort()`:
   - `files.len() <= SKELETON_DIR_FILES` → the current skeleton loop (verbatim).
   - `<= MAP_DIR_FILES` → `render_file_map(&files, dir)`: for each file, the
     `rel (N lines, SIZE)` header line the skeleton path already builds
     (`src/main.rs:417-422`), minus the skeleton body. The `(lines, size)` needs
     a bounded read per file for the line count — the same `read_bounded` the
     skeleton path uses; keep the `MAX_WALK_ENTRIES` budget and cap the map's own
     bytes so a pathological tree still can't run away.
   - larger → `render_dir_rollup(&files, dir)`: group by first path component,
     sum file counts and line counts per group, one line each.

   Note both map renderers **ignore `depth`** — a file/dir listing has no
   per-file body to deepen, and the data shows agents opting into `depth: 2`,
   which would otherwise re-inflate exactly the tier meant to be cheap (§5c).
   `depth` keeps its meaning only on the skeleton path.

3. **Header wording** carries the mode and the escape hatch, so the model always
   knows how to expand (`(map only — index <file>/<subdir> …)` /
   `(rolled up — index <subdir> …)`). This replaces the current
   `(showing N; index a subdirectory for the rest)` marker, which only appears on
   truncation and never names the missing subtree.

4. **`STANDING_CONTEXT` / the tool description** (`src/main.rs:~44`): today they
   say "pass a directory to map a package." Add the progressive-disclosure
   intent: *indexing a large tree returns a file/directory map; index a specific
   file or subdirectory to get skeletons.* This is guidance the model sees every
   session, so it also fixes the *caller* habit — but the extension no longer
   depends on the caller getting it right.

5. **Optional `full: true` escape** in the schema for a caller that truly wants
   skeletons for a big directory (CI tooling, an explicit "expand everything").
   Probably unnecessary — indexing a subdir already gets skeletons — so default
   to *not* adding it and keep the surface minimal unless a real need appears.

6. **Tests** (`tests/wire.rs` + unit tests over the new pure renderers): a
   `>SKELETON_DIR_FILES` fixture returns a map with no skeleton bodies and the
   map header; a `<=SKELETON_DIR_FILES` fixture is unchanged (skeletons); a
   `>MAP_DIR_FILES` fixture returns a per-child rollup; and every tier's byte
   size is asserted to stay small. Add a regression asserting `index <repo-root>`
   of a many-file fixture is well under, say, 48 KB.

## 4. Why not just lower the caps?

Rejected alternatives, and why map-first beats them:

- **Shrink `MAX_DIR_BYTES` to ~32 KB.** Truncates *sooner* but still
  alphabetically and still silently — you'd see the skeletons of the first ~50
  files and be blind to the rest, with no map of what's missing. Strictly worse
  than a complete map at similar size.
- **Force `depth: 0` in directory mode.** Cuts per-file bytes but still emits up
  to 200 skeletons and still truncates the tail. Helps the symptom, not the
  shape.
- **Do nothing; fix it in the caller (skill / prompt).** This is what prompted
  the note. The review skill *was* updated to say "don't `index` the root," but
  that's a discipline every caller must remember forever, and the tool already
  has the information to do the right thing itself. Self-scoping is the robust
  fix; the skill guidance becomes belt-and-suspenders.

Map-first keeps the thing that makes `index` good — cheap structure, exact
line-range pointers, drill only where you look — and stops the one case where it
inverts into the expensive thing it's meant to replace.

## 5. Usage data — what four agents actually did

Mining the durable per-agent logs (§"session capture" below) across the review
runs gives 44 `index` calls from six sub-agents plus the coordinators — enough to
see the real shape, and it's mostly *good news for the tool*: the problem is one
narrow, fixable inversion sitting on top of an otherwise-validated design.

### 5a. The firehose (the problem, quantified)

From a terva whole-project review (one recorded session; identifiers omitted).
The coordinator's first discovery call was
`index { "path": ".", "depth": 1 }` → **116,488 bytes** (header `. — 955
supported files (showing 200; index a subdirectory for the rest)`, i.e. 200 full
file skeletons). It was the single largest tool result in the coordinator's
context and rode every one of its 34 turns.

But the decisive part is that the review dispatched three specialist sub-agents,
each with its own context — and **four independent agents, given no shared
prompting about `index`, all reached for the same firehose.** Mining the
per-agent event logs (`$TERVA_HOME/swarm/agents/*/events.jsonl` — see §"session
capture" below) for their `index` calls and result sizes:

| agent | persona | the large `index` calls it made | result |
|---|---|---|---|
| coordinator | — | `index .` | 116 KB |
| `…-641000` | huoltaja | `index .` | 116 KB |
| | | `index packages` | **171 KB** |
| `…-934000` | kirjuri | `index docs` (depth 2) | 73 KB |
| `…-57000` | koestaja | `index packages/agent/web/client` | 29 KB |

huoltaja alone pulled **~287 KB** of `index` output into its context from two
calls. Every other tool the crew used was well-behaved (dozens of `read` /
`grep` / small `index <file>` calls in the single-KB range); the only pathology
was directory-mode `index` on a large tree, and it recurred across every agent
that tried it. koestaja and kirjuri also made *good* small `index` calls
(`index README.md` → 526 B, `index cmd` → 4 KB) — so the tool works beautifully
at package/file scale and inverts only at repo scale.

That's the case for self-scoping in one table: this isn't a caller who forgot the
rule, it's the *instinct* — "map the project" → `index .` — shared by four agents.
A skill note can teach one caller; it can't teach every sub-agent the harness
spawns. The extension is the only place that sees the file count at call time and
can hand back a 1-2 KB map instead of a 116 KB materialization.

**And it's a tight Pareto — the fix is surgical, not a rewrite.** Across all 44
calls the byte distribution is bimodal: 36 calls (82%) returned under 40 KB (most
under 10 KB — files and small packages), and **8 calls (18%) accounted for 74% of
all `index` bytes** (819 KB of 1.11 MB). Every one of those 8 is a large-directory
call (`index .`, `index packages`, `index docs`, `index packages/agent/web`).
Map-first touches only those 8; the other 36 calls come back byte-for-byte
identical. So the change is low-risk by construction — it can't regress the calls
that already work because it doesn't alter their path.

### 5b. What already works — and what the fix must preserve

The reason to fix this *carefully* rather than just clamp the caps is that the
tool's core loop is not aspirational — it's happening, cleanly, every time:

- **13 of 13** `index <file>` calls that were followed by a `read` of that file
  used a **targeted** read (an `offset`/`limit` aimed at the range the skeleton
  surfaced). **Zero** were a full re-read that would have made the `index`
  wasted. e.g. `index packages/agent/cli.go` (7.5 KB skeleton) →
  `read cli.go offset=570 limit=9000`; `index args.go` (419 B skeleton) →
  `read args.go offset=299 limit=14000`. Structure → line-range pointer →
  targeted read is doing exactly what the tool promises.
- **0 of 44** calls returned an error or hit an unsupported/out-of-workspace path
  — the sandbox and dispatch held up across a real multi-agent run.
- **0** within-agent duplicate indexes — no agent re-indexed the same path.

This is the optimistic half of the report: `index` earns its keep at file and
package scale, measurably. Map-first is worth doing *because* the rest is sound —
it removes the one case that inverts the value prop while leaving the proven loop
(and now making its feeder calls, `index <file>`/`index <package>`, reachable
straight from a root map) fully intact.

### 5c. Agents actively prefer `depth: 2` — so the map tier must be depth-blind

Of the 44 calls, depth was **1 in 19, 2 in 23, 3 in 2** — i.e. agents chose the
*deeper* (more expensive) member view more often than the default. Depth interacts
badly with directory mode precisely because it multiplies per-file bytes across
every file shown: the single largest root call in the sample was
`index . depth:2` at **142 KB** (vs 116 KB at depth 1 — ~25 KB of pure
member-expansion tax on files the agent hadn't decided to read yet). 4 of the 8
firehose calls used `depth: 2`.

Implication for §3: the **map and rollup tiers should ignore `depth` entirely** —
a file map has no per-file body to deepen, so `depth` is meaningless there and
must not be allowed to re-inflate the output. `depth` keeps its full meaning on
the skeleton path (small dir / single file), which is where a member view is
actually wanted. This makes the map tiers depth-invariant, which is both simpler
and immune to the exact amplification the data shows agents opting into.

### 5d. Secondary opportunity — cross-agent redundancy (mostly harness-side)

The same costly directory index was recomputed by independent agents:
`index .` ran 3×, and `index packages/agent/web depth:2` ran **twice at an
identical 75,905 bytes** from two different sub-agents. Because each sub-agent is
its own process with its own context, there's no sharing — the crew paid for the
same skeletons more than once.

A shared skeleton cache keyed on `(path, mtime)` is really a *harness*/coordination
concern (the swarm host, not this extension) and is out of scope here. It's worth
noting for two reasons: (1) map-first shrinks the blast radius anyway — a
redundant large-dir call becomes a 1-2 KB map instead of a 76-171 KB dump, so the
cost of *not* caching drops by ~50×; and (2) an in-process, session-scoped memo
inside the extension (cheap, bounded, invalidated on `session_start`/`/cd`) could
dedupe the within-session repeats a single agent might make, if that ever shows up
— today it doesn't (0 within-agent dupes), so it's a "note, don't build."

### 5e. Projected impact

Applying map-first to the observed sample (turning each of the 8 firehose calls
into a ~1-2 KB map/rollup, leaving the other 36 untouched) takes total `index`
output from **~1.11 MB to ~0.30 MB — a ~72% reduction — with zero change to the
13 validated targeted-read loops or the 36 already-cheap calls.** The entire
saving comes from the calls that were never useful at full expansion in the first
place.

### Session capture — how the evidence above was gathered (reproducible)

terva's swarm persists every sub-agent durably under
`$TERVA_HOME/swarm/agents/<agent-id>/` (on macOS,
`~/Library/Application Support/terva/swarm/agents/…`), one directory per agent:

- `meta.json` — id, task, `persona`, model/provider, and the paths below.
- `events.jsonl` — the full durable event log: every `tool_call` (flat
  `{type,name,args,id,time}`), its `tool_result` (`{type,id,content}`, id matches
  the call), plus `assistant_message` / `turn_*` / `usage` events. This is the
  mineable record.
- `session.json` — the child's replayable session.

So sub-agent activity is *not* ephemeral — it outlives the run and can be
mined exactly like a top-level session. The per-agent `index` table above came
from pairing `index` `tool_call`s to their `tool_result` by `id` and measuring
`len(content)`:

```python
import json, collections
base = "<TERVA_HOME>/swarm/agents"
for ag in os.listdir(base):
    results, calls = {}, []
    for line in open(f"{base}/{ag}/events.jsonl"):
        d = json.loads(line)
        if d.get("type") == "tool_call" and d.get("name") == "index":
            calls.append((d["args"], d["id"]))
        elif d.get("type") == "tool_result":
            results[d["id"]] = len(json.dumps(d.get("content")))
    for args, cid in calls:
        print(ag, args, results.get(cid), "bytes")
```

A maintainer validating this proposal's fix can re-run a review, then diff the
`index` result sizes in these logs before/after — the map-first change should
drop the large-directory calls from 70-170 KB into the low single-KB range while
leaving the small `index <file>` / `index <package>` calls untouched.
