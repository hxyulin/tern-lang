//! Turns source text into tokens.
//!
//! The lexer keeps two kinds of state, and never needs the parser:
//!
//! - A stack of open string interpolations. Inside `"a{x}b"` it emits
//!   `StrStart` (`"a{`), the tokens of `x`, then `StrEnd` (`}b"`). Each entry
//!   counts the `{` opened inside that interpolation, so a `}` closes the
//!   interpolation only when the count is zero.
//! - Whether the last token can end a statement. At a line end after such a
//!   token, the lexer inserts a semicolon with an empty span (Go's rule).
//!
//! Errors are collected and lexing continues, so one run reports them all.

use crate::diagnostic::Diagnostic;
use crate::source::Span;
use crate::token::{Token, TokenKind};

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn lex(text: &str) -> Lexed {
    let mut lexer = Lexer {
        text,
        src: text.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
        interpolations: Vec::new(),
    };
    lexer.run();
    Lexed {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

struct Lexer<'src> {
    text: &'src str,
    src: &'src [u8],
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    /// One entry per open interpolation: how many `{` inside it are open.
    interpolations: Vec<u32>,
}

/// What a run of string text started after.
#[derive(Clone, Copy)]
enum Opening {
    /// The opening `"`.
    Quote,
    /// The `}` that closed an interpolation.
    Brace,
}

impl Lexer<'_> {
    fn run(&mut self) {
        loop {
            self.skip_trivia();
            if self.pos >= self.src.len() {
                self.line_end(self.pos);
                self.push(TokenKind::Eof, self.pos);
                return;
            }
            self.token();
        }
    }

    fn peek(&self, ahead: usize) -> u8 {
        self.src.get(self.pos + ahead).copied().unwrap_or(0)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn error(&mut self, start: usize, end: usize, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::error(Span::new(start, end), message));
    }

    fn last_kind(&self) -> Option<TokenKind> {
        self.tokens.last().map(|t| t.kind)
    }

    /// Handles a line end at byte `at`: a newline, a block comment holding
    /// one, or the end of the file.
    fn line_end(&mut self, at: usize) {
        if !self.interpolations.is_empty() {
            self.error(at, at, "the line ends inside a string interpolation");
            self.interpolations.clear();
        }
        if self.last_kind().is_some_and(TokenKind::ends_statement) {
            self.tokens.push(Token {
                kind: TokenKind::Semicolon,
                span: Span::new(at, at),
            });
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            match (self.peek(0), self.peek(1)) {
                (b' ' | b'\t' | b'\r', _) => self.pos += 1,
                (b'\n', _) => {
                    self.line_end(self.pos);
                    self.pos += 1;
                }
                (b'/', b'/') => {
                    while !self.at_end() && self.peek(0) != b'\n' {
                        self.pos += 1;
                    }
                }
                (b'/', b'*') => self.block_comment(),
                _ => return,
            }
        }
    }

    /// Block comments nest: `/* a /* b */ c */` is one comment. A comment
    /// that spans lines counts as a line end, as in Go.
    fn block_comment(&mut self) {
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1u32;
        let mut has_newline = false;
        while depth > 0 {
            match (self.peek(0), self.peek(1)) {
                _ if self.at_end() => {
                    self.error(start, start + 2, "block comment is not closed");
                    break;
                }
                (b'/', b'*') => {
                    depth += 1;
                    self.pos += 2;
                }
                (b'*', b'/') => {
                    depth -= 1;
                    self.pos += 2;
                }
                (b'\n', _) => {
                    has_newline = true;
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }
        if has_newline {
            self.line_end(start);
        }
    }

    fn token(&mut self) {
        let start = self.pos;
        let c = self.peek(0);
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.word(start),
            b'0'..=b'9' => self.number(start),
            b'"' => {
                self.pos += 1;
                self.string_text(start, Opening::Quote);
            }
            b'\'' => self.char_literal(start),
            b'@' => {
                self.pos += 1;
                if self.peek(0).is_ascii_alphabetic() {
                    self.skip_word_chars();
                    self.push(TokenKind::Attribute, start);
                } else {
                    self.error(start, self.pos, "expected an attribute name after `@`");
                }
            }
            b'{' => {
                if let Some(open) = self.interpolations.last_mut() {
                    *open += 1;
                }
                self.pos += 1;
                self.push(TokenKind::LBrace, start);
            }
            b'}' => {
                self.pos += 1;
                match self.interpolations.last_mut() {
                    Some(0) => self.string_text(start, Opening::Brace),
                    Some(open) => {
                        *open -= 1;
                        self.push(TokenKind::RBrace, start);
                    }
                    None => self.push(TokenKind::RBrace, start),
                }
            }
            _ if c.is_ascii() => self.punctuation(start),
            _ => {
                let ch = self.text[start..]
                    .chars()
                    .next()
                    .expect("start is on a char boundary");
                self.pos += ch.len_utf8();
                self.error(
                    start,
                    self.pos,
                    format!(
                        "unexpected character U+{:04X}; code outside strings and comments is ASCII",
                        u32::from(ch)
                    ),
                );
            }
        }
    }

    fn skip_word_chars(&mut self) {
        while matches!(self.peek(0), b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_') {
            self.pos += 1;
        }
    }

    fn word(&mut self, start: usize) {
        self.skip_word_chars();
        let text = &self.text[start..self.pos];
        let kind = if text == "_" {
            TokenKind::Underscore
        } else {
            TokenKind::keyword(text).unwrap_or(TokenKind::Ident)
        };
        self.push(kind, start);
    }

    /// Integers: decimal, `0x`, `0b` and `0o`. Floats: `1.0`, `1.5e-3`. An
    /// `_` may only stand between two digits.
    fn number(&mut self, start: usize) {
        let radix = match (self.peek(0), self.peek(1)) {
            (b'0', b'x') => 16,
            (b'0', b'b') => 2,
            (b'0', b'o') => 8,
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
            let digits_start = self.pos;
            self.skip_word_chars();
            self.check_digits(digits_start, radix);
            self.push(TokenKind::Int, start);
            return;
        }

        let mut kind = TokenKind::Int;
        self.decimal_digits();
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            kind = TokenKind::Float;
            self.pos += 1;
            self.decimal_digits();
            let sign = usize::from(matches!(self.peek(1), b'+' | b'-'));
            if matches!(self.peek(0), b'e' | b'E') && self.peek(1 + sign).is_ascii_digit() {
                self.pos += 1 + sign;
                self.decimal_digits();
            }
        }
        if matches!(self.peek(0), b'A'..=b'Z' | b'a'..=b'z' | b'_') {
            let bad = self.pos;
            self.skip_word_chars();
            self.error(
                bad,
                self.pos,
                "a number literal cannot be followed by a letter or `_`; tern has no type suffixes",
            );
        }
        self.push(kind, start);
    }

    /// Consumes a run of decimal digits and `_`, checking the `_` placement.
    fn decimal_digits(&mut self) {
        let start = self.pos;
        while matches!(self.peek(0), b'0'..=b'9' | b'_') {
            self.pos += 1;
        }
        self.check_underscores(start);
    }

    fn check_digits(&mut self, start: usize, radix: u32) {
        let text = &self.text[start..self.pos];
        if text.is_empty() {
            self.error(start, self.pos, "expected digits after the number prefix");
            return;
        }
        if let Some(bad) = text.chars().find(|&c| c != '_' && !c.is_digit(radix)) {
            self.error(
                start,
                self.pos,
                format!("`{bad}` is not a digit in base {radix}"),
            );
            return;
        }
        self.check_underscores(start);
    }

    fn check_underscores(&mut self, start: usize) {
        let text = &self.src[start..self.pos];
        let misplaced = text.first() == Some(&b'_')
            || text.last() == Some(&b'_')
            || text.windows(2).any(|w| w == b"__");
        if misplaced {
            self.error(
                start,
                self.pos,
                "`_` in a number must stand between two digits",
            );
        }
    }

    /// Lexes string text after `opening`, up to the closing `"` or the next
    /// `{`, and pushes the token for it.
    fn string_text(&mut self, start: usize, opening: Opening) {
        let kind = loop {
            let c = self.peek(0);
            match c {
                _ if self.at_end() || c == b'\n' => {
                    self.error(start, self.pos, "string literal is not closed on this line");
                    break self.close_string(opening);
                }
                b'"' => {
                    self.pos += 1;
                    break self.close_string(opening);
                }
                b'{' => {
                    self.pos += 1;
                    match opening {
                        Opening::Quote => {
                            self.interpolations.push(0);
                            break TokenKind::StrStart;
                        }
                        Opening::Brace => break TokenKind::StrMid,
                    }
                }
                b'\\' => self.escape(),
                0x20..=0x7E => self.pos += 1,
                _ => self.bad_text_char("string literal"),
            }
        };
        self.push(kind, start);
    }

    fn close_string(&mut self, opening: Opening) -> TokenKind {
        match opening {
            Opening::Quote => TokenKind::Str,
            Opening::Brace => {
                self.interpolations.pop();
                TokenKind::StrEnd
            }
        }
    }

    /// Escapes: `\n \t \r \0 \\ \" \' \{` and `\x00` to `\x7F`.
    fn escape(&mut self) {
        let start = self.pos;
        match self.peek(1) {
            b'n' | b't' | b'r' | b'0' | b'\\' | b'"' | b'\'' | b'{' => self.pos += 2,
            b'x' if matches!(self.peek(2), b'0'..=b'7') && self.peek(3).is_ascii_hexdigit() => {
                self.pos += 4;
            }
            b'x' => {
                self.pos += 2;
                self.error(start, self.pos, "`\\x` takes two hex digits from 00 to 7F");
            }
            _ => {
                self.pos += 1;
                self.error(start, self.pos + 1, "unknown escape");
            }
        }
    }

    /// Reports a byte that cannot appear in a literal and skips its char.
    fn bad_text_char(&mut self, what: &str) {
        let start = self.pos;
        let ch = self.text[start..]
            .chars()
            .next()
            .expect("start is on a char boundary");
        self.pos += ch.len_utf8();
        let message = if ch.is_ascii() {
            format!(
                "a {what} holds printable ASCII; write U+{:04X} as an escape",
                u32::from(ch)
            )
        } else {
            format!("a {what} holds ASCII only, found U+{:04X}", u32::from(ch))
        };
        self.error(start, self.pos, message);
    }

    /// `'a'` or `'\n'`: an integer holding one ASCII code.
    fn char_literal(&mut self, start: usize) {
        self.pos += 1;
        match self.peek(0) {
            b'\'' => {
                self.pos += 1;
                self.error(start, self.pos, "empty character literal");
                self.push(TokenKind::Char, start);
                return;
            }
            b'\\' => self.escape(),
            0x20..=0x7E => self.pos += 1,
            b'\n' => {}
            _ if self.at_end() => {}
            _ => self.bad_text_char("character literal"),
        }
        if self.peek(0) == b'\'' {
            self.pos += 1;
        } else {
            // Skip to a closing quote on the same line, so `'ab'` is one error.
            let rest = &self.src[self.pos..];
            let line = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            if let Some(quote) = rest[..line].iter().position(|&b| b == b'\'') {
                self.pos += quote + 1;
            }
            self.error(start, self.pos, "character literal is not closed");
        }
        self.push(TokenKind::Char, start);
    }

    fn punctuation(&mut self, start: usize) {
        use TokenKind as K;
        let (kind, len) = match (self.peek(0), self.peek(1), self.peek(2)) {
            (b'.', b'.', b'.') => (K::DotDotDot, 3),
            (b'<', b'<', b'=') => (K::ShlEq, 3),
            (b'>', b'>', b'=') => (K::ShrEq, 3),
            (b'.', b'.', _) => (K::DotDot, 2),
            (b'.', b'*', _) => (K::DotStar, 2),
            (b'-', b'>', _) => (K::Arrow, 2),
            (b'=', b'=', _) => (K::EqEq, 2),
            (b'!', b'=', _) => (K::BangEq, 2),
            (b'<', b'=', _) => (K::LtEq, 2),
            (b'>', b'=', _) => (K::GtEq, 2),
            (b'<', b'<', _) => (K::Shl, 2),
            (b'>', b'>', _) => (K::Shr, 2),
            (b'&', b'&', _) => (K::AmpAmp, 2),
            (b'|', b'|', _) => (K::PipePipe, 2),
            (b'+', b'=', _) => (K::PlusEq, 2),
            (b'-', b'=', _) => (K::MinusEq, 2),
            (b'*', b'=', _) => (K::StarEq, 2),
            (b'/', b'=', _) => (K::SlashEq, 2),
            (b'%', b'=', _) => (K::PercentEq, 2),
            (b'&', b'=', _) => (K::AmpEq, 2),
            (b'|', b'=', _) => (K::PipeEq, 2),
            (b'^', b'=', _) => (K::CaretEq, 2),
            (b'(', _, _) => (K::LParen, 1),
            (b')', _, _) => (K::RParen, 1),
            (b'[', _, _) => (K::LBracket, 1),
            (b']', _, _) => (K::RBracket, 1),
            (b',', _, _) => (K::Comma, 1),
            (b':', _, _) => (K::Colon, 1),
            (b';', _, _) => (K::Semicolon, 1),
            (b'.', _, _) => (K::Dot, 1),
            (b'?', _, _) => (K::Question, 1),
            (b'!', _, _) => (K::Bang, 1),
            (b'~', _, _) => (K::Tilde, 1),
            (b'+', _, _) => (K::Plus, 1),
            (b'-', _, _) => (K::Minus, 1),
            (b'*', _, _) => (K::Star, 1),
            (b'/', _, _) => (K::Slash, 1),
            (b'%', _, _) => (K::Percent, 1),
            (b'&', _, _) => (K::Amp, 1),
            (b'|', _, _) => (K::Pipe, 1),
            (b'^', _, _) => (K::Caret, 1),
            (b'=', _, _) => (K::Eq, 1),
            (b'<', _, _) => (K::Lt, 1),
            (b'>', _, _) => (K::Gt, 1),
            (c, _, _) => {
                self.pos += 1;
                let message = if c.is_ascii_graphic() {
                    format!("unexpected character `{}`", char::from(c))
                } else {
                    format!("unexpected control character U+{c:04X}")
                };
                self.error(start, self.pos, message);
                return;
            }
        };
        self.pos += len;
        self.push(kind, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind as K;

    fn kinds(text: &str) -> Vec<TokenKind> {
        let lexed = lex(text);
        assert!(lexed.diagnostics.is_empty(), "{:?}", lexed.diagnostics);
        lexed.tokens.iter().map(|t| t.kind).collect()
    }

    fn errors(text: &str) -> Vec<String> {
        lex(text)
            .diagnostics
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn inserts_semicolons_after_line_ending_tokens() {
        assert_eq!(
            kinds("let x: u8 = f()\nreturn\n"),
            [
                K::Let,
                K::Ident,
                K::Colon,
                K::Ident,
                K::Eq,
                K::Ident,
                K::LParen,
                K::RParen,
                K::Semicolon,
                K::Return,
                K::Semicolon,
                K::Eof,
            ]
        );
    }

    #[test]
    fn no_semicolon_after_operator_or_open_brace() {
        assert_eq!(
            kinds("a +\nb {\n}"),
            [
                K::Ident,
                K::Plus,
                K::Ident,
                K::LBrace,
                K::RBrace,
                K::Semicolon,
                K::Eof
            ]
        );
    }

    #[test]
    fn inserted_semicolon_is_empty_and_at_the_newline() {
        let lexed = lex("x\n");
        assert_eq!(lexed.tokens[1].span, Span::new(1, 1));
    }

    #[test]
    fn multiline_block_comment_is_a_line_end() {
        assert_eq!(
            kinds("x /* a\n b */ y /* c */ z"),
            [
                K::Ident,
                K::Semicolon,
                K::Ident,
                K::Ident,
                K::Semicolon,
                K::Eof
            ]
        );
    }

    #[test]
    fn block_comments_nest() {
        assert_eq!(
            kinds("/* a /* b */ c */ x"),
            [K::Ident, K::Semicolon, K::Eof]
        );
        assert_eq!(errors("/* a /* b */"), ["block comment is not closed"]);
    }

    #[test]
    fn interpolation() {
        assert_eq!(
            kinds(r#""a{x}b{f(y)}c""#),
            [
                K::StrStart,
                K::Ident,
                K::StrMid,
                K::Ident,
                K::LParen,
                K::Ident,
                K::RParen,
                K::StrEnd,
                K::Semicolon,
                K::Eof,
            ]
        );
    }

    #[test]
    fn braces_and_strings_inside_interpolation() {
        assert_eq!(
            kinds(r#""{match a { b { "x{c}" } }}""#),
            [
                K::StrStart,
                K::Match,
                K::Ident,
                K::LBrace,
                K::Ident,
                K::LBrace,
                K::StrStart,
                K::Ident,
                K::StrEnd,
                K::RBrace,
                K::RBrace,
                K::StrEnd,
                K::Semicolon,
                K::Eof,
            ]
        );
    }

    #[test]
    fn escaped_brace_is_text() {
        assert_eq!(kinds(r#""\{x}""#), [K::Str, K::Semicolon, K::Eof]);
    }

    #[test]
    fn string_errors() {
        assert_eq!(
            errors("\"abc\n"),
            ["string literal is not closed on this line"]
        );
        assert_eq!(errors(r#""\q""#), ["unknown escape"]);
        assert_eq!(
            errors(r#""\x80""#),
            ["`\\x` takes two hex digits from 00 to 7F"]
        );
        assert_eq!(
            errors("\"a\tb\""),
            ["a string literal holds printable ASCII; write U+0009 as an escape"]
        );
        assert_eq!(
            errors("\"é\""),
            ["a string literal holds ASCII only, found U+00E9"]
        );
        assert_eq!(
            errors("\"{x\n"),
            ["the line ends inside a string interpolation",]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("1_000 0xFF_FF 0b1010 0o17 1.5 1.5e-3 2.0E10"),
            [
                K::Int,
                K::Int,
                K::Int,
                K::Int,
                K::Float,
                K::Float,
                K::Float,
                K::Semicolon,
                K::Eof
            ]
        );
    }

    #[test]
    fn range_after_integer_is_not_a_float() {
        assert_eq!(
            kinds("0..n"),
            [K::Int, K::DotDot, K::Ident, K::Semicolon, K::Eof]
        );
    }

    #[test]
    fn number_errors() {
        assert_eq!(
            errors("1u8"),
            ["a number literal cannot be followed by a letter or `_`; tern has no type suffixes"]
        );
        assert_eq!(errors("0x"), ["expected digits after the number prefix"]);
        assert_eq!(errors("0b102"), ["`2` is not a digit in base 2"]);
        assert_eq!(
            errors("1__0"),
            ["`_` in a number must stand between two digits"]
        );
        assert_eq!(
            errors("0x_FF"),
            ["`_` in a number must stand between two digits"]
        );
    }

    #[test]
    fn char_literals() {
        assert_eq!(
            kinds(r"'a' '\n' '\x7F' '\''"),
            [K::Char, K::Char, K::Char, K::Char, K::Semicolon, K::Eof]
        );
        assert_eq!(errors("''"), ["empty character literal"]);
        assert_eq!(errors("'ab'"), ["character literal is not closed"]);
    }

    #[test]
    fn words() {
        assert_eq!(
            kinds("_ _x fn inout @packed"),
            [
                K::Underscore,
                K::Ident,
                K::Fn,
                K::Inout,
                K::Attribute,
                K::Eof
            ]
        );
    }

    #[test]
    fn longest_punctuation_wins() {
        assert_eq!(
            kinds("a.* <<= ... -> .. >>"),
            [
                K::Ident,
                K::DotStar,
                K::ShlEq,
                K::DotDotDot,
                K::Arrow,
                K::DotDot,
                K::Shr,
                K::Eof
            ]
        );
    }

    #[test]
    fn non_ascii_outside_strings() {
        assert_eq!(
            errors("let é"),
            ["unexpected character U+00E9; code outside strings and comments is ASCII"]
        );
        assert_eq!(errors("// é\n/* ü */"), Vec::<String>::new());
    }
}
