//! Globals: each initializer is a constant, evaluated here into bytes and
//! the addresses the linker fills in.

use keel_ir::{DataInit, DataItem, DataObject};

use super::Gen;
use super::layout::{EnumKind, type_size};
use crate::ast::{BinaryOp, Binding, DeclKind, ExprId, ExprKind, StrPart, UnaryOp};
use crate::check::Item;
use crate::package::FileId;
use crate::resolve::{DeclRef, Res};
use crate::types::{Ty, TyId};

/// A global's bytes, with the offsets where the linker writes an address.
struct Bytes {
    data: Vec<u8>,
    relocs: Vec<(usize, String)>,
}

impl Gen<'_> {
    pub fn global(&mut self, decl: DeclRef) {
        let DeclKind::Global { local, .. } = self.decl_kind(decl) else {
            return;
        };
        let Binding::Name { ty, .. } = &local.binding else {
            self.unsupported("code generation does not support a destructured global yet");
            return;
        };
        let ty = self.files[decl.file.0 as usize].types[ty.index()].expect("checked");
        let layout = self.layout(ty);
        let size = usize::try_from(layout.size).expect("a global fits in memory");
        let mut bytes = Bytes {
            data: vec![0; size.max(1)],
            relocs: Vec::new(),
        };
        self.constant(decl.file, local.value, ty, &mut bytes, 0);
        let name = self.symbol(decl);
        let items = items(bytes);
        self.add_data(DataObject {
            name,
            init: Some(DataInit {
                constant: !local.mutable,
                align: u32::try_from(layout.align).unwrap_or(8),
                items,
            }),
            ..DataObject::default()
        });
    }

    fn item(&self, file: FileId, e: ExprId) -> Option<Item> {
        self.files[file.0 as usize].exprs[e.index()]
    }

    fn res(&self, file: FileId, e: ExprId) -> Option<Res> {
        self.names.files[file.0 as usize].exprs[e.index()]
    }

    /// Writes the constant `e`, of type `ty`, at `offset`.
    fn constant(&mut self, file: FileId, e: ExprId, ty: TyId, out: &mut Bytes, offset: usize) {
        let ast = &self.program.file(file).ast;
        let ty_core = self.core(ty);
        match &ast.expr(e).kind {
            ExprKind::Paren(inner) => return self.constant(file, *inner, ty, out, offset),
            ExprKind::Float => {
                let text: String = self
                    .text(file, ast.expr(e).span)
                    .chars()
                    .filter(|&c| c != '_')
                    .collect();
                let x: f64 = text.parse().unwrap_or(0.0);
                if self.layout(ty).size == 4 {
                    #[allow(clippy::cast_possible_truncation)]
                    write(out, offset, u128::from((x as f32).to_bits()), 4);
                } else {
                    write(out, offset, u128::from(x.to_bits()), 8);
                }
                return;
            }
            ExprKind::Str(parts) => {
                let text = string_bytes(self, file, parts);
                let symbol = self.string_symbol(&text);
                out.relocs.push((offset, symbol));
                write(out, offset + 8, text.len() as u128, 8);
                return;
            }
            ExprKind::Tuple(fields) => {
                for (name, value) in fields {
                    let name = self.text(file, name.span);
                    let (fty, foff) = self.field(ty_core, name);
                    let foff = usize::try_from(foff).expect("small");
                    self.constant(file, *value, fty, out, offset + foff);
                }
                return;
            }
            ExprKind::Array(elems) => {
                let Ty::Array(_, elem) = self.types.get(ty_core).clone() else {
                    return;
                };
                let size = usize::try_from(self.layout(elem).size).expect("small");
                for (i, value) in elems.iter().enumerate() {
                    self.constant(file, *value, elem, out, offset + i * size);
                }
                return;
            }
            ExprKind::Construct { ty: target, value } => {
                let item = match self.res(file, *target) {
                    Some(Res::Builtin(name @ ("some" | "ok" | "err"))) => Some(Item::Builtin(name)),
                    _ => self.item(file, *target),
                };
                match item {
                    Some(Item::Variant(enum_ty, index)) => {
                        return self.variant_constant(
                            file,
                            enum_ty,
                            index,
                            Some(*value),
                            out,
                            offset,
                        );
                    }
                    Some(Item::Builtin(name)) => {
                        let shape = self.enum_shape(ty).expect("an optional or result");
                        let index = shape
                            .variants
                            .iter()
                            .position(|v| v.name == name)
                            .expect("a variant");
                        return self.variant_constant(file, ty, index, Some(*value), out, offset);
                    }
                    _ => {}
                }
                if self.record_fields(ty_core).is_some() && self.union_members(ty_core).is_none() {
                    return self.constant(file, *value, ty_core, out, offset);
                }
                // A conversion or a newtype: one value in parentheses.
                let inner = match &ast.expr(*value).kind {
                    ExprKind::Paren(inner) => *inner,
                    _ => *value,
                };
                return self.constant(file, inner, ty, out, offset);
            }
            ExprKind::Call { callee, args } => {
                let name = match &ast.expr(*callee).kind {
                    ExprKind::Index { value, .. } => self.res(file, *value),
                    _ => self.res(file, *callee),
                };
                match name {
                    Some(Res::Builtin("fill")) => {
                        if let Ty::Array(n, elem) = self.types.get(ty_core).clone() {
                            let size = usize::try_from(self.layout(elem).size).expect("small");
                            for i in 0..usize::try_from(n).expect("small") {
                                self.constant(file, args[0].value, elem, out, offset + i * size);
                            }
                        }
                        return;
                    }
                    Some(Res::Builtin("c_string")) => {
                        if let ExprKind::Str(parts) = &ast.expr(args[0].value).kind {
                            let text = string_bytes(self, file, parts);
                            let symbol = self.string_symbol(&text);
                            out.relocs.push((offset, symbol));
                        }
                        return;
                    }
                    _ => {}
                }
            }
            ExprKind::Name(_) | ExprKind::Field { .. } => {
                match self.res(file, e) {
                    Some(Res::Decl(decl)) => match self.decl_kind(decl) {
                        DeclKind::Fn(_) | DeclKind::ExternFn { .. } => {
                            self.declare_for_data(decl);
                            let symbol = self.symbol(decl);
                            out.relocs.push((offset, symbol));
                            return;
                        }
                        DeclKind::Global { local, .. } => {
                            return self.constant(decl.file, local.value, ty, out, offset);
                        }
                        _ => {}
                    },
                    Some(Res::Builtin("none")) => return,
                    _ => {}
                }
                if let Some(Item::Value(vty)) = self.item(file, e)
                    && let ExprKind::Field { name, .. } = &ast.expr(e).kind
                    && self.enum_shape(vty).is_some()
                    && self.text(file, name.span) != "len"
                {
                    let name = self.text(file, name.span);
                    let shape = self.enum_shape(vty).expect("checked");
                    let index = shape
                        .variants
                        .iter()
                        .position(|v| v.name == name)
                        .expect("a variant");
                    return self.variant_constant(file, vty, index, None, out, offset);
                }
            }
            _ => {}
        }
        let size = usize::try_from(self.layout(ty).size).expect("small");
        match self.const_int(file, e, 0) {
            Some(n) => {
                #[allow(clippy::cast_sign_loss)]
                write(out, offset, n as u128, size);
            }
            None => self.unsupported(format!(
                "code generation cannot evaluate the constant `{}` yet",
                self.text(file, ast.expr(e).span)
            )),
        }
    }

    fn variant_constant(
        &mut self,
        file: FileId,
        ty: TyId,
        index: usize,
        payload: Option<ExprId>,
        out: &mut Bytes,
        offset: usize,
    ) {
        let shape = self.enum_shape(ty).expect("an enum");
        let variant = shape.variants[index].clone();
        match shape.kind {
            EnumKind::Scalar(tag) => {
                #[allow(clippy::cast_sign_loss)]
                write(
                    out,
                    offset,
                    variant.tag as u128,
                    usize::try_from(type_size(tag)).expect("small"),
                );
            }
            EnumKind::NullablePointer => {
                if let (Some(value), Some(p)) = (payload, variant.payload) {
                    // some.(value: f): the pointer itself.
                    let (vty, _) = self.field(p, "value");
                    let inner = self.tuple_field_expr(file, value, "value");
                    self.constant(file, inner, vty, out, offset);
                }
            }
            EnumKind::Tagged {
                tag,
                payload_offset,
            } => {
                #[allow(clippy::cast_sign_loss)]
                write(
                    out,
                    offset,
                    variant.tag as u128,
                    usize::try_from(type_size(tag)).expect("small"),
                );
                if let (Some(value), Some(p)) = (payload, variant.payload) {
                    let off = usize::try_from(payload_offset).expect("small");
                    self.constant(file, value, p, out, offset + off);
                }
            }
        }
    }

    /// The expression for one field of a tuple literal.
    fn tuple_field_expr(&self, file: FileId, e: ExprId, field: &str) -> ExprId {
        let ast = &self.program.file(file).ast;
        match &ast.expr(e).kind {
            ExprKind::Tuple(fields) => fields
                .iter()
                .find(|(n, _)| self.text(file, n.span) == field)
                .map_or(e, |(_, v)| *v),
            ExprKind::Paren(inner) => self.tuple_field_expr(file, *inner, field),
            _ => e,
        }
    }

    /// A function a global points to must be declared in the module.
    fn declare_for_data(&mut self, decl: DeclRef) {
        let name = self.symbol(decl);
        let sig = self.decl_abi(decl).sig;
        self.reference(&name, &sig);
    }

    /// The value of an integer constant, before it is cut to its type.
    pub fn const_int(&mut self, file: FileId, e: ExprId, depth: u32) -> Option<i128> {
        if depth > 64 {
            return None;
        }
        let ast = &self.program.file(file).ast;
        let expr = ast.expr(e);
        match &expr.kind {
            ExprKind::Int => crate::check::parse_int_literal(self.text(file, expr.span)),
            ExprKind::Char => crate::check::char_literal(self.text(file, expr.span)),
            ExprKind::Paren(inner) => self.const_int(file, *inner, depth),
            ExprKind::Unary { op, operand } => {
                let n = self.const_int(file, *operand, depth)?;
                match op {
                    UnaryOp::Neg => Some(-n),
                    UnaryOp::BitNot => Some(!n),
                    UnaryOp::Not => Some(i128::from(n == 0)),
                    UnaryOp::AddressOf => None,
                }
            }
            ExprKind::Binary { op, left, right } => {
                let a = self.const_int(file, *left, depth)?;
                let b = self.const_int(file, *right, depth)?;
                Some(match op {
                    BinaryOp::Add => a.checked_add(b)?,
                    BinaryOp::Sub => a.checked_sub(b)?,
                    BinaryOp::Mul => a.checked_mul(b)?,
                    BinaryOp::Div => a.checked_div(b)?,
                    BinaryOp::Rem => a.checked_rem(b)?,
                    BinaryOp::Shl => a.checked_shl(u32::try_from(b).ok()?)?,
                    BinaryOp::Shr => a.checked_shr(u32::try_from(b).ok()?)?,
                    BinaryOp::BitAnd => a & b,
                    BinaryOp::BitOr => a | b,
                    BinaryOp::BitXor => a ^ b,
                    BinaryOp::And => i128::from(a != 0 && b != 0),
                    BinaryOp::Or => i128::from(a != 0 || b != 0),
                    BinaryOp::Eq => i128::from(a == b),
                    BinaryOp::Ne => i128::from(a != b),
                    BinaryOp::Lt => i128::from(a < b),
                    BinaryOp::Le => i128::from(a <= b),
                    BinaryOp::Gt => i128::from(a > b),
                    BinaryOp::Ge => i128::from(a >= b),
                })
            }
            ExprKind::Construct { value, .. } => self.const_int(file, *value, depth),
            ExprKind::Call { callee, args } => {
                let name = match &ast.expr(*callee).kind {
                    ExprKind::Index { value, .. } => self.res(file, *value),
                    _ => self.res(file, *callee),
                };
                match name {
                    Some(Res::Builtin("truncate" | "from_address")) => {
                        self.const_int(file, args[0].value, depth)
                    }
                    _ => None,
                }
            }
            ExprKind::Name(_) | ExprKind::Field { .. } => match self.res(file, e) {
                Some(Res::Builtin("true")) => Some(1),
                Some(Res::Builtin("false")) => Some(0),
                Some(Res::Decl(decl)) => match self.decl_kind(decl) {
                    DeclKind::Global { local, .. } if !local.mutable => {
                        self.const_int(decl.file, local.value, depth + 1)
                    }
                    _ => None,
                },
                _ => {
                    let ExprKind::Field { value, name } = &expr.kind else {
                        return None;
                    };
                    let name = self.text(file, name.span);
                    let (Item::Value(base) | Item::Type(base)) = self.item(file, *value)? else {
                        return None;
                    };
                    if name == "len" {
                        if let Ty::Array(n, _) = self.types.get(self.core(base)) {
                            return Some(i128::from(*n));
                        }
                        if let ExprKind::Str(parts) = &ast.expr(*value).kind {
                            return Some(string_bytes(self, file, parts).len() as i128);
                        }
                    }
                    let shape = self.enum_shape(base)?;
                    let variant = shape.variants.iter().find(|v| v.name == name)?;
                    if matches!(shape.kind, EnumKind::Scalar(_)) {
                        Some(i128::from(variant.tag))
                    } else {
                        None
                    }
                }
            },
            _ => None,
        }
    }
}

/// The bytes of a string literal without interpolation, escapes decoded.
pub fn string_bytes(g: &Gen, file: FileId, parts: &[StrPart]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        if let StrPart::Text(span) = part {
            unescape(g.text(file, *span), &mut out);
        }
    }
    out
}

/// Decodes `\n \t \r \0 \\ \" \' \{ \xNN`.
pub fn unescape(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let c = bytes[i + 1];
            i += 2;
            out.push(match c {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'0' => 0,
                b'x' if i + 1 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i..i + 2]).unwrap_or("0");
                    i += 2;
                    u8::from_str_radix(hex, 16).unwrap_or(0)
                }
                other => other,
            });
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
}

fn write(out: &mut Bytes, offset: usize, value: u128, size: usize) {
    for i in 0..size.min(16) {
        if let Some(b) = out.data.get_mut(offset + i) {
            *b = ((value >> (8 * i)) & 0xFF) as u8;
        }
    }
}

/// Splits bytes into keel data items at each address.
fn items(mut bytes: Bytes) -> Vec<DataItem> {
    bytes.relocs.sort_by_key(|(o, _)| *o);
    let mut items = Vec::new();
    let mut at = 0;
    for (offset, symbol) in bytes.relocs {
        if offset > at {
            items.push(DataItem::Bytes(bytes.data[at..offset].to_vec()));
        }
        items.push(DataItem::Ptr { symbol, addend: 0 });
        at = offset + 8;
    }
    if at < bytes.data.len() {
        items.push(DataItem::Bytes(bytes.data[at..].to_vec()));
    }
    items
}
