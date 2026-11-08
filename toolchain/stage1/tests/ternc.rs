//! The tern compiler written in tern, `toolchain/ternc`, built by stage 1.
//! For every input, its `lex` and `parse` commands must print exactly what
//! stage 1's do, errors included.
//!
//! The lexer runs on every tern file in the repository and the tree-sitter
//! corpus files. The parser runs on every tern file and every corpus case.
//! Among the tern files, `tests/lex/malformed.tern` and
//! `tests/parse/malformed.tern` trigger every lexer and parser error.
//!
//! `check` runs on every package directory in `examples`, ternc, std and
//! the run tests, on `tests/check/malformed`, which triggers every checker
//! error the checker's tests do not, and on the loader's fixtures. The
//! resolver's and checker's tests compare it on each program they write.

use std::fs;
use std::path::{Path, PathBuf};

mod common;
use common::{check_differences, corpus_cases, differences, repo};

fn tmp() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn stage1() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_ternc"))
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

/// Runs `command` with both compilers on every file and returns the
/// differences.
fn compare(command: &str, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .flat_map(|file| differences(stage1(), &[command.as_ref(), file.as_os_str()]))
        .collect()
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

/// Every directory under `dir` that holds a `.tern` file, skipping what
/// `tern_files` skips.
fn package_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut files = Vec::new();
    tern_files(dir, &mut files);
    for file in files {
        let parent = file.parent().expect("a file has a directory").to_path_buf();
        if !out.contains(&parent) {
            out.push(parent);
        }
    }
}

#[test]
fn checker_matches_stage1() {
    let std_dir = repo().join("toolchain/std");
    let mut failures = Vec::new();
    for dir in [
        "examples",
        "toolchain/ternc",
        "toolchain/std",
        "toolchain/stage1/tests/run",
        "toolchain/stage1/tests/check",
    ] {
        let mut dirs = Vec::new();
        package_dirs(&repo().join(dir), &mut dirs);
        dirs.sort();
        for d in dirs {
            failures.extend(check_differences(&d, &std_dir));
        }
    }
    let fixtures = repo().join("toolchain/stage1/tests/fixtures");
    let fixture_std = fixtures.join("std");
    for name in ["app", "bad", "cycle"] {
        let d = fixtures.join(name);
        failures.extend(check_differences(&d, &fixture_std));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
