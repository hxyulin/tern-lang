//! Errors reported against a source location.

use std::fmt::Write;

use crate::source::{SourceFile, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            message: message.into(),
        }
    }

    /// Renders `path:line:column: error: message`, then the source line with
    /// a caret under the start of the span.
    pub fn render(&self, file: &SourceFile) -> String {
        let pos = file.position(self.span.start);
        let mut out = format!("{}:{pos}: error: {}\n", file.path.display(), self.message);
        let line_text = file.text.lines().nth(pos.line as usize - 1).unwrap_or("");
        let pad: String = line_text
            .bytes()
            .take(pos.column as usize - 1)
            .map(|b| if b == b'\t' { '\t' } else { ' ' })
            .collect();
        let _ = writeln!(out, "    {line_text}\n    {pad}^");
        out
    }
}
