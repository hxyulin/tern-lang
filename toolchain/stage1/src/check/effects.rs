//! Which globals each function writes, and the two rules that use it:
//! no argument of a call aliases a global the callee writes, and a
//! function used as a value writes no globals, since a function type
//! writes none.
//!
//! The checker records what each function does directly while it checks
//! the body. After every body is checked, the write sets are closed over
//! the call graph and the rules are checked.

use std::collections::HashMap;

use super::{Checker, Fx};
use crate::ast::{Binding, DeclKind, ExprId};
use crate::package::FileId;
use crate::resolve::{DeclRef, Res};
use crate::source::Span;

/// What one function does directly.
#[derive(Default)]
pub(super) struct Direct {
    /// The globals it assigns, or passes `inout`, in the order found.
    writes: Vec<DeclRef>,
    calls: Vec<CallSite>,
}

/// A call to a function declaration.
struct CallSite {
    callee: DeclRef,
    callee_span: Span,
    /// Each argument that starts at a global: the argument, the global,
    /// and the part of the argument that names the global.
    args: Vec<(Span, DeclRef, Span)>,
}

/// A function declaration used as a value, not called.
pub(super) struct ValueUse {
    file: FileId,
    span: Span,
    decl: DeclRef,
}

impl Checker<'_> {
    /// The global a place starts at, with the expression that names it,
    /// unless a dereference leaves the global's own storage.
    fn global_of(&self, fx: &Fx, e: ExprId) -> Option<(DeclRef, Span)> {
        let path = self.access_path(fx, e)?;
        let super::rules::Root::Global(global) = path.0 else {
            return None;
        };
        if path.1.contains(&super::rules::Step::Deref) {
            return None;
        }
        let res = &self.names.files[fx.file.0 as usize].exprs;
        let named = super::rules::spine(fx, e)
            .into_iter()
            .rev()
            .find(|x| matches!(res[x.index()], Some(Res::Decl(d)) if d == global))?;
        Some((global, fx.ast.expr(named).span))
    }

    /// The function being checked changes the place `e`.
    pub(super) fn writes(&mut self, fx: &Fx, e: ExprId) {
        let (Some(decl), Some((global, _))) = (fx.decl, self.global_of(fx, e)) else {
            return;
        };
        self.direct.entry(decl).or_default().writes.push(global);
    }

    /// Records a call whose callee is the expression `callee`, if that
    /// names a function declaration.
    pub(super) fn direct_call(&mut self, fx: &Fx, callee: ExprId, args: &[ExprId]) {
        let Some(decl) = fx.decl else {
            return;
        };
        let Some(Res::Decl(target)) = self.names.files[fx.file.0 as usize].exprs[callee.index()]
        else {
            return;
        };
        if !matches!(
            self.program.file(target.file).ast.decls[target.index].kind,
            DeclKind::Fn(_)
        ) {
            return;
        }
        let args = args
            .iter()
            .filter_map(|&a| {
                let (global, named) = self.global_of(fx, a)?;
                Some((fx.ast.expr(a).span, global, named))
            })
            .collect();
        self.direct.entry(decl).or_default().calls.push(CallSite {
            callee: target,
            callee_span: fx.ast.expr(callee).span,
            args,
        });
    }

    /// A use of a top-level name that is not a call's callee: a function
    /// used as a value.
    pub(super) fn maybe_value_use(&mut self, fx: &Fx, e: ExprId, decl: DeclRef, span: Span) {
        if fx.callees.contains(&e) {
            return;
        }
        if matches!(
            self.program.file(decl.file).ast.decls[decl.index].kind,
            DeclKind::Fn(_)
        ) {
            self.values.push(ValueUse {
                file: fx.file,
                span,
                decl,
            });
        }
    }

    /// Closes the write sets over the call graph, then checks calls and
    /// function values against them.
    pub(super) fn check_effects(&mut self) {
        let mut callers: Vec<DeclRef> = self.direct.keys().copied().collect();
        callers.sort_by_key(|d| (d.file.0, d.index));
        let mut writes: HashMap<DeclRef, Vec<DeclRef>> = callers
            .iter()
            .map(|&d| (d, self.direct[&d].writes.clone()))
            .collect();
        loop {
            let mut changed = false;
            for &caller in &callers {
                for call in &self.direct[&caller].calls {
                    let theirs = writes.get(&call.callee).cloned().unwrap_or_default();
                    let ours = writes.entry(caller).or_default();
                    for g in theirs {
                        if !ours.contains(&g) {
                            ours.push(g);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for set in writes.values_mut() {
            set.sort_by_key(|d| (d.file.0, d.index));
        }
        let mut problems = Vec::new();
        for &caller in &callers {
            for call in &self.direct[&caller].calls {
                let Some(set) = writes.get(&call.callee) else {
                    continue;
                };
                for &(span, global, named) in &call.args {
                    if set.contains(&global) {
                        let message = format!(
                            "`{}` writes `{}`, so `{}` cannot also be its argument: an argument must not alias a global the function writes",
                            self.text(caller.file, call.callee_span),
                            self.text(caller.file, named),
                            self.text(caller.file, span),
                        );
                        problems.push((caller.file, span, message));
                    }
                }
            }
        }
        for value in &self.values {
            let Some(&global) = writes.get(&value.decl).and_then(|s| s.first()) else {
                continue;
            };
            let message = format!(
                "`{}` writes {}, so it cannot be a function value: a function type writes no globals",
                self.text(value.file, value.span),
                self.global_shown(global, value.file),
            );
            problems.push((value.file, value.span, message));
        }
        for loan in std::mem::take(&mut self.global_loans) {
            if writes
                .get(&loan.callee)
                .is_some_and(|set| set.contains(&loan.global))
            {
                let callee = self.text(loan.file, loan.span);
                let global = self.global_shown(loan.global, loan.file);
                let message = match &loan.borrower {
                    Some(who) => {
                        format!("`{callee}` writes {global}, which `{who}` borrows and uses later")
                    }
                    None => format!("`{callee}` writes {global}, which the `for` loop borrows"),
                };
                problems.push((loan.file, loan.span, message));
            }
        }
        for (file, span, message) in problems {
            self.error(file, span, message);
        }
    }

    /// A global as a message names it from `file`: by name in its own
    /// package, by its package and name if it is `pub`, and otherwise as
    /// its package's state, which keeps private globals private.
    fn global_shown(&self, global: DeclRef, file: FileId) -> String {
        let decl = &self.program.file(global.file).ast.decls[global.index];
        let span = match &decl.kind {
            DeclKind::Global { local, .. } => match &local.binding {
                Binding::Name { name: Some(n), .. } => n.span,
                Binding::Name { name: None, .. } => decl.span,
                Binding::Destructure(fields) => fields
                    .first()
                    .map_or(decl.span, |f| f.rename.unwrap_or(f.field).span),
            },
            DeclKind::ExternVar { name, .. } => name.span,
            _ => decl.span,
        };
        let name = self.text(global.file, span);
        let package = self.program.file(global.file).package;
        let path = &self.program.packages[package.0 as usize].path;
        if package == self.program.file(file).package {
            format!("`{name}`")
        } else if decl.public {
            format!("`{path}.{name}`")
        } else {
            format!("the state of `{path}`")
        }
    }
}
