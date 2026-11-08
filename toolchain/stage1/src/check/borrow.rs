//! Rule 2: a view, or a value that holds one, borrows the storage it
//! points into, and that storage cannot change while the borrow is used.
//!
//! After a function body is checked, one walk over it finds each local
//! that borrows, what it borrows (its loans), every use of a local, and
//! every place the function changes. A loan is an access path, or a
//! temporary that dies at the end of its statement. The walk repeats
//! until no local gains a loan, since a loop can carry a loan back to an
//! earlier statement. A change is an error when a local that borrows an
//! overlapping path is used after it, or anywhere in a loop around it.
//!
//! The same walk checks the callee's side: a returned view, and a view
//! stored in an `inout` parameter, must point into a parameter or into
//! static data.

use std::collections::HashMap;

use super::rules::{Root, Step, overlaps, spine};
use super::{Checker, Fx, Item};
use crate::ast::{
    AssignOp, Binding, Block, DeclKind, DestructField, ExprId, ExprKind, FnDecl, ForKind, IfElse,
    Index, Iterable, Mode, PatId, PatternKind, StmtId, StmtKind, StrPart, UnaryOp,
};
use crate::resolve::{DeclRef, LocalId, Res};
use crate::source::Span;
use crate::types::{NamedDef, Ty, TyId};

const TEMPORARY: &str =
    "this borrows a temporary, which is gone at the end of the statement; bind it to a local first";

/// What a loan keeps from changing.
#[derive(Clone, PartialEq)]
enum Source {
    Place(Root, Vec<Step>),
    /// A value that dies at the end of its statement.
    Temp,
}

#[derive(Clone)]
struct Loan {
    source: Source,
    /// Where the borrower took it.
    added: u32,
}

/// A local that borrows, or a `for` loop over borrowed storage.
struct Borrower {
    /// `None` for a loop, which uses its loans throughout its body.
    local: Option<LocalId>,
    name: String,
    /// The local aliases storage it does not own, as a `let` bound to a
    /// place does; a view of it is a view of what it borrows.
    aliases: bool,
    declared: u32,
    /// The block the local is in scope for.
    scope: (u32, u32),
    loans: Vec<Loan>,
}

/// A place the function changes, at a position.
struct Change {
    path: (Root, Vec<Step>),
    at: u32,
    span: Span,
}

/// A call to a function declaration while a local borrows a global; an
/// error if the callee writes that global, which only the write sets
/// say.
pub(super) struct GlobalLoan {
    pub(super) file: crate::package::FileId,
    pub(super) span: Span,
    pub(super) callee: DeclRef,
    pub(super) global: DeclRef,
    /// The local that borrows, or `None` for a `for` loop.
    pub(super) borrower: Option<String>,
}

/// Which walk over the body is running.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Pass {
    /// Records uses, changes, calls and loops.
    #[default]
    First,
    /// Spreads loans until no borrower gains one.
    Again,
    /// Reports the callee's side.
    Report,
}

#[derive(Default)]
struct Walk {
    borrowers: Vec<Borrower>,
    of_local: HashMap<LocalId, usize>,
    uses: HashMap<LocalId, Vec<u32>>,
    changes: Vec<Change>,
    calls: Vec<(DeclRef, Span, u32)>,
    loops: Vec<(u32, u32)>,
    /// The innermost block or arm a binding is scoped to.
    scopes: Vec<(u32, u32)>,
    /// While walking a `defer`: where its uses happen.
    deferred_at: Option<u32>,
    pass: Pass,
    /// A borrower gained a loan in this walk.
    grew: bool,
    params: HashMap<LocalId, Mode>,
    returns_view: bool,
    problems: Vec<(Span, String)>,
}

impl Checker<'_> {
    /// Checks rule 2 in the function `f`, whose body `fx` has checked.
    pub(super) fn borrows(&mut self, fx: &Fx, f: &FnDecl) {
        let mut w = Walk::default();
        for param in &f.params {
            if let Some(name) = param.name
                && let Some(&id) = self.names.files[fx.file.0 as usize]
                    .bindings
                    .get(&name.span.start)
            {
                w.params.insert(id, param.mode);
            }
        }
        w.returns_view = fx.result.is_some_and(|r| self.holds_view(r));
        // Each walk can only add loans, and there are finitely many
        // paths, so this ends.
        loop {
            w.grew = false;
            self.walk_block(fx, &mut w, &f.body);
            w.pass = Pass::Again;
            if !w.grew {
                break;
            }
        }
        w.pass = Pass::Report;
        self.walk_block(fx, &mut w, &f.body);
        self.conflicts(fx, &mut w);
        let mut seen: Vec<(Span, String)> = Vec::new();
        for (span, message) in w.problems {
            if !seen.iter().any(|(s, m)| *s == span && *m == message) {
                seen.push((span, message.clone()));
                self.error(fx.file, span, message);
            }
        }
    }

    fn ty_of(&self, fx: &Fx, e: ExprId) -> Option<TyId> {
        match self.files[fx.file.0 as usize].exprs[e.index()] {
            Some(Item::Value(ty)) => Some(ty),
            _ => None,
        }
    }

    /// Whether a value of `ty` has storage a view can point into: a
    /// fixed or growable array.
    fn has_storage(&self, ty: TyId) -> bool {
        self.storage_in(ty, 0)
    }

    fn storage_in(&self, ty: TyId, depth: u32) -> bool {
        if depth > 32 {
            return false;
        }
        match self.types.get(ty) {
            Ty::Array(..) | Ty::Growable(_) => true,
            Ty::Tuple(fields) => fields.iter().any(|(_, t)| self.storage_in(*t, depth + 1)),
            Ty::Optional(elem) => self.storage_in(*elem, depth + 1),
            Ty::Result(value, error) => {
                value.is_some_and(|v| self.storage_in(v, depth + 1))
                    || self.storage_in(*error, depth + 1)
            }
            Ty::Named(_) => match self.named_info(ty).map(|i| &i.def) {
                Some(NamedDef::Plain { underlying, .. }) => self.storage_in(*underlying, depth + 1),
                Some(NamedDef::Enum(info)) => info
                    .variants
                    .iter()
                    .any(|v| v.payload.is_some_and(|p| self.storage_in(p, depth + 1))),
                Some(NamedDef::Union(fields)) => {
                    fields.iter().any(|(_, t)| self.storage_in(*t, depth + 1))
                }
                None => false,
            },
            _ => false,
        }
    }

    fn local_name(&self, fx: &Fx, id: LocalId) -> String {
        let span = self.names.files[fx.file.0 as usize].locals[id.0 as usize];
        self.text(fx.file, span).to_owned()
    }

    fn here(w: &Walk, e: Span) -> u32 {
        w.deferred_at.unwrap_or(e.start)
    }

    // Statements.

    fn walk_block(&mut self, fx: &Fx, w: &mut Walk, block: &Block) {
        w.scopes.push((block.span.start, block.span.end));
        for &s in &block.stmts {
            self.walk_stmt(fx, w, s, block);
        }
        w.scopes.pop();
    }

    fn walk_stmt(&mut self, fx: &Fx, w: &mut Walk, id: StmtId, block: &Block) {
        let stmt = fx.ast.stmt(id);
        match &stmt.kind {
            StmtKind::Local(local) => {
                let loans = self.loans(fx, w, local.value);
                let declared = fx.ast.expr(local.value).span.end;
                match &local.binding {
                    Binding::Name {
                        name: Some(name), ..
                    } => {
                        self.bind_loans(fx, w, name.span, local.value, &loans, declared);
                    }
                    Binding::Name { name: None, .. } => {}
                    Binding::Destructure(fields) => {
                        self.bind_fields(fx, w, fields, local.value, &loans, declared);
                    }
                }
            }
            StmtKind::Return(value) => {
                if let Some(e) = value {
                    let loans = self.loans(fx, w, *e);
                    if w.returns_view && w.pass == Pass::Report {
                        let span = fx.ast.expr(*e).span;
                        for loan in &loans {
                            if let Some(message) = self.escapes(fx, w, &loan.source, None) {
                                w.problems.push((span, message));
                            }
                        }
                    }
                }
            }
            StmtKind::If(s) => {
                self.loans(fx, w, s.cond);
                self.walk_block(fx, w, &s.then);
                match &s.otherwise {
                    Some(crate::ast::Else::Block(b)) => self.walk_block(fx, w, b),
                    Some(crate::ast::Else::If(next)) => self.walk_stmt(fx, w, *next, block),
                    None => {}
                }
            }
            StmtKind::Match { value, arms } => {
                let subject = self.loans(fx, w, *value);
                for arm in arms {
                    let scope = (arm.body.span.start, arm.body.span.end);
                    for &p in &arm.patterns {
                        self.bind_pattern(fx, w, p, *value, &subject, scope);
                    }
                    self.walk_block(fx, w, &arm.body);
                }
            }
            StmtKind::For(for_loop) => {
                let body = (for_loop.body.span.start, for_loop.body.span.end);
                match &for_loop.kind {
                    ForKind::Forever => Self::mark_loop(w, body),
                    ForKind::While(cond) => {
                        Self::mark_loop(w, (fx.ast.expr(*cond).span.start, body.1));
                        self.loans(fx, w, *cond);
                    }
                    ForKind::In {
                        binding, iterable, ..
                    } => {
                        Self::mark_loop(w, body);
                        match iterable {
                            Iterable::Range { start, end } => {
                                self.loans(fx, w, *start);
                                self.loans(fx, w, *end);
                            }
                            Iterable::Expr(e) => self.walk_for_in(fx, w, binding, *e, body),
                        }
                    }
                }
                self.walk_block(fx, w, &for_loop.body);
            }
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::Defer(call) => {
                let outer = w.deferred_at.replace(block.span.end);
                self.loans(fx, w, *call);
                w.deferred_at = outer;
            }
            StmtKind::Unsafe(body) | StmtKind::Lock { body, .. } => self.walk_block(fx, w, body),
            StmtKind::Assign { left, op, right } => {
                let loans = self.loans(fx, w, *right);
                let at = fx.ast.expr(*right).span.end;
                self.target(fx, w, *left, *op == AssignOp::Set);
                if let Some(path) = self.access_path(fx, *left) {
                    if w.pass == Pass::First {
                        w.changes.push(Change {
                            path: path.clone(),
                            at,
                            span: fx.ast.expr(*left).span,
                        });
                    }
                    let left_ty = self.ty_of(fx, *left);
                    if left_ty.is_some_and(|t| self.holds_view(t)) {
                        self.store(fx, w, &path, &loans, at, fx.ast.expr(*left).span);
                    }
                }
            }
            StmtKind::Expr(e) => {
                self.loans(fx, w, *e);
            }
        }
    }

    fn mark_loop(w: &mut Walk, region: (u32, u32)) {
        if w.pass == Pass::First {
            w.loops.push(region);
        }
    }

    /// Walks an assignment's target: its indexes are read, and so is the
    /// target itself unless the assignment replaces it whole.
    fn target(&mut self, fx: &Fx, w: &mut Walk, left: ExprId, whole: bool) {
        let parts = spine(fx, left);
        for (i, &p) in parts.iter().enumerate() {
            match &fx.ast.expr(p).kind {
                ExprKind::Index {
                    index: Index::Expr(at),
                    ..
                } => {
                    self.loans(fx, w, *at);
                }
                ExprKind::Name(_) if !(whole && i == 0) => {
                    self.use_name(fx, w, p);
                }
                _ => {}
            }
        }
    }

    /// The loans in `loans` go into the place `path`: a local that
    /// borrows gains them, and a parameter must not keep a view of
    /// anything but other parameters.
    fn store(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        path: &(Root, Vec<Step>),
        loans: &[Loan],
        added: u32,
        span: Span,
    ) {
        let Root::Local(id) = path.0 else {
            return;
        };
        if path.1.contains(&Step::Deref) {
            return;
        }
        if w.params.contains_key(&id) {
            if w.pass == Pass::Report {
                for loan in loans {
                    if let Some(message) = self.escapes(fx, w, &loan.source, Some(id)) {
                        w.problems.push((span, message));
                    }
                }
            }
            return;
        }
        if let Some(&b) = w.of_local.get(&id) {
            if w.pass == Pass::Report && loans.iter().any(|l| l.source == Source::Temp) {
                w.problems.push((span, TEMPORARY.to_owned()));
            }
            Self::gain(w, b, loans, added);
        }
    }

    fn gain(w: &mut Walk, b: usize, loans: &[Loan], added: u32) {
        for loan in loans {
            let borrower = &mut w.borrowers[b];
            match borrower.loans.iter_mut().find(|l| l.source == loan.source) {
                Some(known) => {
                    if added < known.added {
                        known.added = added;
                    }
                }
                None => {
                    borrower.loans.push(Loan {
                        source: loan.source.clone(),
                        added,
                    });
                    w.grew = true;
                }
            }
        }
    }

    /// Why a loan cannot leave the function, in a result or in the
    /// `inout` parameter `into`, if it cannot. A result can point into
    /// any parameter, and an `inout` parameter only into itself, so a
    /// caller knows what each borrows without looking inside.
    fn escapes(&self, fx: &Fx, w: &Walk, source: &Source, into: Option<LocalId>) -> Option<String> {
        let what = match source {
            Source::Temp => return Some(TEMPORARY.to_owned()),
            Source::Place(Root::Local(id), _) if into == Some(*id) => return None,
            Source::Place(Root::Local(id), _) if into.is_none() && w.params.contains_key(id) => {
                return None;
            }
            Source::Place(Root::Local(id), _) if w.params.contains_key(id) => format!(
                "the parameter `{}`; an `inout` parameter keeps only views of itself",
                self.local_name(fx, *id)
            ),
            Source::Place(Root::Local(id), _) => format!(
                "`{}`, which is gone when the function returns",
                self.local_name(fx, *id)
            ),
            Source::Place(Root::Global(g), _) => format!(
                "the `var` global `{}`, which can change after the function returns",
                self.global_name(*g)
            ),
        };
        Some(match into {
            None => format!(
                "this returns a view of {what}; return owned values or a view of a parameter"
            ),
            Some(param) => format!(
                "`{}` would keep a view of {what}",
                self.local_name(fx, param)
            ),
        })
    }

    fn global_name(&self, g: DeclRef) -> &str {
        let decl = &self.program.file(g.file).ast.decls[g.index];
        let span = match &decl.kind {
            DeclKind::Global { local, .. } => match &local.binding {
                Binding::Name { name: Some(n), .. } => n.span,
                _ => decl.span,
            },
            DeclKind::ExternVar { name, .. } => name.span,
            _ => decl.span,
        };
        self.text(g.file, span)
    }

    /// A local bound to the value `value`, whose loans are `loans`. The
    /// borrow starts at `declared`, once the value is computed.
    fn bind_loans(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        name: Span,
        value: ExprId,
        loans: &[Loan],
        declared: u32,
    ) {
        let Some(&id) = self.names.files[fx.file.0 as usize]
            .bindings
            .get(&name.start)
        else {
            return;
        };
        let Some(info) = fx.locals.get(&id) else {
            return;
        };
        let ty = info.ty;
        let aliases = !info.owned && self.owning(ty);
        let mut all: Vec<Loan> = Vec::new();
        if self.holds_view(ty) {
            all.extend(loans.iter().cloned());
        }
        if aliases {
            all.extend(self.lend(fx, w, value, declared));
            all.extend(loans.iter().cloned());
        }
        if !self.holds_view(ty) && !aliases {
            return;
        }
        if w.pass == Pass::Report && all.iter().any(|l| l.source == Source::Temp) {
            w.problems
                .push((fx.ast.expr(value).span, TEMPORARY.to_owned()));
        }
        let scope = *w.scopes.last().expect("a binding is in a block");
        let declared = declared.max(scope.0);
        Self::borrower(
            w,
            id,
            self.local_name(fx, id),
            aliases,
            declared,
            scope,
            &all,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn borrower(
        w: &mut Walk,
        id: LocalId,
        name: String,
        aliases: bool,
        declared: u32,
        scope: (u32, u32),
        loans: &[Loan],
    ) {
        let b = match w.of_local.get(&id) {
            Some(&b) => b,
            None => {
                w.borrowers.push(Borrower {
                    local: Some(id),
                    name,
                    aliases,
                    declared,
                    scope,
                    loans: Vec::new(),
                });
                w.of_local.insert(id, w.borrowers.len() - 1);
                w.borrowers.len() - 1
            }
        };
        Self::gain(w, b, loans, declared);
    }

    fn bind_fields(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        fields: &[DestructField],
        value: ExprId,
        loans: &[Loan],
        declared: u32,
    ) {
        for field in fields {
            if field.ty.is_some() {
                let name = field.rename.unwrap_or(field.field);
                self.bind_loans(fx, w, name.span, value, loans, declared);
            }
        }
    }

    fn bind_pattern(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        p: PatId,
        subject: ExprId,
        loans: &[Loan],
        scope: (u32, u32),
    ) {
        let PatternKind::Variant {
            payload: Some(fields),
            ..
        } = &fx.ast.pat(p).kind
        else {
            return;
        };
        w.scopes.push(scope);
        self.bind_fields(fx, w, fields, subject, loans, scope.0);
        w.scopes.pop();
    }

    fn walk_for_in(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        binding: &Binding,
        e: ExprId,
        body: (u32, u32),
    ) {
        let mut loans = self.loans(fx, w, e);
        let Some(ty) = self.ty_of(fx, e) else {
            return;
        };
        if matches!(self.types.get(ty), Ty::Array(..) | Ty::Growable(_)) {
            loans.extend(self.lend(fx, w, e, body.0));
        }
        if !loans.is_empty() {
            let name = "the `for` loop".to_owned();
            match w
                .borrowers
                .iter()
                .position(|b| b.local.is_none() && b.declared == body.0)
            {
                Some(b) => Self::gain(w, b, &loans, body.0),
                None => {
                    w.borrowers.push(Borrower {
                        local: None,
                        name,
                        aliases: false,
                        declared: body.0,
                        scope: body,
                        loans: Vec::new(),
                    });
                    let b = w.borrowers.len() - 1;
                    Self::gain(w, b, &loans, body.0);
                }
            }
        }
        w.scopes.push(body);
        match binding {
            Binding::Name {
                name: Some(name), ..
            } => {
                self.bind_loans(fx, w, name.span, e, &loans, body.0);
            }
            Binding::Name { name: None, .. } => {}
            Binding::Destructure(fields) => self.bind_fields(fx, w, fields, e, &loans, body.0),
        }
        w.scopes.pop();
    }

    // Expressions.

    fn use_name(&self, fx: &Fx, w: &mut Walk, e: ExprId) {
        if w.pass != Pass::First {
            return;
        }
        if let Some(Res::Local(id)) = self.names.files[fx.file.0 as usize].exprs[e.index()] {
            let at = Self::here(w, fx.ast.expr(e).span);
            w.uses.entry(id).or_default().push(at);
        }
    }

    /// What `e` lends when a view of its storage is taken: its own path,
    /// what a local that aliases lends, or a temporary.
    fn lend(&self, fx: &Fx, w: &Walk, e: ExprId, added: u32) -> Vec<Loan> {
        match &fx.ast.expr(e).kind {
            ExprKind::Paren(inner) | ExprKind::Unsafe(inner) | ExprKind::Try(inner) => {
                return self.lend(fx, w, *inner, added);
            }
            ExprKind::If {
                then, otherwise, ..
            } => {
                let mut out = self.lend(fx, w, *then, added);
                let other = match otherwise {
                    IfElse::Branch(x) | IfElse::If(x) => *x,
                };
                out.extend(self.lend(fx, w, other, added));
                return out;
            }
            ExprKind::Match { arms, .. } => {
                return arms
                    .iter()
                    .flat_map(|a| self.lend(fx, w, a.body, added))
                    .collect();
            }
            _ => {}
        }
        let Some(path) = self.access_path(fx, e) else {
            return vec![Loan {
                source: Source::Temp,
                added,
            }];
        };
        if path.1.contains(&Step::Deref) {
            return Vec::new();
        }
        match path.0 {
            Root::Local(id) => {
                if let Some(&b) = w.of_local.get(&id)
                    && w.borrowers[b].aliases
                {
                    return w.borrowers[b].loans.clone();
                }
            }
            Root::Global(g) => {
                let mutable = match &self.program.file(g.file).ast.decls[g.index].kind {
                    DeclKind::Global { shared, local } => *shared || local.mutable,
                    DeclKind::ExternVar { .. } => true,
                    _ => false,
                };
                if !mutable {
                    return Vec::new();
                }
            }
        }
        vec![Loan {
            source: Source::Place(path.0, path.1),
            added,
        }]
    }

    /// The loans of the value of `e`, walking it.
    fn loans(&mut self, fx: &Fx, w: &mut Walk, e: ExprId) -> Vec<Loan> {
        let mut out = self.loans_inner(fx, w, e);
        if fx.viewed.contains(&e) {
            let at = fx.ast.expr(e).span.start;
            out.extend(self.lend(fx, w, e, at));
        }
        out
    }

    fn loans_inner(&mut self, fx: &Fx, w: &mut Walk, e: ExprId) -> Vec<Loan> {
        let expr = fx.ast.expr(e);
        let res = self.names.files[fx.file.0 as usize].exprs[e.index()];
        match &expr.kind {
            ExprKind::Name(_) => {
                self.use_name(fx, w, e);
                match res {
                    Some(Res::Local(id)) => match w.of_local.get(&id) {
                        Some(&b) => w.borrowers[b].loans.clone(),
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                }
            }
            ExprKind::Int | ExprKind::Float | ExprKind::Char => Vec::new(),
            ExprKind::Str(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    if let StrPart::Interpolation(x) = part {
                        out.extend(self.loans(fx, w, *x));
                        if self
                            .ty_of(fx, *x)
                            .is_some_and(|t| matches!(self.types.get(t), Ty::Growable(_)))
                        {
                            out.extend(self.lend(fx, w, *x, fx.ast.expr(*x).span.start));
                        }
                    }
                }
                out
            }
            ExprKind::Paren(inner) | ExprKind::Unsafe(inner) | ExprKind::Try(inner) => {
                self.loans(fx, w, *inner)
            }
            ExprKind::Tuple(fields) => fields
                .iter()
                .flat_map(|(_, x)| self.loans(fx, w, *x))
                .collect(),
            ExprKind::Array(items) => items.iter().flat_map(|x| self.loans(fx, w, *x)).collect(),
            ExprKind::Construct { value, .. } => self.loans(fx, w, *value),
            ExprKind::Field { value, .. } => {
                if matches!(res, Some(Res::Decl(_))) {
                    return Vec::new();
                }
                self.loans(fx, w, *value)
            }
            ExprKind::Index { value, index } => {
                let mut out = self.loans(fx, w, *value);
                match index {
                    Index::Expr(i) => {
                        self.loans(fx, w, *i);
                    }
                    Index::Slice { start, end } => {
                        for x in [start, end].into_iter().flatten() {
                            self.loans(fx, w, *x);
                        }
                        let base = self.ty_of(fx, *value);
                        if base.is_some_and(|t| {
                            matches!(self.types.get(t), Ty::Array(..) | Ty::Growable(_))
                        }) {
                            out.extend(self.lend(fx, w, *value, expr.span.start));
                        }
                    }
                    Index::Type(_) => {}
                }
                out
            }
            ExprKind::Deref(inner) => {
                self.loans(fx, w, *inner);
                Vec::new()
            }
            ExprKind::Unary { operand, op } => {
                if *op == UnaryOp::AddressOf {
                    self.target(fx, w, *operand, false);
                } else {
                    self.loans(fx, w, *operand);
                }
                Vec::new()
            }
            ExprKind::Binary { left, right, .. } => {
                self.loans(fx, w, *left);
                self.loans(fx, w, *right);
                Vec::new()
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.loans(fx, w, *cond);
                let mut out = self.loans(fx, w, *then);
                let other = match otherwise {
                    IfElse::Branch(x) | IfElse::If(x) => *x,
                };
                out.extend(self.loans(fx, w, other));
                out
            }
            ExprKind::Match { value, arms } => {
                let subject = self.loans(fx, w, *value);
                let mut out = Vec::new();
                for arm in arms {
                    let span = fx.ast.expr(arm.body).span;
                    for &p in &arm.patterns {
                        self.bind_pattern(fx, w, p, *value, &subject, (span.start, span.end));
                    }
                    out.extend(self.loans(fx, w, arm.body));
                }
                out
            }
            ExprKind::Call { callee, args } => self.call_loans(fx, w, e, *callee, args),
        }
    }

    fn call_loans(
        &mut self,
        fx: &Fx,
        w: &mut Walk,
        e: ExprId,
        callee: ExprId,
        args: &[crate::ast::Arg],
    ) -> Vec<Loan> {
        let start = Self::here(w, fx.ast.expr(e).span);
        // `cast[*u8](p)` names the built-in through an index.
        let named = match &fx.ast.expr(callee).kind {
            ExprKind::Index { value, .. } => *value,
            _ => callee,
        };
        let res = self.names.files[fx.file.0 as usize].exprs[named.index()];
        let trusted = matches!(
            res,
            Some(Res::Builtin(
                "from_pointer" | "from_address" | "cast" | "offset" | "c_string"
            ))
        );
        if !matches!(res, Some(Res::Builtin(_))) {
            self.loans(fx, w, callee);
        }
        if w.pass == Pass::First
            && let Some(Res::Decl(d)) = res
            && matches!(
                self.program.file(d.file).ast.decls[d.index].kind,
                DeclKind::Fn(_)
            )
        {
            w.calls.push((d, fx.ast.expr(callee).span, start));
        }
        // What each argument can lend the result.
        let mut lent: Vec<Vec<Loan>> = Vec::new();
        for arg in args {
            let mut loans = self.loans(fx, w, arg.value);
            let ty = self.ty_of(fx, arg.value);
            if ty.is_some_and(|t| self.has_storage(t)) && !fx.viewed.contains(&arg.value) {
                loans.extend(self.lend(fx, w, arg.value, start));
            }
            if arg.mode == Mode::Inout
                && w.pass == Pass::First
                && let Some(path) = self.access_path(fx, arg.value)
            {
                w.changes.push(Change {
                    path,
                    at: start,
                    span: fx.ast.expr(arg.value).span,
                });
            }
            lent.push(loans);
        }
        if trusted || !self.ty_of(fx, e).is_some_and(|t| self.holds_view(t)) {
            return Vec::new();
        }
        lent.into_iter().flatten().collect()
    }

    // The check.

    /// Whether `b`'s `loan` is in use at `at`: the local is used after
    /// it, or in a loop around it that the local outlives.
    fn active(w: &Walk, b: &Borrower, loan: &Loan, at: u32) -> bool {
        if at < b.declared || at > b.scope.1 || at < b.scope.0 {
            return false;
        }
        let Some(id) = b.local else {
            return true;
        };
        let uses: &[u32] = w.uses.get(&id).map_or(&[], |u| u.as_slice());
        if loan.added < at && uses.iter().any(|&u| u > at) {
            return true;
        }
        w.loops.iter().any(|&(start, end)| {
            start <= at
                && at <= end
                && start > b.declared
                && end <= b.scope.1
                && uses.iter().any(|&u| start <= u && u <= end)
                && (loan.added < at || (start <= loan.added && loan.added <= end))
        })
    }

    fn conflicts(&mut self, fx: &Fx, w: &mut Walk) {
        for change in &w.changes {
            let found = w.borrowers.iter().find(|b| {
                b.loans.iter().any(|l| {
                    matches!(&l.source, Source::Place(root, steps) if overlaps(&(*root, steps.clone()), &change.path))
                        && Self::active(w, b, l, change.at)
                })
            });
            if let Some(b) = found {
                let place = self.text(fx.file, change.span);
                let message = match b.local {
                    Some(_) => format!(
                        "`{place}` cannot change here: `{}` borrows it and is used later",
                        b.name
                    ),
                    None => format!("`{place}` cannot change inside a `for` loop over it"),
                };
                w.problems.push((change.span, message));
            }
        }
        for &(callee, span, at) in &w.calls {
            for b in &w.borrowers {
                for l in &b.loans {
                    if let Source::Place(Root::Global(g), _) = &l.source
                        && Self::active(w, b, l, at)
                    {
                        self.global_loans.push(GlobalLoan {
                            file: fx.file,
                            span,
                            callee,
                            global: *g,
                            borrower: b.local.map(|_| b.name.clone()),
                        });
                    }
                }
            }
        }
    }
}
