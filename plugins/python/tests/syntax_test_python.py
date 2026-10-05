# SYNTAX TEST "Packages/Python/Python.sublime-syntax"

match command:
# <- keyword.control.conditional
#^^^^^^^^^^^^^ - invalid
#            ^ punctuation.section.block
    case [x, *rest]:
#   ^^^^ keyword.control.conditional
#        ^^^^^^^^^^^ - invalid
    case Point(x=0, y=0):
#   ^^^^ keyword.control.conditional
#        ^^^^^ meta.function-call support.class
#              ^ variable.parameter
    case {"key": value} | None:
#   ^^^^ keyword.control.conditional
#        ^^^^^^^^^^^^^^^^^^^^^^ - invalid
    case str() as s if s:
#   ^^^^ keyword.control.conditional
#        ^^^ support.type
#              ^^ keyword
#                   ^^ keyword
#        ^^^^^^^^^^^^^^^^ - invalid
    case _:
#   ^^^^ keyword.control.conditional
#        ^ variable.language

match = 3
# <- - keyword
#     ^ keyword.operator.assignment
m = re.match(pattern, text)
#      ^^^^^ variable.function - keyword
case = match.group(1)
# <- - keyword
#      ^^^^^ - keyword
match.foo(x)
# <- - keyword
case: int = 3
# <- - keyword

type Alias = list[int]
# <- storage.type
#    ^^^^^ entity.name.type
#          ^ keyword.operator.assignment
#            ^^^^ support.type
type Pair[T] = tuple[T, T]
# <- storage.type
#    ^^^^ entity.name.type
#        ^ punctuation.definition.generic.begin
#         ^ variable.parameter.type
#          ^ punctuation.definition.generic.end
type(x)
# <- support.function.builtin - storage.type

if (n := len(a)) > 10:
#     ^^ keyword.operator.assignment
#  ^^^^^^^^^^^^^^^^^^^ - invalid
#                    ^ punctuation.section.block
while chunk := f.read(10):
#           ^^ keyword.operator.assignment
#                        ^ punctuation.section.block
print([y := 1, y ** 2])
#        ^^ keyword.operator.assignment

class Foo:
    def method(self, x):
#              ^^^^ variable.parameter
        self.value = x
#       ^^^^ variable.language
#           ^ punctuation.accessor
#            ^^^^^ variable.other.member
        cls.count += 1
#       ^^^ variable.language
#           ^^^^^ variable.other.member
        return obj.attr.other
#              ^^^ - variable.other.member
#                  ^^^^ variable.other.member
#                       ^^^^^ variable.other.member
        obj.method()
#           ^^^^^^ variable.function - variable.other.member
        a.b.c()
#         ^ variable.other.member
#           ^ variable.function

x = Foo(1)
#   ^^^ meta.function-call support.class
y = mod.Bar()
#   ^^^ - support.class
#       ^^^ support.class
z = foo(1)
#   ^^^ variable.function - support.class

def first[T](xs: list[T]) -> T:
# <- storage.type.function
#   ^^^^^ entity.name.function
#        ^ punctuation.definition.generic.begin
#         ^ variable.parameter.type
#          ^ punctuation.definition.generic.end
#            ^^ variable.parameter
#                ^^^^ support.type
#                             ^ punctuation.section.function.begin
    return xs[0]

def many[T: int, *Ts, **P](a): pass
#        ^ variable.parameter.type
#           ^^^ support.type
#                 ^^ variable.parameter.type
#                ^ keyword.operator
#                       ^ variable.parameter.type
#                         ^ - invalid

class Box[T](Generic):
# <- storage.type.class
#     ^^^ entity.name.class
#        ^ punctuation.definition.generic.begin
#         ^ variable.parameter.type
#            ^^^^^^^ entity.other.inherited-class
    pass

n: int = 0
#  ^^^ support.type
s: str
#  ^^^ support.type
d: dict[str, list[float]] = {}
#  ^^^^ support.type
#       ^^^ support.type
#            ^^^^ support.type
#                 ^^^^^ support.type
b = bool(x) or bytes(y)
#   ^^^^ support.type
#              ^^^^^ support.type

def greet(name: str, *args: int, **kw: dict) -> str | None:
#               ^^^ support.type
#                    ^ keyword.operator.unpacking
#                                ^^ keyword.operator.unpacking
#                                               ^^^ support.type
#                                                     ^^^^ constant.language
    """Docstring here."""
#   ^^^^^^^^^^^^^^^^^^^^^ comment.block.documentation
    msg = f"hello {name!r:>10} {x + 1}"
#         ^ storage.type.string
#          ^^^^^^^ string.quoted.double
#                 ^ punctuation.section.interpolation.begin
#                  ^^^^ source.python.embedded - string
#                      ^^ storage.modifier.conversion
#                                 ^ keyword.operator.arithmetic
    return msg

@decorator
# <- punctuation.definition.annotation
#^^^^^^^^^ variable.annotation
@functools.lru_cache(maxsize=None)
#          ^^^^^^^^^ variable.annotation.function
async def fetch(url):
# <- storage.modifier.async
#     ^^^ storage.type.function
    async with session.get(url) as resp:
#   ^^^^^ storage.modifier.async
#         ^^^^ keyword.control.flow.with
        data = await resp.json()
#              ^^^^^ keyword.other.await
#                         ^^^^ variable.function
    async for item in aiter(data):
#   ^^^^^ storage.modifier.async
#         ^^^ keyword.control.flow.for
        yield item

import os.path
#      ^^ - variable.other.member
#         ^^^^ - variable.other.member
from a.b import c
#    ^^^^^^^^^^^^ - invalid
try:
    raise ValueError("bad")
#         ^^^^^^^^^^ support.type.exception
except (KeyError, TypeError) as e:
#                            ^^ keyword
    pass
lambda x: x + 1
# <- storage.type.function.inline
items = {k: v for k, v in pairs if v}
#        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid

def handle(event):
    match event.kind:  # dispatch
#   ^^^^^ keyword.control.conditional
#               ^^^^ variable.other.member
#                   ^ punctuation.section.block
#                      ^^^^^^^^^^ comment.line
        case "click" | "tap":
#       ^^^^ keyword.control.conditional
#            ^^^^^^^ string.quoted.double
            pass
