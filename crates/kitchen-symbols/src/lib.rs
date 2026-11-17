//! Definitions in source files: classes, functions, methods, types.
//!
//! Tree-sitter parses Python, Rust, TypeScript/TSX, JavaScript and Go; other
//! files get a line-based regex fallback. Used for the viewer's outline mode
//! and jump-to-definition.

use tree_sitter::{Language, Node, Parser, Point, Tree};

/// Python's `keyword.kwlist` plus the soft keywords `match`, `case` and `type`.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield", "match", "case", "type",
];
/// Rust's strict and reserved keywords.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv", "try", "typeof",
    "unsized", "virtual", "yield",
];
/// JavaScript's reserved words, strict mode's included; TypeScript uses them too.
const JS_KEYWORDS: &[&str] = &[
    "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "enum", "export",
    "extends", "false", "finally", "for", "function", "if", "implements", "import", "in", "instanceof", "interface", "let", "new", "null",
    "package", "private", "protected", "public", "return", "static", "super", "switch", "this", "throw", "true", "try", "typeof", "var",
    "void", "while", "with", "yield",
];
/// Go's keywords.
const GO_KEYWORDS: &[&str] = &[
    "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import",
    "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var",
];
/// Python comprehensions: each is a scope whose `for` targets stay inside it.
const PY_COMPREHENSIONS: &[&str] = &["list_comprehension", "set_comprehension", "dictionary_comprehension", "generator_expression"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// `class`, `function`, `method`, `struct`, `enum`, `trait`, `impl`,
    /// `interface`, `type`, `module`, `const`, `macro`.
    pub kind: String,
    /// 1-based first line.
    pub line: usize,
    /// 1-based last line (inclusive).
    pub end_line: usize,
    /// 0-based column of the name.
    pub col: usize,
    /// Dotted path of enclosing symbols (`Greeter` for `Greeter.greet`).
    pub parent: Option<String>,
    /// First line of the definition, trimmed.
    pub signature: String,
}

impl Symbol {
    /// `Parent.name`, the key summaries are stored under.
    pub fn path(&self) -> String {
        match &self.parent {
            Some(p) => format!("{p}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// Nesting depth (number of enclosing symbols).
    pub fn depth(&self) -> usize {
        self.parent.as_ref().map(|p| p.split('.').count()).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Python,
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
}

fn lang_for(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "py" | "pyi" => Lang::Python,
        "rs" => Lang::Rust,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn language(lang: Lang) -> Language {
    match lang {
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// The tree-sitter grammar for `path` with its highlights and locals queries,
/// combined the way each grammar's tree-sitter.json does (TypeScript builds
/// on the JavaScript queries).
pub fn highlight_grammar(path: &str) -> Option<(Language, String, String)> {
    use tree_sitter_javascript as js;
    use tree_sitter_typescript as ts;
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if ext == "yaml" || ext == "yml" {
        return Some((tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY.into(), String::new()));
    }
    let lang = lang_for(path)?;
    let (highlights, locals) = match lang {
        Lang::Python => (vec![tree_sitter_python::HIGHLIGHTS_QUERY], vec![]),
        Lang::Rust => (vec![tree_sitter_rust::HIGHLIGHTS_QUERY], vec![]),
        Lang::Go => (vec![tree_sitter_go::HIGHLIGHTS_QUERY], vec![]),
        Lang::JavaScript => (vec![js::JSX_HIGHLIGHT_QUERY, js::HIGHLIGHT_QUERY], vec![js::LOCALS_QUERY]),
        Lang::TypeScript => (vec![ts::HIGHLIGHTS_QUERY, js::HIGHLIGHT_QUERY], vec![ts::LOCALS_QUERY, js::LOCALS_QUERY]),
        Lang::Tsx => (vec![ts::HIGHLIGHTS_QUERY, js::JSX_HIGHLIGHT_QUERY, js::HIGHLIGHT_QUERY], vec![js::LOCALS_QUERY]),
    };
    Some((language(lang), highlights.join("\n"), locals.join("\n")))
}

/// Every definition in `source`, in file order.
pub fn extract(path: &str, source: &str) -> Vec<Symbol> {
    let Some(lang) = lang_for(path) else {
        return fallback(source);
    };
    let Some(tree) = parse(lang, source) else {
        return fallback(source);
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    walk(tree.root_node(), lang, source.as_bytes(), &lines, &mut Vec::new(), &mut out);
    out
}

fn parse(lang: Lang, source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language(lang)).ok()?;
    parser.parse(source, None)
}

fn walk(node: Node, lang: Lang, src: &[u8], lines: &[&str], stack: &mut Vec<String>, out: &mut Vec<Symbol>) {
    let found = classify(node, lang, src);
    if let Some((name, kind, name_node)) = &found {
        let line = node.start_position().row + 1;
        out.push(Symbol {
            name: name.clone(),
            kind: kind.to_string(),
            line,
            end_line: node.end_position().row + 1,
            col: name_node.start_position().column,
            parent: (!stack.is_empty()).then(|| stack.join(".")),
            signature: lines.get(line - 1).map(|l| l.trim().to_string()).unwrap_or_default(),
        });
        stack.push(name.clone());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, lang, src, lines, stack, out);
    }
    if found.is_some() {
        stack.pop();
    }
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// If `node` defines something, its (name, kind, name node).
fn classify<'t>(node: Node<'t>, lang: Lang, src: &[u8]) -> Option<(String, &'static str, Node<'t>)> {
    let named = |kind: &'static str| {
        let n = node.child_by_field_name("name")?;
        Some((text(n, src).to_string(), kind, n))
    };
    match (lang, node.kind()) {
        (Lang::Python, "class_definition") => named("class"),
        (Lang::Python, "function_definition") => {
            let in_class = node
                .parent()
                .and_then(|p| if p.kind() == "decorated_definition" { p.parent() } else { Some(p) })
                .map(|p| p.kind() == "block" && p.parent().map(|g| g.kind() == "class_definition").unwrap_or(false))
                .unwrap_or(false);
            named(if in_class { "method" } else { "function" })
        }
        (Lang::Rust, "function_item" | "function_signature_item") => {
            let in_impl = node.parent().and_then(|p| p.parent()).map(|g| matches!(g.kind(), "impl_item" | "trait_item")).unwrap_or(false);
            named(if in_impl { "method" } else { "function" })
        }
        (Lang::Rust, "struct_item" | "union_item") => named("struct"),
        (Lang::Rust, "enum_item") => named("enum"),
        (Lang::Rust, "trait_item") => named("trait"),
        (Lang::Rust, "mod_item") => named("module"),
        (Lang::Rust, "type_item") => named("type"),
        (Lang::Rust, "const_item" | "static_item") => named("const"),
        (Lang::Rust, "macro_definition") => named("macro"),
        (Lang::Rust, "impl_item") => {
            let t = node.child_by_field_name("type")?;
            let name = match node.child_by_field_name("trait") {
                Some(tr) => format!("{} for {}", text(tr, src), text(t, src)),
                None => text(t, src).to_string(),
            };
            Some((name, "impl", t))
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, kind) => match kind {
            "class_declaration" | "abstract_class_declaration" | "class" => named("class"),
            "function_declaration" | "generator_function_declaration" | "function_signature" => named("function"),
            "method_definition" | "method_signature" | "abstract_method_signature" => named("method"),
            "interface_declaration" => named("interface"),
            "type_alias_declaration" => named("type"),
            "enum_declaration" => named("enum"),
            "variable_declarator" => {
                let value = node.child_by_field_name("value")?;
                if matches!(value.kind(), "arrow_function" | "function_expression" | "function") {
                    named("function")
                } else {
                    None
                }
            }
            _ => None,
        },
        (Lang::Go, "function_declaration") => named("function"),
        (Lang::Go, "method_declaration") => named("method"),
        (Lang::Go, "type_spec") => {
            let kind = match node.child_by_field_name("type").map(|t| t.kind()) {
                Some("struct_type") => "struct",
                Some("interface_type") => "interface",
                _ => "type",
            };
            named(kind)
        }
        _ => None,
    }
}

/// Line-based guesses for languages without a grammar.
fn fallback(source: &str) -> Vec<Symbol> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"^\s*(?:export\s+|pub(?:\([^)]*\))?\s+|public\s+|private\s+|protected\s+|static\s+|async\s+)*(class|def|fn|func|function|struct|interface|enum|trait|module|object|sub)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("valid regex")
    });
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        if let Some(c) = re.captures(line) {
            let kw = &c[1];
            let name = c.get(2).expect("group");
            let kind = match kw {
                "class" | "object" => "class",
                "struct" => "struct",
                "interface" => "interface",
                "enum" => "enum",
                "trait" => "trait",
                "module" => "module",
                _ => "function",
            };
            out.push(Symbol {
                name: name.as_str().to_string(),
                kind: kind.into(),
                line: i + 1,
                end_line: i + 1,
                col: name.start(),
                parent: None,
                signature: line.trim().to_string(),
            });
        }
    }
    out
}

/// Definitions of `name` for the fallback search: [`extract`]'s symbols plus
/// Python module- and class-level assignments, and `self.name =` in methods.
/// The outline and summaries keep using [`extract`].
pub fn definitions_named(path: &str, source: &str, name: &str) -> Vec<Symbol> {
    let mut out: Vec<Symbol> = extract(path, source).into_iter().filter(|s| s.name == name).collect();
    if lang_for(path) == Some(Lang::Python) {
        if let Some(tree) = parse(Lang::Python, source) {
            let lines: Vec<&str> = source.lines().collect();
            py_variables(tree.root_node(), name, source.as_bytes(), &lines, &mut Vec::new(), &mut out);
        }
    }
    out.sort_by_key(|s| s.line);
    out
}

/// Assignments to `name` outside functions, and `self.name =` in methods;
/// `classes` is the enclosing class path.
fn py_variables(node: Node, name: &str, src: &[u8], lines: &[&str], classes: &mut Vec<String>, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_definition" => {
                if let (Some(n), Some(body)) = (child.child_by_field_name("name"), child.child_by_field_name("body")) {
                    classes.push(text(n, src).to_string());
                    py_variables(body, name, src, lines, classes, out);
                    classes.pop();
                }
            }
            "function_definition" => {
                if let Some(body) = child.child_by_field_name("body").filter(|_| !classes.is_empty()) {
                    py_self_attributes(body, name, src, lines, &classes.join("."), out);
                }
            }
            "assignment" => {
                if let Some(left) = child.child_by_field_name("left") {
                    let mut targets = Vec::new();
                    py_targets(left, &mut targets);
                    for t in targets.into_iter().filter(|t| t.kind() == "identifier" && text(*t, src) == name) {
                        let parent = (!classes.is_empty()).then(|| classes.join("."));
                        out.push(assigned(t, child, "variable", parent, src, lines));
                    }
                }
                py_variables(child, name, src, lines, classes, out);
            }
            _ => py_variables(child, name, src, lines, classes, out),
        }
    }
}

/// `self.name = …` anywhere in a method body, nested functions and classes aside.
fn py_self_attributes(node: Node, name: &str, src: &[u8], lines: &[&str], class: &str, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" | "class_definition" | "lambda" => continue,
            "assignment" => {
                if let Some(left) = child.child_by_field_name("left") {
                    let mut targets = Vec::new();
                    py_targets(left, &mut targets);
                    for t in targets.into_iter().filter(|t| t.kind() == "attribute") {
                        let (Some(obj), Some(attr)) = (t.child_by_field_name("object"), t.child_by_field_name("attribute")) else {
                            continue;
                        };
                        if text(obj, src) == "self" && text(attr, src) == name {
                            out.push(assigned(attr, child, "attribute", Some(class.to_string()), src, lines));
                        }
                    }
                }
            }
            _ => {}
        }
        py_self_attributes(child, name, src, lines, class, out);
    }
}

/// A [`Symbol`] for `name_node`, assigned by the statement `stmt`.
fn assigned(name_node: Node, stmt: Node, kind: &str, parent: Option<String>, src: &[u8], lines: &[&str]) -> Symbol {
    let line = name_node.start_position().row + 1;
    Symbol {
        name: text(name_node, src).to_string(),
        kind: kind.to_string(),
        line,
        end_line: stmt.end_position().row + 1,
        col: name_node.start_position().column,
        parent,
        signature: lines.get(line - 1).map(|l| l.trim().to_string()).unwrap_or_default(),
    }
}

/// The identifiers and attributes a binding target assigns, through tuple,
/// list, splat and parameter wrappers.
fn py_targets<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    match node.kind() {
        "identifier" | "attribute" => out.push(node),
        "default_parameter" | "typed_default_parameter" => {
            if let Some(n) = node.child_by_field_name("name") {
                py_targets(n, out);
            }
        }
        "pattern_list" | "tuple_pattern" | "list_pattern" | "tuple" | "list" | "expression_list" | "parenthesized_expression" | "list_splat_pattern"
        | "dictionary_splat_pattern" | "list_splat" | "as_pattern_target" | "parameters" | "lambda_parameters" | "typed_parameter" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() != "type" {
                    py_targets(child, out);
                }
            }
        }
        _ => {}
    }
}

/// A binding the scope walk found: where the name is defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDef {
    /// 1-based.
    pub line: usize,
    /// 1-based character of the raw line.
    pub col: usize,
    /// `parameter`, `variable`, `function`, `class`, `loop variable`, ….
    pub kind: &'static str,
}

/// Where the Python name starting at (`line`, `col`: 1-based, characters) is
/// bound in this file, when that is certain. Attributes (`a.b`), names bound
/// only by an import, builtins and non-Python files give None; the caller
/// then searches elsewhere.
pub fn resolve_local(path: &str, source: &str, line: usize, col: usize) -> Option<LocalDef> {
    if lang_for(path)? != Lang::Python {
        return None;
    }
    let tree = parse(Lang::Python, source)?;
    let src = source.as_bytes();
    let raw = source.split('\n').nth(line.checked_sub(1)?)?;
    let (byte, _) = raw.char_indices().nth(col.checked_sub(1)?)?;
    let at = Point { row: line - 1, column: byte };
    let node = tree.root_node().descendant_for_point_range(at, at)?;
    if node.kind() != "identifier" {
        return None;
    }
    if let Some(p) = node.parent() {
        let field = match p.kind() {
            "attribute" => "attribute",
            "keyword_argument" => "name",
            _ => "",
        };
        if !field.is_empty() && p.child_by_field_name(field) == Some(node) {
            return None;
        }
    }
    let (def, kind) = py_lookup(node, text(node, src), src)?;
    if kind == "import" {
        return None;
    }
    let pos = def.start_position();
    let def_line = source.split('\n').nth(pos.row)?;
    Some(LocalDef { line: pos.row + 1, col: def_line.get(..pos.column)?.chars().count() + 1, kind })
}

/// The binding of `name` that `node` refers to: scopes from the inside out,
/// with `global` and `nonlocal` redirecting the search.
fn py_lookup<'t>(node: Node<'t>, name: &str, src: &[u8]) -> Option<(Node<'t>, &'static str)> {
    let scopes = py_scopes(node);
    let mut functions_only = false;
    for &scope in &scopes {
        if functions_only && matches!(scope.kind(), "class_definition" | "module") {
            continue;
        }
        if scope.kind() == "function_definition" {
            match scope.child_by_field_name("body").and_then(|b| py_declaration(b, name, src)) {
                Some("global_statement") => return py_nearest(&py_bindings(*scopes.last()?, name, src), node),
                Some(_) => {
                    functions_only = true;
                    continue;
                }
                None => {}
            }
        }
        let bindings = py_bindings(scope, name, src);
        if !bindings.is_empty() {
            return py_nearest(&bindings, node);
        }
    }
    None
}

/// The scopes `node` can see, innermost first, ending with the module. A
/// function or lambda counts only from its parameters and body, a class body
/// only when `node` is directly in it.
fn py_scopes(node: Node) -> Vec<Node> {
    let within = |outer: Option<Node>| outer.is_some_and(|o| o.start_byte() <= node.start_byte() && node.end_byte() <= o.end_byte());
    let mut scopes = Vec::new();
    let mut cur = node.parent();
    while let Some(a) = cur {
        let applies = match a.kind() {
            "module" => true,
            "function_definition" | "lambda" => within(a.child_by_field_name("body")) || within(a.child_by_field_name("parameters")),
            "class_definition" => scopes.is_empty() && within(a.child_by_field_name("body")),
            k => PY_COMPREHENSIONS.contains(&k),
        };
        if applies {
            scopes.push(a);
        }
        cur = a.parent();
    }
    scopes
}

/// The `global_statement` or `nonlocal_statement` naming `name` in this
/// function body, if any.
fn py_declaration(node: Node, name: &str, src: &[u8]) -> Option<&'static str> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let found = match child.kind() {
            "function_definition" | "class_definition" | "lambda" => None,
            k @ ("global_statement" | "nonlocal_statement") => {
                let mut c = child.walk();
                child.named_children(&mut c).any(|n| text(n, src) == name).then_some(k)
            }
            k if PY_COMPREHENSIONS.contains(&k) => None,
            _ => py_declaration(child, name, src),
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Where `scope` binds `name`, in file order, with each binding's kind.
fn py_bindings<'t>(scope: Node<'t>, name: &str, src: &[u8]) -> Vec<(Node<'t>, &'static str)> {
    let mut out = Vec::new();
    match scope.kind() {
        "module" => py_collect(scope, name, src, false, &mut out),
        "class_definition" => {
            if let Some(body) = scope.child_by_field_name("body") {
                py_collect(body, name, src, false, &mut out);
            }
        }
        "function_definition" | "lambda" => {
            if let Some(params) = scope.child_by_field_name("parameters") {
                py_bind(params, "parameter", name, src, &mut out);
            }
            if let Some(body) = scope.child_by_field_name("body") {
                py_collect(body, name, src, false, &mut out);
            }
        }
        _ => {
            let mut cursor = scope.walk();
            for clause in scope.named_children(&mut cursor).filter(|c| c.kind() == "for_in_clause") {
                if let Some(left) = clause.child_by_field_name("left") {
                    py_bind(left, "loop variable", name, src, &mut out);
                }
            }
        }
    }
    out
}

/// Bindings of `name` under `node`, not inside nested functions, classes,
/// lambdas or comprehensions; in a comprehension (`walrus_only`) only `:=`
/// binds, since it binds in the enclosing scope.
fn py_collect<'t>(node: Node<'t>, name: &str, src: &[u8], walrus_only: bool, out: &mut Vec<(Node<'t>, &'static str)>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let field = |f: &str| child.child_by_field_name(f);
        match child.kind() {
            "named_expression" => {
                if let Some(n) = field("name") {
                    py_bind(n, "variable", name, src, out);
                }
            }
            k if PY_COMPREHENSIONS.contains(&k) => {
                py_collect(child, name, src, true, out);
                continue;
            }
            "lambda" => continue,
            _ if walrus_only => {}
            "function_definition" | "class_definition" => {
                if let Some(n) = field("name") {
                    py_bind(n, if child.kind() == "class_definition" { "class" } else { "function" }, name, src, out);
                }
                continue;
            }
            "assignment" | "augmented_assignment" => {
                if let Some(left) = field("left") {
                    py_bind(left, "variable", name, src, out);
                }
            }
            "for_statement" => {
                if let Some(left) = field("left") {
                    py_bind(left, "loop variable", name, src, out);
                }
            }
            "as_pattern" => {
                let kind = match child.parent().map(|p| p.kind()) {
                    Some("with_item") => "with variable",
                    Some("except_clause") => "exception variable",
                    _ => "variable",
                };
                if let Some(alias) = field("alias") {
                    py_bind(alias, kind, name, src, out);
                }
            }
            "import_statement" | "import_from_statement" => {
                let mut c = child.walk();
                for n in child.children_by_field_name("name", &mut c) {
                    let bound = match n.kind() {
                        "aliased_import" => n.child_by_field_name("alias"),
                        _ => n.named_child(0),
                    };
                    if let Some(b) = bound {
                        py_bind(b, "import", name, src, out);
                    }
                }
                continue;
            }
            "future_import_statement" => continue,
            _ => {}
        }
        py_collect(child, name, src, walrus_only, out);
    }
}

/// Adds the identifiers named `name` that the target `node` binds.
fn py_bind<'t>(node: Node<'t>, kind: &'static str, name: &str, src: &[u8], out: &mut Vec<(Node<'t>, &'static str)>) {
    let mut targets = Vec::new();
    py_targets(node, &mut targets);
    out.extend(targets.into_iter().filter(|t| t.kind() == "identifier" && text(*t, src) == name).map(|t| (t, kind)));
}

/// The last binding at or before `node`, else the first one.
fn py_nearest<'t>(bindings: &[(Node<'t>, &'static str)], node: Node) -> Option<(Node<'t>, &'static str)> {
    let before = bindings.iter().filter(|(b, _)| b.start_byte() <= node.start_byte()).max_by_key(|(b, _)| b.start_byte());
    before.or_else(|| bindings.iter().min_by_key(|(b, _)| b.start_byte())).copied()
}

/// `git` pathspec globs for files of `path`'s language; empty means every file.
pub fn globs_for(path: &str) -> &'static [&'static str] {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "py" | "pyi" => &["*.py", "*.pyi"],
        "rs" => &["*.rs"],
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => &["*.ts", "*.tsx", "*.js", "*.jsx", "*.mjs", "*.cjs"],
        "go" => &["*.go"],
        _ => &[],
    }
}

/// `word` is a reserved word in `path`'s language.
pub fn is_keyword(path: &str, word: &str) -> bool {
    let words = match lang_for(path) {
        Some(Lang::Python) => PYTHON_KEYWORDS,
        Some(Lang::Rust) => RUST_KEYWORDS,
        Some(Lang::TypeScript | Lang::Tsx | Lang::JavaScript) => JS_KEYWORDS,
        Some(Lang::Go) => GO_KEYWORDS,
        None => return false,
    };
    words.contains(&word)
}

/// Modules a Python source imports: `import a.b` -> "a.b"; `from a.b import c`
/// -> "a.b" and "a.b.c". Relative and `__future__` imports are skipped.
pub fn python_imports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(tree) = parse(Lang::Python, source) {
        py_imports(tree.root_node(), source.as_bytes(), &mut out);
    }
    out
}

fn py_imports(node: Node, src: &[u8], out: &mut Vec<String>) {
    let mut push = |m: String| {
        if !out.contains(&m) {
            out.push(m);
        }
    };
    // `a.b` for a dotted_name, or for the name of an aliased_import.
    let dotted = |n: Node| {
        let n = if n.kind() == "aliased_import" { n.child_by_field_name("name").unwrap_or(n) } else { n };
        let mut c = n.walk();
        n.named_children(&mut c).map(|i| text(i, src)).collect::<Vec<_>>().join(".")
    };
    let mut cursor = node.walk();
    match node.kind() {
        "import_statement" => {
            for n in node.children_by_field_name("name", &mut cursor) {
                push(dotted(n));
            }
        }
        "import_from_statement" => {
            let Some(module) = node.child_by_field_name("module_name").filter(|m| m.kind() == "dotted_name") else {
                return;
            };
            let base = dotted(module);
            push(base.clone());
            for n in node.children_by_field_name("name", &mut cursor) {
                push(format!("{base}.{}", dotted(n)));
            }
        }
        "future_import_statement" => {}
        _ => {
            for child in node.named_children(&mut cursor) {
                py_imports(child, src, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[Symbol]) -> Vec<String> {
        v.iter().map(|s| format!("{}:{}:{}", s.path(), s.kind, s.line)).collect()
    }

    #[test]
    fn python() {
        let src = "def hello(name):\n    return 1\n\n\nclass Greeter:\n    @staticmethod\n    def greet(self, who):\n        return hello(who)\n";
        let s = extract("app.py", src);
        assert_eq!(names(&s), ["hello:function:1", "Greeter:class:5", "Greeter.greet:method:7"]);
        assert_eq!(s[1].end_line, 8);
        assert_eq!(s[0].signature, "def hello(name):");
        assert_eq!(s[0].col, 4);
    }

    #[test]
    fn rust() {
        let src = "struct A;\nimpl A {\n    fn new() -> A { A }\n}\nimpl Drop for A {\n    fn drop(&mut self) {}\n}\nfn main() {}\nmod m { pub enum E { X } }\n";
        let s = extract("x.rs", src);
        assert_eq!(
            names(&s),
            ["A:struct:1", "A:impl:2", "A.new:method:3", "Drop for A:impl:5", "Drop for A.drop:method:6", "main:function:8", "m:module:9", "m.E:enum:9"]
        );
    }

    #[test]
    fn typescript_and_js() {
        let src = "export class C {\n  run(): void {}\n}\ninterface I { x: number }\nconst f = (a: number) => a;\nfunction g() {}\n";
        assert_eq!(names(&extract("a.ts", src)), ["C:class:1", "C.run:method:2", "I:interface:4", "f:function:5", "g:function:6"]);
        let js = "class K { m() {} }\nconst h = function() {};\n";
        assert_eq!(names(&extract("a.js", js)), ["K:class:1", "K.m:method:1", "h:function:2"]);
        assert_eq!(names(&extract("a.tsx", "function App() { return <div/>; }\n")), ["App:function:1"]);
    }

    #[test]
    fn go() {
        let src = "package p\n\ntype S struct{}\n\nfunc (s S) M() {}\n\nfunc F() {}\ntype I interface{}\n";
        assert_eq!(names(&extract("a.go", src)), ["S:struct:3", "M:method:5", "F:function:7", "I:interface:8"]);
    }

    #[test]
    fn fallback_languages() {
        let src = "module Foo\n  class Bar\n    def baz\n    end\n  end\nend\n";
        assert_eq!(names(&extract("a.rb", src)), ["Foo:module:1", "Bar:class:2", "baz:function:3"]);
    }

    #[test]
    fn definitions_named_includes_variables() {
        let src = "LIMIT = 10\nx, (y, z) = 1, (2, 3)\n\nclass C:\n    LIMIT: int = 5\n\n    def __init__(self):\n        self.LIMIT = 1\n        LIMIT = 2\n\n\ndef LIMIT():\n    pass\n";
        let d = definitions_named("a.py", src, "LIMIT");
        assert_eq!(names(&d), ["LIMIT:variable:1", "C.LIMIT:variable:5", "C.LIMIT:attribute:8", "LIMIT:function:12"]);
        assert_eq!(d.iter().map(|s| s.col).collect::<Vec<_>>(), [0, 4, 13, 4]);
        assert_eq!(d[0].signature, "LIMIT = 10");
        assert_eq!(d[2].signature, "self.LIMIT = 1");
        let z = definitions_named("a.py", src, "z");
        assert_eq!(names(&z), ["z:variable:2"]);
        assert_eq!(z[0].col, 7);
        assert_eq!(names(&definitions_named("x.rs", "fn f() {}\nfn g() {}\n", "g")), ["g:function:2"]);
    }

    fn at(src: &str, line: usize, col: usize) -> Option<(usize, usize, &'static str)> {
        resolve_local("m.py", src, line, col).map(|d| (d.line, d.col, d.kind))
    }

    #[test]
    fn resolve_local_nearest_binding_before_cursor() {
        let src = "x = 1\nprint(x)\nx = 2\nprint(x)\ndef f(a, *rest, b=1, **kw):\n    return a, rest, b, kw, x\ndef g():\n    return later\nlater = 3\n";
        assert_eq!(at(src, 2, 7), Some((1, 1, "variable")));
        assert_eq!(at(src, 4, 7), Some((3, 1, "variable")));
        assert_eq!(at(src, 3, 1), Some((3, 1, "variable")));
        assert_eq!(at(src, 6, 12), Some((5, 7, "parameter")));
        assert_eq!(at(src, 6, 15), Some((5, 11, "parameter")));
        assert_eq!(at(src, 6, 21), Some((5, 17, "parameter")));
        assert_eq!(at(src, 6, 24), Some((5, 24, "parameter")));
        assert_eq!(at(src, 6, 28), Some((3, 1, "variable")));
        assert_eq!(at(src, 8, 12), Some((9, 1, "variable")));
        assert_eq!(at(src, 8, 14), Some((9, 1, "variable")));
        assert_eq!(at(src, 5, 5), Some((5, 5, "function")));
        assert_eq!(at(src, 0, 1), None);
        assert_eq!(at(src, 2, 40), None);
        assert_eq!(at("t = 'é'; s = 1\nprint('é', s)\n", 2, 12), Some((1, 10, "variable")));
    }

    #[test]
    fn resolve_local_skips_class_scope_in_methods() {
        let src = "name = 'module'\nclass C:\n    name = 'class'\n    alias = name\n    def m(self):\n        return name\n    def n(self):\n        return self.m\n";
        assert_eq!(at(src, 4, 13), Some((3, 5, "variable")));
        assert_eq!(at(src, 6, 16), Some((1, 1, "variable")));
        assert_eq!(at(src, 5, 9), Some((5, 9, "function")));
        assert_eq!(at(src, 8, 16), Some((7, 11, "parameter")));
        assert_eq!(at(src, 2, 7), Some((2, 7, "class")));
    }

    #[test]
    fn resolve_local_global_and_nonlocal() {
        let src = "count = 0\ndef bump():\n    global count\n    count = count + 1\ndef outer():\n    total = 0\n    def inner():\n        nonlocal total\n        total = 1\n        return total\n    return inner\n";
        assert_eq!(at(src, 4, 13), Some((1, 1, "variable")));
        assert_eq!(at(src, 4, 5), Some((1, 1, "variable")));
        assert_eq!(at(src, 10, 16), Some((6, 5, "variable")));
        assert_eq!(at(src, 11, 12), Some((7, 9, "function")));
    }

    #[test]
    fn resolve_local_comprehension_with_and_except_targets() {
        let src = concat!(
            "items = [1, 2]\n",
            "doubled = [i * 2 for i in items if (big := i) > 1]\n",
            "print(big)\n",
            "with open('f') as fh, open('g') as (a, b):\n",
            "    fh.read(); print(a, b)\n",
            "try:\n",
            "    pass\n",
            "except ValueError as err:\n",
            "    print(err)\n",
            "for k, (v, w) in pairs:\n",
            "    print(k, v, w)\n",
            "f = lambda q, *r: q + len(r)\n",
            "print(i)\n",
        );
        assert_eq!(at(src, 2, 12), Some((2, 22, "loop variable")));
        assert_eq!(at(src, 2, 27), Some((1, 1, "variable")));
        assert_eq!(at(src, 2, 44), Some((2, 22, "loop variable")));
        assert_eq!(at(src, 3, 7), Some((2, 37, "variable")));
        assert_eq!(at(src, 5, 5), Some((4, 19, "with variable")));
        assert_eq!(at(src, 5, 25), Some((4, 40, "with variable")));
        assert_eq!(at(src, 9, 11), Some((8, 22, "exception variable")));
        assert_eq!(at(src, 11, 17), Some((10, 12, "loop variable")));
        assert_eq!(at(src, 12, 19), Some((12, 12, "parameter")));
        assert_eq!(at(src, 12, 27), Some((12, 16, "parameter")));
        assert_eq!(at(src, 13, 7), None);
    }

    #[test]
    fn resolve_local_defers_attributes_and_imports() {
        let src = "import os\nfrom typing import List as L\nclass C:\n    def m(self):\n        return os.path, L, self.m, len(self.x)\ndef k(x=1): pass\nk(x=2)\n";
        assert_eq!(at(src, 5, 16), None);
        assert_eq!(at(src, 5, 19), None);
        assert_eq!(at(src, 5, 25), None);
        assert_eq!(at(src, 5, 33), None);
        assert_eq!(at(src, 5, 36), None);
        assert_eq!(at(src, 5, 28), Some((4, 11, "parameter")));
        assert_eq!(at(src, 1, 8), None);
        assert_eq!(at(src, 7, 3), None);
        assert_eq!(resolve_local("a.rs", "let x = 1;\nx\n", 2, 1), None);
    }

    #[test]
    fn globs_and_keywords() {
        assert_eq!(globs_for("a/b.py"), ["*.py", "*.pyi"]);
        assert_eq!(globs_for("b.pyi"), ["*.py", "*.pyi"]);
        assert_eq!(globs_for("x.rs"), ["*.rs"]);
        assert_eq!(globs_for("x.tsx"), ["*.ts", "*.tsx", "*.js", "*.jsx", "*.mjs", "*.cjs"]);
        assert_eq!(globs_for("x.mjs"), globs_for("y.ts"));
        assert_eq!(globs_for("x.go"), ["*.go"]);
        assert!(globs_for("README.md").is_empty());
        assert!(globs_for("Makefile").is_empty());
        assert!(is_keyword("a.py", "def") && is_keyword("a.py", "None") && is_keyword("a.py", "match"));
        assert!(!is_keyword("a.py", "_") && !is_keyword("a.py", "self") && !is_keyword("a.py", "print"));
        assert!(is_keyword("a.rs", "fn") && !is_keyword("a.rs", "main"));
        assert!(is_keyword("a.ts", "function") && is_keyword("a.jsx", "const") && !is_keyword("a.ts", "type"));
        assert!(is_keyword("a.go", "func") && !is_keyword("a.go", "nil"));
        assert!(!is_keyword("a.md", "def"));
    }

    #[test]
    fn python_imports_modules() {
        let src = "from __future__ import annotations\nimport os\nimport a.b as c\nfrom a.b import c, d as e\nfrom . import x\nfrom .m import y\nfrom pkg import *\nimport os\ndef f():\n    import json\n";
        assert_eq!(python_imports(src), ["os", "a.b", "a.b.c", "a.b.d", "pkg", "json"]);
    }
}
