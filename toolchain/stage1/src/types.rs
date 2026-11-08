//! The checker's types, interned so that equal types have equal ids.

use std::collections::HashMap;

use crate::ast::Mode;
use crate::package::PackageId;
use crate::resolve::DeclRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TyId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
}

impl IntTy {
    pub fn signed(self) -> bool {
        matches!(
            self,
            IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize
        )
    }

    /// Both targets are 64-bit, so `isize` and `usize` have 64 bits.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => 64,
        }
    }

    pub fn min(self) -> i128 {
        if self.signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    pub fn max(self) -> i128 {
        if self.signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::Isize => "isize",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
            IntTy::Usize => "usize",
        }
    }

    pub fn from_name(name: &str) -> Option<IntTy> {
        Some(match name {
            "i8" => IntTy::I8,
            "i16" => IntTy::I16,
            "i32" => IntTy::I32,
            "i64" => IntTy::I64,
            "isize" => IntTy::Isize,
            "u8" => IntTy::U8,
            "u16" => IntTy::U16,
            "u32" => IntTy::U32,
            "u64" => IntTy::U64,
            "usize" => IntTy::Usize,
            _ => return None,
        })
    }
}

/// The built-in named types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltinNamed {
    Allocator,
    Format,
    PanicInfo,
    OutOfMemory,
}

/// A nominal type: a `type` declaration or a built-in record or enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedId {
    Decl(DeclRef),
    Builtin(BuiltinNamed),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FnTy {
    pub params: Vec<(Mode, TyId)>,
    pub result: Option<TyId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Int(IntTy),
    Float(u8),
    Bool,
    Named(NamedId),
    Tuple(Vec<(String, TyId)>),
    View(TyId),
    Array(u64, TyId),
    Growable(TyId),
    Pointer(TyId),
    Optional(TyId),
    /// `T ! E`, or `! E` with no value.
    Result(Option<TyId>, TyId),
    Fn(FnTy),
    /// The type of `panic(...)` and other expressions that never finish.
    Never,
    /// Stands in after an error, so one mistake is reported once.
    Error,
}

pub struct Variant {
    pub name: String,
    /// A tuple type.
    pub payload: Option<TyId>,
    pub value: Option<i128>,
}

pub struct EnumInfo {
    pub variants: Vec<Variant>,
    pub int: Option<IntTy>,
    pub open: bool,
}

pub enum NamedDef {
    /// A record, newtype or any other type given a name.
    Plain {
        underlying: TyId,
        /// For a record: which fields are `pub`.
        pub_fields: Vec<bool>,
    },
    Enum(EnumInfo),
    Union(Vec<(String, TyId)>),
}

pub struct NamedInfo {
    /// For messages: `File`, or `io.File` outside the root package.
    pub display: String,
    /// `None` for built-in types.
    pub package: Option<PackageId>,
    pub def: NamedDef,
}

pub struct Types {
    tys: Vec<Ty>,
    ids: HashMap<Ty, TyId>,
    pub named: HashMap<NamedId, NamedInfo>,
}

impl Default for Types {
    fn default() -> Types {
        Types::new()
    }
}

impl Types {
    pub fn new() -> Types {
        Types {
            tys: Vec::new(),
            ids: HashMap::new(),
            named: HashMap::new(),
        }
    }

    pub fn intern(&mut self, ty: Ty) -> TyId {
        if let Some(&id) = self.ids.get(&ty) {
            return id;
        }
        let id = TyId(u32::try_from(self.tys.len()).expect("fewer than 2^32 types"));
        self.tys.push(ty.clone());
        self.ids.insert(ty, id);
        id
    }

    pub fn get(&self, id: TyId) -> &Ty {
        &self.tys[id.0 as usize]
    }

    pub fn int(&mut self, int: IntTy) -> TyId {
        self.intern(Ty::Int(int))
    }

    pub fn usize(&mut self) -> TyId {
        self.int(IntTy::Usize)
    }

    pub fn bool(&mut self) -> TyId {
        self.intern(Ty::Bool)
    }

    pub fn never(&mut self) -> TyId {
        self.intern(Ty::Never)
    }

    pub fn error(&mut self) -> TyId {
        self.intern(Ty::Error)
    }

    pub fn bytes(&mut self) -> TyId {
        let u8 = self.int(IntTy::U8);
        self.intern(Ty::View(u8))
    }

    pub fn named_ty(&mut self, id: NamedId) -> TyId {
        self.intern(Ty::Named(id))
    }

    pub fn is_error(&self, id: TyId) -> bool {
        matches!(self.get(id), Ty::Error)
    }

    /// The integer type of a built-in integer, if `id` is one.
    pub fn as_int(&self, id: TyId) -> Option<IntTy> {
        match self.get(id) {
            Ty::Int(int) => Some(*int),
            _ => None,
        }
    }

    pub fn display(&self, id: TyId) -> String {
        match self.get(id) {
            Ty::Int(int) => int.name().to_owned(),
            Ty::Float(bits) => format!("f{bits}"),
            Ty::Bool => "bool".to_owned(),
            Ty::Named(named) => self
                .named
                .get(named)
                .map_or_else(|| format!("{named:?}"), |n| n.display.clone()),
            Ty::Tuple(fields) => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(name, ty)| format!("{name}: {}", self.display(*ty)))
                    .collect();
                format!("({})", fields.join(", "))
            }
            Ty::View(elem) => format!("[]{}", self.display(*elem)),
            Ty::Array(len, elem) => format!("[{len}]{}", self.display(*elem)),
            Ty::Growable(elem) => format!("[..]{}", self.display(*elem)),
            Ty::Pointer(elem) => format!("*{}", self.display(*elem)),
            Ty::Optional(elem) => format!("?{}", self.display(*elem)),
            Ty::Result(value, error) => match value {
                Some(value) => format!("{} ! {}", self.display(*value), self.display(*error)),
                None => format!("! {}", self.display(*error)),
            },
            Ty::Fn(f) => {
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|(mode, ty)| {
                        let mode = match mode {
                            Mode::Let => "",
                            Mode::Inout => "inout ",
                        };
                        format!("{mode}{}", self.display(*ty))
                    })
                    .collect();
                let result = f
                    .result
                    .map_or_else(String::new, |r| format!(" -> {}", self.display(r)));
                format!("fn({}){result}", params.join(", "))
            }
            Ty::Never => "never".to_owned(),
            Ty::Error => "{error}".to_owned(),
        }
    }
}
