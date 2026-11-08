//! `ternc`, the stage 1 tern compiler.

use std::process::ExitCode;

use stage1::diagnostic::Diagnostic;
use stage1::source::SourceFile;
use stage1::token::TokenKind;
use stage1::{lexer, parser, sexp};

const USAGE: &str = "usage: ternc lex FILE
       ternc parse FILE";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, path) = match args.as_slice() {
        [command, path] => (command.as_str(), path),
        _ => return usage(),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("ternc: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let file = SourceFile::new(path, text);
    match command {
        "lex" => lex(&file),
        "parse" => parse(&file),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// Prints one token per line: position, kind, and source text.
fn lex(file: &SourceFile) -> ExitCode {
    let lexed = lexer::lex(&file.text);
    for token in &lexed.tokens {
        let pos = file.position(token.span.start);
        let text = file.slice(token.span);
        if token.kind == TokenKind::Semicolon && text.is_empty() {
            println!("{pos}\tSemicolon\t(inserted)");
        } else {
            println!("{pos}\t{:?}\t{text}", token.kind);
        }
    }
    report(file, &lexed.diagnostics)
}

/// Prints the syntax tree in tree-sitter's S-expression form.
fn parse(file: &SourceFile) -> ExitCode {
    let parsed = parser::parse(&file.text);
    if parsed.diagnostics.is_empty() {
        println!("{}", sexp::print(&parsed.ast, &file.text));
    }
    report(file, &parsed.diagnostics)
}

fn report(file: &SourceFile, diagnostics: &[Diagnostic]) -> ExitCode {
    for d in diagnostics {
        eprint!("{}", d.render(file));
    }
    if diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
