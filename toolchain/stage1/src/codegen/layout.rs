//! The layout of every type, following "A specified layout for every
//! type" in DESIGN.md: C's rules for records, a tag and then a C union for
//! enums with payloads, and a nullable pointer for `?*T`.

use keel_ir::{Layout, Type};

use super::Gen;
use crate::types::{BuiltinNamed, IntTy, NamedDef, NamedId, Ty, TyId};

/// How a value of a type is held while code runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repr {
    /// No bytes: `never`, an empty record.
    Unit,
    /// One keel value.
    Scalar(Type),
    /// Bytes in memory, reached through their address.
    Memory,
}

#[derive(Clone, Copy, Debug)]
pub struct TyLayout {
    pub size: u64,
    pub align: u64,
    pub repr: Repr,
}

/// An enum, optional or result, as its variants and where they live.
#[derive(Clone, Debug)]
pub struct EnumShape {
    pub variants: Vec<VariantShape>,
    pub kind: EnumKind,
}

#[derive(Clone, Debug)]
pub struct VariantShape {
    pub name: String,
    /// A record type.
    pub payload: Option<TyId>,
    pub tag: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnumKind {
    /// No payloads: the value is the tag.
    Scalar(Type),
    /// A tag of this type, then the payloads at `payload_offset`.
    Tagged { tag: Type, payload_offset: u64 },
    /// `?*T` and an optional function: null is `none`.
    NullablePointer,
}

/// A variant's name, payload record and declared value.
type NamedVariant = (String, Option<TyId>, Option<i128>);

pub fn int_type(bits: u32) -> Type {
    match bits {
        8 => Type::I8,
        16 => Type::I16,
        32 => Type::I32,
        _ => Type::I64,
    }
}

fn scalar(ty: Type, size: u64) -> TyLayout {
    TyLayout {
        size,
        align: size,
        repr: Repr::Scalar(ty),
    }
}

pub fn type_size(ty: Type) -> u64 {
    match ty {
        Type::I8 => 1,
        Type::I16 => 2,
        Type::I32 | Type::F32 => 4,
        _ => 8,
    }
}

/// Offsets for fields of the given layouts, by C's rules, and the record's
/// size and alignment.
pub fn place_fields(fields: &[TyLayout]) -> (Vec<u64>, u64, u64) {
    let mut offset: u64 = 0;
    let mut align: u64 = 1;
    let mut offsets = Vec::with_capacity(fields.len());
    for field in fields {
        offset = offset.next_multiple_of(field.align);
        offsets.push(offset);
        offset += field.size;
        align = align.max(field.align);
    }
    (offsets, offset.next_multiple_of(align), align)
}

impl Gen<'_> {
    pub fn layout(&mut self, ty: TyId) -> TyLayout {
        if let Some(&layout) = self.layouts.get(&ty) {
            return layout;
        }
        let layout = self.compute_layout(ty);
        self.layouts.insert(ty, layout);
        layout
    }

    pub fn repr(&mut self, ty: TyId) -> Repr {
        self.layout(ty).repr
    }

    fn compute_layout(&mut self, ty: TyId) -> TyLayout {
        match self.types.get(ty).clone() {
            Ty::Int(int) => scalar(int_type(int.bits()), u64::from(int.bits() / 8)),
            Ty::Float(32) => scalar(Type::F32, 4),
            Ty::Float(_) => scalar(Type::F64, 8),
            Ty::Bool => scalar(Type::I8, 1),
            Ty::Pointer(_) | Ty::Fn(_) => scalar(Type::Ptr, 8),
            Ty::Never | Ty::Error => TyLayout {
                size: 0,
                align: 1,
                repr: Repr::Unit,
            },
            Ty::Array(n, elem) => {
                let elem = self.layout(elem);
                TyLayout {
                    size: elem.size * n,
                    align: elem.align,
                    repr: Repr::Memory,
                }
            }
            Ty::Named(NamedId::Decl(_) | NamedId::Builtin(_))
                if self.newtype_underlying(ty).is_some() =>
            {
                let underlying = self.newtype_underlying(ty).expect("checked");
                self.layout(underlying)
            }
            Ty::Named(_) | Ty::Optional(_) | Ty::Result(..) if self.enum_shape(ty).is_some() => {
                let shape = self.enum_shape(ty).expect("checked");
                match shape.kind {
                    EnumKind::Scalar(tag) => scalar(tag, type_size(tag)),
                    EnumKind::NullablePointer => scalar(Type::Ptr, 8),
                    EnumKind::Tagged {
                        tag,
                        payload_offset,
                    } => {
                        let mut size = 0;
                        let mut align = type_size(tag);
                        for variant in &shape.variants {
                            if let Some(p) = variant.payload {
                                let l = self.layout(p);
                                size = size.max(l.size);
                                align = align.max(l.align);
                            }
                        }
                        TyLayout {
                            size: (payload_offset + size).next_multiple_of(align),
                            align,
                            repr: Repr::Memory,
                        }
                    }
                }
            }
            Ty::Named(_) => match self.union_members(ty) {
                Some(members) => {
                    let mut size = 0;
                    let mut align = 1;
                    for (_, member) in members {
                        let l = self.layout(member);
                        size = size.max(l.size);
                        align = align.max(l.align);
                    }
                    TyLayout {
                        size: size.next_multiple_of(align),
                        align,
                        repr: Repr::Memory,
                    }
                }
                None => self.record_layout(ty),
            },
            Ty::Tuple(_) | Ty::View(_) | Ty::Growable(_) => self.record_layout(ty),
            Ty::Optional(_) | Ty::Result(..) => unreachable!("handled with enums"),
        }
    }

    fn record_layout(&mut self, ty: TyId) -> TyLayout {
        let fields = self.record_fields(ty).unwrap_or_default();
        let layouts: Vec<TyLayout> = fields.iter().map(|(_, t, _)| self.layout(*t)).collect();
        let (_, size, align) = place_fields(&layouts);
        TyLayout {
            size,
            align,
            repr: if size == 0 { Repr::Unit } else { Repr::Memory },
        }
    }

    /// A named type that is not a record, enum or union: the type it
    /// names, which has the same layout.
    pub fn newtype_underlying(&self, ty: TyId) -> Option<TyId> {
        let Ty::Named(id) = self.types.get(ty) else {
            return None;
        };
        match &self.types.named.get(id)?.def {
            NamedDef::Plain { underlying, .. }
                if !matches!(self.types.get(*underlying), Ty::Tuple(_)) =>
            {
                Some(*underlying)
            }
            _ => None,
        }
    }

    /// Strips newtypes.
    pub fn core(&self, mut ty: TyId) -> TyId {
        while let Some(u) = self.newtype_underlying(ty) {
            ty = u;
        }
        ty
    }

    pub fn union_members(&self, ty: TyId) -> Option<Vec<(String, TyId)>> {
        let Ty::Named(id) = self.types.get(ty) else {
            return None;
        };
        match &self.types.named.get(id)?.def {
            NamedDef::Union(members) => Some(members.clone()),
            _ => None,
        }
    }

    /// A record's fields with their offsets: records, tuples, views and
    /// growable arrays. A union's members all sit at offset 0.
    pub fn record_fields(&mut self, ty: TyId) -> Option<Vec<(String, TyId, u64)>> {
        let fields: Vec<(String, TyId)> = match self.types.get(ty).clone() {
            Ty::Tuple(fields) => fields,
            Ty::View(elem) => {
                let ptr = self.types.intern(Ty::Pointer(elem));
                let usize = self.types.usize();
                vec![("ptr".to_owned(), ptr), ("len".to_owned(), usize)]
            }
            Ty::Growable(elem) => {
                let ptr = self.types.intern(Ty::Pointer(elem));
                let ptr = self.types.intern(Ty::Optional(ptr));
                let usize = self.types.usize();
                let alloc = self
                    .types
                    .named_ty(NamedId::Builtin(BuiltinNamed::Allocator));
                vec![
                    ("ptr".to_owned(), ptr),
                    ("len".to_owned(), usize),
                    ("cap".to_owned(), usize),
                    ("alloc".to_owned(), alloc),
                ]
            }
            Ty::Named(id) => match &self.types.named.get(&id)?.def {
                NamedDef::Plain { underlying, .. } => match self.types.get(*underlying) {
                    Ty::Tuple(fields) => fields.clone(),
                    _ => return None,
                },
                NamedDef::Union(members) => {
                    return Some(members.iter().map(|(n, t)| (n.clone(), *t, 0)).collect());
                }
                NamedDef::Enum(_) => return None,
            },
            _ => return None,
        };
        let layouts: Vec<TyLayout> = fields.iter().map(|(_, t)| self.layout(*t)).collect();
        let (offsets, _, _) = place_fields(&layouts);
        Some(
            fields
                .into_iter()
                .zip(offsets)
                .map(|((n, t), o)| (n, t, o))
                .collect(),
        )
    }

    pub fn field(&mut self, ty: TyId, name: &str) -> (TyId, u64) {
        let fields = self.record_fields(ty).expect("the checker found a record");
        let (_, t, o) = fields
            .into_iter()
            .find(|(n, _, _)| n == name)
            .expect("the checker found the field");
        (t, o)
    }

    pub fn enum_shape(&mut self, ty: TyId) -> Option<EnumShape> {
        let one = |types: &mut crate::types::Types, name: &str, t: TyId| {
            types.intern(Ty::Tuple(vec![(name.to_owned(), t)]))
        };
        let (variants, int): (Vec<NamedVariant>, Option<IntTy>) = match self.types.get(ty).clone() {
            Ty::Optional(value) => {
                if matches!(self.types.get(self.core(value)), Ty::Pointer(_) | Ty::Fn(_)) {
                    let payload = one(&mut self.types, "value", value);
                    return Some(EnumShape {
                        variants: vec![
                            VariantShape {
                                name: "none".to_owned(),
                                payload: None,
                                tag: 0,
                            },
                            VariantShape {
                                name: "some".to_owned(),
                                payload: Some(payload),
                                tag: 1,
                            },
                        ],
                        kind: EnumKind::NullablePointer,
                    });
                }
                let payload = one(&mut self.types, "value", value);
                (
                    vec![
                        ("none".to_owned(), None, None),
                        ("some".to_owned(), Some(payload), None),
                    ],
                    None,
                )
            }
            Ty::Result(value, error) => {
                let ok = value.map(|v| one(&mut self.types, "value", v));
                let err = one(&mut self.types, "error", error);
                (
                    vec![
                        ("ok".to_owned(), ok, None),
                        ("err".to_owned(), Some(err), None),
                    ],
                    None,
                )
            }
            Ty::Named(id) => match &self.types.named.get(&id)?.def {
                NamedDef::Enum(info) => (
                    info.variants
                        .iter()
                        .map(|v| (v.name.clone(), v.payload, v.value))
                        .collect(),
                    info.int,
                ),
                _ => return None,
            },
            _ => return None,
        };
        let mut next: i128 = 0;
        let shapes: Vec<VariantShape> = variants
            .into_iter()
            .map(|(name, payload, value)| {
                let tag = value.unwrap_or(next);
                next = tag + 1;
                VariantShape {
                    name,
                    payload,
                    tag: i64::try_from(tag).unwrap_or(0),
                }
            })
            .collect();
        let tag = match int {
            Some(int) => int_type(int.bits()),
            None => match shapes.len() {
                0..=256 => Type::I8,
                257..=65536 => Type::I16,
                _ => Type::I32,
            },
        };
        let mut payload_align = 1;
        let mut has_payload = false;
        for v in &shapes {
            if let Some(p) = v.payload {
                let l = self.layout(p);
                if l.size > 0 {
                    has_payload = true;
                }
                payload_align = payload_align.max(l.align);
            }
        }
        let kind = if has_payload {
            EnumKind::Tagged {
                tag,
                payload_offset: type_size(tag).next_multiple_of(payload_align),
            }
        } else {
            EnumKind::Scalar(tag)
        };
        Some(EnumShape {
            variants: shapes,
            kind,
        })
    }

    pub fn variant(&mut self, ty: TyId, name: &str) -> (VariantShape, EnumKind) {
        let shape = self.enum_shape(ty).expect("the checker found an enum");
        let variant = shape
            .variants
            .into_iter()
            .find(|v| v.name == name)
            .expect("the checker found the variant");
        (variant, shape.kind)
    }

    /// The layout keel's calling convention reads, for a type passed in
    /// memory.
    pub fn keel_layout(&mut self, ty: TyId) -> Layout {
        let l = self.layout(ty);
        if let Repr::Scalar(t) = l.repr {
            return Layout::Scalar(t);
        }
        if let Some(fields) = self.record_fields(ty)
            && self.union_members(ty).is_none()
        {
            return Layout::Struct(
                fields
                    .iter()
                    .map(|(_, t, _)| self.keel_layout(*t))
                    .collect(),
            );
        }
        match self.types.get(ty).clone() {
            Ty::Array(n, elem) => Layout::Array(
                Box::new(self.keel_layout(elem)),
                u32::try_from(n).unwrap_or(u32::MAX),
            ),
            _ => {
                if let Some(shape) = self.enum_shape(ty)
                    && let EnumKind::Tagged {
                        tag,
                        payload_offset,
                    } = shape.kind
                {
                    let rest = l.size - payload_offset;
                    return Layout::Struct(vec![Layout::Scalar(tag), Self::blob(rest, l.align)]);
                }
                Self::blob(l.size, l.align)
            }
        }
    }

    /// Bytes of a union: words of its alignment.
    fn blob(size: u64, align: u64) -> Layout {
        let unit = int_type(u32::try_from(align.min(8) * 8).unwrap_or(64));
        let n = size / type_size(unit);
        Layout::Array(
            Box::new(Layout::Scalar(unit)),
            u32::try_from(n).unwrap_or(0),
        )
    }
}
