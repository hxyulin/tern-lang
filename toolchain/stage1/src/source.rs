//! Source files, byte spans and line/column lookup.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;

/// A byte range in one source file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span {
            start: u32::try_from(start).expect("source files are under 4 GiB"),
            end: u32::try_from(end).expect("source files are under 4 GiB"),
        }
    }

    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

pub struct SourceFile {
    pub path: PathBuf,
    pub text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

/// A 1-based line and column. The column counts bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

impl SourceFile {
    pub fn new(path: impl Into<PathBuf>, text: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(u32::try_from(i + 1).expect("source files are under 4 GiB"));
            }
        }
        SourceFile {
            path: path.into(),
            text,
            line_starts,
        }
    }

    pub fn position(&self, offset: u32) -> Position {
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        Position {
            line: u32::try_from(line + 1).expect("fewer than 2^32 lines"),
            column: offset - self.line_starts[line] + 1,
        }
    }

    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.range()]
    }
}

/// Reads a source file. Source is UTF-8: if it is not, the text stops
/// before the first byte that breaks it, and the diagnostic points there.
/// The caller then reports that error alone.
pub fn read(path: &Path) -> std::io::Result<(SourceFile, Option<Diagnostic>)> {
    let bytes = std::fs::read(path)?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) => (SourceFile::new(path, text), None),
        Err(e) => {
            let at = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(at);
            let text = String::from_utf8(bytes).expect("valid up to here");
            let span = Span::new(at, at);
            let error = Diagnostic::error(
                span,
                "the file is not UTF-8 from here on; source files are UTF-8",
            );
            (SourceFile::new(path, text), Some(error))
        }
    })
}
