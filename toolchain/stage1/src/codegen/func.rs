//! Lowering one function body.
//!
//! Each expression lowers to a [`V`]: a keel value for a scalar, or the
//! address of its bytes. An address may point into a variable, so code
//! that keeps a value (a binding, an assignment, a field of a new record)
//! copies the bytes.

// Lowering names values as keel does (`a`, `b`, `x`, `v`), and many come
// in pairs such as `l` and `r`.
#![allow(clippy::many_single_char_names, clippy::similar_names)]

use std::collections::{HashMap, HashSet};

use keel_ir::{
    Block, ConvertOp, FloatCC, Function, InsertPoint, InstBuilder, IntCC, Intrinsic, OverflowOp,
    Signature, StackSlot, Symbol, Type, Value,
};

use super::data::{string_bytes, unescape};
use super::layout::{EnumKind, Repr, int_type, type_size};
use super::{Gen, MessagePart, Pass};
use crate::ast::{
    Arg, AssignOp, Ast, BinaryOp, Binding, Block as AstBlock, DeclKind, DestructField, Else,
    ExprId, ExprKind, ForKind, IfElse, Index, Iterable, PatId, PatternKind, StmtId, StmtKind,
    StrPart, UnaryOp,
};
use crate::check::Item;
use crate::package::FileId;
use crate::resolve::{DeclRef, LocalId, Res};
use crate::source::Span;
use crate::types::{BuiltinNamed, IntTy, NamedId, Ty, TyId};

/// A lowered value.
#[derive(Clone, Copy, Debug)]
pub enum V {
    S(Value),
    /// The address of the bytes.
    M(Value),
    Unit,
}

#[derive(Clone, Copy)]
enum Addr {
    Slot(StackSlot),
    Ptr(Value),
}

struct Loop {
    brk: Block,
    cont: Block,
    /// How many `defer` scopes were open when the loop started.
    scopes: usize,
}

/// One piece of a compiler-written panic message.
enum Piece<'s> {
    Text(&'s str),
    Unsigned(Value),
    Signed(Value),
}

pub struct FnGen<'g, 'a> {
    g: &'g mut Gen<'a>,
    f: Function,
    file: FileId,
    ast: &'a Ast,
    fn_name: &'a str,
    block: Block,
    /// Whether code emitted now can run.
    reachable: bool,
    terminated: HashSet<Block>,
    locals: HashMap<LocalId, Addr>,
    result: Option<TyId>,
    sret: Option<Value>,
    defers: Vec<Vec<ExprId>>,
    loops: Vec<Loop>,
    /// Join blocks some reachable code jumps to.
    joined: HashSet<Block>,
    /// The block where the next `match` arm's test starts.
    pending_next: Option<Block>,
    /// The function's place in the module, taken when it starts.
    index: usize,
}

impl Gen<'_> {
    pub fn function(&mut self, decl: DeclRef) {
        let DeclKind::Fn(fn_decl) = self.decl_kind(decl) else {
            return;
        };
        let abi = self.decl_abi(decl);
        let mut f = Function::new(self.symbol(decl), abi.sig.clone());
        let entry = f.create_block();
        let params: Vec<Value> = abi
            .sig
            .params
            .iter()
            .map(|p| f.append_block_param(entry, p.ty))
            .collect();
        let ast = &self.program.file(decl.file).ast;
        let fn_name = self.text(decl.file, fn_decl.name.span);
        let index = self.reserve_function();
        let mut fx = FnGen {
            g: self,
            f,
            file: decl.file,
            ast,
            fn_name,
            block: entry,
            reachable: true,
            terminated: HashSet::new(),
            locals: HashMap::new(),
            result: abi.result,
            sret: abi.sret.then(|| params[0]),
            defers: Vec::new(),
            loops: Vec::new(),
            joined: HashSet::new(),
            pending_next: None,
            index,
        };
        let mut next = usize::from(abi.sret);
        for (param, pass) in fn_decl.params.iter().zip(&abi.params) {
            let value = match pass {
                Pass::Skip => None,
                _ => {
                    next += 1;
                    Some(params[next - 1])
                }
            };
            let Some(name) = param.name else {
                continue;
            };
            let Some(id) = fx.binding(name.span) else {
                continue;
            };
            match (pass, value) {
                (Pass::Scalar, Some(v)) => {
                    let ty = fx.g.files[decl.file.0 as usize].locals[&id];
                    let slot = fx.slot(ty);
                    let addr = fx.ins().stack_addr(slot);
                    fx.ins().store(v, addr);
                    fx.locals.insert(id, Addr::Slot(slot));
                }
                (Pass::ByVal | Pass::Ref | Pass::Borrow, Some(v)) => {
                    fx.locals.insert(id, Addr::Ptr(v));
                }
                _ => {}
            }
        }
        fx.block(&fn_decl.body);
        if fx.reachable {
            match abi.result {
                None => {
                    fx.terminate_with(|b| {
                        b.return_(&[]);
                    });
                }
                Some(_) => fx.trap(),
            }
        }
        fx.finish();
    }
}

impl<'a> FnGen<'_, 'a> {
    // Blocks and instructions.

    fn ins(&mut self) -> InstBuilder<'_> {
        self.f.ins(InsertPoint::End(self.block))
    }

    fn switch_to(&mut self, block: Block, reachable: bool) {
        self.block = block;
        self.reachable = reachable;
    }

    /// Emits a terminator; code after it goes to a fresh, unreachable
    /// block.
    fn terminate_with(&mut self, emit: impl FnOnce(InstBuilder<'_>)) {
        emit(self.f.ins(InsertPoint::End(self.block)));
        self.terminated.insert(self.block);
        let dead = self.f.create_block();
        self.switch_to(dead, false);
    }

    fn jump(&mut self, to: Block) {
        self.terminate_with(|b| {
            b.jump(to, &[]);
        });
    }

    fn brif(&mut self, cond: Value, then: Block, otherwise: Block) {
        self.terminate_with(|b| {
            b.brif(cond, then, &[], otherwise, &[]);
        });
    }

    fn trap(&mut self) {
        self.terminate_with(|b| {
            b.trap();
        });
    }

    /// Every block that has no terminator is unreachable: give it a trap.
    fn finish(mut self) {
        let blocks: Vec<Block> = self.f.blocks().collect();
        for b in blocks {
            if !self.terminated.contains(&b) {
                self.f.ins(InsertPoint::End(b)).trap();
            }
        }
        self.g.module.functions[self.index] = self.f;
    }

    /// The address of a data object.
    fn data_addr(&mut self, symbol: &str) -> Value {
        self.g.reference_data(symbol);
        let data = self.f.import_data(symbol);
        self.ins().symbol_addr(Symbol::Data(data))
    }

    /// A function this one refers to, recorded for the module's
    /// declarations.
    fn import(&mut self, name: &str, sig: Signature) -> keel_ir::FuncRef {
        self.g.reference(name, &sig);
        self.f.import_function(name, sig)
    }

    fn iconst(&mut self, ty: Type, n: i64) -> Value {
        self.ins().iconst(ty, n)
    }

    fn usize_const(&mut self, n: u64) -> Value {
        self.iconst(Type::I64, i64::try_from(n).unwrap_or(i64::MAX))
    }

    fn offset(&mut self, addr: Value, by: u64) -> Value {
        if by == 0 {
            return addr;
        }
        let n = self.usize_const(by);
        self.ins().ptradd(addr, n)
    }

    fn null(&mut self) -> Value {
        let zero = self.iconst(Type::I64, 0);
        self.ins().convert(ConvertOp::IntToPtr, Type::Ptr, zero)
    }

    fn slot(&mut self, ty: TyId) -> StackSlot {
        let l = self.g.layout(ty);
        self.f.create_stack_slot(
            u32::try_from(l.size.max(1)).expect("a frame under 4 GiB"),
            u32::try_from(l.align.clamp(1, 16)).expect("small"),
        )
    }

    fn temp(&mut self, ty: TyId) -> Value {
        let slot = self.slot(ty);
        self.ins().stack_addr(slot)
    }

    fn memcpy(&mut self, dst: Value, src: Value, size: u64, overlap: bool) {
        if size == 0 {
            return;
        }
        let n = self.usize_const(size);
        let id = if overlap {
            Intrinsic::Memmove
        } else {
            Intrinsic::Memcpy
        };
        self.ins().intrinsic(id, &[dst, src, n]);
    }

    /// Calls a C library function by name.
    fn libc(
        &mut self,
        name: &str,
        params: &[Type],
        returns: &[Type],
        args: &[Value],
    ) -> Option<Value> {
        let callee = self.import(name, Signature::new(params, returns));
        let inst = self.ins().call(callee, args);
        self.f.inst_results(inst).first().copied()
    }

    // Names and types.

    fn text(&self, span: Span) -> &'a str {
        self.g.program.file(self.file).source.slice(span)
    }

    fn binding(&self, span: Span) -> Option<LocalId> {
        self.g.names.files[self.file.0 as usize]
            .bindings
            .get(&span.start)
            .copied()
    }

    fn res(&self, e: ExprId) -> Option<Res> {
        self.g.names.files[self.file.0 as usize].exprs[e.index()]
    }

    fn item(&self, e: ExprId) -> Item {
        self.g.files[self.file.0 as usize].exprs[e.index()].unwrap_or(Item::Nothing)
    }

    /// The type of a value expression.
    fn ty(&mut self, e: ExprId) -> TyId {
        match self.item(e) {
            Item::Value(t) | Item::Variant(t, _) => t,
            _ => self.g.types.error(),
        }
    }

    fn written_ty(&self, id: crate::ast::TypeId) -> TyId {
        self.g.files[self.file.0 as usize].types[id.index()].expect("checked")
    }

    fn int_of(&self, ty: TyId) -> Option<IntTy> {
        let core = self.g.core(ty);
        self.g.types.as_int(core)
    }

    // Storage.

    fn local_addr(&mut self, id: LocalId) -> Value {
        match self.locals.get(&id).copied() {
            Some(Addr::Slot(slot)) => self.ins().stack_addr(slot),
            Some(Addr::Ptr(p)) => p,
            None => self.null(),
        }
    }

    /// A new local with its own slot.
    fn bind(&mut self, span: Span, ty: TyId, value: V) {
        let Some(id) = self.binding(span) else {
            return;
        };
        let slot = self.slot(ty);
        let addr = self.ins().stack_addr(slot);
        self.store(value, ty, addr, false);
        self.locals.insert(id, Addr::Slot(slot));
    }

    fn load(&mut self, addr: Value, ty: TyId) -> V {
        match self.g.repr(ty) {
            Repr::Scalar(t) => V::S(self.ins().load(t, addr)),
            Repr::Memory => V::M(addr),
            Repr::Unit => V::Unit,
        }
    }

    fn store(&mut self, value: V, ty: TyId, addr: Value, overlap: bool) {
        match value {
            V::S(v) => {
                self.ins().store(v, addr);
            }
            V::M(src) => {
                let size = self.g.layout(ty).size;
                self.memcpy(addr, src, size, overlap);
            }
            V::Unit => {}
        }
    }

    fn scalar(&mut self, v: V) -> Value {
        match v {
            V::S(x) => x,
            V::M(a) => a,
            V::Unit => self.iconst(Type::I8, 0),
        }
    }

    /// A value of `ty` for code that cannot run.
    fn dummy(&mut self, ty: TyId) -> V {
        match self.g.repr(ty) {
            Repr::Scalar(Type::Ptr) => V::S(self.null()),
            Repr::Scalar(Type::F32) => V::S(self.ins().f32const(0.0)),
            Repr::Scalar(Type::F64) => V::S(self.ins().f64const(0.0)),
            Repr::Scalar(t) => V::S(self.iconst(t, 0)),
            Repr::Memory => V::M(self.temp(ty)),
            Repr::Unit => V::Unit,
        }
    }

    /// The address of a value held in memory, spilling a scalar.
    fn spill(&mut self, v: V, ty: TyId) -> Value {
        match v {
            V::M(a) => a,
            other => {
                let addr = self.temp(ty);
                self.store(other, ty, addr, false);
                addr
            }
        }
    }

    // Statements.

    fn block(&mut self, block: &AstBlock) {
        self.defers.push(Vec::new());
        for &stmt in &block.stmts {
            if !self.reachable {
                break;
            }
            self.stmt(stmt);
        }
        let defers = self.defers.pop().expect("pushed above");
        if self.reachable {
            for &call in defers.iter().rev() {
                self.expr(call);
            }
        }
    }

    /// Runs the `defer`s of scopes from `from` outward, for a jump out.
    fn run_defers(&mut self, from: usize) {
        let pending: Vec<ExprId> = self.defers[from..]
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev().copied())
            .collect();
        for call in pending {
            self.expr(call);
        }
    }

    fn stmt(&mut self, id: StmtId) {
        let ast = self.ast;
        match &ast.stmt(id).kind {
            StmtKind::Local(local) => match &local.binding {
                Binding::Name { name, ty } => {
                    let ty = self.written_ty(*ty);
                    let value = self.value_as(local.value, ty);
                    if let Some(name) = name {
                        self.bind(name.span, ty, value);
                    }
                }
                Binding::Destructure(fields) => {
                    let ty = self.ty(local.value);
                    let value = self.expr(local.value);
                    let addr = self.spill(value, ty);
                    self.destructure(fields, ty, addr);
                }
            },
            StmtKind::Return(value) => self.return_(*value),
            StmtKind::If(if_stmt) => {
                let after = self.f.create_block();
                self.if_stmt(if_stmt, after);
                self.continue_at(after);
            }
            StmtKind::Match { value, arms } => {
                let after = self.f.create_block();
                let sty = self.ty(*value);
                let scrutinee = self.expr(*value);
                for arm in arms {
                    let body = self.arm_test(scrutinee, sty, &arm.patterns);
                    let next = self.pending_next.take().expect("arm_test sets it");
                    let Some(body) = body else {
                        continue;
                    };
                    self.switch_to(body, true);
                    self.block(&arm.body);
                    self.jump_join(after);
                    self.switch_to(next, true);
                }
                // No arm matched: the checker proved this cannot happen.
                self.trap();
                self.continue_at(after);
            }
            StmtKind::For(for_loop) => self.for_loop(for_loop),
            StmtKind::Break => {
                let target = self.loops.last().map(|l| (l.brk, l.scopes));
                if let Some((brk, scopes)) = target {
                    self.run_defers(scopes);
                    self.jump_join(brk);
                }
            }
            StmtKind::Continue => {
                let target = self.loops.last().map(|l| (l.cont, l.scopes));
                if let Some((cont, scopes)) = target {
                    self.run_defers(scopes);
                    self.jump(cont);
                }
            }
            StmtKind::Defer(call) => {
                self.defers.last_mut().expect("inside a block").push(*call);
            }
            StmtKind::Unsafe(block) | StmtKind::Lock { body: block, .. } => self.block(block),
            StmtKind::Assign { left, op, right } => self.assign(*left, *op, *right),
            StmtKind::Expr(e) => {
                self.expr(*e);
            }
        }
    }

    /// Continues at a join block, which runs if any path reaches it.
    fn continue_at(&mut self, block: Block) {
        let preds_reached = self.joined.remove(&block);
        self.switch_to(block, preds_reached);
    }

    fn if_stmt(&mut self, if_stmt: &crate::ast::IfStmt, after: Block) {
        let cond = self.expr(if_stmt.cond);
        let cond = self.scalar(cond);
        let then = self.f.create_block();
        let otherwise = self.f.create_block();
        self.brif(cond, then, otherwise);
        self.switch_to(then, true);
        self.block(&if_stmt.then);
        self.jump_join(after);
        self.switch_to(otherwise, true);
        match &if_stmt.otherwise {
            Some(Else::Block(block)) => self.block(block),
            Some(Else::If(stmt)) => {
                if let StmtKind::If(inner) = &self.ast.stmt(*stmt).kind {
                    self.if_stmt(inner, after);
                }
            }
            None => {}
        }
        self.jump_join(after);
    }

    /// Jumps to a join block if this point can run.
    fn jump_join(&mut self, to: Block) {
        if self.reachable {
            self.joined.insert(to);
            self.jump(to);
        }
    }

    fn return_(&mut self, value: Option<ExprId>) {
        let mut results = Vec::new();
        if let (Some(e), Some(ty)) = (value, self.result) {
            let v = self.value_as(e, ty);
            if !self.reachable {
                return;
            }
            match self.sret {
                Some(out) => self.store(v, ty, out, true),
                None => {
                    if let V::S(x) = v {
                        results.push(x);
                    }
                }
            }
        }
        self.run_defers(0);
        if self.reachable {
            self.terminate_with(|b| {
                b.return_(&results);
            });
        }
    }

    fn assign(&mut self, left: ExprId, op: AssignOp, right: ExprId) {
        let ty = self.ty(left);
        let addr = self.addr_of(left);
        if op == AssignOp::Set {
            let v = self.value_as(right, ty);
            self.store(v, ty, addr, true);
            return;
        }
        let current = self.load(addr, ty);
        let current = self.scalar(current);
        let operand = self.value_as(right, ty);
        let operand = self.scalar(operand);
        let bop = match op {
            AssignOp::Add => BinaryOp::Add,
            AssignOp::Sub => BinaryOp::Sub,
            AssignOp::Mul => BinaryOp::Mul,
            AssignOp::Div => BinaryOp::Div,
            AssignOp::Rem => BinaryOp::Rem,
            AssignOp::And => BinaryOp::BitAnd,
            AssignOp::Or => BinaryOp::BitOr,
            AssignOp::Xor => BinaryOp::BitXor,
            AssignOp::Shl => BinaryOp::Shl,
            AssignOp::Shr => BinaryOp::Shr,
            AssignOp::Set => unreachable!("handled above"),
        };
        let span = self.ast.expr(left).span;
        let result = self.arith(bop, current, operand, ty, span);
        self.ins().store(result, addr);
    }

    fn destructure(&mut self, fields: &[DestructField], ty: TyId, addr: Value) {
        for field in fields {
            if field.ty.is_none() {
                continue;
            }
            let name = self.text(field.field.span);
            let (fty, off) = self.g.field(ty, name);
            let at = self.offset(addr, off);
            let v = self.load(at, fty);
            self.bind(field.rename.unwrap_or(field.field).span, fty, v);
        }
    }

    // Loops.

    fn for_loop(&mut self, for_loop: &crate::ast::For) {
        let header = self.f.create_block();
        let body = self.f.create_block();
        let cont = self.f.create_block();
        let exit = self.f.create_block();
        match &for_loop.kind {
            ForKind::Forever => {
                self.jump(header);
                self.switch_to(header, true);
                self.jump(body);
                self.loop_body(for_loop, body, cont, exit);
                self.switch_to(cont, true);
                self.jump(header);
            }
            ForKind::While(cond) => {
                self.jump(header);
                self.switch_to(header, true);
                let c = self.expr(*cond);
                let c = self.scalar(c);
                self.joined.insert(exit);
                self.brif(c, body, exit);
                self.loop_body(for_loop, body, cont, exit);
                self.switch_to(cont, true);
                self.jump(header);
            }
            ForKind::In {
                binding, iterable, ..
            } => self.for_in(for_loop, binding, iterable, [header, body, cont, exit]),
        }
        self.continue_at(exit);
    }

    fn loop_body(&mut self, for_loop: &crate::ast::For, body: Block, cont: Block, exit: Block) {
        self.switch_to(body, true);
        self.loops.push(Loop {
            brk: exit,
            cont,
            scopes: self.defers.len(),
        });
        self.block(&for_loop.body);
        self.loops.pop();
        if self.reachable {
            self.jump(cont);
        }
    }

    fn for_in(
        &mut self,
        for_loop: &crate::ast::For,
        binding: &Binding,
        iterable: &Iterable,
        [header, body, cont, exit]: [Block; 4],
    ) {
        match iterable {
            Iterable::Range { start, end } => {
                let Binding::Name { name, ty } = binding else {
                    return;
                };
                let ty = self.written_ty(*ty);
                let signed = self.int_of(ty).is_some_and(IntTy::signed);
                let start = self.value_as(*start, ty);
                let start = self.scalar(start);
                let end = self.value_as(*end, ty);
                let end = self.scalar(end);
                let counter = self.temp(ty);
                self.ins().store(start, counter);
                self.jump(header);
                self.switch_to(header, true);
                let t = self.g_scalar(ty);
                let i = self.ins().load(t, counter);
                let cc = if signed { IntCC::Slt } else { IntCC::Ult };
                let more = self.ins().icmp(cc, i, end);
                let bind_block = self.f.create_block();
                self.joined.insert(exit);
                self.brif(more, bind_block, exit);
                self.switch_to(bind_block, true);
                if let Some(name) = name {
                    self.bind(name.span, ty, V::S(i));
                }
                self.jump(body);
                self.loop_body(for_loop, body, cont, exit);
                self.switch_to(cont, true);
                let t = self.g_scalar(ty);
                let i = self.ins().load(t, counter);
                let one = self.iconst(t, 1);
                let next = self.ins().iadd(i, one);
                self.ins().store(next, counter);
                self.jump(header);
            }
            Iterable::Expr(e) => {
                let ty = self.ty(*e);
                let core = self.g.core(ty);
                match self.g.types.get(core).clone() {
                    Ty::Array(..) | Ty::View(_) | Ty::Growable(_) => {
                        let elem = match self.g.types.get(core) {
                            Ty::Array(_, t) | Ty::View(t) | Ty::Growable(t) => *t,
                            _ => unreachable!("matched above"),
                        };
                        let (ptr, len) = self.ptr_len(*e, core);
                        let usize_ty = self.g.types.usize();
                        let counter = self.temp(usize_ty);
                        let zero = self.iconst(Type::I64, 0);
                        self.ins().store(zero, counter);
                        self.jump(header);
                        self.switch_to(header, true);
                        let i = self.ins().load(Type::I64, counter);
                        let more = self.ins().icmp(IntCC::Ult, i, len);
                        let bind_block = self.f.create_block();
                        self.joined.insert(exit);
                        self.brif(more, bind_block, exit);
                        self.switch_to(bind_block, true);
                        let size = self.g.layout(elem).size;
                        let size = self.usize_const(size);
                        let off = self.ins().imul(i, size);
                        let at = self.ins().ptradd(ptr, off);
                        let v = self.load(at, elem);
                        self.bind_loop(binding, elem, v);
                        self.jump(body);
                        self.loop_body(for_loop, body, cont, exit);
                        self.switch_to(cont, true);
                        let i = self.ins().load(Type::I64, counter);
                        let one = self.iconst(Type::I64, 1);
                        let next = self.ins().iadd(i, one);
                        self.ins().store(next, counter);
                        self.jump(header);
                    }
                    _ => {
                        // An iterator: call `next(inout it)` until `none`.
                        let Some(next) = self.next_fn(core) else {
                            return;
                        };
                        let v = self.expr(*e);
                        let it = self.temp(ty);
                        self.store(v, ty, it, false);
                        self.jump(header);
                        self.switch_to(header, true);
                        let abi = self.g.decl_abi(next);
                        let option_ty = abi.result.expect("next returns an optional");
                        let symbol = self.g.symbol(next);
                        let item = self.direct_call(&symbol, &abi, vec![CallArg::Addr(it)]);
                        self.joined.insert(exit);
                        let (some_block, payload) = self.option_some(item, option_ty, exit);
                        self.switch_to(some_block, true);
                        let shape = self.g.enum_shape(option_ty).expect("an optional");
                        let payload_ty = shape.variants[1].payload.expect("some has a payload");
                        let (elem, _) = self.g.field(payload_ty, "value");
                        let v = match payload {
                            V::S(x) => V::S(x),
                            V::M(a) => self.load(a, elem),
                            V::Unit => V::Unit,
                        };
                        self.bind_loop(binding, elem, v);
                        self.jump(body);
                        self.loop_body(for_loop, body, cont, exit);
                        self.switch_to(cont, true);
                        self.jump(header);
                    }
                }
            }
        }
    }

    fn bind_loop(&mut self, binding: &Binding, elem: TyId, v: V) {
        match binding {
            Binding::Name { name, ty } => {
                let declared = self.written_ty(*ty);
                let v = self.coerce(v, elem, declared);
                if let Some(name) = name {
                    self.bind(name.span, declared, v);
                }
            }
            Binding::Destructure(fields) => {
                let addr = self.spill(v, elem);
                self.destructure(fields, elem, addr);
            }
        }
    }

    /// Branches on an optional: to the returned block when it is `some`,
    /// with the address or value of its payload record, and to `none_block`
    /// otherwise.
    fn option_some(&mut self, v: V, ty: TyId, none_block: Block) -> (Block, V) {
        let shape = self.g.enum_shape(ty).expect("an optional");
        let some = self.f.create_block();
        match shape.kind {
            EnumKind::NullablePointer => {
                let p = self.scalar(v);
                let null = self.null();
                let is_some = self.ins().icmp(IntCC::Ne, p, null);
                self.brif(is_some, some, none_block);
                (some, V::S(p))
            }
            EnumKind::Tagged {
                tag,
                payload_offset,
            } => {
                let addr = self.spill(v, ty);
                let t = self.ins().load(tag, addr);
                let one = self.iconst(tag, 1);
                let is_some = self.ins().icmp(IntCC::Eq, t, one);
                self.brif(is_some, some, none_block);
                self.switch_to(some, true);
                let payload = self.offset(addr, payload_offset);
                (some, V::M(payload))
            }
            EnumKind::Scalar(_) => {
                self.jump(none_block);
                (some, V::Unit)
            }
        }
    }

    fn next_fn(&self, ty: TyId) -> Option<DeclRef> {
        let Ty::Named(NamedId::Decl(decl)) = self.g.types.get(ty) else {
            return None;
        };
        let package = self.g.program.file(decl.file).package;
        self.g.names.scopes[package.0 as usize].get("next").copied()
    }

    fn g_scalar(&mut self, ty: TyId) -> Type {
        match self.g.repr(ty) {
            Repr::Scalar(t) => t,
            _ => Type::I64,
        }
    }

    // Matching.

    /// Tests one arm's patterns. Returns the block the arm's body starts
    /// in, with its payload bound, and leaves the block for the next arm
    /// in `pending_next`.
    fn arm_test(&mut self, scrutinee: V, sty: TyId, pats: &[PatId]) -> Option<Block> {
        let body = self.f.create_block();
        let next = self.f.create_block();
        if !self.reachable {
            self.pending_next = Some(next);
            return None;
        }
        let mut bindings: Option<(&'a [DestructField], String)> = None;
        for (i, &pat) in pats.iter().enumerate() {
            let last = i + 1 == pats.len();
            let fail = if last { next } else { self.f.create_block() };
            match &self.ast.pat(pat).kind {
                PatternKind::Discard => self.jump(body),
                PatternKind::Variant { name, payload } => {
                    let name = self.text(name.span).to_owned();
                    let cond = self.variant_test(scrutinee, sty, &name);
                    self.brif(cond, body, fail);
                    if let Some(fields) = payload {
                        bindings = Some((fields.as_slice(), name));
                    }
                }
                PatternKind::Literal(e) => {
                    let cond = self.literal_test(scrutinee, sty, *e, false);
                    self.brif(cond, body, fail);
                }
                PatternKind::Negative(e) => {
                    let cond = self.literal_test(scrutinee, sty, *e, true);
                    self.brif(cond, body, fail);
                }
            }
            if !last {
                self.switch_to(fail, true);
            }
        }
        self.pending_next = Some(next);
        self.switch_to(body, true);
        if let Some((fields, name)) = bindings {
            let (variant, kind) = self.g.variant(sty, &name);
            if let Some(payload_ty) = variant.payload {
                match kind {
                    EnumKind::NullablePointer => {
                        let p = self.scalar(scrutinee);
                        let addr = self.spill(V::S(p), payload_ty);
                        self.destructure(fields, payload_ty, addr);
                    }
                    EnumKind::Tagged { payload_offset, .. } => {
                        let addr = self.spill(scrutinee, sty);
                        let addr = self.offset(addr, payload_offset);
                        self.destructure(fields, payload_ty, addr);
                    }
                    EnumKind::Scalar(_) => {}
                }
            }
        }
        let start = self.f.create_block();
        self.jump(start);
        Some(start)
    }

    fn variant_test(&mut self, scrutinee: V, sty: TyId, name: &str) -> Value {
        let (variant, kind) = self.g.variant(sty, name);
        match kind {
            EnumKind::Scalar(tag) => {
                let v = self.scalar(scrutinee);
                let t = self.iconst(tag, variant.tag);
                self.ins().icmp(IntCC::Eq, v, t)
            }
            EnumKind::NullablePointer => {
                let p = self.scalar(scrutinee);
                let null = self.null();
                let cc = if variant.tag == 0 {
                    IntCC::Eq
                } else {
                    IntCC::Ne
                };
                self.ins().icmp(cc, p, null)
            }
            EnumKind::Tagged { tag, .. } => {
                let addr = self.spill(scrutinee, sty);
                let v = self.ins().load(tag, addr);
                let t = self.iconst(tag, variant.tag);
                self.ins().icmp(IntCC::Eq, v, t)
            }
        }
    }

    fn literal_test(&mut self, scrutinee: V, sty: TyId, e: ExprId, negative: bool) -> Value {
        if let ExprKind::Str(parts) = &self.ast.expr(e).kind {
            let bytes = string_bytes(self.g, self.file, parts);
            let view = self.spill(scrutinee, sty);
            return self.bytes_equal(view, &bytes);
        }
        let v = self.scalar(scrutinee);
        let t = self.g_scalar(sty);
        let n = self.g.const_int(self.file, e, 0).unwrap_or(0);
        let n = if negative { -n } else { n };
        #[allow(clippy::cast_possible_truncation)]
        let c = self.iconst(t, n as i64);
        self.ins().icmp(IntCC::Eq, v, c)
    }

    /// Whether the view at `view` holds exactly `bytes`.
    fn bytes_equal(&mut self, view: Value, bytes: &[u8]) -> Value {
        let len_addr = self.offset(view, 8);
        let len = self.ins().load(Type::I64, len_addr);
        let want = self.usize_const(bytes.len() as u64);
        let same_len = self.ins().icmp(IntCC::Eq, len, want);
        let compare = self.f.create_block();
        let done = self.f.create_block();
        let bool_ty = self.g.types.bool();
        let result = self.temp(bool_ty);
        let zero = self.iconst(Type::I8, 0);
        self.ins().store(zero, result);
        self.brif(same_len, compare, done);
        self.switch_to(compare, true);
        let ptr = self.ins().load(Type::Ptr, view);
        let symbol = self.g.string_symbol(bytes);
        let lit = self.data_addr(&symbol);
        let diff = self
            .libc(
                "memcmp",
                &[Type::Ptr, Type::Ptr, Type::I64],
                &[Type::I32],
                &[ptr, lit, want],
            )
            .expect("memcmp returns");
        let zero32 = self.iconst(Type::I32, 0);
        let eq = self.ins().icmp(IntCC::Eq, diff, zero32);
        self.ins().store(eq, result);
        self.jump(done);
        self.switch_to(done, true);
        self.ins().load(Type::I8, result)
    }

    // Expressions.

    /// `e`, converted to `ty` where the checker allowed a coercion.
    fn value_as(&mut self, e: ExprId, ty: TyId) -> V {
        let actual = self.ty(e);
        let v = self.expr(e);
        self.coerce(v, actual, ty)
    }

    fn coerce(&mut self, v: V, from: TyId, to: TyId) -> V {
        if from == to {
            return v;
        }
        let from_core = self.g.core(from);
        let to_core = self.g.core(to);
        match (
            self.g.types.get(from_core).clone(),
            self.g.types.get(to_core).clone(),
        ) {
            (Ty::Never | Ty::Error, _) => self.dummy(to),
            (Ty::Array(n, _), Ty::View(_)) => {
                let addr = self.scalar(v);
                let len = self.usize_const(n);
                self.make_view(to, addr, len)
            }
            (Ty::Growable(_), Ty::View(_)) => {
                let addr = self.scalar(v);
                let ptr = self.ins().load(Type::Ptr, addr);
                // An empty array's pointer is null; a view's never is.
                let null = self.null();
                let empty = self.ins().icmp(IntCC::Eq, ptr, null);
                let ptr = self.ins().select(empty, addr, ptr);
                let len_addr = self.offset(addr, 8);
                let len = self.ins().load(Type::I64, len_addr);
                self.make_view(to, ptr, len)
            }
            _ => v,
        }
    }

    fn make_view(&mut self, ty: TyId, ptr: Value, len: Value) -> V {
        let addr = self.temp(ty);
        self.ins().store(ptr, addr);
        let len_addr = self.offset(addr, 8);
        self.ins().store(len, len_addr);
        V::M(addr)
    }

    /// The data pointer and length of an array, view or growable array.
    fn ptr_len(&mut self, e: ExprId, core: TyId) -> (Value, Value) {
        match self.g.types.get(core).clone() {
            Ty::Array(n, _) => {
                let addr = self.addr_of(e);
                (addr, self.usize_const(n))
            }
            _ => {
                let addr = self.addr_of(e);
                let ptr = self.ins().load(Type::Ptr, addr);
                let len_addr = self.offset(addr, 8);
                let len = self.ins().load(Type::I64, len_addr);
                (ptr, len)
            }
        }
    }

    /// The address of an expression's storage: a variable, field or
    /// element, or a temporary for any other value.
    fn addr_of(&mut self, e: ExprId) -> Value {
        let ast = self.ast;
        match &ast.expr(e).kind {
            ExprKind::Paren(inner) => return self.addr_of(*inner),
            ExprKind::Name(_) => match self.res(e) {
                Some(Res::Local(id)) => return self.local_addr(id),
                Some(Res::Decl(decl)) if self.is_storage(decl) => return self.decl_addr(decl),
                _ => {}
            },
            ExprKind::Field { value, name } => {
                if let Some(Res::Decl(decl)) = self.res(e) {
                    if self.is_storage(decl) {
                        return self.decl_addr(decl);
                    }
                } else if let Item::Value(base_ty) = self.item(*value) {
                    let core = self.g.core(base_ty);
                    let name = self.text(name.span);
                    let record = !matches!(
                        self.g.types.get(core),
                        Ty::View(_) | Ty::Growable(_) | Ty::Array(..)
                    );
                    if record
                        && let Some(fields) = self.g.record_fields(core)
                        && let Some((_, _, off)) = fields.into_iter().find(|(n, _, _)| n == name)
                    {
                        let base = self.addr_of(*value);
                        return self.offset(base, off);
                    }
                }
            }
            ExprKind::Index {
                value,
                index: Index::Expr(i),
            } => {
                let span = ast.expr(e).span;
                return self.elem_addr(*value, *i, span).0;
            }
            ExprKind::Deref(p) => {
                let v = self.expr(*p);
                return self.scalar(v);
            }
            _ => {}
        }
        let ty = self.ty(e);
        let v = self.expr(e);
        self.spill(v, ty)
    }

    /// Leaves the `[..]T` at `v` empty: no buffer, length and capacity 0.
    /// It keeps its allocator, so it can grow again.
    fn empty_growable(&mut self, v: Value) {
        let null = self.null();
        self.ins().store(null, v);
        let zero = self.iconst(Type::I64, 0);
        for offset in [8, 16] {
            let addr = self.offset(v, offset);
            self.ins().store(zero, addr);
        }
    }

    /// Leaves the value of `ty` at `v` empty, as `take` does: every
    /// `[..]T` in it, directly or in a field, has no buffer, and every
    /// optional that holds one is `none`.
    fn empty(&mut self, v: Value, ty: TyId) {
        let core = self.g.core(ty);
        match self.g.types.get(core) {
            Ty::Growable(_) => {
                self.empty_growable(v);
                return;
            }
            Ty::Optional(_) => {
                let (none, kind) = self.g.variant(core, "none");
                if let EnumKind::Tagged { tag, .. } = kind {
                    let t = self.iconst(tag, none.tag);
                    self.ins().store(t, v);
                }
                return;
            }
            _ => {}
        }
        for (_, field_ty, offset) in self.g.record_fields(core).unwrap_or_default() {
            if self.g.holds_growable(field_ty) {
                let addr = self.offset(v, offset);
                self.empty(addr, field_ty);
            }
        }
    }

    /// Whether a `let` argument can pass the address of its storage: a
    /// local, or a field or element of one held in the local's own memory
    /// or its `[..]T`. A global, or a value reached through a view or a
    /// pointer, is copied, since the callee may change it another way.
    fn borrowable(&mut self, e: ExprId) -> bool {
        match &self.ast.expr(e).kind {
            ExprKind::Paren(inner) => self.borrowable(*inner),
            ExprKind::Name(_) => matches!(self.res(e), Some(Res::Local(_))),
            ExprKind::Field { value, .. } => {
                !matches!(self.res(e), Some(Res::Decl(_))) && self.borrowable(*value)
            }
            ExprKind::Index {
                value,
                index: Index::Expr(_),
            } => {
                let ty = self.ty(*value);
                let core = self.g.core(ty);
                matches!(self.g.types.get(core), Ty::Array(..) | Ty::Growable(_))
                    && self.borrowable(*value)
            }
            _ => false,
        }
    }

    fn is_storage(&self, decl: DeclRef) -> bool {
        matches!(
            self.g.decl_kind(decl),
            DeclKind::Global { .. } | DeclKind::ExternVar { .. }
        )
    }

    fn decl_addr(&mut self, decl: DeclRef) -> Value {
        let symbol = self.g.symbol(decl);
        self.data_addr(&symbol)
    }

    /// The address of `value[i]`, after a bounds check, and the element
    /// type.
    fn elem_addr(&mut self, value: ExprId, i: ExprId, span: Span) -> (Value, TyId) {
        let ty = self.ty(value);
        let core = self.g.core(ty);
        let elem = match self.g.types.get(core) {
            Ty::Array(_, t) | Ty::View(t) | Ty::Growable(t) => *t,
            _ => return (self.null(), self.g.types.error()),
        };
        let (ptr, len) = self.ptr_len(value, core);
        let usize = self.g.types.usize();
        let index = self.value_as(i, usize);
        let index = self.scalar(index);
        let inside = self.ins().icmp(IntCC::Ult, index, len);
        self.check(
            inside,
            &[
                Piece::Text("index "),
                Piece::Unsigned(index),
                Piece::Text(" out of bounds for length "),
                Piece::Unsigned(len),
            ],
            span,
        );
        let size = self.g.layout(elem).size;
        let size = self.usize_const(size);
        let off = self.ins().imul(index, size);
        (self.ins().ptradd(ptr, off), elem)
    }

    pub fn expr(&mut self, e: ExprId) -> V {
        if !self.reachable {
            let ty = self.ty(e);
            return self.dummy(ty);
        }
        let ast = self.ast;
        let span = ast.expr(e).span;
        let ty = self.ty(e);
        match &ast.expr(e).kind {
            ExprKind::Int | ExprKind::Char => {
                let n = self.g.const_int(self.file, e, 0).unwrap_or(0);
                let t = self.g_scalar(ty);
                #[allow(clippy::cast_possible_truncation)]
                V::S(self.iconst(t, n as i64))
            }
            ExprKind::Float => {
                let text: String = self.text(span).chars().filter(|&c| c != '_').collect();
                let x: f64 = text.parse().unwrap_or(0.0);
                if self.g_scalar(ty) == Type::F32 {
                    #[allow(clippy::cast_possible_truncation)]
                    V::S(self.ins().f32const(x as f32))
                } else {
                    V::S(self.ins().f64const(x))
                }
            }
            ExprKind::Str(parts) => self.string(parts, ty),
            ExprKind::Paren(inner) | ExprKind::Unsafe(inner) => self.expr(*inner),
            ExprKind::Tuple(fields) => {
                let addr = self.temp(ty);
                self.tuple_into(addr, ty, e, fields);
                V::M(addr)
            }
            ExprKind::Array(elems) => {
                let addr = self.temp(ty);
                let core = self.g.core(ty);
                if let Ty::Array(_, elem) = self.g.types.get(core).clone() {
                    let size = self.g.layout(elem).size;
                    for (i, &el) in elems.iter().enumerate() {
                        let v = self.value_as(el, elem);
                        let at = self.offset(addr, size * i as u64);
                        self.store(v, elem, at, false);
                    }
                }
                V::M(addr)
            }
            ExprKind::Name(_) => self.name(e, ty),
            ExprKind::Field { value, name } => self.field(e, *value, name.span, ty),
            ExprKind::Construct { ty: target, value } => self.construct(*target, *value, ty, span),
            ExprKind::Call { callee, args } => self.call(*callee, args, ty, span),
            ExprKind::Index { value, index } => match index {
                Index::Expr(i) => {
                    let (addr, elem) = self.elem_addr(*value, *i, span);
                    self.load(addr, elem)
                }
                Index::Slice { start, end } => self.slice(*value, *start, *end, ty, span),
                Index::Type(_) => V::Unit,
            },
            ExprKind::Deref(p) => {
                let v = self.expr(*p);
                let p = self.scalar(v);
                self.load(p, ty)
            }
            ExprKind::Unary { op, operand } => self.unary(*op, *operand, ty, span),
            ExprKind::Try(operand) => self.try_(*operand, ty),
            ExprKind::Binary { op, left, right } => self.binary(*op, *left, *right, ty, span),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                let out = self.result_slot(ty);
                let after = self.f.create_block();
                let c = self.expr(*cond);
                let c = self.scalar(c);
                let then_block = self.f.create_block();
                let else_block = self.f.create_block();
                self.brif(c, then_block, else_block);
                self.switch_to(then_block, true);
                self.branch_value(*then, ty, out, after);
                self.switch_to(else_block, true);
                let other = match otherwise {
                    IfElse::Branch(x) | IfElse::If(x) => *x,
                };
                self.branch_value(other, ty, out, after);
                self.continue_at(after);
                self.result_value(out, ty)
            }
            ExprKind::Match { value, arms } => {
                let out = self.result_slot(ty);
                let after = self.f.create_block();
                let sty = self.ty(*value);
                let scrutinee = self.expr(*value);
                for arm in arms {
                    let body = self.arm_test(scrutinee, sty, &arm.patterns);
                    let next = self.pending_next.take().expect("arm_test sets it");
                    let Some(body) = body else {
                        continue;
                    };
                    self.switch_to(body, true);
                    self.branch_value(arm.body, ty, out, after);
                    self.switch_to(next, true);
                }
                self.trap();
                self.continue_at(after);
                self.result_value(out, ty)
            }
        }
    }

    fn result_slot(&mut self, ty: TyId) -> Option<Value> {
        match self.g.repr(ty) {
            Repr::Unit => None,
            _ => Some(self.temp(ty)),
        }
    }

    fn branch_value(&mut self, e: ExprId, ty: TyId, out: Option<Value>, after: Block) {
        let v = self.value_as(e, ty);
        if let Some(out) = out
            && self.reachable
        {
            self.store(v, ty, out, false);
        }
        self.jump_join(after);
    }

    fn result_value(&mut self, out: Option<Value>, ty: TyId) -> V {
        match out {
            Some(addr) => self.load(addr, ty),
            None => V::Unit,
        }
    }

    fn name(&mut self, e: ExprId, ty: TyId) -> V {
        match self.res(e) {
            Some(Res::Local(id)) => {
                let addr = self.local_addr(id);
                self.load(addr, ty)
            }
            Some(Res::Decl(decl)) => self.decl_value(decl, ty),
            Some(Res::Builtin(name)) => self.builtin_value(name, ty),
            _ => self.dummy(ty),
        }
    }

    fn decl_value(&mut self, decl: DeclRef, ty: TyId) -> V {
        match self.g.decl_kind(decl) {
            DeclKind::Fn(_) | DeclKind::ExternFn { .. } => {
                let abi = self.g.decl_abi(decl);
                let symbol = self.g.symbol(decl);
                if abi.params.iter().any(|p| matches!(p, Pass::ByVal))
                    && matches!(self.g.decl_kind(decl), DeclKind::ExternFn { .. })
                {
                    self.g.unsupported(format!(
                        "`{symbol}` takes a record by value, as C does, so it cannot be a \
                         function value, whose records are passed by pointer"
                    ));
                }
                let callee = self.import(&symbol, abi.sig);
                V::S(self.ins().symbol_addr(Symbol::Func(callee)))
            }
            _ => {
                let addr = self.decl_addr(decl);
                self.load(addr, ty)
            }
        }
    }

    fn builtin_value(&mut self, name: &str, ty: TyId) -> V {
        match name {
            "true" => V::S(self.iconst(Type::I8, 1)),
            "false" => V::S(self.iconst(Type::I8, 0)),
            "none" | "ok" => self.enum_value(ty, name, None),
            _ => self.dummy(ty),
        }
    }

    fn field(&mut self, e: ExprId, value: ExprId, name: Span, ty: TyId) -> V {
        if let Some(Res::Decl(decl)) = self.res(e) {
            return self.decl_value(decl, ty);
        }
        let name = self.text(name);
        match self.item(value) {
            Item::Type(enum_ty) => self.enum_value(enum_ty, name, None),
            Item::Value(base_ty) => {
                let core = self.g.core(base_ty);
                if name == "len" {
                    match self.g.types.get(core).clone() {
                        Ty::Array(n, _) => return V::S(self.usize_const(n)),
                        Ty::View(_) | Ty::Growable(_) => {
                            let addr = self.addr_of(value);
                            let at = self.offset(addr, 8);
                            return V::S(self.ins().load(Type::I64, at));
                        }
                        _ => {}
                    }
                }
                let (fty, off) = self.g.field(core, name);
                let base = self.addr_of(value);
                let at = self.offset(base, off);
                self.load(at, fty)
            }
            _ => self.dummy(ty),
        }
    }

    /// A value of an enum, optional or result. `payload` is the `(...)`
    /// expression after the variant.
    fn enum_value(&mut self, ty: TyId, name: &str, payload: Option<ExprId>) -> V {
        let (variant, kind) = self.g.variant(ty, name);
        match kind {
            EnumKind::Scalar(tag) => V::S(self.iconst(tag, variant.tag)),
            EnumKind::NullablePointer => match (payload, variant.payload) {
                (Some(p), Some(payload_ty)) => {
                    let (vty, _) = self.g.field(payload_ty, "value");
                    let inner = self.tuple_field(p, "value");
                    self.value_as(inner, vty)
                }
                _ => V::S(self.null()),
            },
            EnumKind::Tagged {
                tag,
                payload_offset,
            } => {
                let addr = self.temp(ty);
                let t = self.iconst(tag, variant.tag);
                self.ins().store(t, addr);
                if let (Some(p), Some(payload_ty)) = (payload, variant.payload) {
                    let at = self.offset(addr, payload_offset);
                    self.write_record(at, payload_ty, p);
                }
                V::M(addr)
            }
        }
    }

    /// The expression for one field of a tuple literal.
    fn tuple_field(&self, e: ExprId, field: &str) -> ExprId {
        match &self.ast.expr(e).kind {
            ExprKind::Tuple(fields) => fields
                .iter()
                .find(|(n, _)| self.text(n.span) == field)
                .map_or(e, |(_, v)| *v),
            ExprKind::Paren(inner) => self.tuple_field(*inner, field),
            _ => e,
        }
    }

    /// Writes the record value `e` of type `ty` at `addr`: a tuple literal
    /// field by field, or any other expression whole.
    fn write_record(&mut self, addr: Value, ty: TyId, e: ExprId) {
        if let ExprKind::Tuple(fields) = &self.ast.expr(e).kind {
            self.tuple_into(addr, ty, e, fields);
        } else {
            let v = self.value_as(e, ty);
            self.store(v, ty, addr, false);
        }
    }

    fn tuple_into(
        &mut self,
        addr: Value,
        ty: TyId,
        _e: ExprId,
        fields: &[(crate::ast::Name, ExprId)],
    ) {
        let core = self.g.core(ty);
        for (name, value) in fields {
            let name = self.text(name.span);
            let (fty, off) = self.g.field(core, name);
            let at = self.offset(addr, off);
            self.write_record(at, fty, *value);
        }
    }

    fn construct(&mut self, target: ExprId, value: ExprId, ty: TyId, span: Span) -> V {
        if let Some(Res::Builtin(name @ ("some" | "ok" | "err"))) = self.res(target) {
            return self.enum_value(ty, name, Some(value));
        }
        match self.item(target) {
            Item::Variant(enum_ty, index) => {
                let shape = self.g.enum_shape(enum_ty).expect("an enum");
                let name = shape.variants[index].name.clone();
                self.enum_value(enum_ty, &name, Some(value))
            }
            Item::Builtin(name) => self.enum_value(ty, name, Some(value)),
            Item::Type(target_ty) => {
                let core = self.g.core(target_ty);
                if self.g.record_fields(core).is_some() {
                    let addr = self.temp(target_ty);
                    self.write_record(addr, core, value);
                    return V::M(addr);
                }
                let inner = match &self.ast.expr(value).kind {
                    ExprKind::Paren(inner) => *inner,
                    _ => value,
                };
                let from = self.ty(inner);
                let v = self.expr(inner);
                self.convert(v, from, target_ty, span)
            }
            _ => self.dummy(ty),
        }
    }

    /// `T.(x)`: a conversion between numbers, to or from an integer
    /// enum, or into or out of a newtype.
    fn convert(&mut self, v: V, from: TyId, to: TyId, span: Span) -> V {
        let from_core = self.g.core(from);
        let to_core = self.g.core(to);
        let from_int = self.int_like(from_core);
        let to_int = self.int_like(to_core);
        let from_float = self.g.types.get(from_core).clone();
        let to_float = self.g.types.get(to_core).clone();
        let x = match v {
            V::S(x) => x,
            other => return other,
        };
        match (from_int, to_int) {
            (Some(a), Some(b)) => {
                let converted = self.convert_int(x, a, b, to, span);
                if let Some(shape) = self.g.enum_shape(to_core)
                    && matches!(shape.kind, EnumKind::Scalar(_))
                    && !self.open_enum(to_core)
                {
                    // Only a variant's value is a valid enum.
                    let t = self.g_scalar(to_core);
                    let mut valid = self.iconst(Type::I8, 0);
                    for variant in &shape.variants {
                        let c = self.iconst(t, variant.tag);
                        let eq = self.ins().icmp(IntCC::Eq, converted, c);
                        valid = self.ins().or(valid, eq);
                    }
                    let shown = self.g.types.display(to);
                    let message = format!("no variant of `{shown}` has this value");
                    self.check(valid, &[Piece::Text(&message)], span);
                }
                V::S(converted)
            }
            (Some(a), None) => {
                let op = if a.signed() {
                    ConvertOp::Sitofp
                } else {
                    ConvertOp::Uitofp
                };
                let t = self.g_scalar(to_core);
                V::S(self.ins().convert(op, t, x))
            }
            (None, Some(b)) => {
                let op = if b.signed() {
                    ConvertOp::Fptosi
                } else {
                    ConvertOp::Fptoui
                };
                let wide = if b.bits() <= 32 { Type::I32 } else { Type::I64 };
                let wide_v = self.ins().convert(op, wide, x);
                let t = self.g_scalar(to_core);
                if t == wide {
                    V::S(wide_v)
                } else {
                    V::S(self.ins().trunc(t, wide_v))
                }
            }
            (None, None) => match (from_float, to_float) {
                (Ty::Float(32), Ty::Float(64)) => {
                    V::S(self.ins().convert(ConvertOp::Fpext, Type::F64, x))
                }
                (Ty::Float(64), Ty::Float(32)) => {
                    V::S(self.ins().convert(ConvertOp::Fptrunc, Type::F32, x))
                }
                _ => V::S(x),
            },
        }
    }

    fn open_enum(&self, ty: TyId) -> bool {
        let Ty::Named(id) = self.g.types.get(ty) else {
            return false;
        };
        matches!(
            self.g.types.named.get(id).map(|i| &i.def),
            Some(crate::types::NamedDef::Enum(info)) if info.open
        )
    }

    /// The integer type behind an integer or a payload-free enum.
    fn int_like(&mut self, ty: TyId) -> Option<IntTy> {
        if let Some(int) = self.g.types.as_int(ty) {
            return Some(int);
        }
        let Ty::Named(id) = self.g.types.get(ty) else {
            return None;
        };
        match self.g.types.named.get(id).map(|i| &i.def) {
            Some(crate::types::NamedDef::Enum(info)) => {
                let int = info.int.unwrap_or(IntTy::U8);
                match self.g.enum_shape(ty)?.kind {
                    EnumKind::Scalar(t) => Some(match (t, int.signed()) {
                        (Type::I16, false) => IntTy::U16,
                        (Type::I32, false) => IntTy::U32,
                        _ => int,
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Converts between integer types, panicking if the value does not
    /// fit.
    fn convert_int(&mut self, x: Value, from: IntTy, to: IntTy, to_ty: TyId, span: Span) -> Value {
        let ft = int_type(from.bits());
        let tt = int_type(to.bits());
        let mut ok: Option<Value> = None;
        let mut both = |s: &mut Self, c: Value| {
            ok = Some(match ok {
                Some(prev) => s.ins().and(prev, c),
                None => c,
            });
        };
        #[allow(clippy::cast_possible_truncation)]
        let max_in_from = |to: IntTy| to.max() as i64;
        if from.signed() && !to.signed() {
            let zero = self.iconst(ft, 0);
            let c = self.ins().icmp(IntCC::Sge, x, zero);
            both(self, c);
        }
        if from.bits() > to.bits() || (from.bits() == to.bits() && !from.signed() && to.signed()) {
            let max = self.iconst(ft, max_in_from(to));
            let cc = if from.signed() {
                IntCC::Sle
            } else {
                IntCC::Ule
            };
            let c = self.ins().icmp(cc, x, max);
            both(self, c);
        }
        if from.signed() && to.signed() && from.bits() > to.bits() {
            #[allow(clippy::cast_possible_truncation)]
            let min = self.iconst(ft, to.min() as i64);
            let c = self.ins().icmp(IntCC::Sge, x, min);
            both(self, c);
        }
        if let Some(ok) = ok {
            let shown = self.g.types.display(to_ty);
            let message = format!(" does not fit in `{shown}`");
            let wide = self.widen(x, from);
            let piece = if from.signed() {
                Piece::Signed(wide)
            } else {
                Piece::Unsigned(wide)
            };
            self.check(ok, &[piece, Piece::Text(&message)], span);
        }
        match from.bits().cmp(&to.bits()) {
            std::cmp::Ordering::Equal => x,
            std::cmp::Ordering::Greater => self.ins().trunc(tt, x),
            std::cmp::Ordering::Less => {
                if from.signed() {
                    self.ins().sext(tt, x)
                } else {
                    self.ins().uext(tt, x)
                }
            }
        }
    }

    /// An integer widened to 64 bits, for a message.
    fn widen(&mut self, x: Value, int: IntTy) -> Value {
        if int.bits() == 64 {
            return x;
        }
        if int.signed() {
            self.ins().sext(Type::I64, x)
        } else {
            self.ins().uext(Type::I64, x)
        }
    }

    fn slice(
        &mut self,
        value: ExprId,
        start: Option<ExprId>,
        end: Option<ExprId>,
        ty: TyId,
        span: Span,
    ) -> V {
        let base_ty = self.ty(value);
        let core = self.g.core(base_ty);
        let elem = match self.g.types.get(core) {
            Ty::Array(_, t) | Ty::View(t) | Ty::Growable(t) => *t,
            _ => return self.dummy(ty),
        };
        let (ptr, len) = self.ptr_len(value, core);
        let usize = self.g.types.usize();
        let start = match start {
            Some(s) => {
                let v = self.value_as(s, usize);
                self.scalar(v)
            }
            None => self.iconst(Type::I64, 0),
        };
        let end = match end {
            Some(e) => {
                let v = self.value_as(e, usize);
                self.scalar(v)
            }
            None => len,
        };
        let ordered = self.ins().icmp(IntCC::Ule, start, end);
        let inside = self.ins().icmp(IntCC::Ule, end, len);
        let ok = self.ins().and(ordered, inside);
        self.check(
            ok,
            &[
                Piece::Text("range "),
                Piece::Unsigned(start),
                Piece::Text(".."),
                Piece::Unsigned(end),
                Piece::Text(" out of bounds for length "),
                Piece::Unsigned(len),
            ],
            span,
        );
        let size = self.g.layout(elem).size;
        let size = self.usize_const(size);
        let off = self.ins().imul(start, size);
        let p = self.ins().ptradd(ptr, off);
        let n = self.ins().isub(end, start);
        self.make_view(ty, p, n)
    }

    fn unary(&mut self, op: UnaryOp, operand: ExprId, ty: TyId, span: Span) -> V {
        match op {
            UnaryOp::Neg => {
                if matches!(self.ast.expr(operand).kind, ExprKind::Int) {
                    let n = self.g.const_int(self.file, operand, 0).unwrap_or(0);
                    let t = self.g_scalar(ty);
                    #[allow(clippy::cast_possible_truncation)]
                    return V::S(self.iconst(t, (-n) as i64));
                }
                let v = self.value_as(operand, ty);
                let x = self.scalar(v);
                if matches!(self.g_scalar(ty), Type::F32 | Type::F64) {
                    return V::S(self.ins().fneg(x));
                }
                let t = self.g_scalar(ty);
                let zero = self.iconst(t, 0);
                V::S(self.arith(BinaryOp::Sub, zero, x, ty, span))
            }
            UnaryOp::Not => {
                let v = self.expr(operand);
                let x = self.scalar(v);
                let one = self.iconst(Type::I8, 1);
                V::S(self.ins().xor(x, one))
            }
            UnaryOp::BitNot => {
                let v = self.value_as(operand, ty);
                let x = self.scalar(v);
                V::S(self.ins().bnot(x))
            }
            UnaryOp::AddressOf => V::S(self.addr_of(operand)),
        }
    }

    fn try_(&mut self, operand: ExprId, ty: TyId) -> V {
        let oty = self.ty(operand);
        let v = self.expr(operand);
        let shape = self.g.enum_shape(oty).expect("an optional or result");
        let fail = self.f.create_block();
        let pass = self.f.create_block();
        let success = if matches!(self.g.types.get(oty), Ty::Optional(_)) {
            "some"
        } else {
            "ok"
        };
        let cond = self.variant_test(v, oty, success);
        self.brif(cond, pass, fail);
        // The failure: return `none`, or the same error.
        self.switch_to(fail, true);
        let result = self.result.expect("try needs a function with a result");
        let out = match self.g.types.get(oty) {
            Ty::Optional(_) => self.enum_value(result, "none", None),
            _ => {
                let (err_variant, kind) = self.g.variant(oty, "err");
                let (own_err, own_kind) = self.g.variant(result, "err");
                let addr = self.temp(result);
                if let (
                    EnumKind::Tagged {
                        tag,
                        payload_offset,
                    },
                    EnumKind::Tagged {
                        tag: own_tag,
                        payload_offset: own_offset,
                    },
                ) = (kind, own_kind)
                {
                    let src = self.spill(v, oty);
                    let src = self.offset(src, payload_offset);
                    let t = self.iconst(own_tag, own_err.tag);
                    self.ins().store(t, addr);
                    let dst = self.offset(addr, own_offset);
                    let payload = err_variant.payload.expect("err has a payload");
                    let size = self.g.layout(payload).size;
                    self.memcpy(dst, src, size, false);
                    let _ = tag;
                }
                V::M(addr)
            }
        };
        match self.sret {
            Some(dst) => {
                self.store(out, result, dst, false);
                self.run_defers(0);
                self.terminate_with(|b| {
                    b.return_(&[]);
                });
            }
            None => {
                let x = self.scalar(out);
                self.run_defers(0);
                self.terminate_with(|b| {
                    b.return_(&[x]);
                });
            }
        }
        self.switch_to(pass, true);
        let some = shape
            .variants
            .iter()
            .find(|v| v.name == success)
            .and_then(|v| v.payload);
        match (shape.kind, some) {
            (EnumKind::NullablePointer, _) => V::S(self.scalar(v)),
            (EnumKind::Tagged { payload_offset, .. }, Some(payload)) => {
                let addr = self.spill(v, oty);
                let at = self.offset(addr, payload_offset);
                let (vty, off) = self.g.field(payload, "value");
                let at = self.offset(at, off);
                let _ = ty;
                self.load(at, vty)
            }
            _ => V::Unit,
        }
    }

    fn binary(&mut self, op: BinaryOp, left: ExprId, right: ExprId, ty: TyId, span: Span) -> V {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let out = self.temp(ty);
                let l = self.expr(left);
                let l = self.scalar(l);
                self.ins().store(l, out);
                let rhs = self.f.create_block();
                let after = self.f.create_block();
                if op == BinaryOp::And {
                    self.brif(l, rhs, after);
                } else {
                    self.brif(l, after, rhs);
                }
                self.joined.insert(after);
                self.switch_to(rhs, true);
                let r = self.expr(right);
                let r = self.scalar(r);
                if self.reachable {
                    self.ins().store(r, out);
                }
                self.jump_join(after);
                self.continue_at(after);
                V::S(self.ins().load(Type::I8, out))
            }
            _ => {
                // The operand type: the other side's, for a literal.
                let lty = self.ty(left);
                let rty = self.ty(right);
                let operand_ty = if matches!(self.g.types.get(lty), Ty::Never | Ty::Error) {
                    rty
                } else {
                    lty
                };
                let l = self.value_as(left, operand_ty);
                let l = self.scalar(l);
                let r = if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
                    let v = self.expr(right);
                    self.scalar(v)
                } else {
                    let v = self.value_as(right, operand_ty);
                    self.scalar(v)
                };
                match op {
                    BinaryOp::Eq
                    | BinaryOp::Ne
                    | BinaryOp::Lt
                    | BinaryOp::Le
                    | BinaryOp::Gt
                    | BinaryOp::Ge => V::S(self.compare(op, l, r, operand_ty)),
                    _ => V::S(self.arith(op, l, r, ty, span)),
                }
            }
        }
    }

    fn compare(&mut self, op: BinaryOp, l: Value, r: Value, ty: TyId) -> Value {
        let t = self.g_scalar(ty);
        if matches!(t, Type::F32 | Type::F64) {
            let cc = match op {
                BinaryOp::Eq => FloatCC::Eq,
                BinaryOp::Ne => FloatCC::Ne,
                BinaryOp::Lt => FloatCC::Lt,
                BinaryOp::Le => FloatCC::Le,
                BinaryOp::Gt => FloatCC::Gt,
                _ => FloatCC::Ge,
            };
            return self.ins().fcmp(cc, l, r);
        }
        let signed = self.int_of(ty).is_some_and(IntTy::signed);
        let cc = match (op, signed) {
            (BinaryOp::Eq, _) => IntCC::Eq,
            (BinaryOp::Ne, _) => IntCC::Ne,
            (BinaryOp::Lt, true) => IntCC::Slt,
            (BinaryOp::Lt, false) => IntCC::Ult,
            (BinaryOp::Le, true) => IntCC::Sle,
            (BinaryOp::Le, false) => IntCC::Ule,
            (BinaryOp::Gt, true) => IntCC::Sgt,
            (BinaryOp::Gt, false) => IntCC::Ugt,
            (_, true) => IntCC::Sge,
            (_, false) => IntCC::Uge,
        };
        self.ins().icmp(cc, l, r)
    }

    /// An arithmetic or bitwise operation, with the checks that make
    /// overflow, division by zero and long shifts panic.
    fn arith(&mut self, op: BinaryOp, l: Value, r: Value, ty: TyId, span: Span) -> Value {
        let t = self.g_scalar(ty);
        if matches!(t, Type::F32 | Type::F64) {
            return match op {
                BinaryOp::Add => self.ins().fadd(l, r),
                BinaryOp::Sub => self.ins().fsub(l, r),
                BinaryOp::Mul => self.ins().fmul(l, r),
                _ => self.ins().fdiv(l, r),
            };
        }
        let int = self.int_of(ty).unwrap_or(IntTy::U64);
        let signed = int.signed();
        let text = op.text();
        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
                let oop = match (op, signed) {
                    (BinaryOp::Add, true) => OverflowOp::Sadd,
                    (BinaryOp::Add, false) => OverflowOp::Uadd,
                    (BinaryOp::Sub, true) => OverflowOp::Ssub,
                    (BinaryOp::Sub, false) => OverflowOp::Usub,
                    (_, true) => OverflowOp::Smul,
                    (_, false) => OverflowOp::Umul,
                };
                let (result, overflow) = self.ins().overflow(oop, l, r);
                let one = self.iconst(Type::I8, 1);
                let fine = self.ins().xor(overflow, one);
                let message = format!("`{text}` overflowed");
                self.check(fine, &[Piece::Text(&message)], span);
                result
            }
            BinaryOp::Div | BinaryOp::Rem => {
                let zero = self.iconst(t, 0);
                let nonzero = self.ins().icmp(IntCC::Ne, r, zero);
                self.check(nonzero, &[Piece::Text("division by zero")], span);
                if signed {
                    #[allow(clippy::cast_possible_truncation)]
                    let min = self.iconst(t, int.min() as i64);
                    let minus_one = self.iconst(t, -1);
                    let is_min = self.ins().icmp(IntCC::Eq, l, min);
                    let is_m1 = self.ins().icmp(IntCC::Eq, r, minus_one);
                    let both = self.ins().and(is_min, is_m1);
                    let one = self.iconst(Type::I8, 1);
                    let fine = self.ins().xor(both, one);
                    let message = format!("`{text}` overflowed");
                    self.check(fine, &[Piece::Text(&message)], span);
                }
                match (op, signed) {
                    (BinaryOp::Div, true) => self.ins().sdiv(l, r),
                    (BinaryOp::Div, false) => self.ins().udiv(l, r),
                    (_, true) => self.ins().srem(l, r),
                    (_, false) => self.ins().urem(l, r),
                }
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let rt = self.f.value_type(r);
                let bits = self.iconst(rt, i64::from(int.bits()));
                let fine = self.ins().icmp(IntCC::Ult, r, bits);
                let message = format!("shift by at least {} bits", int.bits());
                self.check(fine, &[Piece::Text(&message)], span);
                match (op, signed) {
                    (BinaryOp::Shl, _) => self.ins().shl(l, r),
                    (_, true) => self.ins().sshr(l, r),
                    (_, false) => self.ins().ushr(l, r),
                }
            }
            BinaryOp::BitAnd => self.ins().and(l, r),
            BinaryOp::BitOr => self.ins().or(l, r),
            _ => self.ins().xor(l, r),
        }
    }

    // Strings and formats.

    fn string(&mut self, parts: &[StrPart], ty: TyId) -> V {
        let is_format = matches!(
            self.g.types.get(ty),
            Ty::Named(NamedId::Builtin(BuiltinNamed::Format))
        );
        if !is_format {
            let bytes = string_bytes(self.g, self.file, parts);
            return self.string_view(&bytes, ty);
        }
        let mut pieces: Vec<FormatPiece> = Vec::new();
        for part in parts {
            match part {
                StrPart::Text(span) => {
                    let mut bytes = Vec::new();
                    unescape(self.text(*span), &mut bytes);
                    if !bytes.is_empty() {
                        pieces.push(FormatPiece::Text(bytes));
                    }
                }
                StrPart::Interpolation(e) => {
                    let ety = self.ty(*e);
                    let v = self.expr(*e);
                    pieces.push(FormatPiece::Value(v, ety));
                }
            }
        }
        self.format(&pieces, ty)
    }

    fn string_view(&mut self, bytes: &[u8], ty: TyId) -> V {
        let symbol = self.g.string_symbol(bytes);
        let ptr = self.data_addr(&symbol);
        let len = self.usize_const(bytes.len() as u64);
        self.make_view(ty, ptr, len)
    }

    /// Builds a `Format`: an array of `fmt.Part` on the stack, and the
    /// view of it.
    fn format(&mut self, pieces: &[FormatPiece], format_ty: TyId) -> V {
        let Some(part_ty) = self.g.fmt_part() else {
            return self.dummy(format_ty);
        };
        let part_size = self.g.layout(part_ty).size;
        let count = pieces.len() as u64;
        let array_ty = self.g.types.intern(Ty::Array(count.max(1), part_ty));
        let parts = self.temp(array_ty);
        let bytes_ty = self.g.types.bytes();
        let shape = self.g.enum_shape(part_ty).expect("fmt.Part is an enum");
        let EnumKind::Tagged {
            tag,
            payload_offset,
        } = shape.kind
        else {
            return self.dummy(format_ty);
        };
        for (i, piece) in pieces.iter().enumerate() {
            let at = self.offset(parts, part_size * i as u64);
            let payload = self.offset(at, payload_offset);
            let variant = |name: &str| {
                shape
                    .variants
                    .iter()
                    .find(|v| v.name == name)
                    .expect("a variant of fmt.Part")
                    .clone()
            };
            let (name, value): (&str, Option<(V, TyId)>) = match piece {
                FormatPiece::Text(bytes) => {
                    let v = self.string_view(bytes, bytes_ty);
                    ("text", Some((v, bytes_ty)))
                }
                FormatPiece::Value(v, ety) => {
                    let core = self.g.core(*ety);
                    match self.g.types.get(core).clone() {
                        Ty::Int(int) => {
                            let x = self.scalar(*v);
                            let wide = self.widen(x, int);
                            let i64_ty = self.g.types.int(IntTy::I64);
                            let u64_ty = self.g.types.int(IntTy::U64);
                            if int.signed() {
                                ("signed", Some((V::S(wide), i64_ty)))
                            } else {
                                ("unsigned", Some((V::S(wide), u64_ty)))
                            }
                        }
                        Ty::Bool => ("boolean", Some((*v, *ety))),
                        Ty::View(_) => ("text", Some((*v, bytes_ty))),
                        Ty::Growable(_) => {
                            let view = self.coerce(*v, *ety, bytes_ty);
                            ("text", Some((view, bytes_ty)))
                        }
                        Ty::Named(NamedId::Builtin(BuiltinNamed::Format)) => {
                            // nested(parts): the inner format's parts.
                            let addr = self.spill(*v, *ety);
                            let parts_ty = self.g.types.intern(Ty::View(part_ty));
                            ("nested", Some((V::M(addr), parts_ty)))
                        }
                        _ => {
                            // An fmt.Part value: copied whole.
                            let src = self.spill(*v, *ety);
                            self.memcpy(at, src, part_size, false);
                            continue;
                        }
                    }
                }
            };
            let variant = variant(name);
            let t = self.iconst(tag, variant.tag);
            self.ins().store(t, at);
            if let (Some((v, vty)), Some(p)) = (value, variant.payload) {
                // Every payload's first field holds the value.
                let fields = self.g.record_fields(p).unwrap_or_default();
                if let Some((_, _, off)) = fields.first() {
                    let dst = self.offset(payload, *off);
                    self.store(v, vty, dst, false);
                }
            }
        }
        let format = self.temp(format_ty);
        self.ins().store(parts, format);
        let len_addr = self.offset(format, 8);
        let n = self.usize_const(count);
        self.ins().store(n, len_addr);
        V::M(format)
    }

    // Panics.

    /// Panics with the message unless `ok` is true.
    fn check(&mut self, ok: Value, message: &[Piece], span: Span) {
        let pass = self.f.create_block();
        let fail = self.f.create_block();
        self.brif(ok, pass, fail);
        self.switch_to(fail, true);
        let mut parts = Vec::new();
        let mut values = Vec::new();
        for piece in message {
            match piece {
                Piece::Text(t) => parts.push(MessagePart::Text((*t).to_owned())),
                Piece::Unsigned(v) => {
                    parts.push(MessagePart::Unsigned);
                    values.push(*v);
                }
                Piece::Signed(v) => {
                    parts.push(MessagePart::Signed);
                    values.push(*v);
                }
            }
        }
        self.call_panic_helper(Some(parts), values, span);
        self.switch_to(pass, true);
    }

    /// Reports through the panic handler and aborts.
    fn panic(&mut self, message: V, span: Span) {
        let format_ty = self
            .g
            .types
            .named_ty(NamedId::Builtin(BuiltinNamed::Format));
        let addr = self.spill(message, format_ty);
        self.call_panic_helper(None, vec![addr], span);
    }

    /// Calls the cold helper that reports a panic with `message`, passing
    /// `args` and the address of the site's constant `PanicInfo`. Without a
    /// panic handler, aborts instead. Keeping the message and `PanicInfo`
    /// out of the function keeps them out of its frame.
    fn call_panic_helper(
        &mut self,
        message: Option<Vec<MessagePart>>,
        mut args: Vec<Value>,
        span: Span,
    ) {
        let has_info = self
            .g
            .types
            .named
            .contains_key(&NamedId::Builtin(BuiltinNamed::PanicInfo));
        if has_info && self.g.panic_handler().is_some() {
            let source = &self.g.program.file(self.file).source;
            let position = source.position(span.start);
            let path = source.path.display().to_string();
            let site = self
                .g
                .panic_site(&path, self.fn_name, position.line, position.column);
            let site = self.data_addr(&site);
            args.push(site);
            let params = panic_helper_params(message.as_deref());
            let name = self.panic_helper(message);
            let callee = self.import(&name, Signature::new(&params, &[]));
            self.ins().call(callee, &args);
        } else {
            self.libc("abort", &[], &[], &[]);
        }
        self.trap();
    }

    /// The cold helper for `message`, made the first time: it builds the
    /// message and a `PanicInfo` from the site and calls the panic handler.
    fn panic_helper(&mut self, message: Option<Vec<MessagePart>>) -> String {
        if let Some(name) = self.g.panic_helpers.get(&message) {
            return name.clone();
        }
        let name = format!("tern.panic.{}", self.g.panic_helpers.len());
        self.g.panic_helpers.insert(message.clone(), name.clone());
        let params = panic_helper_params(message.as_deref());
        let mut f = Function::new(name.clone(), Signature::new(&params, &[]));
        f.local = true;
        f.attrs.cold = true;
        f.attrs.noinline = true;
        f.attrs.noreturn = true;
        let entry = f.create_block();
        let index = self.g.reserve_function();
        let values: Vec<Value> = params
            .iter()
            .map(|t| f.append_block_param(entry, *t))
            .collect();
        let mut fx = FnGen {
            g: &mut *self.g,
            f,
            file: self.file,
            ast: self.ast,
            fn_name: self.fn_name,
            block: entry,
            reachable: true,
            terminated: HashSet::new(),
            locals: HashMap::new(),
            result: None,
            sret: None,
            defers: Vec::new(),
            loops: Vec::new(),
            joined: HashSet::new(),
            pending_next: None,
            index,
        };
        let (&site, values) = values.split_last().expect("the site is a parameter");
        let format_ty = fx.g.types.named_ty(NamedId::Builtin(BuiltinNamed::Format));
        let text = match message {
            None => V::M(values[0]),
            Some(parts) => {
                let mut values = values.iter();
                let pieces: Vec<FormatPiece> = parts
                    .into_iter()
                    .map(|part| match part {
                        MessagePart::Text(t) => FormatPiece::Text(t.into_bytes()),
                        MessagePart::Unsigned => {
                            let ty = fx.g.types.int(IntTy::U64);
                            FormatPiece::Value(V::S(*values.next().expect("a value")), ty)
                        }
                        MessagePart::Signed => {
                            let ty = fx.g.types.int(IntTy::I64);
                            FormatPiece::Value(V::S(*values.next().expect("a value")), ty)
                        }
                    })
                    .collect();
                fx.format(&pieces, format_ty)
            }
        };
        let info_ty =
            fx.g.types
                .named_ty(NamedId::Builtin(BuiltinNamed::PanicInfo));
        let info = fx.temp(info_ty);
        let size = fx.g.layout(info_ty).size;
        fx.memcpy(info, site, size, false);
        let (_, off) = fx.g.field(info_ty, "message");
        let at = fx.offset(info, off);
        fx.store(text, format_ty, at, false);
        if let Some(handler) = fx.g.panic_handler() {
            let abi = fx.g.decl_abi(handler);
            let symbol = fx.g.symbol(handler);
            fx.direct_call(&symbol, &abi, vec![CallArg::Addr(info)]);
        }
        fx.libc("abort", &[], &[], &[]);
        fx.trap();
        fx.finish();
        name
    }

    // Calls.

    fn call(&mut self, callee: ExprId, args: &[Arg], ty: TyId, span: Span) -> V {
        let (builtin, type_arg) = match &self.ast.expr(callee).kind {
            ExprKind::Index { value, index } => match self.res(*value) {
                Some(Res::Builtin(name)) => (Some(name), Some(index)),
                _ => (None, None),
            },
            _ => match self.res(callee) {
                Some(Res::Builtin(name)) => (Some(name), None),
                _ => (None, None),
            },
        };
        if let Some(name) = builtin {
            return self.builtin(name, type_arg, args, ty, span);
        }
        if let Some(Res::Decl(decl)) = self.res(callee)
            && matches!(
                self.g.decl_kind(decl),
                DeclKind::Fn(_) | DeclKind::ExternFn { .. }
            )
        {
            let abi = self.g.decl_abi(decl);
            let call_args = self.call_args(args, &abi);
            let symbol = self.g.symbol(decl);
            return self.direct_call(&symbol, &abi, call_args);
        }
        // A function value.
        let fty = self.ty(callee);
        let abi = self.g.fn_abi(fty, false, false);
        let target = self.expr(callee);
        let target = self.scalar(target);
        let call_args = self.call_args(args, &abi);
        let (values, out) = self.lower_args(&abi, call_args);
        let sig = self.f.import_signature(abi.sig.clone());
        let inst = self.ins().call_indirect(sig, target, &values);
        self.call_result(inst, out)
    }

    fn call_args(&mut self, args: &[Arg], abi: &super::FnAbi) -> Vec<CallArg> {
        let fn_params: Vec<TyId> = match self.g.types.get(self.g.core(abi.fn_ty)).clone() {
            Ty::Fn(f) => f.params.iter().map(|(_, t)| *t).collect(),
            _ => Vec::new(),
        };
        // A call inside an argument that passes something `inout` could
        // change a borrowed argument before the callee reads it, so then
        // borrowed arguments are copied.
        let ast = self.ast;
        let changes = args.iter().any(|a| changes_in_call(ast, a.value));
        let mut out = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let pass = abi.params.get(i).copied();
            let param_ty = fn_params.get(i).copied();
            match (pass, param_ty) {
                (Some(Pass::Ref), _) => out.push(CallArg::Addr(self.addr_of(arg.value))),
                (Some(Pass::Skip), _) => {
                    self.expr(arg.value);
                    out.push(CallArg::None);
                }
                (Some(Pass::Scalar), Some(t)) => {
                    let v = self.value_as(arg.value, t);
                    out.push(CallArg::Value(self.scalar(v)));
                }
                (Some(Pass::ByVal), Some(t)) => {
                    let v = self.value_as(arg.value, t);
                    out.push(CallArg::Addr(self.spill(v, t)));
                }
                (Some(Pass::Borrow), Some(t)) => {
                    if !changes && self.ty(arg.value) == t && self.borrowable(arg.value) {
                        out.push(CallArg::Addr(self.addr_of(arg.value)));
                    } else {
                        let v = self.value_as(arg.value, t);
                        let copy = self.temp(t);
                        self.store(v, t, copy, false);
                        out.push(CallArg::Addr(copy));
                    }
                }
                _ => {
                    // A C variadic argument: widened as C promotes it.
                    let aty = self.ty(arg.value);
                    let v = self.expr(arg.value);
                    let x = self.scalar(v);
                    let x = match (self.g_scalar(aty), self.int_of(aty)) {
                        (Type::I8 | Type::I16, Some(int)) => {
                            if int.signed() {
                                self.ins().sext(Type::I32, x)
                            } else {
                                self.ins().uext(Type::I32, x)
                            }
                        }
                        (Type::F32, _) => self.ins().convert(ConvertOp::Fpext, Type::F64, x),
                        _ => x,
                    };
                    out.push(CallArg::Value(x));
                }
            }
        }
        out
    }

    fn direct_call(&mut self, symbol: &str, abi: &super::FnAbi, args: Vec<CallArg>) -> V {
        let (values, out) = self.lower_args(abi, args);
        let callee = self.import(symbol, abi.sig.clone());
        let inst = self.ins().call(callee, &values);
        self.call_result(inst, out)
    }

    fn lower_args(
        &mut self,
        abi: &super::FnAbi,
        args: Vec<CallArg>,
    ) -> (Vec<Value>, Option<Value>) {
        let mut values = Vec::new();
        let out = match (abi.sret, abi.result) {
            (true, Some(r)) => {
                let addr = self.temp(r);
                values.push(addr);
                Some(addr)
            }
            _ => None,
        };
        for arg in args {
            match arg {
                CallArg::Value(v) | CallArg::Addr(v) => values.push(v),
                CallArg::None => {}
            }
        }
        (values, out)
    }

    fn call_result(&mut self, inst: keel_ir::Inst, out: Option<Value>) -> V {
        if let Some(addr) = out {
            return V::M(addr);
        }
        match self.f.inst_results(inst).first() {
            Some(&v) => V::S(v),
            None => V::Unit,
        }
    }

    fn builtin(
        &mut self,
        name: &str,
        type_arg: Option<&Index>,
        args: &[Arg],
        ty: TyId,
        span: Span,
    ) -> V {
        let arg = |i: usize| args[i].value;
        match name {
            "panic" => {
                let format_ty = self
                    .g
                    .types
                    .named_ty(NamedId::Builtin(BuiltinNamed::Format));
                let message = self.value_as(arg(0), format_ty);
                self.panic(message, span);
                V::Unit
            }
            "make" => {
                let alloc_ty = self
                    .g
                    .types
                    .named_ty(NamedId::Builtin(BuiltinNamed::Allocator));
                let alloc = self.value_as(arg(0), alloc_ty);
                let addr = self.temp(ty);
                let zero = self.iconst(Type::I64, 0);
                let null = self.null();
                self.ins().store(null, addr);
                for off in [8, 16] {
                    let at = self.offset(addr, off);
                    self.ins().store(zero, at);
                }
                let at = self.offset(addr, 24);
                self.store(alloc, alloc_ty, at, false);
                V::M(addr)
            }
            "push" | "append" => {
                let vty = self.ty(arg(0));
                let core = self.g.core(vty);
                let Ty::Growable(elem) = self.g.types.get(core).clone() else {
                    return self.dummy(ty);
                };
                let item = if name == "push" {
                    let v = self.value_as(arg(1), elem);
                    PushItem::One(v)
                } else {
                    let view_ty = self.g.types.intern(Ty::View(elem));
                    let v = self.value_as(arg(1), view_ty);
                    let addr = self.spill(v, view_ty);
                    let ptr = self.ins().load(Type::Ptr, addr);
                    let len_addr = self.offset(addr, 8);
                    let len = self.ins().load(Type::I64, len_addr);
                    PushItem::Many(ptr, len)
                };
                let v = self.addr_of(arg(0));
                let l = self.g.layout(elem);
                let len_addr = self.offset(v, 8);
                let len = self.ins().load(Type::I64, len_addr);
                let extra = match item {
                    PushItem::One(_) => self.iconst(Type::I64, 1),
                    PushItem::Many(_, n) => n,
                };
                let needed = self.ins().iadd(len, extra);
                let size = self.usize_const(l.size);
                let out = self.temp(ty);
                let (ok_variant, kind) = self.g.variant(ty, "ok");
                let (err_variant, _) = self.g.variant(ty, "err");
                let EnumKind::Tagged { tag, .. } = kind else {
                    return self.dummy(ty);
                };
                let stored = self.f.create_block();
                let failed = self.f.create_block();
                let after = self.f.create_block();
                // Only call tern.grow when the items do not fit.
                let cap_addr = self.offset(v, 16);
                let cap = self.ins().load(Type::I64, cap_addr);
                let fits = self.ins().icmp(IntCC::Ule, needed, cap);
                let grow_block = self.f.create_block();
                self.brif(fits, stored, grow_block);
                self.switch_to(grow_block, true);
                let align = self.usize_const(l.align);
                let grow = self.g.grow_symbol();
                let grow_sig = super::grow_signature();
                let callee = self.import(&grow, grow_sig);
                let inst = self.ins().call(callee, &[v, size, align, needed]);
                let grown = self.f.first_result(inst);
                self.brif(grown, stored, failed);
                self.switch_to(stored, true);
                let ptr = self.ins().load(Type::Ptr, v);
                let off = self.ins().imul(len, size);
                let at = self.ins().ptradd(ptr, off);
                match item {
                    PushItem::One(x) => self.store(x, elem, at, false),
                    PushItem::Many(src, n) => {
                        let bytes = self.ins().imul(n, size);
                        self.ins().intrinsic(Intrinsic::Memcpy, &[at, src, bytes]);
                    }
                }
                self.ins().store(needed, len_addr);
                let t = self.iconst(tag, ok_variant.tag);
                self.ins().store(t, out);
                self.jump(after);
                self.switch_to(failed, true);
                // err.(error: OutOfMemory.out_of_memory): a zero payload.
                let t = self.iconst(tag, err_variant.tag);
                self.ins().store(t, out);
                let size_all = self.g.layout(ty).size;
                if size_all > type_size(tag) {
                    let zero = self.iconst(Type::I8, 0);
                    let at = self.offset(out, size_all - 1);
                    self.ins().store(zero, at);
                }
                self.jump(after);
                self.switch_to(after, true);
                V::M(out)
            }
            "pop" => {
                let vty = self.ty(arg(0));
                let core = self.g.core(vty);
                let Ty::Growable(elem) = self.g.types.get(core).clone() else {
                    return self.dummy(ty);
                };
                let size = self.g.layout(elem).size;
                let v = self.addr_of(arg(0));
                let len_addr = self.offset(v, 8);
                let len = self.ins().load(Type::I64, len_addr);
                let zero = self.iconst(Type::I64, 0);
                let has = self.ins().icmp(IntCC::Ne, len, zero);
                // The result is built in a slot, which also serves a `?*T`
                // that is a single pointer.
                let out = self.temp(ty);
                let (some, kind) = self.g.variant(ty, "some");
                let (none, _) = self.g.variant(ty, "none");
                let take = self.f.create_block();
                let empty = self.f.create_block();
                let after = self.f.create_block();
                self.brif(has, take, empty);
                self.switch_to(take, true);
                let one = self.iconst(Type::I64, 1);
                let new_len = self.ins().isub(len, one);
                self.ins().store(new_len, len_addr);
                let ptr = self.ins().load(Type::Ptr, v);
                let size_v = self.usize_const(size);
                let off = self.ins().imul(new_len, size_v);
                let at = self.ins().ptradd(ptr, off);
                match kind {
                    EnumKind::Tagged {
                        tag,
                        payload_offset,
                    } => {
                        let t = self.iconst(tag, some.tag);
                        self.ins().store(t, out);
                        let payload = some.payload.expect("some has a payload");
                        let (_, field_off) = self.g.field(payload, "value");
                        let dst = self.offset(out, payload_offset + field_off);
                        let x = self.load(at, elem);
                        self.store(x, elem, dst, false);
                    }
                    _ => {
                        let p = self.ins().load(Type::Ptr, at);
                        self.ins().store(p, out);
                    }
                }
                self.jump(after);
                self.switch_to(empty, true);
                match kind {
                    EnumKind::Tagged { tag, .. } => {
                        let t = self.iconst(tag, none.tag);
                        self.ins().store(t, out);
                    }
                    _ => {
                        let null = self.null();
                        self.ins().store(null, out);
                    }
                }
                self.jump(after);
                self.switch_to(after, true);
                self.load(out, ty)
            }
            "clear" => {
                let v = self.addr_of(arg(0));
                let zero = self.iconst(Type::I64, 0);
                let len_addr = self.offset(v, 8);
                self.ins().store(zero, len_addr);
                V::Unit
            }
            "free" => {
                let vty = self.ty(arg(0));
                let core = self.g.core(vty);
                let Ty::Growable(elem) = self.g.types.get(core).clone() else {
                    return V::Unit;
                };
                let l = self.g.layout(elem);
                let v = self.addr_of(arg(0));
                let ptr = self.ins().load(Type::Ptr, v);
                let null = self.null();
                let has = self.ins().icmp(IntCC::Ne, ptr, null);
                let release = self.f.create_block();
                let after = self.f.create_block();
                self.brif(has, release, after);
                self.switch_to(release, true);
                let cap_addr = self.offset(v, 16);
                let cap = self.ins().load(Type::I64, cap_addr);
                let size = self.usize_const(l.size);
                let bytes = self.ins().imul(cap, size);
                let align = self.usize_const(l.align);
                let ctx_addr = self.offset(v, 24);
                let ctx = self.ins().load(Type::Ptr, ctx_addr);
                let free_addr = self.offset(v, 48);
                let free_fn = self.ins().load(Type::Ptr, free_addr);
                let sig = self.f.import_signature(Signature::new(
                    &[Type::Ptr, Type::Ptr, Type::I64, Type::I64],
                    &[],
                ));
                self.ins()
                    .call_indirect(sig, free_fn, &[ctx, ptr, bytes, align]);
                self.jump(after);
                self.switch_to(after, true);
                self.empty_growable(v);
                V::Unit
            }
            "take" => {
                let vty = self.ty(arg(0));
                let v = self.addr_of(arg(0));
                let out = self.temp(vty);
                self.store(V::M(v), vty, out, false);
                self.empty(v, vty);
                V::M(out)
            }
            "fill" => {
                let core = self.g.core(ty);
                let Ty::Array(n, elem) = self.g.types.get(core).clone() else {
                    return self.dummy(ty);
                };
                let v = self.value_as(arg(0), elem);
                let addr = self.temp(ty);
                let size = self.g.layout(elem).size;
                // A loop over the elements.
                let usize_ty = self.g.types.usize();
                let counter = self.temp(usize_ty);
                let zero = self.iconst(Type::I64, 0);
                self.ins().store(zero, counter);
                let header = self.f.create_block();
                let body = self.f.create_block();
                let done = self.f.create_block();
                self.jump(header);
                self.switch_to(header, true);
                let i = self.ins().load(Type::I64, counter);
                let total = self.usize_const(n);
                let more = self.ins().icmp(IntCC::Ult, i, total);
                self.brif(more, body, done);
                self.switch_to(body, true);
                let size_v = self.usize_const(size);
                let off = self.ins().imul(i, size_v);
                let at = self.ins().ptradd(addr, off);
                self.store(v, elem, at, false);
                let one = self.iconst(Type::I64, 1);
                let next = self.ins().iadd(i, one);
                self.ins().store(next, counter);
                self.jump(header);
                self.switch_to(done, true);
                V::M(addr)
            }
            "from_address" => {
                let usize = self.g.types.usize();
                let v = self.value_as(arg(0), usize);
                let x = self.scalar(v);
                V::S(self.ins().convert(ConvertOp::IntToPtr, Type::Ptr, x))
            }
            "cast" => {
                let from = self.ty(arg(0));
                let v = self.expr(arg(0));
                let x = self.scalar(v);
                let (ft, tt) = (self.g_scalar(from), self.g_scalar(ty));
                V::S(match (ft, tt) {
                    (Type::Ptr, Type::Ptr) => x,
                    (Type::Ptr, t) => {
                        let wide = self.ins().convert(ConvertOp::PtrToInt, Type::I64, x);
                        if t == Type::I64 {
                            wide
                        } else {
                            self.ins().trunc(t, wide)
                        }
                    }
                    (_, Type::Ptr) => {
                        let wide = if ft == Type::I64 {
                            x
                        } else {
                            self.ins().uext(Type::I64, x)
                        };
                        self.ins().convert(ConvertOp::IntToPtr, Type::Ptr, wide)
                    }
                    _ => x,
                })
            }
            "truncate" => {
                let from = self.ty(arg(0));
                let v = self.expr(arg(0));
                let x = self.scalar(v);
                let (Some(a), Some(b)) = (self.int_of(from), self.int_of(ty)) else {
                    return V::S(x);
                };
                let tt = int_type(b.bits());
                V::S(match a.bits().cmp(&b.bits()) {
                    std::cmp::Ordering::Equal => x,
                    std::cmp::Ordering::Greater => self.ins().trunc(tt, x),
                    std::cmp::Ordering::Less => {
                        if a.signed() {
                            self.ins().sext(tt, x)
                        } else {
                            self.ins().uext(tt, x)
                        }
                    }
                })
            }
            "from_pointer" => {
                let p = self.expr(arg(0));
                let p = self.scalar(p);
                let usize = self.g.types.usize();
                let n = self.value_as(arg(1), usize);
                let n = self.scalar(n);
                self.make_view(ty, p, n)
            }
            "offset" => {
                let p = self.expr(arg(0));
                let p = self.scalar(p);
                let usize = self.g.types.usize();
                let n = self.value_as(arg(1), usize);
                let n = self.scalar(n);
                let pty = self.g.core(ty);
                let size = match self.g.types.get(pty) {
                    Ty::Pointer(elem) => {
                        let elem = *elem;
                        self.g.layout(elem).size
                    }
                    _ => 1,
                };
                let size = self.usize_const(size);
                let off = self.ins().imul(n, size);
                V::S(self.ins().ptradd(p, off))
            }
            "c_string" => {
                let bytes = match &self.ast.expr(arg(0)).kind {
                    ExprKind::Str(parts) => string_bytes(self.g, self.file, parts),
                    _ => Vec::new(),
                };
                let symbol = self.g.string_symbol(&bytes);
                V::S(self.data_addr(&symbol))
            }
            "volatile_load" => {
                let p = self.expr(arg(0));
                let p = self.scalar(p);
                self.load(p, ty)
            }
            "volatile_store" => {
                let pty = self.ty(arg(0));
                let p = self.expr(arg(0));
                let p = self.scalar(p);
                let elem = match self.g.types.get(self.g.core(pty)) {
                    Ty::Pointer(e) => *e,
                    _ => return V::Unit,
                };
                let v = self.value_as(arg(1), elem);
                self.store(v, elem, p, false);
                V::Unit
            }
            "wrapping_add" | "wrapping_sub" | "wrapping_mul" | "unchecked_add"
            | "unchecked_sub" | "unchecked_mul" => {
                let a = self.value_as(arg(0), ty);
                let a = self.scalar(a);
                let b = self.value_as(arg(1), ty);
                let b = self.scalar(b);
                V::S(match &name[name.len() - 3..] {
                    "add" => self.ins().iadd(a, b),
                    "sub" => self.ins().isub(a, b),
                    _ => self.ins().imul(a, b),
                })
            }
            "saturating_add" | "saturating_sub" | "saturating_mul" => {
                let a = self.value_as(arg(0), ty);
                let a = self.scalar(a);
                let b = self.value_as(arg(1), ty);
                let b = self.scalar(b);
                let int = self.int_of(ty).unwrap_or(IntTy::U64);
                let t = int_type(int.bits());
                let op = &name[name.len() - 3..];
                let oop = match (op, int.signed()) {
                    ("add", true) => OverflowOp::Sadd,
                    ("add", false) => OverflowOp::Uadd,
                    ("sub", true) => OverflowOp::Ssub,
                    ("sub", false) => OverflowOp::Usub,
                    (_, true) => OverflowOp::Smul,
                    (_, false) => OverflowOp::Umul,
                };
                let (result, overflow) = self.ins().overflow(oop, a, b);
                #[allow(clippy::cast_possible_truncation)]
                let (min, max) = (int.min() as i64, int.max() as i64);
                let bound = if int.signed() {
                    let zero = self.iconst(t, 0);
                    let negative = if op == "mul" {
                        let x = self.ins().xor(a, b);
                        self.ins().icmp(IntCC::Slt, x, zero)
                    } else {
                        self.ins().icmp(IntCC::Slt, a, zero)
                    };
                    let lo = self.iconst(t, min);
                    let hi = self.iconst(t, max);
                    self.ins().select(negative, lo, hi)
                } else if op == "sub" {
                    self.iconst(t, 0)
                } else {
                    self.iconst(t, -1)
                };
                V::S(self.ins().select(overflow, bound, result))
            }
            _ => {
                self.g
                    .unsupported(format!("code generation does not support `{name}` yet"));
                let _ = type_arg;
                self.dummy(ty)
            }
        }
    }
}

/// A piece of a `Format` being built.
enum FormatPiece {
    Text(Vec<u8>),
    Value(V, TyId),
}

enum PushItem {
    One(V),
    Many(Value, Value),
}

/// An argument, ready for the call.
pub enum CallArg {
    Value(Value),
    Addr(Value),
    None,
}

/// Whether an expression holds a call that passes an argument `inout`.
fn changes_in_call(ast: &crate::ast::Ast, e: ExprId) -> bool {
    let any = |es: &mut dyn Iterator<Item = ExprId>| {
        let mut found = false;
        for x in es {
            found |= changes_in_call(ast, x);
        }
        found
    };
    match &ast.expr(e).kind {
        ExprKind::Name(_) | ExprKind::Int | ExprKind::Float | ExprKind::Char => false,
        ExprKind::Str(parts) => any(&mut parts.iter().filter_map(|p| match p {
            crate::ast::StrPart::Interpolation(x) => Some(*x),
            crate::ast::StrPart::Text(_) => None,
        })),
        ExprKind::Paren(x)
        | ExprKind::Deref(x)
        | ExprKind::Try(x)
        | ExprKind::Unsafe(x)
        | ExprKind::Unary { operand: x, .. }
        | ExprKind::Field { value: x, .. } => changes_in_call(ast, *x),
        ExprKind::Tuple(fields) => any(&mut fields.iter().map(|(_, x)| *x)),
        ExprKind::Call { callee, args } => {
            args.iter().any(|a| a.mode != crate::ast::Mode::Let)
                || changes_in_call(ast, *callee)
                || any(&mut args.iter().map(|a| a.value))
        }
        ExprKind::Construct { ty, value } => {
            changes_in_call(ast, *ty) || changes_in_call(ast, *value)
        }
        ExprKind::Index { value, index } => {
            changes_in_call(ast, *value)
                || match index {
                    Index::Expr(x) => changes_in_call(ast, *x),
                    Index::Slice { start, end } => any(&mut start.iter().chain(end).copied()),
                    Index::Type(_) => false,
                }
        }
        ExprKind::Binary { left, right, .. } => {
            changes_in_call(ast, *left) || changes_in_call(ast, *right)
        }
        ExprKind::If {
            cond,
            then,
            otherwise,
        } => {
            let other = match otherwise {
                crate::ast::IfElse::Branch(x) | crate::ast::IfElse::If(x) => *x,
            };
            any(&mut [*cond, *then, other].into_iter())
        }
        ExprKind::Match { value, arms } => {
            changes_in_call(ast, *value) || any(&mut arms.iter().map(|a| a.body))
        }
        ExprKind::Array(elems) => any(&mut elems.iter().copied()),
    }
}

/// A panic helper's parameters: a 64-bit integer for each value in the
/// message, or the address of a `Format`, then the site's `PanicInfo`.
fn panic_helper_params(message: Option<&[MessagePart]>) -> Vec<Type> {
    let mut params: Vec<Type> = match message {
        None => vec![Type::Ptr],
        Some(parts) => parts
            .iter()
            .filter(|p| !matches!(p, MessagePart::Text(_)))
            .map(|_| Type::I64)
            .collect(),
    };
    params.push(Type::Ptr);
    params
}
