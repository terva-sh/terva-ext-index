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
const VERSION: &str = "0.6.0";

/// We use the protocol-2 `session_start` event to follow `cwd` across `/cd`
/// (so the jail tracks the live working directory). That is the lowest host
/// protocol we actually require; declaring more would refuse compatible hosts.
const MIN_PROTOCOL: i64 = 2;

/// Upper bound on the caller-supplied `depth`, so a runaway value can't drive
/// deep recursion. The output cap still applies on top of this.
const MAX_DEPTH: u64 = 8;

const TOOL_DESC: &str = "Outline a file's structure with exact line ranges — code as imports + \
     type/function/class signatures, JSON/YAML/TOML as their key hierarchy, INI/.env \
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
     (imports + signatures), Markdown (headings as a table of contents), \
     JSON/YAML (key hierarchy with array/sequence lengths; long values elided), \
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
                let (text, is_error) = handle_index(path_arg, depth, &dirs);
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
    let canon = match dirs.jail().resolve(&resolved) {
        Ok(p) => p,
        Err(JailError::NotFound(e)) => {
            return (
                format!(
                    "index: cannot access {} ({e}). Fall back to `read` (it may not exist or be unreadable).",
                    resolved.display()
                ),
                true,
            )
        }
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

/// Directory names we never descend into (heavy / generated trees). Hidden
/// entries (dotfiles/dirs like .git, .venv) are skipped separately.
const DENY_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    "venv",
    "bin",
    "obj",
];

/// A supported source file found by the walk, tagged with the size we already
/// stat'd — so the map and rollup renderers reuse it and never re-stat (and the
/// rollup never reads file bodies at all).
struct WalkFile {
    path: PathBuf,
    size: u64,
}

/// Map a directory subtree with progressive disclosure. A package-sized
/// directory (`<= SKELETON_DIR_FILES`) gets full skeletons, exactly as before; a
/// larger tree gets a *map* (one line per file); a very large tree gets a
/// *rollup* (one line per immediate child directory). So `index <root>` is
/// always a cheap, complete index, and you `index <file>` / `index <subdir>` to
/// expand skeletons only where you look — the file-level structure-before-body
/// loop, applied one level up.
fn handle_directory(dir: &Path, display: &str, depth: usize) -> (String, bool) {
    let mut files = Vec::new();
    let mut budget = MAX_WALK_ENTRIES;
    collect_source_files(dir, &mut files, &mut budget);
    files.sort_by(|a, b| a.path.cmp(&b.path));

    if files.is_empty() {
        return (
            format!(
                "index: {display} contains no supported source files to outline. Use `grep`/`read` to explore it."
            ),
            true,
        );
    }

    // Map and rollup ignore `depth`: a file/directory listing has no per-file
    // body to deepen, and letting `depth` through would re-inflate exactly the
    // tier meant to be cheap. `depth` keeps its meaning only on the skeleton path.
    if files.len() <= SKELETON_DIR_FILES {
        (render_skeletons(dir, display, depth, &files), false)
    } else if files.len() <= MAP_DIR_FILES {
        (render_file_map(dir, display, &files), false)
    } else {
        (render_dir_rollup(dir, display, &files), false)
    }
}

/// Full-skeleton directory rendering (the original behavior), now used only for
/// a package-sized directory. Still bounded by `MAX_DIR_FILES` / `MAX_DIR_BYTES`
/// as a safety backstop: with `files.len() <= SKELETON_DIR_FILES` the file-count
/// cap never bites, but the byte cap still guards a package of unusually large
/// files. Output is byte-for-byte what directory mode emitted before map-first.
fn render_skeletons(dir: &Path, display: &str, depth: usize, files: &[WalkFile]) -> String {
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

    let mut out = if shown < total {
        format!("{display} — {total} supported files (showing {shown}; index a specific file or subdirectory for the rest)\n\n")
    } else {
        format!("{display} — {total} supported files\n\n")
    };
    out.push_str(&body);
    out
}

/// A complete map of every supported file under `dir`: one line each with a
/// `(lines, size)` hint, no skeleton body. The header tells the model how to
/// expand it. Reads each file once for the line count (bounded by
/// `MAP_DIR_FILES`); the byte size comes from the size stat'd during the walk.
/// Every file is listed — nothing is silently dropped.
fn render_file_map(dir: &Path, display: &str, files: &[WalkFile]) -> String {
    let mut out = format!(
        "{}\n(map only — `index <file>` or `index <subdir>` for skeletons)\n\n",
        scale_header(dir, display, files),
    );
    for f in files {
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        // The line count needs the file's bytes; an unreadable or
        // >MAX_FILE_BYTES file has none, so show `?` rather than `0` — a bare
        // `0 lines` next to a multi-MB size reads like an empty file.
        let lines = match read_bounded(&f.path, outline::MAX_FILE_BYTES) {
            Ok(Some(b)) => line_count(&b).to_string(),
            _ => "?".to_string(),
        };
        out.push_str(&format!(
            "{} ({lines} lines, {})\n",
            rel.to_string_lossy(),
            outline::human_bytes(f.size as usize),
        ));
    }
    out
}

/// Roll a large tree up to one line per immediate child (a subdirectory, or a
/// file sitting directly in `dir`), summing supported-file counts and the bytes
/// stat'd during the walk. Reads nothing — even a monorepo root renders in a
/// couple of KB — and `index <child>` descends into any group.
fn render_dir_rollup(dir: &Path, display: &str, files: &[WalkFile]) -> String {
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
        scale_header(dir, display, files),
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
fn scale_header(dir: &Path, display: &str, files: &[WalkFile]) -> String {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for f in files {
        let rel = f.path.strip_prefix(dir).unwrap_or(&f.path);
        seen.insert(rel.parent().unwrap_or(Path::new("")).to_path_buf());
    }
    let dirs = seen.len();
    let dir_word = if dirs == 1 {
        "directory"
    } else {
        "directories"
    };
    format!(
        "{display} — {} supported files in {dirs} {dir_word}",
        files.len()
    )
}

/// Recursively collect supported source files under `root`, sorted within each
/// directory. Skips hidden entries, denied dirs, and symlinks (so the walk
/// cannot loop or escape the workspace). `budget` bounds total entries visited.
fn collect_source_files(root: &Path, out: &mut Vec<WalkFile>, budget: &mut usize) {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip hidden entries, but allowlist the common dotfile configs (`.env`
        // family) so a directory walk still surfaces them.
        if name.starts_with('.') && !is_env_basename(&name) {
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
                collect_source_files(&entry.path(), out, budget);
            }
        } else if ft.is_file() {
            let path = entry.path();
            if is_supported(&path) {
                // One extra stat per supported file, reused by every render
                // path — the rollup relies on it to avoid reading file bodies.
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(WalkFile { path, size });
            }
        }
    }
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

/// The extension `index` dispatches on. Normally the file extension, but the
/// `.env` family has no usable one (`.env` has none; `.env.local`'s is `local`),
/// so those filenames map to `env` — the one place that filename rule lives.
fn dispatch_ext(path: &Path) -> String {
    if is_env_name(path) {
        return "env".to_string();
    }
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

/// True for the dotenv family (`.env`, `.env.local`, `.env.production`, …),
/// matched by filename because the extension is unreliable.
fn is_env_basename(name: &str) -> bool {
    name == ".env" || name.starts_with(".env.")
}

fn is_env_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(is_env_basename)
        .unwrap_or(false)
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
        assert!(text.contains("Fall back to `read`"), "{text}");
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
    fn empty_directory_is_error() {
        let dir = tempdir("emptydir");
        // Extensions outside the supported set (and not on the roadmap).
        std::fs::write(dir.join("image.png"), b"\x89PNG\r\n").unwrap();
        std::fs::write(dir.join("data.bin"), b"\x00\x01\x02").unwrap();
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
        assert!(is_err, "{text}");
        assert!(text.contains("no supported source files"), "{text}");
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
        let mut files = Vec::new();
        let mut budget = MAX_WALK_ENTRIES;
        collect_source_files(
            &std::fs::canonicalize(&dir).unwrap(),
            &mut files,
            &mut budget,
        );
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
