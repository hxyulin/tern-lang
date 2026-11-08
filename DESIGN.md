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
   signatures alone, in parallel. No checks cross function boundaries.
3. **Systems level.** No garbage collector and no required runtime, so
   the language works for embedded targets and OS development.
4. **Spatial memory safety.** Indexing is bounds-checked, there is no
   null, and pointer arithmetic is a built-in. Use after free and
   dangling views are not checked in the first version (see
   [Temporal safety](#temporal-safety-not-checked)). Hardware access and
   other unchecked operations go in `unsafe` blocks.
5. **One way to do each thing at the syntax level**, with no paradigm
   forced on program structure. The user's first framing was
   "unopinionated, but not many ways to do the same thing".
6. **Easy tooling.** A highlighter only needs the lexer. The parser is
   simple enough to write by hand.

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
| `sink` | consuming parameter |
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
name: a field cannot be called `type`, `in` or `sink`.
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

Rejected: `let (q: quot, r: rem) = ...` for renaming, because it reuses
`name: Type` with a different meaning.

### The `_` name

**Decided (2026-11-08).** Outside destructuring, `_` as a binding name
discards the value: `let _: u32 = f()`, or an unused parameter
`fn handler(_: Event)`. A discard still shows its type.

Ignoring any function result without `let _` is a compile error, as in
Zig. Every dropped error is visible in review.

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
A view shared with another context is unchecked, like every other view
lifetime (see [Temporal safety](#temporal-safety-not-checked)).

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

Not checked, since `uses` lists were dropped on 2026-11-08: that an
interrupt handler reaches only `shared` globals, and that a call inside
`lock` does not reach another `lock`, which could deadlock. Both need to
know every global a callee touches. The author and the reviewer check
them, as in C.

`shared` is a keyword, not an attribute, because it changes which rules
apply; attributes never change what code means.

### Deferred

- **`parallel`**, structured fork-join: `parallel (a: f(x), b: g(y))`,
  checked like one call with several arguments, so branches can borrow
  from the caller. The user judged it too much for the initial design.
- **Threads and channels.** A spawned function would take only `sink`
  parameters and use only `shared` globals. The API needs the allocator
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

**Leaning (2026-11-08): import roots.** A project file at the project's
root names the project and its dependencies. An import path resolves in
one of three places:

- `std.` starts a standard library path. The name `std` is reserved.
- A first segment that names a dependency in the project file starts a
  path inside that dependency: `import vendor.x.io`.
- Anything else is a directory under the project root: `import board`
  is `<root>/board/`.

This is Go modules without repeating the module name in every import.
The project file's format is open.

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

### Temporal safety: not checked

**Decided (2026-11-08).** The first version checks spatial safety only:
bounds, null, and use after `sink`. It does not check that a view or
pointer outlives what it points into. This is where Zig and Odin stand.

The earlier design checked temporal safety without lifetimes: views could
not be stored in fields or globals, a returned view named its source with
`in`, and functions listed their globals with `uses`. The example programs
showed the cost. A parser that returns `pair(key: str, value: str)` needed
a new kind of type, a view type, that every containing type inherits. The
user judged that this is not clearly simpler than Rust's lifetimes, and
asked to drop the requirement from the initial design.

What safe code can now do wrong:

```
var all: List[Line] = ...
push(inout all, try ini.parse_line(text[0..n]))
free(inout text)        // compiles; each Line in all now dangles
```

A reviewer has to catch this, as in C and Zig. The mitigations need no
syntax: a debug allocator that poisons freed memory and detects use after
free, as Zig's does, and sanitizers.

The syntax keeps the door open. A later checker could be a lint over the
same code, so this decision can be reversed without changing programs.

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

The compiler avoids the copies. An assignment from a variable that is not
used again is a move. A large read-only parameter is passed by pointer,
which no code can observe.

**Decided (2026-11-08):** types that own resources are move-only.
`var b: [..]u8 = a` moves if `a` is not used again and is an error
otherwise. Copying is written out (`try copy(alloc, a)`), so a reviewer
sees every expensive copy.

Sources: Hylo (formerly Val) and Swift structs. Paper: Dimitri Racordon,
Denys Shabalin, Daniel Zheng, Dave Abrahams and Brennan Saeta,
"Implementation Strategies for Mutable Value Semantics", Journal of Object
Technology 21(2), 2022. It expands "Native Implementation of Mutable Value
Semantics" (ICOOOLPS 2021, arXiv 2106.12678). Checked on 2026-11-08
against the Google Research listing and the authors' preprint.

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
- `sink`: takes ownership. The caller cannot use the value afterward. The
  call site writes `sink` too (decided 2026-11-08), so a reviewer sees
  where a value stops being usable.

```
fn bump(p: inout Point) {
    p.x += 1
}

bump(inout a)
consume(sink f)
```

Sources: Hylo's `let`, `inout`, `sink` modes; Swift's `inout` with `&` at
the call site; Pascal and Ada parameter modes. On second-class values in
general: Leo Osvald, Grégory Essertel, Xilun Wu, Lilliam I. González
Alayón and Tiark Rompf, "Gentrification Gone too Far? Affordable 2nd-Class
Values for Fun and (Co-)Effect", OOPSLA 2016, pages 234 to 251,
doi:10.1145/2983990.2984009. Checked on 2026-11-08 against the SPLASH 2016
program and researchr.

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

The check stays after temporal safety was dropped, because it is local to
one call. It no longer proves that an `inout` parameter is unaliased: the
callee can reach the same value through a global, a view or a pointer.

**Decided (2026-11-08):** that aliasing is defined behavior. The
backend marks a parameter `noalias` only where it proves it, for example
in a function that touches no globals and no pointers. Fortran's rule,
which makes the aliasing undefined so every `inout` gets `noalias`, would
add a second kind of undefined behavior that nothing in the code shows.
C code mostly runs without `restrict` and does fine. An attribute that
opts one function into `noalias` can come later; it would change speed,
not meaning.

### Views

**Decided.** Slices are a built-in view type, `[]T`. Text is `[]u8`. A view is a pointer and a length into memory owned by something
else.

**Decided (2026-11-08):** views are ordinary values. A view can be a
local, a parameter, a return value, a field, an enum payload or a global.
Nothing checks that it outlives what it points into (see
[Temporal safety](#temporal-safety-not-checked)).

```
pub type Line enum {
    section(name: []u8)
    pair(key: []u8, value: []u8)
}

fn trim(s: []u8) -> []u8 { ... }
fn find(haystack: []u8, needle: []u8) -> ?[]u8 { ... }
```

A string literal is a view of static data, so `let prompt: []u8 = "> "`
is a valid global.

Rejected on 2026-11-08 with temporal safety: views only as locals,
parameters and results; `-> ?str in haystack` to name the source of a
returned view; a borrow check within each function; and `view type` for
types that hold views.

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

**Decided (2026-11-08):** `unsafe` stays after temporal safety was
dropped, with a narrower promise. It marks raw pointer dereferences, union
reads, C calls and other operations that can break spatial safety, the
lines a reviewer should read twice. It does not mean code outside it is
free of memory errors: a view can dangle anywhere.

Untagged unions, attributes and `extern` functions are in
[C interop and layout](#c-interop-and-layout).

The user asked whether a systems language needs storable references.
With temporal safety dropped, views are the storable reference, and raw
pointers cover the rest. Rust's safe references stored in structs
(`&'a T`) need lifetime parameters, variance and region inference across
functions, which this design leaves out.

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

Rejected on 2026-11-08 with temporal safety: `uses` lists, where each
function named the globals it and its callees read, or with `inout`
wrote. They let the compiler check that a callee could not free what the
caller held a view into, and check interrupt handlers from their
signatures. The cost was a list that propagates up every caller, as with
Java's checked exceptions, and it leaked private globals into other
packages' signatures. Without view checks, little of the benefit was
left.

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
defer free(sink v)
```

- A function that creates allocating values takes an `Allocator`, so its
  signature shows that it allocates. A program without a heap never
  creates one. From Zig.
- The container stores its allocator, so `push` does not repeat it. This
  is Zig's "managed" container. The cost: a function that only grows an
  existing container allocates without its signature saying so.
- `Allocator` is a built-in record: a context pointer and function
  pointers. Function values capture nothing, so it needs no closures.
  The standard library provides a C heap allocator, an arena, and a
  fixed-buffer allocator over a `[N]u8` for embedded targets.
- Allocating operations return `! OutOfMemory`, as in Zig. Every
  allocation shows as a `try`, and kernels and embedded code can handle
  the failure. Rejected: Rust's `push`, which stops the program instead,
  hiding each allocation point and making the failure fatal.
- Release is explicit, with `free(sink v)` and usually `defer`.
  Containers are move-only (see
  [Mutable value semantics](#mutable-value-semantics)).

### Growable arrays: `[..]T`

**Decided (2026-11-08).** `[..]T` is the growable array. With `[N]T` and
`[]T` it completes the family: fixed, view, growable. The spelling is
Jai's. `[..]u8` is the owned string; there is no separate `String`.

- `make(alloc)` creates an empty one. Like an array literal, it takes its
  type from the binding.
- It has `.len`, and passing it where a `[]T` is expected makes a view.
- It needs no generic type syntax such as `List[T]`, which the grammar
  does not have.
- `push` can move the elements, which leaves earlier views of them
  dangling. Nothing checks this (see
  [Temporal safety](#temporal-safety-not-checked)); a debug allocator
  catches it.

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
  2026-11-08): see [Temporal safety](#temporal-safety-not-checked).

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
  commit in `Cargo.lock`. The dependency is added with code generation.
- The safety checks are local: bounds, null, use after `sink`, and
  exclusivity at each call site. Nothing crosses function boundaries.

### Stage 1

**Decided (2026-11-08).** The first compiler is written in Rust and lives
in `toolchain/stage1`. Its lexer follows this document: Go's semicolon
rule, nested block comments, and `StrStart`, `StrMid` and `StrEnd` tokens
for interpolation. A line may not end inside an interpolation.

Decided the same day:

- **Parser:** hand-written recursive descent, one function per rule of
  the tree-sitter grammar. It prints its tree in tree-sitter's
  S-expression form, so the tests compare it with every corpus tree, and
  every `:error` input must produce a diagnostic. The grammar is the
  specification and the corpus is the parser's test suite.
- **Syntax tree:** expressions, types, statements and patterns live in
  arenas and refer to each other by index.
- **Scope:** the whole language except concurrency and the details of C
  interop, aiming first to compile `examples/ini`.

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

### Generics and traits

Not in the first version. `[T]` is reserved for type arguments.

## Open questions

- The project file's format.
- Nested functions (see Deferred).
- The language's name.

Raised by the programs in `examples/` on 2026-11-08. They parse, but a
checker would have to settle these. Dropping temporal safety settled
views in enum payloads, `uses` across packages, global string constants
and length. The decisions of the same day settled `str` against `[]u8`,
payload renames, fallible `defer` calls and constant initializers, and
gave import roots a leaning.

- **Converting an error needs a statement `match`.** A `match`
  expression arm cannot `return`, and there is no `From`, so passing an
  `io.Error` up as `AppError` takes a whole `match` with a `return` in
  each arm. The user judged this an inconvenience, not a problem, and it
  stays for now.
