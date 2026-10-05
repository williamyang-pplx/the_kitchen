#! SYNTAX TEST "Rust.sublime-syntax"
---
#! <- meta.frontmatter.rust punctuation.section.frontmatter.begin.rust
#!^ meta.frontmatter.rust punctuation.section.frontmatter.begin.rust
#! ^ meta.frontmatter.rust - punctuation
[section]
#! <- meta.frontmatter.rust source.toml.embedded.rust
key = "value"
#! <- meta.frontmatter.rust source.toml.embedded.rust
---
#! <- meta.frontmatter.rust punctuation.section.frontmatter.end.rust
#!^ meta.frontmatter.rust punctuation.section.frontmatter.end.rust
#! ^ meta.frontmatter.rust - punctuation

fn main() {}
#! <- meta.function.rust keyword.declaration.function.rust
#!^^^^^^^^^^ meta.function.rust

// kitchen: TOML-scope assertions on lines 7 and 9 trimmed to the embed scope
// (syntect has no TOML grammar).
