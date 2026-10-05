// SYNTAX TEST "Packages/Rust/Rust.sublime-syntax"

//! Crate-level docs.
// <- comment.line.documentation punctuation.definition.comment
//^^^^^^^^^^^^^^^^^^^ comment.line.documentation

/// Item docs with `code`.
// <- comment.line.documentation punctuation.definition.comment
//  ^^^^^^^^^ comment.line.documentation

#![allow(dead_code)]
// <- meta.annotation punctuation.definition.annotation
// ^^^^^ meta.annotation variable.annotation

#[derive(Debug, Clone, PartialEq)]
// <- meta.annotation punctuation.definition.annotation
//^^^^^^ meta.annotation variable.annotation
//      ^ punctuation.section.group.begin
//                               ^ punctuation.section.group.end

use std::io::{self, Read};
// <- keyword.declaration.import
//  ^^^ meta.path variable.namespace
//     ^^ punctuation.accessor
//       ^^ variable.namespace
//            ^^^^ variable.language
//                  ^^^^ storage.type
//                       ^ punctuation.terminator - invalid

use crate::config::Config;
//  ^^^^^ keyword.other
//         ^^^^^^ variable.namespace
//                 ^^^^^^ storage.type - variable.namespace

const MAX_LEN: usize = 1_024;
// <- keyword.declaration.variable.constant
//    ^^^^^^^ entity.name.constant
//             ^^^^^ storage.type
//                     ^^^^^ constant.numeric.integer.decimal

static GREETING: &'static str = "hello";
//               ^ keyword.operator
//                ^^^^^^^ storage.modifier.lifetime
//                        ^^^ storage.type
//                              ^^^^^^^ string.quoted.double

pub(crate) struct Parser<'src, T: AsRef<str>> {
// <- storage.modifier
//         ^^^^^^ keyword.declaration.struct
//                ^^^^^^ entity.name.struct
//                       ^^^^ storage.modifier.lifetime
//                                ^^^^^ support.type
    input: &'src T,
//  ^^^^^ variable.other.member
//          ^^^^ storage.modifier.lifetime
    pos: usize,
//       ^^^^^ storage.type
}

impl<'src, T> Iterator for Parser<'src, T>
// <- meta.impl keyword.declaration.impl
//            ^^^^^^^^ support.type
//                     ^^^ keyword.other
//                         ^^^^^^ entity.name.impl
where
// <- keyword.other
    T: AsRef<str> + Send,
//     ^^^^^ support.type
//                  ^^^^ support.type
{
    type Item = Result<u8, Error>;
//  ^^^^ keyword.declaration.type
//              ^^^^^^ support.type

    fn next(&mut self) -> Option<Self::Item> {
//  ^^ keyword.declaration.function
//     ^^^^ entity.name.function
//           ^^^ storage.modifier
//               ^^^^ variable.parameter
//                     ^^ punctuation.separator
//                        ^^^^^^ support.type
//                               ^^^^ storage.type
        let bytes = self.input.as_ref().as_bytes();
//                  ^^^^ variable.language
//                             ^^^^^^ variable.function
//                                      ^^^^^^^^ variable.function
        let Some(&b) = bytes.get(self.pos) else {
//      ^^^ keyword.declaration.variable
//          ^^^^ support.constant
//                                         ^^^^ keyword.control
            return None;
//          ^^^^^^ keyword.control
//                 ^^^^ support.constant
        };
        self.pos += 1;
        Some(Ok(b))
//      ^^^^ support.constant
//           ^^ support.constant
    }
}

async fn fetch(url: &str) -> anyhow::Result<String> {
// <- keyword
//    ^^ keyword.declaration.function
//       ^^^^^ entity.name.function
//                           ^^^^^^ variable.namespace
//                                   ^^^^^^ support.type
    let body = client.get(url).send().await?.text().await?;
//                    ^^^ variable.function
//                                    ^^^^^ keyword.control
//                                         ^ keyword.operator
    let n = body.trim().parse::<u32>()?;
//                      ^^^^^ variable.function
//                           ^^ punctuation.accessor
//                             ^ punctuation.definition.generic.begin
//                              ^^^ storage.type
//                                    ^ keyword.operator
    let task = tokio::spawn(async move { n * 2 });
//             ^^^^^ meta.path variable.namespace
//                    ^^^^^ variable.function
//                          ^^^^^ keyword
//                                ^^^^ storage.modifier
    Ok(body)
//  ^^ support.constant
}

fn collect_evens(v: &[i32]) -> Vec<i32> {
    v.iter().copied().filter(|x| x % 2 == 0).collect::<Vec<_>>()
//    ^^^^ variable.function
//                           ^ meta.function.closure punctuation.section.parameters.begin
//                            ^ variable.parameter
//                             ^ punctuation.section.parameters.end
//                                           ^^^^^^^ variable.function
//                                                  ^^ punctuation.accessor
//                                                    ^ punctuation.definition.generic.begin
//                                                     ^^^ support.type
//                                                         ^ storage.type.inference
}

fn boxed(f: Box<dyn Fn(u8) -> u8 + Send + 'static>) -> impl Iterator<Item = u8> {
//              ^^^ storage.type.trait
//                  ^^ support.type
//                                        ^^^^^^^ storage.modifier.lifetime
//                                                     ^^^^ storage.type.impl
//                                                          ^^^^^^^^ support.type
    std::iter::once(f(1))
//  ^^^ variable.namespace
//       ^^^^ variable.namespace
//             ^^^^ variable.function - variable.namespace
}

fn literals() {
    println!("{} {name:?} {0:>8.2} {{not}}", a, name = b);
//  ^^^^^^^^ support.macro
//            ^^ constant.other.placeholder
//               ^^^^^^^^ constant.other.placeholder
//                        ^^^^^^^^ constant.other.placeholder
//                                 ^^ constant.character.escape - constant.other.placeholder
    let s = format!("{x}");
//          ^^^^^^^ support.macro
//                   ^^^ string.quoted.double constant.other.placeholder
    let v = vec![1, 2, 3];
//          ^^^^ support.macro
    my_macro! { a => b };
//  ^^^^^^^^^ support.macro
    let raw = r#"has "quotes" and \n"#;
//            ^ storage.type.string
//             ^^ punctuation.definition.string.begin
//            ^^^^^^^^^^^^^^^^^^^^^^^^ string.quoted.double.raw
//                                ^^ - constant.character.escape
//                                  ^^ punctuation.definition.string.end
    let bytes = b"bytes\n";
//              ^ storage.type.string
//               ^^^^^^^^^ string.quoted.double
//                     ^^ constant.character.escape
    let c = 'c';
//          ^^^ string.quoted.single - storage.modifier.lifetime
    let nl = '\n';
//           ^^^^ string.quoted.single
//            ^^ constant.character.escape
    let byte = b'a';
//             ^ storage.type.string
//              ^^^ string.quoted.single
    let x = 0xFF_u8 + 1.5e3f64 + 1_000i64 + 0b1010;
//          ^^^^^ constant.numeric.integer.hexadecimal
//               ^^ storage.type.numeric
//                    ^^^^^ constant.numeric.float
//                         ^^^ storage.type.numeric
//                               ^^^^^ constant.numeric.integer.decimal
//                                    ^^^ storage.type.numeric
//                                          ^^^^^^ constant.numeric.integer.binary
    let t = true && !false;
//          ^^^^ constant.language
//                   ^^^^^ constant.language
    let y = x as u64;
//            ^^ keyword.operator
//               ^^^ storage.type
}

fn control<const N: usize>(grid: [[u8; N]; N]) -> usize {
//         ^^^^^ storage.modifier
//               ^ storage.type
    let mut count = 0;
//      ^^^ storage.modifier
    'outer: for row in grid.iter() {
//  ^^^^^^ entity.name.label
//        ^ punctuation.separator
//          ^^^ keyword.control
//                  ^^ keyword.operator
        for &cell in row {
            if cell == 0 { continue 'outer; }
//          ^^ keyword.control
//                         ^^^^^^^^ keyword.control
//                                  ^^^^^^ entity.name.label
            if cell > 9 { break 'outer; }
//                              ^^^^^^ entity.name.label - storage.modifier.lifetime
            count += 1;
        }
    }
    match count {
//  ^^^^^ keyword.control
        0 => MAX_LEN,
//        ^^ keyword.operator
//           ^^^^^^^ constant.other
        n if n < 10 => n,
        _ => unsafe { std::ptr::read(&count) },
//           ^^^^^^ storage.modifier
//                    ^^^ variable.namespace
//                         ^^^ variable.namespace
//                              ^^^^ variable.function
    }
}

pub mod tests {
//  ^^^ keyword.declaration.module
//      ^^^^^ entity.name.module
    use super::*;
//      ^^^^^ keyword.other - variable.namespace
    impl Default for Config {
        fn default() -> Self {
            Self { name: String::new(), ..Default::default() }
//          ^^^^ - variable.namespace
//                       ^^^^^^ support.type - variable.namespace
//                               ^^^ variable.function
//                                        ^^^^^^^ support.type
//                                                 ^^^^^^^ variable.function
        }
    }
}

trait Shape: Clone + Send {
// <- keyword.declaration.trait
//    ^^^^^ entity.name.trait
    fn area(&self) -> f64;
//                    ^^^ storage.type
}

enum Token<'a> { Ident(&'a str), Num(i64), Eof }
// <- keyword.declaration.enum
//   ^^^^^ entity.name.enum
//         ^^ storage.modifier.lifetime
//               ^^^^^ storage.type
//                                         ^^^ storage.type

fn types(m: std::collections::HashMap<String, crate::model::User>) {}
//          ^^^ variable.namespace
//               ^^^^^^^^^^^ variable.namespace
//                            ^^^^^^^ support.type - variable.namespace
//                                            ^^^^^ - variable.namespace
//                                                   ^^^^^ variable.namespace
//                                                          ^^^^ storage.type
