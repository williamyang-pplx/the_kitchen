//! The changed files as a tree, GitHub style: folders first, then files,
//! both sorted, and folder chains with a single subfolder merged into one
//! row (`crates/app/src/`). The viewer keeps its changed files in this
//! order, so the list, `⌘J`/`⌘K` and `]f`/`[f` all step the same way.

use std::collections::BTreeMap;

use kitchen_core::git::ChangedFile;

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub depth: usize,
    /// `dir/` (possibly `a/b/`) for folders, the file name for files.
    pub label: String,
    /// Index into the changed files for file rows.
    pub file: Option<usize>,
}

#[derive(Default)]
struct Node<'a> {
    dirs: BTreeMap<&'a str, Node<'a>>,
    files: Vec<(&'a str, usize)>,
}

fn build<'a>(paths: &[&'a str]) -> Node<'a> {
    let mut root = Node::default();
    for (i, p) in paths.iter().enumerate() {
        let mut node = &mut root;
        let mut segs: Vec<&str> = p.split('/').collect();
        let name = segs.pop().unwrap_or(p);
        for s in segs {
            node = node.dirs.entry(s).or_default();
        }
        node.files.push((name, i));
    }
    root
}

fn emit(node: &Node, depth: usize, out: &mut Vec<Row>) {
    for (name, mut child) in &node.dirs {
        let mut label = format!("{name}/");
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (n, c) = child.dirs.iter().next().expect("one subfolder");
            label.push_str(&format!("{n}/"));
            child = c;
        }
        out.push(Row { depth, label, file: None });
        emit(child, depth + 1, out);
    }
    let mut files = node.files.clone();
    files.sort();
    out.extend(files.into_iter().map(|(name, i)| Row { depth, label: name.to_string(), file: Some(i) }));
}

/// Tree rows for `paths`; file rows point back into `paths`.
pub fn rows(paths: &[&str]) -> Vec<Row> {
    let mut out = Vec::new();
    emit(&build(paths), 0, &mut out);
    out
}

/// Put `changed` in tree order.
pub fn sort(changed: &mut Vec<ChangedFile>) {
    let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let order: Vec<usize> = rows(&paths).into_iter().filter_map(|r| r.file).collect();
    let mut slots: Vec<Option<ChangedFile>> = std::mem::take(changed).into_iter().map(Some).collect();
    *changed = order.into_iter().filter_map(|i| slots[i].take()).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first_and_chains_merged() {
        let paths = ["README.md", "crates/app/src/viewer/mod.rs", "crates/app/src/main.rs", "crates/core/git.rs", "a/x.rs", "a/y/z.rs"];
        let shown: Vec<String> = rows(&paths).iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.label)).collect();
        assert_eq!(
            shown,
            vec!["a/", "  y/", "    z.rs", "  x.rs", "crates/", "  app/src/", "    viewer/", "      mod.rs", "    main.rs", "  core/", "    git.rs", "README.md"]
        );
        let mut changed: Vec<ChangedFile> = paths.iter().map(|p| ChangedFile { path: p.to_string(), old_path: None, status: 'M', added: 1, removed: 0 }).collect();
        sort(&mut changed);
        let order: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(order, vec!["a/y/z.rs", "a/x.rs", "crates/app/src/viewer/mod.rs", "crates/app/src/main.rs", "crates/core/git.rs", "README.md"]);
        let file_rows: Vec<usize> = rows(&order).into_iter().filter_map(|r| r.file).collect();
        assert_eq!(file_rows, (0..6).collect::<Vec<_>>(), "sorted: file rows count up");
    }
}
