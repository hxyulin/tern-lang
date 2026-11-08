//! Tokens produced by the lexer.

use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Ident,
    /// A lone `_`.
    Underscore,
    /// `@name`; the span includes the `@`.
    Attribute,
    Int,
    Float,
    Char,
    /// A string literal without interpolation: `"text"`.
    Str,
    /// The part of an interpolated string up to its first `{`: `"text{`.
    StrStart,
    /// The part between two interpolations: `}text{`.
    StrMid,
    /// The part after the last interpolation: `}text"`.
    StrEnd,

    // Keywords.
    As,
    Break,
    Continue,
    Defer,
    Else,
    Enum,
    Extern,
    Fn,
    For,
    If,
    Import,
    In,
    Inout,
    Let,
    Lock,
    Match,
    Pub,
    Return,
    Shared,
    Move,
    Sink,
    Try,
    Type,
    Union,
    Unsafe,
    Var,

    // Punctuation.
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    /// Written, or inserted at a line end. An inserted one has an empty span.
    Semicolon,
    Dot,
    DotDot,
    DotDotDot,
    DotStar,
    Arrow,
    Question,
    Bang,
    Tilde,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    AmpAmp,
    PipePipe,
    Eq,
    EqEq,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,

    Eof,
}

impl TokenKind {
    pub fn keyword(text: &str) -> Option<TokenKind> {
        use TokenKind::{
            As, Break, Continue, Defer, Else, Enum, Extern, Fn, For, If, Import, In, Inout, Let,
            Lock, Match, Move, Pub, Return, Shared, Sink, Try, Type, Union, Unsafe, Var,
        };
        Some(match text {
            "as" => As,
            "break" => Break,
            "continue" => Continue,
            "defer" => Defer,
            "else" => Else,
            "enum" => Enum,
            "extern" => Extern,
            "fn" => Fn,
            "for" => For,
            "if" => If,
            "import" => Import,
            "in" => In,
            "inout" => Inout,
            "let" => Let,
            "lock" => Lock,
            "match" => Match,
            "move" => Move,
            "pub" => Pub,
            "return" => Return,
            "shared" => Shared,
            "sink" => Sink,
            "try" => Try,
            "type" => Type,
            "union" => Union,
            "unsafe" => Unsafe,
            "var" => Var,
            _ => return None,
        })
    }

    /// Whether a line end after this token inserts a semicolon. Go's rule.
    pub fn ends_statement(self) -> bool {
        matches!(
            self,
            TokenKind::Ident
                | TokenKind::Int
                | TokenKind::Float
                | TokenKind::Char
                | TokenKind::Str
                | TokenKind::StrEnd
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::DotStar
                | TokenKind::Return
                | TokenKind::Break
                | TokenKind::Continue
        )
    }
}
