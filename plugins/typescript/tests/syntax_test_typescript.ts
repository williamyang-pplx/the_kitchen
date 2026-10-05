// SYNTAX TEST "Packages/TypeScript/TypeScript.sublime-syntax"

import type { Foo, Bar } from "./types";
// <- keyword.control.import
//     ^^^^ keyword.control.type
//                            ^^^^^^^^^ string.quoted.double
import { readFile } from "node:fs/promises";
//       ^^^^^^^^ variable.other.readwrite.alias
//                  ^^^^ keyword.control.from
import * as path from "path";
//     ^ constant.language.import-export-all
//       ^^ keyword.control.as
export type { Foo };
// <- keyword.control.export
//     ^^^^ keyword.control.type
export default class {}
//     ^^^^^^^ keyword.control.default

interface Point<T extends number = number> {
// <- storage.type.interface
//        ^^^^^ entity.name.type.interface
//              ^ entity.name.type
//                ^^^^^^^ storage.modifier
  readonly x: T;
//^^^^^^^^ storage.modifier
//         ^ variable.object.property
//            ^ entity.name.type
  y?: number;
// ^ keyword.operator.optional
//    ^^^^^^ support.type.primitive
}

type Keys = keyof typeof config;
// <- storage.type.type
//   ^^^^ entity.name.type.alias
//          ^^^^^ keyword.operator.expression.keyof
//                ^^^^^^ keyword.operator.expression.typeof
type Getter<T> = { [K in keyof T as `get${Capitalize<string & K>}`]: () => T[K] };
//                    ^^ keyword.operator.expression.in
//                               ^^ keyword.control.as
//                                  ^^^^ string.template
//                                      ^^ punctuation.definition.template-expression.begin
type Unwrap<T> = T extends Promise<infer U> ? U : never;
//                 ^^^^^^^ storage.modifier
//                                 ^^^^^ keyword.operator.expression.infer
//                                                ^^^^^ support.type.primitive
type Fn = (a: string, b?: number) => void;
//                                ^^ storage.type.function.arrow
type U = "a" | "b" | null;
//           ^ keyword.operator.type
//                   ^^^^ support.type.builtin
type Tpl = `prefix-${string}`;
//         ^^^^^^^^^^^^^^^^^^ string.template
type Ro = readonly string[];
//        ^^^^^^^^ storage.modifier

enum Color { Red = 1, Green }
// <- storage.type.enum
//   ^^^^^ entity.name.type.enum
//           ^^^ variable.other.enummember
const enum Dir { Up, Down }
// <- storage.modifier
//    ^^^^ storage.type.enum
declare module "foo" {}
// <- storage.modifier
//      ^^^^^^ storage.type.namespace
namespace NS {}
// <- storage.type.namespace
//        ^^ entity.name.type.module

@Component({ selector: "app" })
// <- meta.decorator punctuation.decorator
//^^^^^^^^ meta.decorator entity.name.function
abstract class Animal<T> extends Base implements Walker, Runner {
// <- storage.modifier
//       ^^^^^ storage.type.class
//             ^^^^^^ entity.name.type.class
//                       ^^^^^^^ storage.modifier
//                               ^^^^ entity.other.inherited-class
//                                    ^^^^^^^^^^ storage.modifier
//                                               ^^^^^^ entity.other.inherited-class
  private readonly name: string;
//^^^^^^^ storage.modifier
//        ^^^^^^^^ storage.modifier
//                 ^^^^ meta.definition.property variable.object.property
  protected static count = 0;
//^^^^^^^^^ storage.modifier
//          ^^^^^^ storage.modifier
  public value!: number;
//^^^^^^ storage.modifier
//            ^ keyword.operator.definiteassignment
  #secret = 1;
//^^^^^^^ variable.object.property
  declare foo: string;
//^^^^^^^ storage.modifier
  accessor bar = 1;
//^^^^^^^^ storage.modifier
  @Input() label = "";
//^ punctuation.decorator
// ^^^^^ entity.name.function
  constructor(public readonly id: number, private svc?: Service) {
//^^^^^^^^^^^ storage.type
//            ^^^^^^ storage.modifier
//                   ^^^^^^^^ storage.modifier
//                            ^^ variable.parameter
    super();
//  ^^^^^ variable.language.super
  }
  override async method<const T>(x: T): Promise<T> {
//^^^^^^^^ storage.modifier
//         ^^^^^ storage.modifier.async
//               ^^^^^^ meta.definition.method entity.name.function
//                      ^^^^^ storage.modifier
    return await this.svc.run(x);
//  ^^^^^^ keyword.control.flow
//         ^^^^^ keyword.control.flow
//               ^^^^ variable.language.this
  }
  abstract speak(): void;
//^^^^^^^^ storage.modifier
  get size(): number { return 1; }
//^^^ storage.type.property
  static { init(); }
//^^^^^^ storage.modifier
}

interface Producer<out T> { get(): T }
//                 ^^^ storage.modifier
interface Consumer<in T> { set(v: T): void }
//                 ^^ storage.modifier
interface Both<in out T> {}
//             ^^ storage.modifier
//                ^^^ storage.modifier

function assertIsString(val: any): asserts val is string {}
// <- storage.type.function
//       ^^^^^^^^^^^^^^ entity.name.function
//                           ^^^ support.type.primitive
//                                 ^^^^^^^ keyword.operator.type.asserts
//                                             ^^ keyword.operator.expression.is
function isFoo(x: unknown): x is Foo {}
//                ^^^^^^^ support.type.primitive
//                            ^^ keyword.operator.expression.is
function ident<const T extends readonly unknown[]>(x: T): T { return x; }
//             ^^^^^ storage.modifier
//                     ^^^^^^^ storage.modifier

const conf = { port: 80 } satisfies Config;
// <- storage.type
//    ^^^^ variable.other.constant
//                        ^^^^^^^^^ keyword.control.satisfies
//                                  ^^^^^^ entity.name.type
const n = value as unknown as number;
//              ^^ keyword.control.as
//                 ^^^^^^^ support.type.primitive
//                         ^^ keyword.control.as
const tuple = [1, 2] as const;
//                   ^^ keyword.control.as
//                      ^^^^^ storage.modifier
const legacy = <string>someValue;
//              ^^^^^^ support.type.primitive
let maybe = obj?.prop?.[0]?.(arg);
// <- storage.type
//             ^^ punctuation.accessor.optional
//                   ^^ punctuation.accessor.optional
//                        ^^ punctuation.accessor.optional
let forced = obj!.prop;
//              ^ keyword.operator.definiteassignment
x ??= y ?? z;
//^^^ keyword.operator.assignment.compound
//      ^^ keyword.operator.logical
a ||= b;
//^^^ keyword.operator.assignment.compound
a &&= b;
//^^^ keyword.operator.assignment.compound
const big = 1_000_000n + 0xffn;
//          ^^^^^^^^^^ constant.numeric
//                       ^^^^^ constant.numeric

using res = getResource();
// <- storage.type
//    ^^^ variable.other
await using conn = open();
// <- keyword.control.flow
//    ^^^^^ storage.type

const add = async (a: number, b: number): Promise<number> => a + b;
//          ^^^^^ storage.modifier.async
//                 ^ variable.parameter
//                    ^^^^^^ support.type.primitive
//                                                        ^^ storage.type.function.arrow
const id = <T,>(x: T) => x;
//                    ^^ storage.type.function.arrow
async function* gen(): AsyncGenerator<number> {
// <- storage.modifier.async
//    ^^^^^^^^ storage.type.function
//            ^ keyword.generator.asterisk
  yield 1;
//^^^^^ keyword.control.flow
}

const msg = `hello ${user.name}!`;
//          ^^^^^^^ string.template
//                 ^^ punctuation.definition.template-expression.begin
//                   ^^^^ variable.other.object
//                            ^ punctuation.definition.template-expression.end
const tagged = sql`SELECT ${id}`;
//             ^^^ entity.name.function.tagged-template
const re = /ab+c/gi;
//         ^^^^^^^^ string.regexp

for (const [k, v] of Object.entries(map)) {}
// <- keyword.control.loop
//                ^^ keyword.operator.expression.of
try { throw new Error("x"); } catch (e: unknown) {} finally {}
// <- keyword.control.trycatch
//    ^^^^^ keyword.control.trycatch
//          ^^^ keyword.operator.new
//                            ^^^^^ keyword.control.trycatch
if (x instanceof Y && !z) {} else {}
// <- keyword.control.conditional
//    ^^^^^^^^^^ keyword.operator.expression.instanceof
//                 ^^ keyword.operator.logical
//                           ^^^^ keyword.control.conditional

/** JSDoc @param x the value */
// <- comment.block.documentation
//        ^^^^^^ storage.type.class.jsdoc
/* block */
// <- comment.block
// line comment
// <- comment.line.double-slash

export const handler = (req: Request): Response => new Response(null);
// <- keyword.control.export - invalid
//     ^^^^^ storage.type - invalid
//                                                     ^^^^^^^^ entity.name.function - invalid
export { a as b, c } from "./mod";
//       ^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid
export * from "./all";
//     ^ constant.language.import-export-all
declare global { interface Window { app: App } }
// <- storage.modifier
//      ^^^^^^ - invalid
function overload(a: string): void;
//                                ^ - invalid
const sym: unique symbol = Symbol();
//         ^^^^^^ keyword.operator.expression.unique
//                ^^^^^^ support.type.primitive
label: for (;;) { break label; }
//                ^^^^^ keyword.control.loop
const ok = !flag && a!==b && fn()!.x;
//         ^ keyword.operator.logical
//                   ^^^ keyword.operator.comparison
//                               ^ keyword.operator.definiteassignment
const accessor = 1;
//    ^^^^^^^^ - storage.modifier
const using = 2;
//    ^^^^^ - storage.type
