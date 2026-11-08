//! The tern compiler written in tern, `toolchain/ternc`, built by stage 1.
//! For every input, its `lex` and `parse` commands must print exactly what
//! stage 1's do, errors included.
//!
//! The lexer runs on every tern file in the repository and the tree-sitter
//! corpus files. The parser runs on every tern file and every corpus case.
//! Among the tern files, `tests/lex/malformed.tern` and
//! `tests/parse/malformed.tern` trigger every lexer and parser error.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use stage1::build::{Options, build};

mod common;
use common::corpus_cases;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tmp() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// ternc built plain and with `-O`, once for all tests.
fn compilers() -> &'static [(PathBuf, &'static str); 2] {
    static BUILT: OnceLock<[(PathBuf, &'static str); 2]> = OnceLock::new();
    BUILT.get_or_init(|| {
        [(false, "plain"), (true, "-O")].map(|(optimize, label)| {
            let out = tmp().join(if optimize { "ternc-O" } else { "ternc" });
            let options = Options {
                std_dir: repo().join("toolchain/std"),
                optimize,
                assembly_only: false,
                ir_only: false,
            };
            build(&repo().join("toolchain/ternc"), &out, &options)
                .unwrap_or_else(|e| panic!("cannot build ternc ({label}):\n{e}"));
            (out, label)
        })
    })
}

/// Every `.tern` file under `dir`, skipping build output, npm packages and
/// hidden directories such as `.git` and `.direnv`.
fn tern_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("can read directory").flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target"
                && name != "node_modules"
                && !name.to_string_lossy().starts_with('.')
            {
                tern_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "tern") {
            out.push(path);
        }
    }
}

fn corpus_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(repo().join("tree-sitter/test/corpus"))
        .expect("corpus exists")
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

fn run(exe: &Path, command: &str, file: &Path) -> Output {
    Command::new(exe)
        .arg(command)
        .arg(file)
        .output()
        .expect("can run")
}

fn same(a: &Output, b: &Output) -> bool {
    a.stdout == b.stdout && a.stderr == b.stderr && a.status.code() == b.status.code()
}

/// Runs `command` with both compilers on every file and returns the
/// differences.
fn compare(command: &str, files: &[PathBuf]) -> Vec<String> {
    let mut failures = Vec::new();
    for (compiler, label) in compilers() {
        for file in files {
            let want = run(Path::new(env!("CARGO_BIN_EXE_ternc")), command, file);
            let got = run(compiler, command, file);
            if !same(&want, &got) {
                failures.push(format!(
                    "{} ({label}): exit {:?}, want {:?}\n--- stdout\n{}\n--- stderr\n{}\n--- want stderr\n{}",
                    file.display(),
                    got.status.code(),
                    want.status.code(),
                    String::from_utf8_lossy(&got.stdout),
                    String::from_utf8_lossy(&got.stderr),
                    String::from_utf8_lossy(&want.stderr),
                ));
            }
        }
    }
    failures
}

#[test]
fn lexer_matches_stage1() {
    let mut files = Vec::new();
    tern_files(&repo(), &mut files);
    files.extend(corpus_files());
    files.sort();
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("tests/lex/malformed.tern")),
        "the malformed input is missing"
    );
    assert!(files.len() >= 40, "found only {} inputs", files.len());
    let failures = compare("lex", &files);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn parser_matches_stage1() {
    let mut files = Vec::new();
    tern_files(&repo(), &mut files);
    let dir = tmp().join("corpus-cases");
    fs::create_dir_all(&dir).expect("can create the case directory");
    for corpus in corpus_files() {
        let text = fs::read_to_string(&corpus).expect("corpus file is readable");
        let stem = corpus.file_stem().expect("has a name").to_string_lossy();
        for (i, case) in corpus_cases(&text).into_iter().enumerate() {
            let path = dir.join(format!("{stem}-{i}.tern"));
            fs::write(&path, case.input).expect("can write the case");
            files.push(path);
        }
    }
    files.sort();
    assert!(files.len() >= 160, "found only {} inputs", files.len());
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("tests/parse/malformed.tern")),
        "the malformed input is missing"
    );
    let failures = compare("parse", &files);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
