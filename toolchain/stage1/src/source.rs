//! Source files, byte spans and line/column lookup.

use std::fmt;
use std::path::PathBuf;

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
