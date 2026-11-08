//! The local rules that need no flow: `unsafe`, `shared` globals,
//! exclusivity of `inout` arguments, the `Format` rule and constant
//! global initializers.

use super::{Checker, Fx};
use crate::ast::{DeclKind, ExprId, ExprKind, Index, Mode, StrPart, TypeDef, TypeKind, UnaryOp};
use crate::package::FileId;
use crate::resolve::{BUILTIN_TYPES, DeclRef, LocalId, Res};
use crate::source::Span;
use crate::types::{BuiltinNamed, NamedDef, NamedId, Ty, TyId};

/// The place expressions `e` is built from: `e`, then its base, through
/// fields, indexing and parentheses. Assigning to `e` or taking its
/// address does not read any of them.
pub(super) fn spine(fx: &Fx, e: ExprId) -> Vec<ExprId> {
    let mut out = Vec::new();
    let mut current = e;
    loop {
        out.push(current);
        current = match &fx.ast.expr(current).kind {
            ExprKind::Paren(inner) => *inner,
            ExprKind::Field { value, .. } | ExprKind::Index { value, .. } => *value,
            _ => return out,
        };
    }
}

/// Where an argument's storage starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Root {
    Local(LocalId),
    Global(DeclRef),
}

/// One step from a root to an argument's storage.
#[derive(PartialEq, Eq)]
enum Step {
    Field(String),
    /// An index, when it is a constant.
    Index(Option<i128>),
    Deref,
}

impl Checker<'_> {
    pub(super) fn require_unsafe(&mut self, fx: &Fx, span: Span, what: &str) {
        if !fx.in_unsafe {
            self.error(fx.file, span, format!("{what} needs `unsafe`"));
        }
    }

    /// A call to a C function or an `unsafe fn` needs `unsafe`.
    pub(super) fn unsafe_callee(&mut self, fx: &Fx, callee: ExprId) {
        let Some(Res::Decl(decl)) = self.names.files[fx.file.0 as usize].exprs[callee.index()]
        else {
            return;
        };
        let what = match &self.program.file(decl.file).ast.decls[decl.index].kind {
            DeclKind::Fn(f) if f.is_unsafe => "the unsafe function",
            DeclKind::ExternFn { .. } => "the C function",
            _ => return,
        };
        let span = fx.ast.expr(callee).span;
        let message = format!("calling {what} `{}`", self.text(fx.file, span));
        self.require_unsafe(fx, span, &message);
    }

    /// A use of a top-level name: a `shared` global needs its `lock`, and
    /// a C global needs `unsafe` unless `&` takes its address.
    pub(super) fn use_decl(&mut self, fx: &Fx, e: ExprId, decl: DeclRef, span: Span) {
        match &self.program.file(decl.file).ast.decls[decl.index].kind {
            DeclKind::Global { shared: true, .. } if !fx.locked.contains(&decl) => {
                let name = self.text(fx.file, span);
                let message =
                    format!("`{name}` is `shared`: use it inside `lock {name} {{ ... }}`");
                self.error(fx.file, span, message);
            }
            DeclKind::ExternVar { .. } if !fx.addressed.contains(&e) => {
                let message = format!("using the C global `{}`", self.text(fx.file, span));
                self.require_unsafe(fx, span, &message);
            }
            _ => {}
        }
    }

    // Exclusivity.

    /// Checks that no `inout` argument overlaps another argument.
    pub(super) fn exclusive(&mut self, fx: &Fx, args: &[(ExprId, Mode)]) {
        let paths: Vec<Option<(Root, Vec<Step>)>> =
            args.iter().map(|(e, _)| self.access_path(fx, *e)).collect();
        for (i, (a, mode)) in args.iter().enumerate() {
            if *mode != Mode::Inout {
                continue;
            }
            let Some(path_a) = &paths[i] else {
                continue;
            };
            for (j, (b, other_mode)) in args.iter().enumerate() {
                // A pair of `inout` arguments is reported once.
                if j == i || (*other_mode == Mode::Inout && j < i) {
                    continue;
                }
                let Some(path_b) = &paths[j] else {
                    continue;
                };
                if overlaps(path_a, path_b) {
                    let span = fx.ast.expr(*b).span;
                    let message = format!(
                        "`{}` overlaps `{}`, which is passed `inout`; an `inout` argument must not overlap another argument",
                        self.text(fx.file, span),
                        self.text(fx.file, fx.ast.expr(*a).span),
                    );
                    self.error(fx.file, span, message);
                }
            }
        }
    }

    /// The storage a place expression names, as a root and steps.
    fn access_path(&self, fx: &Fx, e: ExprId) -> Option<(Root, Vec<Step>)> {
        let res = self.names.files[fx.file.0 as usize].exprs[e.index()];
        match &fx.ast.expr(e).kind {
            ExprKind::Paren(inner) => self.access_path(fx, *inner),
            ExprKind::Name(_) | ExprKind::Field { .. } if matches!(res, Some(Res::Decl(_))) => {
                let Some(Res::Decl(decl)) = res else {
                    return None;
                };
                match self.program.file(decl.file).ast.decls[decl.index].kind {
                    DeclKind::Global { .. } | DeclKind::ExternVar { .. } => {
                        Some((Root::Global(decl), Vec::new()))
                    }
                    _ => None,
                }
            }
            ExprKind::Name(_) => match res {
                Some(Res::Local(id)) => Some((Root::Local(id), Vec::new())),
                _ => None,
            },
            ExprKind::Field { value, name } => {
                let (root, mut steps) = self.access_path(fx, *value)?;
                steps.push(Step::Field(self.text(fx.file, name.span).to_owned()));
                Some((root, steps))
            }
            ExprKind::Index { value, index } => {
                let (root, mut steps) = self.access_path(fx, *value)?;
                let at = match index {
                    Index::Expr(i) => self.const_int(fx.file, *i, 0),
                    _ => None,
                };
                steps.push(Step::Index(at));
                Some((root, steps))
            }
            ExprKind::Deref(pointer) => {
                let (root, mut steps) = self.access_path(fx, *pointer)?;
                steps.push(Step::Deref);
                Some((root, steps))
            }
            _ => None,
        }
    }

    // The `Format` rule.

    /// Whether a value of `ty` holds a `Format`, which points into the
    /// frame that made it. A declared type is checked where it is
    /// declared, so it never holds one.
    fn holds_format(&self, ty: TyId) -> bool {
        match self.types.get(ty) {
            Ty::Named(NamedId::Builtin(BuiltinNamed::Format | BuiltinNamed::PanicInfo)) => true,
            Ty::Tuple(fields) => fields.iter().any(|(_, t)| self.holds_format(*t)),
            Ty::View(elem)
            | Ty::Array(_, elem)
            | Ty::Growable(elem)
            | Ty::Pointer(elem)
            | Ty::Optional(elem) => self.holds_format(*elem),
            Ty::Result(value, error) => {
                value.is_some_and(|v| self.holds_format(v)) || self.holds_format(*error)
            }
            Ty::Fn(f) => {
                f.result.is_some_and(|r| self.holds_format(r))
                    || f.params
                        .iter()
                        .any(|(_, p)| !self.is_format_param(*p) && self.holds_format(*p))
            }
            _ => false,
        }
    }

    fn is_format_param(&self, ty: TyId) -> bool {
        self.is_named(ty, BuiltinNamed::Format) || self.is_named(ty, BuiltinNamed::PanicInfo)
    }

    /// `ty` is written somewhere other than a parameter.
    pub(super) fn no_format(&mut self, file: FileId, span: Span, ty: TyId) {
        if !self.holds_format(ty) {
            return;
        }
        let message = if self.is_named(ty, BuiltinNamed::Format) {
            "a `Format` can only be a parameter: it points into the frame of the call that made it"
                .to_owned()
        } else {
            format!(
                "`{}` holds a `Format`, which can only be a parameter: it points into the frame of the call that made it",
                self.show(ty)
            )
        };
        self.error(file, span, message);
    }

    /// A parameter's type may be `Format` itself, but not hold one.
    pub(super) fn format_param(&mut self, file: FileId, span: Span, ty: TyId) {
        if !self.is_format_param(ty) {
            self.no_format(file, span, ty);
        }
    }

    /// The fields, payloads and members of a declared type.
    pub(super) fn no_format_in_type(&mut self, file: FileId, def: &TypeDef, ty: TyId) {
        let ast = &self.program.file(file).ast;
        let mut parts: Vec<(Span, TyId)> = Vec::new();
        match (def, self.named_info(ty).map(|i| &i.def)) {
            (TypeDef::Type(t), Some(NamedDef::Plain { underlying, .. })) => {
                match (&ast.ty(*t).kind, self.types.get(*underlying)) {
                    (TypeKind::Tuple(fields), Ty::Tuple(tys)) => {
                        for (field, (_, t)) in fields.iter().zip(tys) {
                            parts.push((ast.ty(field.ty).span, *t));
                        }
                    }
                    _ => parts.push((ast.ty(*t).span, *underlying)),
                }
            }
            (TypeDef::Enum { items, .. }, Some(NamedDef::Enum(info))) => {
                let payloads = items.iter().filter_map(|item| match item {
                    crate::ast::EnumItem::Variant { payload, .. } => Some(*payload),
                    crate::ast::EnumItem::Open(_) => None,
                });
                for (payload, variant) in payloads.zip(&info.variants) {
                    if let (Some(p), Some(t)) = (payload, variant.payload) {
                        parts.push((ast.ty(p).span, t));
                    }
                }
            }
            (TypeDef::Union(fields), Some(NamedDef::Union(tys))) => {
                for (field, (_, t)) in fields.iter().zip(tys) {
                    parts.push((ast.ty(field.ty).span, *t));
                }
            }
            _ => {}
        }
        for (span, t) in parts {
            self.no_format(file, span, t);
        }
    }

    // Constant initializers.

    /// A global's initializer is computed before the program runs, so it
    /// must be a constant.
    pub(super) fn constant(&mut self, file: FileId, e: ExprId) {
        if let Some(span) = self.non_constant(file, e) {
            self.error(
                file,
                span,
                "a global's value is computed before the program runs, so this must be a constant",
            );
        }
    }

    /// The first part of `e` that is not a constant.
    fn non_constant(&self, file: FileId, e: ExprId) -> Option<Span> {
        let ast = &self.program.file(file).ast;
        let res = |e: ExprId| self.names.files[file.0 as usize].exprs[e.index()];
        let expr = ast.expr(e);
        let all = |es: Vec<ExprId>| es.into_iter().find_map(|e| self.non_constant(file, e));
        match &expr.kind {
            ExprKind::Int | ExprKind::Float | ExprKind::Char => None,
            ExprKind::Str(parts) => parts
                .iter()
                .any(|p| matches!(p, StrPart::Interpolation(_)))
                .then_some(expr.span),
            ExprKind::Paren(inner)
            | ExprKind::Unary {
                op: UnaryOp::Neg | UnaryOp::Not | UnaryOp::BitNot,
                operand: inner,
            } => self.non_constant(file, *inner),
            ExprKind::Binary { left, right, .. } => all(vec![*left, *right]),
            ExprKind::Tuple(fields) => all(fields.iter().map(|(_, v)| *v).collect()),
            ExprKind::Array(elems) => all(elems.clone()),
            ExprKind::Construct { value, .. } => self.non_constant(file, *value),
            ExprKind::Name(_) => match res(e) {
                Some(Res::Decl(decl)) => self.variable_decl(decl).then_some(expr.span),
                _ => None,
            },
            ExprKind::Field { value, name } => match res(e) {
                Some(Res::Decl(decl)) => self.variable_decl(decl).then_some(expr.span),
                // A variant, as in `Color.red`.
                _ if self.is_type_expr(file, *value) => None,
                _ if self.text(file, name.span) == "len" => self.non_constant(file, *value),
                _ => Some(expr.span),
            },
            ExprKind::Call { callee, args } => {
                let name = match &ast.expr(*callee).kind {
                    ExprKind::Index { value, .. } => res(*value),
                    _ => res(*callee),
                };
                match name {
                    Some(Res::Builtin("from_address" | "fill" | "c_string" | "truncate")) => {
                        all(args.iter().map(|a| a.value).collect())
                    }
                    _ => Some(expr.span),
                }
            }
            _ => Some(expr.span),
        }
    }

    /// Whether a top-level name is not a constant: `var` globals and C
    /// globals. Functions are constants: their addresses are fixed.
    fn variable_decl(&self, decl: DeclRef) -> bool {
        match &self.program.file(decl.file).ast.decls[decl.index].kind {
            DeclKind::Global { local, .. } => local.mutable,
            DeclKind::ExternVar { .. } => true,
            _ => false,
        }
    }

    fn is_type_expr(&self, file: FileId, e: ExprId) -> bool {
        let res = self.names.files[file.0 as usize].exprs[e.index()];
        match res {
            Some(Res::Builtin(name)) => BUILTIN_TYPES.contains(&name),
            Some(Res::Decl(decl)) => matches!(
                self.program.file(decl.file).ast.decls[decl.index].kind,
                DeclKind::Type { .. }
            ),
            _ => false,
        }
    }
}

/// Whether two access paths can name the same storage: the same root, and
/// no step where they go to different fields or different constant
/// indexes.
fn overlaps(a: &(Root, Vec<Step>), b: &(Root, Vec<Step>)) -> bool {
    if a.0 != b.0 {
        return false;
    }
    for (x, y) in a.1.iter().zip(&b.1) {
        match (x, y) {
            (Step::Field(m), Step::Field(n)) if m != n => return false,
            (Step::Index(Some(m)), Step::Index(Some(n))) if m != n => return false,
            _ => {}
        }
    }
    true
}
