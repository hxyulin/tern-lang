//! Finds and parses every package a program uses, starting from the root.
//!
//! A package is a directory, and its files are the `.tern` files directly
//! inside it. `import std.io` resolves to `<std>/io/`; any other path to a
//! directory under the root, so `import board.uart` is `<root>/board/uart/`.

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::Ast;
use crate::diagnostic::Diagnostic;
use crate::parser;
use crate::source::{SourceFile, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackageId(pub u32);

pub struct File {
    pub source: SourceFile,
    pub ast: Ast,
    pub package: PackageId,
}

pub struct Package {
    /// The dotted import path, such as `std.io`. Empty for the root.
    pub path: String,
    pub dir: PathBuf,
    pub files: Vec<FileId>,
    /// The packages this one imports, each once.
    pub imports: Vec<PackageId>,
}

#[derive(Default)]
pub struct Program {
    pub files: Vec<File>,
    pub packages: Vec<Package>,
    /// Errors in a source file.
    pub diagnostics: Vec<(FileId, Diagnostic)>,
    /// Errors with no source location, such as an unreadable root.
    pub errors: Vec<String>,
}

impl Program {
    pub fn file(&self, id: FileId) -> &File {
        &self.files[id.0 as usize]
    }

    pub fn package(&self, id: PackageId) -> &Package {
        &self.packages[id.0 as usize]
    }

    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty() || !self.errors.is_empty()
    }

    /// Every error, rendered with its source line where it has one.
    pub fn render_errors(&self) -> String {
        let mut out = String::new();
        for error in &self.errors {
            let _ = writeln!(out, "error: {error}");
        }
        for (file, d) in &self.diagnostics {
            out += &d.render(&self.file(*file).source);
        }
        out
    }
}

/// Loads the package in `root` and everything it imports. Standard library
/// packages are looked up in `std_dir`.
pub fn load(root: &Path, std_dir: &Path) -> Program {
    loader(root, std_dir).program
}

/// Loads a program for a hosted target: as [`load`], plus `std.rt`, the
/// runtime with C's entry point, which every hosted program links.
pub fn load_hosted(root: &Path, std_dir: &Path) -> Program {
    let mut loader = loader(root, std_dir);
    if !loader.program.has_errors() && !loader.by_path.contains_key("std.rt") {
        let dir = std_dir.join("rt");
        if let Err(e) = loader.read_package("std.rt".to_owned(), dir.clone()) {
            loader
                .program
                .errors
                .push(format!("cannot read {}: {e}", dir.display()));
        }
    }
    loader.program
}

fn loader(root: &Path, std_dir: &Path) -> Loader {
    let mut loader = Loader {
        root: root.to_owned(),
        std_dir: std_dir.to_owned(),
        program: Program::default(),
        by_path: HashMap::new(),
        loading: Vec::new(),
    };
    match loader.read_package(String::new(), root.to_owned()) {
        Ok(Some(_)) => {}
        Ok(None) => loader
            .program
            .errors
            .push(format!("{} holds no .tern files", root.display())),
        Err(e) => loader
            .program
            .errors
            .push(format!("cannot read {}: {e}", root.display())),
    }
    loader
}

struct Loader {
    root: PathBuf,
    std_dir: PathBuf,
    program: Program,
    by_path: HashMap<String, PackageId>,
    /// The packages being loaded, outermost first, to report cycles.
    loading: Vec<String>,
}

impl Loader {
    /// Parses the package at `dir` and loads its imports. Returns `None` if
    /// the directory holds no `.tern` files.
    fn read_package(&mut self, path: String, dir: PathBuf) -> std::io::Result<Option<PackageId>> {
        let mut file_paths: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "tern"))
            .collect();
        if file_paths.is_empty() {
            return Ok(None);
        }
        file_paths.sort();

        let id = PackageId(u32::try_from(self.program.packages.len()).expect("few packages"));
        self.program.packages.push(Package {
            path: path.clone(),
            dir,
            files: Vec::new(),
            imports: Vec::new(),
        });
        self.by_path.insert(path.clone(), id);
        self.loading.push(path);

        for file_path in file_paths {
            let file = self.parse_file(file_path, id);
            self.program.packages[id.0 as usize].files.push(file);
            self.load_imports(file, id);
        }

        self.loading.pop();
        Ok(Some(id))
    }

    fn parse_file(&mut self, path: PathBuf, package: PackageId) -> FileId {
        let id = FileId(u32::try_from(self.program.files.len()).expect("few files"));
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                self.program
                    .errors
                    .push(format!("cannot read {}: {e}", path.display()));
                String::new()
            }
        };
        let parsed = parser::parse(&text);
        self.program
            .diagnostics
            .extend(parsed.diagnostics.into_iter().map(|d| (id, d)));
        self.program.files.push(File {
            source: SourceFile::new(path, text),
            ast: parsed.ast,
            package,
        });
        id
    }

    fn load_imports(&mut self, file: FileId, package: PackageId) {
        // (dotted path, local name, span of the import)
        let imports: Vec<(Vec<String>, String, Span)> = {
            let f = self.program.file(file);
            f.ast
                .imports
                .iter()
                .map(|import| {
                    let segments: Vec<String> = import
                        .path
                        .iter()
                        .map(|n| f.source.slice(n.span).to_owned())
                        .collect();
                    let local = import.alias.map_or_else(
                        || segments.last().cloned().unwrap_or_default(),
                        |a| f.source.slice(a.span).to_owned(),
                    );
                    (segments, local, import.span)
                })
                .collect()
        };

        let mut local_names: HashMap<String, Span> = HashMap::new();
        for (segments, local, span) in imports {
            if local_names.insert(local.clone(), span).is_some() {
                self.error(
                    file,
                    span,
                    format!("`{local}` is imported twice; rename one with `as`"),
                );
            }
            if let Some(imported) = self.resolve(file, &segments, span)
                && !self.program.packages[package.0 as usize]
                    .imports
                    .contains(&imported)
            {
                self.program.packages[package.0 as usize]
                    .imports
                    .push(imported);
            }
        }
    }

    fn resolve(&mut self, file: FileId, segments: &[String], span: Span) -> Option<PackageId> {
        let path = segments.join(".");
        let importer = &self.program.package(self.program.file(file).package).path;
        if importer.starts_with("std.") && segments[0] != "std" {
            self.error(
                file,
                span,
                "a standard library package imports only `std` packages",
            );
            return None;
        }
        if let Some(pos) = self.loading.iter().position(|p| *p == path) {
            let mut cycle: Vec<&str> = self.loading[pos..].iter().map(String::as_str).collect();
            cycle.push(&path);
            let message = format!("import cycle: {}", cycle.join(" -> "));
            self.error(file, span, message);
            return None;
        }
        if let Some(&id) = self.by_path.get(&path) {
            return Some(id);
        }

        let (base, rest) = if segments[0] == "std" {
            if segments.len() == 1 {
                self.error(file, span, "name a package inside `std`, such as `std.io`");
                return None;
            }
            (self.std_dir.clone(), &segments[1..])
        } else {
            (self.root.clone(), segments)
        };
        let dir = rest.iter().fold(base, |dir, s| dir.join(s));
        match self.read_package(path.clone(), dir.clone()) {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                let message = format!("package `{path}` has no .tern files in {}", dir.display());
                self.error(file, span, message);
                None
            }
            Err(_) => {
                let message = format!("no package `{path}`: {} does not exist", dir.display());
                self.error(file, span, message);
                None
            }
        }
    }

    fn error(&mut self, file: FileId, span: Span, message: impl Into<String>) {
        self.program
            .diagnostics
            .push((file, Diagnostic::error(span, message)));
    }
}
