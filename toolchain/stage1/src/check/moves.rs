//! Moves. A value passed `sink` cannot be used afterward, and a `move
//! type`, or a type that holds one or a `[..]T`, moves instead of copying.
//!
//! The check follows each function's control flow. It records the places
//! moved on the way to each point; branches join by taking every place
//! moved on any path that reaches the join. A place is a local the
//! function owns, or a field of one, as in `free(sink l.tokens)`: the
//! other fields stay usable, and assigning the field restores it. An
//! element, a global, or what a pointer or borrowed parameter holds
//! cannot be moved out of.

use std::collections::HashMap;

use super::{Checker, ExprKind, Fx, Owner, Res};
use crate::ast::{Block, ExprId, Mode};
use crate::resolve::LocalId;
use crate::source::Span;
use crate::types::{NamedDef, NamedId, Ty, TyId};

/// A local, or a field of it reached through `.name` steps.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Place {
    pub local: LocalId,
    pub fields: Vec<String>,
}

impl Place {
    pub fn whole(local: LocalId) -> Place {
        Place {
            local,
            fields: Vec::new(),
        }
    }

    /// Whether one place holds the other, or they are the same.
    pub fn overlaps(&self, other: &Place) -> bool {
        self.local == other.local && self.fields.iter().zip(&other.fields).all(|(a, b)| a == b)
    }

    /// Whether `self` holds `other` and is not the same place.
    fn strictly_holds(&self, other: &Place) -> bool {
        self.local == other.local
            && self.fields.len() < other.fields.len()
            && other.fields.starts_with(&self.fields)
    }
}

/// Moved places, each with where it moved.
pub(super) type Moved = HashMap<Place, Span>;

/// What a loop's `break`s and `continue`s carry to where they go.
#[derive(Default)]
pub(super) struct LoopFlow {
    pub breaks: Option<Moved>,
    pub continues: Option<Moved>,
}

pub(super) fn union_into(target: &mut Option<Moved>, moved: &Moved) {
    let target = target.get_or_insert_with(Moved::new);
    for (place, &span) in moved {
        target.entry(place.clone()).or_insert(span);
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
    /// Whether values of `ty` move instead of copying: `move type`s, and
    /// types that hold a `[..]T` or a `move type`.
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
            Ty::Named(NamedId::Decl(decl)) if self.declared_moving(*decl) => {
                self.move_only.insert(ty, true);
                return true;
            }
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

    fn declared_moving(&self, decl: crate::resolve::DeclRef) -> bool {
        matches!(
            self.program.file(decl.file).ast.decls[decl.index].kind,
            crate::ast::DeclKind::Type { moves: true, .. }
        )
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
                    self.move_place(fx, Place::whole(id), span);
                }
            }
            ExprKind::Field { .. } if self.local_place(fx, e).is_some() => {
                let (_, place) = self.local_place(fx, e).expect("checked above");
                self.move_place(fx, place, span);
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
                self.move_place(fx, Place::whole(id), span);
            }
            _ => self.consume(fx, e, param),
        }
    }

    /// The place `e` names: a local the function sees, or fields of one,
    /// with the name expression at its root.
    pub(super) fn local_place(&self, fx: &Fx, e: ExprId) -> Option<(ExprId, Place)> {
        match &fx.ast.expr(e).kind {
            ExprKind::Paren(inner) => self.local_place(fx, *inner),
            ExprKind::Name(_) => match self.names.files[fx.file.0 as usize].exprs[e.index()] {
                Some(Res::Local(id)) => Some((e, Place::whole(id))),
                _ => None,
            },
            ExprKind::Field { value, name } => {
                let (root, mut place) = self.local_place(fx, *value)?;
                place.fields.push(self.text(fx.file, name.span).to_owned());
                Some((root, place))
            }
            _ => None,
        }
    }

    fn place_name(&self, fx: &Fx, place: &Place) -> String {
        let mut name = self.local_name(fx, place.local).to_owned();
        for field in &place.fields {
            name.push('.');
            name.push_str(field);
        }
        name
    }

    fn move_place(&mut self, fx: &mut Fx, place: Place, span: Span) {
        let Some(info) = fx.locals.get(&place.local) else {
            return;
        };
        let (ty, owner) = (info.ty, info.owner);
        let name = self.place_name(fx, &place);
        let moves = self.move_only(ty);
        let problem = match owner {
            Owner::Own => None,
            Owner::Part(whole) => {
                if moves {
                    self.move_place(fx, Place::whole(whole), span);
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
        let deferred = fx.deferred.iter().find(|(p, _)| p.overlaps(&place));
        if let Some((p, &at)) = deferred {
            let line = self.line(fx, at);
            let what = self.place_name(fx, p);
            let message =
                format!("`{what}` is already passed `sink` by the `defer` on line {line}");
            self.error(fx.file, span, message);
        }
        fx.moved.entry(place).or_insert(span);
    }

    /// A use of a local, or of the fields `field()` recorded for it: no
    /// part of what it reads may have moved. An assignment's target may
    /// have moved, but not something that holds it.
    pub(super) fn use_local(&mut self, fx: &mut Fx, e: ExprId, id: LocalId, span: Span) {
        let used = Place {
            local: id,
            fields: fx.field_uses.remove(&e).unwrap_or_default(),
        };
        let assigning = fx.assign_target == Some(e);
        let mut conflicts: Vec<(Place, Span)> = fx
            .moved
            .iter()
            .filter(|(moved, _)| {
                if assigning {
                    moved.strictly_holds(&used)
                } else {
                    moved.overlaps(&used)
                }
            })
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        conflicts.sort_by_key(|(_, at)| at.start);
        // Reported once: later uses are the same mistake.
        let Some((moved, at)) = conflicts.into_iter().next() else {
            return;
        };
        fx.moved
            .retain(|p, _| !p.overlaps(&used) || (assigning && !p.strictly_holds(&used)));
        let used_name = self.place_name(fx, &used);
        let moved_name = self.place_name(fx, &moved);
        let line = self.line(fx, at);
        let message = if used_name == moved_name {
            format!("`{used_name}` is used after it was moved on line {line}")
        } else {
            format!("`{used_name}` is used after `{moved_name}` was moved on line {line}")
        };
        self.error(fx.file, span, message);
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
        let mut carried: Vec<(Place, Span)> = back
            .iter()
            .filter(|(place, _)| {
                !before.contains_key(place)
                    && fx
                        .locals
                        .get(&place.local)
                        .is_some_and(|l| l.loop_depth <= depth)
            })
            .map(|(place, span)| (place.clone(), *span))
            .collect();
        carried.sort_by_key(|(_, span)| span.start);
        for (place, span) in carried {
            let name = self.place_name(fx, &place);
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
