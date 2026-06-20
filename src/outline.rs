//! Pure tree-sitter walk: a source file -> a compact skeleton string.
//!
//! The skeleton is: a header line (file name), an optional `imports:` line,
//! then one line per top-level declaration and one level of nested
//! declarations (methods inside a type/impl/class). Each declaration line
//! carries its EXACT 1-based inclusive line range from tree-sitter node rows
//! (extended to cover a leading decorator / `export` wrapper), so the model
//! can follow up with `read offset/limit` on just the part it needs.
//!
//! This module is intentionally free of any protocol/IO concerns so it can be
//! unit-tested in isolation against fixture files.

use tree_sitter::{Node, Parser, Tree};

/// Files larger than this are refused (the model is told to fall back to
/// `read`). ~2 MB per the spec.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Cap on emitted declaration lines. A huge generated file (still under
/// MAX_FILE_BYTES) could otherwise produce a skeleton with tens of thousands
/// of lines — the opposite of this tool's "spend fewer tokens" purpose, and a
/// risk of brushing the host's 1 MiB tool-result / 4 MiB frame limits. Past
/// the cap we stop and emit a truncation marker.
pub const MAX_DECL_LINES: usize = 500;

/// Imports list is capped separately (a file can `use`/`import` a lot).
const MAX_IMPORTS: usize = 40;

/// A single signature line is clipped to this many characters. Guards against
/// a pathological one-line declaration (e.g. a generated function with
/// thousands of parameters) blowing up a single output line.
const MAX_SIG_CHARS: usize = 200;

/// A language we know how to outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Go,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Java,
    C,
    Cpp,
    Ruby,
    Markdown,
}

impl Lang {
    /// Pick a language from a file extension (lowercased, no dot).
    pub fn from_extension(ext: &str) -> Option<Lang> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Lang::Rust,
            "go" => Lang::Go,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "java" => Lang::Java,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            "rb" => Lang::Ruby,
            "md" | "markdown" => Lang::Markdown,
            _ => return None,
        })
    }

    /// Human-readable name (used in stderr diagnostics).
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::Go => "Go",
            Lang::Python => "Python",
            Lang::JavaScript => "JavaScript",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::Java => "Java",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::Ruby => "Ruby",
            Lang::Markdown => "Markdown",
        }
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
        }
    }
}

/// The reasons outlining can decline (mapped to a model-readable error by the
/// caller).
#[derive(Debug)]
pub enum OutlineError {
    /// File extension is not one of the supported languages.
    UnsupportedLanguage { ext: String },
    /// File exceeds MAX_FILE_BYTES.
    TooLarge { bytes: usize },
    /// tree-sitter could not even set the language (should not happen with a
    /// pinned, ABI-matched grammar set).
    Parser(String),
}

impl std::fmt::Display for OutlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutlineError::UnsupportedLanguage { ext } => write!(
                f,
                "index: unsupported file type \".{ext}\" — no tree-sitter grammar for it. Fall back to `read` for this file."
            ),
            OutlineError::TooLarge { bytes } => write!(
                f,
                "index: file is {bytes} bytes (> {} MB limit) — too large to outline. Fall back to `read` (use offset/limit to page through it).",
                MAX_FILE_BYTES / (1024 * 1024)
            ),
            OutlineError::Parser(msg) => write!(
                f,
                "index: internal parser error ({msg}). Fall back to `read` for this file."
            ),
        }
    }
}

/// Outline `source` (the bytes of a file named `display_name`, with extension
/// `ext`). Descends one level of nesting. Returns the skeleton text on success.
///
/// The depth-1 convenience wrapper over [`outline_with_depth`]. The binary
/// always passes an explicit depth, so this is exercised only by the unit
/// tests (hence `allow(dead_code)` in this binary crate).
#[allow(dead_code)]
pub fn outline(display_name: &str, ext: &str, source: &[u8]) -> Result<String, OutlineError> {
    outline_with_depth(display_name, ext, source, 1)
}

/// Like [`outline`], but descends `max_depth` levels of nesting:
/// - `0` = top-level declarations only (no members),
/// - `1` = the default (top-level + one level of members, e.g. methods),
/// - `2`+ = members of members (e.g. methods of an `impl` nested in a `mod`).
///
/// The output cap (`MAX_DECL_LINES`) still applies, so a deep descent on a
/// large file truncates rather than runs away.
pub fn outline_with_depth(
    display_name: &str,
    ext: &str,
    source: &[u8],
    max_depth: usize,
) -> Result<String, OutlineError> {
    if source.len() > MAX_FILE_BYTES {
        return Err(OutlineError::TooLarge {
            bytes: source.len(),
        });
    }
    let lang = Lang::from_extension(ext).ok_or_else(|| OutlineError::UnsupportedLanguage {
        ext: ext.to_string(),
    })?;

    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|e| OutlineError::Parser(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| OutlineError::Parser("parse returned no tree".into()))?;

    Ok(render(display_name, lang, &tree, source, max_depth))
}

/// One emitted declaration line.
struct DeclLine {
    start_row: usize, // 0-based
    end_row: usize,   // 0-based
    indent: usize,    // 0 = top-level, 1 = nested
    text: String,
}

/// Accumulates declaration lines, enforcing MAX_DECL_LINES. Once full it
/// stops accepting and flags `truncated` so the caller can emit a marker; the
/// walk also short-circuits, so a 200k-declaration file costs O(cap), not
/// O(file).
struct DeclSink {
    lines: Vec<DeclLine>,
    truncated: bool,
}

impl DeclSink {
    fn new() -> DeclSink {
        DeclSink {
            lines: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, line: DeclLine) {
        if self.lines.len() >= MAX_DECL_LINES {
            self.truncated = true;
            return;
        }
        self.lines.push(line);
    }

    fn full(&self) -> bool {
        self.truncated
    }
}

fn render(display_name: &str, lang: Lang, tree: &Tree, src: &[u8], max_depth: usize) -> String {
    let root = tree.root_node();
    let mut out = String::new();
    out.push_str(display_name);
    out.push('\n');

    let imports = collect_imports(lang, root, src);
    if !imports.is_empty() {
        out.push_str("  imports: ");
        out.push_str(&imports.join(", "));
        out.push('\n');
    }

    let mut sink = DeclSink::new();
    if lang == Lang::Markdown {
        // Markdown's structure is its headings; collect them directly off
        // heading levels (robust to ATX vs. setext grouping).
        collect_markdown(root, src, max_depth, &mut sink);
    } else {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            collect_decls(lang, child, src, 0, max_depth, &mut sink);
        }
    }

    for l in &sink.lines {
        // 1-based inclusive range.
        let range = format!("[{}-{}]", l.start_row + 1, l.end_row + 1);
        // Pad the range column so signatures line up; two-space base indent
        // plus one extra level per nesting depth.
        out.push_str("  ");
        for _ in 0..l.indent {
            out.push_str("  ");
        }
        out.push_str(&format!("{range:<10} {}\n", l.text));
    }

    if sink.truncated {
        out.push_str(&format!(
            "  … (output truncated at {MAX_DECL_LINES} declarations — read the file directly for the rest)\n"
        ));
    }
    if imports.is_empty() && sink.lines.is_empty() {
        out.push_str("  (no imports or top-level declarations found)\n");
    }

    out
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn collect_imports(lang: Lang, root: Node, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        gather_imports(lang, child, src, &mut out);
    }
    // De-dup while preserving order, and cap the list so a file with a huge
    // import block does not blow the skeleton up.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<String> = out
        .into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    if deduped.len() > MAX_IMPORTS {
        let extra = deduped.len() - MAX_IMPORTS;
        deduped.truncate(MAX_IMPORTS);
        deduped.push(format!("(+{extra} more)"));
    }
    deduped
}

/// Imports are normalized to bare module names / paths across all languages
/// (matching Go's `fmt, strings` style) rather than echoing the raw statement,
/// so the line stays compact and consistent.
fn gather_imports(lang: Lang, node: Node, src: &[u8], out: &mut Vec<String>) {
    let kind = node.kind();
    match lang {
        Lang::Go => {
            // import_declaration wraps import_spec_list / import_spec.
            if kind == "import_declaration" {
                collect_string_literals(node, src, out);
            }
        }
        Lang::Rust => {
            if kind == "use_declaration" {
                if let Some(arg) = node.child_by_field_name("argument") {
                    out.push(text_of(arg, src).replace(char::is_whitespace, ""));
                }
            }
        }
        Lang::Python => {
            // `import a.b, c` -> a.b, c ; `from x.y import z` -> x.y
            if kind == "import_statement" {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    match ch.kind() {
                        "dotted_name" => out.push(text_of(ch, src)),
                        "aliased_import" => {
                            if let Some(n) = ch.child_by_field_name("name") {
                                out.push(text_of(n, src));
                            }
                        }
                        _ => {}
                    }
                }
            } else if kind == "import_from_statement" {
                if let Some(m) = node.child_by_field_name("module_name") {
                    out.push(text_of(m, src));
                }
            }
        }
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if kind == "import_statement" {
                // The `source` field is the module specifier string.
                if let Some(s) = node.child_by_field_name("source") {
                    out.push(unquote(&text_of(s, src)));
                } else {
                    out.push(squash_ws(&text_of(node, src)));
                }
            }
        }
        Lang::Java => {
            if kind == "import_declaration" {
                let full = squash_ws(&text_of(node, src));
                let body = full.strip_prefix("import ").unwrap_or(&full);
                let body = body.strip_prefix("static ").unwrap_or(body);
                out.push(body.trim_end_matches(';').trim().to_string());
            }
        }
        Lang::C | Lang::Cpp => {
            if kind == "preproc_include" {
                if let Some(path) = node.child_by_field_name("path") {
                    out.push(text_of(path, src));
                }
            }
            // C++ `using` / namespace imports are noise; skip.
        }
        Lang::Ruby => {
            // `require "x"` / `require_relative "x"` are method calls.
            if kind == "call" {
                let txt = text_of(node, src);
                if txt.starts_with("require") {
                    if let Some(s) = first_string_literal(node, src) {
                        out.push(s);
                    } else {
                        out.push(squash_ws(&txt));
                    }
                }
            }
        }
        // Markdown has no imports; its structure (headings) is all declarations.
        Lang::Markdown => {}
    }
}

fn collect_string_literals(node: Node, src: &[u8], out: &mut Vec<String>) {
    if node.kind() == "interpreted_string_literal" || node.kind() == "raw_string_literal" {
        let t = text_of(node, src);
        out.push(t.trim_matches(|c| c == '"' || c == '`').to_string());
        return;
    }
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        collect_string_literals(ch, src, out);
    }
}

/// First string-literal descendant's text, unquoted (for Ruby `require`).
fn first_string_literal(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "string" || node.kind().ends_with("string_literal") {
        return Some(unquote(&squash_ws(&text_of(node, src))));
    }
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        if let Some(s) = first_string_literal(ch, src) {
            return Some(s);
        }
    }
    None
}

fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

fn collect_decls(
    lang: Lang,
    node: Node,
    src: &[u8],
    depth: usize,
    max_depth: usize,
    sink: &mut DeclSink,
) {
    collect_decls_at(
        lang,
        node,
        node.start_position().row,
        src,
        depth,
        max_depth,
        sink,
    );
}

/// `start_row` is the row the emitted range should start at. It is normally
/// the node's own start, but when we look through a wrapper (a Python
/// `@decorator` block, a JS `export`) we pass the wrapper's start down so the
/// reported range includes the decorator / export keyword line.
fn collect_decls_at(
    lang: Lang,
    node: Node,
    start_row: usize,
    src: &[u8],
    depth: usize,
    max_depth: usize,
    sink: &mut DeclSink,
) {
    if sink.full() {
        return;
    }
    if let Some((sig, body)) = decl_signature(lang, node, src) {
        sink.push(DeclLine {
            start_row,
            end_row: node.end_position().row,
            indent: depth,
            text: clip(sig),
        });
        // Descend while we have depth budget left (max_depth=1 -> one level).
        if depth < max_depth {
            if let Some(body) = body {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    collect_decls(lang, child, src, depth + 1, max_depth, sink);
                }
            }
        }
    } else if let Some(inner) = transparent_wrapper(lang, node) {
        // Look through one wrapper layer at ANY depth. Doing this only at
        // depth 0 (the old behavior) dropped decorated methods *inside* a
        // class — e.g. every @property / @staticmethod. Keep `start_row` so
        // the decorator line is part of the range.
        collect_decls_at(lang, inner, start_row, src, depth, max_depth, sink);
    }
}

/// Wrappers that we look through to find the real declaration underneath.
fn transparent_wrapper<'a>(lang: Lang, node: Node<'a>) -> Option<Node<'a>> {
    let kind = node.kind();
    let is_wrapper = match lang {
        Lang::Python => kind == "decorated_definition",
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            kind == "export_statement" || kind == "ambient_declaration"
        }
        Lang::Java => kind == "annotation", // rarely top-level; harmless
        _ => false,
    };
    if !is_wrapper {
        return None;
    }
    // Return the most significant named child (the declaration itself).
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| decl_kind_significant(lang, c.kind()));
    found
}

fn decl_kind_significant(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Python => matches!(kind, "function_definition" | "class_definition"),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "lexical_declaration"
                | "variable_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_class_declaration"
        ),
        _ => true,
    }
}

/// Returns (signature line, optional body node to descend into) when `node` is
/// a declaration we emit; None otherwise.
fn decl_signature<'a>(
    lang: Lang,
    node: Node<'a>,
    src: &[u8],
) -> Option<(String, Option<Node<'a>>)> {
    let kind = node.kind();
    match lang {
        Lang::Go => go_decl(node, kind, src),
        Lang::Rust => rust_decl(node, kind, src),
        Lang::Python => python_decl(node, kind, src),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => js_decl(lang, node, kind, src),
        Lang::Java => java_decl(node, kind, src),
        Lang::C => c_decl(node, kind, src),
        Lang::Cpp => cpp_decl(node, kind, src),
        Lang::Ruby => ruby_decl(node, kind, src),
        // Markdown is handled by collect_markdown, not this body-descent path.
        Lang::Markdown => None,
    }
}

// --- Go --------------------------------------------------------------------

fn go_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_declaration" | "method_declaration" => {
            Some((signature_before_body(node, src, "body"), None))
        }
        "type_declaration" => {
            // type_declaration -> type_spec(name, type)
            let mut cursor = node.walk();
            for spec in node.named_children(&mut cursor) {
                if spec.kind() == "type_spec" {
                    let name = spec
                        .child_by_field_name("name")
                        .map(|n| text_of(n, src))
                        .unwrap_or_default();
                    let ty = spec.child_by_field_name("type");
                    let (kw, body) = match ty.map(|t| t.kind()) {
                        Some("struct_type") => ("struct", ty),
                        Some("interface_type") => ("interface", ty),
                        _ => {
                            let inner = ty.map(|t| text_of(t, src)).unwrap_or_default();
                            return Some((format!("type {name} {}", squash_ws(&inner)), None));
                        }
                    };
                    return Some((format!("type {name} {kw}"), body));
                }
            }
            None
        }
        "const_declaration" | "var_declaration" => Some((first_line(&text_of(node, src)), None)),
        _ => None,
    }
}

// --- Rust ------------------------------------------------------------------

fn rust_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_item" => Some((signature_before_body(node, src, "body"), None)),
        "struct_item" | "enum_item" | "union_item" | "trait_item" => {
            // Descend into trait bodies (method signatures); struct/enum bodies
            // are fields, not decls, so no descent.
            let body = if kind == "trait_item" {
                node.child_by_field_name("body")
            } else {
                None
            };
            Some((signature_before_brace(node, src), body))
        }
        "impl_item" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_brace(node, src), body))
        }
        "mod_item" => {
            let body = node.child_by_field_name("body");
            Some((format!("mod {}", name_or(node, src, "name")), body))
        }
        "const_item" | "static_item" | "type_item" => Some((
            first_line(&text_of(node, src))
                .trim_end_matches(';')
                .to_string(),
            None,
        )),
        "macro_definition" => Some((format!("macro_rules! {}", name_or(node, src, "name")), None)),
        _ => None,
    }
}

// --- Python ----------------------------------------------------------------

fn python_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_definition" => Some((signature_before_body(node, src, "body"), None)),
        "class_definition" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_body(node, src, "body"), body))
        }
        _ => None,
    }
}

// --- JS / TS ---------------------------------------------------------------

fn js_decl<'a>(
    _lang: Lang,
    node: Node<'a>,
    kind: &str,
    src: &[u8],
) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            Some((signature_before_body(node, src, "body"), None))
        }
        "class_declaration" | "abstract_class_declaration" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_body(node, src, "body"), body))
        }
        "interface_declaration" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_brace(node, src), body))
        }
        "enum_declaration" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_brace(node, src), body))
        }
        "type_alias_declaration" => Some((first_line(&text_of(node, src)), None)),
        "lexical_declaration" | "variable_declaration" => {
            // Only surface a module-level const/let/var when it binds a
            // function/arrow/class — the structurally interesting case.
            // Trivial scalar bindings (`const A = 1`) are noise; skip them.
            if binds_notable_value(node) {
                Some((first_line(&text_of(node, src)), None))
            } else {
                None
            }
        }
        // Inside a class/interface body:
        "public_field_definition" | "property_signature" | "method_signature" => Some((
            first_line(&text_of(node, src))
                .trim_end_matches([';', ','])
                .to_string(),
            None,
        )),
        _ => None,
    }
}

/// True if a JS/TS `lexical_declaration`/`variable_declaration` has any
/// declarator whose value is a function, arrow, generator, or class.
fn binds_notable_value(node: Node) -> bool {
    let mut cursor = node.walk();
    for declarator in node.named_children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        if let Some(value) = declarator.child_by_field_name("value") {
            if matches!(
                value.kind(),
                "arrow_function"
                    | "function"
                    | "function_expression"
                    | "generator_function"
                    | "class"
                    | "class_expression"
            ) {
                return true;
            }
        }
    }
    false
}

// --- Java ------------------------------------------------------------------

fn java_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_brace(node, src), body))
        }
        "method_declaration" | "constructor_declaration" => {
            Some((signature_before_body(node, src, "body"), None))
        }
        "field_declaration" => Some((
            first_line(&text_of(node, src))
                .trim_end_matches(';')
                .to_string(),
            None,
        )),
        _ => None,
    }
}

// --- C ---------------------------------------------------------------------

fn c_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_definition" => Some((signature_before_body(node, src, "body"), None)),
        "declaration" => {
            // Could be a prototype or a global; keep the first line.
            Some((
                first_line(&text_of(node, src))
                    .trim_end_matches(';')
                    .to_string(),
                None,
            ))
        }
        "struct_specifier" | "enum_specifier" | "union_specifier" => {
            Some((signature_before_brace(node, src), None))
        }
        "type_definition" => Some((
            first_line(&text_of(node, src))
                .trim_end_matches(';')
                .to_string(),
            None,
        )),
        _ => None,
    }
}

// --- C++ -------------------------------------------------------------------

fn cpp_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "function_definition" => Some((signature_before_body(node, src, "body"), None)),
        "declaration" | "field_declaration" => Some((
            first_line(&text_of(node, src))
                .trim_end_matches(';')
                .to_string(),
            None,
        )),
        "class_specifier" | "struct_specifier" => {
            let body = node.child_by_field_name("body");
            Some((signature_before_brace(node, src), body))
        }
        "enum_specifier" | "union_specifier" => Some((signature_before_brace(node, src), None)),
        "namespace_definition" => {
            let body = node.child_by_field_name("body");
            Some((format!("namespace {}", name_or(node, src, "name")), body))
        }
        "template_declaration" => {
            // Surface the templated decl underneath.
            let mut cursor = node.walk();
            for ch in node.named_children(&mut cursor) {
                if let Some((sig, body)) = cpp_decl(ch, ch.kind(), src) {
                    return Some((format!("template {sig}"), body));
                }
            }
            None
        }
        _ => None,
    }
}

// --- Ruby ------------------------------------------------------------------

fn ruby_decl<'a>(node: Node<'a>, kind: &str, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    match kind {
        "method" | "singleton_method" => {
            // Signature is everything up to (but not including) the body.
            Some((ruby_signature(node, src), None))
        }
        "class" | "module" => {
            let body = node.child_by_field_name("body");
            Some((ruby_signature(node, src), body))
        }
        _ => None,
    }
}

fn ruby_signature(node: Node, src: &[u8]) -> String {
    // Take text up to the body field (or first newline) so we don't drag the
    // whole method in.
    if let Some(body) = node.child_by_field_name("body") {
        let start = node.start_byte();
        let end = body.start_byte();
        return squash_ws(&String::from_utf8_lossy(&src[start..end]));
    }
    first_line(&text_of(node, src))
}

// --- Markdown --------------------------------------------------------------

/// A heading found in the document, in source order.
struct MdHeading {
    level: usize,     // 1..=6 (h1..h6)
    start_row: usize, // 0-based, the heading's first line
    text: String,     // "## Quick start" (ATX, markers kept) or the setext title
}

/// Markdown outline: every heading becomes a declaration. The block grammar
/// groups ATX headings into nested `section`s but leaves setext headings flat,
/// so rather than trust that grouping we collect headings directly and rebuild
/// the hierarchy from their levels. Indent reflects nesting relative to the
/// shallowest level present; each heading's range spans its whole section (down
/// to the next heading of equal-or-higher level) so a follow-up `read` lands on
/// that section's body. `depth` caps the relative nesting shown (0 = top
/// headings only, 1 = + one level, …).
fn collect_markdown(root: Node, src: &[u8], max_depth: usize, sink: &mut DeclSink) {
    let mut headings = Vec::new();
    gather_headings(root, src, &mut headings);

    let last_row = root.end_position().row;
    let mut ancestors: Vec<usize> = Vec::new(); // levels of enclosing headings
    for i in 0..headings.len() {
        if sink.full() {
            return;
        }
        let level = headings[i].level;
        while ancestors.last().is_some_and(|&l| l >= level) {
            ancestors.pop();
        }
        let depth = ancestors.len();
        ancestors.push(level);
        if depth > max_depth {
            continue;
        }
        // Section span: to the row before the next equal-or-higher heading.
        let end_row = headings[i + 1..]
            .iter()
            .find(|h| h.level <= level)
            .map(|h| h.start_row.saturating_sub(1))
            .unwrap_or(last_row);
        sink.push(DeclLine {
            start_row: headings[i].start_row,
            end_row: end_row.max(headings[i].start_row),
            indent: depth,
            text: clip(headings[i].text.clone()),
        });
    }
}

fn gather_headings(node: Node, src: &[u8], out: &mut Vec<MdHeading>) {
    match node.kind() {
        "atx_heading" => {
            if let Some(level) = atx_level(node) {
                out.push(MdHeading {
                    level,
                    start_row: node.start_position().row,
                    text: squash_ws(&text_of(node, src)),
                });
            }
            return; // headings don't nest other headings
        }
        "setext_heading" => {
            out.push(MdHeading {
                level: setext_level(node),
                start_row: node.start_position().row,
                text: first_line(&text_of(node, src)),
            });
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        gather_headings(child, src, out);
    }
}

fn atx_level(node: Node) -> Option<usize> {
    let mut cursor = node.walk();
    let level = node
        .named_children(&mut cursor)
        .find_map(|c| match c.kind() {
            "atx_h1_marker" => Some(1),
            "atx_h2_marker" => Some(2),
            "atx_h3_marker" => Some(3),
            "atx_h4_marker" => Some(4),
            "atx_h5_marker" => Some(5),
            "atx_h6_marker" => Some(6),
            _ => None,
        });
    level
}

fn setext_level(node: Node) -> usize {
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        match c.kind() {
            "setext_h1_underline" => return 1,
            "setext_h2_underline" => return 2,
            _ => {}
        }
    }
    1
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn text_of(node: Node, src: &[u8]) -> String {
    String::from_utf8_lossy(&src[node.start_byte()..node.end_byte()]).into_owned()
}

fn name_or(node: Node, src: &[u8], field: &str) -> String {
    node.child_by_field_name(field)
        .map(|n| text_of(n, src))
        .unwrap_or_default()
}

/// Signature = the node's text from its start to the start of its `body_field`,
/// whitespace-squashed. Falls back to the first line if there is no body.
fn signature_before_body(node: Node, src: &[u8], body_field: &str) -> String {
    if let Some(body) = node.child_by_field_name(body_field) {
        let s = &src[node.start_byte()..body.start_byte()];
        return squash_ws(&String::from_utf8_lossy(s))
            .trim_end()
            .to_string();
    }
    first_line(&text_of(node, src))
        .trim_end_matches([';', '{'])
        .trim()
        .to_string()
}

/// Signature = the node's text up to the first `{`, whitespace-squashed.
fn signature_before_brace(node: Node, src: &[u8]) -> String {
    let full = text_of(node, src);
    let head = match full.find('{') {
        Some(i) => &full[..i],
        None => &full,
    };
    squash_ws(head).trim_end().to_string()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim_end().to_string()
}

/// Collapse all runs of whitespace (incl. newlines) to single spaces.
fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Clip a single signature to MAX_SIG_CHARS characters (char-safe), with an
/// ellipsis, so one pathological declaration can't blow up an output line.
fn clip(s: String) -> String {
    if s.chars().count() <= MAX_SIG_CHARS {
        return s;
    }
    let mut t: String = s.chars().take(MAX_SIG_CHARS).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(skeleton: &str) -> Vec<&str> {
        skeleton.lines().collect()
    }

    #[test]
    fn unsupported_language() {
        let err = outline("a.txt", "txt", b"hello").unwrap_err();
        match err {
            OutlineError::UnsupportedLanguage { ext } => assert_eq!(ext, "txt"),
            other => panic!("expected unsupported, got {other:?}"),
        }
        assert!(err_text("a.txt", "txt", b"x").contains("Fall back to `read`"));
    }

    fn err_text(name: &str, ext: &str, src: &[u8]) -> String {
        outline(name, ext, src).unwrap_err().to_string()
    }

    #[test]
    fn too_large() {
        let big = vec![b'a'; MAX_FILE_BYTES + 1];
        let err = outline("big.rs", "rs", &big).unwrap_err();
        assert!(matches!(err, OutlineError::TooLarge { .. }));
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(Lang::from_extension("RS"), Some(Lang::Rust));
        assert_eq!(Lang::from_extension("tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::from_extension("hpp"), Some(Lang::Cpp));
        assert_eq!(Lang::from_extension("zig"), None);
    }

    #[test]
    fn go_skeleton() {
        let src = br#"package main

import (
	"fmt"
	"strings"
)

func Resolve(args Args) (Resolved, error) {
	return Resolved{}, nil
}

type Sandbox struct {
	root string
}

func (s *Sandbox) CheckPath(path string) error {
	_ = fmt.Sprint(strings.TrimSpace(path))
	return nil
}
"#;
        let sk = outline("foo.go", "go", src).unwrap();
        let lines = rows(&sk);
        assert_eq!(lines[0], "foo.go");
        assert!(sk.contains("imports: fmt, strings"), "imports: {sk}");
        assert!(
            sk.contains("func Resolve(args Args) (Resolved, error)"),
            "{sk}"
        );
        assert!(sk.contains("type Sandbox struct"), "{sk}");
        assert!(
            sk.contains("func (s *Sandbox) CheckPath(path string) error"),
            "{sk}"
        );
        // Exact line ranges (1-based inclusive). Resolve is on lines 8-10.
        assert!(
            sk.contains("[8-10]"),
            "expected [8-10] for Resolve in:\n{sk}"
        );
        // Sandbox struct lines 12-14.
        assert!(
            sk.contains("[12-14]"),
            "expected [12-14] for Sandbox in:\n{sk}"
        );
    }

    #[test]
    fn rust_skeleton_with_nested_methods() {
        let src = r#"use std::fmt;
use std::collections::HashMap;

pub fn resolve(args: Args) -> Result<Resolved, Error> {
    todo!()
}

pub struct Sandbox {
    root: String,
}

impl Sandbox {
    pub fn check_path(&self, path: &str) -> Result<(), Error> {
        Ok(())
    }
    fn helper(&self) {}
}
"#;
        let sk = outline("lib.rs", "rs", src.as_bytes()).unwrap();
        assert!(
            sk.contains("imports: std::fmt, std::collections::HashMap"),
            "{sk}"
        );
        assert!(
            sk.contains("pub fn resolve(args: Args) -> Result<Resolved, Error>"),
            "{sk}"
        );
        assert!(sk.contains("pub struct Sandbox"), "{sk}");
        assert!(sk.contains("impl Sandbox"), "{sk}");
        // Nested method appears indented and with its own range.
        assert!(
            sk.contains("pub fn check_path(&self, path: &str) -> Result<(), Error>"),
            "{sk}"
        );
        assert!(sk.contains("fn helper(&self)"), "{sk}");
        // resolve is lines 4-6.
        assert!(sk.contains("[4-6]"), "{sk}");
    }

    #[test]
    fn python_skeleton() {
        let src = r#"import os
from typing import List

def resolve(args):
    return None

class Sandbox:
    def check_path(self, path):
        return True
"#;
        let sk = outline("mod.py", "py", src.as_bytes()).unwrap();
        // Imports normalized to module names.
        assert!(sk.contains("imports: os, typing"), "{sk}");
        assert!(sk.contains("def resolve(args)"), "{sk}");
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("def check_path(self, path)"), "{sk}");
        // class Sandbox is line 7.
        assert!(
            sk.lines()
                .any(|l| l.contains("[7-9]") && l.contains("class Sandbox")),
            "{sk}"
        );
    }

    #[test]
    fn python_decorated_methods_inside_class_are_kept() {
        // Regression: decorated methods live under `decorated_definition` at
        // depth 1; they must not be dropped.
        let src = r#"class Service:
    @property
    def name(self):
        return self._n

    @staticmethod
    def make():
        return Service()

    def plain(self):
        return 1

@app.route("/x")
def handler():
    return 2
"#;
        let sk = outline("svc.py", "py", src.as_bytes()).unwrap();
        assert!(
            sk.contains("def name(self)"),
            "decorated @property dropped:\n{sk}"
        );
        assert!(
            sk.contains("def make()"),
            "decorated @staticmethod dropped:\n{sk}"
        );
        assert!(sk.contains("def plain(self)"), "{sk}");
        // The decorated top-level fn's range should include the decorator line
        // (line 13), i.e. start at 13 not 14.
        assert!(sk.contains("def handler()"), "{sk}");
        assert!(
            sk.lines()
                .any(|l| l.contains("[13-15]") && l.contains("def handler()")),
            "decorator line should be in range:\n{sk}"
        );
    }

    #[test]
    fn typescript_skeleton() {
        let src = r#"import { foo } from "./foo";

export interface Args {
    path: string;
}

export function resolve(args: Args): Resolved {
    return null as any;
}

export class Sandbox {
    checkPath(path: string): boolean {
        return true;
    }
}
"#;
        let sk = outline("x.ts", "ts", src.as_bytes()).unwrap();
        assert!(sk.contains("imports: ./foo"), "imports normalized: {sk}");
        assert!(sk.contains("interface Args"), "{sk}");
        assert!(
            sk.contains("function resolve(args: Args): Resolved"),
            "{sk}"
        );
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("checkPath(path: string): boolean"), "{sk}");
    }

    #[test]
    fn javascript_skeleton() {
        let src = r#"import fs from "fs";

export function resolve(args) {
    return null;
}

class Sandbox {
    checkPath(path) {
        return true;
    }
}
"#;
        let sk = outline("x.js", "js", src.as_bytes()).unwrap();
        assert!(sk.contains("imports: fs"), "{sk}");
        assert!(sk.contains("function resolve(args)"), "{sk}");
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("checkPath(path)"), "{sk}");
    }

    #[test]
    fn javascript_trivial_consts_are_filtered() {
        let src = r#"const A = 1;
const B = "hello";
export const handler = () => 42;
const Widget = class {};
function real() { return 1; }
"#;
        let sk = outline("x.js", "js", src.as_bytes()).unwrap();
        assert!(
            !sk.contains("const A = 1"),
            "trivial scalar const leaked:\n{sk}"
        );
        assert!(
            !sk.contains("const B"),
            "trivial string const leaked:\n{sk}"
        );
        assert!(
            sk.contains("const handler = () => 42"),
            "arrow const dropped:\n{sk}"
        );
        assert!(
            sk.contains("const Widget = class"),
            "class expr const dropped:\n{sk}"
        );
        assert!(sk.contains("function real()"), "{sk}");
    }

    #[test]
    fn java_skeleton() {
        let src = r#"package com.example;

import java.util.List;

public class Sandbox {
    public void checkPath(String path) {
        return;
    }
}
"#;
        let sk = outline("Sandbox.java", "java", src.as_bytes()).unwrap();
        assert!(sk.contains("imports: java.util.List"), "{sk}");
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("void checkPath(String path)"), "{sk}");
    }

    #[test]
    fn c_skeleton() {
        let src = r#"#include <stdio.h>

struct Sandbox {
    int root;
};

int resolve(int x) {
    return x;
}
"#;
        let sk = outline("a.c", "c", src.as_bytes()).unwrap();
        assert!(
            sk.contains("imports: <stdio.h>") || sk.contains("stdio.h"),
            "{sk}"
        );
        assert!(sk.contains("int resolve(int x)"), "{sk}");
        assert!(sk.contains("struct Sandbox"), "{sk}");
    }

    #[test]
    fn cpp_skeleton() {
        let src = r#"#include <string>

class Sandbox {
public:
    bool checkPath(const std::string& path);
};

int resolve(int x) {
    return x;
}
"#;
        let sk = outline("a.cpp", "cpp", src.as_bytes()).unwrap();
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("int resolve(int x)"), "{sk}");
    }

    #[test]
    fn ruby_skeleton() {
        let src = r#"require "json"

def resolve(args)
  nil
end

class Sandbox
  def check_path(path)
    true
  end
end
"#;
        let sk = outline("a.rb", "rb", src.as_bytes()).unwrap();
        assert!(
            sk.contains("imports: json"),
            "ruby require normalized: {sk}"
        );
        assert!(sk.contains("def resolve(args)"), "{sk}");
        assert!(sk.contains("class Sandbox"), "{sk}");
        assert!(sk.contains("def check_path(path)"), "{sk}");
    }

    #[test]
    fn output_is_capped_with_marker() {
        let mut src = String::new();
        for i in 0..(MAX_DECL_LINES + 50) {
            src.push_str(&format!("pub fn f{i}(x: i32) -> i32 {{ x }}\n"));
        }
        let sk = outline("many.rs", "rs", src.as_bytes()).unwrap();
        let decl_lines = sk.lines().filter(|l| l.contains("pub fn f")).count();
        assert_eq!(
            decl_lines, MAX_DECL_LINES,
            "decl lines not capped: {decl_lines}"
        );
        assert!(
            sk.contains("output truncated at"),
            "truncation marker missing:\n{}",
            sk.lines().last().unwrap_or("")
        );
    }

    #[test]
    fn long_signature_is_clipped() {
        let params: String = (0..400).map(|i| format!("a{i}: i32, ")).collect();
        let src = format!("pub fn wide({params}) {{}}\n");
        let sk = outline("wide.rs", "rs", src.as_bytes()).unwrap();
        let sig_line = sk.lines().find(|l| l.contains("pub fn wide")).unwrap();
        assert!(sig_line.contains('…'), "expected clip ellipsis: {sig_line}");
        // The decl portion (after the range column) stays bounded.
        assert!(
            sig_line.len() < 260,
            "line not clipped: {} chars",
            sig_line.len()
        );
    }

    #[test]
    fn empty_file_reports_no_declarations() {
        let sk = outline("empty.rs", "rs", b"// just a comment\n").unwrap();
        assert!(sk.contains("no imports or top-level declarations"), "{sk}");
    }

    const NESTED: &str = r#"mod m {
    pub struct S;
    impl S {
        pub fn deep(&self) {}
    }
}
"#;

    #[test]
    fn depth_zero_is_top_level_only() {
        let sk = outline_with_depth("n.rs", "rs", NESTED.as_bytes(), 0).unwrap();
        assert!(sk.contains("mod m"), "{sk}");
        assert!(
            !sk.contains("impl S"),
            "depth 0 should not descend into mod:\n{sk}"
        );
        assert!(!sk.contains("fn deep"), "{sk}");
    }

    #[test]
    fn depth_one_matches_default() {
        let with_depth = outline_with_depth("n.rs", "rs", NESTED.as_bytes(), 1).unwrap();
        let default = outline("n.rs", "rs", NESTED.as_bytes()).unwrap();
        assert_eq!(with_depth, default);
        assert!(with_depth.contains("impl S"), "{with_depth}");
        // One level: the impl shows but its method does not.
        assert!(
            !with_depth.contains("fn deep"),
            "depth 1 stops at the impl:\n{with_depth}"
        );
    }

    #[test]
    fn depth_two_descends_into_nested_members() {
        let sk = outline_with_depth("n.rs", "rs", NESTED.as_bytes(), 2).unwrap();
        assert!(sk.contains("impl S"), "{sk}");
        assert!(
            sk.contains("pub fn deep(&self)"),
            "depth 2 should reach impl methods:\n{sk}"
        );
    }

    const MD_DOC: &str =
        "# Title\n\nintro\n\n## Section A\n\nbody\n\n### Sub A1\n\nmore\n\n## Section B\n\nend\n";

    #[test]
    fn markdown_skeleton() {
        let sk = outline("doc.md", "md", MD_DOC.as_bytes()).unwrap();
        // Default depth (1): top heading + one level (## sections), not ###.
        assert!(sk.contains("# Title"), "{sk}");
        assert!(sk.contains("## Section A"), "{sk}");
        assert!(sk.contains("## Section B"), "{sk}");
        assert!(
            !sk.contains("### Sub A1"),
            "depth 1 should stop above h3:\n{sk}"
        );
        // Headings carry exact line ranges, like any other declaration.
        assert!(
            sk.lines().any(|l| l.contains("# Title") && l.contains('[')),
            "{sk}"
        );
        // No "imports:" line for Markdown.
        assert!(!sk.contains("imports:"), "{sk}");
    }

    #[test]
    fn markdown_depth_controls_toc_levels() {
        let shallow = outline_with_depth("doc.md", "md", MD_DOC.as_bytes(), 0).unwrap();
        assert!(shallow.contains("# Title"), "{shallow}");
        assert!(
            !shallow.contains("## Section A"),
            "depth 0 = top headings only:\n{shallow}"
        );

        let deep = outline_with_depth("doc.md", "md", MD_DOC.as_bytes(), 2).unwrap();
        assert!(
            deep.contains("### Sub A1"),
            "depth 2 should reach h3:\n{deep}"
        );
        // Subsection is nested (indented) under its parent.
        let sub = deep.lines().find(|l| l.contains("### Sub A1")).unwrap();
        let sec = deep.lines().find(|l| l.contains("## Section A")).unwrap();
        let indent = |l: &str| l.len() - l.trim_start().len();
        assert!(
            indent(sub) > indent(sec),
            "h3 should indent past its h2:\n{deep}"
        );
    }

    #[test]
    fn markdown_setext_and_extensionless_alias() {
        // `.markdown` maps too; setext headings (underlined) are captured.
        let src = "Title\n=====\n\nSubtitle\n--------\n\nbody\n";
        let sk = outline_with_depth("doc.markdown", "markdown", src.as_bytes(), 2).unwrap();
        assert!(sk.contains("Title"), "{sk}");
        assert!(sk.contains("Subtitle"), "{sk}");
    }
}
