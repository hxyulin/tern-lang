//! Name resolution: connects every name to what it refers to.
//!
//! A name is looked up in this order: the enclosing locals, innermost
//! first; the package's top-level declarations; the file's imports; the
//! built-in names. Rules checked here:
//!
//! - A top-level name is declared once per package.
//! - A local never reuses a name already in scope: another local, a
//!   top-level declaration, an import or a built-in. Top-level
//!   declarations may reuse a built-in name.
//! - Unused locals, parameters and imports are errors; `_` discards.
//! - Another package's declarations are reachable only if `pub`.
//! - A type position names a type.
//!
//! Field names after `.` on a value, and variant names in patterns, need
//! types; the type checker resolves those.

use std::collections::HashMap;

use crate::ast::{
    Ast, Binding, Block, Decl, DeclKind, DestructField, Else, EnumItem, ExprId, ExprKind, ForKind,
    IfElse, Index, Iterable, Name, PatId, PatternKind, StmtId, StmtKind, StrPart, TypeDef, TypeId,
    TypeKind,
};
use crate::diagnostic::Diagnostic;
use crate::package::{FileId, PackageId, Program};
use crate::source::Span;

/// The built-in names that are types.
pub const BUILTIN_TYPES: &[&str] = &[
    "i8",
    "i16",
    "i32",
    "i64",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "f32",
    "f64",
    "bool",
    "Allocator",
    "Format",
    "PanicInfo",
    "OutOfMemory",
];

/// The built-in values. Like the types, no local may take their names.
pub const BUILTIN_VALUES: &[&str] = &["true", "false", "none", "some", "ok", "err"];

/// The built-in functions. They are only ever called, so a local may take
/// one of their names: a bare name means the local, and a call means the
/// built-in unless the package declares that name.
pub const BUILTIN_FUNCTIONS: &[&str] = &[
    "panic",
    "make",
    "push",
    "pop",
    "append",
    "clear",
    "free",
    "take",
    "copy",
    "fill",
    "from_address",
    "from_pointer",
    "offset",
    "cast",
    "truncate",
    "c_string",
    "volatile_load",
    "volatile_store",
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "saturating_add",
    "saturating_sub",
    "saturating_mul",
    "unchecked_add",
    "unchecked_sub",
    "unchecked_mul",
];

fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN_TYPES
        .iter()
        .chain(BUILTIN_VALUES)
        .chain(BUILTIN_FUNCTIONS)
        .find(|b| **b == name)
        .copied()
}

/// A top-level declaration: its file and its index in that file's
/// declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeclRef {
    pub file: FileId,
    pub index: usize,
}

/// A local variable or parameter, numbered within its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalId(pub u32);

/// What a name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Res {
    Local(LocalId),
    Decl(DeclRef),
    Package(PackageId),
    Builtin(&'static str),
}

/// The resolution of one file, indexed like its syntax tree.
#[derive(Default)]
pub struct FileNames {
    /// For a name expression, or a field expression that names a member
    /// of an imported package.
    pub exprs: Vec<Option<Res>>,
    /// For a named type.
    pub types: Vec<Option<Res>>,
    /// Where each local was declared.
    pub locals: Vec<Span>,
    /// The local each binding declares, keyed by the start of its name.
    pub bindings: HashMap<u32, LocalId>,
    /// The `shared` global each name in a `lock` takes, keyed by the start
    /// of the name.
    pub locks: HashMap<u32, DeclRef>,
    /// A call to a built-in function made while a local has its name,
    /// keyed by the start of the callee's name. The checker rejects the
    /// call if that local is a function value, which the reader might
    /// take it to call.
    pub hidden_locals: HashMap<u32, LocalId>,
}

pub struct Resolution {
    pub files: Vec<FileNames>,
    /// Each package's top-level names.
    pub scopes: Vec<HashMap<String, DeclRef>>,
    pub diagnostics: Vec<(FileId, Diagnostic)>,
}

pub fn resolve(program: &Program) -> Resolution {
    let mut diagnostics = Vec::new();
    let scopes: Vec<HashMap<String, DeclRef>> = program
        .packages
        .iter()
        .map(|package| {
            let mut scope = HashMap::new();
            for &file in &package.files {
                declare_top_level(program, file, &mut scope, &mut diagnostics);
            }
            scope
        })
        .collect();
    let by_path: HashMap<&str, PackageId> = program
        .packages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let id = PackageId(u32::try_from(i).expect("few packages"));
            (p.path.as_str(), id)
        })
        .collect();

    let mut files = Vec::new();
    for (i, file) in program.files.iter().enumerate() {
        let id = FileId(u32::try_from(i).expect("few files"));
        let mut resolver = FileResolver {
            program,
            scopes: &scopes,
            file: id,
            ast: &file.ast,
            text: &file.source.text,
            package: file.package,
            imports: HashMap::new(),
            locals: Vec::new(),
            out: FileNames {
                exprs: vec![None; file.ast.exprs.len()],
                types: vec![None; file.ast.types.len()],
                locals: Vec::new(),
                bindings: HashMap::new(),
                locks: HashMap::new(),
                hidden_locals: HashMap::new(),
            },
            diagnostics: Vec::new(),
        };
        resolver.file(&by_path);
        resolver.diagnostics.sort_by_key(|d| d.span.start);
        diagnostics.extend(resolver.diagnostics.into_iter().map(|d| (id, d)));
        files.push(resolver.out);
    }
    Resolution {
        files,
        scopes,
        diagnostics,
    }
}

/// The names a declaration puts in its package's scope.
fn declared_names(decl: &Decl) -> Vec<Name> {
    match &decl.kind {
        DeclKind::Fn(f) => vec![f.name],
        DeclKind::ExternFn { name, .. }
        | DeclKind::ExternVar { name, .. }
        | DeclKind::Type { name, .. } => vec![*name],
        DeclKind::Global { local, .. } => match &local.binding {
            Binding::Name { name, .. } => name.iter().copied().collect(),
            Binding::Destructure(fields) => fields
                .iter()
                .filter(|f| f.ty.is_some())
                .map(|f| f.rename.unwrap_or(f.field))
                .collect(),
        },
    }
}

fn declare_top_level(
    program: &Program,
    file: FileId,
    scope: &mut HashMap<String, DeclRef>,
    diagnostics: &mut Vec<(FileId, Diagnostic)>,
) {
    let f = program.file(file);
    for (index, decl) in f.ast.decls.iter().enumerate() {
        for name in declared_names(decl) {
            let text = f.source.slice(name.span).to_owned();
            if let Some(previous) = scope.get(&text) {
                let at = describe_decl(program, *previous);
                diagnostics.push((
                    file,
                    Diagnostic::error(
                        name.span,
                        format!("`{text}` is already declared in this package, at {at}"),
                    ),
                ));
            } else {
                scope.insert(text, DeclRef { file, index });
            }
        }
    }
}

/// `path:line` of a declaration, for messages.
fn describe_decl(program: &Program, decl: DeclRef) -> String {
    let f = program.file(decl.file);
    let pos = f.source.position(f.ast.decls[decl.index].span.start);
    format!("{}:{}", f.source.path.display(), pos.line)
}

struct Import {
    /// `None` when the package failed to load; the loader reported it.
    package: Option<PackageId>,
    span: Span,
    used: bool,
}

struct LocalBinding {
    name: String,
    id: LocalId,
    span: Span,
    used: bool,
    /// The field name, for a binding from a destructured field.
    field: Option<String>,
}

struct FileResolver<'a> {
    program: &'a Program,
    scopes: &'a [HashMap<String, DeclRef>],
    file: FileId,
    ast: &'a Ast,
    text: &'a str,
    package: PackageId,
    imports: HashMap<String, Import>,
    /// One entry per enclosing block, innermost last.
    locals: Vec<Vec<LocalBinding>>,
    out: FileNames,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> FileResolver<'a> {
    fn name_text(&self, name: Name) -> &'a str {
        &self.text[name.span.range()]
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(span, message));
    }

    fn line_of(&self, span: Span) -> u32 {
        self.program
            .file(self.file)
            .source
            .position(span.start)
            .line
    }

    fn package_scope(&self) -> &'a HashMap<String, DeclRef> {
        &self.scopes[self.package.0 as usize]
    }

    fn file(&mut self, by_path: &HashMap<&str, PackageId>) {
        for import in &self.ast.imports {
            let path: Vec<&str> = import.path.iter().map(|n| self.name_text(*n)).collect();
            let local = import.alias.unwrap_or(*import.path.last().expect("a path"));
            let local_text = self.name_text(local).to_owned();
            if let Some(decl) = self.package_scope().get(&local_text) {
                let at = describe_decl(self.program, *decl);
                self.error(
                    local.span,
                    format!("`{local_text}` is both imported and declared at {at}; rename the import with `as`"),
                );
            }
            // The loader reports a name imported twice.
            self.imports.entry(local_text).or_insert(Import {
                package: by_path.get(path.join(".").as_str()).copied(),
                span: import.span,
                used: false,
            });
        }
        for decl in &self.ast.decls {
            self.decl(decl);
        }
        let mut unused: Vec<(Span, String)> = self
            .imports
            .iter()
            .filter(|(_, i)| !i.used && i.package.is_some())
            .map(|(name, i)| (i.span, name.clone()))
            .collect();
        unused.sort_by_key(|(span, _)| span.start);
        for (span, name) in unused {
            self.error(span, format!("`{name}` is imported but never used"));
        }
    }

    // Scopes.

    fn push_scope(&mut self) {
        self.locals.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        let scope = self.locals.pop().expect("a scope is open");
        for local in scope.into_iter().filter(|l| !l.used) {
            let fix = match &local.field {
                Some(field) => format!("skip the field with `{field}: _`"),
                None => "name it `_` to discard it".to_owned(),
            };
            self.error(local.span, format!("`{}` is never used; {fix}", local.name));
        }
    }

    /// Declares a local in the innermost scope.
    fn bind(&mut self, name: Name) {
        self.bind_field(name, None);
    }

    /// Declares a local; `field` is the field it comes from, if any.
    fn bind_field(&mut self, name: Name, field: Option<Name>) {
        let text = self.name_text(name);
        let earlier = self
            .locals
            .iter()
            .flatten()
            .find(|l| l.name == text)
            .map(|l| l.span);
        if let Some(earlier) = earlier {
            let line = self.line_of(earlier);
            self.error(
                name.span,
                format!("`{text}` is already a local, declared on line {line}; pick another name"),
            );
        } else if let Some(decl) = self.package_scope().get(text) {
            let at = describe_decl(self.program, *decl);
            self.error(
                name.span,
                format!("`{text}` is already declared at {at}; pick another name"),
            );
        } else if self.imports.contains_key(text) {
            self.error(
                name.span,
                format!("`{text}` is an imported package; pick another name"),
            );
        } else if BUILTIN_TYPES.contains(&text) || BUILTIN_VALUES.contains(&text) {
            self.error(
                name.span,
                format!("`{text}` is a built-in name; pick another name"),
            );
        }
        let field = field.map(|f| self.name_text(f).to_owned());
        let id = LocalId(u32::try_from(self.out.locals.len()).expect("few locals"));
        self.out.locals.push(name.span);
        self.out.bindings.insert(name.span.start, id);
        self.locals
            .last_mut()
            .expect("a scope is open")
            .push(LocalBinding {
                name: text.to_owned(),
                id,
                span: name.span,
                used: false,
                field,
            });
    }

    /// Looks a name up, marking a local or import as used.
    fn lookup(&mut self, name: Name) -> Option<Res> {
        let text = self.name_text(name);
        if let Some(local) = self
            .locals
            .iter_mut()
            .rev()
            .flat_map(|scope| scope.iter_mut().rev())
            .find(|l| l.name == text)
        {
            local.used = true;
            return Some(Res::Local(local.id));
        }
        if let Some(decl) = self.package_scope().get(text) {
            return Some(Res::Decl(*decl));
        }
        if let Some(import) = self.imports.get_mut(text) {
            import.used = true;
            return import.package.map(Res::Package);
        }
        builtin(text).map(Res::Builtin)
    }

    /// A call's callee. A built-in function's name, bare or with an
    /// argument in brackets as in `fill[4](0)`, skips the locals: a local
    /// of that name does not hide the built-in.
    fn callee(&mut self, id: ExprId) {
        let (name, bracketed) = match &self.ast.expr(id).kind {
            ExprKind::Name(name) => (*name, None),
            ExprKind::Index {
                value,
                index: index @ (Index::Type(_) | Index::Expr(_)),
            } => match self.ast.expr(*value).kind {
                ExprKind::Name(name) => (name, Some((*value, index))),
                _ => return self.expr(id),
            },
            _ => return self.expr(id),
        };
        let text = self.name_text(name);
        let Some(&builtin) = BUILTIN_FUNCTIONS.iter().find(|b| **b == text) else {
            return self.expr(id);
        };
        let res = match self.package_scope().get(text) {
            Some(decl) => Res::Decl(*decl),
            None => {
                let local = self.locals.iter().flatten().find(|l| l.name == text);
                if let Some(local) = local {
                    let id = local.id;
                    self.out.hidden_locals.insert(name.span.start, id);
                }
                Res::Builtin(builtin)
            }
        };
        match bracketed {
            None => self.out.exprs[id.index()] = Some(res),
            Some((value, index)) => {
                self.out.exprs[value.index()] = Some(res);
                match index {
                    Index::Type(ty) => self.ty(*ty),
                    Index::Expr(e) => self.expr(*e),
                    Index::Slice { .. } => unreachable!("matched above"),
                }
            }
        }
    }

    /// `pkg.name`: a member of an imported package.
    fn member(&mut self, package: PackageId, name: Name) -> Option<DeclRef> {
        let text = self.name_text(name);
        let path = &self.program.package(package).path;
        let Some(&decl) = self.scopes[package.0 as usize].get(text) else {
            self.error(name.span, format!("package `{path}` has no `{text}`"));
            return None;
        };
        let target = self.program.file(decl.file);
        if !target.ast.decls[decl.index].public {
            self.error(
                name.span,
                format!("`{text}` is private to package `{path}`; it needs `pub`"),
            );
        }
        Some(decl)
    }

    fn is_type_decl(&self, decl: DeclRef) -> bool {
        matches!(
            self.program.file(decl.file).ast.decls[decl.index].kind,
            DeclKind::Type { .. }
        )
    }

    // Declarations.

    fn decl(&mut self, decl: &'a Decl) {
        match &decl.kind {
            DeclKind::Fn(f) => {
                self.push_scope();
                for param in &f.params {
                    self.ty(param.ty);
                }
                if let Some(result) = f.result {
                    self.ty(result);
                }
                for param in &f.params {
                    if let Some(name) = param.name {
                        self.bind(name);
                    }
                }
                self.block(&f.body);
                self.pop_scope();
            }
            DeclKind::ExternFn { params, result, .. } => {
                for param in params {
                    self.ty(param.ty);
                }
                if let Some(result) = result {
                    self.ty(*result);
                }
            }
            DeclKind::ExternVar { ty, .. } => self.ty(*ty),
            DeclKind::Type { def, .. } => match def {
                TypeDef::Type(ty) => self.ty(*ty),
                TypeDef::Enum { int_type, items } => {
                    if let Some(ty) = int_type {
                        self.ty(*ty);
                    }
                    for item in items {
                        if let EnumItem::Variant { payload, value, .. } = item {
                            if let Some(ty) = payload {
                                self.ty(*ty);
                            }
                            if let Some(value) = value {
                                self.expr(*value);
                            }
                        }
                    }
                }
                TypeDef::Union(fields) => {
                    for field in fields {
                        self.ty(field.ty);
                    }
                }
            },
            DeclKind::Global { local, .. } => {
                // The names are already in the package scope.
                match &local.binding {
                    Binding::Name { ty, .. } => self.ty(*ty),
                    Binding::Destructure(fields) => {
                        for field in fields {
                            self.check_rename(field);
                            if let Some(ty) = field.ty {
                                self.ty(ty);
                            }
                        }
                    }
                }
                self.expr(local.value);
            }
        }
    }

    fn check_rename(&mut self, field: &DestructField) {
        if let (Some(rename), None) = (field.rename, field.ty) {
            self.error(
                rename.span,
                "a skipped field has no name to rename; drop the `as`",
            );
        }
    }

    // Types.

    fn ty(&mut self, id: TypeId) {
        match &self.ast.ty(id).kind {
            TypeKind::Name { package, name } => {
                let res = match package {
                    Some(package) => match self.lookup(*package) {
                        Some(Res::Package(p)) => self.member(p, *name).map(Res::Decl),
                        Some(_) => {
                            let text = self.name_text(*package);
                            self.error(package.span, format!("`{text}` is not a package"));
                            None
                        }
                        None => {
                            self.undefined(*package);
                            None
                        }
                    },
                    None => match self.lookup(*name) {
                        None => {
                            self.undefined(*name);
                            None
                        }
                        res => res,
                    },
                };
                let is_type = match res {
                    Some(Res::Decl(decl)) => self.is_type_decl(decl),
                    Some(Res::Builtin(b)) => BUILTIN_TYPES.contains(&b),
                    Some(Res::Local(_) | Res::Package(_)) => false,
                    None => true,
                };
                if !is_type {
                    let text = self.name_text(*name);
                    self.error(name.span, format!("`{text}` is not a type"));
                }
                self.out.types[id.index()] = res;
            }
            TypeKind::Tuple(fields) => {
                for field in fields {
                    self.ty(field.ty);
                }
            }
            TypeKind::View(elem)
            | TypeKind::Growable(elem)
            | TypeKind::Pointer(elem)
            | TypeKind::Optional(elem) => self.ty(*elem),
            TypeKind::Array { len, elem } => {
                self.expr(*len);
                self.ty(*elem);
            }
            TypeKind::Result { value, error } => {
                if let Some(value) = value {
                    self.ty(*value);
                }
                self.ty(*error);
            }
            TypeKind::Fn { params, result } => {
                for &(_, ty) in params {
                    self.ty(ty);
                }
                if let Some(result) = result {
                    self.ty(*result);
                }
            }
        }
    }

    fn undefined(&mut self, name: Name) {
        let text = self.name_text(name);
        self.error(name.span, format!("`{text}` is not defined"));
    }

    // Statements.

    fn block(&mut self, block: &'a Block) {
        self.push_scope();
        for &stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.pop_scope();
    }

    /// Resolves a binding's types and declares its names.
    fn binding(&mut self, binding: &'a Binding) {
        match binding {
            Binding::Name { name, ty } => {
                self.ty(*ty);
                if let Some(name) = name {
                    self.bind(*name);
                }
            }
            Binding::Destructure(fields) => self.destructure(fields),
        }
    }

    fn destructure(&mut self, fields: &'a [DestructField]) {
        for field in fields {
            self.check_rename(field);
            if let Some(ty) = field.ty {
                self.ty(ty);
                self.bind_field(field.rename.unwrap_or(field.field), Some(field.field));
            }
        }
    }

    fn stmt(&mut self, id: StmtId) {
        match &self.ast.stmt(id).kind {
            StmtKind::Local(local) => {
                // The value cannot see the names it initializes.
                self.expr(local.value);
                self.binding(&local.binding);
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    self.expr(*value);
                }
            }
            StmtKind::If(if_stmt) => {
                self.expr(if_stmt.cond);
                self.block(&if_stmt.then);
                match &if_stmt.otherwise {
                    Some(Else::Block(block)) => self.block(block),
                    Some(Else::If(stmt)) => self.stmt(*stmt),
                    None => {}
                }
            }
            StmtKind::Match { value, arms } => {
                self.expr(*value);
                for arm in arms {
                    self.push_scope();
                    self.patterns(&arm.patterns);
                    self.block(&arm.body);
                    self.pop_scope();
                }
            }
            StmtKind::For(for_loop) => {
                self.push_scope();
                match &for_loop.kind {
                    ForKind::Forever => {}
                    ForKind::While(cond) => self.expr(*cond),
                    ForKind::In {
                        binding, iterable, ..
                    } => {
                        match iterable {
                            Iterable::Expr(e) => self.expr(*e),
                            Iterable::Range { start, end } => {
                                self.expr(*start);
                                self.expr(*end);
                            }
                        }
                        self.binding(binding);
                    }
                }
                self.block(&for_loop.body);
                self.pop_scope();
            }
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::Defer(e) | StmtKind::Expr(e) => self.expr(*e),
            StmtKind::Unsafe(block) => self.block(block),
            StmtKind::Lock { globals, body } => {
                for global in globals {
                    let decl = match global.package {
                        Some(package) => {
                            if let Some(Res::Package(p)) = self.lookup(package) {
                                self.member(p, global.name)
                            } else {
                                let text = self.name_text(package);
                                self.error(package.span, format!("`{text}` is not a package"));
                                None
                            }
                        }
                        None => match self.lookup(global.name) {
                            Some(Res::Decl(decl)) => Some(decl),
                            Some(_) => {
                                let text = self.name_text(global.name);
                                self.error(
                                    global.name.span,
                                    format!("`lock` takes a `shared` global; `{text}` is not one"),
                                );
                                None
                            }
                            None => {
                                self.undefined(global.name);
                                None
                            }
                        },
                    };
                    if let Some(decl) = decl {
                        let shared = matches!(
                            self.program.file(decl.file).ast.decls[decl.index].kind,
                            DeclKind::Global { shared: true, .. }
                        );
                        if shared {
                            self.out.locks.insert(global.name.span.start, decl);
                        } else {
                            let text = self.name_text(global.name);
                            self.error(
                                global.name.span,
                                format!("`lock` takes a `shared` global; `{text}` is not one"),
                            );
                        }
                    }
                }
                self.block(body);
            }
            StmtKind::Assign { left, right, .. } => {
                self.expr(*left);
                self.expr(*right);
            }
        }
    }

    fn patterns(&mut self, patterns: &'a [PatId]) {
        let several = patterns.len() > 1;
        for &pat in patterns {
            match &self.ast.pat(pat).kind {
                PatternKind::Variant { payload, .. } => {
                    if let Some(fields) = payload {
                        if several && fields.iter().any(|f| f.ty.is_some()) {
                            self.error(
                                self.ast.pat(pat).span,
                                "an arm with several patterns cannot bind payloads",
                            );
                        }
                        self.destructure(fields);
                    }
                }
                PatternKind::Literal(e) | PatternKind::Negative(e) => self.expr(*e),
                PatternKind::Discard => {}
            }
        }
    }

    // Expressions.

    fn expr(&mut self, id: ExprId) {
        match &self.ast.expr(id).kind {
            ExprKind::Name(name) => {
                let res = self.lookup(*name);
                match res {
                    None => self.undefined(*name),
                    Some(Res::Package(_)) => {
                        let text = self.name_text(*name);
                        self.error(
                            name.span,
                            format!(
                                "`{text}` is a package; use one of its names, as in `{text}.name`"
                            ),
                        );
                    }
                    Some(_) => {}
                }
                self.out.exprs[id.index()] = res;
            }
            ExprKind::Int | ExprKind::Float | ExprKind::Char => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(e) = part {
                        self.expr(*e);
                    }
                }
            }
            ExprKind::Paren(e)
            | ExprKind::Deref(e)
            | ExprKind::Try(e)
            | ExprKind::Unsafe(e)
            | ExprKind::Unary { operand: e, .. } => self.expr(*e),
            ExprKind::Tuple(fields) => {
                for &(_, value) in fields {
                    self.expr(value);
                }
            }
            ExprKind::Call { callee, args } => {
                self.callee(*callee);
                for arg in args {
                    self.expr(arg.value);
                }
            }
            ExprKind::Field { value, name } => {
                if let ExprKind::Name(package_name) = self.ast.expr(*value).kind
                    && let Some(Res::Package(package)) = self.lookup(package_name)
                {
                    self.out.exprs[value.index()] = Some(Res::Package(package));
                    self.out.exprs[id.index()] = self.member(package, *name).map(Res::Decl);
                } else {
                    self.expr(*value);
                }
            }
            ExprKind::Construct { ty, value } => {
                self.expr(*ty);
                self.expr(*value);
            }
            ExprKind::Index { value, index } => {
                self.expr(*value);
                match index {
                    Index::Expr(e) => self.expr(*e),
                    Index::Type(ty) => self.ty(*ty),
                    Index::Slice { start, end } => {
                        for e in start.iter().chain(end) {
                            self.expr(*e);
                        }
                    }
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.expr(*left);
                self.expr(*right);
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(*cond);
                self.expr(*then);
                match otherwise {
                    IfElse::Branch(e) | IfElse::If(e) => self.expr(*e),
                }
            }
            ExprKind::Match { value, arms } => {
                self.expr(*value);
                for arm in arms {
                    self.push_scope();
                    self.patterns(&arm.patterns);
                    self.expr(arm.body);
                    self.pop_scope();
                }
            }
            ExprKind::Array(elems) => {
                for &e in elems {
                    self.expr(e);
                }
            }
        }
    }
}
