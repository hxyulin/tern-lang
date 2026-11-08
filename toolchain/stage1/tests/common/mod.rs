//! Helpers shared by the integration tests. Each test binary uses only
//! some of them.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use stage1::build::{Options, build};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// ternc, the compiler in tern, built plain and with `-O`, once per test
/// binary. Each binary builds its own copy, so binaries can run at once.
pub fn ternc_compilers() -> &'static [(PathBuf, &'static str); 2] {
    static BUILT: OnceLock<[(PathBuf, &'static str); 2]> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("ternc-builds")
            .join(env!("CARGO_CRATE_NAME"));
        std::fs::create_dir_all(&dir).expect("can create the build directory");
        [(false, "plain"), (true, "-O")].map(|(optimize, label)| {
            let out = dir.join(if optimize { "ternc-O" } else { "ternc" });
            let options = Options {
                optimize,
                link: vec![keel_archive()],
                ..Options::new(repo().join("toolchain/std"))
            };
            build(&repo().join("toolchain/ternc"), &out, &options)
                .unwrap_or_else(|e| panic!("cannot build ternc ({label}):\n{e}"));
            (out, label)
        })
    })
}

/// keel's C API as a static library, which ternc links. Cargo builds it
/// next to the test binaries because keel-ffi is a dev-dependency; after a
/// keel update the newest one is current.
pub fn keel_archive() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary has a path");
    let deps = exe.parent().expect("a test binary has a directory");
    std::fs::read_dir(deps)
        .expect("can read the deps directory")
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("libkeel_ffi-") && name.ends_with(".a")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
        .expect("cargo builds libkeel_ffi.a for the tests")
}

pub fn run<S: AsRef<OsStr>>(exe: &Path, args: &[S]) -> Output {
    Command::new(exe).args(args).output().expect("can run")
}

pub fn same(a: &Output, b: &Output) -> bool {
    a.stdout == b.stdout && a.stderr == b.stderr && a.status.code() == b.status.code()
}

/// Runs stage 1's ternc and both builds of ternc with `args`, and
/// returns a description of each build whose output differs.
pub fn differences<S: AsRef<OsStr>>(stage1: &Path, args: &[S]) -> Vec<String> {
    let want = run(stage1, args);
    let shown: Vec<String> = args
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect();
    let mut failures = Vec::new();
    for (compiler, label) in ternc_compilers() {
        let got = run(compiler, args);
        if !same(&want, &got) {
            failures.push(format!(
                "ternc {} ({label}): exit {:?}, want {:?}\n--- stdout\n{}\n--- want stdout\n{}\n--- stderr\n{}\n--- want stderr\n{}",
                shown.join(" "),
                got.status.code(),
                want.status.code(),
                String::from_utf8_lossy(&got.stdout),
                String::from_utf8_lossy(&want.stdout),
                String::from_utf8_lossy(&got.stderr),
                String::from_utf8_lossy(&want.stderr),
            ));
        }
    }
    failures
}

/// One test case from a tree-sitter corpus file.
pub struct Case {
    pub name: String,
    pub rejected: bool,
    pub input: String,
    pub tree: String,
}

/// Splits a corpus file into cases. A header is a name between two `===`
/// lines, with `:error` marking input the grammar must reject. The input
/// runs to a `---` line, and the expected tree to the next header.
pub fn corpus_cases(text: &str) -> Vec<Case> {
    let lines: Vec<&str> = text.lines().collect();
    let is_header = |l: &str| l.starts_with("===");
    let mut cases = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_header(lines[i]) {
            i += 1;
            continue;
        }
        let header_end = i
            + 1
            + lines[i + 1..]
                .iter()
                .position(|l| is_header(l))
                .expect("header is closed");
        let header = &lines[i + 1..header_end];
        let body_end = header_end
            + 1
            + lines[header_end + 1..]
                .iter()
                .position(|l| l.starts_with("---"))
                .expect("input ends with a --- line");
        let tree_end = lines[body_end + 1..]
            .iter()
            .position(|l| is_header(l))
            .map_or(lines.len(), |p| body_end + 1 + p);
        cases.push(Case {
            name: header[0].to_owned(),
            rejected: header.contains(&":error"),
            input: lines[header_end + 1..body_end].join("\n"),
            tree: lines[body_end + 1..tree_end].join("\n"),
        });
        i = tree_end;
    }
    cases
}

/// The differences between stage 1's `ternc check root --std std_dir` and
/// ternc's.
pub fn check_differences(root: &Path, std_dir: &Path) -> Vec<String> {
    differences(
        Path::new(env!("CARGO_BIN_EXE_ternc")),
        &[
            OsStr::new("check"),
            root.as_os_str(),
            OsStr::new("--std"),
            std_dir.as_os_str(),
        ],
    )
}
