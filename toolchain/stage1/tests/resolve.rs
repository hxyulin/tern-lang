//! Name resolution: each test writes a small program to a temporary
//! directory and checks the exact messages.

use std::fs;
use std::path::Path;

use stage1::{package, resolve};

mod common;

/// Writes `files` (path, text) under a fresh directory named `test` and
/// returns the resolver's messages in order. ternc's `check` must print
/// what stage 1's prints for the same program.
fn messages(test: &str, files: &[(&str, &str)]) -> Vec<String> {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("resolve")
        .join(test);
    let _ = fs::remove_dir_all(&root);
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("can create directory");
        fs::write(path, text).expect("can write file");
    }
    let std_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/std");
    let program = package::load(&root, &std_dir);
    assert!(!program.has_errors(), "{}", program.render_errors());
    let failures = common::check_differences(&root, &std_dir);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    resolve::resolve(&program)
        .diagnostics
        .into_iter()
        .map(|(_, d)| d.message)
        .collect()
}

fn one(test: &str, source: &str) -> Vec<String> {
    messages(test, &[("main.tern", source)])
}

#[test]
fn resolves_a_clean_program() {
    let text = "import util

type Point (x: i32, y: i32)

fn main() -> u8 {
    let p: Point = Point.(x: 1, y: 2)
    var total: i32 = 0
    for let i: i32 in 0..p.x {
        total += i
    }
    return util.small(total)
}
";
    let util = "pub fn small(n: i32) -> u8 {
    return truncate[u8](n)
}
";
    assert_eq!(
        messages("clean", &[("main.tern", text), ("util/util.tern", util)]),
        Vec::<String>::new()
    );
}

#[test]
fn undefined_names() {
    assert_eq!(
        one(
            "undefined",
            "fn f() -> u8 {\n    let x: u8 = x\n    let _: u8 = x\n    return y\n}\n"
        ),
        ["`x` is not defined", "`y` is not defined"]
    );
}

#[test]
fn duplicate_top_level_names_across_files() {
    let got = messages(
        "duplicate",
        &[("a.tern", "fn f() {\n}\n"), ("b.tern", "fn f() {\n}\n")],
    );
    assert_eq!(got.len(), 1);
    assert!(
        got[0].starts_with("`f` is already declared in this package, at "),
        "{got:?}"
    );
}

#[test]
fn locals_do_not_shadow() {
    let text = "import std.io

fn helper() {
}

fn f(a: u8) {
    let b: u8 = a
    if b == 0 {
        let b: u8 = 1
        let helper: u8 = b
        let io: u8 = helper
        let none: u8 = io
        let _: u8 = none
    }
}
";
    let got = one("shadow", text);
    assert_eq!(got.len(), 5, "{got:?}");
    assert_eq!(got[0], "`io` is imported but never used");
    assert_eq!(
        got[1],
        "`b` is already a local, declared on line 7; pick another name"
    );
    assert!(
        got[2].starts_with("`helper` is already declared at "),
        "{got:?}"
    );
    assert_eq!(got[3], "`io` is an imported package; pick another name");
    assert_eq!(got[4], "`none` is a built-in name; pick another name");
}

#[test]
fn top_level_may_reuse_builtin_names() {
    assert_eq!(
        one("builtin", "fn free(n: u8) -> u8 {\n    return n\n}\n"),
        Vec::<String>::new()
    );
}

#[test]
fn unused_names() {
    let text = "import std.io

fn f(a: u8, _: u8) {
    let b: u8 = 1
    match g() {
        some(value: u8) {
        }
        none {
        }
    }
}

fn g() -> ?u8 {
    return none
}
";
    assert_eq!(
        one("unused", text),
        [
            "`io` is imported but never used",
            "`a` is never used; name it `_` to discard it",
            "`b` is never used; name it `_` to discard it",
            "`value` is never used; skip the field with `value: _`",
        ]
    );
}

#[test]
fn package_members() {
    let lib = "pub fn shown() {\n}\n\nfn hidden() {\n}\n";
    let main = "import lib

fn f() {
    lib.shown()
    lib.hidden()
    lib.missing()
    let x: u8 = lib
}
";
    assert_eq!(
        messages("members", &[("main.tern", main), ("lib/lib.tern", lib)]),
        [
            "`hidden` is private to package `lib`; it needs `pub`",
            "package `lib` has no `missing`",
            "`x` is never used; name it `_` to discard it",
            "`lib` is a package; use one of its names, as in `lib.name`",
        ]
    );
}

#[test]
fn type_positions_name_types() {
    let text = "fn g() {
}

let b: u8 = 0

fn f(a: g) -> b {
    return a
}
";
    assert_eq!(
        one("types", text),
        ["`g` is not a type", "`b` is not a type"]
    );
}

#[test]
fn lock_takes_shared_globals() {
    let text = "shared var rx: u32 = 0
var plain: u32 = 0

fn f() {
    lock rx, plain {
    }
}
";
    assert_eq!(
        one("lock", text),
        ["`lock` takes a `shared` global; `plain` is not one"]
    );
}

#[test]
fn pattern_rules() {
    let text = "fn f(x: ?u8) {
    match x {
        some(value: u8), none {
        }
    }
    let (q as quotient: _, r: u8) = divmod()
    let _: u8 = r
}

fn divmod() -> (q: u8, r: u8) {
    return (q: 1, r: 2)
}
";
    let got = one("patterns", text);
    assert_eq!(
        got,
        [
            "an arm with several patterns cannot bind payloads",
            "`value` is never used; skip the field with `value: _`",
            "a skipped field has no name to rename; drop the `as`",
        ]
    );
}

#[test]
fn import_and_declaration_clash() {
    let lib = "pub fn g() {\n}\n";
    let main = "import lib

fn lib() {
}
";
    let got = messages("clash", &[("main.tern", main), ("lib/lib.tern", lib)]);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], "`lib` is imported but never used");
    assert!(
        got[1].starts_with("`lib` is both imported and declared at "),
        "{got:?}"
    );
}

#[test]
fn qualified_names_need_a_package() {
    let text = "type Point (x: i32)

shared var count: u32 = 0

fn f(_: Point.Inner) {
    lock Point.count {
    }
}
";
    assert_eq!(
        one("not_a_package", text),
        ["`Point` is not a package", "`Point` is not a package"]
    );
}
