//! Moves. A value passed `sink` cannot be used afterward, and a type that
//! holds a `[..]T` moves instead of copying.
//!
//! The check follows each function's control flow. It records the locals
//! moved on the way to each point; branches join by taking every local
//! moved on any path that reaches the join. A move of a whole local is
//! the only kind: a field, element or global cannot be moved out of.

use std::collections::HashMap;

use super::{Checker, ExprKind, Fx, Owner, Res};
use crate::ast::{Block, ExprId, Mode};
use crate::resolve::LocalId;
use crate::source::Span;
use crate::types::{NamedDef, NamedId, Ty, TyId};

/// Moved locals, each with where it moved.
pub(super) type Moved = HashMap<LocalId, Span>;

/// What a loop's `break`s and `continue`s carry to where they go.
#[derive(Default)]
pub(super) struct LoopFlow {
    pub breaks: Option<Moved>,
    pub continues: Option<Moved>,
}

pub(super) fn union_into(target: &mut Option<Moved>, moved: &Moved) {
    let target = target.get_or_insert_with(Moved::new);
    for (&id, &span) in moved {
        target.entry(id).or_insert(span);
    }
}

/// The locals moved after branches, each given with the state at its end
/// and whether it never finishes. `before` is the result when no branch
/// finishes.
pub(super) fn join(paths: Vec<(Moved, bool)>, before: Moved) -> Moved {
    let mut joined: Option<Moved> = None;
    for (moved, diverges) in paths {
        if !diverges {
            union_into(&mut joined, &moved);
        }
    }
    joined.unwrap_or(before)
}

impl<'a> Checker<'a> {
    /// Whether values of `ty` move instead of copying: types that hold a
    /// `[..]T`, which owns memory.
    pub(super) fn move_only(&mut self, ty: TyId) -> bool {
        if let Some(&known) = self.move_only.get(&ty) {
            return known;
        }
        // Assume not while computing, so a type that refers to itself
        // through a pointer or view ends.
        self.move_only.insert(ty, false);
        let parts: Vec<TyId> = match self.types.get(ty) {
            Ty::Growable(_) => {
                self.move_only.insert(ty, true);
                return true;
            }
            Ty::Tuple(fields) => fields.iter().map(|(_, t)| *t).collect(),
            Ty::Array(_, elem) | Ty::Optional(elem) => vec![*elem],
            Ty::Result(value, error) => value.iter().copied().chain([*error]).collect(),
            Ty::Named(NamedId::Decl(_)) => match self.named_info(ty).map(|i| &i.def) {
                Some(NamedDef::Plain { underlying, .. }) => vec![*underlying],
                Some(NamedDef::Enum(info)) => {
                    info.variants.iter().filter_map(|v| v.payload).collect()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let result = parts.into_iter().any(|t| self.move_only(t));
        self.move_only.insert(ty, result);
        result
    }

    /// `e` is used where its value is kept: bound, returned, assigned,
    /// stored in a construction or pushed. If the type moves, a local
    /// named there is moved.
    pub(super) fn consume(&mut self, fx: &mut Fx, e: ExprId, target: TyId) {
        if !self.move_only(target) {
            return;
        }
        let span = fx.ast.expr(e).span;
        let res = self.names.files[fx.file.0 as usize].exprs[e.index()];
        match &fx.ast.expr(e).kind {
            ExprKind::Paren(inner) | ExprKind::Unsafe(inner) => self.consume(fx, *inner, target),
            ExprKind::If {
                then, otherwise, ..
            } => {
                let other = match otherwise {
                    crate::ast::IfElse::Branch(e) | crate::ast::IfElse::If(e) => *e,
                };
                self.consume(fx, *then, target);
                self.consume(fx, other, target);
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.consume(fx, arm.body, target);
                }
            }
            ExprKind::Name(_) | ExprKind::Field { .. } if matches!(res, Some(Res::Decl(_))) => {
                let message = format!(
                    "cannot move a `{}` out of a global; it would leave the global without a value",
                    self.show(target)
                );
                self.error(fx.file, span, message);
            }
            ExprKind::Name(_) => {
                if let Some(Res::Local(id)) = res {
                    self.move_local(fx, id, span);
                }
            }
            ExprKind::Field { .. } | ExprKind::Index { .. } | ExprKind::Deref(_) => {
                let message = format!(
                    "cannot move a `{}` out of a field, element or pointer; move the whole value, or destructure it",
                    self.show(target)
                );
                self.error(fx.file, span, message);
            }
            _ => {}
        }
    }

    /// An argument passed `sink`: a local is moved whatever its type.
    pub(super) fn sink(&mut self, fx: &mut Fx, e: ExprId, param: TyId) {
        let inner = super::paren_inner(fx, e);
        let res = self.names.files[fx.file.0 as usize].exprs[inner.index()];
        match (&fx.ast.expr(inner).kind, res) {
            (ExprKind::Name(_), Some(Res::Local(id))) => {
                let span = fx.ast.expr(inner).span;
                self.move_local(fx, id, span);
            }
            _ => self.consume(fx, e, param),
        }
    }

    fn move_local(&mut self, fx: &mut Fx, id: LocalId, span: Span) {
        let Some(info) = fx.locals.get(&id) else {
            return;
        };
        let (ty, owner) = (info.ty, info.owner);
        let name = self.local_name(fx, id);
        let moves = self.move_only(ty);
        let problem = match owner {
            Owner::Own => None,
            Owner::Part(whole) => {
                if moves {
                    self.move_local(fx, whole, span);
                }
                None
            }
            _ if !moves => None,
            Owner::Param(Mode::Inout) => Some(format!(
                "cannot move `{name}` out of an `inout` parameter: the caller still owns it"
            )),
            Owner::Param(_) => Some(format!(
                "cannot move `{name}`: the parameter is borrowed; declare it `sink` to own it"
            )),
            Owner::Loop => Some(format!(
                "cannot move `{name}`: a loop's binding borrows from what the loop goes over"
            )),
            Owner::Borrowed => Some(format!(
                "cannot move `{name}`: it is part of a field, element or global"
            )),
        };
        if let Some(message) = problem {
            self.error(fx.file, span, message);
            return;
        }
        if let Some(&at) = fx.deferred.get(&id) {
            let line = self.line(fx, at);
            let message =
                format!("`{name}` is already passed `sink` by the `defer` on line {line}");
            self.error(fx.file, span, message);
        }
        fx.moved.entry(id).or_insert(span);
    }

    /// A read of a local, which must not have moved.
    pub(super) fn use_local(&mut self, fx: &mut Fx, e: ExprId, id: LocalId, span: Span) {
        if fx.assign_target == Some(e) {
            return;
        }
        // Reported once: later uses are the same mistake.
        if let Some(at) = fx.moved.remove(&id) {
            let name = self.local_name(fx, id);
            let line = self.line(fx, at);
            self.error(
                fx.file,
                span,
                format!("`{name}` is used after it was moved on line {line}"),
            );
        }
    }

    /// Who owns the payloads a `match` on `value` binds.
    pub(super) fn payload_owner(&self, fx: &Fx, value: ExprId) -> Owner {
        let inner = super::paren_inner(fx, value);
        let res = self.names.files[fx.file.0 as usize].exprs[inner.index()];
        match (&fx.ast.expr(inner).kind, res) {
            (ExprKind::Name(_), Some(Res::Local(id))) => Owner::Part(id),
            (
                ExprKind::Name(_)
                | ExprKind::Field { .. }
                | ExprKind::Index { .. }
                | ExprKind::Deref(_),
                _,
            ) => Owner::Borrowed,
            _ => Owner::Own,
        }
    }

    /// Checks a loop body, then the moves it carries into the next pass.
    pub(super) fn loop_body(&mut self, fx: &mut Fx, body: &Block, forever: bool) -> bool {
        let before = fx.moved.clone();
        fx.loops.push(LoopFlow::default());
        let diverges = self.block(fx, body);
        let flow = fx.loops.pop().expect("pushed above");
        let mut back = flow.continues;
        if !diverges {
            union_into(&mut back, &fx.moved);
        }
        let back = back.unwrap_or_default();
        // A local declared outside the loop and moved on a path back to
        // the top would be used moved by the next pass.
        let depth = fx.loops.len();
        let mut carried: Vec<(LocalId, Span)> = back
            .iter()
            .filter(|(id, _)| {
                !before.contains_key(id) && fx.locals.get(id).is_some_and(|l| l.loop_depth <= depth)
            })
            .map(|(id, span)| (*id, *span))
            .collect();
        carried.sort_by_key(|(_, span)| span.start);
        for (id, span) in carried {
            let name = self.local_name(fx, id);
            self.error(
                fx.file,
                span,
                format!("`{name}` is moved inside a loop, so the next pass would use it moved; give it a new value before the pass ends"),
            );
        }
        let has_break = flow.breaks.is_some();
        let mut after = flow.breaks;
        if !forever {
            union_into(&mut after, &before);
            union_into(&mut after, &back);
        }
        fx.moved = after.unwrap_or(before);
        forever && !has_break
    }

    fn local_name(&self, fx: &Fx, id: LocalId) -> &'a str {
        let span = self.names.files[fx.file.0 as usize].locals[id.0 as usize];
        self.text(fx.file, span)
    }

    fn line(&self, fx: &Fx, span: Span) -> u32 {
        self.program.file(fx.file).source.position(span.start).line
    }
}
