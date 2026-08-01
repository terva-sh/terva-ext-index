//! terva `index` extension (Rust).
//!
//! Speaks the raw terva extension wire protocol (newline-delimited JSON over
//! stdin/stdout). It registers a single read-only tool, `index`, that emits a
//! file's tree-sitter skeleton — imports plus type/function/class signatures
//! with exact line ranges — so the model can understand a file's structure
//! cheaply before spending tokens on a full `read`.
//!
//! HARD RULE: stdout is the protocol wire. Every diagnostic byte goes to
//! stderr (terva captures it to $TERVA_HOME/logs/ext-index.log). One stray
//! stdout write corrupts the JSON stream.
//!
//! Sandbox: like every terva extension, `index` self-jails. It only outlines
//! files inside the workspace (`cwd`) or its own data/extension dirs; there is
//! no unjail. See `sandbox.rs`.

mod outline;
mod sandbox;

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use sandbox::{Jail, JailError};

/// Kept equal to Cargo.toml's `version` and extension.json's `version` by the
/// `version_matches_manifests` test below; bump all three together on release.
const VERSION: &str = "0.7.0";

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

fn main() {
    if let Err(e) = run() {
        // A real IO error on the wire (EOF is handled inside run()).
        eprintln!("[index] fatal: {e}");
        std::process::exit(1);
    }
}

/// Host-provided locations captured from the handshake. `cwd` refreshes on
/// every `session_start` (e.g. after `/cd`); `data_dir`/`extension_dir` are
/// fixed for the process lifetime.
#[derive(Default)]
struct HostDirs {
    cwd: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    extension_dir: Option<PathBuf>,
}

impl HostDirs {
    /// The jail roots the `index` tool is allowed to read inside: the
    /// workspace plus the extension's own dirs. Falls back to the process cwd
    /// if the host never sent one, so we never end up with an empty (refuse-
    /// everything) jail on a well-behaved host that omitted the field.
    fn jail(&self) -> Jail {
        let mut roots: Vec<PathBuf> = Vec::new();
        match &self.cwd {
            Some(c) => roots.push(c.clone()),
            None => {
                if let Ok(d) = std::env::current_dir() {
                    roots.push(d);
                }
            }
        }
        if let Some(d) = &self.data_dir {
            roots.push(d.clone());
        }
        if let Some(e) = &self.extension_dir {
            roots.push(e.clone());
        }
        Jail::new(roots)
    }
}

fn run() -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // --- Handshake: hello, register_tool, register_context, subscribe, ready.
    // Sent eagerly before hello_ack, matching the Go SDK: the host buffers
    // registrations until `ready` and replies with hello_ack we read below.
    send(
        &mut out,
        &json!({
            "type": "hello",
            "name": "index",
            "version": VERSION,
            "capabilities": ["tools"],
            "min_protocol": MIN_PROTOCOL,
        }),
    )?;
    send(
        &mut out,
        &json!({
            "type": "register_tool",
            "name": "index",
            "description": TOOL_DESC,
            "schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "file or directory to outline (inside the workspace)" },
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "nesting levels to descend (default 1; 0 = top-level only)"
                    }
                },
                "required": ["path"]
            },
            "read_only": true,
            "authority": "local-read",
            // Our standing guidance ("prefer `index` before `read`") is always
            // in the model's context, so the tool it names has to be advertised
            // from turn one. Under lazy tool visibility an extension's tools
            // otherwise defer behind an `activate_tools` round-trip, and the
            // first read usually lands before the model thinks to activate us —
            // exactly the read the guidance exists to prevent. Additive: a host
            // that predates the field ignores it, and one with lazy visibility
            // off advertises everything anyway.
            "essential": true,
        }),
    )?;
    send(
        &mut out,
        &json!({
            "type": "register_context",
            "text": STANDING_CONTEXT,
        }),
    )?;
    // Follow the working directory across `/cd` so the jail never goes stale.
    send(
        &mut out,
        &json!({
            "type": "subscribe",
            "events": ["session_start"],
            "intercept": [],
        }),
    )?;
    send(&mut out, &json!({ "type": "ready" }))?;

    // --- Read loop. ---------------------------------------------------------
    let stdin = io::stdin();
    let mut dirs = HostDirs::default();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF: the host closed the wire. Exit cleanly.
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[index] skipping unparseable frame: {e}");
                continue;
            }
        };
        let ftype = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match ftype {
            "hello_ack" => {
                dirs.cwd = path_field(&frame, "cwd").or(dirs.cwd.take());
                dirs.data_dir = path_field(&frame, "data_dir");
                dirs.extension_dir = path_field(&frame, "extension_dir");
                eprintln!(
                    "[index] ready; cwd={:?} data_dir={:?} extension_dir={:?} protocol={:?}",
                    dirs.cwd,
                    dirs.data_dir,
                    dirs.extension_dir,
                    frame.get("protocol_version")
                );
            }
            "event" => {
                // session_start re-fires on every `/cd`; keep cwd current.
                if frame.get("event").and_then(Value::as_str) == Some("session_start") {
                    if let Some(c) = path_field(&frame, "cwd") {
                        eprintln!("[index] session_start: cwd -> {}", c.display());
                        dirs.cwd = Some(c);
                    }
                }
            }
            "tool_call" => {
                let id = frame
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = frame.get("args");
                let path_arg = args
                    .and_then(|a| a.get("path"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let depth = args
                    .and_then(|a| a.get("depth"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .min(MAX_DEPTH) as usize;
                // One bad file must not take the tool out for the rest of the
                // session. `handle_index` is read-only and owns no shared state,
                // so catching an unwind here loses nothing — the default panic
                // hook has already written the details to stderr, which terva
                // captures to its extension log. (A grammar segfault is still
                // fatal; that is not the failure this guards.)
                let call = std::panic::AssertUnwindSafe(|| handle_index(path_arg, depth, &dirs));
                let (text, is_error) = std::panic::catch_unwind(call).unwrap_or_else(|_| {
                    (
                        format!(
                            "index: internal error while outlining {path_arg} (see the extension \
                             log). The tool is still running; fall back to `read` for this file."
                        ),
                        true,
                    )
                });
                send(
                    &mut out,
                    &json!({
                        "type": "tool_result",
                        "id": id,
                        "content": [{ "type": "text", "text": text }],
                        "is_error": is_error,
                    }),
                )?;
            }
            "shutdown" => {
                send(&mut out, &json!({ "type": "shutdown_ack" }))?;
                return Ok(());
            }
            other => {
                // Unknown / unhandled frame: ignore (panel frames, etc.).
                if !other.is_empty() {
                    eprintln!("[index] ignoring frame type {other:?}");
                }
            }
        }
    }
}

/// Read a non-empty string field as a PathBuf.
fn path_field(frame: &Value, key: &str) -> Option<PathBuf> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Resolve the path, confine it to the jail, read it (bounded), and outline.
/// `depth` is the nesting depth passed to the outliner. Returns (text, is_error).
fn handle_index(path_arg: &str, depth: usize, dirs: &HostDirs) -> (String, bool) {
    if path_arg.is_empty() {
        return ("index: missing `path` argument.".into(), true);
    }
    let resolved = resolve_path(path_arg, dirs.cwd.as_deref());

    // Confine to the workspace. This both canonicalizes (resolving symlinks,
    // which defeats a symlink that escapes the workspace) and confirms the
    // result is inside a jail root.
    let jail = dirs.jail();
    let canon = match jail.resolve(&resolved) {
        Ok(p) => p,
        // A path that does not exist is answered, not just refused: see
        // `path_miss_message`. Notably it does *not* suggest `read` — `read`
        // hits the same ENOENT one call later.
        Err(JailError::NotFound(e)) => return (path_miss_message(&resolved, &e, &jail, dirs), true),
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
fn path_miss_message(resolved: &Path, err: &io::Error, jail: &Jail, dirs: &HostDirs) -> String {
    let asked = workspace_display(resolved, dirs);

    let Some((dir, missing)) = nearest_existing_ancestor(resolved, jail) else {
        return format!(
            "index: cannot access {asked} ({err}). No directory above it exists inside \
             the workspace; use `glob` to locate the file."
        );
    };
    // `.` is the workspace root itself, where "in ./" reads badly.
    let here = match workspace_display(&dir, dirs) {
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
fn workspace_display(path: &Path, dirs: &HostDirs) -> String {
    if let Some(cwd) = dirs.cwd.as_deref() {
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

/// Read up to `max` bytes; `Ok(None)` means the file exceeded the cap.
fn read_bounded(path: &Path, max: usize) -> io::Result<Option<Vec<u8>>> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max {
        Ok(None)
    } else {
        Ok(Some(buf))
    }
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

fn send<W: Write>(out: &mut W, frame: &Value) -> io::Result<()> {
    let mut s = serde_json::to_string(frame).expect("serialize frame");
    s.push('\n');
    out.write_all(s.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("terva-ext-index-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn dirs_for(cwd: Option<PathBuf>) -> HostDirs {
        HostDirs {
            cwd,
            data_dir: None,
            extension_dir: None,
        }
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
        let (text, is_err) = handle_index("/nope/does-not-exist.rs", 1, &dirs_for(None));
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
        let (text, is_err) = handle_index("store/save/save.go", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index("internal/nope/x.rs", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index("gone.rs", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index("gone.rs", 1, &dirs_for(Some(dir.clone())));
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
        let (text, _) = handle_index(".env", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index("/etc/nope.rs", 1, &dirs_for(Some(dir.clone())));
        assert!(is_err, "{text}");
        assert!(
            !text.contains("which contains"),
            "leaked a listing outside the workspace:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_path_is_error() {
        let (_t, is_err) = handle_index("", 1, &dirs_for(None));
        assert!(is_err);
    }

    #[test]
    fn outlines_file_inside_workspace() {
        let dir = tempdir("inside");
        std::fs::write(dir.join("a.rs"), b"pub fn hi() {}\n").unwrap();
        let (text, is_err) = handle_index("a.rs", 1, &dirs_for(Some(dir.clone())));
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
        let (shallow, _) = handle_index("n.rs", 1, &dirs_for(Some(dir.clone())));
        assert!(
            !shallow.contains("fn deep"),
            "depth 1 should not reach it:\n{shallow}"
        );
        let (deep, _) = handle_index("n.rs", 2, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) =
            handle_index(secret.to_str().unwrap(), 1, &dirs_for(Some(work.clone())));
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
        let (text, is_err) = handle_index("../outside.rs", 1, &dirs_for(Some(work.clone())));
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

        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
        assert!(!is_err, "{text}");
        assert!(text.contains("src/bin/tool.rs"), "bin/ skipped:\n{text}");
        // Real build output stays denied.
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/gen.rs"), b"pub fn gen() {}\n").unwrap();
        let (text, _) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".env", 1, &dirs_for(Some(dir.clone())));
        assert!(!is_err, "{text}");
        assert!(
            text.contains("SECRET_TOKEN") && text.contains("PORT"),
            "{text}"
        );
        assert!(!text.contains("abc123"), "value leaked:\n{text}");
        // The directory walk surfaces the dotfile config despite the hidden filter.
        let (dirtext, _) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
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
        let (text, is_err) = handle_index(".", 2, &dirs_for(Some(dir.clone())));
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
    fn session_start_updates_cwd() {
        let mut dirs = HostDirs::default();
        let ev = json!({"type":"event","event":"session_start","cwd":"/new/place"});
        // Mirror the loop's handling.
        if ev.get("event").and_then(Value::as_str) == Some("session_start") {
            if let Some(c) = path_field(&ev, "cwd") {
                dirs.cwd = Some(c);
            }
        }
        assert_eq!(dirs.cwd, Some(PathBuf::from("/new/place")));
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
