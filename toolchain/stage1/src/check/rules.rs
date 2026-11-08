//! The local rules that need no flow: `unsafe`, `shared` globals,
//! exclusivity of `inout` arguments, one owner per buffer, the `Format`
//! rule and constant global initializers.

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
pub(super) enum Root {
    Local(LocalId),
    Global(DeclRef),
}

/// One step from a root to an argument's storage.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Step {
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
    pub(super) fn access_path(&self, fx: &Fx, e: ExprId) -> Option<(Root, Vec<Step>)> {
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

    // One owner per buffer.

    /// Whether `ty` holds a `[..]T`, so a value of it never copies
    /// implicitly.
    pub(super) fn owning(&mut self, ty: TyId) -> bool {
        if let Some(&known) = self.owning.get(&ty) {
            return known;
        }
        // A type that holds itself does so through a `[..]T`, which
        // decides it before the cycle closes.
        self.owning.insert(ty, false);
        let parts: Vec<TyId> = match self.types.get(ty).clone() {
            Ty::Growable(_) => {
                self.owning.insert(ty, true);
                return true;
            }
            Ty::Tuple(fields) => fields.iter().map(|(_, t)| *t).collect(),
            Ty::Array(_, elem) | Ty::Optional(elem) => vec![elem],
            Ty::Result(value, error) => value.into_iter().chain([error]).collect(),
            Ty::Named(_) => match self.named_info(ty).map(|i| &i.def) {
                Some(NamedDef::Plain { underlying, .. }) => vec![*underlying],
                Some(NamedDef::Enum(info)) => {
                    info.variants.iter().filter_map(|v| v.payload).collect()
                }
                Some(NamedDef::Union(fields)) => fields.iter().map(|(_, t)| *t).collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        let owning = parts.into_iter().any(|t| self.owning(t));
        self.owning.insert(ty, owning);
        owning
    }

    /// `e` becomes a new home's value of type `target`. If `target` is
    /// owning, `e` must not be a place that keeps the buffer too. Inside
    /// a `return` value, a place that starts at one of the function's
    /// own locals is taken instead, once.
    pub(super) fn fresh(&mut self, fx: &mut Fx, e: ExprId, target: TyId) {
        if self.types.is_error(target) || !self.owning(target) {
            return;
        }
        match &fx.ast.expr(e).kind {
            ExprKind::Paren(inner) | ExprKind::Unsafe(inner) => {
                self.fresh(fx, *inner, target);
            }
            ExprKind::If {
                then, otherwise, ..
            } => {
                self.fresh(fx, *then, target);
                match otherwise {
                    crate::ast::IfElse::Branch(value) | crate::ast::IfElse::If(value) => {
                        self.fresh(fx, *value, target);
                    }
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.fresh(fx, arm.body, target);
                }
            }
            ExprKind::Name(_) | ExprKind::Field { .. } | ExprKind::Index { .. } => {
                let Some(path) = self.access_path(fx, e) else {
                    return;
                };
                let text = self.text(fx.file, fx.ast.expr(e).span);
                if let (Root::Local(id), Some(taken)) = (path.0, &mut fx.returning)
                    && fx.locals.get(&id).is_some_and(|l| l.owned)
                {
                    if fx.defers.iter().flatten().any(|d| *d == id) {
                        let message = format!(
                            "a `defer` uses `{}` after this `return`; hand the value over with `take(inout {text})`",
                            self.text(fx.file, Self::root_name(fx, e))
                        );
                        self.error(fx.file, fx.ast.expr(e).span, message);
                        return;
                    }
                    if !taken.iter().any(|t| overlaps(t, &path)) {
                        taken.push(path);
                        return;
                    }
                    let message = format!(
                        "this `return` already takes `{text}` or part of it; a buffer can go to only one place"
                    );
                    self.error(fx.file, fx.ast.expr(e).span, message);
                    return;
                }
                let message = format!(
                    "this would copy `{text}`, and its buffer would have two owners; hand it over with `take(inout {text})` or make a second one with `try copy(alloc, {text})`"
                );
                self.error(fx.file, fx.ast.expr(e).span, message);
            }
            // Everything else is a new value. A raw pointer's target, read
            // with `.*`, is trusted, as `unsafe` is.
            _ => {}
        }
    }

    /// Whether `take` can leave a value of `ty` empty: each owning part
    /// is a `[..]T` or an optional, which becomes `none`, or a record or
    /// tuple whose owning parts are.
    pub(super) fn emptiable(&mut self, ty: TyId) -> bool {
        if !self.owning(ty) {
            return true;
        }
        let fields: Vec<TyId> = match self.types.get(ty) {
            Ty::Growable(_) | Ty::Optional(_) => return true,
            Ty::Tuple(fields) => fields.iter().map(|(_, t)| *t).collect(),
            Ty::Named(_) => match self.named_info(ty).map(|i| &i.def) {
                Some(NamedDef::Plain { underlying, .. }) => vec![*underlying],
                _ => return false,
            },
            _ => return false,
        };
        fields.into_iter().all(|t| self.emptiable(t))
    }

    /// The name a place expression starts at.
    fn root_name(fx: &Fx, e: ExprId) -> Span {
        let spine = spine(fx, e);
        fx.ast
            .expr(*spine.last().expect("a spine has its start"))
            .span
    }

    /// `append` and `copy` copy each element byte for byte, which an
    /// owning element cannot be.
    pub(super) fn no_owning_copy(&mut self, fx: &Fx, span: Span, name: &str, elem: TyId) {
        if self.owning(elem) {
            let message = format!(
                "`{name}` copies each element, and a `{}` cannot be copied: its buffer would have two owners",
                self.show(elem)
            );
            self.error(fx.file, span, message);
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

    // Views are not stored (rule 3).

    /// Whether a value of `ty` holds a view. Such a value is a local, a
    /// parameter or a result, where rule 2 follows what it borrows. A
    /// pointer and a function value hold none that the checker follows.
    pub(super) fn holds_view(&mut self, ty: TyId) -> bool {
        if let Some(&known) = self.views.get(&ty) {
            return known;
        }
        // A type that holds itself does so through a `[..]T` or a
        // pointer, which never hold a view.
        self.views.insert(ty, false);
        let parts: Vec<TyId> = match self.types.get(ty).clone() {
            Ty::View(_)
            | Ty::Named(NamedId::Builtin(BuiltinNamed::Format | BuiltinNamed::PanicInfo)) => {
                self.views.insert(ty, true);
                return true;
            }
            Ty::Tuple(fields) => fields.iter().map(|(_, t)| *t).collect(),
            Ty::Array(_, elem) | Ty::Optional(elem) => vec![elem],
            Ty::Result(value, error) => value.into_iter().chain([error]).collect(),
            Ty::Named(_) => match self.named_info(ty).map(|i| &i.def) {
                Some(NamedDef::Plain { underlying, .. }) => vec![*underlying],
                Some(NamedDef::Enum(info)) => {
                    info.variants.iter().filter_map(|v| v.payload).collect()
                }
                Some(NamedDef::Union(fields)) => fields.iter().map(|(_, t)| *t).collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        let holds = parts.into_iter().any(|t| self.holds_view(t));
        self.views.insert(ty, holds);
        holds
    }

    /// `ty` is the type of `place`, which outlives every frame: a
    /// `[..]T` element or a `var` global. A `Format` is reported by the
    /// `Format` rule instead.
    pub(super) fn no_view(&mut self, file: FileId, span: Span, ty: TyId, place: &str) {
        if self.types.is_error(ty) || self.holds_format(ty) || !self.holds_view(ty) {
            return;
        }
        let shown = self.show(ty);
        let message = if matches!(self.types.get(ty), Ty::View(_)) {
            format!(
                "{place} cannot be a view: it can outlive what `{shown}` points into, so store owned values or indexes"
            )
        } else {
            format!(
                "{place} cannot hold a view, and `{shown}` holds one: it can outlive what the view points into, so store owned values or indexes"
            )
        };
        self.error(file, span, message);
    }

    /// Every written `[..]T`, once every type is known.
    pub(super) fn no_views_in_growables(&mut self) {
        for (file, span, elem) in std::mem::take(&mut self.growables) {
            self.no_view(file, span, elem, "a `[..]T` element");
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
pub(super) fn overlaps(a: &(Root, Vec<Step>), b: &(Root, Vec<Step>)) -> bool {
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
