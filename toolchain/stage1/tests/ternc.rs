//! The tern compiler written in tern, `toolchain/ternc`, built by stage 1.
//! Its lexer must print exactly what stage 1's does, errors included, for
//! every tern file in the repository, the tree-sitter corpus and
//! `tests/lex/malformed.tern`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use stage1::build::{Options, build};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.tern` file under `dir`, skipping build output and npm packages.
fn tern_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("can read directory").flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "node_modules" && name != ".git" {
                tern_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "tern") {
            out.push(path);
        }
    }
}

fn inputs() -> Vec<PathBuf> {
    let mut files = Vec::new();
    tern_files(&repo(), &mut files);
    let corpus = repo().join("tree-sitter/test/corpus");
    for entry in fs::read_dir(corpus).expect("corpus exists").flatten() {
        files.push(entry.path());
    }
    files.sort();
    files
}

fn same(a: &Output, b: &Output) -> bool {
    a.stdout == b.stdout && a.stderr == b.stderr && a.status.code() == b.status.code()
}

#[test]
fn lexer_matches_stage1() {
    let mut failures = Vec::new();
    let files = inputs();
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("tests/lex/malformed.tern")),
        "the malformed input is missing"
    );
    for optimize in [false, true] {
        let compiler =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(if optimize { "ternc-O" } else { "ternc" });
        let options = Options {
            std_dir: repo().join("toolchain/std"),
            optimize,
            assembly_only: false,
            ir_only: false,
        };
        build(&repo().join("toolchain/ternc"), &compiler, &options)
            .unwrap_or_else(|e| panic!("cannot build ternc:\n{e}"));
        for file in &files {
            let run = |exe: &Path| {
                Command::new(exe)
                    .arg("lex")
                    .arg(file)
                    .output()
                    .expect("can run")
            };
            let want = run(Path::new(env!("CARGO_BIN_EXE_ternc")));
            let got = run(&compiler);
            if !same(&want, &got) {
                failures.push(format!(
                    "{} ({}): exit {:?}, want {:?}\n--- stderr\n{}",
                    file.display(),
                    if optimize { "-O" } else { "plain" },
                    got.status.code(),
                    want.status.code(),
                    String::from_utf8_lossy(&got.stderr),
                ));
            }
        }
    }
    assert!(files.len() >= 40, "found only {} inputs", files.len());
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
