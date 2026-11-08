//! Type checking.
//!
//! Each function body is checked against declarations only, top to
//! bottom. Types flow from context into literals: an integer literal, a
//! `none`, a `some.(...)` or an array literal takes the type its position
//! expects, and an operator takes the type of its other operand.
//!
//! The same pass checks the local safety rules: `unsafe`, `shared`
//! globals, moves (in [`moves`]), and exclusivity, the `Format` rule and
//! constant initializers (in [`rules`]).

mod moves;
mod rules;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Arg, AssignOp, Ast, BinaryOp, Binding, Block, DeclKind, DestructField, Else, EnumItem, ExprId,
    ExprKind, ForKind, IfElse, Index, Iterable, Mode, Name, OpGroup, PatId, PatternKind, StmtId,
    StmtKind, StrPart, TypeDef, TypeId, TypeKind, UnaryOp,
};
use crate::diagnostic::Diagnostic;
use crate::package::{FileId, Program};
use crate::resolve::{BUILTIN_TYPES, DeclRef, LocalId, Res, Resolution};
use crate::source::Span;
use crate::types::{
    BuiltinNamed, EnumInfo, FnTy, IntTy, NamedDef, NamedId, NamedInfo, Ty, TyId, Types, Variant,
};
use moves::{LoopFlow, Moved};

pub struct Checked {
    pub types: Types,
    pub diagnostics: Vec<(FileId, Diagnostic)>,
    /// What the checker found, per file, for code generation.
    pub files: Vec<FileTypes>,
    /// Each function's type.
    pub fn_tys: HashMap<DeclRef, TyId>,
}

/// The checker's results for one file, indexed like its syntax tree.
#[derive(Default)]
pub struct FileTypes {
    /// What each expression denotes, with its type.
    pub exprs: Vec<Option<Item>>,
    /// Each written type, lowered.
    pub types: Vec<Option<TyId>>,
    /// Each local's type.
    pub locals: HashMap<LocalId, TyId>,
}

pub fn check(program: &Program, names: &Resolution) -> Checked {
    let mut checker = Checker {
        program,
        names,
        types: Types::new(),
        fn_tys: HashMap::new(),
        move_only: HashMap::new(),
        diagnostics: Vec::new(),
        files: program
            .files
            .iter()
            .map(|f| FileTypes {
                exprs: vec![None; f.ast.exprs.len()],
                types: vec![None; f.ast.types.len()],
                locals: HashMap::new(),
            })
            .collect(),
    };
    for (i, file) in program.files.iter().enumerate() {
        let file_id = FileId(u32::try_from(i).expect("few files"));
        for (index, decl) in file.ast.decls.iter().enumerate() {
            checker.decl(
                file_id,
                DeclRef {
                    file: file_id,
                    index,
                },
                &decl.kind,
            );
        }
    }
    let mut diagnostics = checker.diagnostics;
    diagnostics.sort_by_key(|(file, d)| (file.0, d.span.start));
    Checked {
        types: checker.types,
        diagnostics,
        files: checker.files,
        fn_tys: checker.fn_tys,
    }
}

/// What an expression denotes. Most are values; a few positions also
/// accept a type, a built-in function or a variant to construct.
#[derive(Clone, Copy, Debug)]
pub enum Item {
    Value(TyId),
    /// A call that returns nothing.
    Nothing,
    Type(TyId),
    Builtin(&'static str),
    /// A variant with a payload, before `.(...)`.
    Variant(TyId, usize),
}

struct LocalInfo {
    ty: TyId,
    mutable: bool,
    owner: Owner,
    /// How many loops enclose the binding.
    loop_depth: usize,
}

/// Whether a function owns a local's value, which decides whether it can
/// move it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// A `let` or `var` local, or a `sink` parameter.
    Own,
    /// A parameter passed without `sink`.
    Param(Mode),
    /// A `for` loop's binding.
    Loop,
    /// A payload bound by a `match` on this local.
    Part(LocalId),
    /// A payload bound by a `match` on a field, element or global.
    Borrowed,
}

/// The function being checked.
struct Fx<'a> {
    file: FileId,
    ast: &'a Ast,
    /// `None` for a function that returns nothing, and outside functions.
    result: Option<TyId>,
    locals: HashMap<LocalId, LocalInfo>,
    /// One entry per enclosing loop.
    loops: Vec<LoopFlow>,
    in_unsafe: bool,
    /// The globals the enclosing `lock` takes.
    locked: Vec<DeclRef>,
    /// Places being assigned to, and places whose address `&` takes. A
    /// union member or C global here is not read.
    written: HashSet<ExprId>,
    addressed: HashSet<ExprId>,
    /// The local that `x = ...` gives a new value, which may have been
    /// moved.
    assign_target: Option<ExprId>,
    /// The locals moved on the way to the current point.
    moved: Moved,
    /// The locals a `defer` in an enclosing block passes `sink`.
    deferred: Moved,
}

impl<'a> Fx<'a> {
    fn new(file: FileId, ast: &'a Ast) -> Fx<'a> {
        Fx {
            file,
            ast,
            result: None,
            locals: HashMap::new(),
            loops: Vec::new(),
            in_unsafe: false,
            locked: Vec::new(),
            written: HashSet::new(),
            addressed: HashSet::new(),
            assign_target: None,
            moved: Moved::new(),
            deferred: Moved::new(),
        }
    }
}

/// An enum's variants, each with its payload type, and whether the enum
/// is open.
type Variants = (Vec<(String, Option<TyId>)>, bool);

/// Which arms a `match` has covered.
#[derive(Default)]
struct Coverage {
    variants: HashSet<String>,
    wildcard: bool,
}

struct Checker<'a> {
    program: &'a Program,
    names: &'a Resolution,
    types: Types,
    fn_tys: HashMap<DeclRef, TyId>,
    /// Whether each type holds a `[..]T`, and so moves instead of copying.
    move_only: HashMap<TyId, bool>,
    diagnostics: Vec<(FileId, Diagnostic)>,
    files: Vec<FileTypes>,
}

impl<'a> Checker<'a> {
    fn error(&mut self, file: FileId, span: Span, message: impl Into<String>) {
        self.diagnostics
            .push((file, Diagnostic::error(span, message)));
    }

    fn text(&self, file: FileId, span: Span) -> &'a str {
        self.program.file(file).source.slice(span)
    }

    fn show(&self, ty: TyId) -> String {
        self.types.display(ty)
    }

    // Types.

    fn lower(&mut self, file: FileId, id: TypeId) -> TyId {
        let ty = self.lower_type(file, id);
        self.files[file.0 as usize].types[id.index()] = Some(ty);
        ty
    }

    fn lower_type(&mut self, file: FileId, id: TypeId) -> TyId {
        let ast = &self.program.file(file).ast;
        let kind = &ast.ty(id).kind;
        let ty = match kind {
            TypeKind::Name { .. } => {
                return match self.names.files[file.0 as usize].types[id.index()] {
                    Some(Res::Builtin(name)) => self.builtin_type(name),
                    Some(Res::Decl(decl)) => self.named(NamedId::Decl(decl)),
                    _ => self.types.error(),
                };
            }
            TypeKind::Tuple(fields) => Ty::Tuple(
                fields
                    .iter()
                    .map(|f| {
                        (
                            self.text(file, f.name.span).to_owned(),
                            self.lower(file, f.ty),
                        )
                    })
                    .collect(),
            ),
            TypeKind::View(elem) => Ty::View(self.lower(file, *elem)),
            TypeKind::Growable(elem) => Ty::Growable(self.lower(file, *elem)),
            TypeKind::Pointer(elem) => Ty::Pointer(self.lower(file, *elem)),
            TypeKind::Optional(elem) => Ty::Optional(self.lower(file, *elem)),
            TypeKind::Array { len, elem } => {
                let elem = self.lower(file, *elem);
                match self
                    .const_int(file, *len, 0)
                    .and_then(|n| u64::try_from(n).ok())
                {
                    Some(n) => Ty::Array(n, elem),
                    None => {
                        let span = ast.expr(*len).span;
                        self.error(
                            file,
                            span,
                            "an array length is a constant, non-negative integer",
                        );
                        return self.types.error();
                    }
                }
            }
            TypeKind::Result { value, error } => {
                Ty::Result(value.map(|v| self.lower(file, v)), self.lower(file, *error))
            }
            TypeKind::Fn { params, result } => Ty::Fn(FnTy {
                params: params
                    .iter()
                    .map(|&(mode, ty)| (mode, self.lower(file, ty)))
                    .collect(),
                result: result.map(|r| self.lower(file, r)),
            }),
        };
        self.types.intern(ty)
    }

    fn builtin_type(&mut self, name: &str) -> TyId {
        if let Some(int) = IntTy::from_name(name) {
            return self.types.int(int);
        }
        match name {
            "f32" => self.types.intern(Ty::Float(32)),
            "f64" => self.types.intern(Ty::Float(64)),
            "bool" => self.types.bool(),
            "Allocator" => self.named(NamedId::Builtin(BuiltinNamed::Allocator)),
            "Format" => self.named(NamedId::Builtin(BuiltinNamed::Format)),
            "PanicInfo" => self.named(NamedId::Builtin(BuiltinNamed::PanicInfo)),
            "OutOfMemory" => self.named(NamedId::Builtin(BuiltinNamed::OutOfMemory)),
            _ => self.types.error(),
        }
    }

    /// The type for a named type, computing its definition the first time.
    fn named(&mut self, id: NamedId) -> TyId {
        let ty = self.types.named_ty(id);
        if self.types.named.contains_key(&id) {
            return ty;
        }
        // A placeholder, so a type that refers to itself through a view or
        // pointer does not recurse forever.
        let placeholder = NamedInfo {
            display: String::new(),
            package: None,
            def: NamedDef::Union(Vec::new()),
        };
        self.types.named.insert(id, placeholder);
        let info = match id {
            NamedId::Decl(decl) => self.named_decl(decl),
            NamedId::Builtin(builtin) => self.named_builtin(builtin),
        };
        self.types.named.insert(id, info);
        ty
    }

    fn named_decl(&mut self, decl: DeclRef) -> NamedInfo {
        let file = self.program.file(decl.file);
        let DeclKind::Type { name, def, .. } = &file.ast.decls[decl.index].kind else {
            unreachable!("only type declarations are named types");
        };
        let package_path = &self.program.package(file.package).path;
        let short = self.text(decl.file, name.span);
        let display = match package_path.rsplit('.').next() {
            Some(last) if !last.is_empty() => format!("{last}.{short}"),
            _ => short.to_owned(),
        };
        let def = match def {
            TypeDef::Type(ty) => {
                let pub_fields = match &file.ast.ty(*ty).kind {
                    TypeKind::Tuple(fields) => fields.iter().map(|f| f.public).collect(),
                    _ => Vec::new(),
                };
                NamedDef::Plain {
                    underlying: self.lower(decl.file, *ty),
                    pub_fields,
                }
            }
            TypeDef::Enum { int_type, items } => {
                let int = int_type.and_then(|t| {
                    let ty = self.lower(decl.file, t);
                    let int = self.types.as_int(ty);
                    if int.is_none() && !self.types.is_error(ty) {
                        let span = file.ast.ty(t).span;
                        self.error(
                            decl.file,
                            span,
                            "an enum's integer type is a built-in integer",
                        );
                    }
                    int
                });
                let mut variants = Vec::new();
                let mut open = false;
                for item in items {
                    match item {
                        EnumItem::Variant {
                            name,
                            payload,
                            value,
                        } => {
                            let payload = payload.map(|p| self.lower(decl.file, p));
                            let value = value.and_then(|v| self.enum_value(decl.file, v, int));
                            variants.push(Variant {
                                name: self.text(decl.file, name.span).to_owned(),
                                payload,
                                value,
                            });
                        }
                        EnumItem::Open(_) => open = true,
                    }
                }
                NamedDef::Enum(EnumInfo {
                    variants,
                    int,
                    open,
                })
            }
            TypeDef::Union(fields) => NamedDef::Union(
                fields
                    .iter()
                    .map(|f| {
                        (
                            self.text(decl.file, f.name.span).to_owned(),
                            self.lower(decl.file, f.ty),
                        )
                    })
                    .collect(),
            ),
        };
        NamedInfo {
            display,
            package: Some(file.package),
            def,
        }
    }

    fn enum_value(&mut self, file: FileId, value: ExprId, int: Option<IntTy>) -> Option<i128> {
        let span = self.program.file(file).ast.expr(value).span;
        let Some(int) = int else {
            self.error(
                file,
                span,
                "only an enum with an integer type, as in `enum u8`, gives its variants values",
            );
            return None;
        };
        let Some(n) = self.const_int(file, value, 0) else {
            self.error(file, span, "a variant's value is a constant integer");
            return None;
        };
        if n < int.min() || n > int.max() {
            self.error(
                file,
                span,
                format!("`{n}` does not fit in `{}`", int.name()),
            );
        }
        Some(n)
    }

    fn named_builtin(&mut self, builtin: BuiltinNamed) -> NamedInfo {
        let record = |fields: Vec<(&str, TyId)>| {
            let pub_fields = vec![true; fields.len()];
            (
                Ty::Tuple(fields.into_iter().map(|(n, t)| (n.to_owned(), t)).collect()),
                pub_fields,
            )
        };
        let (display, def) = match builtin {
            BuiltinNamed::Allocator => {
                let u8 = self.types.int(IntTy::U8);
                let ptr = self.types.intern(Ty::Pointer(u8));
                let context = self.types.intern(Ty::Optional(ptr));
                let usize = self.types.usize();
                let alloc = self.fn_ty(vec![context, usize, usize], Some(context));
                let resize = self.fn_ty(vec![context, ptr, usize, usize, usize], Some(context));
                let free = self.fn_ty(vec![context, ptr, usize, usize], None);
                let (ty, pub_fields) = record(vec![
                    ("context", context),
                    ("alloc", alloc),
                    ("resize", resize),
                    ("free", free),
                ]);
                let underlying = self.types.intern(ty);
                (
                    "Allocator",
                    NamedDef::Plain {
                        underlying,
                        pub_fields,
                    },
                )
            }
            BuiltinNamed::Format => {
                // (parts: []fmt.Part), when std.fmt is part of the program.
                let part = self.fmt_part().map(|p| self.named(NamedId::Decl(p)));
                let fields = match part {
                    Some(part) => vec![("parts", self.types.intern(Ty::View(part)))],
                    None => Vec::new(),
                };
                let (ty, pub_fields) = record(fields);
                let underlying = self.types.intern(ty);
                (
                    "Format",
                    NamedDef::Plain {
                        underlying,
                        pub_fields,
                    },
                )
            }
            BuiltinNamed::PanicInfo => {
                let format = self.named(NamedId::Builtin(BuiltinNamed::Format));
                let bytes = self.types.bytes();
                let u32 = self.types.int(IntTy::U32);
                let (ty, pub_fields) = record(vec![
                    ("message", format),
                    ("file", bytes),
                    ("line", u32),
                    ("column", u32),
                    ("function", bytes),
                ]);
                let underlying = self.types.intern(ty);
                (
                    "PanicInfo",
                    NamedDef::Plain {
                        underlying,
                        pub_fields,
                    },
                )
            }
            BuiltinNamed::OutOfMemory => (
                "OutOfMemory",
                NamedDef::Enum(EnumInfo {
                    variants: vec![Variant {
                        name: "out_of_memory".to_owned(),
                        payload: None,
                        value: None,
                    }],
                    int: None,
                    open: false,
                }),
            ),
        };
        NamedInfo {
            display: display.to_owned(),
            package: None,
            def,
        }
    }

    fn fn_ty(&mut self, params: Vec<TyId>, result: Option<TyId>) -> TyId {
        self.types.intern(Ty::Fn(FnTy {
            params: params.into_iter().map(|t| (Mode::Let, t)).collect(),
            result,
        }))
    }

    fn fmt_part(&self) -> Option<DeclRef> {
        let index = self
            .program
            .packages
            .iter()
            .position(|p| p.path == "std.fmt")?;
        self.names.scopes[index].get("Part").copied()
    }

    fn is_named(&self, ty: TyId, builtin: BuiltinNamed) -> bool {
        matches!(self.types.get(ty), Ty::Named(NamedId::Builtin(b)) if *b == builtin)
    }

    fn named_info(&self, ty: TyId) -> Option<&NamedInfo> {
        match self.types.get(ty) {
            Ty::Named(id) => self.types.named.get(id),
            _ => None,
        }
    }

    /// The integer behind a built-in integer or an integer newtype.
    fn int_core(&self, ty: TyId) -> Option<IntTy> {
        match self.types.get(ty) {
            Ty::Int(int) => Some(*int),
            Ty::Named(_) => match &self.named_info(ty)?.def {
                NamedDef::Plain { underlying, .. } => self.types.as_int(*underlying),
                _ => None,
            },
            _ => None,
        }
    }

    fn is_float(&self, ty: TyId) -> bool {
        matches!(self.types.get(ty), Ty::Float(_))
    }

    /// The fields of a record, and whether each is visible from `file`.
    fn record_fields(&self, ty: TyId, file: FileId) -> Option<Vec<(String, TyId, bool)>> {
        match self.types.get(ty) {
            Ty::Tuple(fields) => Some(fields.iter().map(|(n, t)| (n.clone(), *t, true)).collect()),
            Ty::Named(_) => {
                let info = self.named_info(ty)?;
                let local = info
                    .package
                    .is_none_or(|p| p == self.program.file(file).package);
                match &info.def {
                    NamedDef::Plain {
                        underlying,
                        pub_fields,
                    } => match self.types.get(*underlying) {
                        Ty::Tuple(fields) => Some(
                            fields
                                .iter()
                                .zip(pub_fields)
                                .map(|((n, t), p)| (n.clone(), *t, local || *p))
                                .collect(),
                        ),
                        _ => None,
                    },
                    NamedDef::Union(fields) => {
                        Some(fields.iter().map(|(n, t)| (n.clone(), *t, true)).collect())
                    }
                    NamedDef::Enum(_) => None,
                }
            }
            _ => None,
        }
    }

    /// The variants of an enum, optional or result: name and payload.
    fn variants_of(&mut self, ty: TyId) -> Option<Variants> {
        match self.types.get(ty).clone() {
            Ty::Optional(value) => {
                let payload = self
                    .types
                    .intern(Ty::Tuple(vec![("value".to_owned(), value)]));
                Some((
                    vec![
                        ("some".to_owned(), Some(payload)),
                        ("none".to_owned(), None),
                    ],
                    false,
                ))
            }
            Ty::Result(value, error) => {
                let ok = value.map(|v| self.types.intern(Ty::Tuple(vec![("value".to_owned(), v)])));
                let err = self
                    .types
                    .intern(Ty::Tuple(vec![("error".to_owned(), error)]));
                Some((
                    vec![("ok".to_owned(), ok), ("err".to_owned(), Some(err))],
                    false,
                ))
            }
            Ty::Named(_) => match &self.named_info(ty)?.def {
                NamedDef::Enum(info) => Some((
                    info.variants
                        .iter()
                        .map(|v| (v.name.clone(), v.payload))
                        .collect(),
                    info.open,
                )),
                _ => None,
            },
            _ => None,
        }
    }

    fn coerces(&self, actual: TyId, expected: TyId) -> bool {
        if actual == expected {
            return true;
        }
        match (self.types.get(actual), self.types.get(expected)) {
            (Ty::Never | Ty::Error, _) | (_, Ty::Error) => true,
            (Ty::Array(_, a) | Ty::Growable(a), Ty::View(e)) => a == e,
            // A function value takes a named function type from context,
            // as a literal takes a newtype.
            (Ty::Fn(_), Ty::Named(_)) => self.fn_underlying(expected) == Some(actual),
            _ => false,
        }
    }

    /// The function type behind a named function type.
    fn fn_underlying(&self, ty: TyId) -> Option<TyId> {
        match &self.named_info(ty)?.def {
            NamedDef::Plain { underlying, .. }
                if matches!(self.types.get(*underlying), Ty::Fn(_)) =>
            {
                Some(*underlying)
            }
            _ => None,
        }
    }

    fn expect_ty(&mut self, file: FileId, span: Span, actual: TyId, expected: TyId) {
        if !self.coerces(actual, expected) {
            let message = format!(
                "expected `{}`, found `{}`",
                self.show(expected),
                self.show(actual)
            );
            self.error(file, span, message);
        }
    }

    // Constants.

    fn const_int(&self, file: FileId, e: ExprId, depth: u32) -> Option<i128> {
        if depth > 32 {
            return None;
        }
        let ast = &self.program.file(file).ast;
        let expr = ast.expr(e);
        match &expr.kind {
            ExprKind::Int => parse_int(self.text(file, expr.span)),
            ExprKind::Char => char_value(self.text(file, expr.span)),
            ExprKind::Paren(inner) => self.const_int(file, *inner, depth),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => self.const_int(file, *operand, depth).map(|n| -n),
            ExprKind::Binary { op, left, right } => {
                let a = self.const_int(file, *left, depth)?;
                let b = self.const_int(file, *right, depth)?;
                match op {
                    BinaryOp::Add => a.checked_add(b),
                    BinaryOp::Sub => a.checked_sub(b),
                    BinaryOp::Mul => a.checked_mul(b),
                    BinaryOp::Div => a.checked_div(b),
                    BinaryOp::Rem => a.checked_rem(b),
                    BinaryOp::Shl => u32::try_from(b).ok().and_then(|b| a.checked_shl(b)),
                    BinaryOp::Shr => u32::try_from(b).ok().and_then(|b| a.checked_shr(b)),
                    BinaryOp::BitAnd => Some(a & b),
                    BinaryOp::BitOr => Some(a | b),
                    BinaryOp::BitXor => Some(a ^ b),
                    _ => None,
                }
            }
            ExprKind::Name(_) | ExprKind::Field { .. } => {
                let Some(Res::Decl(decl)) = self.names.files[file.0 as usize].exprs[e.index()]
                else {
                    return None;
                };
                let target = &self.program.file(decl.file).ast.decls[decl.index];
                match &target.kind {
                    DeclKind::Global {
                        shared: false,
                        local,
                    } if !local.mutable && matches!(local.binding, Binding::Name { .. }) => {
                        self.const_int(decl.file, local.value, depth + 1)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    // Declarations.

    fn decl_fn_ty(&mut self, decl: DeclRef) -> TyId {
        if let Some(&ty) = self.fn_tys.get(&decl) {
            return ty;
        }
        let ast = &self.program.file(decl.file).ast;
        let (params, result) = match &ast.decls[decl.index].kind {
            DeclKind::Fn(f) => (&f.params, f.result),
            DeclKind::ExternFn { params, result, .. } => (params, *result),
            _ => unreachable!("only functions have function types"),
        };
        let ty = Ty::Fn(FnTy {
            params: params
                .iter()
                .map(|p| (p.mode, self.lower(decl.file, p.ty)))
                .collect(),
            result: result.map(|r| self.lower(decl.file, r)),
        });
        let ty = self.types.intern(ty);
        self.fn_tys.insert(decl, ty);
        ty
    }

    /// What a top-level name denotes. `name` picks the field of a
    /// destructured global.
    fn decl_item(&mut self, decl: DeclRef, name: &str) -> Item {
        let ast = &self.program.file(decl.file).ast;
        match &ast.decls[decl.index].kind {
            DeclKind::Fn(_) | DeclKind::ExternFn { .. } => Item::Value(self.decl_fn_ty(decl)),
            DeclKind::ExternVar { ty, .. } => Item::Value(self.lower(decl.file, *ty)),
            DeclKind::Type { .. } => Item::Type(self.named(NamedId::Decl(decl))),
            DeclKind::Global { local, .. } => match &local.binding {
                Binding::Name { ty, .. } => Item::Value(self.lower(decl.file, *ty)),
                Binding::Destructure(fields) => {
                    let field = fields
                        .iter()
                        .find(|f| self.text(decl.file, f.rename.unwrap_or(f.field).span) == name);
                    match field.and_then(|f| f.ty) {
                        Some(ty) => Item::Value(self.lower(decl.file, ty)),
                        None => Item::Value(self.types.error()),
                    }
                }
            },
        }
    }

    fn decl(&mut self, file: FileId, decl: DeclRef, kind: &'a DeclKind) {
        let ast = &self.program.file(file).ast;
        let mut fx = Fx::new(file, ast);
        match kind {
            DeclKind::Fn(f) => {
                self.decl_fn_ty(decl);
                fx.result = f.result.map(|r| self.lower(file, r));
                if let (Some(result), Some(ty)) = (fx.result, f.result) {
                    self.no_format(file, ast.ty(ty).span, result);
                }
                for param in &f.params {
                    let ty = self.lower(file, param.ty);
                    self.format_param(file, ast.ty(param.ty).span, ty);
                    let owner = match param.mode {
                        Mode::Sink => Owner::Own,
                        mode => Owner::Param(mode),
                    };
                    if let Some(name) = param.name {
                        self.bind(&mut fx, name, ty, param.mode != Mode::Let, owner);
                    }
                }
                let diverges = self.block(&mut fx, &f.body);
                if let Some(result) = fx.result
                    && !diverges
                {
                    let message = match self.types.get(result) {
                        Ty::Result(None, _) => {
                            "this function can reach its end without `return ok`".to_owned()
                        }
                        _ => format!(
                            "this function can reach its end without returning a `{}`",
                            self.show(result)
                        ),
                    };
                    self.error(file, f.name.span, message);
                }
            }
            DeclKind::ExternFn { params, result, .. } => {
                let ty = self.decl_fn_ty(decl);
                if let Ty::Fn(f) = self.types.get(ty).clone() {
                    let spans = params.iter().map(|p| p.ty).chain(*result);
                    let tys = f.params.iter().map(|(_, t)| *t).chain(f.result);
                    for (span, ty) in spans.zip(tys) {
                        self.no_format(file, ast.ty(span).span, ty);
                    }
                }
            }
            DeclKind::ExternVar { ty: ty_id, .. } => {
                let ty = self.lower(file, *ty_id);
                self.no_format(file, ast.ty(*ty_id).span, ty);
            }
            DeclKind::Type { def, .. } => {
                let ty = self.named(NamedId::Decl(decl));
                self.no_format_in_type(file, def, ty);
            }
            DeclKind::Global { local, .. } => {
                match &local.binding {
                    Binding::Name { ty: ty_id, .. } => {
                        let ty = self.lower(file, *ty_id);
                        self.no_format(file, ast.ty(*ty_id).span, ty);
                        self.check(&mut fx, local.value, ty);
                    }
                    Binding::Destructure(fields) => {
                        let value = self.value(&mut fx, local.value, None);
                        let span = ast.expr(local.value).span;
                        self.destructure(&mut fx, fields, value, span, false, Owner::Own);
                    }
                }
                self.constant(file, local.value);
            }
        }
    }

    fn bind(&mut self, fx: &mut Fx, name: Name, ty: TyId, mutable: bool, owner: Owner) {
        if let Some(&id) = self.names.files[fx.file.0 as usize]
            .bindings
            .get(&name.span.start)
        {
            let loop_depth = fx.loops.len();
            self.files[fx.file.0 as usize].locals.insert(id, ty);
            fx.locals.insert(
                id,
                LocalInfo {
                    ty,
                    mutable,
                    owner,
                    loop_depth,
                },
            );
        }
    }

    /// Checks a destructuring against a record value's type, and binds
    /// the fields that have names.
    fn destructure(
        &mut self,
        fx: &mut Fx,
        fields: &[DestructField],
        value: TyId,
        span: Span,
        mutable: bool,
        owner: Owner,
    ) {
        if self.types.is_error(value) {
            return;
        }
        let Some(record) = self.record_fields(value, fx.file) else {
            let message = format!(
                "only a record can be destructured, not `{}`",
                self.show(value)
            );
            self.error(fx.file, span, message);
            return;
        };
        for field in fields {
            let name = self.text(fx.file, field.field.span);
            let Some((_, field_ty, visible)) = record.iter().find(|(n, _, _)| n == name) else {
                let message = format!("`{}` has no field `{name}`", self.show(value));
                self.error(fx.file, field.field.span, message);
                continue;
            };
            if !visible {
                let message = format!(
                    "field `{name}` of `{}` is private to its package",
                    self.show(value)
                );
                self.error(fx.file, field.field.span, message);
            }
            if let Some(ty) = field.ty {
                let declared = self.lower(fx.file, ty);
                self.no_format(fx.file, fx.ast.ty(ty).span, declared);
                if !self.types.is_error(declared) && declared != *field_ty {
                    let message = format!(
                        "field `{name}` is `{}`, not `{}`",
                        self.show(*field_ty),
                        self.show(declared)
                    );
                    self.error(fx.file, fx.ast.ty(ty).span, message);
                }
                self.bind(
                    fx,
                    field.rename.unwrap_or(field.field),
                    *field_ty,
                    mutable,
                    owner,
                );
            }
        }
    }

    // Statements. Each returns whether it never finishes normally.

    fn block(&mut self, fx: &mut Fx, block: &Block) -> bool {
        let deferred_before: HashSet<LocalId> = fx.deferred.keys().copied().collect();
        let mut diverges = false;
        for &stmt in &block.stmts {
            diverges |= self.stmt(fx, stmt);
        }
        // This block's `defer`s run at its end, and move what they sink.
        let ended: Vec<LocalId> = fx
            .deferred
            .keys()
            .filter(|id| !deferred_before.contains(id))
            .copied()
            .collect();
        for id in ended {
            let span = fx.deferred.remove(&id).expect("listed above");
            if !diverges {
                fx.moved.entry(id).or_insert(span);
            }
        }
        diverges
    }

    fn stmt(&mut self, fx: &mut Fx, id: StmtId) -> bool {
        let span = fx.ast.stmt(id).span;
        match &fx.ast.stmt(id).kind {
            StmtKind::Local(local) => match &local.binding {
                Binding::Name { name, ty: ty_id } => {
                    let ty = self.lower(fx.file, *ty_id);
                    self.no_format(fx.file, fx.ast.ty(*ty_id).span, ty);
                    let value = self.check(fx, local.value, ty);
                    self.consume(fx, local.value, ty);
                    if let Some(name) = name {
                        self.bind(fx, *name, ty, local.mutable, Owner::Own);
                    }
                    matches!(self.types.get(value), Ty::Never)
                }
                Binding::Destructure(fields) => {
                    let value = self.value(fx, local.value, None);
                    self.consume(fx, local.value, value);
                    let span = fx.ast.expr(local.value).span;
                    self.destructure(fx, fields, value, span, local.mutable, Owner::Own);
                    false
                }
            },
            StmtKind::Return(value) => {
                self.return_stmt(fx, *value, span);
                true
            }
            StmtKind::If(if_stmt) => {
                let bool = self.types.bool();
                self.check(fx, if_stmt.cond, bool);
                let before = fx.moved.clone();
                let then = self.block(fx, &if_stmt.then);
                let after_then = std::mem::replace(&mut fx.moved, before.clone());
                let otherwise = match &if_stmt.otherwise {
                    Some(Else::Block(block)) => self.block(fx, block),
                    Some(Else::If(stmt)) => self.stmt(fx, *stmt),
                    None => false,
                };
                let after_else = std::mem::take(&mut fx.moved);
                fx.moved = moves::join(vec![(after_then, then), (after_else, otherwise)], before);
                then && otherwise
            }
            StmtKind::Match { value, arms } => {
                let scrutinee = self.value(fx, *value, None);
                let owner = self.payload_owner(fx, *value);
                let mut coverage = Coverage::default();
                let before = fx.moved.clone();
                let mut paths = Vec::new();
                for arm in arms {
                    fx.moved.clone_from(&before);
                    self.patterns(fx, &arm.patterns, scrutinee, &mut coverage, owner);
                    let diverges = self.block(fx, &arm.body);
                    paths.push((std::mem::take(&mut fx.moved), diverges));
                }
                let all = paths.iter().all(|(_, d)| *d);
                fx.moved = moves::join(paths, before);
                let span = fx.ast.expr(*value).span;
                self.exhaustive(fx, span, scrutinee, &coverage);
                all
            }
            StmtKind::For(for_loop) => {
                let bool = self.types.bool();
                match &for_loop.kind {
                    ForKind::Forever => {}
                    ForKind::While(cond) => {
                        self.check(fx, *cond, bool);
                    }
                    ForKind::In {
                        mutable,
                        binding,
                        iterable,
                    } => self.for_in(fx, *mutable, binding, iterable),
                }
                let forever = matches!(for_loop.kind, ForKind::Forever);
                self.loop_body(fx, &for_loop.body, forever)
            }
            StmtKind::Break => {
                match fx.loops.last_mut() {
                    Some(flow) => moves::union_into(&mut flow.breaks, &fx.moved),
                    None => self.error(fx.file, span, "`break` is only allowed inside a loop"),
                }
                true
            }
            StmtKind::Continue => {
                match fx.loops.last_mut() {
                    Some(flow) => moves::union_into(&mut flow.continues, &fx.moved),
                    None => self.error(fx.file, span, "`continue` is only allowed inside a loop"),
                }
                true
            }
            StmtKind::Defer(call) => {
                // The call runs when the block ends, so what it passes
                // `sink` stays usable until then.
                let before = fx.moved.clone();
                let item = self.expr(fx, *call, None);
                let after = std::mem::replace(&mut fx.moved, before);
                for (id, at) in after {
                    if !fx.moved.contains_key(&id) {
                        fx.deferred.insert(id, at);
                    }
                }
                if let Item::Value(ty) = item
                    && !self.types.is_error(ty)
                {
                    let message = format!(
                        "`defer` takes a call that returns nothing; this one returns `{}`",
                        self.show(ty)
                    );
                    self.error(fx.file, span, message);
                }
                false
            }
            StmtKind::Unsafe(block) => {
                let outer = std::mem::replace(&mut fx.in_unsafe, true);
                let diverges = self.block(fx, block);
                fx.in_unsafe = outer;
                diverges
            }
            StmtKind::Lock { globals, body } => {
                if !fx.locked.is_empty() {
                    self.error(
                        fx.file,
                        span,
                        "`lock` blocks do not nest; take every global in one `lock a, b`",
                    );
                }
                let outer = fx.locked.len();
                for global in globals {
                    let start = global.name.span.start;
                    if let Some(&decl) = self.names.files[fx.file.0 as usize].locks.get(&start) {
                        if fx.locked[outer..].contains(&decl) {
                            let message = format!(
                                "`{}` is already taken by this `lock`",
                                self.text(fx.file, global.name.span)
                            );
                            self.error(fx.file, global.name.span, message);
                        }
                        fx.locked.push(decl);
                    }
                }
                let diverges = self.block(fx, body);
                fx.locked.truncate(outer);
                diverges
            }
            StmtKind::Assign { left, op, right } => {
                self.assign(fx, *left, *op, *right);
                false
            }
            StmtKind::Expr(e) => match self.expr(fx, *e, None) {
                Item::Value(ty) => match self.types.get(ty) {
                    Ty::Never => true,
                    Ty::Error => false,
                    _ => {
                        let message = format!(
                            "the `{}` this returns is ignored; write `let _: {} = ...` to discard it",
                            self.show(ty),
                            self.show(ty)
                        );
                        self.error(fx.file, span, message);
                        false
                    }
                },
                _ => false,
            },
        }
    }

    fn return_stmt(&mut self, fx: &mut Fx, value: Option<ExprId>, span: Span) {
        match (fx.result, value) {
            (None, None) => {}
            (None, Some(value)) => {
                let span = fx.ast.expr(value).span;
                self.error(fx.file, span, "this function returns nothing");
            }
            (Some(result), None) => {
                let message = match self.types.get(result) {
                    Ty::Result(None, _) => "write `return ok` to succeed".to_owned(),
                    _ => format!("this function returns a `{}`", self.show(result)),
                };
                self.error(fx.file, span, message);
            }
            (Some(result), Some(value)) => {
                self.check(fx, value, result);
                self.consume(fx, value, result);
            }
        }
    }

    fn assign(&mut self, fx: &mut Fx, left: ExprId, op: AssignOp, right: ExprId) {
        // `x = ...` gives a moved local a value again; any other
        // assignment reads it first.
        let whole = paren_inner(fx, left);
        let local = match (op, &fx.ast.expr(whole).kind) {
            (AssignOp::Set, ExprKind::Name(_)) => {
                match self.names.files[fx.file.0 as usize].exprs[whole.index()] {
                    Some(Res::Local(id)) => Some(id),
                    _ => None,
                }
            }
            _ => None,
        };
        if local.is_some() {
            fx.assign_target = Some(whole);
        }
        let spine = rules::spine(fx, left);
        fx.written.extend(&spine);
        let ty = self.value(fx, left, None);
        for e in &spine {
            fx.written.remove(e);
        }
        fx.assign_target = None;
        self.require_mutable(fx, left, "assign to");
        if op == AssignOp::Set {
            self.check(fx, right, ty);
            self.consume(fx, right, ty);
            if let Some(id) = local {
                fx.moved.remove(&id);
            }
            return;
        }
        if self.types.is_error(ty) {
            self.value(fx, right, None);
            return;
        }
        let int_only = !matches!(
            op,
            AssignOp::Add | AssignOp::Sub | AssignOp::Mul | AssignOp::Div | AssignOp::Rem
        );
        let ok = self.types.as_int(ty).is_some() || (!int_only && self.is_float(ty));
        if !ok {
            let span = fx.ast.expr(left).span;
            let message = format!("this operator does not apply to `{}`", self.show(ty));
            self.error(fx.file, span, message);
        }
        if matches!(op, AssignOp::Shl | AssignOp::Shr) {
            self.shift_amount(fx, right, ty);
        } else {
            self.check(fx, right, ty);
        }
    }

    fn for_in(&mut self, fx: &mut Fx, mutable: bool, binding: &Binding, iterable: &Iterable) {
        match iterable {
            Iterable::Range { start, end } => {
                let Binding::Name { name, ty } = binding else {
                    let span = fx.ast.expr(*start).span;
                    self.error(
                        fx.file,
                        span,
                        "a range yields integers, which cannot be destructured",
                    );
                    return;
                };
                let ty = self.lower(fx.file, *ty);
                if self.types.as_int(ty).is_none() && !self.types.is_error(ty) {
                    let span = fx.ast.expr(*start).span;
                    let message = format!("a range yields integers, not `{}`", self.show(ty));
                    self.error(fx.file, span, message);
                }
                self.check(fx, *start, ty);
                self.check(fx, *end, ty);
                if let Some(name) = name {
                    // Bound inside the loop: each pass has its own.
                    fx.loops.push(LoopFlow::default());
                    self.bind(fx, *name, ty, mutable, Owner::Own);
                    fx.loops.pop();
                }
            }
            Iterable::Expr(e) => {
                let ty = self.value(fx, *e, None);
                let span = fx.ast.expr(*e).span;
                let elem = self.element_of_iterable(fx, ty, span);
                fx.loops.push(LoopFlow::default());
                match binding {
                    Binding::Name { name, ty } => {
                        let declared = self.lower(fx.file, *ty);
                        self.no_format(fx.file, fx.ast.ty(*ty).span, declared);
                        if !self.coerces(elem, declared) {
                            let message = format!(
                                "the loop yields `{}`, not `{}`",
                                self.show(elem),
                                self.show(declared)
                            );
                            self.error(fx.file, fx.ast.ty(*ty).span, message);
                        }
                        if let Some(name) = name {
                            self.bind(fx, *name, declared, mutable, Owner::Loop);
                        }
                    }
                    Binding::Destructure(fields) => {
                        self.destructure(fx, fields, elem, span, mutable, Owner::Loop);
                    }
                }
                fx.loops.pop();
            }
        }
    }

    /// What a `for` loop over `ty` yields: the elements of an array, view
    /// or growable array, or what `next` returns for an iterator type.
    fn element_of_iterable(&mut self, fx: &Fx, ty: TyId, span: Span) -> TyId {
        match self.types.get(ty) {
            Ty::Array(_, elem) | Ty::View(elem) | Ty::Growable(elem) => return *elem,
            Ty::Error => return ty,
            _ => {}
        }
        if let Ty::Named(NamedId::Decl(decl)) = self.types.get(ty) {
            let package = self.program.file(decl.file).package;
            if let Some(&next) = self.names.scopes[package.0 as usize].get("next")
                && let Item::Value(next_ty) = self.decl_item(next, "next")
                && let Ty::Fn(f) = self.types.get(next_ty)
                && f.params.as_slice() == [(Mode::Inout, ty)]
                && let Some(result) = f.result
                && let Ty::Optional(elem) = self.types.get(result)
            {
                return *elem;
            }
        }
        let shown = self.show(ty);
        self.error(
            fx.file,
            span,
            format!("`{shown}` cannot be iterated: its package has no `fn next(it: inout {shown}) -> ?T`"),
        );
        self.types.error()
    }

    // Patterns.

    fn patterns(
        &mut self,
        fx: &mut Fx,
        pats: &[PatId],
        scrutinee: TyId,
        coverage: &mut Coverage,
        owner: Owner,
    ) {
        for &pat in pats {
            self.pattern(fx, pat, scrutinee, coverage, owner);
        }
    }

    fn pattern(
        &mut self,
        fx: &mut Fx,
        pat: PatId,
        scrutinee: TyId,
        coverage: &mut Coverage,
        owner: Owner,
    ) {
        let span = fx.ast.pat(pat).span;
        if self.types.is_error(scrutinee) {
            coverage.wildcard = true;
            return;
        }
        match &fx.ast.pat(pat).kind {
            PatternKind::Discard => coverage.wildcard = true,
            PatternKind::Variant { name, payload } => {
                let text = self.text(fx.file, name.span);
                let Some((variants, _)) = self.variants_of(scrutinee) else {
                    let message = format!(
                        "`{}` has no variants; match it with literals and `_`",
                        self.show(scrutinee)
                    );
                    self.error(fx.file, name.span, message);
                    return;
                };
                let Some((_, variant_payload)) = variants.iter().find(|(n, _)| n == text) else {
                    let message =
                        format!("`{text}` is not a variant of `{}`", self.show(scrutinee));
                    self.error(fx.file, name.span, message);
                    return;
                };
                if !coverage.variants.insert(text.to_owned()) {
                    self.error(fx.file, name.span, format!("`{text}` is already matched"));
                }
                match (payload, variant_payload) {
                    (Some(fields), Some(payload)) => {
                        self.destructure(fx, fields, *payload, span, false, owner);
                    }
                    (Some(_), None) => {
                        self.error(fx.file, span, format!("`{text}` carries no payload"));
                    }
                    (None, _) => {}
                }
            }
            PatternKind::Literal(e) => {
                if self.variants_of(scrutinee).is_some() {
                    let message = format!(
                        "`{}` is matched by its variants, not by literals",
                        self.show(scrutinee)
                    );
                    self.error(fx.file, span, message);
                    return;
                }
                self.check(fx, *e, scrutinee);
            }
            PatternKind::Negative(e) => {
                let value = parse_int(self.text(fx.file, fx.ast.expr(*e).span)).map(|n| -n);
                self.int_fits(fx, span, value, scrutinee);
            }
        }
    }

    fn exhaustive(&mut self, fx: &Fx, span: Span, scrutinee: TyId, coverage: &Coverage) {
        if coverage.wildcard || self.types.is_error(scrutinee) {
            return;
        }
        match self.variants_of(scrutinee) {
            Some((_, true)) => {
                let message = format!(
                    "a match on the open enum `{}` needs a `_` arm",
                    self.show(scrutinee)
                );
                self.error(fx.file, span, message);
            }
            Some((variants, false)) => {
                let missing: Vec<String> = variants
                    .into_iter()
                    .filter(|(n, _)| !coverage.variants.contains(n))
                    .map(|(n, _)| format!("`{n}`"))
                    .collect();
                if !missing.is_empty() {
                    let message = format!("this match does not cover {}", missing.join(", "));
                    self.error(fx.file, span, message);
                }
            }
            None => {
                let message = format!("a match on `{}` needs a `_` arm", self.show(scrutinee));
                self.error(fx.file, span, message);
            }
        }
    }

    // Places.

    /// Whether `e` names storage, and if so whether it can be changed.
    fn place(&self, fx: &Fx, e: ExprId) -> Option<bool> {
        let res = self.names.files[fx.file.0 as usize].exprs[e.index()];
        match &fx.ast.expr(e).kind {
            ExprKind::Name(_) => self.res_place(fx, res),
            ExprKind::Field { value, .. } => match res {
                Some(Res::Decl(_)) => self.res_place(fx, res),
                _ => self.place(fx, *value),
            },
            ExprKind::Index { value, .. } => self.place(fx, *value),
            ExprKind::Paren(inner) => self.place(fx, *inner),
            ExprKind::Deref(_) => Some(true),
            _ => None,
        }
    }

    fn res_place(&self, fx: &Fx, res: Option<Res>) -> Option<bool> {
        match res? {
            Res::Local(id) => fx.locals.get(&id).map(|l| l.mutable),
            Res::Decl(decl) => match &self.program.file(decl.file).ast.decls[decl.index].kind {
                DeclKind::Global { local, .. } => Some(local.mutable),
                DeclKind::ExternVar { mutable, .. } => Some(*mutable),
                _ => None,
            },
            _ => None,
        }
    }

    fn require_mutable(&mut self, fx: &Fx, e: ExprId, action: &str) {
        let span = fx.ast.expr(e).span;
        match self.place(fx, e) {
            Some(true) => {}
            Some(false) => {
                let message = format!(
                    "cannot {action} `{}`: it is not a `var` or an `inout` parameter",
                    self.text(fx.file, span)
                );
                self.error(fx.file, span, message);
            }
            None => {
                let message =
                    format!("cannot {action} this: it is not a variable, field or element");
                self.error(fx.file, span, message);
            }
        }
    }

    // Expressions.

    /// Checks `e` against `expected` and returns `expected`.
    fn check(&mut self, fx: &mut Fx, e: ExprId, expected: TyId) -> TyId {
        let actual = self.value(fx, e, Some(expected));
        let span = fx.ast.expr(e).span;
        self.expect_ty(fx.file, span, actual, expected);
        if matches!(self.types.get(actual), Ty::Never) {
            actual
        } else {
            expected
        }
    }

    /// The type of `e`, which must be a value.
    fn value(&mut self, fx: &mut Fx, e: ExprId, expected: Option<TyId>) -> TyId {
        let span = fx.ast.expr(e).span;
        match self.expr(fx, e, expected) {
            Item::Value(ty) => ty,
            Item::Nothing => {
                self.error(
                    fx.file,
                    span,
                    "this call returns nothing, so it has no value",
                );
                self.types.error()
            }
            Item::Type(ty) => {
                let message = format!("`{}` is a type, not a value", self.show(ty));
                self.error(fx.file, span, message);
                self.types.error()
            }
            Item::Builtin(name) => {
                let message = match name {
                    "some" => "write `some.(value: x)`".to_owned(),
                    "err" => "write `err.(error: e)`".to_owned(),
                    _ => format!("`{name}` is a built-in function; call it"),
                };
                self.error(fx.file, span, message);
                self.types.error()
            }
            Item::Variant(ty, _) => {
                let message = format!(
                    "this variant of `{}` has a payload; build it with `.(...)`",
                    self.show(ty)
                );
                self.error(fx.file, span, message);
                self.types.error()
            }
        }
    }

    /// Whether `e` takes its type from context: literals, `none`, `ok`,
    /// `some.(...)` and array literals.
    fn needs_context(&self, fx: &Fx, e: ExprId) -> bool {
        let res = |e: ExprId| self.names.files[fx.file.0 as usize].exprs[e.index()];
        match &fx.ast.expr(e).kind {
            ExprKind::Int | ExprKind::Float | ExprKind::Char | ExprKind::Array(_) => true,
            ExprKind::Paren(inner)
            | ExprKind::Unary {
                op: UnaryOp::Neg | UnaryOp::BitNot,
                operand: inner,
            } => self.needs_context(fx, *inner),
            ExprKind::Name(_) => matches!(res(e), Some(Res::Builtin("none" | "ok"))),
            ExprKind::Construct { ty, .. } => {
                matches!(res(*ty), Some(Res::Builtin("some" | "ok" | "err")))
            }
            _ => false,
        }
    }

    fn expr(&mut self, fx: &mut Fx, e: ExprId, expected: Option<TyId>) -> Item {
        let item = self.expr_item(fx, e, expected);
        self.files[fx.file.0 as usize].exprs[e.index()] = Some(item);
        item
    }

    fn expr_item(&mut self, fx: &mut Fx, e: ExprId, expected: Option<TyId>) -> Item {
        let span = fx.ast.expr(e).span;
        match &fx.ast.expr(e).kind {
            ExprKind::Name(name) => self.name(fx, e, *name, expected),
            ExprKind::Int => Item::Value(self.int_literal(fx, span, false, expected)),
            ExprKind::Char => {
                let value = char_value(self.text(fx.file, span));
                Item::Value(self.int_with_value(fx, span, value, expected, "a character literal"))
            }
            ExprKind::Float => match expected {
                Some(ty) if self.is_float(ty) || self.types.is_error(ty) => Item::Value(ty),
                Some(ty) => {
                    let message = format!("expected `{}`, found a float literal", self.show(ty));
                    self.error(fx.file, span, message);
                    Item::Value(self.types.error())
                }
                None => {
                    self.unknown_literal(fx, span, "float");
                    Item::Value(self.types.error())
                }
            },
            ExprKind::Str(parts) => Item::Value(self.string(fx, parts, expected)),
            ExprKind::Paren(inner) => self.expr(fx, *inner, expected),
            ExprKind::Unsafe(inner) => {
                let outer = std::mem::replace(&mut fx.in_unsafe, true);
                let ty = self.value(fx, *inner, expected);
                fx.in_unsafe = outer;
                Item::Value(ty)
            }
            ExprKind::Tuple(fields) => Item::Value(self.tuple(fx, span, fields, expected)),
            ExprKind::Call { callee, args } => self.call(fx, span, *callee, args, expected),
            ExprKind::Field { value, name } => self.field(fx, e, *value, *name),
            ExprKind::Construct { ty, value } => self.construct(fx, span, *ty, *value, expected),
            ExprKind::Index { value, index } => Item::Value(self.index(fx, *value, index)),
            ExprKind::Deref(pointer) => {
                self.require_unsafe(fx, span, "reading or writing through a pointer with `.*`");
                let ty = self.value(fx, *pointer, None);
                match self.types.get(ty) {
                    Ty::Pointer(elem) => Item::Value(*elem),
                    Ty::Error => Item::Value(ty),
                    _ => {
                        let message =
                            format!("`.*` reads through a pointer, not a `{}`", self.show(ty));
                        self.error(fx.file, span, message);
                        Item::Value(self.types.error())
                    }
                }
            }
            ExprKind::Unary { op, operand } => {
                Item::Value(self.unary(fx, span, *op, *operand, expected))
            }
            ExprKind::Try(operand) => self.try_expr(fx, span, *operand),
            ExprKind::Binary { op, left, right } => {
                Item::Value(self.binary(fx, span, *op, *left, *right, expected))
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                let bool = self.types.bool();
                self.check(fx, *cond, bool);
                let other = match otherwise {
                    IfElse::Branch(e) | IfElse::If(e) => *e,
                };
                Item::Value(self.branches(fx, &[*then, other], expected))
            }
            ExprKind::Match { value, arms } => {
                let scrutinee = self.value(fx, *value, None);
                let owner = self.payload_owner(fx, *value);
                let mut coverage = Coverage::default();
                for arm in arms {
                    self.patterns(fx, &arm.patterns, scrutinee, &mut coverage, owner);
                }
                let value_span = fx.ast.expr(*value).span;
                self.exhaustive(fx, value_span, scrutinee, &coverage);
                let bodies: Vec<ExprId> = arms.iter().map(|a| a.body).collect();
                Item::Value(self.branches(fx, &bodies, expected))
            }
            ExprKind::Array(elems) => Item::Value(self.array(fx, span, elems, expected)),
        }
    }

    /// The common type of `if` or `match` branches.
    fn branches(&mut self, fx: &mut Fx, bodies: &[ExprId], expected: Option<TyId>) -> TyId {
        if let Some(expected) = expected {
            for &body in bodies {
                self.check(fx, body, expected);
            }
            return expected;
        }
        let mut result: Option<TyId> = None;
        for &body in bodies {
            match result {
                Some(ty) => {
                    self.check(fx, body, ty);
                }
                None => {
                    let ty = self.value(fx, body, None);
                    if !matches!(self.types.get(ty), Ty::Never) {
                        result = Some(ty);
                    }
                }
            }
        }
        result.unwrap_or_else(|| self.types.never())
    }

    fn name(&mut self, fx: &mut Fx, e: ExprId, name: Name, expected: Option<TyId>) -> Item {
        match self.names.files[fx.file.0 as usize].exprs[e.index()] {
            Some(Res::Local(id)) => {
                self.use_local(fx, e, id, name.span);
                Item::Value(
                    fx.locals
                        .get(&id)
                        .map_or_else(|| self.types.error(), |l| l.ty),
                )
            }
            Some(Res::Decl(decl)) => {
                self.use_decl(fx, e, decl, name.span);
                let text = self.text(fx.file, name.span);
                self.decl_item(decl, text)
            }
            Some(Res::Builtin(builtin)) => self.builtin_name(fx, name.span, builtin, expected),
            Some(Res::Package(_)) | None => Item::Value(self.types.error()),
        }
    }

    fn builtin_name(
        &mut self,
        fx: &Fx,
        span: Span,
        name: &'static str,
        expected: Option<TyId>,
    ) -> Item {
        if BUILTIN_TYPES.contains(&name) {
            return Item::Type(self.builtin_type(name));
        }
        match name {
            "true" | "false" => Item::Value(self.types.bool()),
            "none" => match expected.map(|t| (t, self.types.get(t))) {
                Some((ty, Ty::Optional(_) | Ty::Error)) => Item::Value(ty),
                _ => {
                    self.error(
                        fx.file,
                        span,
                        "`none` needs a place that expects an optional",
                    );
                    Item::Value(self.types.error())
                }
            },
            "ok" => match expected.map(|t| (t, self.types.get(t))) {
                Some((ty, Ty::Result(None, _) | Ty::Error)) => Item::Value(ty),
                Some((_, Ty::Result(Some(_), _))) => {
                    self.error(
                        fx.file,
                        span,
                        "this result carries a value: write `ok.(value: x)`",
                    );
                    Item::Value(self.types.error())
                }
                _ => {
                    self.error(fx.file, span, "`ok` needs a place that expects a result");
                    Item::Value(self.types.error())
                }
            },
            _ => Item::Builtin(name),
        }
    }

    fn unknown_literal(&mut self, fx: &Fx, span: Span, what: &str) {
        self.error(
            fx.file,
            span,
            format!("the type of this {what} literal is unknown; convert it, as in `u32.(1)`"),
        );
    }

    fn int_literal(&mut self, fx: &Fx, span: Span, negative: bool, expected: Option<TyId>) -> TyId {
        let value = parse_int(self.text(fx.file, span)).map(|n| if negative { -n } else { n });
        self.int_with_value(fx, span, value, expected, "an integer literal")
    }

    fn int_with_value(
        &mut self,
        fx: &Fx,
        span: Span,
        value: Option<i128>,
        expected: Option<TyId>,
        what: &str,
    ) -> TyId {
        let Some(expected) = expected else {
            self.unknown_literal(fx, span, "integer");
            return self.types.error();
        };
        if self.types.is_error(expected) {
            return expected;
        }
        if self.int_core(expected).is_none() {
            let message = format!("expected `{}`, found {what}", self.show(expected));
            self.error(fx.file, span, message);
            return self.types.error();
        }
        self.int_fits(fx, span, value, expected);
        expected
    }

    fn int_fits(&mut self, fx: &Fx, span: Span, value: Option<i128>, ty: TyId) {
        let Some(int) = self.int_core(ty) else {
            if !self.types.is_error(ty) {
                let message = format!("expected `{}`, found an integer", self.show(ty));
                self.error(fx.file, span, message);
            }
            return;
        };
        match value {
            Some(n) if n >= int.min() && n <= int.max() => {}
            Some(n) => {
                let message = format!("`{n}` does not fit in `{}`", self.show(ty));
                self.error(fx.file, span, message);
            }
            None => {
                let message = format!("this literal does not fit in `{}`", self.show(ty));
                self.error(fx.file, span, message);
            }
        }
    }

    fn string(&mut self, fx: &mut Fx, parts: &[StrPart], expected: Option<TyId>) -> TyId {
        let mut interpolated = false;
        for part in parts {
            if let StrPart::Interpolation(e) = part {
                interpolated = true;
                let ty = self.value(fx, *e, None);
                let part = self.fmt_part().map(|p| self.named(NamedId::Decl(p)));
                let bytes = self.types.bytes();
                let ok = self.types.as_int(ty).is_some()
                    || matches!(self.types.get(ty), Ty::Bool | Ty::Error | Ty::Never)
                    || ty == bytes
                    || self.is_named(ty, BuiltinNamed::Format)
                    || Some(ty) == part;
                if !ok {
                    let span = fx.ast.expr(*e).span;
                    let message = format!(
                        "`{}` cannot be interpolated; `{{}}` takes integers, `bool`, `[]u8`, a `Format` and `fmt.Part`",
                        self.show(ty)
                    );
                    self.error(fx.file, span, message);
                }
            }
        }
        let format = self.named(NamedId::Builtin(BuiltinNamed::Format));
        if interpolated || expected == Some(format) {
            format
        } else {
            self.types.bytes()
        }
    }

    fn tuple(
        &mut self,
        fx: &mut Fx,
        span: Span,
        fields: &[(Name, ExprId)],
        expected: Option<TyId>,
    ) -> TyId {
        if let Some(expected) = expected
            && let Ty::Tuple(want) = self.types.get(expected).clone()
        {
            let names: Vec<&str> = fields
                .iter()
                .map(|(n, _)| self.text(fx.file, n.span))
                .collect();
            let want_names: Vec<&str> = want.iter().map(|(n, _)| n.as_str()).collect();
            if names != want_names {
                let message = format!(
                    "expected the fields ({}), found ({})",
                    want_names.join(", "),
                    names.join(", ")
                );
                self.error(fx.file, span, message);
                return self.types.error();
            }
            for ((_, value), (_, ty)) in fields.iter().zip(&want) {
                self.check(fx, *value, *ty);
                self.consume(fx, *value, *ty);
            }
            return expected;
        }
        let fields: Vec<(String, TyId)> = fields
            .iter()
            .map(|(name, value)| {
                let ty = self.value(fx, *value, None);
                self.consume(fx, *value, ty);
                (self.text(fx.file, name.span).to_owned(), ty)
            })
            .collect();
        self.types.intern(Ty::Tuple(fields))
    }

    fn array(&mut self, fx: &mut Fx, span: Span, elems: &[ExprId], expected: Option<TyId>) -> TyId {
        let Some(expected) = expected else {
            self.error(fx.file, span, "the type of this array literal is unknown");
            return self.types.error();
        };
        match self.types.get(expected).clone() {
            Ty::Array(len, elem) => {
                for &e in elems {
                    self.check(fx, e, elem);
                    self.consume(fx, e, elem);
                }
                if elems.len() as u64 != len {
                    let message = format!("expected {len} elements, found {}", elems.len());
                    self.error(fx.file, span, message);
                }
                expected
            }
            Ty::Error => expected,
            _ => {
                let message = format!("expected `{}`, found an array literal", self.show(expected));
                self.error(fx.file, span, message);
                self.types.error()
            }
        }
    }

    fn field(&mut self, fx: &mut Fx, e: ExprId, value: ExprId, name: Name) -> Item {
        let text = self.text(fx.file, name.span);
        if let Some(Res::Decl(decl)) = self.names.files[fx.file.0 as usize].exprs[e.index()] {
            let span = fx.ast.expr(e).span;
            self.use_decl(fx, e, decl, span);
            return self.decl_item(decl, text);
        }
        let base = match self.expr(fx, value, None) {
            Item::Type(ty) => return self.variant(fx, ty, name),
            Item::Value(ty) => ty,
            _ => self.value(fx, value, None),
        };
        match self.types.get(base) {
            Ty::Error => return Item::Value(base),
            Ty::Array(..) | Ty::View(_) | Ty::Growable(_) if text == "len" => {
                return Item::Value(self.types.usize());
            }
            Ty::Pointer(_) => {
                self.error(
                    fx.file,
                    name.span,
                    format!("read through the pointer first: `.*.{text}`"),
                );
                return Item::Value(self.types.error());
            }
            _ => {}
        }
        let field = self
            .record_fields(base, fx.file)
            .and_then(|fields| fields.into_iter().find(|(n, _, _)| n == text));
        match field {
            Some((_, ty, visible)) => {
                if !visible {
                    let message = format!(
                        "field `{text}` of `{}` is private to its package",
                        self.show(base)
                    );
                    self.error(fx.file, name.span, message);
                }
                let union = matches!(
                    self.named_info(base),
                    Some(NamedInfo {
                        def: NamedDef::Union(_),
                        ..
                    })
                );
                if union && !fx.written.contains(&e) && !fx.addressed.contains(&e) {
                    let what = format!("reading the union member `{text}`");
                    self.require_unsafe(fx, name.span, &what);
                }
                Item::Value(ty)
            }
            None => {
                let message = format!("`{}` has no field `{text}`", self.show(base));
                self.error(fx.file, name.span, message);
                Item::Value(self.types.error())
            }
        }
    }

    /// `T.name`: a variant of the enum `T`.
    fn variant(&mut self, fx: &Fx, ty: TyId, name: Name) -> Item {
        let text = self.text(fx.file, name.span);
        if let Some(NamedInfo {
            def: NamedDef::Enum(info),
            ..
        }) = self.named_info(ty)
        {
            if let Some(index) = info.variants.iter().position(|v| v.name == text) {
                return if info.variants[index].payload.is_some() {
                    Item::Variant(ty, index)
                } else {
                    Item::Value(ty)
                };
            }
            let message = format!("`{}` has no variant `{text}`", self.show(ty));
            self.error(fx.file, name.span, message);
        } else if !self.types.is_error(ty) {
            let message = format!(
                "`{}` is not an enum, so it has no variant `{text}`",
                self.show(ty)
            );
            self.error(fx.file, name.span, message);
        }
        Item::Value(self.types.error())
    }

    /// `T.(value)`: construction, wrapping and numeric conversion.
    fn construct(
        &mut self,
        fx: &mut Fx,
        span: Span,
        target: ExprId,
        value: ExprId,
        expected: Option<TyId>,
    ) -> Item {
        let target_item = match self.names.files[fx.file.0 as usize].exprs[target.index()] {
            Some(Res::Builtin(name @ ("some" | "ok" | "err"))) => Item::Builtin(name),
            _ => self.expr(fx, target, None),
        };
        let ty = match target_item {
            Item::Type(ty) => ty,
            Item::Variant(ty, index) => {
                let payload = match self.named_info(ty) {
                    Some(NamedInfo {
                        def: NamedDef::Enum(info),
                        ..
                    }) => info.variants[index].payload,
                    _ => None,
                };
                if let Some(payload) = payload {
                    self.construct_value(fx, value, payload);
                }
                return Item::Value(ty);
            }
            Item::Builtin(name @ ("some" | "ok" | "err")) => {
                return Item::Value(self.construct_builtin(fx, span, name, value, expected));
            }
            Item::Value(ty) if self.types.is_error(ty) => {
                self.value(fx, value, None);
                return Item::Value(ty);
            }
            _ => {
                self.error(
                    fx.file,
                    span,
                    "only a type or a variant is built with `.(...)`",
                );
                return Item::Value(self.types.error());
            }
        };
        match self.types.get(ty).clone() {
            Ty::Int(_) | Ty::Float(_) => {
                self.convert_number(fx, value, ty);
                Item::Value(ty)
            }
            Ty::Named(_) => {
                let def = self.named_info(ty).map(|i| match &i.def {
                    NamedDef::Plain { underlying, .. } => Ok(*underlying),
                    NamedDef::Enum(info) => Err(info.int.is_some()),
                    NamedDef::Union(_) => Err(false),
                });
                match def {
                    Some(Ok(underlying)) => {
                        if let Some(fields) = self.record_fields(ty, fx.file)
                            && let Some((name, _, _)) =
                                fields.iter().find(|(_, _, visible)| !visible)
                        {
                            let message = format!(
                                "`{}` cannot be built outside its package: field `{name}` is private",
                                self.show(ty)
                            );
                            self.error(fx.file, span, message);
                        }
                        self.construct_value(fx, value, underlying);
                    }
                    Some(Err(true)) => {
                        // A checked conversion from an integer.
                        let inner = paren_inner(fx, value);
                        let actual = self.value(fx, inner, None);
                        if self.int_core(actual).is_none() && !self.types.is_error(actual) {
                            let message =
                                format!("`{}` converts only from an integer", self.show(ty));
                            self.error(fx.file, span, message);
                        }
                    }
                    _ => {
                        let message = format!(
                            "`{}` is built from a variant, as in `{}.name`",
                            self.show(ty),
                            self.show(ty)
                        );
                        self.error(fx.file, span, message);
                    }
                }
                Item::Value(ty)
            }
            Ty::Error => Item::Value(ty),
            _ => {
                let message = format!("`{}` cannot be built with `.(...)`", self.show(ty));
                self.error(fx.file, span, message);
                Item::Value(self.types.error())
            }
        }
    }

    /// Checks the `(...)` of a construction against the underlying type:
    /// a tuple literal for a record, or one value in parentheses.
    fn construct_value(&mut self, fx: &mut Fx, value: ExprId, underlying: TyId) {
        self.check(fx, value, underlying);
        self.consume(fx, value, underlying);
    }

    fn construct_builtin(
        &mut self,
        fx: &mut Fx,
        span: Span,
        name: &str,
        value: ExprId,
        expected: Option<TyId>,
    ) -> TyId {
        let fits = match (name, expected.map(|t| self.types.get(t).clone())) {
            ("some", Some(Ty::Optional(v))) | ("ok", Some(Ty::Result(Some(v), _))) => {
                Some(("value", v))
            }
            ("err", Some(Ty::Result(_, e))) => Some(("error", e)),
            (_, Some(Ty::Error)) => {
                self.value(fx, value, None);
                return self.types.error();
            }
            _ => None,
        };
        let Some((field, ty)) = fits else {
            let what = if name == "some" {
                "an optional"
            } else {
                "a result"
            };
            let message = match expected {
                Some(t) => format!(
                    "`{name}` builds {what}, but `{}` is expected here",
                    self.show(t)
                ),
                None => format!("`{name}` needs a place that expects {what}"),
            };
            self.error(fx.file, span, message);
            self.value(fx, value, None);
            return self.types.error();
        };
        let payload = self.types.intern(Ty::Tuple(vec![(field.to_owned(), ty)]));
        self.check(fx, value, payload);
        expected.expect("checked above")
    }

    /// `u64.(x)`: a numeric conversion. It checks at run time that the
    /// value fits, except `truncate` which never checks.
    fn convert_number(&mut self, fx: &mut Fx, value: ExprId, target: TyId) {
        let inner = paren_inner(fx, value);
        if self.needs_context(fx, inner) {
            self.check(fx, inner, target);
            return;
        }
        let actual = self.value(fx, inner, None);
        let float_newtype = matches!(
            self.named_info(actual),
            Some(NamedInfo { def: NamedDef::Plain { underlying, .. }, .. }) if self.is_float(*underlying)
        );
        let numeric = self.int_core(actual).is_some()
            || self.is_float(actual)
            || float_newtype
            || matches!(
                self.named_info(actual),
                Some(NamedInfo {
                    def: NamedDef::Enum(EnumInfo { int: Some(_), .. }),
                    ..
                })
            )
            || self.types.is_error(actual);
        if !numeric {
            let span = fx.ast.expr(inner).span;
            let message = format!(
                "cannot convert `{}` to `{}`",
                self.show(actual),
                self.show(target)
            );
            self.error(fx.file, span, message);
        }
    }

    fn index(&mut self, fx: &mut Fx, value: ExprId, index: &Index) -> TyId {
        let base = self.value(fx, value, None);
        let elem = match self.types.get(base) {
            Ty::Array(_, elem) | Ty::View(elem) | Ty::Growable(elem) => *elem,
            Ty::Error => base,
            _ => {
                let span = fx.ast.expr(value).span;
                let message = format!("`{}` cannot be indexed", self.show(base));
                self.error(fx.file, span, message);
                self.types.error()
            }
        };
        let usize = self.types.usize();
        match index {
            Index::Expr(i) => {
                self.check(fx, *i, usize);
                elem
            }
            Index::Slice { start, end } => {
                for e in start.iter().chain(end) {
                    self.check(fx, *e, usize);
                }
                if self.types.is_error(elem) {
                    elem
                } else {
                    self.types.intern(Ty::View(elem))
                }
            }
            Index::Type(ty) => {
                let span = fx.ast.ty(*ty).span;
                self.error(
                    fx.file,
                    span,
                    "a type argument goes only on a built-in function, as in `truncate[u8](x)`",
                );
                self.types.error()
            }
        }
    }

    fn unary(
        &mut self,
        fx: &mut Fx,
        span: Span,
        op: UnaryOp,
        operand: ExprId,
        expected: Option<TyId>,
    ) -> TyId {
        match op {
            UnaryOp::Neg => {
                if matches!(fx.ast.expr(operand).kind, ExprKind::Int) {
                    let operand_span = fx.ast.expr(operand).span;
                    let value = parse_int(self.text(fx.file, operand_span)).map(|n| -n);
                    return self.int_with_value(fx, span, value, expected, "an integer literal");
                }
                let ty = self.value(fx, operand, expected);
                let ok = self.types.as_int(ty).is_some_and(IntTy::signed)
                    || self.is_float(ty)
                    || self.types.is_error(ty);
                if !ok {
                    let message = format!(
                        "`-` applies to signed integers and floats, not `{}`",
                        self.show(ty)
                    );
                    self.error(fx.file, span, message);
                }
                ty
            }
            UnaryOp::Not => {
                let bool = self.types.bool();
                self.check(fx, operand, bool)
            }
            UnaryOp::BitNot => {
                let ty = self.value(fx, operand, expected);
                if self.types.as_int(ty).is_none() && !self.types.is_error(ty) {
                    let message = format!("`~` applies to integers, not `{}`", self.show(ty));
                    self.error(fx.file, span, message);
                }
                ty
            }
            UnaryOp::AddressOf => {
                let inner_expected = expected.and_then(|t| match self.types.get(t) {
                    Ty::Pointer(elem) => Some(*elem),
                    _ => None,
                });
                let spine = rules::spine(fx, operand);
                fx.addressed.extend(&spine);
                let ty = self.value(fx, operand, inner_expected);
                for e in &spine {
                    fx.addressed.remove(e);
                }
                if self.place(fx, operand).is_none() {
                    self.error(fx.file, span, "`&` takes a variable, field or element");
                }
                self.types.intern(Ty::Pointer(ty))
            }
        }
    }

    fn try_expr(&mut self, fx: &mut Fx, span: Span, operand: ExprId) -> Item {
        let ty = self.value(fx, operand, None);
        let result = fx.result.map(|r| self.types.get(r).clone());
        match self.types.get(ty).clone() {
            Ty::Optional(value) => {
                if !matches!(result, Some(Ty::Optional(_))) {
                    self.error(
                        fx.file,
                        span,
                        "`try` on an optional needs a function that returns an optional",
                    );
                }
                Item::Value(value)
            }
            Ty::Result(value, error) => {
                match result {
                    Some(Ty::Result(_, own)) if own == error => {}
                    _ => {
                        let returns = fx.result.map_or_else(
                            || "nothing".to_owned(),
                            |r| format!("`{}`", self.show(r)),
                        );
                        let message = format!(
                            "`try` passes up a `{}` error, but this function returns {returns}",
                            self.show(error)
                        );
                        self.error(fx.file, span, message);
                    }
                }
                value.map_or(Item::Nothing, Item::Value)
            }
            Ty::Error => Item::Value(ty),
            _ => {
                let message = format!(
                    "`try` takes an optional or a result, not `{}`",
                    self.show(ty)
                );
                self.error(fx.file, span, message);
                Item::Value(self.types.error())
            }
        }
    }

    /// The type of two operands that must match. A literal takes the type
    /// of the other side.
    fn operands(
        &mut self,
        fx: &mut Fx,
        left: ExprId,
        right: ExprId,
        expected: Option<TyId>,
    ) -> TyId {
        if self.needs_context(fx, left) && !self.needs_context(fx, right) {
            let ty = self.value(fx, right, expected);
            self.check(fx, left, ty);
            ty
        } else {
            let ty = self.value(fx, left, expected);
            self.check(fx, right, ty);
            ty
        }
    }

    fn shift_amount(&mut self, fx: &mut Fx, amount: ExprId, shifted: TyId) {
        if self.needs_context(fx, amount) {
            self.check(fx, amount, shifted);
            return;
        }
        let ty = self.value(fx, amount, None);
        if self.types.as_int(ty).is_none() && !self.types.is_error(ty) {
            let span = fx.ast.expr(amount).span;
            let message = format!("a shift amount is an integer, not `{}`", self.show(ty));
            self.error(fx.file, span, message);
        }
    }

    fn binary(
        &mut self,
        fx: &mut Fx,
        span: Span,
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
        expected: Option<TyId>,
    ) -> TyId {
        let bool = self.types.bool();
        match op.group() {
            OpGroup::And | OpGroup::Or => {
                self.check(fx, left, bool);
                self.check(fx, right, bool);
                bool
            }
            OpGroup::Comparison => {
                let ty = self.operands(fx, left, right, None);
                let equality = matches!(op, BinaryOp::Eq | BinaryOp::Ne);
                let ok = match self.types.get(ty) {
                    Ty::Int(_) | Ty::Float(_) | Ty::Error | Ty::Never => true,
                    Ty::Bool | Ty::Pointer(_) => equality,
                    Ty::Named(_) => {
                        equality
                            && matches!(
                                self.named_info(ty),
                                Some(NamedInfo { def: NamedDef::Enum(info), .. })
                                    if info.variants.iter().all(|v| v.payload.is_none())
                            )
                    }
                    _ => false,
                };
                if !ok {
                    let message = format!("`{}` does not apply to `{}`", op.text(), self.show(ty));
                    self.error(fx.file, span, message);
                }
                bool
            }
            OpGroup::Shift => {
                let ty = self.value(fx, left, expected);
                self.require_int(fx, span, op, ty);
                self.shift_amount(fx, right, ty);
                ty
            }
            OpGroup::BitAnd | OpGroup::BitOr | OpGroup::BitXor => {
                let ty = self.operands(fx, left, right, expected);
                self.require_int(fx, span, op, ty);
                ty
            }
            OpGroup::Additive | OpGroup::Multiplicative => {
                let ty = self.operands(fx, left, right, expected);
                let ok = matches!(
                    self.types.get(ty),
                    Ty::Int(_) | Ty::Float(_) | Ty::Error | Ty::Never
                );
                if !ok {
                    let message = format!("`{}` does not apply to `{}`", op.text(), self.show(ty));
                    self.error(fx.file, span, message);
                }
                ty
            }
        }
    }

    fn require_int(&mut self, fx: &Fx, span: Span, op: BinaryOp, ty: TyId) {
        if !matches!(self.types.get(ty), Ty::Int(_) | Ty::Error | Ty::Never) {
            let message = format!(
                "`{}` applies to built-in integers, not `{}`",
                op.text(),
                self.show(ty)
            );
            self.error(fx.file, span, message);
        }
    }

    // Calls.

    fn call(
        &mut self,
        fx: &mut Fx,
        span: Span,
        callee: ExprId,
        args: &[Arg],
        expected: Option<TyId>,
    ) -> Item {
        // A built-in with a type argument: truncate[u8](x).
        if let ExprKind::Index { value, index } = &fx.ast.expr(callee).kind
            && let Some(Res::Builtin(name)) =
                self.names.files[fx.file.0 as usize].exprs[value.index()]
            && !BUILTIN_TYPES.contains(&name)
        {
            return self.builtin_call(fx, span, name, Some(index), args, expected);
        }
        self.unsafe_callee(fx, callee);
        match self.expr(fx, callee, None) {
            Item::Builtin(name) => self.builtin_call(fx, span, name, None, args, expected),
            Item::Value(ty) => match self.types.get(self.fn_underlying(ty).unwrap_or(ty)).clone() {
                Ty::Fn(f) => {
                    let modes: Vec<(ExprId, Mode)> = args
                        .iter()
                        .zip(&f.params)
                        .map(|(arg, (mode, _))| (arg.value, *mode))
                        .collect();
                    self.exclusive(fx, &modes);
                    if f.params.len() != args.len() {
                        let message = format!(
                            "expected {} arguments, found {}",
                            f.params.len(),
                            args.len()
                        );
                        self.error(fx.file, span, message);
                    }
                    for (arg, (mode, param)) in args.iter().zip(&f.params) {
                        self.argument(fx, arg, *mode, *param);
                    }
                    for arg in args.iter().skip(f.params.len()) {
                        self.value(fx, arg.value, None);
                    }
                    f.result.map_or(Item::Nothing, Item::Value)
                }
                Ty::Error => {
                    for arg in args {
                        self.value(fx, arg.value, None);
                    }
                    Item::Value(ty)
                }
                _ => {
                    let message = format!("`{}` is not a function", self.show(ty));
                    self.error(fx.file, span, message);
                    Item::Value(self.types.error())
                }
            },
            Item::Type(ty) => {
                let shown = self.show(ty);
                self.error(
                    fx.file,
                    span,
                    format!("build a `{shown}` with `{shown}.(...)`, not a call"),
                );
                Item::Value(self.types.error())
            }
            Item::Variant(ty, _) => Item::Value(ty),
            Item::Nothing => Item::Value(self.types.error()),
        }
    }

    fn argument(&mut self, fx: &mut Fx, arg: &Arg, mode: Mode, param: TyId) {
        let span = fx.ast.expr(arg.value).span;
        if arg.mode != mode {
            let message = match (mode, arg.mode) {
                (Mode::Inout, _) => "this parameter is `inout`; write `inout` before the argument",
                (Mode::Sink, _) => "this parameter is `sink`; write `sink` before the argument",
                (Mode::Let, Mode::Inout) => "this parameter is not `inout`; remove `inout`",
                (Mode::Let, _) => "this parameter is not `sink`; remove `sink`",
            };
            self.error(fx.file, span, message);
        }
        if mode == Mode::Inout {
            let ty = self.value(fx, arg.value, None);
            if !self.types.is_error(ty) && !self.types.is_error(param) && ty != param {
                let message = format!("expected `{}`, found `{}`", self.show(param), self.show(ty));
                self.error(fx.file, span, message);
            }
            self.require_mutable(fx, arg.value, "pass `inout`");
        } else {
            self.check(fx, arg.value, param);
            if mode == Mode::Sink {
                self.sink(fx, arg.value, param);
            }
        }
    }

    /// A type argument: `u8` in `truncate[u8]`, `*u32` in `from_address[*u32]`.
    fn type_arg(&mut self, fx: &mut Fx, index: &Index) -> TyId {
        match index {
            Index::Type(ty) => self.lower(fx.file, *ty),
            Index::Expr(e) => match self.expr(fx, *e, None) {
                Item::Type(ty) => ty,
                _ => {
                    let span = fx.ast.expr(*e).span;
                    self.error(fx.file, span, "expected a type argument");
                    self.types.error()
                }
            },
            Index::Slice { .. } => self.types.error(),
        }
    }

    fn builtin_call(
        &mut self,
        fx: &mut Fx,
        span: Span,
        name: &'static str,
        type_arg: Option<&Index>,
        args: &[Arg],
        expected: Option<TyId>,
    ) -> Item {
        if matches!(name, "volatile_load" | "volatile_store" | "from_pointer") {
            self.require_unsafe(fx, span, &format!("`{name}`"));
        }
        let wants_type_arg = matches!(name, "fill" | "from_address" | "cast" | "truncate");
        if wants_type_arg != type_arg.is_some() {
            let message = if wants_type_arg {
                format!("`{name}` takes a type argument, as in `{name}[T](...)`")
            } else {
                format!("`{name}` takes no type argument")
            };
            self.error(fx.file, span, message);
            for arg in args {
                self.value(fx, arg.value, None);
            }
            return Item::Value(self.types.error());
        }
        let arity = match name {
            "make" | "panic" | "from_address" | "cast" | "truncate" | "c_string"
            | "volatile_load" | "fill" | "free" => 1,
            _ => 2,
        };
        if args.len() != arity {
            let message = format!("`{name}` takes {arity} arguments, found {}", args.len());
            self.error(fx.file, span, message);
            for arg in args {
                self.value(fx, arg.value, None);
            }
            return Item::Value(self.types.error());
        }
        let modes: &[Mode] = match name {
            "push" | "append" => &[Mode::Inout, Mode::Let],
            "free" => &[Mode::Sink],
            _ => &[Mode::Let, Mode::Let],
        };
        for (arg, mode) in args.iter().zip(modes) {
            if arg.mode != *mode {
                let span = fx.ast.expr(arg.value).span;
                let mode = match mode {
                    Mode::Let => "no mode",
                    Mode::Inout => "`inout`",
                    Mode::Sink => "`sink`",
                };
                self.error(
                    fx.file,
                    span,
                    format!("this argument of `{name}` takes {mode}"),
                );
            }
        }
        let a = args[0].value;
        let usize = self.types.usize();
        let oom = self.named(NamedId::Builtin(BuiltinNamed::OutOfMemory));
        let oom_result = self.types.intern(Ty::Result(None, oom));
        let ty = match name {
            "panic" => {
                let format = self.named(NamedId::Builtin(BuiltinNamed::Format));
                self.check(fx, a, format);
                self.types.never()
            }
            "make" => {
                let allocator = self.named(NamedId::Builtin(BuiltinNamed::Allocator));
                self.check(fx, a, allocator);
                match expected.map(|t| (t, self.types.get(t))) {
                    Some((ty, Ty::Growable(_) | Ty::Error)) => ty,
                    _ => {
                        self.error(fx.file, span, "`make` needs a place that expects a `[..]T`");
                        self.types.error()
                    }
                }
            }
            "push" | "append" => {
                self.exclusive(fx, &[(a, Mode::Inout), (args[1].value, Mode::Let)]);
                let v = self.value(fx, a, None);
                self.require_mutable(fx, a, "pass `inout`");
                match self.types.get(v).clone() {
                    Ty::Growable(elem) => {
                        let item = if name == "push" {
                            elem
                        } else {
                            self.types.intern(Ty::View(elem))
                        };
                        self.check(fx, args[1].value, item);
                        self.consume(fx, args[1].value, item);
                    }
                    Ty::Error => {}
                    _ => {
                        let message = format!("`{name}` takes a `[..]T`, not `{}`", self.show(v));
                        self.error(fx.file, fx.ast.expr(a).span, message);
                    }
                }
                oom_result
            }
            "free" => {
                let v = self.value(fx, a, None);
                if !matches!(self.types.get(v), Ty::Growable(_) | Ty::Error) {
                    let message = format!("`free` takes a `[..]T`, not `{}`", self.show(v));
                    self.error(fx.file, fx.ast.expr(a).span, message);
                }
                self.sink(fx, a, v);
                return Item::Nothing;
            }
            "copy" => {
                let allocator = self.named(NamedId::Builtin(BuiltinNamed::Allocator));
                self.check(fx, a, allocator);
                let v = self.value(fx, args[1].value, None);
                match self.types.get(v).clone() {
                    Ty::Growable(elem) | Ty::View(elem) | Ty::Array(_, elem) => {
                        let growable = self.types.intern(Ty::Growable(elem));
                        self.types.intern(Ty::Result(Some(growable), oom))
                    }
                    Ty::Error => v,
                    _ => {
                        let message = format!(
                            "`copy` takes an array, view or `[..]T`, not `{}`",
                            self.show(v)
                        );
                        self.error(fx.file, span, message);
                        self.types.error()
                    }
                }
            }
            "fill" => {
                let Some(Index::Expr(len)) = type_arg else {
                    self.error(fx.file, span, "`fill` takes a length, as in `fill[16](0)`");
                    return Item::Value(self.types.error());
                };
                let Some(n) = self
                    .const_int(fx.file, *len, 0)
                    .and_then(|n| u64::try_from(n).ok())
                else {
                    let span = fx.ast.expr(*len).span;
                    self.error(
                        fx.file,
                        span,
                        "`fill`'s length is a constant, non-negative integer",
                    );
                    return Item::Value(self.types.error());
                };
                let elem = match expected.map(|t| self.types.get(t).clone()) {
                    Some(Ty::Array(want, elem)) => {
                        self.check(fx, a, elem);
                        if want != n {
                            let message = format!("expected {want} elements, found {n}");
                            self.error(fx.file, span, message);
                            return Item::Value(self.types.error());
                        }
                        elem
                    }
                    _ => self.value(fx, a, None),
                };
                self.types.intern(Ty::Array(n, elem))
            }
            "from_address" => {
                let target = self.type_arg(fx, type_arg.expect("checked above"));
                self.check(fx, a, usize);
                if !matches!(self.types.get(target), Ty::Pointer(_) | Ty::Error) {
                    let message = format!(
                        "`from_address` makes a pointer, not `{}`",
                        self.show(target)
                    );
                    self.error(fx.file, span, message);
                }
                target
            }
            "cast" | "truncate" => {
                let target = self.type_arg(fx, type_arg.expect("checked above"));
                let v = self.value(fx, a, None);
                if name == "truncate" {
                    let ok = self.types.as_int(target).is_some()
                        && (self.types.as_int(v).is_some() || self.types.is_error(v));
                    if !ok && !self.types.is_error(target) {
                        let message = format!(
                            "`truncate` converts between integers, not `{}` to `{}`",
                            self.show(v),
                            self.show(target)
                        );
                        self.error(fx.file, span, message);
                    }
                }
                target
            }
            "from_pointer" | "offset" => {
                let p = self.value(fx, a, None);
                self.check(fx, args[1].value, usize);
                match self.types.get(p).clone() {
                    Ty::Pointer(elem) if name == "from_pointer" => {
                        self.types.intern(Ty::View(elem))
                    }
                    Ty::Pointer(_) | Ty::Error => p,
                    _ => {
                        let message = format!("`{name}` takes a pointer, not `{}`", self.show(p));
                        self.error(fx.file, fx.ast.expr(a).span, message);
                        self.types.error()
                    }
                }
            }
            "c_string" => {
                let is_plain = matches!(&fx.ast.expr(a).kind, ExprKind::Str(parts) if parts.iter().all(|p| matches!(p, StrPart::Text(_))));
                if !is_plain {
                    self.error(
                        fx.file,
                        fx.ast.expr(a).span,
                        "`c_string` takes a string literal without interpolation",
                    );
                }
                let u8 = self.types.int(IntTy::U8);
                self.types.intern(Ty::Pointer(u8))
            }
            "volatile_load" | "volatile_store" => {
                let p = self.value(fx, a, None);
                let elem = match self.types.get(p) {
                    Ty::Pointer(elem) => *elem,
                    Ty::Error => p,
                    _ => {
                        let message = format!("`{name}` takes a pointer, not `{}`", self.show(p));
                        self.error(fx.file, fx.ast.expr(a).span, message);
                        self.types.error()
                    }
                };
                if name == "volatile_store" {
                    self.check(fx, args[1].value, elem);
                    return Item::Nothing;
                }
                elem
            }
            _ => {
                // wrapping_add and its relatives.
                let ty = self.operands(fx, a, args[1].value, expected);
                if !matches!(self.types.get(ty), Ty::Int(_) | Ty::Error) {
                    let message =
                        format!("`{name}` takes built-in integers, not `{}`", self.show(ty));
                    self.error(fx.file, span, message);
                }
                ty
            }
        };
        Item::Value(ty)
    }
}

/// The expression inside `.(...)`, for a form that takes one value.
fn paren_inner(fx: &Fx, value: ExprId) -> ExprId {
    match &fx.ast.expr(value).kind {
        ExprKind::Paren(inner) => *inner,
        _ => value,
    }
}

/// The value of an integer literal, for code generation.
pub fn parse_int_literal(text: &str) -> Option<i128> {
    parse_int(text)
}

/// The code of a character literal, for code generation.
pub fn char_literal(text: &str) -> Option<i128> {
    char_value(text)
}

/// The value of an integer literal, or `None` if it does not fit in 128
/// bits.
fn parse_int(text: &str) -> Option<i128> {
    let digits: String = text.chars().filter(|&c| c != '_').collect();
    let (radix, body) = match digits.get(..2) {
        Some("0x") => (16, &digits[2..]),
        Some("0b") => (2, &digits[2..]),
        Some("0o") => (8, &digits[2..]),
        _ => (10, digits.as_str()),
    };
    i128::from_str_radix(body, radix).ok()
}

/// The code of a character literal such as `'a'` or `'\n'`.
fn char_value(text: &str) -> Option<i128> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    let bytes = inner.as_bytes();
    let code = match bytes {
        [b] => *b,
        [b'\\', b'n'] => b'\n',
        [b'\\', b't'] => b'\t',
        [b'\\', b'r'] => b'\r',
        [b'\\', b'0'] => 0,
        [b'\\', c] => *c,
        [b'\\', b'x', ..] => u8::from_str_radix(&inner[2..], 16).ok()?,
        _ => return None,
    };
    Some(i128::from(code))
}
