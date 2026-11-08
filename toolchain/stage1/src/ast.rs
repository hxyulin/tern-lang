//! The syntax tree. Expressions, types, statements and patterns live in
//! arenas inside [`Ast`] and refer to each other by index.

use crate::source::Span;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(u32);

        impl $name {
            fn new(index: usize) -> $name {
                $name(u32::try_from(index).expect("fewer than 2^32 nodes"))
            }

            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id_type!(ExprId);
id_type!(TypeId);
id_type!(StmtId);
id_type!(PatId);

#[derive(Debug, Default)]
pub struct Ast {
    pub exprs: Vec<Expr>,
    pub types: Vec<Type>,
    pub stmts: Vec<Stmt>,
    pub pats: Vec<Pattern>,
    pub imports: Vec<Import>,
    pub decls: Vec<Decl>,
}

impl Ast {
    pub fn add_expr(&mut self, kind: ExprKind, span: Span) -> ExprId {
        self.exprs.push(Expr { kind, span });
        ExprId::new(self.exprs.len() - 1)
    }

    pub fn add_type(&mut self, kind: TypeKind, span: Span) -> TypeId {
        self.types.push(Type { kind, span });
        TypeId::new(self.types.len() - 1)
    }

    pub fn add_stmt(&mut self, kind: StmtKind, span: Span) -> StmtId {
        self.stmts.push(Stmt { kind, span });
        StmtId::new(self.stmts.len() - 1)
    }

    pub fn add_pat(&mut self, kind: PatternKind, span: Span) -> PatId {
        self.pats.push(Pattern { kind, span });
        PatId::new(self.pats.len() - 1)
    }

    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.index()]
    }

    pub fn ty(&self, id: TypeId) -> &Type {
        &self.types[id.index()]
    }

    pub fn stmt(&self, id: StmtId) -> &Stmt {
        &self.stmts[id.index()]
    }

    pub fn pat(&self, id: PatId) -> &Pattern {
        &self.pats[id.index()]
    }
}

/// An identifier, by its place in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    pub span: Span,
}

/// `import std.io as io2`
#[derive(Debug)]
pub struct Import {
    pub path: Vec<Name>,
    pub alias: Option<Name>,
    pub span: Span,
}

/// `@align(16)`. Arguments are integer or string literals.
#[derive(Debug)]
pub struct Attribute {
    pub name: Span,
    pub args: Vec<ExprId>,
}

#[derive(Debug)]
pub struct Decl {
    pub attributes: Vec<Attribute>,
    pub public: bool,
    pub kind: DeclKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum DeclKind {
    Fn(FnDecl),
    ExternFn {
        name: Name,
        params: Vec<Param>,
        variadic: bool,
        result: Option<TypeId>,
    },
    ExternVar {
        mutable: bool,
        name: Name,
        ty: TypeId,
    },
    Type {
        name: Name,
        def: TypeDef,
    },
    Global {
        shared: bool,
        local: Local,
    },
}

#[derive(Debug)]
pub struct FnDecl {
    pub is_unsafe: bool,
    pub name: Name,
    pub params: Vec<Param>,
    pub result: Option<TypeId>,
    pub body: Block,
}

/// How an argument is passed. `Let` is the default and is not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Let,
    Inout,
    Sink,
}

#[derive(Debug)]
pub struct Param {
    /// `None` for `_`.
    pub name: Option<Name>,
    pub mode: Mode,
    pub ty: TypeId,
}

#[derive(Debug)]
pub enum TypeDef {
    Type(TypeId),
    Enum {
        int_type: Option<TypeId>,
        items: Vec<EnumItem>,
    },
    Union(Vec<Field>),
}

#[derive(Debug)]
pub enum EnumItem {
    Variant {
        name: Name,
        /// A tuple type.
        payload: Option<TypeId>,
        value: Option<ExprId>,
    },
    /// A trailing `_`: the enum is open.
    Open(Span),
}

#[derive(Debug)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum TypeKind {
    Name {
        package: Option<Name>,
        name: Name,
    },
    Tuple(Vec<Field>),
    View(TypeId),
    Array {
        len: ExprId,
        elem: TypeId,
    },
    Growable(TypeId),
    Pointer(TypeId),
    Optional(TypeId),
    Result {
        value: Option<TypeId>,
        error: TypeId,
    },
    Fn {
        params: Vec<(Mode, TypeId)>,
        result: Option<TypeId>,
    },
}

#[derive(Debug)]
pub struct Field {
    pub public: bool,
    pub name: Name,
    pub ty: TypeId,
}

/// `let` or `var` with its value.
#[derive(Debug)]
pub struct Local {
    pub mutable: bool,
    pub binding: Binding,
    pub value: ExprId,
}

#[derive(Debug)]
pub enum Binding {
    /// `x: T`, or `_: T` with `name` `None`.
    Name { name: Option<Name>, ty: TypeId },
    /// `(q: u32, r as rest: _)`
    Destructure(Vec<DestructField>),
}

#[derive(Debug)]
pub struct DestructField {
    pub field: Name,
    /// The name after `as`, if the binding differs from the field.
    pub rename: Option<Name>,
    /// `None` for `_`: the field is not bound.
    pub ty: Option<TypeId>,
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<StmtId>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    Local(Local),
    Return(Option<ExprId>),
    If(IfStmt),
    Match {
        value: ExprId,
        arms: Vec<Arm<Block>>,
    },
    For(For),
    Break,
    Continue,
    /// A call expression.
    Defer(ExprId),
    Unsafe(Block),
    Lock {
        globals: Vec<GlobalName>,
        body: Block,
    },
    Assign {
        left: ExprId,
        op: AssignOp,
        right: ExprId,
    },
    /// A call, or `try` applied to an expression.
    Expr(ExprId),
}

#[derive(Debug)]
pub struct IfStmt {
    pub cond: ExprId,
    pub then: Block,
    pub otherwise: Option<Else>,
}

#[derive(Debug)]
pub enum Else {
    Block(Block),
    /// An `if` statement.
    If(StmtId),
}

#[derive(Debug)]
pub struct Arm<B> {
    pub patterns: Vec<PatId>,
    pub body: B,
}

#[derive(Debug)]
pub struct For {
    pub kind: ForKind,
    pub body: Block,
}

#[derive(Debug)]
pub enum ForKind {
    Forever,
    While(ExprId),
    In {
        mutable: bool,
        binding: Binding,
        iterable: Iterable,
    },
}

#[derive(Debug)]
pub enum Iterable {
    Expr(ExprId),
    Range { start: ExprId, end: ExprId },
}

/// `rx` or `board.rx` in a `lock`.
#[derive(Debug)]
pub struct GlobalName {
    pub package: Option<Name>,
    pub name: Name,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

#[derive(Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatternKind {
    /// `none`, or `some(value: u8)`. A bare name is always a variant.
    Variant {
        name: Name,
        payload: Option<Vec<DestructField>>,
    },
    /// An integer, character or string literal expression.
    Literal(ExprId),
    /// `-1`: an integer literal expression, negated.
    Negative(ExprId),
    Discard,
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Name(Name),
    Int,
    Float,
    Char,
    Str(Vec<StrPart>),
    Paren(ExprId),
    Tuple(Vec<(Name, ExprId)>),
    Call {
        callee: ExprId,
        args: Vec<Arg>,
    },
    Field {
        value: ExprId,
        name: Name,
    },
    /// `Point.(x: 1, y: 2)`. The value is a tuple or parenthesized
    /// expression.
    Construct {
        ty: ExprId,
        value: ExprId,
    },
    Index {
        value: ExprId,
        index: Index,
    },
    Deref(ExprId),
    Unary {
        op: UnaryOp,
        operand: ExprId,
    },
    Try(ExprId),
    Binary {
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    },
    If {
        cond: ExprId,
        then: ExprId,
        otherwise: IfElse,
    },
    Match {
        value: ExprId,
        arms: Vec<Arm<ExprId>>,
    },
    Unsafe(ExprId),
    Array(Vec<ExprId>),
}

/// One piece of a string literal, in source order.
#[derive(Debug)]
pub enum StrPart {
    /// Literal text, with its escapes still written out.
    Text(Span),
    Interpolation(ExprId),
}

#[derive(Debug)]
pub enum IfElse {
    /// `else { x }`
    Branch(ExprId),
    /// `else if ...`: an `if` expression.
    If(ExprId),
}

#[derive(Debug)]
pub struct Arg {
    pub mode: Mode,
    pub value: ExprId,
}

#[derive(Debug)]
pub enum Index {
    Expr(ExprId),
    Slice {
        start: Option<ExprId>,
        end: Option<ExprId>,
    },
    /// A type argument that is not a plain name: `from_address[*u32]`.
    Type(TypeId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    AddressOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinaryOp {
    pub fn text(self) -> &'static str {
        use BinaryOp as B;
        match self {
            B::Or => "||",
            B::And => "&&",
            B::Eq => "==",
            B::Ne => "!=",
            B::Lt => "<",
            B::Le => "<=",
            B::Gt => ">",
            B::Ge => ">=",
            B::Add => "+",
            B::Sub => "-",
            B::Mul => "*",
            B::Div => "/",
            B::Rem => "%",
            B::BitAnd => "&",
            B::BitOr => "|",
            B::BitXor => "^",
            B::Shl => "<<",
            B::Shr => ">>",
        }
    }

    /// The operator group, in the partial order of DESIGN.md. Operators of
    /// different groups do not mix without parentheses.
    pub fn group(self) -> OpGroup {
        use BinaryOp as B;
        match self {
            B::Or => OpGroup::Or,
            B::And => OpGroup::And,
            B::Eq | B::Ne | B::Lt | B::Le | B::Gt | B::Ge => OpGroup::Comparison,
            B::Add | B::Sub => OpGroup::Additive,
            B::Mul | B::Div | B::Rem => OpGroup::Multiplicative,
            B::BitAnd => OpGroup::BitAnd,
            B::BitOr => OpGroup::BitOr,
            B::BitXor => OpGroup::BitXor,
            B::Shl | B::Shr => OpGroup::Shift,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpGroup {
    Or,
    And,
    Comparison,
    Additive,
    Multiplicative,
    BitAnd,
    BitOr,
    BitXor,
    Shift,
}
