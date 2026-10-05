// SYNTAX TEST "Packages/Go/Go.sublime-syntax"
// Kitchen-specific tests: generics, builtins and the syntect-only patches.

package main
// <- keyword.declaration.namespace.go
//      ^^^^ entity.name.namespace.go

import (
    "fmt"
//  ^^^^^ string.quoted.double.go
    str "strings"
//  ^^^ variable.other.go
//      ^^^^^^^^^ string.quoted.double.go
)

func Map[K comparable, V any](m map[K]V, f func(V) V) []V { return nil }
// <- keyword.declaration.function.go
//   ^^^ entity.name.function.go
//      ^ punctuation.section.brackets.begin.go
//       ^ variable.parameter.type.go
//         ^^^^^^^^^^ storage.type.go support.type.builtin.go
//                     ^ variable.parameter.type.go
//                       ^^^ storage.type.go support.type.builtin.go
//                          ^ punctuation.section.brackets.end.go
//                            ^ variable.parameter.go
//                              ^^^ keyword.declaration.map.go
//                                  ^ storage.type.go
//                                    ^ storage.type.go
//                                                      ^ storage.type.go
// ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid

type Set[T any] struct {
// <- keyword.declaration.type.go
//   ^^^ entity.name.type.go
//       ^ variable.parameter.type.go
//         ^^^ storage.type.go support.type.builtin.go
//              ^^^^^^ keyword.declaration.struct.go
    Name  string `json:"name,omitempty" yaml:"-"`
//  ^^^^ variable.other.member.declaration.go
//        ^^^^^^ storage.type.go support.type.builtin.go
//               ^ punctuation.definition.annotation.begin.go
//                ^^^^ variable.annotation.go
//               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ meta.annotation
    items map[T]struct{}
//  ^^^^^ variable.other.member.declaration.go
//        ^^^ keyword.declaration.map.go
//            ^ storage.type.go
}
// <- punctuation.section.braces.end.go - invalid

type Number interface {
    ~int | ~float64
//  ^ keyword.operator
//   ^^^ storage.type.go support.type.builtin.go
}

func (s *Set[T]) Add(v T) bool {
//    ^ variable.parameter.go
//      ^ keyword.operator
//       ^^^ storage.type.go
//           ^ variable.parameter.type.go
//               ^^^ entity.name.function.go
//                   ^ variable.parameter.go
//                     ^ storage.type.go
//                        ^^^^ storage.type.go support.type.builtin.go
    s.items[v] = struct{}{}
//  ^ variable.other.go
//   ^ punctuation.accessor.dot.go
//    ^^^^^ variable.other.member.go
//         ^ - variable.parameter
    return true
//  ^^^^^^ keyword.control
//         ^^^^ constant.language.boolean.true.go
}

var m = Map[int, string]{}
//      ^^^ variable.other.go
//         ^ punctuation.section.brackets.begin.go
//          ^^^ variable.other.type.go - variable.parameter
//               ^^^^^^ variable.other.type.go - variable.parameter
//                     ^ punctuation.section.brackets.end.go
// ^^^^^^^^^^^^^^^^^^^^^^^^ - invalid

var r = Map[int, string](nil, nil)
//      ^^^ variable.function.go
//          ^^^ - variable.parameter
// ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid

var xs = make([]int, 0, 10)
//       ^^^^ variable.function.go support.function.builtin.go
//            ^ punctuation.section.brackets.begin.go
//             ^ punctuation.section.brackets.end.go
//              ^^^ storage.type.go support.type.builtin.go
//                   ^ constant.numeric
// ^^^^^^^^^^^^^^^^^^^^^^^^ - invalid

var ok = xs[0] == 1
//       ^^ variable.other.go
//          ^ constant.numeric
// ^^^^^^^^^^^^^^^^ - invalid

func builtins(c chan int) {
    n := len(xs) + cap(xs) + copy(xs, xs) + min(1, 2) + max(1, 2)
//  ^ variable.other.readwrite.declaration.go
//       ^^^ variable.function.go support.function.builtin.go
//                 ^^^ variable.function.go support.function.builtin.go
//                           ^^^^ variable.function.go support.function.builtin.go
//                                          ^^^ variable.function.go support.function.builtin.go
//                                                      ^^^ variable.function.go support.function.builtin.go
    xs = append(xs, n)
//       ^^^^^^ variable.function.go support.function.builtin.go
    p := new(int)
//       ^^^ variable.function.go support.function.builtin.go
//           ^^^ storage.type.go support.type.builtin.go
    delete(m, 1)
//  ^^^^^^ variable.function.go support.function.builtin.go
    clear(m)
//  ^^^^^ variable.function.go support.function.builtin.go
    close(c)
//  ^^^^^ variable.function.go support.function.builtin.go
    z := complex(1, 2)
//       ^^^^^^^ variable.function.go support.function.builtin.go
    _, _ = real(z), imag(z)
//  ^ variable.language.anonymous.go
//         ^^^^ variable.function.go support.function.builtin.go
//                  ^^^^ variable.function.go support.function.builtin.go
    print(p, nil, iota)
//  ^^^^^ variable.function.go support.function.builtin.go
//           ^^^ constant.language.null.go
    println(false)
//  ^^^^^^^ variable.function.go support.function.builtin.go
//          ^^^^^ constant.language.boolean.false.go
    defer func() { _ = recover() }()
//  ^^^^^ keyword.control
//        ^^^^ keyword.declaration.function.go
//                     ^^^^^^^ variable.function.go support.function.builtin.go
    panic("boom")
//  ^^^^^ variable.function.go support.function.builtin.go
    var e error = nil
//      ^ variable.other.readwrite.declaration.go
//        ^^^^^ storage.type.go support.type.builtin.go
    b := byte(rune(1))
//       ^^^^ variable.function.go support.type.builtin.go
//            ^^^^ variable.function.go support.type.builtin.go
    _, _ = e, b
}

func concurrency(ch chan string, done <-chan struct{}) {
//                  ^^^^ keyword.declaration.chan.go
//                                    ^^ keyword.operator
    go func(s string) {
//  ^^ keyword.control
//     ^^^^ keyword.declaration.function.go
//          ^ variable.parameter.go
        ch <- s
//         ^^ keyword.operator
    }("hi")
    select {
//  ^^^^^^ keyword.control
    case v := <-ch:
//  ^^^^ keyword.control.conditional.case.go
//       ^ variable.other.readwrite.declaration.go
//            ^^ keyword.operator
        fmt.Println(v)
//      ^^^ variable.other.go
//          ^^^^^^^ variable.function.go
    case <-done:
    default:
//  ^^^^^^^ keyword.control.conditional.default
    }
    for i := range 10 {
//  ^^^ keyword.control
//      ^ variable.other.readwrite.declaration.go
//           ^^^^^ keyword.other
        if i%2 == 0 {
            continue
//          ^^^^^^^^ keyword.control
        }
    }
}

func strings() {
    fmt.Printf("%d %-5s %v %%\n", 1, "a", 2)
//             ^ punctuation.definition.string.begin.go
//              ^^ constant.other.placeholder.go
//                 ^^^^ constant.other.placeholder.go
//                      ^^ constant.other.placeholder.go
//                         ^^ constant.character.escape.go
//                           ^^ constant.character.escape.go
    raw := `raw\n %d
//         ^ punctuation.definition.string.begin.go
//         ^^^^^^^^^ string.quoted.backtick.go
//             ^^ - constant.character.escape
multi-line`
// <- string.quoted.backtick.go
//        ^ punctuation.definition.string.end.go
    r1, r2 := 'a', '\n'
//            ^^^ string.quoted.single.go
//                  ^^ constant.character.escape.go
    _, _, _ = raw, r1, r2
    n := 0x1F + 1_000 + 3.14 + 2i
//       ^^^^ constant.numeric
//              ^^^^^ constant.numeric
//                      ^^^^ constant.numeric
//                             ^^ constant.numeric
    _ = n
}

type Pair[K comparable, V any] struct{ Key K; Val V }

func closures() {
    p := Pair[string, int]{Key: "a", Val: 1}
//       ^^^^ variable.other.go
//            ^^^^^^ variable.other.type.go
//                    ^^^ variable.other.type.go
// ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid
    flags := []bool{true}
    if flags[0] {
//     ^^^^^ variable.other.go
//           ^ constant.numeric
    }
    add := func(a, b int) int { return a + b }
//         ^^^^ keyword.declaration.function.go
//              ^ variable.parameter.go
//                 ^ variable.parameter.go
//                   ^^^ storage.type.go support.type.builtin.go
    _, _ = p.Key, add(1, 2)
//           ^^^ variable.other.member.go
//                ^^^ variable.function.go
    const C = 1
//  ^^^^^ keyword.declaration.const.go
//        ^ variable.other.constant.declaration.go
outer:
// <- entity.name.label.go
    for {
        break outer
//      ^^^^^ keyword.control
//            ^^^^^ variable.label.go
    }
}
// <- punctuation.section.braces.end.go - invalid
