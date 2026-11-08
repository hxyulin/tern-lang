//! Package loading: import resolution, cycles and missing packages.

use std::path::{Path, PathBuf};

use stage1::package::{self, Program};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(root: &str) -> Program {
    package::load(&fixtures().join(root), &fixtures().join("std"))
}

fn messages(program: &Program) -> Vec<String> {
    program
        .diagnostics
        .iter()
        .map(|(_, d)| {
            d.message
                .replace(&fixtures().display().to_string(), "<fixtures>")
        })
        .chain(program.errors.iter().cloned())
        .collect()
}

#[test]
fn loads_root_imports_and_std_once() {
    let program = load("app");
    assert_eq!(messages(&program), Vec::<String>::new());
    let paths: Vec<&str> = program.packages.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, ["", "util", "std.io", "util.text"]);
    assert_eq!(program.files.len(), 4);
}

#[test]
fn reports_import_cycles() {
    assert_eq!(messages(&load("cycle")), ["import cycle: a -> b -> a"]);
}

#[test]
fn reports_bad_imports() {
    assert_eq!(
        messages(&load("bad")),
        [
            "no package `missing`: <fixtures>/bad/missing does not exist",
            "package `empty` has no .tern files in <fixtures>/bad/empty",
            "name a package inside `std`, such as `std.io`",
            "a standard library package imports only `std` packages",
            "`io` is imported twice; rename one with `as`",
            "no package `x.io`: <fixtures>/bad/x/io does not exist",
        ]
    );
}
