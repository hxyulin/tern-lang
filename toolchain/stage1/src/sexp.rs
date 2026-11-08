//! Prints a syntax tree in tree-sitter's S-expression form, with the node
//! names of `tree-sitter/grammar.js`. The tests compare this output with the
//! trees in the tree-sitter corpus.

use crate::ast::{
    Ast, Attribute, BinaryOp, Binding, Block, Decl, DeclKind, DestructField, Else, EnumItem,
    ExprId, ExprKind, Field, IfElse, Import, Index, Iterable, Mode, OpGroup, Param, PatId,
    PatternKind, StmtId, StmtKind, StrPart, TypeDef, TypeId, TypeKind,
};
use crate::source::Span;

pub fn print(ast: &Ast, text: &str) -> String {
    let mut printer = Printer {
        ast,
        text,
        out: String::new(),
        depth: 0,
    };
    printer.open("source_file");
    for import in &ast.imports {
        printer.import(import);
    }
    for decl in &ast.decls {
        printer.decl(decl);
    }
    printer.close();
    printer.out
}

struct Printer<'a> {
    ast: &'a Ast,
    text: &'a str,
    out: String,
    depth: usize,
}

impl Printer<'_> {
    fn open(&mut self, name: &str) {
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        self.out.push('(');
        self.out.push_str(name);
        self.depth += 1;
    }

    fn close(&mut self) {
        self.out.push(')');
        self.depth -= 1;
    }

    fn leaf(&mut self, name: &str) {
        self.open(name);
        self.close();
    }

    fn ident(&mut self) {
        self.leaf("identifier");
    }

    fn mode(&mut self, mode: Mode) {
        if mode != Mode::Let {
            self.leaf("parameter_mode");
        }
    }

    fn import(&mut self, import: &Import) {
        self.open("import_declaration");
        self.open("import_path");
        for _ in &import.path {
            self.ident();
        }
        self.close();
        if import.alias.is_some() {
            self.ident();
        }
        self.close();
    }

    fn attributes(&mut self, attributes: &[Attribute]) {
        for attribute in attributes {
            self.open("attribute");
            self.leaf("attribute_name");
            for &arg in &attribute.args {
                self.expr(arg);
            }
            self.close();
        }
    }

    fn decl(&mut self, decl: &Decl) {
        let name = match &decl.kind {
            DeclKind::Fn(_) => "function_declaration",
            DeclKind::ExternFn { .. } => "extern_declaration",
            DeclKind::ExternVar { .. } => "extern_variable",
            DeclKind::Type { .. } => "type_declaration",
            DeclKind::Global { .. } => "global_declaration",
        };
        self.open(name);
        self.attributes(&decl.attributes);
        if decl.public {
            self.leaf("visibility");
        }
        match &decl.kind {
            DeclKind::Fn(f) => {
                if f.is_unsafe {
                    self.leaf("unsafe");
                }
                self.ident();
                self.open("parameter_list");
                self.params(&f.params);
                self.close();
                self.opt_ty(f.result);
                self.block(&f.body);
            }
            DeclKind::ExternFn {
                params,
                variadic,
                result,
                ..
            } => {
                self.ident();
                self.open("extern_parameter_list");
                self.params(params);
                if *variadic {
                    self.leaf("variadic");
                }
                self.close();
                self.opt_ty(*result);
            }
            DeclKind::ExternVar { ty, .. } => {
                self.ident();
                self.ty(*ty);
            }
            DeclKind::Type { def, .. } => {
                self.ident();
                self.type_def(def);
            }
            DeclKind::Global { shared, local } => {
                if *shared {
                    self.leaf("shared");
                }
                self.local(local.mutable, &local.binding, local.value);
            }
        }
        self.close();
    }

    fn params(&mut self, params: &[Param]) {
        for param in params {
            self.open("parameter");
            self.leaf(if param.name.is_some() {
                "identifier"
            } else {
                "discard"
            });
            self.mode(param.mode);
            self.ty(param.ty);
            self.close();
        }
    }

    fn type_def(&mut self, def: &TypeDef) {
        match def {
            TypeDef::Type(ty) => self.ty(*ty),
            TypeDef::Enum { int_type, items } => {
                self.open("enum_type");
                self.opt_ty(*int_type);
                for item in items {
                    match item {
                        EnumItem::Variant { payload, value, .. } => {
                            self.open("variant");
                            self.ident();
                            self.opt_ty(*payload);
                            if let Some(value) = value {
                                self.expr(*value);
                            }
                            self.close();
                        }
                        EnumItem::Open(_) => self.leaf("discard"),
                    }
                }
                self.close();
            }
            TypeDef::Union(fields) => {
                self.open("union_type");
                self.fields(fields);
                self.close();
            }
        }
    }

    fn fields(&mut self, fields: &[Field]) {
        for field in fields {
            self.open("tuple_type_field");
            if field.public {
                self.leaf("visibility");
            }
            self.ident();
            self.ty(field.ty);
            self.close();
        }
    }

    fn opt_ty(&mut self, ty: Option<TypeId>) {
        if let Some(ty) = ty {
            self.ty(ty);
        }
    }

    fn ty(&mut self, id: TypeId) {
        match &self.ast.ty(id).kind {
            TypeKind::Name { package, .. } => {
                self.open("type_name");
                if package.is_some() {
                    self.ident();
                }
                self.ident();
                self.close();
            }
            TypeKind::Tuple(fields) => {
                self.open("tuple_type");
                self.fields(fields);
                self.close();
            }
            TypeKind::View(elem) => self.wrap_ty("view_type", *elem),
            TypeKind::Growable(elem) => self.wrap_ty("growable_type", *elem),
            TypeKind::Pointer(elem) => self.wrap_ty("pointer_type", *elem),
            TypeKind::Optional(elem) => self.wrap_ty("optional_type", *elem),
            TypeKind::Array { len, elem } => {
                self.open("array_type");
                self.expr(*len);
                self.ty(*elem);
                self.close();
            }
            TypeKind::Result { value, error } => {
                self.open("result_type");
                self.opt_ty(*value);
                self.ty(*error);
                self.close();
            }
            TypeKind::Fn { params, result } => {
                self.open("function_type");
                for &(mode, ty) in params {
                    self.open("function_type_parameter");
                    self.mode(mode);
                    self.ty(ty);
                    self.close();
                }
                self.opt_ty(*result);
                self.close();
            }
        }
    }

    fn wrap_ty(&mut self, name: &str, inner: TypeId) {
        self.open(name);
        self.ty(inner);
        self.close();
    }

    fn local(&mut self, mutable: bool, binding: &Binding, value: ExprId) {
        self.open(if mutable {
            "var_declaration"
        } else {
            "let_declaration"
        });
        self.binding(binding);
        self.expr(value);
        self.close();
    }

    fn binding(&mut self, binding: &Binding) {
        match binding {
            Binding::Name { name, ty } => {
                self.leaf(if name.is_some() {
                    "identifier"
                } else {
                    "discard"
                });
                self.ty(*ty);
            }
            Binding::Destructure(fields) => self.destructuring(fields),
        }
    }

    fn destructuring(&mut self, fields: &[DestructField]) {
        self.open("destructuring_pattern");
        for field in fields {
            self.open("destructuring_field");
            self.ident();
            if field.rename.is_some() {
                self.ident();
            }
            match field.ty {
                Some(ty) => self.ty(ty),
                None => self.leaf("discard"),
            }
            self.close();
        }
        self.close();
    }

    fn block(&mut self, block: &Block) {
        self.open("block");
        for &stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.close();
    }

    fn stmt(&mut self, id: StmtId) {
        match &self.ast.stmt(id).kind {
            StmtKind::Local(local) => self.local(local.mutable, &local.binding, local.value),
            StmtKind::Return(value) => {
                self.open("return_statement");
                if let Some(value) = value {
                    self.expr(*value);
                }
                self.close();
            }
            StmtKind::If(if_stmt) => {
                self.open("if_statement");
                self.expr(if_stmt.cond);
                self.block(&if_stmt.then);
                match &if_stmt.otherwise {
                    Some(Else::Block(block)) => self.block(block),
                    Some(Else::If(stmt)) => self.stmt(*stmt),
                    None => {}
                }
                self.close();
            }
            StmtKind::Match { value, arms } => {
                self.open("match_statement");
                self.expr(*value);
                for arm in arms {
                    self.open("match_arm");
                    self.patterns(&arm.patterns);
                    self.block(&arm.body);
                    self.close();
                }
                self.close();
            }
            StmtKind::For(for_loop) => {
                self.open("for_statement");
                match &for_loop.kind {
                    crate::ast::ForKind::Forever => {}
                    crate::ast::ForKind::While(cond) => self.expr(*cond),
                    crate::ast::ForKind::In {
                        binding, iterable, ..
                    } => {
                        self.open("for_in_clause");
                        self.binding(binding);
                        match iterable {
                            Iterable::Expr(e) => self.expr(*e),
                            Iterable::Range { start, end } => {
                                self.open("range");
                                self.expr(*start);
                                self.expr(*end);
                                self.close();
                            }
                        }
                        self.close();
                    }
                }
                self.block(&for_loop.body);
                self.close();
            }
            StmtKind::Break => self.leaf("break_statement"),
            StmtKind::Continue => self.leaf("continue_statement"),
            StmtKind::Defer(call) => self.wrap_expr("defer_statement", *call),
            StmtKind::Unsafe(block) => {
                self.open("unsafe_statement");
                self.block(block);
                self.close();
            }
            StmtKind::Lock { globals, body } => {
                self.open("lock_statement");
                for global in globals {
                    self.open("global_name");
                    if global.package.is_some() {
                        self.ident();
                    }
                    self.ident();
                    self.close();
                }
                self.block(body);
                self.close();
            }
            StmtKind::Assign { left, right, .. } => {
                self.open("assignment_statement");
                self.expr(*left);
                self.expr(*right);
                self.close();
            }
            StmtKind::Expr(e) => self.wrap_expr("expression_statement", *e),
        }
    }

    fn patterns(&mut self, patterns: &[PatId]) {
        for &pat in patterns {
            match &self.ast.pat(pat).kind {
                PatternKind::Variant { payload, .. } => {
                    self.open("variant_pattern");
                    self.ident();
                    if let Some(fields) = payload {
                        self.destructuring(fields);
                    }
                    self.close();
                }
                PatternKind::Literal(e) => self.expr(*e),
                PatternKind::Negative(e) => self.wrap_expr("negative_literal", *e),
                PatternKind::Discard => self.leaf("discard"),
            }
        }
    }

    fn wrap_expr(&mut self, name: &str, inner: ExprId) {
        self.open(name);
        self.expr(inner);
        self.close();
    }

    fn branch(&mut self, value: ExprId) {
        self.wrap_expr("branch", value);
    }

    fn expr(&mut self, id: ExprId) {
        match &self.ast.expr(id).kind {
            ExprKind::Name(_) => self.ident(),
            ExprKind::Int => self.leaf("integer_literal"),
            ExprKind::Float => self.leaf("float_literal"),
            ExprKind::Char => self.leaf("char_literal"),
            ExprKind::Str(parts) => {
                self.open("string_literal");
                for part in parts {
                    match part {
                        StrPart::Text(span) => self.string_text(*span),
                        StrPart::Interpolation(e) => self.wrap_expr("interpolation", *e),
                    }
                }
                self.close();
            }
            ExprKind::Paren(inner) => self.wrap_expr("parenthesized_expression", *inner),
            ExprKind::Tuple(fields) => {
                self.open("tuple_literal");
                for &(_, value) in fields {
                    self.open("tuple_field");
                    self.ident();
                    self.expr(value);
                    self.close();
                }
                self.close();
            }
            ExprKind::Call { callee, args } => {
                self.open("call_expression");
                self.expr(*callee);
                self.open("argument_list");
                for arg in args {
                    self.open("argument");
                    self.mode(arg.mode);
                    self.expr(arg.value);
                    self.close();
                }
                self.close();
                self.close();
            }
            ExprKind::Field { value, .. } => {
                self.open("field_expression");
                self.expr(*value);
                self.ident();
                self.close();
            }
            ExprKind::Construct { ty, value } => {
                self.open("construct_expression");
                self.expr(*ty);
                self.expr(*value);
                self.close();
            }
            ExprKind::Index { value, index } => {
                self.open("index_expression");
                self.expr(*value);
                match index {
                    Index::Expr(e) => self.expr(*e),
                    Index::Type(ty) => self.ty(*ty),
                    Index::Slice { start, end } => {
                        self.open("slice");
                        if let Some(start) = start {
                            self.expr(*start);
                        }
                        if let Some(end) = end {
                            self.expr(*end);
                        }
                        self.close();
                    }
                }
                self.close();
            }
            ExprKind::Deref(inner) => self.wrap_expr("dereference_expression", *inner),
            ExprKind::Unary { operand, .. } => self.wrap_expr("unary_expression", *operand),
            ExprKind::Try(operand) => self.wrap_expr("try_expression", *operand),
            ExprKind::Binary { op, left, right } => {
                self.open(binary_name(*op));
                self.expr(*left);
                self.expr(*right);
                self.close();
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.open("if_expression");
                self.expr(*cond);
                self.branch(*then);
                match otherwise {
                    IfElse::Branch(e) => self.branch(*e),
                    IfElse::If(e) => self.expr(*e),
                }
                self.close();
            }
            ExprKind::Match { value, arms } => {
                self.open("match_expression");
                self.expr(*value);
                for arm in arms {
                    self.open("match_expression_arm");
                    self.patterns(&arm.patterns);
                    self.branch(arm.body);
                    self.close();
                }
                self.close();
            }
            ExprKind::Unsafe(body) => {
                self.open("unsafe_expression");
                self.branch(*body);
                self.close();
            }
            ExprKind::Array(elems) => {
                self.open("array_literal");
                for &e in elems {
                    self.expr(e);
                }
                self.close();
            }
        }
    }

    /// Splits string text into tree-sitter's `string_content` and
    /// `escape_sequence` nodes.
    fn string_text(&mut self, span: Span) {
        let bytes = self.text[span.range()].as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                self.leaf("escape_sequence");
                i += if bytes.get(i + 1) == Some(&b'x') {
                    4
                } else {
                    2
                };
            } else {
                self.leaf("string_content");
                while i < bytes.len() && bytes[i] != b'\\' {
                    i += 1;
                }
            }
        }
    }
}

fn binary_name(op: BinaryOp) -> &'static str {
    match op.group() {
        OpGroup::Or => "or_expression",
        OpGroup::And => "and_expression",
        OpGroup::Comparison => "comparison_expression",
        OpGroup::Additive => "additive_expression",
        OpGroup::Multiplicative => "multiplicative_expression",
        OpGroup::BitAnd => "bit_and_expression",
        OpGroup::BitOr => "bit_or_expression",
        OpGroup::BitXor => "bit_xor_expression",
        OpGroup::Shift => "shift_expression",
    }
}
