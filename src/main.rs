//! terva `index` extension (Rust).
//!
//! Registers a single read-only tool, `index`, that emits a file's
//! tree-sitter skeleton — imports plus type/function/class signatures with
//! exact line ranges — so the model can understand a file's structure
//! cheaply before spending tokens on a full `read`.
//!
//! The wire protocol lives in `terva-extsdk` (this repo was the extension
//! that proved the extraction — see docs/rust-sdk-extraction.md): the SDK
//! owns the handshake, LF-JSON framing with the 4 MiB cap, `session_start`
//! cwd-following, panic-isolated dispatch, and the self-`Jail`. This file
//! is the `index` domain only: the tool's schema and guidance, path
//! resolution and miss reporting, and the file/directory outline dispatch.
//!
//! HARD RULE: stdout is the protocol wire. Every diagnostic byte goes to
//! stderr (terva captures it to $TERVA_HOME/logs/ext-index.log). One stray
//! stdout write corrupts the JSON stream.
//!
//! Sandbox: like every terva extension, `index` self-jails. It only outlines
//! files inside the workspace (`cwd`) or its own data/extension dirs; there
//! is no unjail.

mod outline;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use terva_extsdk::{read_bounded, Authority, Extension, Jail, JailError, Tool, ToolResult};

/// Kept equal to Cargo.toml's `version` and extension.json's `version` by the
/// `version_matches_manifests` test below; bump all three together on release.
const VERSION: &str = "0.8.2";

/// We use the protocol-2 `session_start` event to follow `cwd` across `/cd`
/// (so the jail tracks the live working directory). That is the lowest host
/// protocol we actually require; declaring more would refuse compatible hosts.
const MIN_PROTOCOL: i64 = 2;

/// Upper bound on the caller-supplied `depth`, so a runaway value can't drive
/// deep recursion. The output cap still applies on top of this.
const MAX_DEPTH: u64 = 8;

const TOOL_DESC: &str = "Outline a file's structure with exact line ranges — code as imports + \
     type/function/class signatures, shell scripts as functions + variable names, \
     Makefiles as targets, Dockerfiles as build stages, XML/HTML as their element \
     tree and CSS as its selectors, \
     JSON/YAML/TOML as their key hierarchy, INI/.env \
     as key names with values omitted, CSV/logs summarized — or pass a \
     directory: a small package maps every file's skeleton, while a large tree \
     returns a file/directory map instead — `index` a specific file or \
     subdirectory to expand that to skeletons. Optional `depth` (default 1) \
     descends that many nesting \
     levels — 0 = top-level only, 2+ = members of members. Prefer index before \
     read (~70-90% cheaper); then read offset/limit on the specific range you \
     need. Confined to the workspace; falls back with an error for unsupported, \
     oversized, or out-of-workspace paths.";

const STANDING_CONTEXT: &str =
    "Prefer `index` before `read`: it returns a file's skeleton with exact line \
     ranges for ~70-90% fewer tokens than a full read. Use the line range it \
     gives you to follow up with `read` offset/limit on just the part you need. \
     Supported: Rust, Go, Python, JavaScript, TypeScript/TSX, Java, C, C++, Ruby \
     (imports + signatures), shell (sh/bash/zsh: functions + sourced files, with \
     variable names only), Makefile (targets), Dockerfile (build stages and \
     their instructions), XML (element tree with text/attributes; \
     `.xml`/`.xsd`/`.csproj`/`.plist`/…), HTML (element tree, with script/link \
     references as imports), CSS (selectors and at-rules), \
     Markdown (headings as a table of contents), \
     JSON/YAML (key hierarchy with array/sequence lengths; long values elided \
     and values under secret-looking key names redacted), \
     TOML/INI (sections + key names), .env (key names only — values never \
     shown), CSV/TSV (header + column/row summary), and logs/text (line counts + \
     first/last line + severity scan). Pass a directory to map it: a small \
     package returns every file's skeleton; a large tree returns a \
     file/directory map instead — then `index` a specific file or subdirectory \
     to expand it to skeletons, so you never pay for a whole tree's skeletons in \
     one call. \
     `index` is confined to the workspace; for other file types, files over \
     ~2 MB, or files outside the project, it returns an error telling you to use \
     `read`.";

/// The tool's JSON Schema. Written on ONE line by SDK requirement: the
/// schema passes to the wire byte-exact (never re-serialized), and the wire
/// is LF-delimited — a pretty-printed schema would corrupt the stream.
const SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"file or directory to outline (inside the workspace)"},"depth":{"type":"integer","minimum":0,"description":"nesting levels to descend (default 1; 0 = top-level only)"}},"required":["path"]}"#;

fn main() {
    if let Err(e) = extension().run() {
        // A real IO error on the wire, or a refused handshake (EOF and
        // shutdown are clean exits inside the SDK's run loop).
        eprintln!("[index] fatal: {e}");
        std::process::exit(1);
    }
}

/// The `index` extension, declared on terva-extsdk. The SDK registers
/// eagerly (hello / register_tool / register_context / subscribe / ready),
/// follows `session_start` so the jail tracks the live `cwd`, and isolates
/// handler panics (one bad file answers an error result instead of taking
/// the tool out for the rest of the session).
fn extension() -> Extension {
    Extension::new("index", VERSION)
        .min_protocol(MIN_PROTOCOL)
        .context(STANDING_CONTEXT)
        .follow_cwd()
        .tool(
            Tool::new("index", TOOL_DESC, SCHEMA)
                .read_only()
                .authority(Authority::LocalRead)
                // Our standing guidance ("prefer `index` before `read`") is
                // always in the model's context, so the tool it names has to
                // be advertised from turn one. Under lazy tool visibility an
                // extension's tools otherwise defer behind an activate_tools
                // round-trip, and the first read usually lands before the
                // model thinks to activate us — exactly the read the guidance
                // exists to prevent. Additive: a host that predates the field
                // ignores it, and one with lazy visibility off advertises
                // everything anyway.
                .essential(),
            |call, host| {
                let path_arg = call.arg_str("path").unwrap_or("");
                let depth = call.arg_u64("depth").unwrap_or(1).min(MAX_DEPTH) as usize;
                let (text, is_error) = handle_index(path_arg, depth, host.cwd(), &host.jail());
                if is_error {
                    ToolResult::error(text)
                } else {
                    ToolResult::text(text)
                }
            },
        )
}

/// Resolve the path, confine it to the jail, read it (bounded), and outline.
/// `depth` is the nesting depth passed to the outliner; `cwd` and `jail` come
/// from the SDK's [`terva_extsdk::Host`] (the jail is the workspace plus the
/// extension's own dirs, falling back to the process cwd when the host never
/// sent one). Returns (text, is_error).
fn handle_index(path_arg: &str, depth: usize, cwd: Option<&Path>, jail: &Jail) -> (String, bool) {
    if path_arg.is_empty() {
        return ("index: missing `path` argument.".into(), true);
    }
    let resolved = resolve_path(path_arg, cwd);

    // Confine to the workspace. This both canonicalizes (resolving symlinks,
    // which defeats a symlink that escapes the workspace) and confirms the
    // result is inside a jail root.
    let canon = match jail.resolve(&resolved) {
        Ok(p) => p,
        // A path that does not exist is answered, not just refused: see
        // `path_miss_message`. Notably it does *not* suggest `read` — `read`
        // hits the same ENOENT one call later.
        Err(JailError::NotFound(e)) => return (path_miss_message(&resolved, &e, jail, cwd), true),
        Err(JailError::Outside) => {
            return (
                format!(
                    "index: {} is outside the workspace; `index` is confined to the project directory. \
                     Use `read` for files outside it (host `read` can be unjailed), or copy the file in first.",
                    resolved.display()
                ),
                true,
            )
        }
    };

    // "Fall back to `read`" is right *here* and wrong for a path miss: the
    // canonicalize above already succeeded, so this is a race or a permission
    // problem, not a missing file, and `read` is a reasonable second opinion.
    // Same for the read error below.
    let meta = match std::fs::metadata(&canon) {
        Ok(m) => m,
        Err(e) => {
            return (
                format!(
                    "index: cannot stat {} ({e}). Fall back to `read`.",
                    canon.display()
                ),
                true,
            )
        }
    };
    if meta.is_dir() {
        return handle_directory(&canon, path_arg, depth);
    }
    if meta.len() as usize > outline::MAX_FILE_BYTES {
        return (
            outline::OutlineError::TooLarge {
                bytes: meta.len() as usize,
            }
            .to_string(),
            true,
        );
    }

    // Bounded read: cap at MAX_FILE_BYTES+1 so a file that grows between the
    // stat above and this read can't pull an unbounded amount into memory.
    let bytes = match read_bounded(&canon, outline::MAX_FILE_BYTES) {
        Ok(Some(b)) => b,
        Ok(None) => {
            return (
                outline::OutlineError::TooLarge {
                    bytes: meta.len() as usize,
                }
                .to_string(),
                true,
            )
        }
        Err(e) => {
            return (
                format!(
                    "index: cannot read {} ({e}). Fall back to `read`.",
                    canon.display()
                ),
                true,
            )
        }
    };

    let ext = dispatch_ext(&canon);
    if let Some(fmt) = outline::format_name(&ext) {
        eprintln!("[index] outlining {} as {}", canon.display(), fmt);
    }
    let display = display_name(path_arg, &canon);

    match outline::outline_with_depth(&display, &ext, &bytes, depth) {
        Ok(skeleton) => (skeleton, false),
        Err(e) => (e.to_string(), true),
    }
}

// --- Path misses -----------------------------------------------------------

/// How many entries a path-miss listing names before it truncates. Deliberately
/// far below `MAP_DIR_FILES` (300): this listing rides inside an error message,
/// and one that renders 300 filenames has re-created the very firehose
/// directory mode exists to avoid.
const MISS_LIST_ENTRIES: usize = 50;

/// Answer a path that does not exist by naming what *does*.
///
/// The old message said "fall back to `read`", which cannot work — `read` hits
/// the identical ENOENT one call later. What the caller needs is the contents
/// of the directory it was aiming at: a missed path is usually structurally
/// right and filename-wrong (`save.go` asked for, only `save_test.go` present),
/// and without the listing the misses chain inside one directory. See
/// `docs/proposals/path-miss-guidance.md`.
fn path_miss_message(resolved: &Path, err: &io::Error, jail: &Jail, cwd: Option<&Path>) -> String {
    let asked = workspace_display(resolved, cwd);

    let Some((dir, missing)) = nearest_existing_ancestor(resolved, jail) else {
        return format!(
            "index: cannot access {asked} ({err}). No directory above it exists inside \
             the workspace; use `glob` to locate the file."
        );
    };
    // `.` is the workspace root itself, where "in ./" reads badly.
    let here = match workspace_display(&dir, cwd) {
        d if d == "." => "the workspace root".to_string(),
        d => format!("{d}/"),
    };

    let Some((names, total)) = list_dir_entries(&dir) else {
        return format!(
            "index: no such path {asked} — `{missing}` is not in {here}, which could not \
             be listed. Use `glob` to locate the file."
        );
    };
    if total == 0 {
        return format!("index: no such path {asked} — {here} is empty.");
    }
    let mut msg = format!(
        "index: no such path {asked} — `{missing}` is not in {here}, which contains: {}",
        render_entries(&names, total)
    );
    if total > names.len() {
        msg.push_str(" Use `glob` if the file you want is not listed.");
    }
    msg
}

/// Walk up from a missing path to the deepest ancestor that exists inside the
/// jail, returning it with the component under it that does not — i.e. the
/// first thing the caller got wrong.
///
/// `Jail::resolve` is a single `canonicalize`, which reports only *that*
/// something in the path is missing, never *which* component, so the walk has
/// to be redone here. It costs one `canonicalize` per component on the error
/// path only, and it self-terminates: above the jail root `resolve` returns
/// `Outside`, so the climb stops at the workspace instead of marching to `/`.
fn nearest_existing_ancestor(resolved: &Path, jail: &Jail) -> Option<(PathBuf, String)> {
    let mut missing = resolved.file_name()?.to_string_lossy().into_owned();
    let mut cur = resolved.parent()?;
    loop {
        match jail.resolve(cur) {
            Ok(canon) if canon.is_dir() => return Some((canon, missing)),
            // An ancestor exists but is a file (ENOTDIR) — nothing to list.
            Ok(_) => return None,
            Err(JailError::Outside) => return None,
            Err(JailError::NotFound(_)) => {
                missing = cur.file_name()?.to_string_lossy().into_owned();
                cur = cur.parent()?;
            }
        }
    }
}

/// One level of `dir`, stat-only, capped at `MISS_LIST_ENTRIES`; returns the
/// names alongside the true total so the caller can render a truncation tail.
/// Directories carry a trailing `/`.
///
/// Unlike the directory walk this lists hidden entries and denied dirs rather
/// than filtering them: it never descends, so `node_modules/` costs one line —
/// and a listing that omits `.env` while the caller is hunting `.env` is worse
/// than a long one.
fn list_dir_entries(dir: &Path) -> Option<(Vec<String>, usize)> {
    let rd = std::fs::read_dir(dir).ok()?;
    // Keep a bounded window and count the rest, rather than materializing every
    // name in a directory that might hold a million of them. The window holds
    // one extra so a sort can still surface the alphabetically-smallest set.
    const WINDOW: usize = MISS_LIST_ENTRIES * 4;
    let mut names: Vec<String> = Vec::new();
    let mut total = 0usize;
    for e in rd.flatten() {
        total += 1;
        if names.len() < WINDOW {
            let mut name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            names.push(name);
        }
    }
    names.sort();
    names.truncate(MISS_LIST_ENTRIES);
    Some((names, total))
}

/// Render a capped listing on one line, with an `… and N more` tail when the
/// directory held more than we show.
fn render_entries(names: &[String], total: usize) -> String {
    let mut s = names.join("  ");
    if total > names.len() {
        s.push_str(&format!(
            "  … and {} more ({total} total).",
            total - names.len()
        ));
    }
    s
}

/// Path as the caller thinks of it: relative to the workspace when it is inside
/// one, else absolute. Tries the raw `cwd` and its canonical form, since a
/// resolved-but-missing path has been through neither.
fn workspace_display(path: &Path, cwd: Option<&Path>) -> String {
    if let Some(cwd) = cwd {
        let roots = [Some(cwd.to_path_buf()), std::fs::canonicalize(cwd).ok()];
        for root in roots.into_iter().flatten() {
            if let Ok(rel) = path.strip_prefix(&root) {
                let rel = rel.to_string_lossy();
                return if rel.is_empty() {
                    ".".to_string()
                } else {
                    rel.into_owned()
                };
            }
        }
    }
    path.display().to_string()
}

// --- Directory mode --------------------------------------------------------

/// A directory request maps a whole subtree, so it needs its own bounds on top
/// of the per-file caps.
const MAX_DIR_FILES: usize = 200; // files actually outlined
const MAX_DIR_BYTES: usize = 256 * 1024; // total skeleton bytes emitted
const MAX_WALK_ENTRIES: usize = 20_000; // directory entries visited during the walk

/// Scale thresholds for map-first directory mode (progressive disclosure). At or
/// below `SKELETON_DIR_FILES` a directory is a "package": emit full skeletons for
/// every file (the original, unchanged behavior). Above it the result would be a
/// firehose, so emit a *map* instead — one line per file. Above `MAP_DIR_FILES`
/// even the flat map is large, so roll it up to one line per immediate child
/// directory. A map/rollup line is a short size hint, not a skeleton, so its
/// length is content-independent — a file *count* bounds the output size
/// accurately, with no need to render-then-measure.
const SKELETON_DIR_FILES: usize = 30;
const MAP_DIR_FILES: usize = 300;

/// Hidden directories the walk descends into anyway.
///
/// The walk skips dotfiles wholesale, which quietly excluded CI configuration —
/// `.github/workflows/*.yml` is YAML we fully support and is often the only
/// description of how a project builds, tests and releases. These are small,
/// hand-written, and exactly the kind of thing someone indexing a repo wants.
/// The `.env` family is allowlisted the same way at the file level.
const ALLOW_HIDDEN_DIRS: &[&str] = &[".github", ".gitlab", ".circleci"];

/// Directory names we never descend into (heavy / generated trees). Hidden
/// entries (dotfiles/dirs like .git, .venv) are skipped separately, except for
/// `ALLOW_HIDDEN_DIRS`.
///
/// Every name here must be *only* ever build output. `bin` used to be on this
/// list and was wrong: Rust binary crates live in `src/bin/`, and a `bin/`
/// directory of scripts is ordinary source — denying it made real files
/// invisible with no way for the caller to tell. A directory of compiled
/// binaries costs nothing to walk anyway, since only supported source
/// extensions are collected.
const DENY_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    "venv",
    "obj",
];

/// A supported source file found by the walk, tagged with the size we already
/// stat'd — so the map and rollup renderers reuse it and never re-stat (and the
/// rollup never reads file bodies at all).
struct WalkFile {
    path: PathBuf,
    size: u64,
}

/// The result of walking a subtree: the supported files found, and whether the
/// entry budget ran out before the walk finished.
///
/// `truncated` exists because the budget used to be a bare `&mut usize` that the
/// walk silently returned on — a 20,500-file tree reported "19999 supported
/// files" as though that were the exact count. Every renderer now marks a
/// truncated walk as a lower bound.
struct Walk {
    files: Vec<WalkFile>,
    budget: usize,
    truncated: bool,
}

impl Walk {
    fn new(budget: usize) -> Walk {
        Walk {
            files: Vec::new(),
            budget,
            truncated: false,
        }
    }

    /// Recursively collect supported source files under `root`, newest bounds
    /// applied. Skips hidden entries, denied dirs, and symlinks (so the walk
    /// cannot loop or escape the workspace).
    fn collect(&mut self, root: &Path) {
        let rd = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            Err(_) => return,
        };
        // Stream the directory rather than collecting it whole: a directory
        // with a million entries would otherwise materialize a million
        // `DirEntry`s before the budget below ever got a look in. We still sort
        // what we keep, so output stays deterministic.
        let mut entries: Vec<std::fs::DirEntry> = Vec::new();
        for entry in rd.flatten() {
            if entries.len() >= self.budget {
                self.truncated = true;
                break;
            }
            entries.push(entry);
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if self.budget == 0 {
                self.truncated = true;
                return;
            }
            self.budget -= 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip hidden entries, but allowlist the common dotfile configs
            // (`.env` family) and CI directories so a walk still surfaces them.
            if name.starts_with('.')
                && !is_env_basename(&name)
                && !ALLOW_HIDDEN_DIRS.contains(&name.as_ref())
            {
                continue; // hidden (.git, .venv, other dotfiles)
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue; // don't follow: avoids loops and jail escapes
            }
            if ft.is_dir() {
                if !DENY_DIRS.contains(&name.as_ref()) {
                    self.collect(&entry.path());
                }
            } else if ft.is_file() {
                let path = entry.path();
                if is_supported(&path) {
                    // One extra stat per supported file, reused by every render
                    // path — the rollup relies on it to avoid reading bodies.
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    self.files.push(WalkFile { path, size });
                }
            }
        }
    }
}

/// The clause appended to a directory header when the walk hit its entry
/// budget, so a truncated count is never read as an exact one.
fn walk_note(truncated: bool) -> String {
    if truncated {
        format!(
            " — walk stopped at {MAX_WALK_ENTRIES} entries, so this is a LOWER BOUND; \
             index a subdirectory for a complete view"
        )
    } else {
        String::new()
    }
}

/// Map a directory subtree with progressive disclosure. A package-sized
/// directory (`<= SKELETON_DIR_FILES`) gets full skeletons, exactly as before; a
/// larger tree gets a *map* (one line per file); a very large tree gets a
/// *rollup* (one line per immediate child directory). So `index <root>` is
/// always a cheap, complete index, and you `index <file>` / `index <subdir>` to
/// expand skeletons only where you look — the file-level structure-before-body
/// loop, applied one level up.
fn handle_directory(dir: &Path, display: &str, depth: usize) -> (String, bool) {
    let mut walk = Walk::new(MAX_WALK_ENTRIES);
    walk.collect(dir);
    let mut files = walk.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let cut = walk.truncated;

    if files.is_empty() {
        // Same shape as a path miss: we know what is in there, so say so rather
        // than sending the caller off to `grep` blind. The listing also explains
        // the result — a directory that is "empty" to `index` because everything
        // in it sits under `node_modules/`, or is a format we don't outline.
        let contents = match list_dir_entries(dir) {
            Some((names, total)) if total > 0 => {
                format!(" It contains: {}", render_entries(&names, total))
            }
            Some(_) => " The directory is empty.".to_string(),
            None => String::new(),
        };
        return (
            format!(
                "index: {display} contains no supported source files to outline.{contents} \
                 Use `grep`/`read` to explore it."
            ),
            true,
        );
    }

    // Map and rollup ignore `depth`: a file/directory listing has no per-file
    // body to deepen, and letting `depth` through would re-inflate exactly the
    // tier meant to be cheap. `depth` keeps its meaning only on the skeleton path.
    if files.len() <= SKELETON_DIR_FILES {
        (render_skeletons(dir, display, depth, &files, cut), false)
    } else if files.len() <= MAP_DIR_FILES {
        (render_file_map(dir, display, &files, cut), false)
    } else {
        (render_dir_rollup(dir, display, &files, cut), false)
    }
}

/// Full-skeleton directory rendering (the original behavior), now used only for
/// a package-sized directory. Still bounded by `MAX_DIR_FILES` / `MAX_DIR_BYTES`
/// as a safety backstop: with `files.len() <= SKELETON_DIR_FILES` the file-count
/// cap never bites, but the byte cap still guards a package of unusually large
/// files. Output is byte-for-byte what directory mode emitted before map-first.
fn render_skeletons(
    dir: &Path,
    display: &str,
    depth: usize,
    files: &[WalkFile],
    truncated: bool,
) -> String {
    let total = files.len();
    let mut body = String::new();
    let mut shown = 0usize;
    for f in files {
        if shown >= MAX_DIR_FILES || body.len() >= MAX_DIR_BYTES {
            break;
        }
        let bytes = match read_bounded(&f.path, outline::MAX_FILE_BYTES) {
            Ok(Some(b)) => b,
            _ => continue, // unreadable or grew past the cap: skip, keep going
        };
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        let ext = dispatch_ext(&f.path);
        // Annotate the header with a size hint so the model can tell, in one
        // call, which files are cheap to `read` — no separate `wc -l` / `ls -l`
        // round-trip. `bytes.len()` is the true size (read_bounded already
        // skipped anything over the cap), so both numbers come from the buffer
        // we already hold.
        let header = format!(
            "{} ({} lines, {})",
            rel.to_string_lossy(),
            line_count(&bytes),
            outline::human_bytes(bytes.len())
        );
        if let Ok(sk) = outline::outline_with_depth(&header, &ext, &bytes, depth) {
            body.push_str(&sk);
            body.push('\n');
            shown += 1;
        }
    }

    let note = walk_note(truncated);
    let mut out = if shown < total {
        format!("{display} — {total} supported files (showing {shown}; index a specific file or subdirectory for the rest){note}\n\n")
    } else {
        format!("{display} — {total} supported files{note}\n\n")
    };
    out.push_str(&body);
    out
}

/// Total bytes the map will read to compute line counts. A line count needs the
/// whole file, and the map tier runs to `MAP_DIR_FILES` (300) files of up to
/// `MAX_FILE_BYTES` (2 MB) each — so an unbudgeted map could do 600 MB of I/O to
/// render a "cheap" listing. The rollup tier was made stat-only for exactly this
/// reason; this puts a ceiling on the tier that still reads. Past the budget the
/// count shows `?`, which the format already renders for an unreadable file.
const MAP_LINE_COUNT_BYTES: usize = 8 * 1024 * 1024;

/// A complete map of every supported file under `dir`: one line each with a
/// `(lines, size)` hint, no skeleton body. The header tells the model how to
/// expand it. Line counts are read per file within `MAP_LINE_COUNT_BYTES`; the
/// byte size always comes from the size stat'd during the walk, so every file
/// keeps a size hint even past the budget. Every file the walk found is listed —
/// and if the walk itself was cut short, the header says so rather than passing
/// a partial count off as complete.
fn render_file_map(dir: &Path, display: &str, files: &[WalkFile], truncated: bool) -> String {
    let mut out = format!(
        "{}\n(map only — `index <file>` or `index <subdir>` for skeletons)\n\n",
        scale_header(dir, display, files, truncated),
    );
    let mut io_budget = MAP_LINE_COUNT_BYTES;
    let mut unread = 0usize;
    for f in files {
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        // The line count needs the file's bytes; an unreadable, oversized or
        // over-budget file has none, so show `?` rather than `0` — a bare
        // `0 lines` next to a multi-MB size reads like an empty file.
        let lines = if f.size as usize > io_budget {
            unread += 1;
            "?".to_string()
        } else {
            match read_bounded(&f.path, outline::MAX_FILE_BYTES) {
                Ok(Some(b)) => {
                    io_budget = io_budget.saturating_sub(b.len());
                    line_count(&b).to_string()
                }
                _ => "?".to_string(),
            }
        };
        out.push_str(&format!(
            "{} ({lines} lines, {})\n",
            rel.to_string_lossy(),
            outline::human_bytes(f.size as usize),
        ));
    }
    // Say which cap produced the `?`s rather than leaving them unexplained.
    if unread > 0 {
        out.push_str(&format!(
            "\n({unread} line counts skipped past a {} read budget — sizes above are exact)\n",
            outline::human_bytes(MAP_LINE_COUNT_BYTES)
        ));
    }
    out
}

/// Roll a large tree up to one line per immediate child (a subdirectory, or a
/// file sitting directly in `dir`), summing supported-file counts and the bytes
/// stat'd during the walk. Reads nothing — even a monorepo root renders in a
/// couple of KB — and `index <child>` descends into any group.
fn render_dir_rollup(dir: &Path, display: &str, files: &[WalkFile], truncated: bool) -> String {
    let mut groups: HashMap<String, (usize, u64)> = HashMap::new();
    for f in files {
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        let mut comps = rel.components();
        let first = match comps.next() {
            Some(c) => c.as_os_str().to_string_lossy().into_owned(),
            None => continue,
        };
        // More than one component => the file lives under a subdirectory.
        let label = if comps.next().is_some() {
            format!("{first}/")
        } else {
            first
        };
        let entry = groups.entry(label).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += f.size;
    }
    // Deterministic output: HashMap iteration order is unspecified, so sort the
    // group labels.
    let mut order: Vec<&String> = groups.keys().collect();
    order.sort();

    let mut out = format!(
        "{}\n(rolled up — `index <subdir>` to descend)\n\n",
        scale_header(dir, display, files, truncated),
    );
    for label in order {
        let (count, bytes) = groups[label];
        let file_word = if count == 1 { "file" } else { "files" };
        out.push_str(&format!(
            "{label:<28} {count:>5} {file_word:<5} {}\n",
            outline::human_bytes(bytes as usize),
        ));
    }
    out
}

/// The header line shared by the map and rollup tiers: `<display> — N supported
/// files in M directories`, where M is the number of distinct directories (root
/// counts as one) that hold a supported file — a cheap scale hint.
/// A truncated walk renders its count as `N+`, so the number itself carries the
/// caveat even if the trailing note is skimmed past.
fn scale_header(dir: &Path, display: &str, files: &[WalkFile], truncated: bool) -> String {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for f in files {
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        seen.insert(rel.parent().unwrap_or(Path::new("")).to_path_buf());
    }
    let dirs = seen.len();
    // `1+` means "at least one", so it takes the plural — "1+ directory" reads
    // like a typo.
    let dir_word = if dirs == 1 && !truncated {
        "directory"
    } else {
        "directories"
    };
    let plus = if truncated { "+" } else { "" };
    format!(
        "{display} — {}{plus} supported files in {dirs}{plus} {dir_word}{}",
        files.len(),
        walk_note(truncated)
    )
}

/// Number of lines in `bytes`: the newline count, plus one for a final line
/// with no trailing newline (so `a\nb` and `a\nb\n` both report 2). An empty
/// file reports 0.
fn line_count(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// The extension `index` dispatches on. Normally the file extension — but a
/// few formats are identified by FILENAME instead, because their extension is
/// unusable (`.env` has none, `.env.local`'s is `local`) or absent entirely
/// (`Makefile`, `Dockerfile`). Those map to a pseudo-extension the outliner
/// recognizes, and this is the one place that filename rule lives.
fn dispatch_ext(path: &Path) -> String {
    if let Some(pseudo) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(basename_ext)
    {
        return pseudo.to_string();
    }
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

/// The pseudo-extension for a filename-identified format, if `name` is one.
fn basename_ext(name: &str) -> Option<&'static str> {
    if is_env_basename(name) {
        return Some("env");
    }
    // Case-insensitive: `makefile` and `Makefile` are both common, and
    // `Dockerfile.prod` / `Containerfile` are the usual variants.
    let lower = name.to_ascii_lowercase();
    if lower == "makefile" || lower == "gnumakefile" {
        return Some("mk");
    }
    if lower == "dockerfile"
        || lower == "containerfile"
        || lower.starts_with("dockerfile.")
        || lower.starts_with("containerfile.")
    {
        return Some("dockerfile");
    }
    None
}

/// True for the dotenv family (`.env`, `.env.local`, `.env.production`, …),
/// matched by filename because the extension is unreliable.
fn is_env_basename(name: &str) -> bool {
    name == ".env" || name.starts_with(".env.")
}

/// Whether the directory walk should outline this file.
fn is_supported(path: &Path) -> bool {
    outline::supported_ext(&dispatch_ext(path))
}

/// Join a relative path against the captured cwd; leave absolute paths alone.
fn resolve_path(path_arg: &str, cwd: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(path_arg);
    if p.is_absolute() {
        return p;
    }
    match cwd {
        Some(base) => base.join(p),
        None => p,
    }
}

/// What to print on the skeleton's header line: the basename keeps it compact,
/// matching the spec's `foo.go` example.
fn display_name(path_arg: &str, resolved: &Path) -> String {
    resolved
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path_arg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn tempdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("terva-ext-index-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Drive `handle_index` the way the production handler does: the jail is
    /// the workspace root, falling back to the process cwd when the host
    /// never sent one — mirroring `terva_extsdk::Host::jail`.
    fn run_index(path: &str, depth: usize, cwd: Option<&Path>) -> (String, bool) {
        let jail = match cwd {
            Some(c) => Jail::new([c]),
            None => Jail::new(std::env::current_dir()),
        };
        handle_index(path, depth, cwd, &jail)
    }

    #[test]
    fn resolve_relative_against_cwd() {
        let r = resolve_path("src/foo.rs", Some(Path::new("/proj")));
        assert_eq!(r, PathBuf::from("/proj/src/foo.rs"));
    }

    #[test]
    fn resolve_absolute_ignores_cwd() {
        let r = resolve_path("/abs/foo.rs", Some(Path::new("/proj")));
        assert_eq!(r, PathBuf::from("/abs/foo.rs"));
    }

    #[test]
    fn missing_file_is_error() {
        let (text, is_err) = run_index("/nope/does-not-exist.rs", 1, None);
        assert!(is_err);
        // Nothing above it is inside the jail, so there is no listing to give —
        // but it must still not send the caller to `read`, which fails the same.
        assert!(text.contains("use `glob`"), "{text}");
        assert!(!text.contains("`read`"), "read cannot help here:\n{text}");
    }

    #[test]
    fn path_miss_names_the_siblings() {
        let dir = tempdir("miss-siblings");
        std::fs::create_dir_all(dir.join("store/save")).unwrap();
        std::fs::write(dir.join("store/save/save_test.go"), b"package save\n").unwrap();
        std::fs::write(dir.join("store/save/replay.go"), b"package save\n").unwrap();
        let (text, is_err) = run_index("store/save/save.go", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(text.contains("`save.go` is not in store/save/"), "{text}");
        assert!(text.contains("save_test.go"), "sibling missing:\n{text}");
        assert!(text.contains("replay.go"), "sibling missing:\n{text}");
        assert!(!text.contains("Fall back to `read`"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_miss_climbs_to_the_deepest_existing_parent() {
        let dir = tempdir("miss-climb");
        std::fs::create_dir_all(dir.join("internal")).unwrap();
        std::fs::write(dir.join("internal/real.rs"), b"pub fn r() {}\n").unwrap();
        // Two components missing: `nope/` and the file under it.
        let (text, is_err) = run_index("internal/nope/x.rs", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(
            text.contains("`nope` is not in internal/"),
            "should name the missing component, not the leaf:\n{text}"
        );
        assert!(text.contains("real.rs"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_miss_at_the_workspace_root_reads_naturally() {
        let dir = tempdir("miss-root");
        std::fs::write(dir.join("main.rs"), b"pub fn m() {}\n").unwrap();
        let (text, is_err) = run_index("gone.rs", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(text.contains("not in the workspace root"), "{text}");
        assert!(text.contains("main.rs"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_miss_listing_is_capped() {
        let dir = tempdir("miss-cap");
        let n = MISS_LIST_ENTRIES + 12;
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i:03}.rs")), b"pub fn a() {}\n").unwrap();
        }
        let (text, is_err) = run_index("gone.rs", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(
            text.contains(&format!("… and 12 more ({n} total)")),
            "{text}"
        );
        assert!(text.contains("Use `glob`"), "{text}");
        // The whole point of the cap: this stays a message, not a firehose.
        assert!(
            text.len() < 2_000,
            "listing too large ({} bytes)",
            text.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_miss_lists_hidden_entries() {
        let dir = tempdir("miss-hidden");
        std::fs::write(dir.join(".env.local"), b"K=v\n").unwrap();
        let (text, _) = run_index(".env", 1, Some(&dir));
        assert!(
            text.contains(".env.local"),
            "a listing that hides the answer is worse than a long one:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_miss_outside_the_workspace_lists_nothing() {
        // The climb must stop at the jail root rather than listing a parent of
        // the workspace. `/etc/nope.rs` has an existing parent — outside the jail.
        let dir = tempdir("miss-outside");
        let (text, is_err) = run_index("/etc/nope.rs", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(
            !text.contains("which contains"),
            "leaked a listing outside the workspace:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_path_is_error() {
        let (_t, is_err) = run_index("", 1, None);
        assert!(is_err);
    }

    #[test]
    fn outlines_file_inside_workspace() {
        let dir = tempdir("inside");
        std::fs::write(dir.join("a.rs"), b"pub fn hi() {}\n").unwrap();
        let (text, is_err) = run_index("a.rs", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(text.contains("pub fn hi()"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn depth_arg_controls_nesting() {
        let dir = tempdir("depth");
        std::fs::write(
            dir.join("n.rs"),
            b"mod m {\n    impl S {\n        pub fn deep(&self) {}\n    }\n}\n",
        )
        .unwrap();
        let (shallow, _) = run_index("n.rs", 1, Some(&dir));
        assert!(
            !shallow.contains("fn deep"),
            "depth 1 should not reach it:\n{shallow}"
        );
        let (deep, _) = run_index("n.rs", 2, Some(&dir));
        assert!(
            deep.contains("pub fn deep(&self)"),
            "depth 2 should reach it:\n{deep}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_file_outside_workspace() {
        let work = tempdir("work");
        let elsewhere = tempdir("elsewhere");
        let secret = elsewhere.join("secret.rs");
        std::fs::write(&secret, b"pub fn leak() {}\n").unwrap();
        // Absolute path that exists but is outside the jail root.
        let (text, is_err) = run_index(secret.to_str().unwrap(), 1, Some(&work));
        assert!(is_err, "out-of-workspace read should be refused: {text}");
        assert!(text.contains("outside the workspace"), "{text}");
        std::fs::remove_dir_all(&work).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
    }

    #[test]
    fn refuses_parent_traversal() {
        let work = tempdir("traverse-work");
        let parent = work.parent().unwrap().to_path_buf();
        std::fs::write(parent.join("outside.rs"), b"pub fn x() {}\n").unwrap();
        let (text, is_err) = run_index("../outside.rs", 1, Some(&work));
        assert!(is_err, "`..` traversal should be refused: {text}");
        assert!(text.contains("outside the workspace"), "{text}");
        std::fs::remove_dir_all(&work).ok();
        std::fs::remove_file(parent.join("outside.rs")).ok();
    }

    #[test]
    fn outlines_a_directory_recursively() {
        let dir = tempdir("dirmode");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
        std::fs::write(dir.join("sub/b.py"), b"def b():\n    pass\n").unwrap();
        std::fs::write(dir.join("notes.bin"), b"\x00unsupported\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/c.js"), b"function c() {}\n").unwrap();
        std::fs::write(dir.join(".hidden/d.rs"), b"pub fn d() {}\n").unwrap();

        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(text.contains("supported files"), "summary missing:\n{text}");
        assert!(text.contains("pub fn a()"), "{text}");
        assert!(text.contains("def b()"), "nested file missing:\n{text}");
        // Relative header for the nested file.
        assert!(text.contains("sub/b.py"), "rel header missing:\n{text}");
        // Excluded: unsupported, node_modules, hidden.
        assert!(!text.contains("notes.bin"), "unsupported leaked:\n{text}");
        assert!(!text.contains("function c"), "node_modules leaked:\n{text}");
        assert!(!text.contains("pub fn d()"), "hidden dir leaked:\n{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walks_ci_config_but_not_other_hidden_dirs() {
        let dir = tempdir("cidir");
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        std::fs::create_dir_all(dir.join(".venv/lib")).unwrap();
        std::fs::write(
            dir.join(".github/workflows/ci.yml"),
            b"on: push\njobs: {}\n",
        )
        .unwrap();
        std::fs::write(dir.join(".venv/lib/junk.py"), b"def junk():\n    pass\n").unwrap();
        std::fs::write(dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(
            text.contains(".github/workflows/ci.yml"),
            "CI config still hidden:\n{text}"
        );
        assert!(!text.contains("junk"), ".venv leaked:\n{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walks_bin_directories() {
        // `bin` was in DENY_DIRS, which made Rust binary crates (`src/bin/`)
        // and `bin/` script dirs invisible with no hint they had been skipped.
        let dir = tempdir("bindir");
        std::fs::create_dir_all(dir.join("src/bin")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), b"pub fn lib() {}\n").unwrap();
        std::fs::write(dir.join("src/bin/tool.rs"), b"pub fn tool() {}\n").unwrap();
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(text.contains("src/bin/tool.rs"), "bin/ skipped:\n{text}");
        // Real build output stays denied.
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/gen.rs"), b"pub fn gen() {}\n").unwrap();
        let (text, _) = run_index(".", 1, Some(&dir));
        assert!(!text.contains("gen.rs"), "target/ leaked:\n{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_truncated_walk_says_so() {
        // The budget is exercised directly: exhausting the real MAX_WALK_ENTRIES
        // would mean creating 20k files for one assertion.
        let dir = tempdir("walkcut");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        for i in 0..40 {
            std::fs::write(dir.join(format!("pkg/f{i:03}.rs")), b"pub fn a() {}\n").unwrap();
        }
        let mut walk = Walk::new(10);
        walk.collect(&dir);
        assert!(walk.truncated, "budget exhaustion not reported");
        assert!(
            walk.files.len() < 40,
            "walk should be short: {}",
            walk.files.len()
        );

        // …and the count must not be presented as exact.
        let header = scale_header(&dir, ".", &walk.files, walk.truncated);
        assert!(header.contains('+'), "count not marked partial:\n{header}");
        assert!(header.contains("LOWER BOUND"), "no caveat:\n{header}");

        // A walk that fits its budget stays clean.
        let mut full = Walk::new(MAX_WALK_ENTRIES);
        full.collect(&dir);
        assert!(!full.truncated);
        assert_eq!(full.files.len(), 40);
        let header = scale_header(&dir, ".", &full.files, full.truncated);
        assert!(
            !header.contains('+'),
            "clean walk marked partial:\n{header}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_directory_is_error() {
        let dir = tempdir("emptydir");
        // Extensions outside the supported set (and not on the roadmap).
        std::fs::write(dir.join("image.png"), b"\x89PNG\r\n").unwrap();
        std::fs::write(dir.join("data.bin"), b"\x00\x01\x02").unwrap();
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(is_err, "{text}");
        assert!(text.contains("no supported source files"), "{text}");
        // Naming what *is* there explains the result instead of just refusing.
        assert!(text.contains("image.png"), "{text}");
        assert!(text.contains("data.bin"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_name_recognition() {
        assert!(is_env_basename(".env"));
        assert!(is_env_basename(".env.local"));
        assert!(is_env_basename(".env.production"));
        assert!(!is_env_basename(".envrc"));
        assert!(!is_env_basename(".gitignore"));
        assert_eq!(dispatch_ext(Path::new("/p/.env")), "env");
        assert_eq!(dispatch_ext(Path::new("/p/.env.local")), "env");
        assert_eq!(dispatch_ext(Path::new("/p/config.env")), "env");
        assert_eq!(dispatch_ext(Path::new("/p/foo.rs")), "rs");
    }

    #[test]
    fn env_file_dispatch_and_walk() {
        let dir = tempdir("envwalk");
        std::fs::write(dir.join(".env"), b"SECRET_TOKEN=abc123\nPORT=8080\n").unwrap();
        std::fs::write(dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
        // A direct `index .env` works (dotfile bypasses the walk's hidden filter)
        // and never leaks the value.
        let (text, is_err) = run_index(".env", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(
            text.contains("SECRET_TOKEN") && text.contains("PORT"),
            "{text}"
        );
        assert!(!text.contains("abc123"), "value leaked:\n{text}");
        // The directory walk surfaces the dotfile config despite the hidden filter.
        let (dirtext, _) = run_index(".", 1, Some(&dir));
        assert!(dirtext.contains(".env"), "dotfile not in walk:\n{dirtext}");
        assert!(
            !dirtext.contains("abc123"),
            "value leaked in walk:\n{dirtext}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_mode_shows_size_hints() {
        let dir = tempdir("sizehint");
        // Two lines, no trailing-newline ambiguity.
        std::fs::write(dir.join("a.rs"), b"pub fn a() {}\npub fn b() {}\n").unwrap();
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        // The file's header line carries `(N lines, X B/KB)`.
        assert!(
            text.lines().any(|l| l.starts_with("a.rs (2 lines,")),
            "size hint missing on header:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn small_directory_still_emits_skeletons() {
        let dir = tempdir("smallskel");
        for i in 0..3 {
            std::fs::write(dir.join(format!("f{i}.rs")), b"pub fn hello() {}\n").unwrap();
        }
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        // The package case is unchanged: real skeleton bodies, no map/rollup header.
        assert!(text.contains("pub fn hello()"), "skeleton missing:\n{text}");
        assert!(
            !text.contains("map only"),
            "small dir should not map:\n{text}"
        );
        assert!(
            !text.contains("rolled up"),
            "small dir should not roll up:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn large_directory_returns_a_complete_map() {
        let dir = tempdir("largemap");
        // Over SKELETON_DIR_FILES, under MAP_DIR_FILES -> a flat file map.
        let n = SKELETON_DIR_FILES + 10;
        for i in 0..n {
            std::fs::write(
                dir.join(format!("f{i:03}.rs")),
                b"pub fn a() {}\npub fn b() {}\n",
            )
            .unwrap();
        }
        let (text, is_err) = run_index(".", 1, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(text.contains("map only"), "map header missing:\n{text}");
        // A map carries size hints but no skeleton body.
        assert!(
            !text.contains("pub fn"),
            "skeleton body leaked into map:\n{text}"
        );
        assert!(text.contains("f000.rs ("), "file line missing:\n{text}");
        // Complete: the alphabetically-last file — the one the old
        // sort-then-cap path could silently drop — is present.
        assert!(
            text.contains(&format!("f{:03}.rs (", n - 1)),
            "last file missing (silent truncation regressed):\n{text}"
        );
        // Cheap: a map of ~40 tiny files is a couple of KB, not a firehose.
        assert!(text.len() < 8 * 1024, "map too big: {} bytes", text.len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn very_large_directory_rolls_up_by_child_dir() {
        let dir = tempdir("rollup");
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        let n = MAP_DIR_FILES + 20;
        for i in 0..n {
            let sub = if i % 2 == 0 { "alpha" } else { "beta" };
            std::fs::write(
                dir.join(sub).join(format!("f{i:04}.rs")),
                b"pub fn a() {}\n",
            )
            .unwrap();
        }
        // depth: 2 must NOT re-inflate the rollup — it's depth-blind.
        let (text, is_err) = run_index(".", 2, Some(&dir));
        assert!(!is_err, "{text}");
        assert!(text.contains("rolled up"), "rollup header missing:\n{text}");
        // One line per immediate child dir, not per file.
        assert!(text.contains("alpha/"), "group missing:\n{text}");
        assert!(text.contains("beta/"), "group missing:\n{text}");
        assert!(
            !text.contains("f0000.rs"),
            "per-file line leaked into rollup:\n{text}"
        );
        // Even a 300+ file tree is a couple of KB.
        assert!(
            text.len() < 4 * 1024,
            "rollup too big: {} bytes",
            text.len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_count_handles_trailing_newline() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a\nb\n"), 2);
        assert_eq!(line_count(b"a\nb"), 2); // no trailing newline still counts the last line
        assert_eq!(line_count(b"\n"), 1);
    }

    #[test]
    fn directory_walk_skips_symlinks() {
        let dir = tempdir("dirsym");
        let outside = tempdir("dirsym-out");
        std::fs::write(outside.join("secret.rs"), b"pub fn secret() {}\n").unwrap();
        std::fs::write(dir.join("real.rs"), b"pub fn real() {}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.rs"), dir.join("link.rs")).unwrap();
        let mut walk = Walk::new(MAX_WALK_ENTRIES);
        walk.collect(&std::fs::canonicalize(&dir).unwrap());
        let files = walk.files;
        assert!(
            files.iter().any(|f| f.path.ends_with("real.rs")),
            "{:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(
            !files.iter().any(|f| f.path.ends_with("link.rs")),
            "symlink followed: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn version_matches_manifests() {
        // Cargo.toml
        let cargo = include_str!("../Cargo.toml");
        let cargo_ver = cargo
            .lines()
            .find_map(|l| l.strip_prefix("version = "))
            .map(|v| v.trim().trim_matches('"'))
            .expect("version in Cargo.toml");
        assert_eq!(cargo_ver, VERSION, "Cargo.toml version != code VERSION");

        // extension.json
        let manifest = include_str!("../extension.json");
        let m: Value = serde_json::from_str(manifest).expect("extension.json parses");
        assert_eq!(
            m.get("version").and_then(Value::as_str),
            Some(VERSION),
            "extension.json version != code VERSION"
        );
        assert_eq!(
            m.get("name").and_then(Value::as_str),
            Some("index"),
            "extension.json name must be 'index'"
        );
    }
}
