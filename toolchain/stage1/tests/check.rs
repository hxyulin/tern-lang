//! Type checking: each test writes a small program and checks the exact
//! messages, after name resolution has passed.

use std::fs;
use std::path::Path;

use stage1::{check, package, resolve};

fn messages(test: &str, files: &[(&str, &str)]) -> Vec<String> {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("check")
        .join(test);
    let _ = fs::remove_dir_all(&root);
    for (path, source) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("can create directory");
        fs::write(path, source).expect("can write file");
    }
    let std_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../std");
    let program = package::load(&root, &std_dir);
    assert!(!program.has_errors(), "{}", program.render_errors());
    let names = resolve::resolve(&program);
    let unresolved: Vec<String> = names
        .diagnostics
        .iter()
        .map(|(_, d)| d.message.clone())
        .collect();
    assert!(unresolved.is_empty(), "name errors: {unresolved:?}");
    check::check(&program, &names)
        .diagnostics
        .into_iter()
        .map(|(_, d)| d.message)
        .collect()
}

fn one(test: &str, source: &str) -> Vec<String> {
    messages(test, &[("main.tern", source)])
}

fn none() -> Vec<String> {
    Vec::new()
}

#[test]
fn literals_take_their_type_from_context() {
    let source = "type Fd i32

fn f() -> u8 {
    let a: u8 = 255
    let b: u8 = 300
    let c: i8 = -128
    let d: Fd = 3
    let e: u32 = 1 << 5
    let _: bool = a == b && c < 0 && i32.(d) > 0 && e == 32
    return 'a'
}
";
    assert_eq!(one("literals", source), ["`300` does not fit in `u8`"]);
}

#[test]
fn literals_without_context() {
    let source = "fn f() {
    let _: bool = 1 == 2
    let _: ?u8 = g(none)
}

fn g(x: ?u8) -> ?u8 {
    return x
}
";
    assert_eq!(
        one("no_context", source),
        ["the type of this integer literal is unknown; convert it, as in `u32.(1)`"]
    );
}

#[test]
fn mismatches_and_conversions() {
    let source = "type Celsius f64

fn f(small: u8, t: Celsius) -> u32 {
    let _: u32 = small
    let _: f64 = f64.(t) + 1.5
    let _: Celsius = t + t
    return u32.(small)
}
";
    assert_eq!(
        one("mismatch", source),
        [
            "expected `u32`, found `u8`",
            "`+` does not apply to `Celsius`",
        ]
    );
}

#[test]
fn mutability() {
    let source = "fn bump(x: inout u32) {
    x += 1
}

fn f(a: u32) {
    let b: u32 = a
    var c: u32 = a
    b = 2
    c = b
    bump(inout c)
    bump(inout b)
    bump(c)
}
";
    assert_eq!(
        one("mutability", source),
        [
            "cannot assign to `b`: it is not a `var` or an `inout` parameter",
            "cannot pass `inout` `b`: it is not a `var` or an `inout` parameter",
            "this parameter is `inout`; write `inout` before the argument",
        ]
    );
}

#[test]
fn results_and_returns() {
    let source = "type E enum {
    bad
}

type Other enum {
    worse
}

fn value() -> u8 {
    return 1
}

fn fails() -> u8 ! E {
    return err.(error: E.bad)
}

fn other() -> ! Other {
    return ok
}

fn no_end() -> u8 {
    value()
}

fn no_ok() -> ! E {
    let _: u8 = try fails()
    try other()
}

fn nothing() {
    defer value()
}
";
    assert_eq!(
        one("results", source),
        [
            "this function can reach its end without returning a `u8`",
            "the `u8` this returns is ignored; write `let _: u8 = ...` to discard it",
            "this function can reach its end without `return ok`",
            "`try` passes up a `Other` error, but this function returns `! E`",
            "`defer` takes a call that returns nothing; this one returns `u8`",
        ]
    );
}

#[test]
fn match_rules() {
    let source = "type Shape enum {
    circle(radius: f32)
    square(side: f32)
    empty
}

type Op enum u8 {
    nop = 0
    halt = 1
    _
}

fn area(s: Shape) -> f32 {
    return match s {
        circle(radius: f32) { radius * radius }
        square(side: u32) { f32.(side) }
    }
}

fn run(op: Op, s: Shape) {
    match op {
        nop {
        }
        halt {
        }
    }
    match s {
        empty, empty {
        }
        triangle {
        }
        _ {
        }
    }
}
";
    assert_eq!(
        one("match", source),
        [
            "this match does not cover `empty`",
            "field `side` is `f32`, not `u32`",
            "a match on the open enum `Op` needs a `_` arm",
            "`empty` is already matched",
            "`triangle` is not a variant of `Shape`",
        ]
    );
}

#[test]
fn privacy_across_packages() {
    let lib = "pub type Buffer (data: u32, pub len: u32)

pub fn make_buffer() -> Buffer {
    return Buffer.(data: 0, len: 0)
}
";
    let main = "import lib

fn f() -> u32 {
    let b: lib.Buffer = lib.make_buffer()
    let _: lib.Buffer = lib.Buffer.(data: 1, len: 1)
    return b.len + b.data
}
";
    assert_eq!(
        messages("privacy", &[("main.tern", main), ("lib/lib.tern", lib)]),
        [
            "`lib.Buffer` cannot be built outside its package: field `data` is private",
            "field `data` of `lib.Buffer` is private to its package",
        ]
    );
}

#[test]
fn loops_and_iterators() {
    let source = "type Counter (n: u32)

fn next(it: inout Counter) -> ?u32 {
    if it.n == 0 {
        return none
    }
    it.n -= 1
    return some.(value: it.n)
}

type Plain (n: u32)

fn f(xs: []u8) {
    for let x: u32 in Counter.(n: 3) {
        let _: u32 = x
    }
    for let b: u32 in xs {
        let _: u32 = b
    }
    for let p: u32 in Plain.(n: 1) {
        let _: u32 = p
    }
    break
}
";
    assert_eq!(
        one("loops", source),
        [
            "the loop yields `u8`, not `u32`",
            "`Plain` cannot be iterated: its package has no `fn next(it: inout Plain) -> ?T`",
            "`break` is only allowed inside a loop",
        ]
    );
}

#[test]
fn strings_and_formats() {
    let source = "type Point (x: i32, y: i32)

fn show(text: Format) {
    pass_on(text)
}

fn pass_on(_: Format) {
}

fn f(p: Point, n: u32) {
    show(\"plain\")
    show(\"n={n}\")
    let _: []u8 = \"n={n}\"
    show(\"p={p}\")
}
";
    assert_eq!(
        one("strings", source),
        [
            "expected `[]u8`, found `Format`",
            "`Point` cannot be interpolated; `{}` takes integers, `bool`, `[]u8`, a `Format` and `fmt.Part`",
        ]
    );
}

#[test]
fn arrays_and_builtins() {
    let source = "fn f(alloc: Allocator) -> ! OutOfMemory {
    let a: [4]u8 = [1, 2, 3]
    let b: [4]u8 = fill[3](0)
    var v: [..]u8 = make(alloc)
    try push(inout v, a[0])
    try append(inout v, b[1..])
    let w: u8 = truncate[u8](v.len)
    let _: u8 = w
    free(sink v)
    return ok
}
";
    assert_eq!(
        one("arrays", source),
        [
            "expected 4 elements, found 3",
            "expected 4 elements, found 3"
        ]
    );
}

#[test]
fn std_and_examples_check() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let std_dir = manifest.join("../std");
    for example in ["ini", "uart"] {
        let program = package::load(&manifest.join("../../examples").join(example), &std_dir);
        let names = resolve::resolve(&program);
        let checked = check::check(&program, &names);
        let errors: Vec<String> = checked
            .diagnostics
            .iter()
            .map(|(f, d)| d.render(&program.file(*f).source))
            .collect();
        assert_eq!(errors, none(), "{example}");
    }
}

#[test]
fn unsafe_operations() {
    let source = "type Word union (bits: u32, value: f32)

extern fn c_abs(n: i32) -> i32
extern var counter: i32

unsafe fn risky() {
}

fn f(p: *u32, w: Word) {
    var x: Word = w
    x.bits = 1
    let _: *Word = &x
    let _: *i32 = &counter
    let _: u32 = p.*
    let _: u32 = w.bits
    let _: i32 = c_abs(1)
    risky()
    let _: i32 = counter
    let _: u32 = volatile_load(p)
    unsafe {
        p.* = w.bits
        risky()
        counter = c_abs(-1)
    }
    let _: f32 = unsafe { x.value }
}
";
    assert_eq!(
        one("unsafe", source),
        [
            "reading or writing through a pointer with `.*` needs `unsafe`",
            "reading the union member `bits` needs `unsafe`",
            "calling the C function `c_abs` needs `unsafe`",
            "calling the unsafe function `risky` needs `unsafe`",
            "using the C global `counter` needs `unsafe`",
            "`volatile_load` needs `unsafe`",
        ]
    );
}

#[test]
fn shared_globals_need_lock() {
    let source = "shared var rx: u32 = 0
shared var tx: u32 = 0

fn f() -> u32 {
    rx += 1
    lock rx {
        rx += 1
        lock tx {
            tx = rx
        }
    }
    lock rx, rx {
    }
    return tx
}
";
    assert_eq!(
        one("shared", source),
        [
            "`rx` is `shared`: use it inside `lock rx { ... }`",
            "`lock` blocks do not nest; take every global in one `lock a, b`",
            "`rx` is already taken by this `lock`",
            "`tx` is `shared`: use it inside `lock tx { ... }`",
        ]
    );
}

#[test]
fn moves() {
    let source = "type Names (list: [..][]u8, count: u32)

fn take(_: sink [..]u8) {
}

fn read(_: []u8) {
}

fn f(alloc: Allocator, borrowed: [..]u8, edit: inout [..]u8, n: u32) {
    var a: [..]u8 = make(alloc)
    let b: [..]u8 = a
    read(a)
    read(b)
    take(sink b)
    take(sink b)
    let _: [..]u8 = borrowed
    take(sink edit)
    var c: [..]u8 = make(alloc)
    if n > 1 {
        free(sink c)
    }
    read(c)
    var d: [..]u8 = make(alloc)
    free(sink d)
    d = make(alloc)
    read(d)
    let names: Names = Names.(list: make(alloc), count: 0)
    let _: [..][]u8 = names.list
    var e: [..]u8 = make(alloc)
    defer free(sink e)
    read(e)
    take(sink e)
    let m: u32 = n
    take_u32(sink m)
    let _: u32 = m
}

fn take_u32(_: sink u32) {
}

fn in_loops(alloc: Allocator, items: [][..]u8, n: u32) -> [..]u8 {
    var kept: [..]u8 = make(alloc)
    for let i: u32 in 0..n {
        if i == 3 {
            return kept
        }
        take(sink kept)
    }
    var other: [..]u8 = make(alloc)
    for {
        take(sink other)
        other = make(alloc)
        if n > 2 {
            break
        }
    }
    for let item: [..]u8 in items {
        take(sink item)
    }
    for let _: u32 in 0..n {
        var fresh: [..]u8 = make(alloc)
        take(sink fresh)
    }
    return other
}

fn payloads(alloc: Allocator, maybe: ?[..]u8) {
    var mine: ?[..]u8 = some.(value: make(alloc))
    match mine {
        some(value: [..]u8) {
            take(sink value)
        }
        none {
        }
    }
    match mine {
        some(value: [..]u8) {
            read(value)
        }
        none {
        }
    }
    match maybe {
        some(value: [..]u8) {
            take(sink value)
        }
        none {
        }
    }
}
";
    assert_eq!(
        one("moves", source),
        [
            "`a` is used after it was moved on line 11",
            "`b` is used after it was moved on line 14",
            "cannot move `borrowed`: the parameter is borrowed; declare it `sink` to own it",
            "cannot move `edit` out of an `inout` parameter: the caller still owns it",
            "`c` is used after it was moved on line 20",
            "cannot move a `[..][]u8` out of a field, element or pointer; move the whole value, or destructure it",
            "`e` is already passed `sink` by the `defer` on line 30",
            "`m` is used after it was moved on line 34",
            "`kept` is moved inside a loop, so the next pass would use it moved; give it a new value before the pass ends",
            "cannot move `item`: a loop's binding borrows from what the loop goes over",
            "`mine` is used after it was moved on line 71",
            "cannot move `maybe`: the parameter is borrowed; declare it `sink` to own it",
        ]
    );
}

#[test]
fn inout_arguments_do_not_overlap() {
    let source = "import std.heap

type Pair (x: u32, y: u32)

fn swap(a: inout u32, b: inout u32) {
    let t: u32 = a
    a = b
    b = t
}

fn add(to: inout u32, n: u32) {
    to += n
}

fn f(p: inout Pair, v: inout [4]u32, i: usize, j: usize) {
    swap(inout p.x, inout p.y)
    swap(inout p.x, inout p.x)
    swap(inout v[0], inout v[1])
    swap(inout v[i], inout v[j])
    add(inout p.x, p.x)
    add(inout p.x, p.y)
    var items: [..]u32 = make(heap.c_allocator)
    match push(inout items, items[0]) {
        ok {
        }
        err(error: _) {
        }
    }
    free(sink items)
}
";
    assert_eq!(
        one("exclusive", source),
        [
            "`p.x` overlaps `p.x`, which is passed `inout`; an `inout` argument must not overlap another argument",
            "`v[j]` overlaps `v[i]`, which is passed `inout`; an `inout` argument must not overlap another argument",
            "`p.x` overlaps `p.x`, which is passed `inout`; an `inout` argument must not overlap another argument",
            "`items[0]` overlaps `items`, which is passed `inout`; an `inout` argument must not overlap another argument",
        ]
    );
}

#[test]
fn format_is_only_a_parameter() {
    let source = "type Saved (message: Format)

fn keep(f: Format) -> Format {
    return f
}

fn many(_: []Format, _: ?Format) {
}

fn handler(info: PanicInfo, callback: fn(Format)) {
    let _: PanicInfo = info
    callback(info.message)
}
";
    assert_eq!(
        one("format_rule", source),
        [
            "a `Format` can only be a parameter: it points into the frame of the call that made it",
            "a `Format` can only be a parameter: it points into the frame of the call that made it",
            "`[]Format` holds a `Format`, which can only be a parameter: it points into the frame of the call that made it",
            "`?Format` holds a `Format`, which can only be a parameter: it points into the frame of the call that made it",
            "`PanicInfo` holds a `Format`, which can only be a parameter: it points into the frame of the call that made it",
        ]
    );
}

#[test]
fn globals_are_constant() {
    let source = "type Color enum {
    red
    green
}

type Regs (data: u32)

type Handler fn()

fn handler() {
}

fn now() -> u32 {
    return 1
}

let base: usize = 0x4000_0000
let regs: *Regs = from_address[*Regs](base + 0x10)
let table: [4]u8 = fill[4](0)
let mask: u32 = 1 << 5
let both: (on: bool, color: Color) = (on: true, color: Color.green)
let handlers: [2]Handler = [handler, handler]
let name: *u8 = c_string(\"tern\")
let size: usize = table.len
var count: u32 = mask
let start: u32 = now()
let copy: u32 = count
let where: *u32 = &count
";
    assert_eq!(
        one("constants", source),
        [
            "a global's value is computed before the program runs, so this must be a constant",
            "a global's value is computed before the program runs, so this must be a constant",
            "a global's value is computed before the program runs, so this must be a constant",
        ]
    );
}
