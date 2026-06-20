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

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use sandbox::{Jail, JailError};

/// Kept equal to Cargo.toml's `version` and extension.json's `version` by the
/// `version_matches_manifests` test below; bump all three together on release.
const VERSION: &str = "0.3.3";

/// We use the protocol-2 `session_start` event to follow `cwd` across `/cd`
/// (so the jail tracks the live working directory). That is the lowest host
/// protocol we actually require; declaring more would refuse compatible hosts.
const MIN_PROTOCOL: i64 = 2;

/// Upper bound on the caller-supplied `depth`, so a runaway value can't drive
/// deep recursion. The output cap still applies on top of this.
const MAX_DEPTH: u64 = 8;

const TOOL_DESC: &str =
    "Outline a file's structure — imports + type/function/class signatures with \
     exact line ranges — or pass a directory to map every supported file under \
     it (recursively). Optional `depth` (default 1) descends that many nesting \
     levels — 0 = top-level only, 2+ = members of members. Prefer index before \
     read (~70-90% cheaper); then read offset/limit on the specific range you \
     need. Confined to the workspace; falls back with an error for unsupported, \
     oversized, or out-of-workspace paths.";

const STANDING_CONTEXT: &str =
    "Prefer `index` before `read`: it returns a file's skeleton (imports + \
     signatures with exact line ranges) for ~70-90% fewer tokens than a full \
     read. Use the line range it gives you to follow up with `read` \
     offset/limit on just the part you need. Supported: Rust, Go, Python, \
     JavaScript, TypeScript/TSX, Java, C, C++, Ruby, and Markdown (headings as \
     a table of contents). Pass a directory to map a whole package at once. \
     `index` is confined to the workspace; for other \
     file types, files over ~2 MB, or files outside the project, it returns an \
     error telling you to use `read`.";

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

    let ext = canon.extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some(lang) = outline::Lang::from_extension(ext) {
        eprintln!("[index] outlining {} as {}", canon.display(), lang.name());
    }
    let display = display_name(path_arg, &canon);

    match outline::outline_with_depth(&display, ext, &bytes, depth) {
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

/// Outline every supported source file under `dir` (recursively), each headed
/// by its path relative to `dir`. Deterministic (files are sorted) and bounded
/// by file count, total bytes, and entries visited.
fn handle_directory(dir: &Path, display: &str, depth: usize) -> (String, bool) {
    let mut files = Vec::new();
    let mut budget = MAX_WALK_ENTRIES;
    collect_source_files(dir, &mut files, &mut budget);
    files.sort();

    if files.is_empty() {
        return (
            format!(
                "index: {display} contains no supported source files to outline. Use `grep`/`read` to explore it."
            ),
            true,
        );
    }

    let total = files.len();
    let mut body = String::new();
    let mut shown = 0usize;
    for f in &files {
        if shown >= MAX_DIR_FILES || body.len() >= MAX_DIR_BYTES {
            break;
        }
        let bytes = match read_bounded(f, outline::MAX_FILE_BYTES) {
            Ok(Some(b)) => b,
            _ => continue, // unreadable or grew past the cap: skip, keep going
        };
        let rel = f.strip_prefix(dir).unwrap_or(f);
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Ok(sk) = outline::outline_with_depth(&rel.to_string_lossy(), ext, &bytes, depth) {
            body.push_str(&sk);
            body.push('\n');
            shown += 1;
        }
    }

    let mut out = if shown < total {
        format!("{display} — {total} supported files (showing {shown}; index a subdirectory for the rest)\n\n")
    } else {
        format!("{display} — {total} supported files\n\n")
    };
    out.push_str(&body);
    (out, false)
}

/// Recursively collect supported source files under `root`, sorted within each
/// directory. Skips hidden entries, denied dirs, and symlinks (so the walk
/// cannot loop or escape the workspace). `budget` bounds total entries visited.
fn collect_source_files(root: &Path, out: &mut Vec<PathBuf>, budget: &mut usize) {
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
        if name.starts_with('.') {
            continue; // hidden (.git, .venv, dotfiles)
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
            if path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(outline::Lang::from_extension)
                .is_some()
            {
                out.push(path);
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
        std::fs::write(dir.join("notes.txt"), b"unsupported\n").unwrap();
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
        assert!(!text.contains("notes.txt"), "unsupported leaked:\n{text}");
        assert!(!text.contains("function c"), "node_modules leaked:\n{text}");
        assert!(!text.contains("pub fn d()"), "hidden dir leaked:\n{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_directory_is_error() {
        let dir = tempdir("emptydir");
        std::fs::write(dir.join("notes.txt"), b"plain text\n").unwrap(); // unsupported only
        std::fs::write(dir.join("data.json"), b"{}\n").unwrap();
        let (text, is_err) = handle_index(".", 1, &dirs_for(Some(dir.clone())));
        assert!(is_err, "{text}");
        assert!(text.contains("no supported source files"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
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
        assert!(files.iter().any(|f| f.ends_with("real.rs")), "{files:?}");
        assert!(
            !files.iter().any(|f| f.ends_with("link.rs")),
            "symlink followed: {files:?}"
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
