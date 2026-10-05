// SYNTAX TEST "Packages/TypeScript/TypeScriptReact.sublime-syntax"

import type { FC, ReactNode } from "react";
// <- keyword.control.import
//     ^^^^ keyword.control.type
import { useState } from "react";
//                  ^^^^ keyword.control.from

interface Props {
// <- storage.type.interface
//        ^^^^^ entity.name.type.interface
  title: string;
//       ^^^^^^ support.type.primitive
  children?: ReactNode;
//        ^ keyword.operator.optional
}

type Variant = "primary" | "secondary";
// <- storage.type.type
//   ^^^^^^^ entity.name.type.alias

enum Size { Small, Large }
// <- storage.type.enum

const theme = { dark: true } satisfies Theme;
//                           ^^^^^^^^^ keyword.control.satisfies
const el = document.getElementById("root") as HTMLElement;
//                                         ^^ keyword.control.as
const name = user?.profile?.name ?? "anon";
//               ^^ punctuation.accessor.optional
//                               ^^ keyword.operator.logical
count ??= 0;
//    ^^^ keyword.operator.assignment.compound

export const Button: FC<Props> = ({ title, children }) => {
// <- keyword.control.export
//     ^^^^^ storage.type
//           ^^^^^^ variable.other.constant
//                                                     ^^ storage.type.function.arrow
  const [open, setOpen] = useState<boolean>(false);
//                        ^^^^^^^^ entity.name.function
//                                 ^^^^^^^ support.type.primitive
  const label = `${title}!`;
//               ^^ punctuation.definition.template-expression.begin
  return (
//^^^^^^ keyword.control.flow
    <div className="btn" onClick={() => setOpen(!open)}>
//  ^ punctuation.definition.tag.begin
//   ^^^ entity.name.tag
//       ^^^^^^^^^ entity.other.attribute-name
//                 ^^^^^ string.quoted.double
//                               ^ punctuation.section.embedded.begin
//                                   ^^ storage.type.function.arrow
//                                                     ^ punctuation.definition.tag.end
      <Icon size={Size.Small} />
//     ^^^^ entity.name.tag support.class.component
//          ^^^^ entity.other.attribute-name
//                            ^^ punctuation.definition.tag
      {children}
      <span>{label}</span>
//     ^^^^ entity.name.tag
//           ^^^^^ meta.embedded.expression
//                   ^^^^ entity.name.tag
    </div>
  );
};

@observer
// <- meta.decorator punctuation.decorator
class Store {
// <- storage.type.class
  private items: string[] = [];
//^^^^^^^ storage.modifier
  accessor ready = false;
//^^^^^^^^ storage.modifier
  async load<const T>(x: T): Promise<void> {
//^^^^^ storage.modifier.async
//           ^^^^^ storage.modifier
    await fetch(`/api/${x}`);
//  ^^^^^ keyword.control.flow
  }
}

function generic<T>(x: T): T { return x; }
// <- storage.type.function
//       ^^^^^^^ entity.name.function
type K = keyof typeof theme;
//       ^^^^^ keyword.operator.expression.keyof
//             ^^^^^^ keyword.operator.expression.typeof
let v = obj!.field;
//         ^ keyword.operator.definiteassignment

export default function App(): JSX.Element {
// <- keyword.control.export - invalid
//     ^^^^^^^ keyword.control.default
  return <Button title="hi"><b>bold</b></Button>;
//       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ - invalid
//        ^^^^^^ entity.name.tag
}
