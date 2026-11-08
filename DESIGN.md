# Design

Status as of 2026-11-08. Nothing is implemented. Every decision below is
marked:

- **Decided**: agreed, change only with a reason.
- **Leaning**: the current choice, not yet confirmed.
- **Open**: needs a decision.

## Origin

The design came out of a conversation on 2026-11-08, while working on
[flux](../flux-lang), a compiler for a subset of Rust that lowers to the
keel backend (main at commit `cc2905ef`).

Building flux showed where Rust's compile time goes:

- Abstractions that cost nothing at runtime cost a lot at compile time.
  Iterator chains, `Deref`, `Into`, `Option::map` and drop glue
  monomorphize into many tiny functions and blocks, one set per type
  argument and per closure.
- The backend did most of the work. A flux stage build took about 16 GB,
  mostly in the backend, and per-crate builds spent most of their time
  re-emitting generic instances.
- The frontend has no cheap layer. Name resolution depends on macro
  expansion, inference depends on trait solving, trait solving depends on
  normalization, and borrow checking depends on all of it.

Two observations about AI-assisted programming motivated a new language:

- Prototyping with AI tools is fast, so compile time becomes the
  bottleneck.
- AI writes most of the code and people review it. Complex languages,
  and languages where a reader needs the compiler to know a variable's
  type, are hard to review.

## Goals

1. **Optimize for reading, not writing.** AI writes the code and people
   review it. Extra keystrokes cost the writer nothing. This rule decides
   most of the choices below.
2. **Fast compilation.** Every function body can be checked from
   signatures alone, in parallel. No type checks cross function
   boundaries; inferring which globals a function writes is a separate
   pass over the call graph (2026-11-09, see
   [Mutable globals](#mutable-globals)).
3. **Systems level.** No garbage collector and no required runtime, so
   the language works for embedded targets and OS development.
4. **Memory safety.** Indexing is bounds-checked, there is no null, and
   pointer arithmetic is a built-in. Since 2026-11-09 the design also
   makes safe code free of use after free and dangling views, without
   lifetimes, and the compilers are being changed to match (see
   [Temporal safety](#temporal-safety-without-lifetimes)). Hardware
   access and other unchecked operations go in `unsafe` blocks.
5. **One way to do each thing at the syntax level**, with no paradigm
   forced on program structure. The user's first framing was
   "unopinionated, but not many ways to do the same thing".
6. **Easy tooling.** A highlighter only needs the lexer. The parser is
   simple enough to write by hand.
7. **Verifiable** (added 2026-11-09). A safe program can be proved free
   of runtime errors automatically, and correct against written
   contracts. See [Verification](#verification).

## Not in the first version

OOP, traits, user-defined generics, iterator adapters, nested functions,
macros, and running user code at compile time. Nested functions have a
candidate design, recorded under [Deferred](#deferred).

## Syntax

### Grammar class: LL(1)

**Leaning.** The user first suggested LALR(1). Every LL(1) grammar is also
LR(1), though not always LALR(1). LL(1) allows a hand-written recursive
descent parser with good error messages and recovery. flux's parser is
hand-written the same way.

The tree-sitter grammar in `tree-sitter/` checks the syntax mechanically.
It uses no precedence or conflict declarations, so a clean generate means
no LR(1) conflicts. Tree-sitter does not check LL(1).

Found by the grammar on 2026-11-08: after `(`, an identifier followed by
`:` starts a tuple literal, and any other identifier starts a parenthesized
expression. A recursive descent parser needs two tokens of lookahead there,
or must parse the identifier before choosing. Both are cheap, but strictly
the expression grammar is not LL(1).

### Keywords

The running list. Each new keyword needs a reason.

| Keyword | Use |
| --- | --- |
| `fn` | function declaration |
| `type` | type declaration |
| `let` | immutable binding, and the default parameter mode |
| `var` | mutable binding |
| `return` | return statement |
| `inout` | read-write parameter, written at the call site too |
| `defer` | cleanup at block exit |
| `if`, `else` | branches, as a statement or an expression |
| `for`, `in` | the one loop form |
| `break`, `continue` | leave or restart the innermost loop |
| `enum` | sum types and integer enums |
| `match` | branch on a value, as a statement or an expression |
| `try` | return early on `err` or `none` |
| `import`, `as` | import a package, renaming it on a clash; `as` also renames a destructured field |
| `pub` | visible outside the package |
| `unsafe` | unchecked operations, and functions with unchecked preconditions |
| `extern` | a C function declared without a body |
| `union` | untagged unions, for C interop |
| `shared` | a global reachable from interrupt handlers or other contexts |
| `lock` | the only place a `shared` global can be accessed |

`true` and `false` are predeclared names, not keywords, as in Go.

Keywords are reserved everywhere, so none can be a field or variable
name: a field cannot be called `type`, `in` or `inout`.
`_` is a reserved token for discards, not a keyword.

### Keyword-led declarations and statements

**Decided.** Every declaration and statement starts with a keyword: `fn`,
`let`, `var`, `type`, `if`, `for`, `return`. A highlighter can classify
tokens without a parser. From Go and Rust.

Exceptions: assignments (`p.x = 1`) and call statements (`bump(inout a)`)
start with an expression, as in Go and Rust.

### Explicit types on every binding

**Decided.** `let x: i32 = f()`, never `let x = f()`. The user's reason:
in Go and Rust a reader often needs the compiler to learn a variable's
type. It also makes checking a single forward pass over each body, with no
inference across statements.

```
fn manhattan(a: Point, b: Point) -> i32 {
    let dx: i32 = abs(a.x - b.x)
    let dy: i32 = abs(a.y - b.y)
    return dx + dy
}
```

### Lexer state, but no parser feedback

**Decided.** The lexer may keep state. It never needs information from the
parser.

- **Nested block comments**, with a depth counter.
- **String interpolation**, with a mode stack. On `{` inside a string the
  lexer pushes code mode and counts braces; on the matching `}` it returns
  to string mode. It emits `StringStart`, `StringMid` and `StringEnd`
  tokens around the expression tokens. From Swift, Kotlin and Dart.

The user chose both for their usability, overriding an earlier suggestion
of a context-free lexer. The rule exists to avoid lexers that need the
parser: C's typedef names, Rust splitting `>>` inside generics, and
JavaScript's `/` meaning either division or a regex.

### Optional semicolons

**Decided.** Go's rule: at a newline, the lexer inserts `;` if the line
ends in an identifier, a literal, `)`, `]`, `}`, `return`, `break` or
`continue`. Writing `;` explicitly puts two statements on one line.

Consequences, enforced by the formatter:

- An opening `{` goes on the same line as its `if`, `for` or `fn`.
- A broken expression ends the line with its operator.

Found by the stage 1 lexer on 2026-11-08: an attribute on its own line
that ends in `)`, such as `@export("main")`, gets a semicolon after it.
The parser skips semicolons between an attribute and its declaration.

Rejected: JavaScript's rule, which inserts `;` only when parsing fails, so
a line starting with `(` or `[` joins the line above it.

### No `<>` for type arguments

**Decided.** `a < b > c` is ambiguous, which is why Rust needs `::<>` and
C++ needs `template` disambiguators. `[T]` is reserved for generics, as in
Go, so they can be added later without breaking the grammar.

### Struct literals

**Leaning (2026-11-08).** A value of a named type is written `T.(value)`:
the type name, a dot, and a parenthesized value of the underlying type.
For a record, the value is a tuple literal.

```
let p: Point = Point.(x: 1, y: 2)
let t: Celsius = Celsius.(20.0)
let raw: f64 = f64.(t)
let n: i64 = i64.(count)
```

- One form covers record literals, wrapping and unwrapping newtypes, and
  numeric conversions. Whether numeric conversions may truncate is open.
- After `.`, an identifier means field access and `(` means construction,
  so one token of lookahead decides. `pkg.Point.(...)` works.
- A name followed by `.(` is always a type, so a highlighter needs no name
  resolution.
- Only a type name can precede `.(`. Conversions to `[]T` or `*T` are
  unsafe intrinsics.
- Go's type assertion `x.(T)` puts the type on the other side.

Rejected:

- `Point(x: 1, y: 2)`, Go's conversion form: if functions ever take
  keyword arguments, the user's stated possibility, a call and a
  construction would look the same. `Celsius(20.0)` already looks like a
  call.
- `Point.{ x: 1, y: 2 }`, the earlier leaning from Zig: records now use
  parentheses, not braces.
- An implicit conversion from a tuple literal when the target type is
  known: the type is invisible in an argument like `draw((x: 1, y: 2))`.

### Built-in formatter

**Leaning.** The compiler ships the formatter, like `gofmt`, so layout is
never a review topic.

## Types and values

### No null

**Decided.** A built-in optional type instead. From Rust, Swift and Zig.
See [Errors as values](#errors-as-values) for its spelling.

### No implicit numeric conversions

**Decided (2026-11-08).** From Go and Rust. Numbers change type only
through `T.(x)`, which is checked:

- A conversion that cannot lose the value always succeeds:
  `i64.(x)` with `x: i32`.
- A narrowing conversion traps if the value does not fit.
- Deliberate truncation uses a built-in, `truncate[u8](x)`.

`T.(x)` never silently changes a value.

### Number literals

**Decided (2026-11-08).**

- Decimal, `0x` hex, `0b` binary and `0o` octal integers, with `_`
  between digits: `1_000_000`, `0xFF_FF`.
- A float has digits on both sides of the point: `1.0` and `1.5e-3`, not
  `1.` or `.5`.
- No type suffixes such as Rust's `1u8`. A literal takes its type from
  where it is used, and every binding states one. A literal that does not
  fit, as in `let b: u8 = 300`, is a compile error.

### Integer overflow

**Decided (2026-11-08), syntax only.** Wrapping and saturating
arithmetic use built-in functions such as `wrapping_add(a, b)`, not
operators like Zig's `+%`. The operator set stays small and every intended
wrap is visible.

**Decided (2026-11-08):** plain arithmetic traps on overflow in every
build, as in Swift and like bounds checks. So do division by zero,
`i32.min / -1`, and a shift by at least the type's bit width. Rust traps
only in debug builds, so release code behaves differently from the code
that was tested and reviewed; a reviewer would have to know which build
they reason about.

The cost is a branch per operation, which the optimizer removes in most
loops with known bounds. Code that cannot afford it uses
`wrapping_add` for an intended wrap, or an `unsafe` `unchecked_add` where
the author knows it cannot overflow.

A trap is a [`panic`](#stopping-the-program-panic) with a message the
compiler writes.

### Strings and text

**Decided (2026-11-08).** String and character literals are ASCII only for
now. UTF-8 and UTF-16 handling goes in the standard library until a need
for language support shows up. ASCII is a subset of UTF-8, so every literal
stays valid if UTF-8 moves into the language later.

- **Source files are UTF-8** (decided 2026-11-08). The compiler checks
  before lexing and reports the first byte that breaks UTF-8 as the
  file's only error. Outside comments, the lexer still accepts ASCII
  only.
- A string literal is a `[]u8`, a view of static bytes, with no encoding
  guarantee. There is no separate `str` type (decided 2026-11-08, the
  user's proposal): one byte view type covers text and binary data, and
  indexing and slicing it use byte offsets, as for any view. A UTF-8
  string type can come from the standard library.
- A string literal holds printable ASCII (`0x20` to `0x7E`). A tab, a
  newline or any other control character must be an escape, so invisible
  or direction-changing characters cannot hide in a literal (the "Trojan
  Source" attack).
- Escapes: `\n`, `\t`, `\r`, `\0`, `\\`, `\"`, `\'`, `\{`, and `\x00`
  to `\x7F`. Every escape uses a backslash; `{{` is not an escape.
- Interpolation is `{expr}`. A `}` outside an interpolation is plain
  text. A literal with interpolation is a `Format`, not a `[]u8`; see
  [Formatting without a heap](#formatting-without-a-heap).
- No format-specifier language. Formatting is an ordinary call:
  `"addr={hex(a, 8)}"`, not `"addr={a:08x}"`.
- `'a'` is an integer literal holding an ASCII code. Like a number, it
  takes its type from context: `let b: u8 = 'a'`. There is no `char`
  type.

Discussed and set aside on 2026-11-08: `str` as guaranteed UTF-8 (Rust),
with a `char` type for Unicode scalar values and character literals typed
from context. Grapheme clusters (Swift's `Character`) need Unicode tables,
too much for embedded targets.

### Enums

**Decided (2026-11-08).** An enum is a sum type. Each variant may carry a
payload, which is a named tuple.

```
type Shape enum {
    circle(radius: f32)
    rect(w: f32, h: f32)
    empty
}
```

- An enum appears only in a `type` declaration, so every enum has a name.
- Variants are separated by line ends, or `;` on one line. No commas.
- A value is built with the construction rule:
  `Shape.circle.(radius: 1.0)`, and `Shape.empty` for a variant without a
  payload.

### Integer enums, closed and open

**Decided (2026-11-08).** An enum without payloads can name its integer
type and give each variant a value, for hardware and protocol values.

```
type Irq enum u8 { timer = 0; uart = 3 }

type Opcode enum u8 {
    nop = 0
    halt = 1
    _
}
```

- **Closed** (`Irq`): only the listed values exist. `Irq.(byte)` traps if
  `byte` is not one of them, the same checked conversion numbers use.
- **Open** (`Opcode`, marked by a trailing `_`): every `u8` is a valid
  `Opcode`, and the names are the known ones. `Opcode.(byte)` always
  succeeds, so a register or packet holding an unknown value is not an
  error. From Zig's non-exhaustive enums.
- `u8.(op)` always succeeds for both.

The user called these "raw enums". Untagged unions (C's `union`) were the
other reading, and belong with `unsafe` in part 7.

### Line items and comma items

Braces hold items separated by line ends: statements, enum variants and
match arms. Parentheses hold items separated by commas: fields,
parameters and arguments. A multi-line parenthesized list ends each line
with a comma.

## Expressions

### Operator precedence as a partial order

**Decided (2026-11-08).** Some operator groups have no order between
them, so mixing them without parentheses is a syntax error. A reader never
needs to remember the table for those pairs. From Carbon.

| Group | Operators | Rule |
| --- | --- | --- |
| Postfix | call, `.field`, `.(...)`, `[i]` | Tightest |
| Prefix | `-x`, `!x`, `~x` | |
| Multiplicative | `*` `/` `%` | Above additive |
| Additive | `+` `-` | |
| Bitwise | `&` `\|` `^` `<<` `>>` | Never mixed with arithmetic or with a different bitwise operator. `&`, `\|` and `^` repeat; shifts do not chain. |
| Comparison | `==` `!=` `<` `<=` `>` `>=` | Below arithmetic and bitwise. Non-associative. |
| Logical | `&&` `\|\|` | Lowest. The two do not mix. |

```
a + b * c          // fine
flags & MASK == 0  // fine: & binds tighter than ==, unlike C
(a & b) | c        // fine
a & b | c          // error
x << 2 + 1         // error
a < b < c          // error
a && b || c        // error
```

The grammar has one layer of nonterminals per group, so it enforces these
rules without precedence declarations.

Logical operators are symbols, not the words `and`, `or` and `not`, which
would add three keywords.

### Statements built from expressions

**Decided (2026-11-08).**

- Assignment and compound assignment (`+=`, `<<=` and the rest) are
  statements. No `++` or `--`.
- The left side of an assignment is a postfix expression, so `a + b = c`
  does not parse.
- Only a call can stand alone as a statement, as in Go. `a + b` on its
  own line is a syntax error.

## Control flow

### `if`

**Decided (2026-11-08).** `if cond { ... } else if cond { ... } else
{ ... }`. No parentheses around the condition; braces always. From Go and
Rust. The semicolon rule requires `else` on the same line as the `}`
before it.

Struct literals never use braces (see
[Struct literals](#struct-literals)), so a `{` after a condition always
starts the body. Rust and Go need a special rule for that.

### `if` as an expression

**Decided (2026-11-08).** The user's reason: it simplifies a lot of code.
A value chosen by a condition can be a `let` binding instead of a `var`
assigned in each branch.

```
let x: i32 = if c { a } else { b }
let s: []u8 = if n == 0 { "none" } else if n == 1 { "one" } else { "many" }
```

- The `else` is required.
- Each branch is one expression, not a block of statements.
- An `if` expression is never an operand: `1 + if c { a } else { b }`
  does not parse. It can be a binding's value, an argument or a return
  value.
- No ternary `?:`.

### Loops: one `for`

**Decided (2026-11-08).** From Go.

```
for { ... }                                 // forever
for i < n { ... }                           // while
for let p: Point in points { ... }          // over an array or view
for let (k: Key, v: Value) in pairs { ... } // with destructuring
for let i: u32 in 0..n { ... }              // over a range
for var line: []u8 in lines { ... }          // the body may change its copy
```

- No C-style three-part `for`.
- `a..b` excludes `b`, and there is no inclusive form. A range appears
  only in a `for` header for now; slicing is part of the view design.
- The loop variable starts with `let` or `var`, followed by anything a
  `let` declaration accepts, including destructuring and `_`. So every
  binding in the language starts with `let` or `var`.
- After `for`, one token decides the form: `let` or `var` starts a loop
  variable, `{` starts a forever loop, and anything else starts a
  condition.
- The loop variable's type is written out, which repeats the element
  type. If the repetition becomes a problem, inferring it is a possible
  later change.

History: the first version was `for p: Point in points`. On 2026-11-08 the
tree-sitter grammar showed that destructuring there was ambiguous:
`for (a: b, c: d) ...` reads as a pattern before `in` or a tuple-literal
condition, and the parser cannot tell until `in`. The user chose
`for let` to fix it.

### Iterators

**Decided (2026-11-08): candidate A, external iterators without
traits.** The user preferred Rust's external iterators to internal ones.
Deferred earlier the same day, because an iterator holding a view needed
a view struct; once views could be fields, an iterator became an
ordinary struct.

```
type Lines (rest: []u8)

fn lines(s: []u8) -> Lines { return Lines.(rest: s) }
fn next(it: inout Lines) -> ?[]u8 { ... }

for let line: []u8 in lines(text) { ... }
```

- `for let x: T in e` works on arrays, views and ranges, and on any
  value of a type `X` whose package declares
  `fn next(it: inout X) -> ?T`. The compiler finds `next` by name, as
  Zig does, with no traits.
- The INI example iterates over lines this way.

Limitations:

- Each item borrows the iterator, not the collection, so it ends when
  `next` is called again. A `for` loop is unaffected, but collecting
  earlier items is not possible. Rust tells the cases apart with named
  lifetimes.
- A recursive traversal needs an explicit stack inside the iterator.
- No `map` or `filter` adapters without generics.

A supports `zip`, peeking, and pausing an iteration.

Candidate B, internal iteration. An `iter fn` `yield`s to the loop body,
which the compiler inlines. No iterator object exists, and recursion works.
It cannot express `zip` or pausing. From CLU iterators, Ruby blocks and
Hylo projections.

Rejected: candidate B as well. Choosing both would give two ways to
iterate.

### `match`

**Decided (2026-11-08).** `match` has the same shape as `if`: arms with
blocks as a statement, arms with one expression each as an expression.
There is no `=>`.

```
let area: f32 = match s {
    circle(radius: f32) { pi * radius * radius }
    rect(w: f32, h: _) { w * w }
    empty { 0.0 }
}
```

- A payload pattern is the destructuring syntax: fields by name, each with
  a type or `_`.
- A bare name in a pattern is always a variant, never a new binding,
  because bindings always state a type. Variants are not qualified
  (`empty`, not `Shape.empty`); the matched value's type says which enum.
- Integer, character and string literals are patterns too, including
  negative integers.
- A match must be exhaustive. Arms never fall through.
- A `_` arm matches everything else. The user chose to allow it on enums
  too: it is useful in most matches. The cost is that a new variant falls
  into an existing `_` arm without a compile error.
- A match on an open enum always needs a `_` arm.

**Decided (2026-11-08):** commas separate several patterns in one arm.

```
match s[i] {
    ';', '#' { return s[..i] }
    _ { }
}
```

- An arm with more than one pattern cannot bind payloads, so no rule is
  needed for alternatives that bind different names.
- Range patterns are deferred. Ranges exclude their end, so `a` to `z`
  would read `'a'..'{'`, and an inclusive form would be a second range
  syntax. `if` covers these cases.
- No guards such as `x if x > 3`; they are another way to write `if`.

### `break` and `continue`

**Decided (2026-11-08).** They act on the innermost loop.

**Deferred:** labels. Each spelling clashes with existing syntax:
`outer: for` reuses `name:` and needs two tokens of lookahead, and Rust's
`'outer` clashes with character literals.

### Indexing and type arguments share `[ ]`

`v[i]` and `truncate[u8](x)` parse the same way, and the checker tells
them apart, as in Go. A highlighter cannot color a type argument
differently without name resolution.

A named type in brackets parses as a name. Types that cannot start an
expression are accepted directly: `from_address[*u32](a)`,
`cast[[]u8](x)`, `cast[?*u8](x)`, `cast[fn(i32) -> bool](f)`. A tuple type
can start an expression, so a tuple type argument needs a named type.
Found when `from_address[*u32]` failed to parse on 2026-11-08.

### Errors as values

**Decided (2026-11-08).** A built-in result type and one propagation
operator, no exceptions. From Rust's `Result` and `?`, Zig's error unions
and Go's error returns.

Optional and result are built-in enums, so `match` handles them with no
extra unwrapping syntax.

| Type | Variants |
| --- | --- |
| `?T` | `some(value: T)`, `none` |
| `T ! E` | `ok(value: T)`, `err(error: E)` |

```
fn read(f: inout File, buf: inout []u8) -> usize ! IoError { ... }

fn copy(path: []u8) -> usize ! IoError {
    var f: File = try open(path)
    defer close(inout f)
    let n: usize = try read(inout f, inout buf)
    return ok.(value: n)
}
```

- `E` can be any type, usually an enum.
- A result type sits only at the top of a type, so `[]T ! E` is a result
  holding a view. A view of results needs a named type. `T ! E ! F` does
  not parse.
- **Wrapping is explicit.** `return ok.(value: n)`, `return none`,
  `return err.(error: e)`. The user chose this over Swift's and Zig's
  implicit wrapping, keeping the no-implicit-conversion rule without
  exceptions for now.
- **`try`** is a prefix: `try read(...)` returns early with the `err`, or
  with `none` for an optional. It sits at the start, where a reviewer
  scans, unlike Rust's postfix `?`. It applies to its operand only:
  `try read(...) + 1` adds to the unwrapped value.
- The error type must match the function's exactly. No automatic
  conversion like Rust's `From`; converting is an explicit `match`.

**Decided (2026-11-08):** a function that returns nothing but can fail
is written `-> ! IoError`, and succeeds with a bare `return ok`. The
language has no unit type, so there is no `T` to put before `!`. Falling
off the end of such a function is an error: success is written out, as
all wrapping is. The INI example uses this form.

Rejected: `Result[T, E]` as a built-in generic. It uses the reserved
`[T]` form but is longer in every signature.

### Stopping the program: `panic`

**Decided (2026-11-08).** The built-in `panic("message")` stops the
program. It is for states the code cannot recover from and for
unfinished paths while code is being written. Errors a caller can handle
go through results, as above.

```
let k: Kind = match op {
    add { Kind.additive }
    _ { panic("parse_op: unknown operator") }
}
```

- It is a function, not a keyword, and never returns. A `panic` call can
  stand where any value is expected, as in the arm above, and a function
  needs no `return` after one.
- One built-in covers Rust's `panic!`, `todo!` and `unreachable!`, so
  there is one way to stop.
- Bounds, overflow and division traps take the same path, with a message
  the compiler writes.
- The user asked for a "bail" function. `panic` is used because `bail`
  means an early error return in Rust's anyhow crate.

What a panic reports and what happens next is in [Runtime](#runtime).

### No user generics; built-in generic types

**Leaning.** Users cannot define generic types or functions. The compiler
provides a few: arrays, views, optional and result. Go 1.0 did this with
slices, maps and channels. It removes monomorphization, the main source of
flux's backend cost.

**Decided (2026-11-08):** the language also provides a growable array,
`[..]T`, which is also the owned string (`[..]u8`). See
[Allocation](#allocation). There is no built-in map in the first version.

### Named tuples

**Decided.** Every tuple field has a name. A named tuple is an anonymous
struct with structural identity. The user proposed this to avoid writing
intermediate structs, mostly for multiple return values.

```
fn divmod(a: u32, b: u32) -> (q: u32, r: u32) {
    return (q: a / b, r: a % b)
}
```

Rules:

- Two named tuples with the same field names and types, in the same order,
  are the same type. The type interner hash-conses them, so the compiler
  cost is small.
- Field order is part of the type, because it fixes the layout.
- Destructuring binds by name, not position, so swapped names cannot
  silently bind the wrong values.
- No positional tuples, so `.0` and `.1` never appear.

Influences: Swift's labeled tuples, Go's named results, and structural
records in ML and Elm.

### Type declarations: `type <name> <type>`

**Decided (2026-11-08).** `type Name T` defines a new nominal type whose
layout is `T`. It is never an alias. From Go's defined types.

```
type Point (x: i32, y: i32)
type Celsius f64
type PageAddr u64
```

- A struct is a named tuple given a name by `type`. The language has no
  `struct` keyword and no anonymous struct type, so a record has one
  spelling: the named tuple.
- `Point` and `(x: i32, y: i32)` are different types, with no implicit
  conversion between them.
- A newtype such as `Celsius` does not convert implicitly to or from
  `f64`, so a physical address cannot be passed where a virtual one is
  expected.
- Every named type starts with `type Name`, so a grep for it finds the
  declaration whatever kind of type it is.
- Type aliases do not exist.

Multi-line fields need a trailing comma, because the semicolon rule
inserts `;` after a type name at a newline. The formatter adds it.

```
type Header (
    magic: u32,
    len: u32,
)
```

**Decided (2026-11-08): newtypes and operators.** An integer literal
takes a newtype from context: `let fd: c.int = 3`. Operators do not
apply to a newtype; code unwraps first, as in `i32.(fd) < 0`. A newtype
is meant to opt in to its operators, so `Celsius + Celsius` can work
while `Celsius + Meters` never does, but the opt-in mechanism is open.

Rejected:

- `type Point struct { ... }` (Go): `struct { ... }` would be a type
  expression, which gives anonymous structs, a second spelling of named
  tuples.
- `struct Point { ... }` (Rust, Zig): a grep must know each declaration
  keyword, and newtypes need a one-field struct.

### Destructuring

**Decided (2026-11-08).** `let` or `var` with a tuple pattern binds fields
by name. Each binding shows its type, as every binding does.

```
let (q: u32, r: u32) = divmod(17, 5)
let (q: u32, r: _) = divmod(17, 5)
let (x: i32, y: i32) = p            // p: Point
```

- Each name must be a field of the value. Order does not matter.
- Every field must be listed, so adding a field to a return type breaks
  each destructuring caller where a reviewer sees it.
- `_` in the type position skips a field. The field name stays, because
  binding is by name: `(q: u32, _: u32)` would not say which field is
  skipped.
- It works on named types such as `Point`, which are named tuples
  underneath.
- **Decided (2026-11-08):** `as` binds a field under another name:
  `let (q as quotient: u32, r: _) = divmod(17, 5)`. It reads "field `q`,
  as `quotient`, of type `u32`". It reuses the import keyword. The same
  form works in `match` payloads, where a nested match would otherwise
  shadow the outer binding:

  ```
  match next {
      some(value as b: u8) { ... }
      none { ... }
  }
  ```

  The examples raised the need. Renaming a skipped field
  (`r as x: _`) is a checker error.
- **Decided (2026-11-08):** `var` right before the bound name makes
  that one binding mutable: `some(var value: [..]u8)`, or
  `some(value as var bytes: [..]u8)`. It is the same place `var` takes
  in `var x: T`. Since [Values and buffers](#values-and-buffers),
  `free` and `take` change their argument, and a `match` payload was
  always immutable, so freeing one took a rebinding first:

  ```
  match read_all(alloc, path) {
      ok(value as var text: [..]u8) {
          defer free(inout text)
          ...
      }
      ...
  }
  ```

  Rejected: letting `free` take a `let` binding without emptying it,
  which would make `free` behave two ways.

Rejected: `let (q: quot, r: rem) = ...` for renaming, because it reuses
`name: Type` with a different meaning.

### The `_` name

**Decided (2026-11-08).** Outside destructuring, `_` as a binding name
discards the value: `let _: u32 = f()`, or an unused parameter
`fn handler(_: Event)`. A discard still shows its type.

Ignoring any function result without `let _` is a compile error, as in
Zig. Every dropped error is visible in review.

### Names and scopes

**Decided (2026-11-08).** A name is looked up in the enclosing locals,
innermost first, then the package's top-level declarations, the file's
imports, and the built-in names.

- **No shadowing.** A local never reuses a name already in scope: an
  outer local, a parameter, a top-level declaration, an import, a
  built-in type or a built-in value. A reader never has to work out
  which `x` a line means; `as` covers the nested `match` case. A
  top-level declaration may reuse a built-in name, as in Go, so
  `std.libc` can declare C's `free`.
- **Built-in functions are names only in call position.** Decided on
  2026-11-08, after `offset` could not name a parameter. A local may
  take a built-in function's name: bare `offset` means the local, and
  `offset(...)` means the built-in unless the package declares its own
  `offset`. Calling a local function value that has a built-in's name
  is an error, since the call would not reach it. Rejected: Go's rule,
  where a local hides the built-in in its scope, which breaks the rule
  against shadowing.
- **Unused names are errors**, as in Go: imports, locals, parameters and
  payload bindings. `_` discards on purpose: `fn f(_: u8)`,
  `err(error: _)`. Parameters are included because the user-chosen
  discard already covers them, and a function that must match a function
  type, such as an allocator's `free`, names unused ones `_`.
- **A top-level name is declared once per package**, across all its
  files, and may not also be an import name in any of them.

**Decided (2026-11-08): the built-in names.** One fixed list, all
predeclared, none keywords.

| Kind | Names |
| --- | --- |
| Types | `i8` `i16` `i32` `i64` `isize` `u8` `u16` `u32` `u64` `usize` `f32` `f64` `bool` `Allocator` `Format` `PanicInfo` `OutOfMemory` |
| Values | `true` `false` `none` `some` `ok` `err` |
| Control | `panic` |
| Growable arrays | `make` `push` `pop` `append` `clear` `free` `take` `copy` |
| Arrays | `fill` |
| Pointers | `from_address` `from_pointer` `offset` `cast` `volatile_load` `volatile_store` |
| Conversions | `truncate` `c_string` |
| Arithmetic | `wrapping_add` `wrapping_sub` `wrapping_mul` `saturating_add` `saturating_sub` `saturating_mul` `unchecked_add` `unchecked_sub` `unchecked_mul` |

### Multiple return values

**Decided.** Through named tuples. Large results are written directly into
the caller's slot (return value optimization), so returning by value costs
the same as an output parameter.

Rejected: an `out` parameter mode (Ada, C#, Hylo's `set`). It would be a
second way to return values.

## C interop and layout

### Attributes

**Decided (2026-11-08).**

```
@packed
type Header (magic: u32, len: u16)

@section(".text.boot") @export("reset_handler")
pub fn reset() { ... }
```

- `@name` or `@name(args)`, with integer and string literal arguments
  only. Attributes go before a declaration, on its line or the lines
  above.
- The compiler defines every attribute. There are no user attributes, so
  attributes cannot grow into a macro system.
- An attribute may change layout, ABI or linking, never what the code
  means, so a reviewer can skim them. Untagged unions change meaning, so
  they get a keyword instead.
- `@name` is one token, so a highlighter needs only the lexer.

Attributes so far: `@packed`, `@align(N)`, `@export`, `@export("name")`,
`@section("name")`, `@interrupt`, `@panic_handler`.

### C layout for every record

**Decided (2026-11-08), "for now".** Field order already fixes a record's
layout, so every record uses C's rules: declaration order, natural
alignment, C padding. No `repr(C)` to forget, and the layout reads off
the declaration.

- The compiler never reorders fields to save padding; ordering fields
  well is the author's job.
- `@packed` removes padding; `@align(N)` raises a type's alignment.
- Integer enums have their integer type's layout. Enums with payloads have
  an unspecified layout and cannot cross the C boundary.
- `?*T` is a nullable C pointer.

### A specified layout for every type

**Leaning (2026-11-08).** The user asked for layouts specified the way
C++'s standard types are, so a compiled library has an ABI and does not
depend on the compiler that built it, unlike Rust. Every type gets a
layout written down here, expressed as C:

- `[]T`: `(ptr: *T, len: usize)`. An empty view's pointer is not null.
- `[..]T`: `(ptr: ?*T, len: usize, cap: usize, alloc: Allocator)`.
- An enum with payloads: a tag, then a C union of the payload records.
  Variants are numbered from 0 in declaration order, and the tag is the
  smallest of `u8`, `u16` and `u32` that holds them. This is Rust's
  `#[repr(C, u8)]` and the tagged union a C programmer writes by hand.
- `?T`: the enum `{ none, some(value: T) }`, so a `u8` tag, then `T`. The
  exceptions are `?*T` and an optional function value, which are a
  pointer that may be null.
- `T ! E`: the enum `{ ok(value: T), err(error: E) }`.
- `Format`, `PanicInfo` and `Allocator` are records, with C layout like
  every record.

Symbols: a function's symbol is its package path and name,
`std.io.open`. Without overloading or generics, no mangling is needed.
The root package's path is its directory's name, and a package under the
root adds its own path to that: `ini.ini.parse_line` for the `ini`
package of a program in `ini/`. Without the prefix, a root named `ini`
and its package `ini` would share symbols. The root's `main` is
`tern_main`, and `@export` sets a symbol by hand.

With this, std can ship as `libstd.a` or a shared library plus the export
data that importers read, and any compiler that follows these rules can
link against it. The cost is that a layout is fixed once chosen:
optimizations such as packing an optional into a type's unused bit
patterns, which Rust does, are not available except where listed.
Open: whether `Allocator` should be two words, a context and a pointer
to a table of its three functions, as Zig's is, to keep `[..]T` at five
words instead of seven.

### `extern` functions

**Decided (2026-11-08).**

```
extern fn write(fd: c.int, buf: *u8, n: usize) -> isize
extern fn printf(format: *u8, ...) -> c.int
```

- An `extern fn` declares a C function and has no body.
- Calling one is always `unsafe`, because C can do anything.
- Parameters and results use integers, floats, pointers, `?*T`, records
  and integer enums. Views cannot cross: pass a pointer and a
  length.
- `...` marks a C variadic function. It comes last, and only in an
  `extern` declaration.

**Decided (2026-11-08):** `extern let` and `extern var` declare a C
global or a linker symbol, with a type and no initializer.

```
extern var errno: c.int
extern let _sbss: u8        // only its address matters: &_sbss
```

- Reading or writing one requires `unsafe`, since C or the linker owns
  it. Taking its address with `&` is safe, as for any pointer.
- It adds no keyword.
- C's integer types come from a standard package: `c.int`, `c.long`,
  `c.size_t`.
- `@export` or `@export("name")` gives a function an unmangled symbol so
  C can call it.
- Library names and linking belong to the build system.

### One calling convention

**Decided (2026-11-08).** Every function uses the platform's C calling
convention, so any `fn(...)` value can be passed to C as a callback, such
as for `qsort` or an interrupt table. No separate `extern "C" fn` type.

The cost is giving up a custom internal ABI. It is small: on x86-64 and
AArch64 the C convention already returns records up to 16 bytes in
registers. A view passes as a pointer and a length.

**Decided (2026-11-08): a `let` parameter held in memory is a pointer.**
A record, tuple, view, array or other value held in memory that a tern
function takes as `let` arrives as a pointer to the caller's value, on
every target; C sees `fn f(b: Big)` as `void f(const Big *b)`. This
makes the promise of [Mutable value semantics](#mutable-value-semantics)
real: before, every such argument was copied, which made ternc's parser
four times slower. Function types follow the same rule. An `extern`
function takes records by value, as C does, so a call to C still copies,
and an `extern` function that takes a record by value cannot be a
function value. Rejected: keeping C's by-value records exactly and
skipping the copy only where C already passes a pointer (AArch64,
records over 16 bytes), which leaves the copies on x86-64.

The caller passes the address of its own storage when the argument is
a local, or a field or element held in a local's memory or `[..]T`. It
copies when the callee could change the value another way: the argument
is a global, or is reached through a view or a pointer, or another
argument of the same call holds a call that passes something `inout`.
A callee that changes the caller's value through an alias the
compiler cannot see, such as a global that an `inout` parameter also
names, makes the change visible through the parameter; this is defined
behavior, as for `inout` under [Exclusivity](#exclusivity). The
`borrow` run test covers each case.

Rule 4 (2026-11-09) changes this once it is built: a callee that writes
a global cannot take that global as an argument, so the copy for
globals and the visible change through an alias both go away.

### Untagged unions

**Decided (2026-11-08).**

```
type Word union (bits: u32, value: f32)

var w: Word = Word.(bits: 0x3F80_0000)
let v: f32 = unsafe { w.value }
```

- Members are written like named tuple fields and read with `.`.
- Reading a member requires `unsafe`, because it reinterprets bytes.
  Writing one is safe: the danger is only in a later read of another
  member, which is already `unsafe`.
- The user asked whether unions should be `unsafe` more broadly. Marking
  the declaration or writes `unsafe` would add noise without adding
  safety. Making some reads safe, for unions whose members accept every
  bit pattern such as `u32` and `[4]u8`, is possible later.
- A union appears only in a `type` declaration, like an enum.

### C strings

**Decided (2026-11-08).** The built-in `c_string("text")` takes a string
literal and produces a `*u8` to static, NUL-terminated bytes, computed at
compile time. No new literal form like Rust's `c"..."`.

### Interrupt handlers

`@interrupt fn uart_isr()` sets the handler ABI. What a handler may touch
is in [Concurrency](#concurrency).

## Concurrency

Tasks share memory through mutable globals, views, raw pointers and
library types built on `unsafe`. The design gives rules for globals only.
A view cannot be stored (see [Views](#views)), so only a global or a
raw pointer reaches another context.

The user asked on 2026-11-08 for the syntax and design now. A compiler
will not implement this for a long time.

### `shared` globals and `lock`

**Decided (2026-11-08).** Thread safety is opt-in: only a global marked
`shared` can be reached from more than one context.

```
shared var rx: Ring = Ring.(head: 0, tail: 0)

@interrupt
fn uart_isr() {
    lock rx {
        push(inout rx, read_data_register())
    }
}

fn next_byte() -> ?u8 {
    lock rx {
        return pop(inout rx)
    }
}
```

- `shared var` marks a global reachable from interrupt handlers and other
  contexts. Plain `var` globals belong to the main context. `shared let`
  does not exist; a `let` global never changes and is safe to read
  anywhere.
- A `shared` global is accessed only inside `lock g { ... }`. The target
  decides what a lock does: mask interrupts on a single-core
  microcontroller, save interrupt state and take a spinlock in an SMP
  kernel, take a mutex in user space. The same source works in all three.
- The check is local: touching a `shared` global outside a `lock` block
  is an error.
- `lock` blocks do not nest lexically. `lock a, b { ... }` takes several
  globals in a fixed order.
- `lock` names globals only, not expressions.

Checkable again since write sets are inferred (2026-11-09), and planned:
that an interrupt handler reaches only `shared` globals, and that a call
inside `lock` does not reach another `lock`, which could deadlock. Both
need every global a callee touches, which the inferred sets give once
they record reads too. Until then the author and the reviewer check
them, as in C.

`shared` is a keyword, not an attribute, because it changes which rules
apply; attributes never change what code means.

### Deferred

- **`parallel`**, structured fork-join: `parallel (a: f(x), b: g(y))`,
  checked like one call with several arguments, so branches can borrow
  from the caller. The user judged it too much for the initial design.
- **Threads and channels.** A spawned function would take parameters
  it alone holds and use only `shared` globals. The API needs the allocator
  model and generics.
- **Atomics.** `lock` covers flags and counters at the cost of an
  interrupt mask or lock per access. An `atomic u32` usable without a lock
  can come later.
- **Per-instance shared state**, such as a concurrent hash map with its
  own lock. For now a library uses `unsafe` internally, as Rust's `Mutex`
  does.

## Packages

### Packages are directories

**Decided (2026-11-08).** A package is a directory, as in Go. Files carry
no `package` line: the directory already says which package a file
belongs to.

### Imports

**Decided (2026-11-08).**

```
import std.io
import vendor.x.io as xio

let f: io.File = try io.open(path)
```

- Paths are dotted names, not strings. The package's last segment names
  it.
- `as` renames, for when two imports would share a name.
- Every use is qualified. No unqualified imports (Rust's `use x::*`, Go's
  dot import), so a reader always sees where a name comes from.
- Imports come before every declaration in a file, enforced by the
  grammar.
- Packages cannot form import cycles (see
  [Compiler design](#compiler-design)).

**Decided (2026-11-08): one root, no project file.** The compiler is
given one directory, the root package. An import path resolves in one of
two places:

- `std.` starts a standard library path, inside the library that ships
  with the compiler. The name `std` is reserved.
- Anything else is a directory under the root: `import board` is
  `<root>/board/`, and `import board.uart` is `<root>/board/uart/`.

The root package itself cannot be imported. The user chose this over a
project file for now; dependencies on other projects are deferred (see
[Deferred](#dependencies)).

### Visibility

**Decided (2026-11-08).** `pub` marks a top-level declaration or a field
as visible outside its package. There are two levels only: package-private
and `pub`. No `pub(crate)`.

```
pub type Point (pub x: i32, pub y: i32)
pub type Buffer (data: *u8, len: usize)
```

- Fields are private unless marked. Outside its package, `Buffer` cannot
  be constructed or have its fields read, so a type can protect a safety
  invariant, such as a generational arena's handles.
- `pub` on a field appears only in a `type` declaration. Anonymous named
  tuples, such as return values, are fully visible. The grammar accepts
  `pub` on any tuple field and the checker enforces this: restricting it
  in the grammar conflicts with types like `type R (x: i32) ! E`.

Rejected: Go's capitalization. Exporting a name renames it at every use,
which inflates the diff a reviewer reads.

## Memory model

No garbage collector, because the language targets embedded and kernel
code (goal 3). The model has these layers.

### Temporal safety without lifetimes

**Decided (2026-11-09), replacing "temporal safety: not checked".** The
user made mathematical verification a priority (see
[Verification](#verification)), so safe code is now free of use after
free and dangling views, without lifetime annotations. Five rules do it,
each local to a function or a signature:

1. **One owner per buffer.** A type that holds a `[..]T` does not copy
   implicitly; `take` and `copy` say what happens. See
   [Values and buffers](#values-and-buffers).
2. **References do not escape, and a function can return a view
   derived from its parameters.** See
   [Second-class references](#second-class-references-and-parameter-modes).
3. **Records store indices, not views.** A view is a local, a
   parameter or a result, never a field or a `[..]T` element. See
   [Views](#views).
4. **The compiler infers which globals each function writes.** Nothing
   can alias an `inout` argument. See
   [Exclusivity](#exclusivity) and [Mutable globals](#mutable-globals).
5. **`unsafe` is the trusted boundary.** Raw pointers, C calls and
   allocators sit behind contracts the checker assumes. See
   [`unsafe`](#unsafe).

Together they make every safe function a mathematical function from its
inputs, `inout` arguments included, to its result and new `inout`
values. A verifier can model a `[..]T` as a sequence, with no heap.

The earlier design, from 2026-11-08, checked bounds and null only, as
Zig and Odin do. The one before it checked views with `in` clauses and
`uses` lists, and the user rejected it because a type holding a view
needed a new kind of type. Rule 3 avoids that: views never go in types,
so no type inherits anything.

**Status.** Decided and being built in this order, in stage 1 and ternc
together: rule 1, rule 4, then rules 3 and 2, which need std and ternc
to store spans and owned text instead of views. Until a rule is built,
the program it would reject still compiles.

### Bounds checks, no null, no pointer arithmetic

**Decided.** Safe code gets bounds-checked indexing, no null, and no
pointer arithmetic. This covers spatial safety with no runtime beyond the
checks.

### Mutable value semantics

**Decided.** Every variable owns its value. Assignment and argument passing
give the receiver its own value, so changing one variable never changes
another.

```
var a: Point = Point.(x: 1, y: 2)
var b: Point = a
b.x = 10
// a.x is still 1
```

The exception is a handle: a view or a `[..]T` refers to elements stored
elsewhere, and a copy of it refers to the same elements (see
[Values and buffers](#values-and-buffers)).

The compiler avoids the copies. A large read-only parameter is passed by
pointer (see [One calling convention](#one-calling-convention)), and an
assignment from a variable that is not used again can reuse its storage.

Sources: Hylo (formerly Val) and Swift structs. Paper: Dimitri Racordon,
Denys Shabalin, Daniel Zheng, Dave Abrahams and Brennan Saeta,
"Implementation Strategies for Mutable Value Semantics", Journal of Object
Technology 21(2), 2022. It expands "Native Implementation of Mutable Value
Semantics" (ICOOOLPS 2021, arXiv 2106.12678). Checked on 2026-11-08
against the Google Research listing and the authors' preprint.

### Values and buffers

**Decided (2026-11-08), revised 2026-11-09 for rule 1.** A type is
*owning* if it holds a `[..]T`: a growable array, or a record, tuple,
fixed array, optional, result or enum with an owning part. Every other
type copies byte for byte, as before.

- **An owning value never copies implicitly.** `let b: T = a`,
  `b = a`, a field in a construction, a `[..]T` element and an argument
  to `push` all need a value that nothing else holds: a call result, a
  construction, a literal, `make(alloc)`, `take(inout a)` or
  `try copy(alloc, a)`. Reading a place of owning type into a new home
  is an error that names both.
- **`take(inout v)`** returns the value and leaves `v` empty: no
  buffer, length and capacity 0, the same allocator. For a record it
  empties every owning part. `v` stays valid, so the checker tracks no
  moved-from state; this is what made the earlier move-only types heavy.
- **`return` of a local takes it.** `return v` or `return ok.(value:
  r.items)`, where the place starts at a local of the function, leaves
  the local empty before any `defer` runs, so `defer free(inout v)`
  frees nothing. A parameter or a global still needs `take`.
- **`try copy(alloc, v)`** is a deep copy into a new buffer from
  `alloc`.
- **Borrowing needs no copy.** A `let` argument, a `for let` element and
  a `match` payload bound from a place read the value where it lives;
  they cannot be stored without `take` or `copy`. A `var` payload of
  owning type needs a scrutinee that is not a place, such as
  `match take(inout x)` or `match try f()`.
- **`free(inout v)` is deep.** It frees every buffer `v` owns, at every
  level, and leaves `v` empty. Freeing an empty value does nothing, so a
  second `free` is harmless. With one owner per buffer this is
  well-defined; before rule 1, two copies could share a buffer and a
  deep `free` could not know which one owned it.
- A resource handle follows the convention. `io.File` is a plain record;
  `io.close(inout f)` sets the descriptor to -1, so a second `close`
  does nothing.

The user chose copying handles on 2026-11-08 because tracking moves was
neither simple nor intuitive: every friction item from porting the
lexer and parser came from it. Rule 1 keeps what that fixed, since
nothing moves and every variable is always valid, and adds back one
owner per buffer, which verification needs. Under the handle design,
`var all: [..]V = args` followed by `push(inout all, x)` left `args`
pointing at a freed buffer whenever the push moved the elements.

Rejected:

- **Copyable handles** (2026-11-08 to 2026-11-09): a copy of a `[..]T`
  shared its buffer, as in Zig. A verifier would need a heap model, and
  the compiler could not report a double free or a use after free.
- **Move-only types with flow tracking** (decided earlier on
  2026-11-08): `let b: T = a` moved or copied depending on what `T`
  held, and the checker tracked moved locals and fields through each
  function's control flow.
- **Declared owners (`move type`):** kept every move rule.

### Second-class references and parameter modes

**Decided.** A reference parameter is a way to pass a parameter, not a
type. It cannot be stored in a variable or a field, or returned. It
always points into a caller's frame and ends when the call returns.
Views, below, are the storable kind of reference, and are not checked.

Parameter modes:

- `let` (the default): read-only. The callee gets a reference or a copy,
  whichever is cheaper. The mode is never written: `p: Point`, not
  `p: let Point`, so it has one spelling (decided 2026-11-08).
- `inout`: read and write. The call site writes `inout` too, so a reviewer
  knows a call can change only the arguments marked `inout`.
- `sink`, which took ownership, was removed on 2026-11-08 with move-only
  types: nothing is owned, so there is nothing to take.

```
fn bump(p: inout Point) {
    p.x += 1
}

bump(inout a)
```

Sources: Hylo's `let` and `inout` modes; Swift's `inout` with `&` at
the call site; Pascal and Ada parameter modes. On second-class values in
general: Leo Osvald, Grégory Essertel, Xilun Wu, Lilliam I. González
Alayón and Tiark Rompf, "Gentrification Gone too Far? Affordable 2nd-Class
Values for Fun and (Co-)Effect", OOPSLA 2016, pages 234 to 251,
doi:10.1145/2983990.2984009. Checked on 2026-11-08 against the SPLASH 2016
program and researchr.

**Decided (2026-11-09): a function can return a reference derived from
its parameters** (rule 2). A function whose result holds a view, such
as `fn trim(s: []u8) -> []u8`, returns something that points into its
arguments, never into its own frame. The checker enforces both sides
within one function:

- In the callee, a returned view must come from a parameter, from
  static data such as a string literal, or from another call that
  follows this rule. A view of a local is an error.
- In the caller, the result borrows every argument that can hold or
  own the memory: each argument whose type holds a view or is owning.
  Until the result's last use, those places cannot be passed `inout`,
  assigned, taken or freed, and a function that writes them, as a
  global, cannot be called. A borrow of a temporary, as in
  `trim(read_line())`, is an error, since the temporary dies at the end
  of the statement.
- The rule needs no annotation: the source of a result is always all of
  its candidate arguments, as Rust's lifetime elision assumes for one
  input. Code that wants a narrower source passes fewer arguments.

A place returned for writing, so `slot(inout m, k) += 1` works, is
planned on the same rule, after Hylo's subscripts.

### Exclusivity

**Decided.** In one call, an argument passed as `inout` must not overlap any
other argument. The compiler compares access paths at each call site.

```
swap(inout a.x, inout a.y)      // fine, different fields
swap(inout a, inout a)          // error
swap(inout v[i], inout v[j])    // error: i != j is not provable
```

Built-ins such as `swap_at(inout v, i, j)` cover the indexed case. From
Swift's Law of Exclusivity (Swift Evolution proposal SE-0176).

**Decided (2026-11-09): nothing aliases an `inout` argument** (rule 4).
An argument rooted at a global cannot be passed `inout`, or as a `let`
reference, to a function that writes that global, which the compiler
knows from the inferred write sets (see
[Mutable globals](#mutable-globals)). Views cannot be stored (rule 3),
so a view that reaches the same value is another argument of the same
call, which the path check above already covers. Only a raw pointer
dereference inside `unsafe` can still alias, and that is the trusted
boundary.

So every `inout` parameter is unaliased, and keel can mark it
`noalias`. This replaces the decision of 2026-11-08 that made such
aliasing defined behavior, and the copy a `let` argument rooted at a
global needed.

### Views

**Decided.** Slices are a built-in view type, `[]T`. Text is `[]u8`. A
view is a pointer and a length into memory owned by something else.

**Decided (2026-11-09): a view is a local, a parameter or a result**
(rule 3). It cannot be a field of a declared type, an enum payload, a
`[..]T` element or a `var` global. A named tuple can hold views, and is
then bound by the same rule, so a function can still return
`(head: []u8, tail: []u8)`. A `let` global can be a view of static
data, such as a string literal, since that memory never goes away.

```
fn trim(s: []u8) -> []u8 { ... }
fn find(haystack: []u8, needle: []u8) -> ?[]u8 { ... }
fn split(s: []u8, at: usize) -> (head: []u8, tail: []u8) { ... }

let prompt: []u8 = "> "
```

Data structures hold what a view would have pointed at, or the means to
find it again: owned text as `[..]u8`, a span of indexes into a buffer
the structure owns, or a typed index or generational handle. ternc
already works this way: `Span`, `TyId` and `DeclRef` are indexes.

```
pub type Line enum {
    section(name: Span)
    pair(key: Span, value: Span)
}
```

The cost: the ini parser's `pair(key: []u8, value: []u8)` becomes a
pair of spans into the line, or owned copies. The user accepted it on
2026-11-09. The `Format` rule, which keeps a `Format` out of everything
but parameters, is the same kind of check.

Rejected:

- **Views anywhere, unchecked** (2026-11-08 to 2026-11-09): no
  temporal safety.
- **`view type` for types that hold views, with `-> ?str in haystack`**
  (until 2026-11-08): every containing type inherited the restriction.

### Length

**Decided (2026-11-08).** Arrays, views and growable arrays have a
`.len` property:
`buf.len`, `s[1..].len`. Length is part of what these values are, so it
reads as a property, not a call. For `[N]T` it is the constant `N`. The
name `len` is still free for fields of user types.

Library types use functions, as in Java, where an array has `length`
and a list has `size()`.

### Slicing

**Decided (2026-11-08).** `s[a..b]` makes a view of `s` from `a` up to,
not including, `b`. `s[a..]` and `s[..b]` omit an end, which is allowed
only inside brackets. `s[..]` is not allowed; it is just `s`.

### Fixed arrays

**Decided (2026-11-08).** `[N]T` is an array of `N` values of type `T`,
stored inline. It is a value like any other: assigning it copies it.

```
type Mac [6]u8

let primes: [4]u32 = [2, 3, 5, 7]
var table: [256]u8 = fill[256](0)
```

- An array literal `[a, b, c]` takes its type from context, like a number
  literal. It has at least one element.
- Like an `if` expression, an array literal sits at the top of an
  expression only: no `[1, 2][i]`, and `a == ([1, 2])` needs parentheses.
  Found by the grammar on 2026-11-08: as an operand, `[4]*u8` and
  `[2](x)` read as either a literal or an array type in a type argument,
  and an empty `[]` reads as the start of a view type.
- A repeated value uses the built-in `fill[N](value)`. Rust's `[0; 256]`
  would put a `;` inside brackets, which reads badly next to the semicolon
  rule.

### Function values

**Decided (2026-11-08).** A function type lists parameter modes and
types, without names: `fn(inout []i32, u32) -> bool`. A function is
passed by its name.

```
type Less fn(i32, i32) -> bool

fn sort(v: inout []i32, less: fn(i32, i32) -> bool) { ... }

sort(inout v, by_size)
```

- A function value is a code address and captures nothing, so it is safe
  to store in fields and variables. Only nested functions, which are
  deferred, would capture and be second-class.
- Like a result type, a function type sits only at the top of a type:
  `fn() -> u8 ! E` returns a result. A view of function values needs a
  named type.

### Generational handles

**Leaning.** Graphs, parent links and object pools use an index plus a
generation number into an arena. A stale handle fails a check instead of
corrupting memory, at the cost of one compare per access. Arenas can use
static memory, which suits embedded targets.

Sources: Vale's generational references, the index-into-arena pattern in
ECS game engines and in Zig and Odin code, and Rust's `generational-arena`
crate.

### Raw pointers

**Decided.** `*T` exists for MMIO, DMA, page tables, allocators, C
interop and intrusive lists. Creating, storing and comparing a raw pointer
is safe. Dereferencing one requires `unsafe`. Volatile loads and stores are
intrinsics on raw pointers, not a separate pointer type. From Rust's raw
pointers and Zig's pointers.

**Decided (2026-11-08), syntax from Zig:**

```
var x: u32 = 0
let p: *u32 = &x
let uart: *u32 = from_address[*u32](0x4000_0000)
unsafe {
    p.* = 5
    h.*.next.*.len = 0
}
```

- `&x` makes a `*T`. `p.*` dereferences; it is postfix, so it chains left
  to right. `.*` is one token, so `p.*= 1` cannot lex as `.` and `*=`.
- A `*T` is never null. `?*T` is the nullable pointer, the same size as
  `*T`, and null checks go through `match`.
- An integer address becomes a pointer through the built-in
  `from_address[*T](n)`. It is safe, since creating a pointer is safe
  (decided 2026-11-08); only the dereference needs `unsafe`. Offsets use `offset(p, n)`; `+` never applies to
  pointers.

### `unsafe`

**Decided (2026-11-08).**

- `unsafe { ... }` is a block where unchecked operations are allowed.
  Like `if`, it also has an expression form holding one expression:
  `let v: u32 = unsafe { p.* }`.
- An `unsafe fn` has preconditions the compiler cannot check, and can be
  called only inside `unsafe`.
- A lint requires a `// SAFETY:` comment above each `unsafe` block, so the
  reviewer reads why it is sound. From Rust's lints.

**Decided (2026-11-09): `unsafe` is the trusted boundary** (rule 5). It
marks raw pointer dereferences, union reads, C calls and other
operations the checker cannot see through. Code outside it is free of
memory errors, given that each `unsafe` block and `unsafe fn` keeps the
promise its `// SAFETY:` comment states. A verifier assumes those
promises as contracts, as SPARK assumes code outside SPARK.

Untagged unions, attributes and `extern` functions are in
[C interop and layout](#c-interop-and-layout).

The user asked whether a systems language needs storable references.
Data structures store indexes and owned values instead (see
[Views](#views)), and raw pointers cover the rest. Rust's safe
references stored in structs (`&'a T`) need lifetime parameters,
variance and region inference across functions, which this design
leaves out.

### Mutable globals

**Decided (2026-11-08).** A `var` global can be read and written from any
function in its package, or from anywhere if it is `pub`. No annotation
and no `unsafe`.

```
var counter: u32 = 0

fn bump() {
    counter += 1
}
```

- Global initializers are constant. Nothing runs before `main`, matching
  "no user code at compile time".

**Decided (2026-11-08):** a constant is a literal, a construction, an
operator applied to constants, another `let` global, or one of a closed
list of built-ins: `from_address`, `fill`, `c_string`, `truncate`, and
`.len` on an array. There are no user constant functions; Go's constant
expressions with `len` and `unsafe.Sizeof` work the same way.

```
let uart: *Regs = from_address[*Regs](0x4000_C000)
let table: [256]u8 = fill[256](0)
let mask: u32 = 1 << 5
```
- A global reachable from an interrupt handler is declared `shared` and
  accessed inside `lock`. See [Concurrency](#concurrency).

**Decided (2026-11-09): the compiler infers which globals each function
writes** (rule 4). Nobody writes the list.

- The write set of a function is the `var` globals it assigns, takes,
  frees or passes `inout`, plus its callees' write sets. The compiler
  computes it bottom up over the call graph, with a fixpoint over each
  group of mutually recursive functions, after type checking.
- **Outside its package, a private global appears as the package's
  state.** If `io.print` writes a private buffer, callers in other
  packages see that it writes `io`, not which global. Changing which
  private globals a function touches changes nothing outside the
  package. A `pub var` global keeps its own name. From SPARK's abstract
  state.
- **A function type writes nothing unless it says so:**
  `fn(u8) uses io`. Passing a function that writes a package's state
  where the type does not allow it is an error, so a call through a
  function value has a known write set, and comparators and similar
  callbacks stay pure.
- **An `extern` function writes "the outside world"**, which no tern
  global belongs to, unless it is given a contract (see
  [Verification](#verification)).
- `ternc` prints each public function's write set, so a reviewer sees
  it without the source, and a change to one across a package boundary
  can be flagged.

Type checking stays local to each function, from signatures alone.
Write-set inference is a separate pass after it; it still runs in
parallel across independent parts of the call graph, but not fully per
function. The user accepted this relaxation of goal 2 on 2026-11-09.

Rejected on 2026-11-08 and replaced: written `uses` lists, where each
function named the globals it and its callees read or wrote. They
propagated up every caller, as Java's checked exceptions do, and leaked
private globals into other packages' signatures. Inference with package
state removes both costs.

Also rejected earlier: Swift's dynamic exclusivity checks for globals,
and requiring `unsafe` for every access to a global.

### Allocation

**Decided (2026-11-08).** No global allocator. Code that creates an
allocating value is given an `Allocator`, and the value stores it.

```
fn names(alloc: Allocator, text: []u8) -> [..][]u8 ! OutOfMemory {
    var out: [..][]u8 = make(alloc)
    for let line: []u8 in lines(text) {
        try push(inout out, line)
    }
    return ok.(value: out)
}

var v: [..][]u8 = try names(heap.c_allocator, text)
defer free(inout v)
```

- A function that creates allocating values takes an `Allocator`, so its
  signature shows that it allocates. A program without a heap never
  creates one. From Zig.
- The container stores its allocator, so `push` does not repeat it. This
  is Zig's "managed" container. The cost: a function that only grows an
  existing container allocates without its signature saying so.
- `Allocator` is a built-in record: a context pointer and function
  pointers. Function values capture nothing, so it needs no closures.
  The standard library provides a C heap allocator and an arena (see
  [Arenas](#arenas)); a fixed-buffer allocator over a `[N]u8` for
  embedded targets is planned.
- Allocating operations return `! OutOfMemory`, as in Zig. Every
  allocation shows as a `try`, and kernels and embedded code can handle
  the failure. Rejected: Rust's `push`, which stops the program instead,
  hiding each allocation point and making the failure fatal.
- Release is explicit, with `free(inout v)` and usually `defer` (see
  [Values and buffers](#values-and-buffers)).

### Arenas

**Decided (2026-11-08).** `std.heap.Arena` is a bump allocator for values
built and freed together, such as a syntax tree.

```
var arena: heap.Arena = heap.new_arena(heap.c_allocator)
defer heap.free_arena(inout arena)
let alloc: Allocator = heap.arena_allocator(&arena)
```

- It takes memory from a parent allocator in chunks, each twice the
  last, from 4 KiB. Freeing or resizing the newest allocation works in
  place; freeing any other does nothing.
- An allocation of 64 KiB or more gets a block of its own from the
  parent, so a large `[..]T` grows through the parent's `resize`
  instead of being copied into the arena each time. Without this,
  ternc's parse of a 32,000-line file used 1.7 times the memory.
- `free_arena` returns every chunk and block; the arena can be used
  again.
- The `Allocator` points at the `Arena`, so the arena must stay where
  it is while the allocator is in use, and outlive every value
  allocated from it. The checker cannot see either, so
  `arena_allocator` and `free_arena` are `unsafe fn` (decided
  2026-11-09, rule 5), and the `// SAFETY:` comment at the call states
  the promise. `c_allocator` lives forever and needs neither.
- A resize that moves copies with C's `memcpy`, so the arena needs
  libc, as `c_allocator` does. A bare-metal arena needs a copy that is
  not C's; that comes with the fixed-buffer allocator.

ternc's driver lexes, parses and prints in one arena. Parsing a
32,000-line file takes the same time as with `c_allocator` (23 ms) and
9% more memory.

### Growable arrays: `[..]T`

**Decided (2026-11-08).** `[..]T` is the growable array. With `[N]T` and
`[]T` it completes the family: fixed, view, growable. The spelling is
Jai's. `[..]u8` is the owned string; there is no separate `String`.

- `make(alloc)` creates an empty one. Like an array literal, it takes its
  type from the binding.
- It has `.len`, and passing it where a `[]T` is expected makes a view.
- It needs no generic type syntax such as `List[T]`, which the grammar
  does not have.
- `pop(inout v)` removes the last element and returns it as a `?T`;
  the caller owns it. `clear(inout v)` frees what the elements own,
  sets the length to 0 and keeps the memory (deep since 2026-11-09).
  Added on 2026-11-08 for the lexer's stack.
- `push` can move the elements. A view of them is a borrow of the
  array, so the checker rejects a `push` while one is in use (rule 2).

### Formatting without a heap

**Decided (2026-11-08).** A string literal with interpolation produces a
built-in `Format` value. It holds the literal parts and the interpolated
values, and never allocates. From Rust's `format_args!`.

```
io.print("x={x}\n")                         // print(f: Format)
let s: [..]u8 = try format(alloc, "x={x}")   // owned text, explicitly
```

- A literal without interpolation is still a `[]u8`.
- `{}` accepts integers, `bool`, `[]u8` and other `Format` values.
- A function takes `f: Format` and writes each part where it wants: a
  file, a buffer, or a UART register on a board with no heap. The UART
  example prints this way.
- Rejected: interpolation that returns an allocated string. Every literal
  would hide an allocation, and targets without a heap could not use it.

**Decided (2026-11-08):** how code uses a `Format`.

- A `Format` can only be a parameter. It cannot be stored in a variable
  or field, or returned, because it points at values in the frame where
  the literal was written. Rust's `fmt::Arguments` has the same limit.
  The check is local. It is the one place where a quiet dangle would be
  easy to write and hard to see in review, so the rule stays although
  temporal safety is otherwise unchecked.
- Code reads one with an iterator. Literal parts come out as views of
  the literal; integers are formatted into a small buffer inside the
  iterator. A `for` loop can use any local state, such as a file
  handle, where a function value could not capture it.

  ```
  pub fn print(f: Format) {
      for let chunk: []u8 in fmt.chunks(f) {
          write_str(chunk)
      }
  }
  ```

- `{}` accepts integers, `bool`, `[]u8`, a `Format` parameter passed on,
  and a small closed set of built-in formatters such as `hex(n, digits)`.
- User types are not interpolated. Code interpolates their fields, or
  writes a function such as `show(p: Point, out: inout [..]u8)`.
  Interpolating user types needs traits or generics.

### No built-in map

**Decided (2026-11-08):** no map in the first version. A map needs
hashing and equality for every key type, which only the compiler can
provide without generics or traits; that is why Go 1.0 built `map` in.
It would also cost a keyword or new type syntax. The cost of waiting:
users cannot write generic types, so a map added later must again be
built in.

### Cleanup: `defer`

**Decided (2026-11-08).** Cleanup is written out with `defer`, as in Go
and Zig. The user chose it over destructors because the release is visible
where the resource is acquired.

```
var f: File = try open(path)
defer close(inout f)
```

Forgetting a `defer` is not an error in the first version. An annotation
or a warning for owning values that are never released may come later.

**Decided (2026-11-08):** `defer` runs at the end of the enclosing
block, as in Zig. Go runs it at function exit, so a `defer` inside a loop
needs a runtime list of pending calls. `defer` takes one call; cleanup
that needs several calls uses several `defer`s.

**Decided (2026-11-08):** `defer` accepts only a call that returns
nothing. A fallible call there would leave two questions the code does
not answer: where its error goes, and which error wins when the block is
already returning one. Zig forbids `try` in `defer` for the same reason,
and Go's silently ignored `defer f.Close()` errors are a known bug
source. Libraries split cleanup instead: `close` returns nothing, and
the part that can fail, such as `flush`, is a separate call made with
`try` before the block ends.

```
var f: File = try open(path)
defer close(inout f)
try write(inout f, data)
try flush(inout f)
```

**Deferred:** an error-only form like Zig's `errdefer`. It helps a
function that builds something in steps and must undo them on failure,
but it is a second cleanup form. It comes back if real code needs it.

Rejected on 2026-11-08:

- **Linear types** (Austral): every owning value must be consumed on every
  path, including each early return through the error operator. That
  makes error propagation verbose.
- **Destructors** (Rust's `Drop`, C++ RAII): the cleanup is not visible in
  the code a reviewer reads.

### Rejected memory models

- **Tracing GC** (Go, Java): needs a runtime and has pauses. Conflicts with
  goal 3.
- **Reference counting** (Swift, Nim ARC/ORC, Koka's Perceus): needs a
  heap, atomic counts across threads, and dropping one value can free an
  unbounded chain. Many embedded and kernel targets have no heap.
- **First-class references with lifetimes** (Rust): the frontend cost flux
  ran into, and the main source of difficulty when reading Rust.
- **Second-class views with `uses` lists** (this design until
  2026-11-08): see [Temporal safety](#temporal-safety-without-lifetimes).
- **Move-only types with ownership tracking** (this design until
  2026-11-08): see [Values and buffers](#values-and-buffers).
- **Unchecked temporal safety with copyable handles**, Zig's model
  (this design from 2026-11-08 to 2026-11-09): a verifier would need a
  heap model, and use after free went unreported.

## Verification

**Decided (2026-11-09).** The user made mathematical verification a
priority. The aim is SPARK's, for a systems language: proofs over
ordinary code, with the language rules doing most of the work.

- **Two levels.** Absence of runtime errors is proved automatically:
  every runtime error in tern is an explicit check (bounds, overflow,
  division, conversions, `unwrap`-like matches), so the proof is that
  no check fails. Functional correctness needs written contracts.
- **Why it is tractable.** Mutable value semantics, second-class
  references and one owner per buffer make each safe function a
  function from values to values, with `inout` parameters as extra
  results. Rust verifiers such as Creusot need prophecy variables for
  `&mut`, because a Rust reference can be stored; a tern reference
  cannot, so a direct translation works. There are no generics,
  closures, exceptions or hidden control flow to model.
- **The trusted base** is `unsafe` code, `extern` functions and
  allocators, each with a contract the proof assumes.
- **Open:** the contract syntax (`requires`, `ensures`, loop
  invariants, ghost values), and whether to translate to Why3, Viper
  or straight to an SMT solver. Rules 1 to 5 of
  [Temporal safety](#temporal-safety-without-lifetimes) come first.

## Runtime

**Decided (2026-11-08).** The runtime is small: a panic path and, on
hosted targets, an entry point. Everything else is library code.

### What a panic reports

The message, the file, line and column, and the enclosing function. The
user proposed reporting where the panic was called from.

```
panic: unknown operator
  at parse_op (calc/parse.tern:42:15)
```

- The location is a constant the compiler fills in at each call site.
  The caller passes nothing and nothing is computed at runtime.
- Traps carry the values: `index 5 out of bounds for length 3`. The
  compiler builds the message as a `Format`, so no heap is needed.
- No backtrace in the first version. It needs unwind tables or frame
  pointers, and can be added on hosted targets without language changes.
- Each panic site stores its file and function name as static data. A
  build setting may later shorten this to an address on small chips; that
  is a toolchain choice.

### After the report

- The program aborts. Nothing unwinds and pending `defer`s do not run,
  as in Zig and Rust's `panic = abort`. Unwinding would give every
  `defer` a second cleanup path and cost code size everywhere.
- On a hosted target the operating system reclaims memory and files, and
  a core dump stays possible.
- A panic inside the panic handler halts at once without printing.

### The panic handler

On a hosted target the report goes to stderr. A bare-metal program says
where it goes:

```
@panic_handler
fn on_panic(info: PanicInfo) {
    board.print("panic: {info.message}\r\n")
}
```

- `PanicInfo` is a built-in record:
  `(message: Format, file: []u8, line: u32, column: u32, function: []u8)`.
- `@panic_handler` picks the function the runtime calls. That is a
  linking choice, so it fits the rule that attributes never change
  meaning.
- Hosted targets have a default handler, which a program may replace. A
  bare-metal program must provide one; a missing handler is a link error.
- If the handler returns, the runtime halts: an abort on hosted targets,
  a loop on bare metal. The language needs no "never returns" type.

### Entry point

- Hosted: `fn main() -> u8`, whose result is the exit status. It is the
  only form: no `main()` without a result, and no `main() -> ! E`, since
  printing an arbitrary error type needs formatting the language lacks.
- Bare metal: the language defines no entry point. Startup code calls
  what it exports, such as `@export("main")`. The reset handler, clearing
  `.bss` and the vector table are board code; `extern let` covers the
  linker symbols they need.
- Arguments and environment are library functions: `os.args() -> [][]u8`
  and `os.env(name) -> ?[]u8`. They return views of memory the operating
  system set up, which lives as long as the program.

## Standard library

**Decided (2026-11-08).** The standard library is written in tern, in
`toolchain/std`, and calls the C library through `extern fn` on hosted
targets. The first packages cover what the examples use:

| Package | Holds |
| --- | --- |
| `std.c` | C's integer types: `c.int`, `c.size_t`, ... |
| `std.libc` | `extern` declarations of the C functions std calls |
| `std.heap` | `c_allocator`, over `malloc`, `realloc` and `free`; `Arena` |
| `std.fmt` | `Part`, `hex`, and the `chunks` iterator over a `Format` |
| `std.io` | `File`, `open`, `close`, `read_to_end`, `print`, `eprint`, `Error` |
| `std.os` | `args` and `env` |
| `std.rt` | the C entry point and the default panic handler |

A `std` package imports only other `std` packages.

**Leaning (2026-11-08):** writing std filled these gaps. Each is a
proposal for the user to confirm.

- **`Allocator`'s fields.** A context pointer and three functions, with
  sizes and alignments in bytes:

  ```
  (
      context: ?*u8,
      alloc: fn(?*u8, usize, usize) -> ?*u8,               // size, align
      resize: fn(?*u8, *u8, usize, usize, usize) -> ?*u8,  // p, old size, new size, align
      free: fn(?*u8, *u8, usize, usize),                   // p, size, align
  )
  ```

  `none` from `alloc` or `resize` means out of memory. Passing `free` a
  pointer this allocator did not return is undefined, yet the function
  type cannot say so; see the open questions.
- **`Format`'s representation.** The built-in record
  `(parts: []fmt.Part)`. `fmt.Part` is an enum: text, a signed or
  unsigned integer, a `bool`, a hex number, or a nested `Format`. The
  compiler builds the parts on the stack. `{}` also accepts a `fmt.Part`,
  which is how `hex(n, digits)` works: it is an ordinary std function, so
  the "closed set of built-in formatters" is the functions in `std.fmt`
  that return a `Part`.
- **A plain string literal is a `Format` where one is expected.** Like a
  number literal, a string literal takes its type from context: `[]u8` by
  default, `Format` for a `Format` parameter. Without this,
  `io.eprint("usage: inicheck FILE\n")` would not compile.
- **Two more built-ins.** `from_pointer(p, n)` makes a `[]T` from a `*T`
  and a length, and is unsafe. `append(inout v, items)` adds a view's
  elements to a `[..]T` and returns `! OutOfMemory`; `read_to_end` uses it
  rather than pushing one byte at a time. A view's address is `&v[0]`, so
  no built-in is needed in that direction.
- **The hosted entry point is std code.** `std.rt` exports C's `main`,
  saves the arguments, and calls the program's `main`, which the compiler
  exports as `tern_main`. The compiler links `std.rt` into every hosted
  program. Its `@panic_handler` is the default; a program's own handler
  replaces it.
- **`io.Error` says only which operation failed.** Reading `errno` takes
  `__error()` on macOS and `__errno_location()` on Linux, and the
  language has no way yet to write code for one target.

## Compiler design

**Leaning**, all from the compile-time goal:

- Each function body is checked against signatures only, so bodies check
  in parallel. From Go.
- Imports are explicit and packages cannot form cycles. Each package
  writes export data that importers read instead of source. From Go; flux's
  `.flib` files play the same role, but here no generic bodies go into
  them.
- No user code runs at compile time.
- Without user generics, one source function becomes about one backend
  function. The backend is keel (decided 2026-11-08): stage 1 takes it
  as a git dependency on https://github.com/hxyulin/keel, pinned to a
  commit in `Cargo.lock`, now `b56275c`.
- The safety checks are local: bounds, null, and exclusivity at each
  call site. Nothing crosses function boundaries.

### Stage 1

**Decided (2026-11-08).** The first compiler is written in Rust and lives
in `toolchain/stage1`. Its lexer follows this document: Go's semicolon
rule, nested block comments, and `StrStart`, `StrMid` and `StrEnd` tokens
for interpolation. A line may not end inside an interpolation.

Decided the same day:

- **Checker:** name resolution, then type checking. Types flow from
  context into literals, `none`, `some.(...)`, `make` and array
  literals; an operator takes the type of its other operand. Assigning
  to a `let`, or passing it `inout`, is an error, and so is a call
  statement that ignores a result. The same pass checks `unsafe`,
  `shared` access, exclusivity, the `Format` parameter rule and
  constant initializers.
- **Parser:** hand-written recursive descent, one function per rule of
  the tree-sitter grammar. It prints its tree in tree-sitter's
  S-expression form, so the tests compare it with every corpus tree, and
  every `:error` input must produce a diagnostic. The grammar is the
  specification and the corpus is the parser's test suite.
- **Syntax tree:** expressions, types, statements and patterns live in
  arenas and refer to each other by index.
- **Scope:** the whole language except concurrency and the details of C
  interop, aiming first to compile `examples/ini`.

**Decided (2026-11-08):** details the safety checks had to settle. They
follow the decided rules where those reach, and fill gaps where they do
not. The user agreed to each.

- **Moves.** Removed on 2026-11-08 with move-only types; see
  [Values and buffers](#values-and-buffers). The checker had tracked
  moved locals and fields through each function's control flow, with
  rules for loops, `defer` and `match` payloads.
- **`unsafe`.** The body of an `unsafe fn` is not an `unsafe` block, so
  each unchecked operation inside still sits in one, as std already
  writes it. `volatile_load`, `volatile_store` and `from_pointer` need
  `unsafe`, since they read or write through a pointer. Assigning to a
  union member, and taking the address of a member or a C global with
  `&`, does not read it, so needs no `unsafe`.
- **Exclusivity** compares access paths: the same local or global, then
  fields and constant indexes. Any other argument that overlaps an
  `inout` one is an error, so `add(inout p.x, p.x)` is rejected; write
  `let n: u32 = p.x` first. The user kept this strict form unless real
  code runs into problems with it.
- **Constants** also include function names, whose addresses are fixed,
  so an interrupt table or `heap.c_allocator` is a constant. Cycles
  between `let` globals are not checked yet.
- **`Format`** may be a parameter's whole type and nothing else.
  `PanicInfo` holds one, so the same rule applies to it.
- **Named function types.** A function value takes a named function type
  from context, as a literal takes a newtype, and a value of a named
  function type can be called. Without this, `[2]Handler` could not
  hold functions.

### Code generation

**Decided (2026-11-08).** The user agreed to the plan:

- **One keel module per program.** Every package, std included, is
  lowered into one module. On macOS, keel's code generator hands each
  function's instructions to keel's assembler, and keel's linker links
  the object with the SDK's libSystem, all in memory. On Linux, keel
  writes assembly and `clang` assembles and links it. A hosted build
  always loads `std.rt`, the runtime with C's `main`.
  `keel_ir::merge_modules` can combine several keel modules, which a
  separately built std could use later.
- **Scalars are keel values; everything else is memory.** Integers,
  `bool`, floats, pointers, function values, `?*T` and payload-free
  enums are keel values. Records, tuples, views, arrays, `[..]T`, enums
  with payloads, `?T`, `T ! E` and `Format` live in memory and are
  handled through their address; copying one is a `memcpy`. Every local
  has a stack slot, and keel's `stack_promote` pass moves scalars into
  registers.
- **The C calling convention, with the specified layouts.** A value in
  memory is returned through `sret`; an `inout` argument is a pointer.
  A `let` parameter in memory is a pointer to the caller's value (see
  [One calling convention](#one-calling-convention)); a record an
  `extern` function takes is passed `byval`. The layouts
  stay Leaning: the user asked to change them as development needs.
- **Everything but `@export` functions is local to the program**
  (2026-11-08). The module is the whole program, so keel may inline any
  other function; before this, every function was a public symbol and
  keel's inliner skipped them all.
- **Checks are inline.** Arithmetic uses keel's overflow instructions,
  and indexing, slicing, division, shifts and conversions compare
  first. A failed check calls a cold helper with the values and the
  address of the site's constant `PanicInfo`. There is one helper per
  message (`index 5 out of bounds for length 3`), and one for
  `panic(...)`. The helper fills in the message, calls the
  `@panic_handler` (the program's own, or std.rt's), and calls `abort`.
  The function with the check keeps neither in its frame, so keel can
  still inline it.
- **`[..]T` grows through one helper,** `tern.grow`, which calls the
  array's allocator through its `alloc` or `resize` function value.
  `push` and `append` compare the length with the capacity first and
  call it only when the items do not fit.
- **`ternc build DIR`** writes an executable; `-O` runs keel's full
  pipeline and instruction selection, as `keel compile -O3` does, `-S` writes assembly and `--ir` keel's IR. Extra `.a` and
  `.o` arguments are linked with the program, and `-l NAME` links a
  system library: on macOS keel's linker reads the archive and the SDK's
  `libNAME.tbd`, elsewhere both go to clang. This stands in for the
  build system until there is one.

Not lowered yet: `copy`, destructured globals, and `lock`, which runs
its block without taking anything until a target defines what a lock
does. A float converted to an integer saturates instead of panicking
when it does not fit, and a panic inside the panic handler is not
detected.

### ternc, the compiler in tern

**Decided (2026-11-08).** From here on the compiler is developed in tern,
in `toolchain/ternc`, and stage 1 builds it. Stage 1 stays as the
bootstrap. ternc was built one part at a time: the lexer, the parser,
the checker, and then code generation through keel's C API.

- **Layout.** `main.tern` is the driver. The `syntax` package holds the
  tokens, the lexer, line lookup, the syntax tree and the parser. The
  `sexp` package prints a tree as stage 1's `ternc parse` does.
  `program` loads a program's packages, `resolve` resolves names,
  `check` checks types and the safety rules, and `maps` holds the hash
  maps they use, one per key and value type. `gen` generates code and
  `keel` binds keel's C API.
- **`ternc check DIR --std DIR`** reports what stage 1's `check` does.
  It loads, resolves and checks the whole program in one `heap.Arena`.
  Messages other than the lexer's and parser's are built as text with
  `program.format_text`, since a checker message has too many shapes
  for an enum. ternc takes `--std` always; it has no default location
  for the standard library.
- **Tested against stage 1.** `ternc lex FILE` prints what stage 1's
  `ternc lex FILE` prints, errors included. A stage 1 test builds ternc,
  plain and with `-O`, and requires identical stdout, stderr and exit
  status on every tern file in the repository, every tree-sitter corpus
  file, and the files in `toolchain/stage1/tests/lex`: `malformed.tern`
  triggers every lexer error, and the `utf8-*` files each break UTF-8 a
  different way.
- **The tree is arrays and ranges** (decided 2026-11-08, the layout of
  Zig's syntax tree). Each kind of node lives in an array of the `Ast`
  and is referred to by index, such as `ExprId`. A list of nodes is a
  `List`, a range of one shared `extra: [..]u32` holding item indexes;
  records that appear only in lists, such as `Param` and `Arg`, have
  arrays of their own. The parser gathers a list on a scratch stack and
  copies it into `extra` once it is complete. No node owns memory. The
  driver lexes, parses and prints in one `heap.Arena` and frees it once;
  before the arena, freeing the tree took one `free` per array. Rejected: nodes that own
  `[..]T` lists as stage 1's do, which makes freeing a walk over the
  tree.
- **The checker is tested against stage 1 too.** Every program a
  resolver or checker test writes is also run through both compilers'
  `check`, which must print the same output, and so is every package
  directory in `examples`, ternc, std and the run tests, and the
  loader's fixtures. `toolchain/stage1/tests/check/malformed` triggers
  every checker error the checker's tests do not.
- **The checker's types** are interned as stage 1's are. A tuple's
  fields and a function's parameters live in arrays of their own, and
  interning encodes a type as a list of words. Constant expressions are
  128-bit, as in stage 1, in `check/wide.tern`, since tern has no
  `i128`. On ternc's own source, ternc's `check` built with `-O` takes
  20 ms against stage 1's 12 ms.
- **The parser is tested like the lexer.** `ternc parse FILE` prints
  what stage 1's prints: the S-expression tree, or the diagnostics. The
  test compares the two on every tern file and every tree-sitter corpus
  case, which it writes to its own file.
  `toolchain/stage1/tests/parse/malformed.tern` triggers every parser
  error. The two parsers agree on every input, plain and with `-O`.
- **Diagnostics are enums.** A `Format` cannot be stored, so the lexer
  records a `Problem` with its values, such as
  `bad_digit(digit: u8, base: u32)`, and the driver turns it into text
  when it prints it.
- **Std grows as needed.** The UTF-8 check added `std.utf8`, with
  `valid_up_to`. The package loader added `std.fs`, whose `list` reads a
  directory. It reads macOS's `struct dirent`, so ternc lists
  directories only on macOS until tern has target-specific code (the
  user chose to wait, 2026-11-09). Without errno, ternc reports an
  unreadable root as `cannot read DIR`, with no reason.
- **`ternc build`** (2026-11-09) is a port of stage 1's code generator
  in the `gen` package. It builds the module through keel's C API, which
  the `keel` package binds; its wrappers turn a rejected call into an
  internal-error panic. ternc links `libkeel_ffi.a`, the static library
  Cargo builds next to the tests, since keel-ffi is a dev-dependency of
  stage 1. Signed executables come from the C API's linker on macOS.
- **The IR must match stage 1's byte for byte.** keel numbers values,
  blocks and slots in creation order, so ternc creates everything in
  stage 1's order. To make that order something ternc can reproduce
  through the C API, stage 1 now reserves a function's place when it
  starts lowering it, and lists declarations and data in the order they
  are first referenced. A test compares `build --ir` for every run test,
  the ini example and ternc itself, and builds and runs every run test
  and the ini example with ternc. ternc built by ternc builds a
  byte-identical ternc.
- **Two differences from stage 1 below the IR.** With `-O`, the C API
  optimizes without the host's vector context and selects instructions
  with its own tier, so machine code can differ. ternc finds the macOS
  SDK through `SDKROOT` or the Command Line Tools path and does not run
  `xcrun`.

Friction found while porting the lexer, and what became of it:

- `[..]T` had no `pop` or `clear`: both are now built-ins.
- Freeing a record that holds `[..]T` fields took destructuring every
  field. A field of a local could then be moved out; since the move to
  [Values and buffers](#values-and-buffers), nothing moves and
  `free(inout l.tokens)` simply frees it.
- `offset` could not name a parameter: built-in functions are now names
  only in call position.
- Input was not checked to be UTF-8: both compilers now check it.
- Every `push` can fail, so every lexer function that adds a token
  returns `! OutOfMemory` and each call takes a `try`. The user judged
  this acceptable.

Friction found while porting the parser:

- A function's name cannot name a local anywhere in its package, so
  the parser's helpers took names such as `next_kind` and `subscript`
  to leave `kind` and `index` free for locals in the lexer.
- A `let` parameter of a record type was passed `byval`, so it was
  copied on every call: `next_kind(p: Parser)` copied the 600-byte
  `Parser` twice. Parsing a 32,000-line file took 96 ms against stage
  1's 9 ms. Acted on: such parameters are now pointers, and functions
  are local so keel inlines them; the same file takes 23 ms.
- `==` does not apply to optionals, so comparing the next operator with
  a known one takes a `match`.

Friction found while porting the checker:

- A newtype has no operators, so comparing two `TyId`s takes a helper,
  `same_ty`. This is the open question of opting in to a newtype's
  operators.
- An empty array literal `[]` does not parse, so an empty view comes
  from an empty `[..]T`.
- The strict exclusivity rule rejects `ty(inout r, r.ast.params[i].ty)`,
  since the argument reads through `r`. Each such call copies the item
  into a local first.
- An arm with several patterns cannot bind payloads, so arms that would
  share a body, such as `view(elem)` and `pointer(elem)`, repeat it.
- A function's name blocks that name for locals in the whole package,
  which the checker hit often: `decl`, `shift_amount` and
  `variant_payload` all clashed and were renamed.

Friction found while porting the code generator:

- A union cannot be built, so reading a float's bits casts a pointer to
  it: `cast[*u64](&held).*`.
- The checker rejects extra arguments to a variadic `extern` function,
  so `std.io.create` calls `creat` instead of `open` with a mode.
- `return` cannot appear in a `match` used as an expression, so such
  matches became statements.
- A value-returning helper needs a value before a `match` assigns it.
  `keel.no_value()` gives one without emitting an instruction; a dummy
  `iconst` there shifted every later value number.

## Deferred

### Nested functions

Deferred on 2026-11-08. Candidate: nested functions that can read and
write enclosing locals, and can be passed as arguments but not stored or
returned. They need only a pointer to the enclosing frame (a static link),
with no heap. From Pascal and Ada (downward funargs) and Swift's
non-escaping closures.

```
fn total_size(dir: Dir) -> u64 {
    var sum: u64 = 0
    fn add(f: File) {
        sum = sum + f.size
    }
    walk(dir, add)
    return sum
}
```

Limitation: `break` or `return` inside a nested function cannot leave the
enclosing function, so they suit callbacks (tree walks, comparators) but
not iteration. Kotlin solves this by inlining lambdas, which brings back
the code size cost.

### Dependencies

Deferred on 2026-11-08, with the project file. The earlier leaning: a
project file at the root names the project and its dependencies, and an
import whose first segment names a dependency resolves inside it, as
`import vendor.x.io`. This is Go modules without repeating the module
name in every import.

### Generics and traits

Not in the first version. `[T]` is reserved for type arguments.

**Constraints decided on 2026-11-09,** so the memory model fits them
later:

- **Checked at the definition, not per use.** A generic function is
  checked against its constraints once, and verified once for every
  type. Zig's comptime and C++ templates can only be checked and
  verified per instantiation, which breaks goals 2 and 7.
- **Owning is the first constraint.** Rule 1 splits types into those
  that copy and those that do not, as Swift's `~Copyable` and Rust's
  `Copy` do. A type parameter states which it accepts.
- **Constraints carry contracts.** An interface's operations have
  `requires` and `ensures`, as Dafny's traits and SPARK's generic
  formal parameters do, so a verified `sort[T]` can rely on `less`
  being a strict order.
- **The built-in generic types follow the same rules.** `[..]T`, `?T`
  and `T ! E` behave as user generics will, so they can become library
  types.
- **Implementation is open.** Monomorphizing everything brings back
  the compile time flux ran into; passing operation tables, as Swift
  does, compiles each generic function once. It does not change the
  language.

## Open questions

- Nested functions (see Deferred).
- The language's name.
- Target-specific code. `errno` is the first need: a different C
  function on macOS and Linux. Reading a directory is the second:
  `struct dirent` differs too.
- An opt-in for a newtype's operators.
- Writing through a view. The checker lets code write `v[i]` when the
  view itself is a `var` or an `inout` parameter, which is a guess:
  nothing says whether `[]T` is read-only.
- Unsafe function types. A function value with preconditions, such as an
  allocator's `free`, can be called without `unsafe`. One option is
  `unsafe fn(...)` as a type. The same gap lets code call a C function
  through a function value without `unsafe`.

Raised by the programs in `examples/` on 2026-11-08. They parse, but a
checker would have to settle these. Dropping temporal safety settled
views in enum payloads, `uses` across packages, global string constants
and length. The decisions of the same day settled `str` against `[]u8`,
payload renames, fallible `defer` calls, constant initializers and
import roots.

- **Converting an error needs a statement `match`.** A `match`
  expression arm cannot `return`, and there is no `From`, so passing an
  `io.Error` up as `AppError` takes a whole `match` with a `return` in
  each arm. The user judged this an inconvenience, not a problem, and it
  stays for now.
