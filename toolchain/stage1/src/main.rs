//! `ternc`, the stage 1 tern compiler.

use std::path::Path;
use std::process::ExitCode;

use stage1::diagnostic::Diagnostic;
use stage1::source::SourceFile;
use stage1::token::TokenKind;
use stage1::{check, lexer, package, parser, resolve, sexp};

const USAGE: &str = "usage: ternc lex FILE
       ternc parse FILE
       ternc check DIR [--std DIR]
       ternc build DIR [FILE.a|FILE.o ...] [-l NAME] [-o OUT] [-O] [-S] [--ir]
                   [--std DIR]";

/// The standard library in this repository, for a compiler run from it.
const DEFAULT_STD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../std");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|c| c == "build") {
        return build(&args[1..]);
    }
    let (command, path) = match args.as_slice() {
        [command, root, rest @ ..] if command == "check" => {
            return match rest {
                [] => check(Path::new(root), Path::new(DEFAULT_STD)),
                [flag, std_dir] if flag == "--std" => check(Path::new(root), Path::new(std_dir)),
                _ => usage(),
            };
        }
        [command, path] => (command.as_str(), path),
        _ => return usage(),
    };
    let file = match stage1::source::read(Path::new(path)) {
        Ok((file, Some(error))) => return report(&file, &[error]),
        Ok((file, None)) => file,
        Err(e) => {
            eprintln!("ternc: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match command {
        "lex" => lex(&file),
        "parse" => parse(&file),
        _ => usage(),
    }
}

/// `ternc build DIR`: an executable named after the directory, or `-o`.
fn build(args: &[String]) -> ExitCode {
    let mut root: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut options = stage1::build::Options::new(DEFAULT_STD.into());
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-o" => out = rest.next().map(String::as_str),
            "--std" => {
                if let Some(dir) = rest.next() {
                    options.std_dir = dir.into();
                }
            }
            "-O" => options.optimize = true,
            "-S" => options.assembly_only = true,
            "--ir" => options.ir_only = true,
            "-l" => match rest.next() {
                Some(name) => options.libraries.push(name.clone()),
                None => return usage(),
            },
            _ if arg.starts_with("-l") && arg.len() > 2 => options.libraries.push(arg[2..].into()),
            _ if Path::new(arg)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("a") || e.eq_ignore_ascii_case("o")) =>
            {
                options.link.push(arg.into());
            }
            _ if root.is_none() => root = Some(arg),
            _ => return usage(),
        }
    }
    let Some(root) = root else {
        return usage();
    };
    let root = Path::new(root);
    let default_out = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "a.out".to_owned());
    let default_out = if options.assembly_only {
        format!("{default_out}.s")
    } else {
        default_out
    };
    let out = Path::new(out.unwrap_or(&default_out));
    match stage1::build::build(root, out, &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprint!("{message}");
            if !message.ends_with('\n') {
                eprintln!();
            }
            ExitCode::FAILURE
        }
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

/// Loads the root package and everything it imports, and reports errors.
/// So far that means parse, import, name and type errors.
fn check(root: &Path, std_dir: &Path) -> ExitCode {
    let program = package::load(root, std_dir);
    if program.has_errors() {
        eprint!("{}", program.render_errors());
        return ExitCode::FAILURE;
    }
    let names = resolve::resolve(&program);
    let mut diagnostics = names.diagnostics.clone();
    if diagnostics.is_empty() {
        diagnostics = check::check(&program, &names).diagnostics;
    }
    if !diagnostics.is_empty() {
        for (file, d) in &diagnostics {
            eprint!("{}", d.render(&program.file(*file).source));
        }
        return ExitCode::FAILURE;
    }
    println!(
        "{} packages, {} files",
        program.packages.len(),
        program.files.len()
    );
    ExitCode::SUCCESS
}
