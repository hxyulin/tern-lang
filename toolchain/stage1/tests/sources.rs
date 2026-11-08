//! Runs the parser over the example programs and the tree-sitter corpus.
//!
//! - Every example must parse.
//! - Every corpus input the grammar accepts must parse to the same tree as
//!   tree-sitter's, ignoring comment nodes.
//! - Every corpus input marked `:error` must produce a diagnostic.

use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use stage1::source::SourceFile;
use stage1::{parser, sexp};

mod common;
use common::corpus_cases;

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

fn render(name: &str, text: &str, diagnostics: &[stage1::diagnostic::Diagnostic]) -> String {
    let file = SourceFile::new(name, text.to_owned());
    diagnostics.iter().map(|d| d.render(&file)).collect()
}

#[test]
fn examples_parse() {
    let mut files = Vec::new();
    tern_files(&repo_root().join("examples"), &mut files);
    assert!(!files.is_empty(), "no example programs found");
    let mut failures = String::new();
    for path in files {
        let text = fs::read_to_string(&path).expect("example is readable");
        let parsed = parser::parse(&text);
        failures += &render(&path.display().to_string(), &text, &parsed.diagnostics);
    }
    assert!(failures.is_empty(), "parse errors:\n{failures}");
}

/// The node names of an S-expression in order, with `(` and `)`. Field
/// labels such as `name:` are dropped, since the order of children already
/// fixes their roles. Comment nodes are dropped too: tree-sitter places
/// comments in the tree, and the parser does not keep them.
fn tree_tokens(tree: &str) -> Vec<String> {
    let spaced = tree.replace('(', " ( ").replace(')', " ) ");
    let raw: Vec<&str> = spaced
        .split_whitespace()
        .filter(|t| !t.ends_with(':'))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == "(" && matches!(raw.get(i + 1), Some(&("line_comment" | "block_comment"))) {
            i += 3;
            continue;
        }
        out.push(raw[i].to_owned());
        i += 1;
    }
    out
}

#[test]
fn corpus() {
    let dir = repo_root().join("tree-sitter/test/corpus");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("corpus directory is readable")
        .map(|e| e.expect("directory entry is readable").path())
        .collect();
    paths.sort();
    let mut accepted = 0;
    let mut rejected = 0;
    let mut failures = String::new();
    for path in paths {
        let text = fs::read_to_string(&path).expect("corpus file is readable");
        let file_name = path.file_name().expect("file has a name").to_string_lossy();
        for case in corpus_cases(&text) {
            let name = format!("{file_name}: {}", case.name);
            let parsed = parser::parse(&case.input);
            if case.rejected {
                rejected += 1;
                if parsed.diagnostics.is_empty() {
                    let _ = writeln!(failures, "{name}: accepted, but the grammar rejects it\n");
                }
                continue;
            }
            accepted += 1;
            if !parsed.diagnostics.is_empty() {
                let _ = writeln!(
                    failures,
                    "{name}: rejected\n{}",
                    render(&name, &case.input, &parsed.diagnostics)
                );
                continue;
            }
            let got = sexp::print(&parsed.ast, &case.input);
            if tree_tokens(&got) != tree_tokens(&case.tree) {
                let _ = writeln!(
                    failures,
                    "{name}: tree differs\nexpected:\n{}\ngot:\n{got}\n",
                    case.tree
                );
            }
        }
    }
    assert!(
        accepted >= 50 && rejected >= 70,
        "found {accepted} accepted and {rejected} rejected cases"
    );
    assert!(failures.is_empty(), "{failures}");
}
