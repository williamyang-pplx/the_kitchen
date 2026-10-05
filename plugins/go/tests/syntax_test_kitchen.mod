// SYNTAX TEST "Packages/Gomod/Gomod.sublime-syntax"
module github.com/example/app
// <- keyword.gomod
//     ^^^^^^^^^^^^^^^^^^^^^^ variable.gomod

go 1.23
// <- keyword.gomod
// ^^^^ variable.gomod

toolchain go1.23.2
// <- keyword.gomod

require (
// <- keyword.gomod
//      ^ punctuation.section.parens.begin.gomod
    golang.org/x/sync v0.8.0 // indirect
//  ^^^^^^^^^^^^^^^^^ variable.gomod
//                    ^^^^^^ string.version.gomod
//                           ^^^^^^^^^^^ comment.line.gomod
)
// <- punctuation.section.parens.end.gomod

replace example.com/a v1.0.0 => ../a
// <- keyword.gomod
//      ^^^^^^^^^^^^^ variable.gomod
//                    ^^^^^^ string.version.gomod
//                           ^^ keyword.operator.gomod
//                              ^^^^ variable.gomod

use ./tools
// <- keyword.gomod
//  ^^^^^^^ variable.gomod
