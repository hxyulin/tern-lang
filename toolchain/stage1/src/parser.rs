//! A recursive descent parser, one function per grammar rule of
//! `tree-sitter/grammar.js`.
//!
//! Every choice is made from the next token, with two exceptions: after
//! `(`, an identifier followed by `:` starts a tuple literal; and in an
//! index, `[e]` is an array literal when `]` follows it and an array type
//! otherwise.
//!
//! On an error, the parser records a diagnostic and skips to the end of the
//! statement or declaration, then continues.

use crate::ast::{
    Arg, Arm, AssignOp, Ast, Attribute, BinaryOp, Binding, Block, Decl, DeclKind, DestructField,
    Else, EnumItem, ExprId, ExprKind, Field, FnDecl, For, ForKind, GlobalName, IfElse, IfStmt,
    Import, Index, Iterable, Local, Mode, Name, OpGroup, Param, PatId, PatternKind, StmtId,
    StmtKind, StrPart, TypeDef, TypeId, TypeKind, UnaryOp,
};
use crate::diagnostic::Diagnostic;
use crate::lexer;
use crate::source::Span;
use crate::token::{Token, TokenKind as K};

pub struct Parsed {
    pub ast: Ast,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes and parses one file. Lexer errors come first in the diagnostics.
pub fn parse(text: &str) -> Parsed {
    let lexed = lexer::lex(text);
    let mut parser = Parser {
        text,
        tokens: lexed.tokens,
        pos: 0,
        ast: Ast::default(),
        diagnostics: Vec::new(),
    };
    parser.file();
    let mut diagnostics = lexed.diagnostics;
    diagnostics.append(&mut parser.diagnostics);
    Parsed {
        ast: parser.ast,
        diagnostics,
    }
}

/// The error was recorded; the caller recovers.
struct Failed;

type P<T> = Result<T, Failed>;

struct Parser<'src> {
    text: &'src str,
    tokens: Vec<Token>,
    pos: usize,
    ast: Ast,
    diagnostics: Vec<Diagnostic>,
}

fn assign_op(kind: K) -> Option<AssignOp> {
    Some(match kind {
        K::Eq => AssignOp::Set,
        K::PlusEq => AssignOp::Add,
        K::MinusEq => AssignOp::Sub,
        K::StarEq => AssignOp::Mul,
        K::SlashEq => AssignOp::Div,
        K::PercentEq => AssignOp::Rem,
        K::AmpEq => AssignOp::And,
        K::PipeEq => AssignOp::Or,
        K::CaretEq => AssignOp::Xor,
        K::ShlEq => AssignOp::Shl,
        K::ShrEq => AssignOp::Shr,
        _ => return None,
    })
}

fn binary_op(kind: K) -> Option<BinaryOp> {
    Some(match kind {
        K::PipePipe => BinaryOp::Or,
        K::AmpAmp => BinaryOp::And,
        K::EqEq => BinaryOp::Eq,
        K::BangEq => BinaryOp::Ne,
        K::Lt => BinaryOp::Lt,
        K::LtEq => BinaryOp::Le,
        K::Gt => BinaryOp::Gt,
        K::GtEq => BinaryOp::Ge,
        K::Plus => BinaryOp::Add,
        K::Minus => BinaryOp::Sub,
        K::Star => BinaryOp::Mul,
        K::Slash => BinaryOp::Div,
        K::Percent => BinaryOp::Rem,
        K::Amp => BinaryOp::BitAnd,
        K::Pipe => BinaryOp::BitOr,
        K::Caret => BinaryOp::BitXor,
        K::Shl => BinaryOp::Shl,
        K::Shr => BinaryOp::Shr,
        _ => return None,
    })
}

impl Parser<'_> {
    // Token access.

    fn peek(&self) -> K {
        self.peek_at(0)
    }

    fn peek_at(&self, ahead: usize) -> K {
        self.tokens.get(self.pos + ahead).map_or(K::Eof, |t| t.kind)
    }

    fn current(&self) -> Token {
        self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn bump(&mut self) -> Token {
        let token = self.current();
        if token.kind != K::Eof {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: K) -> bool {
        if self.peek() == kind {
            self.bump();
            true
        } else {
            false
        }
    }

    /// The end of the last consumed token.
    fn prev_end(&self) -> u32 {
        self.pos
            .checked_sub(1)
            .map_or(0, |i| self.tokens[i].span.end)
    }

    fn span_from(&self, start: u32) -> Span {
        Span {
            start,
            end: self.prev_end().max(start),
        }
    }

    fn start(&self) -> u32 {
        self.current().span.start
    }

    fn describe(&self, token: Token) -> String {
        match token.kind {
            K::Eof => "the end of the file".to_owned(),
            K::Semicolon if token.span.start == token.span.end => "the end of the line".to_owned(),
            _ => format!("`{}`", &self.text[token.span.range()]),
        }
    }

    /// Records an error at the current token. Errors at or before the last
    /// one are dropped, since they usually follow from it.
    fn error_here(&mut self, message: impl Into<String>) {
        let span = self.current().span;
        self.error_at(span, message);
    }

    fn error_at(&mut self, span: Span, message: impl Into<String>) {
        if self
            .diagnostics
            .last()
            .is_some_and(|d| span.start <= d.span.start)
        {
            return;
        }
        self.diagnostics.push(Diagnostic::error(span, message));
    }

    fn fail<T>(&mut self, expected: &str) -> P<T> {
        let found = self.describe(self.current());
        self.error_here(format!("expected {expected}, found {found}"));
        Err(Failed)
    }

    fn expect(&mut self, kind: K, expected: &str) -> P<Token> {
        if self.peek() == kind {
            Ok(self.bump())
        } else {
            self.fail(expected)
        }
    }

    fn name(&mut self) -> P<Name> {
        let token = self.expect(K::Ident, "a name")?;
        Ok(Name { span: token.span })
    }

    /// Skips to the end of the current line item: past a `;`, or up to a
    /// `}` that closes the enclosing braces. Brackets opened while skipping
    /// are skipped whole.
    fn recover(&mut self) {
        let mut depth = 0u32;
        loop {
            match self.peek() {
                K::Eof => return,
                K::Semicolon if depth == 0 => {
                    self.bump();
                    return;
                }
                K::RBrace if depth == 0 => return,
                K::LParen | K::LBracket | K::LBrace | K::StrStart => depth += 1,
                K::RParen | K::RBracket | K::RBrace | K::StrEnd => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
    }

    /// Items between braces, separated by line ends or `;`. The `{` is
    /// consumed already; this consumes the `}`.
    fn lines<T>(&mut self, mut item: impl FnMut(&mut Self) -> P<T>) -> P<Vec<T>> {
        let mut items = Vec::new();
        loop {
            if self.eat(K::RBrace) {
                return Ok(items);
            }
            if self.peek() == K::Eof {
                return self.fail("`}`");
            }
            let Ok(x) = item(self) else {
                self.recover();
                continue;
            };
            items.push(x);
            match self.peek() {
                K::Semicolon => {
                    self.bump();
                }
                K::RBrace => {}
                _ => {
                    let _ = self.fail::<()>("the end of the line or `;`");
                    self.recover();
                }
            }
        }
    }

    /// Items between parentheses, separated by commas, with an optional
    /// trailing comma. The `(` is consumed already; this consumes the `)`.
    fn comma_list<T>(&mut self, mut item: impl FnMut(&mut Self) -> P<T>) -> P<Vec<T>> {
        let mut items = Vec::new();
        loop {
            if self.eat(K::RParen) {
                return Ok(items);
            }
            items.push(item(self)?);
            if self.peek() == K::Semicolon && self.current().span.start == self.current().span.end {
                self.error_here("a list that spans lines ends each line with a comma");
                return Err(Failed);
            }
            if !self.eat(K::Comma) {
                self.expect(K::RParen, "`,` or `)`")?;
                return Ok(items);
            }
        }
    }

    // Declarations.

    fn file(&mut self) {
        while self.peek() == K::Import {
            let Ok(import) = self.import() else {
                self.recover();
                continue;
            };
            self.ast.imports.push(import);
            self.end_top_level_item();
        }
        while self.peek() != K::Eof {
            if self.peek() == K::Import {
                self.error_here("imports come before all declarations");
                self.recover();
                continue;
            }
            let Ok(decl) = self.declaration() else {
                self.recover_top_level();
                continue;
            };
            self.ast.decls.push(decl);
            self.end_top_level_item();
        }
    }

    fn end_top_level_item(&mut self) {
        if !self.eat(K::Semicolon) && self.peek() != K::Eof {
            let _ = self.fail::<()>("the end of the line or `;`");
            self.recover_top_level();
        }
    }

    fn recover_top_level(&mut self) {
        self.recover();
        // A stray `}` at the top level would stop recovery forever.
        while self.eat(K::RBrace) {
            self.recover();
        }
    }

    fn import(&mut self) -> P<Import> {
        let start = self.start();
        self.bump();
        let mut path = vec![self.name()?];
        while self.eat(K::Dot) {
            path.push(self.name()?);
        }
        let alias = if self.eat(K::As) {
            Some(self.name()?)
        } else {
            None
        };
        Ok(Import {
            path,
            alias,
            span: self.span_from(start),
        })
    }

    fn declaration(&mut self) -> P<Decl> {
        let start = self.start();
        let mut attributes = Vec::new();
        while self.peek() == K::Attribute {
            attributes.push(self.attribute()?);
            // An attribute on its own line ends with an inserted `;`.
            self.eat(K::Semicolon);
        }
        let public = self.eat(K::Pub);
        let kind = match self.peek() {
            K::Fn => DeclKind::Fn(self.function(false)?),
            K::Unsafe => {
                self.bump();
                if self.peek() != K::Fn {
                    return self.fail("`fn` after `unsafe`");
                }
                DeclKind::Fn(self.function(true)?)
            }
            K::Extern => {
                self.bump();
                match self.peek() {
                    K::Fn => self.extern_function()?,
                    K::Let | K::Var => {
                        let mutable = self.bump().kind == K::Var;
                        let name = self.name()?;
                        self.expect(K::Colon, "`:`")?;
                        let ty = self.ty()?;
                        DeclKind::ExternVar { mutable, name, ty }
                    }
                    _ => return self.fail("`fn`, `let` or `var` after `extern`"),
                }
            }
            K::Type => self.type_declaration()?,
            K::Let | K::Var => DeclKind::Global {
                shared: false,
                local: self.local()?,
            },
            K::Shared => {
                self.bump();
                if self.peek() != K::Var {
                    return self.fail("`var` after `shared`");
                }
                DeclKind::Global {
                    shared: true,
                    local: self.local()?,
                }
            }
            _ => return self.fail("a declaration"),
        };
        Ok(Decl {
            attributes,
            public,
            kind,
            span: self.span_from(start),
        })
    }

    /// `@name` or `@name(args)`, with the `(` written right after the name.
    fn attribute(&mut self) -> P<Attribute> {
        let name = self.bump().span;
        let mut args = Vec::new();
        if self.peek() == K::LParen && self.current().span.start == name.end {
            self.bump();
            args = self.comma_list(|p| match p.peek() {
                K::Int | K::Str | K::StrStart => p.primary(),
                _ => p.fail("an integer or string literal"),
            })?;
        }
        Ok(Attribute { name, args })
    }

    fn function(&mut self, is_unsafe: bool) -> P<FnDecl> {
        self.bump();
        let name = self.name()?;
        self.expect(K::LParen, "`(`")?;
        let params = self.comma_list(Self::parameter)?;
        let result = self.result_type()?;
        let body = self.block()?;
        Ok(FnDecl {
            is_unsafe,
            name,
            params,
            result,
            body,
        })
    }

    fn result_type(&mut self) -> P<Option<TypeId>> {
        if self.eat(K::Arrow) {
            Ok(Some(self.ty()?))
        } else {
            Ok(None)
        }
    }

    fn extern_function(&mut self) -> P<DeclKind> {
        self.bump();
        let name = self.name()?;
        self.expect(K::LParen, "`(`")?;
        let mut params = Vec::new();
        let mut variadic = false;
        loop {
            if self.eat(K::RParen) {
                break;
            }
            if self.peek() == K::DotDotDot {
                if params.is_empty() {
                    self.error_here("`...` follows at least one parameter");
                    return Err(Failed);
                }
                self.bump();
                variadic = true;
                self.eat(K::Comma);
                self.expect(K::RParen, "`)` after `...`")?;
                break;
            }
            params.push(self.parameter()?);
            if !self.eat(K::Comma) {
                self.expect(K::RParen, "`,` or `)`")?;
                break;
            }
        }
        let result = self.result_type()?;
        Ok(DeclKind::ExternFn {
            name,
            params,
            variadic,
            result,
        })
    }

    fn mode(&mut self) -> Mode {
        if self.eat(K::Inout) {
            Mode::Inout
        } else {
            Mode::Let
        }
    }

    fn parameter(&mut self) -> P<Param> {
        let name = if self.eat(K::Underscore) {
            None
        } else {
            Some(self.name()?)
        };
        self.expect(K::Colon, "`:` and a type")?;
        let mode = self.mode();
        let ty = self.ty()?;
        Ok(Param { name, mode, ty })
    }

    fn type_declaration(&mut self) -> P<DeclKind> {
        self.bump();
        let name = self.name()?;
        let def = match self.peek() {
            K::Enum => self.enum_type()?,
            K::Union => {
                self.bump();
                self.expect(K::LParen, "`(`")?;
                TypeDef::Union(self.fields()?)
            }
            _ => TypeDef::Type(self.ty()?),
        };
        Ok(DeclKind::Type { name, def })
    }

    fn enum_type(&mut self) -> P<TypeDef> {
        self.bump();
        let int_type = if self.peek() == K::Ident {
            Some(self.type_name()?)
        } else {
            None
        };
        self.expect(K::LBrace, "`{`")?;
        let items = self.lines(|p| {
            if p.peek() == K::Underscore {
                return Ok(EnumItem::Open(p.bump().span));
            }
            let name = p.name()?;
            let payload = if p.peek() == K::LParen {
                Some(p.simple_type()?)
            } else {
                None
            };
            let value = if p.eat(K::Eq) {
                Some(p.expression()?)
            } else {
                None
            };
            Ok(EnumItem::Variant {
                name,
                payload,
                value,
            })
        })?;
        Ok(TypeDef::Enum { int_type, items })
    }

    /// Fields of a tuple type, after the `(`. At least one.
    fn fields(&mut self) -> P<Vec<Field>> {
        let fields = self.comma_list(|p| {
            let public = p.eat(K::Pub);
            let name = p.name()?;
            p.expect(K::Colon, "`:` and a type")?;
            let ty = p.ty()?;
            Ok(Field { public, name, ty })
        })?;
        if fields.is_empty() {
            let span = Span {
                start: self.prev_end() - 1,
                end: self.prev_end(),
            };
            self.error_at(span, "a tuple type has at least one field");
            return Err(Failed);
        }
        Ok(fields)
    }

    // Types.

    /// A type: a simple type, a result type or a function type.
    fn ty(&mut self) -> P<TypeId> {
        let start = self.start();
        match self.peek() {
            K::Fn => {
                self.bump();
                self.expect(K::LParen, "`(`")?;
                let params = self.comma_list(|p| {
                    let mode = p.mode();
                    Ok((mode, p.ty()?))
                })?;
                let result = self.result_type()?;
                Ok(self
                    .ast
                    .add_type(TypeKind::Fn { params, result }, self.span_from(start)))
            }
            K::Bang => {
                self.bump();
                let error = self.simple_type()?;
                Ok(self.ast.add_type(
                    TypeKind::Result { value: None, error },
                    self.span_from(start),
                ))
            }
            _ => {
                let value = self.simple_type()?;
                if self.eat(K::Bang) {
                    let error = self.simple_type()?;
                    Ok(self.ast.add_type(
                        TypeKind::Result {
                            value: Some(value),
                            error,
                        },
                        self.span_from(start),
                    ))
                } else {
                    Ok(value)
                }
            }
        }
    }

    fn type_name(&mut self) -> P<TypeId> {
        let start = self.start();
        let first = self.name()?;
        let (package, name) = if self.eat(K::Dot) {
            (Some(first), self.name()?)
        } else {
            (None, first)
        };
        Ok(self
            .ast
            .add_type(TypeKind::Name { package, name }, self.span_from(start)))
    }

    fn simple_type(&mut self) -> P<TypeId> {
        let start = self.start();
        let kind = match self.peek() {
            K::Ident => return self.type_name(),
            K::LParen => {
                self.bump();
                TypeKind::Tuple(self.fields()?)
            }
            K::Star => {
                self.bump();
                TypeKind::Pointer(self.simple_type()?)
            }
            K::Question => {
                self.bump();
                TypeKind::Optional(self.simple_type()?)
            }
            K::LBracket => {
                self.bump();
                match self.peek() {
                    K::RBracket => {
                        self.bump();
                        TypeKind::View(self.simple_type()?)
                    }
                    K::DotDot => {
                        self.bump();
                        self.expect(K::RBracket, "`]` after `[..`")?;
                        TypeKind::Growable(self.simple_type()?)
                    }
                    _ => {
                        let len = self.expression()?;
                        self.expect(K::RBracket, "`]`")?;
                        let elem = self.simple_type()?;
                        TypeKind::Array { len, elem }
                    }
                }
            }
            _ => return self.fail("a type"),
        };
        Ok(self.ast.add_type(kind, self.span_from(start)))
    }

    // Bindings.

    /// `let` or `var`, a binding, `=` and a value.
    fn local(&mut self) -> P<Local> {
        let mutable = self.bump().kind == K::Var;
        let binding = self.binding()?;
        if self.peek() != K::Eq {
            return self.fail("`=` and a value; every binding is initialized");
        }
        self.bump();
        let value = self.expression()?;
        Ok(Local {
            mutable,
            binding,
            value,
        })
    }

    fn binding(&mut self) -> P<Binding> {
        if self.eat(K::LParen) {
            return Ok(Binding::Destructure(self.destructuring()?));
        }
        let name = if self.eat(K::Underscore) {
            None
        } else {
            Some(self.name()?)
        };
        if self.peek() != K::Colon {
            return self.fail("`:` and a type; every binding states its type");
        }
        self.bump();
        let ty = self.ty()?;
        Ok(Binding::Name { name, ty })
    }

    /// `(q: u32, r as rest: _)`, after the `(`. At least one field.
    fn destructuring(&mut self) -> P<Vec<DestructField>> {
        let fields = self.comma_list(|p| {
            // `var` comes right before the bound name: `var q: T`, or
            // `q as var quotient: T`.
            let mut mutable = p.eat(K::Var);
            let field = p.name()?;
            let mut rename = None;
            if !mutable && p.eat(K::As) {
                mutable = p.eat(K::Var);
                rename = Some(p.name()?);
            }
            p.expect(K::Colon, "`:` and a type")?;
            let ty = if p.eat(K::Underscore) {
                None
            } else {
                Some(p.ty()?)
            };
            Ok(DestructField {
                field,
                rename,
                mutable,
                ty,
            })
        })?;
        if fields.is_empty() {
            self.error_at(
                self.span_from(self.prev_end() - 1),
                "a pattern has at least one field",
            );
            return Err(Failed);
        }
        Ok(fields)
    }

    // Statements.

    fn block(&mut self) -> P<Block> {
        let start = self.start();
        self.expect(K::LBrace, "`{`")?;
        let stmts = self.lines(Self::statement)?;
        Ok(Block {
            stmts,
            span: self.span_from(start),
        })
    }

    fn statement(&mut self) -> P<StmtId> {
        let start = self.start();
        let kind = match self.peek() {
            K::Let | K::Var => StmtKind::Local(self.local()?),
            K::Return => {
                self.bump();
                let value = match self.peek() {
                    K::Semicolon | K::RBrace | K::Eof => None,
                    _ => Some(self.expression()?),
                };
                StmtKind::Return(value)
            }
            K::If => return self.if_statement(),
            K::Match => {
                self.bump();
                let value = self.expression()?;
                self.expect(K::LBrace, "`{`")?;
                let arms = self.lines(|p| {
                    let patterns = p.patterns()?;
                    let body = p.block()?;
                    Ok(Arm { patterns, body })
                })?;
                StmtKind::Match { value, arms }
            }
            K::For => StmtKind::For(self.for_statement()?),
            K::Break => {
                self.bump();
                StmtKind::Break
            }
            K::Continue => {
                self.bump();
                StmtKind::Continue
            }
            K::Defer => {
                self.bump();
                let call = self.postfix()?;
                self.require_call(call, "`defer` takes a call")?;
                StmtKind::Defer(call)
            }
            K::Unsafe => {
                self.bump();
                StmtKind::Unsafe(self.block()?)
            }
            K::Lock => {
                self.bump();
                let mut globals = vec![self.global_name()?];
                while self.eat(K::Comma) {
                    globals.push(self.global_name()?);
                }
                StmtKind::Lock {
                    globals,
                    body: self.block()?,
                }
            }
            K::Try => StmtKind::Expr(self.unary()?),
            K::Ident | K::Int | K::Float | K::Char | K::Str | K::StrStart | K::LParen => {
                let left = self.postfix()?;
                if let Some(op) = assign_op(self.peek()) {
                    self.bump();
                    let right = self.expression()?;
                    StmtKind::Assign { left, op, right }
                } else {
                    self.require_call(
                        left,
                        "only a call or an assignment can stand as a statement",
                    )?;
                    StmtKind::Expr(left)
                }
            }
            _ => return self.fail("a statement"),
        };
        Ok(self.ast.add_stmt(kind, self.span_from(start)))
    }

    fn require_call(&mut self, expr: ExprId, message: &str) -> P<()> {
        if matches!(self.ast.expr(expr).kind, ExprKind::Call { .. }) {
            Ok(())
        } else {
            self.error_at(self.ast.expr(expr).span, message);
            Err(Failed)
        }
    }

    fn global_name(&mut self) -> P<GlobalName> {
        let first = self.name()?;
        if self.eat(K::Dot) {
            Ok(GlobalName {
                package: Some(first),
                name: self.name()?,
            })
        } else {
            Ok(GlobalName {
                package: None,
                name: first,
            })
        }
    }

    fn if_statement(&mut self) -> P<StmtId> {
        let start = self.start();
        self.bump();
        let cond = self.expression()?;
        let then = self.block()?;
        let otherwise = if self.eat(K::Else) {
            if self.peek() == K::If {
                Some(Else::If(self.if_statement()?))
            } else {
                Some(Else::Block(self.block()?))
            }
        } else {
            None
        };
        let kind = StmtKind::If(IfStmt {
            cond,
            then,
            otherwise,
        });
        Ok(self.ast.add_stmt(kind, self.span_from(start)))
    }

    fn for_statement(&mut self) -> P<For> {
        self.bump();
        let kind = match self.peek() {
            K::LBrace => ForKind::Forever,
            K::Let | K::Var => {
                let mutable = self.bump().kind == K::Var;
                let binding = self.binding()?;
                self.expect(K::In, "`in`")?;
                let first = self.expression()?;
                let iterable = if self.eat(K::DotDot) {
                    self.require_additive(first)?;
                    let end = self.expression()?;
                    self.require_additive(end)?;
                    Iterable::Range { start: first, end }
                } else {
                    Iterable::Expr(first)
                };
                ForKind::In {
                    mutable,
                    binding,
                    iterable,
                }
            }
            _ => ForKind::While(self.expression()?),
        };
        let body = self.block()?;
        Ok(For { kind, body })
    }

    /// Range and slice bounds are arithmetic: anything that binds tighter
    /// than comparison, except bitwise operators.
    fn require_additive(&mut self, expr: ExprId) -> P<()> {
        let ok = match &self.ast.expr(expr).kind {
            ExprKind::Binary { op, .. } => {
                matches!(op.group(), OpGroup::Additive | OpGroup::Multiplicative)
            }
            ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Unsafe(_)
            | ExprKind::Array(_) => false,
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            self.error_at(
                self.ast.expr(expr).span,
                "a range bound is an arithmetic expression; add parentheses",
            );
            Err(Failed)
        }
    }

    // Patterns.

    fn patterns(&mut self) -> P<Vec<PatId>> {
        let mut patterns = vec![self.pattern()?];
        while self.eat(K::Comma) {
            patterns.push(self.pattern()?);
        }
        Ok(patterns)
    }

    fn pattern(&mut self) -> P<PatId> {
        let start = self.start();
        let kind = match self.peek() {
            K::Ident => {
                let name = self.name()?;
                let payload = if self.eat(K::LParen) {
                    Some(self.destructuring()?)
                } else {
                    None
                };
                PatternKind::Variant { name, payload }
            }
            K::Int | K::Char | K::Str | K::StrStart => PatternKind::Literal(self.primary()?),
            K::Minus => {
                self.bump();
                if self.peek() != K::Int {
                    return self.fail("an integer after `-` in a pattern");
                }
                PatternKind::Negative(self.primary()?)
            }
            K::Underscore => {
                self.bump();
                PatternKind::Discard
            }
            _ => return self.fail("a pattern"),
        };
        Ok(self.ast.add_pat(kind, self.span_from(start)))
    }

    // Expressions, from loosest to tightest.

    fn expr(&mut self, kind: ExprKind, start: u32) -> ExprId {
        let span = self.span_from(start);
        self.ast.add_expr(kind, span)
    }

    /// A full expression. `if`, `match`, `unsafe` and array literals appear
    /// only here, never as operands.
    fn expression(&mut self) -> P<ExprId> {
        let start = self.start();
        match self.peek() {
            K::If => self.if_expression(),
            K::Match => {
                self.bump();
                let value = self.expression()?;
                self.expect(K::LBrace, "`{`")?;
                let arms = self.lines(|p| {
                    let patterns = p.patterns()?;
                    let body = p.branch()?;
                    Ok(Arm { patterns, body })
                })?;
                Ok(self.expr(ExprKind::Match { value, arms }, start))
            }
            K::Unsafe => {
                self.bump();
                let body = self.branch()?;
                Ok(self.expr(ExprKind::Unsafe(body), start))
            }
            K::LBracket => {
                self.bump();
                let first = self.expression()?;
                self.array_literal(first, start)
            }
            _ => self.logical(),
        }
    }

    /// The rest of an array literal after its first element.
    fn array_literal(&mut self, first: ExprId, start: u32) -> P<ExprId> {
        let mut elems = vec![first];
        while self.eat(K::Comma) {
            if self.peek() == K::RBracket {
                break;
            }
            elems.push(self.expression()?);
        }
        self.expect(K::RBracket, "`,` or `]`")?;
        Ok(self.expr(ExprKind::Array(elems), start))
    }

    fn if_expression(&mut self) -> P<ExprId> {
        let start = self.start();
        self.bump();
        let cond = self.expression()?;
        let then = self.branch()?;
        if self.peek() != K::Else {
            return self.fail("`else`; an `if` expression needs both branches");
        }
        self.bump();
        let otherwise = if self.peek() == K::If {
            IfElse::If(self.if_expression()?)
        } else {
            IfElse::Branch(self.branch()?)
        };
        Ok(self.expr(
            ExprKind::If {
                cond,
                then,
                otherwise,
            },
            start,
        ))
    }

    /// `{ expr }`: one expression in braces.
    fn branch(&mut self) -> P<ExprId> {
        self.expect(K::LBrace, "`{`")?;
        let value = self.expression()?;
        self.eat(K::Semicolon);
        self.expect(K::RBrace, "`}`; a branch holds one expression")?;
        Ok(value)
    }

    fn binary(&mut self, op: BinaryOp, left: ExprId, right: ExprId) -> ExprId {
        let start = self.ast.expr(left).span.start;
        self.expr(ExprKind::Binary { op, left, right }, start)
    }

    fn next_binary_op(&self) -> Option<BinaryOp> {
        binary_op(self.peek())
    }

    /// `a && b && c` or `a || b || c`. The two do not mix.
    fn logical(&mut self) -> P<ExprId> {
        let mut left = self.comparison()?;
        let Some(op) = self
            .next_binary_op()
            .filter(|op| matches!(op, BinaryOp::And | BinaryOp::Or))
        else {
            return Ok(left);
        };
        while self.next_binary_op() == Some(op) {
            self.bump();
            let right = self.comparison()?;
            left = self.binary(op, left, right);
        }
        if matches!(self.next_binary_op(), Some(BinaryOp::And | BinaryOp::Or)) {
            self.error_here("`&&` and `||` do not mix; add parentheses");
            return Err(Failed);
        }
        Ok(left)
    }

    /// `a < b`. Comparisons do not chain.
    fn comparison(&mut self) -> P<ExprId> {
        let left = self.operand()?;
        let Some(op) = self
            .next_binary_op()
            .filter(|op| op.group() == OpGroup::Comparison)
        else {
            return Ok(left);
        };
        self.bump();
        let right = self.operand()?;
        let expr = self.binary(op, left, right);
        if self
            .next_binary_op()
            .is_some_and(|op| op.group() == OpGroup::Comparison)
        {
            self.error_here("comparisons do not chain; add parentheses");
            return Err(Failed);
        }
        Ok(expr)
    }

    /// An operand of a comparison: arithmetic, or one kind of bitwise
    /// operator, or one shift.
    fn operand(&mut self) -> P<ExprId> {
        let mut left = self.unary()?;
        let Some(first) = self.next_binary_op() else {
            return Ok(left);
        };
        let group = first.group();
        match group {
            OpGroup::Additive | OpGroup::Multiplicative => {
                left = self.multiplicative_tail(left)?;
                while let Some(op) = self
                    .next_binary_op()
                    .filter(|op| op.group() == OpGroup::Additive)
                {
                    self.bump();
                    let right = self.unary()?;
                    let right = self.multiplicative_tail(right)?;
                    left = self.binary(op, left, right);
                }
            }
            OpGroup::BitAnd | OpGroup::BitOr | OpGroup::BitXor => {
                while self.next_binary_op() == Some(first) {
                    self.bump();
                    let right = self.unary()?;
                    left = self.binary(first, left, right);
                }
            }
            OpGroup::Shift => {
                self.bump();
                let right = self.unary()?;
                left = self.binary(first, left, right);
                if self
                    .next_binary_op()
                    .is_some_and(|op| op.group() == OpGroup::Shift)
                {
                    self.error_here("shifts do not chain; add parentheses");
                    return Err(Failed);
                }
            }
            OpGroup::Or | OpGroup::And | OpGroup::Comparison => return Ok(left),
        }
        if let Some(next) = self.next_binary_op()
            && !matches!(
                next.group(),
                OpGroup::Or | OpGroup::And | OpGroup::Comparison
            )
        {
            let (a, b) = (first.text(), next.text());
            self.error_here(format!("`{a}` and `{b}` do not mix; add parentheses"));
            return Err(Failed);
        }
        Ok(left)
    }

    fn multiplicative_tail(&mut self, mut left: ExprId) -> P<ExprId> {
        while let Some(op) = self
            .next_binary_op()
            .filter(|op| op.group() == OpGroup::Multiplicative)
        {
            self.bump();
            let right = self.unary()?;
            left = self.binary(op, left, right);
        }
        Ok(left)
    }

    fn unary(&mut self) -> P<ExprId> {
        let start = self.start();
        let op = match self.peek() {
            K::Minus => UnaryOp::Neg,
            K::Bang => UnaryOp::Not,
            K::Tilde => UnaryOp::BitNot,
            K::Amp => UnaryOp::AddressOf,
            K::Try => {
                self.bump();
                let operand = self.unary()?;
                return Ok(self.expr(ExprKind::Try(operand), start));
            }
            _ => return self.postfix(),
        };
        self.bump();
        let operand = self.unary()?;
        Ok(self.expr(ExprKind::Unary { op, operand }, start))
    }

    fn postfix(&mut self) -> P<ExprId> {
        let start = self.start();
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                K::LParen => {
                    self.bump();
                    let args = self.comma_list(|p| {
                        let mode = p.mode();
                        Ok(Arg {
                            mode,
                            value: p.expression()?,
                        })
                    })?;
                    expr = self.expr(ExprKind::Call { callee: expr, args }, start);
                }
                K::Dot => {
                    self.bump();
                    match self.peek() {
                        K::Ident => {
                            let name = self.name()?;
                            expr = self.expr(ExprKind::Field { value: expr, name }, start);
                        }
                        K::LParen => {
                            let value = self.paren_or_tuple()?;
                            expr = self.expr(ExprKind::Construct { ty: expr, value }, start);
                        }
                        _ => return self.fail("a field name or `(` after `.`"),
                    }
                }
                K::DotStar => {
                    self.bump();
                    expr = self.expr(ExprKind::Deref(expr), start);
                }
                K::LBracket => {
                    self.bump();
                    let index = self.index()?;
                    self.expect(K::RBracket, "`]`")?;
                    expr = self.expr(ExprKind::Index { value: expr, index }, start);
                }
                _ => return Ok(expr),
            }
        }
    }

    /// What goes between `[` and `]` after a value: an index, a slice, or a
    /// type argument.
    fn index(&mut self) -> P<Index> {
        match self.peek() {
            K::Star | K::Question | K::Fn => return Ok(Index::Type(self.ty()?)),
            K::DotDot => {
                self.bump();
                let end = self.expression()?;
                self.require_additive(end)?;
                return Ok(Index::Slice {
                    start: None,
                    end: Some(end),
                });
            }
            K::LBracket => {
                if matches!(self.peek_at(1), K::RBracket | K::DotDot) {
                    return Ok(Index::Type(self.ty()?));
                }
                let start = self.start();
                self.bump();
                let first = self.expression()?;
                if self.peek() == K::RBracket && self.peek_at(1) != K::RBracket {
                    // `[n]T`: an array type.
                    self.bump();
                    let elem = self.simple_type()?;
                    let kind = TypeKind::Array { len: first, elem };
                    return Ok(Index::Type(self.ast.add_type(kind, self.span_from(start))));
                }
                return Ok(Index::Expr(self.array_literal(first, start)?));
            }
            _ => {}
        }
        let first = self.expression()?;
        if !self.eat(K::DotDot) {
            return Ok(Index::Expr(first));
        }
        self.require_additive(first)?;
        let end = if self.peek() == K::RBracket {
            None
        } else {
            let end = self.expression()?;
            self.require_additive(end)?;
            Some(end)
        };
        Ok(Index::Slice {
            start: Some(first),
            end,
        })
    }

    fn primary(&mut self) -> P<ExprId> {
        let start = self.start();
        let kind = match self.peek() {
            K::Ident => ExprKind::Name(self.name()?),
            K::Int => {
                self.bump();
                ExprKind::Int
            }
            K::Float => {
                self.bump();
                ExprKind::Float
            }
            K::Char => {
                self.bump();
                ExprKind::Char
            }
            K::Str | K::StrStart => ExprKind::Str(self.string()?),
            K::LParen => return self.paren_or_tuple(),
            _ => return self.fail("an expression"),
        };
        Ok(self.expr(kind, start))
    }

    /// A string literal: one `Str` token, or `StrStart`, then expressions
    /// separated by `StrMid`, then `StrEnd`.
    fn string(&mut self) -> P<Vec<StrPart>> {
        let mut parts = Vec::new();
        loop {
            let token = self.bump();
            self.string_text(token, &mut parts);
            if matches!(token.kind, K::Str | K::StrEnd) {
                return Ok(parts);
            }
            parts.push(StrPart::Interpolation(self.expression()?));
            if !matches!(self.peek(), K::StrMid | K::StrEnd) {
                return self.fail("`}` to close the interpolation");
            }
        }
    }

    /// The text of a string token without its delimiters.
    fn string_text(&self, token: Token, parts: &mut Vec<StrPart>) {
        let text = &self.text[token.span.range()];
        let start = token.span.start + 1;
        let closed = text.len() >= 2 && (text.ends_with('"') || text.ends_with('{'));
        let end = if closed {
            token.span.end - 1
        } else {
            token.span.end
        };
        if end > start {
            parts.push(StrPart::Text(Span { start, end }));
        }
    }

    /// `(expr)`, or a tuple literal `(name: expr, ...)`.
    fn paren_or_tuple(&mut self) -> P<ExprId> {
        let start = self.start();
        self.bump();
        if self.peek() == K::Ident && self.peek_at(1) == K::Colon {
            let fields = self.comma_list(|p| {
                let name = p.name()?;
                p.expect(K::Colon, "`:`")?;
                Ok((name, p.expression()?))
            })?;
            return Ok(self.expr(ExprKind::Tuple(fields), start));
        }
        let inner = self.expression()?;
        self.expect(K::RParen, "`)`")?;
        Ok(self.expr(ExprKind::Paren(inner), start))
    }
}
