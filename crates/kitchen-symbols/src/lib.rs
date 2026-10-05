//! Definitions in source files: classes, functions, methods, types.
//!
//! Tree-sitter parses Python, Rust, TypeScript/TSX, JavaScript and Go; other
//! files get a line-based regex fallback. Used for the viewer's outline mode
//! and jump-to-definition. Compiled into `kitchenctl` too, so a devbox can
//! index its own checkout (`kitchenctl __index`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// Whether `path` is a language the parser (not just the fallback) handles.
pub fn is_supported(path: &str) -> bool {
    lang_for(path).is_some()
}

/// Every definition in `source`, in file order.
pub fn extract(path: &str, source: &str) -> Vec<Symbol> {
    let Some(lang) = lang_for(path) else {
        return fallback(source);
    };
    let mut parser = Parser::new();
    if parser.set_language(&language(lang)).is_err() {
        return fallback(source);
    }
    let Some(tree) = parser.parse(source, None) else {
        return fallback(source);
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    walk(tree.root_node(), lang, source.as_bytes(), &lines, &mut Vec::new(), &mut out);
    out
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

/// Symbols of many files, searchable by name.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Index {
    /// path -> symbols in that file.
    pub files: HashMap<String, Vec<Symbol>>,
}

impl Index {
    pub fn add(&mut self, path: &str, source: &str) {
        self.files.insert(path.to_string(), extract(path, source));
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where `name` is defined, as (path, symbol): `from_path` first, then files
/// in the same directory, then the rest of the repo (alphabetical).
pub fn definitions<'a>(index: &'a Index, name: &str, from_path: &str) -> Vec<(&'a str, &'a Symbol)> {
    let dir = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    let from_dir = dir(from_path);
    let mut hits: Vec<(&str, &Symbol)> = index
        .files
        .iter()
        .flat_map(|(p, syms)| syms.iter().filter(|s| s.name == name).map(move |s| (p.as_str(), s)))
        .collect();
    hits.sort_by_key(|(p, s)| (*p != from_path, dir(p) != from_dir, p.to_string(), s.line));
    hits
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
    fn definition_ranking() {
        let mut idx = Index::default();
        idx.add("src/a.py", "def f():\n    pass\n");
        idx.add("src/b.py", "def f():\n    pass\n");
        idx.add("lib/c.py", "def f():\n    pass\n");
        let d = definitions(&idx, "f", "src/b.py");
        assert_eq!(d.iter().map(|(p, _)| *p).collect::<Vec<_>>(), ["src/b.py", "src/a.py", "lib/c.py"]);
        assert!(definitions(&idx, "nope", "x").is_empty());
    }
}
