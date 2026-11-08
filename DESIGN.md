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
4. **Memory safe by default.** Hardware access and other unchecked
   operations go in `unsafe` blocks.
5. **One way to do each thing at the syntax level**, with no paradigm
   forced on program structure. The user's first framing was
   "unopinionated, but not many ways to do the same thing".
6. **Easy tooling.** A highlighter only needs the lexer. The parser is
   simple enough to write by hand.

## Not in the first version

OOP, traits, user-defined generics, iterators, nested functions, macros,
and running user code at compile time. Iterators and nested functions have
candidate designs, recorded under [Deferred](#deferred).

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

Planned by earlier decisions: `if`, `for`, `break`, `continue`, `unsafe`.
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

### No implicit numeric conversions

**Leaning.** From Go and Rust.

### Errors as values

**Leaning.** A built-in result type and one propagation operator, no
exceptions. From Rust's `Result` and `?`, Zig's error unions and Go's
error returns. The operator's spelling is open.

### No user generics; built-in generic types

**Leaning.** Users cannot define generic types or functions. The compiler
provides a few: arrays, views, optional and result. Go 1.0 did this with
slices, maps and channels. It removes monomorphization, the main source of
flux's backend cost.

**Decided (2026-11-08):** the language also provides an owned string and a
growable array. Their API depends on the allocator model, which is open,
but it does not block the syntax design. Whether a built-in map exists is
open too.

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
- There is no renaming. To use other names, bind the whole value and read
  its fields: `let d: (q: u32, r: u32) = divmod(17, 5)`, then `d.q`.

Rejected: `let (q: quot, r: rem) = ...` for renaming, because it reuses
`name: Type` with a different meaning.

### The `_` name

**Leaning (2026-11-08).** Outside destructuring, `_` as a binding name
discards the value: `let _: u32 = f()`, or an unused parameter
`fn handler(_: Event)`. A discard still shows its type.

**Open:** whether ignoring a function's result without `let _` is an
error, as in Zig.

### Multiple return values

**Decided.** Through named tuples. Large results are written directly into
the caller's slot (return value optimization), so returning by value costs
the same as an output parameter.

Rejected: an `out` parameter mode (Ada, C#, Hylo's `set`). It would be a
second way to return values.

## Memory model

No garbage collector, because the language targets embedded and kernel
code (goal 3). The model has these layers.

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

**Leaning:** types that own resources are move-only, and copying one is
written out (`copy(a)`), so a reviewer sees every expensive copy.

Sources: Hylo (formerly Val) and Swift structs. Paper: Dimitri Racordon,
Denys Shabalin, Daniel Zheng, Dave Abrahams and Brennan Saeta,
"Implementation Strategies for Mutable Value Semantics", Journal of Object
Technology 21(2), 2022. It expands "Native Implementation of Mutable Value
Semantics" (ICOOOLPS 2021, arXiv 2106.12678). Checked on 2026-11-08
against the Google Research listing and the authors' preprint.

### Second-class references and parameter modes

**Decided.** A safe reference is a way to pass a parameter, not a type. It
cannot be stored in a variable or a field, or returned (except views,
below). It always points into a caller's frame and ends when the call
returns, so it cannot dangle and needs no lifetime annotations.

Parameter modes:

- `let` (the default): read-only. The callee gets a reference or a copy,
  whichever is cheaper.
- `inout`: read and write. The call site writes `inout` too, so a reviewer
  knows a call can change only the arguments marked `inout`.
- `sink`: takes ownership. The caller cannot use the value afterward.

```
fn bump(p: inout Point) {
    p.x = p.x + 1
}

bump(inout a)
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

Because references never alias, the backend can mark every reference
parameter `noalias`.

### Views

**Decided.** Slices are a built-in view type, `[]T`, and `str` is a byte
view. The user called slices "a big issue" for strictly second-class
references: without views, `fn trim(s: str) -> str` is illegal.

Rules:

1. A view can be a local variable, a parameter or a return value. It
   cannot be a struct field or a global.
2. A function that returns a view takes exactly one view or reference
   parameter, and the result borrows from that argument. This is Rust's
   lifetime elision without named lifetimes.

   **Leaning (2026-11-08):** a function with more than one such parameter
   names the one the result borrows from, for example
   `fn find(haystack: str, needle: str) -> str from haystack`. Without the
   annotation, `find` would be illegal. The spelling is open.
3. While a view of `v` is live in a function, the function cannot mutate,
   move or resize `v`.

```
fn trim(s: str) -> str { ... }

var line: String = read_line()
let t: str = trim(line)       // t borrows line
print(t)
line.push('x')                // fine: t is not used again
```

Rule 3 needs a borrow check, but only within one function: compare each
view's live range with later writes to what it borrows. That is the cheap
part of Rust's non-lexical lifetimes. Rust's cost comes from lifetimes in
signatures and structs, which rules 1 and 2 exclude.

**Open:** a `static str` type that points only to static data and can be
stored anywhere, so string literals can go in struct fields.

**Open:** a struct that holds input across calls, such as a parser. It
stores an owned buffer plus offsets, or a raw pointer in `unsafe` code.

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

The user asked whether a systems language needs storable references. The
answer recorded here: safe code covers passing, returning, views and
graphs, and the rest is unsafe in Rust too. What the design leaves out is
Rust's middle tier, safe references stored in structs (`&'a T`), which
requires lifetime parameters, variance and region inference across
functions.

### Mutable globals

**Leaning.** Accessing a mutable global requires `unsafe` or a
critical-section type, because interrupt handlers and threads share them.
From Rust's `static mut` restrictions and embedded Rust's
`critical_section::Mutex`. Moving a value to another thread is safe under
value semantics.

### Cleanup: `defer`

**Decided (2026-11-08).** Cleanup is written out with `defer`, as in Go
and Zig. The user chose it over destructors because the release is visible
where the resource is acquired.

```
var f: File = open(path)?
defer close(f)
```

Forgetting a `defer` is not an error in the first version. An annotation
or a warning for owning values that are never released may come later.

**Leaning:** `defer` runs at the end of the enclosing block, as in Zig.
Go runs it at function exit, so a `defer` inside a loop needs a runtime
list of pending calls.

**Open:** whether errors need a separate form that runs only on an error
exit, like Zig's `errdefer`.

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
  function. keel is the likely backend.
- The safety checks are local: exclusivity at each call site, and view
  liveness within each function. Nothing crosses function boundaries.

## Deferred

### Iterators

Deferred on 2026-11-08. The user preferred Rust's external iterators to
internal ones. Without iterators, `for` works only on built-in arrays,
views and ranges.

Candidate A, external iterators without traits. The compiler knows the
`next` protocol by name, as in Zig. An iterator that holds a view needs a
**view struct**, a struct that may contain views and follows the view
rules itself. This is Rust's `struct Iter<'a>` with one implicit lifetime.

```
view type LineIter (rest: str)

fn lines(s: str) -> LineIter { ... }
fn next(it: inout LineIter) -> ?str { ... }

for line in lines(text) { ... }
```

Limitations of A:

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

Choosing both would give two ways to iterate.

### Nested functions

Deferred on 2026-11-08. Candidate: nested functions that can read and
write enclosing locals, and can be passed as arguments but not stored or
returned. They need only a pointer to the enclosing frame (a static link),
with no heap. From Pascal and Ada (downward funargs) and Swift's
non-escaping closures.

```
fn total_size(dir: let Dir) -> u64 {
    var sum: u64 = 0
    fn add(f: let File) {
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

- `defer` scope, and whether an error-only form is needed.
- Whether ignoring a function result without `let _` is an error.
- Function value syntax: the type of a function and how a function is
  passed. Everything beyond the syntax is deferred.
- Spelling of the view source annotation (`-> str from haystack`).
- String interpolation format specifiers. If `:` starts a specifier, as in
  `"{x:08x}"`, it cannot be an operator inside an interpolation.
- Spelling of the error propagation operator.
- The allocator model, and whether a built-in map exists.
- `static str`, and how long-lived structs refer to input.
- Iterators and nested functions (see Deferred).
- The language's name.
