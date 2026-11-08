//! Code generation: lowers a checked program to one keel module.
//!
//! Scalars (integers, `bool`, floats, pointers, function values and
//! payload-free enums) are keel values. Every other value lives in memory
//! and is handled through its address. Every local has a stack slot;
//! keel's `stack_promote` pass moves the scalar ones into registers.
//!
//! Every function uses the C calling convention. A value held in memory
//! is returned through `sret`, and an `inout` argument is passed as a
//! pointer. A `let` parameter held in memory is a pointer to the caller's
//! value, which C sees as `const T *`; a record an `extern` function
//! takes is passed `byval`, as C passes records.

mod data;
mod func;
mod layout;

use std::collections::{HashMap, HashSet};

use keel_ir::{
    AbiParam, ArgExt, DataInit, DataItem, DataObject, Function, InsertPoint, IntCC, Module,
    Purpose, Signature, Type,
};

use crate::ast::{Attribute, DeclKind, Mode};
use crate::check::Checked;
use crate::package::{FileId, PackageId, Program};
use crate::resolve::{DeclRef, Resolution};
use crate::types::{Ty, TyId, Types};
use layout::{Repr, TyLayout};

/// A failure to generate code: a feature stage 1 does not lower yet.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowers a program that checked without errors.
pub fn generate(program: &Program, names: &Resolution, checked: Checked) -> Result<Module, Error> {
    let root_name = program
        .packages
        .iter()
        .find(|p| p.path.is_empty())
        .and_then(|p| p.dir.canonicalize().ok())
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "main".to_owned());
    let mut g = Gen {
        program,
        names,
        types: checked.types,
        files: checked.files,
        fn_tys: checked.fn_tys,
        root_name,
        module: Module::default(),
        layouts: HashMap::new(),
        strings: HashMap::new(),
        grow: None,
        panic_helpers: HashMap::new(),
        panic_sites: 0,
        references: Vec::new(),
        referenced: HashSet::new(),
        data_names: HashSet::new(),
        errors: Vec::new(),
    };
    // The symbols code outside the program may refer to.
    let mut exported: Vec<String> = Vec::new();
    for (i, file) in program.files.iter().enumerate() {
        let file_id = FileId(u32::try_from(i).expect("few files"));
        for index in 0..file.ast.decls.len() {
            let decl = DeclRef {
                file: file_id,
                index,
            };
            match &file.ast.decls[index].kind {
                DeclKind::Fn(_) => {
                    if g.attribute(decl, "export").is_some() {
                        exported.push(g.symbol(decl));
                    }
                    g.function(decl);
                }
                DeclKind::Global { .. } => g.global(decl),
                DeclKind::ExternVar { .. } => {
                    let name = g.symbol(decl);
                    g.reference_data(&name);
                }
                DeclKind::ExternFn { .. } | DeclKind::Type { .. } => {}
            }
        }
    }
    // Every function referred to but not defined is declared, in the order
    // of the first reference, as keel's C API declares them. A name both
    // declared and defined, such as `tern_main`, is defined.
    let defined: HashSet<String> = g.module.functions.iter().map(|f| f.name.clone()).collect();
    g.module.decls = std::mem::take(&mut g.references)
        .into_iter()
        .filter(|d| !defined.contains(&d.name))
        .collect();
    // The module is the whole program, so everything not exported is
    // local to it, which lets keel inline it.
    keel_ir::internalize(&mut g.module, |name| exported.iter().any(|e| e == name));
    if let Some(first) = g.errors.into_iter().next() {
        return Err(first);
    }
    Ok(g.module)
}

pub struct Gen<'a> {
    program: &'a Program,
    names: &'a Resolution,
    pub types: Types,
    files: Vec<crate::check::FileTypes>,
    fn_tys: HashMap<DeclRef, TyId>,
    /// The root package's directory name, which prefixes its symbols.
    root_name: String,
    pub module: Module,
    layouts: HashMap<TyId, TyLayout>,
    /// The data object for each string literal's bytes.
    strings: HashMap<Vec<u8>, String>,
    /// The symbol of the helper that grows a `[..]T`, once made.
    grow: Option<String>,
    /// The cold helper that reports each kind of panic: one per message
    /// the compiler writes, and `None` for `panic(message)`.
    panic_helpers: HashMap<Option<Vec<MessagePart>>, String>,
    /// How many panic sites have their `PanicInfo` in data.
    panic_sites: usize,
    /// Every function referred to, in the order of the first reference.
    references: Vec<keel_ir::FuncDecl>,
    referenced: HashSet<String>,
    /// The names of the module's data objects.
    data_names: HashSet<String>,
    errors: Vec<Error>,
}

/// A piece of a panic message the compiler writes: text, or a 64-bit
/// integer the helper takes as a parameter.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MessagePart {
    Text(String),
    Unsigned,
    Signed,
}

/// How one parameter crosses a call.
#[derive(Clone, Copy, Debug)]
pub enum Pass {
    Scalar,
    /// The address of the value; the callee gets its own copy.
    ByVal,
    /// The address of the caller's value, which the callee only reads.
    Borrow,
    /// The address of the caller's place.
    Ref,
    /// A value with no bytes, which is not passed.
    Skip,
}

/// A function's signature and how its parameters and result cross.
pub struct FnAbi {
    pub sig: Signature,
    pub params: Vec<Pass>,
    /// The result goes through memory the caller passes first.
    pub sret: bool,
    pub result: Option<TyId>,
    pub fn_ty: TyId,
}

/// `tern.grow`'s signature.
pub fn grow_signature() -> Signature {
    let mut sig = Signature::new(&[Type::Ptr, Type::I64, Type::I64, Type::I64], &[Type::I8]);
    sig.returns[0].ext = ArgExt::Uext;
    sig
}

impl<'a> Gen<'a> {
    fn text(&self, file: FileId, span: crate::source::Span) -> &'a str {
        self.program.file(file).source.slice(span)
    }

    fn decl_kind(&self, decl: DeclRef) -> &'a DeclKind {
        &self.program.file(decl.file).ast.decls[decl.index].kind
    }

    fn decl_name(&self, decl: DeclRef) -> &'a str {
        let name = match self.decl_kind(decl) {
            DeclKind::Fn(f) => f.name,
            DeclKind::ExternFn { name, .. }
            | DeclKind::ExternVar { name, .. }
            | DeclKind::Type { name, .. } => *name,
            DeclKind::Global { local, .. } => match &local.binding {
                crate::ast::Binding::Name { name: Some(n), .. } => *n,
                _ => return "_",
            },
        };
        self.text(decl.file, name.span)
    }

    fn attribute(&self, decl: DeclRef, name: &str) -> Option<&'a Attribute> {
        let file = decl.file;
        self.program.file(file).ast.decls[decl.index]
            .attributes
            .iter()
            .find(|a| self.text(file, a.name).trim_start_matches('@') == name)
    }

    /// A package's prefix for symbols: `std.io`, or the root's directory
    /// name and then the package's path.
    fn package_prefix(&self, package: PackageId) -> String {
        let path = &self.program.package(package).path;
        if path.is_empty() {
            self.root_name.clone()
        } else if path == "std" || path.starts_with("std.") {
            path.clone()
        } else {
            format!("{}.{path}", self.root_name)
        }
    }

    /// The linker symbol of a function or global.
    pub fn symbol(&self, decl: DeclRef) -> String {
        let name = self.decl_name(decl);
        match self.decl_kind(decl) {
            DeclKind::ExternFn { .. } | DeclKind::ExternVar { .. } => return name.to_owned(),
            DeclKind::Fn(_) => {
                if let Some(export) = self.attribute(decl, "export") {
                    return match export.args.first() {
                        Some(&arg) => {
                            let span = self.program.file(decl.file).ast.expr(arg).span;
                            self.text(decl.file, span).trim_matches('"').to_owned()
                        }
                        None => name.to_owned(),
                    };
                }
                let package = self.program.file(decl.file).package;
                if self.program.package(package).path.is_empty() && name == "main" {
                    return "tern_main".to_owned();
                }
            }
            _ => {}
        }
        let package = self.program.file(decl.file).package;
        format!("{}.{name}", self.package_prefix(package))
    }

    pub fn fn_ty(&self, decl: DeclRef) -> TyId {
        self.fn_tys[&decl]
    }

    fn abi_param(&mut self, ty: TyId) -> AbiParam {
        let Repr::Scalar(t) = self.repr(ty) else {
            unreachable!("only scalars are plain parameters");
        };
        let mut param = AbiParam::new(t);
        if matches!(t, Type::I8 | Type::I16) {
            let signed = self
                .types
                .as_int(self.core(ty))
                .is_some_and(crate::types::IntTy::signed);
            param.ext = if signed { ArgExt::Sext } else { ArgExt::Uext };
        }
        param
    }

    /// The ABI of a function type. `c_records` is set for an `extern`
    /// function, which takes records by value as C does.
    pub fn fn_abi(&mut self, fn_ty: TyId, variadic: bool, c_records: bool) -> FnAbi {
        let ty = self.core(fn_ty);
        let Ty::Fn(f) = self.types.get(ty).clone() else {
            unreachable!("a function type");
        };
        let mut sig = Signature::new(&[], &[]);
        sig.variadic = variadic;
        let mut sret = false;
        if let Some(result) = f.result {
            match self.repr(result) {
                Repr::Scalar(_) => sig.returns.push(self.abi_param(result)),
                Repr::Memory => {
                    sret = true;
                    let layout = self.keel_layout(result);
                    sig.params.push(AbiParam {
                        ty: Type::Ptr,
                        ext: ArgExt::None,
                        purpose: Purpose::Sret(layout),
                    });
                }
                Repr::Unit => {}
            }
        }
        let mut params = Vec::new();
        for (mode, ty) in &f.params {
            let pass = match (mode, self.repr(*ty)) {
                (Mode::Inout, _) => Pass::Ref,
                (_, Repr::Unit) => Pass::Skip,
                (_, Repr::Scalar(_)) => Pass::Scalar,
                (Mode::Let, Repr::Memory) if !c_records => Pass::Borrow,
                (_, Repr::Memory) => Pass::ByVal,
            };
            match pass {
                Pass::Scalar => sig.params.push(self.abi_param(*ty)),
                Pass::Ref | Pass::Borrow => sig.params.push(AbiParam::new(Type::Ptr)),
                Pass::ByVal => {
                    let layout = self.keel_layout(*ty);
                    sig.params.push(AbiParam {
                        ty: Type::Ptr,
                        ext: ArgExt::None,
                        purpose: Purpose::ByVal(layout),
                    });
                }
                Pass::Skip => {}
            }
            params.push(pass);
        }
        FnAbi {
            sig,
            params,
            sret,
            result: f.result,
            fn_ty,
        }
    }

    pub fn decl_abi(&mut self, decl: DeclRef) -> FnAbi {
        let variadic = matches!(
            self.decl_kind(decl),
            DeclKind::ExternFn { variadic: true, .. }
        );
        let c_records = matches!(self.decl_kind(decl), DeclKind::ExternFn { .. });
        let ty = self.fn_ty(decl);
        self.fn_abi(ty, variadic, c_records)
    }

    /// The data object holding a string's bytes and a NUL after them.
    pub fn string_symbol(&mut self, bytes: &[u8]) -> String {
        if let Some(name) = self.strings.get(bytes) {
            return name.clone();
        }
        let name = format!("tern.str.{}", self.strings.len());
        let mut with_nul = bytes.to_vec();
        with_nul.push(0);
        self.add_data(DataObject {
            name: name.clone(),
            local: true,
            init: Some(DataInit {
                constant: true,
                align: 1,
                items: vec![DataItem::Bytes(with_nul)],
            }),
            ..DataObject::default()
        });
        self.strings.insert(bytes.to_vec(), name.clone());
        name
    }

    /// The symbol of `tern.grow(v: *[..]T, size, align, needed) -> bool`,
    /// which makes room for `needed` elements of `size` bytes, through
    /// the array's allocator.
    pub fn grow_symbol(&mut self) -> String {
        if let Some(name) = &self.grow {
            return name.clone();
        }
        let name = "tern.grow".to_owned();
        let mut f = Function::new(name.clone(), grow_signature());
        f.local = true;
        // Callers test the capacity first, so this only runs to grow.
        f.attrs.noinline = true;
        f.attrs.cold = true;
        let entry = f.create_block();
        let v = f.append_block_param(entry, Type::Ptr);
        let size = f.append_block_param(entry, Type::I64);
        let align = f.append_block_param(entry, Type::I64);
        let needed = f.append_block_param(entry, Type::I64);
        let at = |b| InsertPoint::End(b);
        let cap_addr = {
            let off = f.ins(at(entry)).iconst(Type::I64, 16);
            f.ins(at(entry)).ptradd(v, off)
        };
        let cap = f.ins(at(entry)).load(Type::I64, cap_addr);
        let enough = f.ins(at(entry)).icmp(IntCC::Ule, needed, cap);
        let done = f.create_block();
        let grow = f.create_block();
        f.ins(at(entry)).brif(enough, done, &[], grow, &[]);
        let one = f.ins(at(done)).iconst(Type::I8, 1);
        f.ins(at(done)).return_(&[one]);
        // The new capacity: twice the old, at least 4 and at least needed.
        let two = f.ins(at(grow)).iconst(Type::I64, 2);
        let doubled = f.ins(at(grow)).imul(cap, two);
        let four = f.ins(at(grow)).iconst(Type::I64, 4);
        let more = f.ins(at(grow)).icmp(IntCC::Ugt, doubled, four);
        let base = f.ins(at(grow)).select(more, doubled, four);
        let fits = f.ins(at(grow)).icmp(IntCC::Ugt, base, needed);
        let new_cap = f.ins(at(grow)).select(fits, base, needed);
        let (bytes, overflow) = f
            .ins(at(grow))
            .overflow(keel_ir::OverflowOp::Umul, new_cap, size);
        let fail = f.create_block();
        let call = f.create_block();
        f.ins(at(grow)).brif(overflow, fail, &[], call, &[]);
        let zero = f.ins(at(fail)).iconst(Type::I8, 0);
        f.ins(at(fail)).return_(&[zero]);
        // alloc is at offset 24: context, alloc, resize, free.
        let load_at = |f: &mut Function, b, base, off: i64, ty| {
            let off = f.ins(at(b)).iconst(Type::I64, off);
            let addr = f.ins(at(b)).ptradd(base, off);
            f.ins(at(b)).load(ty, addr)
        };
        let ptr = f.ins(at(call)).load(Type::Ptr, v);
        let context = load_at(&mut f, call, v, 24, Type::Ptr);
        let null = f.ins(at(call)).iconst(Type::I64, 0);
        let null = f
            .ins(at(call))
            .convert(keel_ir::ConvertOp::IntToPtr, Type::Ptr, null);
        let fresh = f.ins(at(call)).icmp(IntCC::Eq, ptr, null);
        let alloc_block = f.create_block();
        let resize_block = f.create_block();
        let after = f.create_block();
        let result = f.append_block_param(after, Type::Ptr);
        f.ins(at(call))
            .brif(fresh, alloc_block, &[], resize_block, &[]);
        let alloc_fn = load_at(&mut f, alloc_block, v, 32, Type::Ptr);
        let alloc_sig = f.import_signature(Signature::new(
            &[Type::Ptr, Type::I64, Type::I64],
            &[Type::Ptr],
        ));
        let inst =
            f.ins(at(alloc_block))
                .call_indirect(alloc_sig, alloc_fn, &[context, bytes, align]);
        let p = f.first_result(inst);
        f.ins(at(alloc_block)).jump(after, &[p]);
        let resize_fn = load_at(&mut f, resize_block, v, 40, Type::Ptr);
        let resize_sig = f.import_signature(Signature::new(
            &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64],
            &[Type::Ptr],
        ));
        let old_bytes = f.ins(at(resize_block)).imul(cap, size);
        let inst = f.ins(at(resize_block)).call_indirect(
            resize_sig,
            resize_fn,
            &[context, ptr, old_bytes, bytes, align],
        );
        let p = f.first_result(inst);
        f.ins(at(resize_block)).jump(after, &[p]);
        let failed = f.ins(at(after)).icmp(IntCC::Eq, result, null);
        let stored = f.create_block();
        f.ins(at(after)).brif(failed, fail, &[], stored, &[]);
        f.ins(at(stored)).store(result, v);
        f.ins(at(stored)).store(new_cap, cap_addr);
        let one = f.ins(at(stored)).iconst(Type::I8, 1);
        f.ins(at(stored)).return_(&[one]);
        self.module.functions.push(f);
        self.grow = Some(name.clone());
        name
    }

    /// A constant `PanicInfo` for a panic at `line` and `column` of `path`
    /// in `function`, with an empty message the helper fills in.
    pub fn panic_site(&mut self, path: &str, function: &str, line: u32, column: u32) -> String {
        let info_ty = self.types.named_ty(crate::types::NamedId::Builtin(
            crate::types::BuiltinNamed::PanicInfo,
        ));
        let mut fields: Vec<(u64, Vec<DataItem>)> = Vec::new();
        for (field, text) in [("file", path), ("function", function)] {
            let symbol = self.string_symbol(text.as_bytes());
            let (_, off) = self.field(info_ty, field);
            let len = i64::try_from(text.len()).expect("a short name");
            fields.push((
                off,
                vec![
                    DataItem::Ptr { symbol, addend: 0 },
                    DataItem::Int {
                        ty: Type::I64,
                        value: len,
                    },
                ],
            ));
        }
        for (field, n) in [("line", line), ("column", column)] {
            let (_, off) = self.field(info_ty, field);
            let value = i64::from(n);
            fields.push((
                off,
                vec![DataItem::Int {
                    ty: Type::I32,
                    value,
                }],
            ));
        }
        fields.sort_by_key(|(off, _)| *off);
        let layout = self.layout(info_ty);
        let mut items = Vec::new();
        let mut at = 0;
        for (off, field) in fields {
            if off > at {
                items.push(DataItem::Zero(u32::try_from(off - at).expect("small")));
            }
            at = off + field.iter().map(DataItem::size).sum::<u64>();
            items.extend(field);
        }
        if layout.size > at {
            items.push(DataItem::Zero(
                u32::try_from(layout.size - at).expect("small"),
            ));
        }
        let name = format!("tern.panic_site.{}", self.panic_sites);
        self.panic_sites += 1;
        self.add_data(DataObject {
            name: name.clone(),
            local: true,
            init: Some(DataInit {
                constant: true,
                align: u32::try_from(layout.align).expect("small"),
                items,
            }),
            ..DataObject::default()
        });
        name
    }

    /// A data object that code refers to. Until it is defined, it is
    /// declared where it was first referred to, as keel's C API places it.
    pub fn reference_data(&mut self, name: &str) {
        if self.data_names.insert(name.to_owned()) {
            self.module.data.push(DataObject {
                name: name.to_owned(),
                ..DataObject::default()
            });
        }
    }

    /// Defines a data object, in the place of its declaration if code
    /// referred to it first.
    pub fn add_data(&mut self, object: DataObject) {
        if self.data_names.insert(object.name.clone()) {
            self.module.data.push(object);
        } else if let Some(slot) = self.module.data.iter_mut().find(|d| d.name == object.name) {
            *slot = object;
        }
    }

    /// `std.fmt.Part`, if the program uses std.fmt.
    pub fn fmt_part(&mut self) -> Option<TyId> {
        let index = self
            .program
            .packages
            .iter()
            .position(|p| p.path == "std.fmt")?;
        let decl = *self.names.scopes[index].get("Part")?;
        let id = crate::types::NamedId::Decl(decl);
        self.types
            .named
            .contains_key(&id)
            .then(|| self.types.named_ty(id))
    }

    /// The `@panic_handler` function: the program's own, or std.rt's.
    pub fn panic_handler(&self) -> Option<DeclRef> {
        let mut found: Option<DeclRef> = None;
        for (i, file) in self.program.files.iter().enumerate() {
            let file_id = FileId(u32::try_from(i).expect("few files"));
            for index in 0..file.ast.decls.len() {
                let decl = DeclRef {
                    file: file_id,
                    index,
                };
                if matches!(self.decl_kind(decl), DeclKind::Fn(_))
                    && self.attribute(decl, "panic_handler").is_some()
                {
                    let in_std = self.program.package(file.package).path.starts_with("std");
                    if found.is_none() || !in_std {
                        found = Some(decl);
                    }
                }
            }
        }
        found
    }

    /// Takes the next place among the module's functions for a function
    /// being built, so functions keep the order they were started in, as
    /// keel's C API orders them.
    pub fn reserve_function(&mut self) -> usize {
        self.module
            .functions
            .push(Function::new(String::new(), Signature::new(&[], &[])));
        self.module.functions.len() - 1
    }

    /// Records a reference to the function `name`.
    pub fn reference(&mut self, name: &str, sig: &Signature) {
        if self.referenced.insert(name.to_owned()) {
            self.references.push(keel_ir::FuncDecl {
                name: name.to_owned(),
                sig: sig.clone(),
                attrs: keel_ir::function::FunctionAttrs::default(),
            });
        }
    }

    fn unsupported(&mut self, what: impl Into<String>) {
        self.errors.push(Error(what.into()));
    }
}
