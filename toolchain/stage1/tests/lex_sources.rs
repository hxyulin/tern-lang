//! Lexes every example program and every tree-sitter corpus input that the
//! grammar accepts. None may produce a lexer error.

use std::fs;
use std::path::{Path, PathBuf};

use stage1::lexer;
use stage1::source::SourceFile;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tern_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("examples directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        if path.is_dir() {
            tern_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "tern") {
            out.push(path);
        }
    }
}

fn assert_lexes(name: &str, text: &str) {
    let lexed = lexer::lex(text);
    if !lexed.diagnostics.is_empty() {
        let file = SourceFile::new(name, text.to_owned());
        let report: String = lexed.diagnostics.iter().map(|d| d.render(&file)).collect();
        panic!("lexer errors:\n{report}");
    }
}

#[test]
fn examples_lex() {
    let mut files = Vec::new();
    tern_files(&repo_root().join("examples"), &mut files);
    assert!(!files.is_empty(), "no example programs found");
    for path in files {
        let text = fs::read_to_string(&path).expect("example is readable");
        assert_lexes(&path.display().to_string(), &text);
    }
}

/// One test case from a tree-sitter corpus file.
struct Case {
    name: String,
    rejected: bool,
    input: String,
}

/// Splits a corpus file into cases. A header is a name between two `===`
/// lines, with `:error` marking input the grammar must reject; the input
/// runs to a `---` line.
fn corpus_cases(text: &str) -> Vec<Case> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cases = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("===") {
            i += 1;
            continue;
        }
        let header_end = i
            + 1
            + lines[i + 1..]
                .iter()
                .position(|l| l.starts_with("==="))
                .expect("header is closed");
        let header = &lines[i + 1..header_end];
        let body_end = header_end
            + 1
            + lines[header_end + 1..]
                .iter()
                .position(|l| l.starts_with("---"))
                .expect("input ends with a --- line");
        cases.push(Case {
            name: header[0].to_owned(),
            rejected: header.contains(&":error"),
            input: lines[header_end + 1..body_end].join("\n"),
        });
        i = body_end + 1;
    }
    cases
}

#[test]
fn accepted_corpus_inputs_lex() {
    let dir = repo_root().join("tree-sitter/test/corpus");
    let mut count = 0;
    for entry in fs::read_dir(&dir).expect("corpus directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        let text = fs::read_to_string(&path).expect("corpus file is readable");
        for case in corpus_cases(&text).into_iter().filter(|c| !c.rejected) {
            assert_lexes(&format!("{}: {}", path.display(), case.name), &case.input);
            count += 1;
        }
    }
    assert!(count >= 50, "found only {count} corpus inputs");
}
