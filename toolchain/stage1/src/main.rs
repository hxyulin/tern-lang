//! `ternc`, the stage 1 tern compiler.

use std::process::ExitCode;

use stage1::lexer;
use stage1::source::SourceFile;
use stage1::token::TokenKind;

const USAGE: &str = "usage: ternc lex FILE";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, path] if command == "lex" => lex(path),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Prints one token per line: position, kind, and source text.
fn lex(path: &str) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("ternc: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let file = SourceFile::new(path, text);
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
    for d in &lexed.diagnostics {
        eprint!("{}", d.render(&file));
    }
    if lexed.diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
