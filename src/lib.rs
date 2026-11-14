//! vzjson: High-performance JSON library migrated from yyjson 0.13.0 to pure Rust.
//!
//! Single-file, zero-dependency implementation featuring:
//! - 16-byte tagged `Val` immutable DOM (`Doc`, `ValRef`)
//! - Arena-backed circular doubly-linked mutable DOM (`MutDoc`, `MutValId`)
//! - Fast RFC 8259 & JSON5-capable JSON reader with exact IEEE-754 float parsing
//! - Incremental JSON reader (`IncrReader`)
//! - Fast JSON writer with pretty-printing, Unicode escaping, and FP formatting
//! - JSON Pointer (RFC 6901), JSON Patch (RFC 6902), and JSON Merge-Patch (RFC 7386)

#![forbid(unsafe_code)]
#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::io::Write;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use std::path::Path;

// ============================================================================
// Version
// ============================================================================

pub const VZJSON_VERSION_MAJOR: u32 = 1;
pub const VZJSON_VERSION_MINOR: u32 = 0;
pub const VZJSON_VERSION_PATCH: u32 = 1;
pub const VZJSON_VERSION_HEX: u32 = 0x010001;
pub const VZJSON_VERSION_STRING: &str = "1.0.1";

pub const fn vzjson_version() -> u32 {
    VZJSON_VERSION_HEX
}

// ============================================================================
// JSON Types & 16-Byte Tagged Value Representation
// ============================================================================

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    None = 0,
    Raw = 1,
    Null = 2,
    Bool = 3,
    Num = 4,
    Str = 5,
    Arr = 6,
    Obj = 7,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Subtype {
    None = 0 << 3,
    True = 1 << 3,
    Real = 2 << 3,
}

impl Subtype {
    pub const FALSE: Subtype = Subtype::None;
    pub const UINT: Subtype = Subtype::None;
    pub const SINT: Subtype = Subtype::True;
    pub const NOESC: Subtype = Subtype::True;
}

pub const TYPE_MASK: u8 = 0x07;
pub const TYPE_BIT: u8 = 3;
pub const SUBTYPE_MASK: u8 = 0x18;
pub const SUBTYPE_BIT: u8 = 2;
pub const RESERVED_MASK: u8 = 0xE0;
pub const RESERVED_BIT: u8 = 3;
pub const TAG_MASK: u8 = 0xFF;
pub const TAG_BIT: u8 = 8;
pub const PADDING_SIZE: usize = 4;

const TAG_NONE: u64 = Type::None as u64;
const TAG_RAW: u64 = Type::Raw as u64;
const TAG_NULL: u64 = Type::Null as u64;
const TAG_FALSE: u64 = (Type::Bool as u64) | (Subtype::FALSE as u64);
const TAG_TRUE: u64 = (Type::Bool as u64) | (Subtype::True as u64);
const TAG_UINT: u64 = (Type::Num as u64) | (Subtype::UINT as u64);
const TAG_SINT: u64 = (Type::Num as u64) | (Subtype::SINT as u64);
const TAG_REAL: u64 = (Type::Num as u64) | (Subtype::Real as u64);
const TAG_STR: u64 = Type::Str as u64;
const TAG_STR_NOESC: u64 = (Type::Str as u64) | (Subtype::NOESC as u64);
const TAG_ARR: u64 = Type::Arr as u64;
const TAG_OBJ: u64 = Type::Obj as u64;

#[inline(always)]
fn make_tag(ty: u64, len: usize) -> u64 {
    ty | ((len as u64) << TAG_BIT)
}

#[inline(always)]
fn tag_type(tag: u64) -> Type {
    match (tag as u8) & TYPE_MASK {
        0 => Type::None,
        1 => Type::Raw,
        2 => Type::Null,
        3 => Type::Bool,
        4 => Type::Num,
        5 => Type::Str,
        6 => Type::Arr,
        7 => Type::Obj,
        _ => unreachable!(),
    }
}

#[inline(always)]
fn tag_subtype_bits(tag: u64) -> u8 {
    (tag as u8) & SUBTYPE_MASK
}

#[inline(always)]
fn tag_len(tag: u64) -> usize {
    (tag >> TAG_BIT) as usize
}

/// 16-byte immutable JSON value node, matching `yyjson_val` layout.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Val {
    pub tag: u64,
    pub uni: u64,
}

impl Val {
    #[inline(always)]
    pub const fn empty() -> Self {
        Self { tag: 0, uni: 0 }
    }

    #[inline(always)]
    pub fn get_type(&self) -> Type {
        tag_type(self.tag)
    }

    #[inline(always)]
    pub fn is_uint(&self) -> bool {
        (self.tag as u8) == TAG_UINT as u8
    }

    #[inline(always)]
    pub fn is_sint(&self) -> bool {
        (self.tag as u8) == TAG_SINT as u8
    }

    #[inline(always)]
    pub fn is_real(&self) -> bool {
        (self.tag as u8) == TAG_REAL as u8
    }

    #[inline(always)]
    pub fn as_u64(&self) -> Option<u64> {
        if self.is_uint() {
            Some(self.uni)
        } else if self.is_sint() {
            let v = self.uni as i64;
            if v >= 0 { Some(v as u64) } else { None }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_i64(&self) -> Option<i64> {
        if self.is_sint() {
            Some(self.uni as i64)
        } else if self.is_uint() {
            if self.uni <= i64::MAX as u64 { Some(self.uni as i64) } else { None }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_f64(&self) -> Option<f64> {
        if self.is_real() {
            Some(f64::from_bits(self.uni))
        } else {
            None
        }
    }
}

// ============================================================================
// Reader & Writer Flags
// ============================================================================

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadFlags(pub u32);

impl ReadFlags {
    pub const NOFLAG: Self = Self(0);
    pub const INSITU: Self = Self(1 << 0);
    pub const STOP_WHEN_DONE: Self = Self(1 << 1);
    pub const ALLOW_TRAILING_COMMAS: Self = Self(1 << 2);
    pub const ALLOW_COMMENTS: Self = Self(1 << 3);
    pub const ALLOW_INF_AND_NAN: Self = Self(1 << 4);
    pub const NUMBER_AS_RAW: Self = Self(1 << 5);
    pub const ALLOW_INVALID_UNICODE: Self = Self(1 << 6);
    pub const BIGNUM_AS_RAW: Self = Self(1 << 7);
    pub const ALLOW_BOM: Self = Self(1 << 8);
    pub const ALLOW_EXT_NUMBER: Self = Self(1 << 9);
    pub const ALLOW_EXT_ESCAPE: Self = Self(1 << 10);
    pub const ALLOW_EXT_WHITESPACE: Self = Self(1 << 11);
    pub const ALLOW_SINGLE_QUOTED_STR: Self = Self(1 << 12);
    pub const ALLOW_UNQUOTED_KEY: Self = Self(1 << 13);
    pub const JSON5: Self = Self(
        (1 << 2)
            | (1 << 3)
            | (1 << 4)
            | (1 << 9)
            | (1 << 10)
            | (1 << 11)
            | (1 << 12)
            | (1 << 13),
    );

    #[inline(always)]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline(always)]
    pub const fn any(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

impl BitOr for ReadFlags {
    type Output = Self;
    #[inline(always)]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for ReadFlags {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl BitAnd for ReadFlags {
    type Output = Self;
    #[inline(always)]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl BitAndAssign for ReadFlags {
    #[inline(always)]
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}
impl Not for ReadFlags {
    type Output = Self;
    #[inline(always)]
    fn not(self) -> Self {
        Self(!self.0)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteFlags(pub u32);

impl WriteFlags {
    pub const NOFLAG: Self = Self(0);
    pub const PRETTY: Self = Self(1 << 0);
    pub const ESCAPE_UNICODE: Self = Self(1 << 1);
    pub const ESCAPE_SLASHES: Self = Self(1 << 2);
    pub const ALLOW_INF_AND_NAN: Self = Self(1 << 3);
    pub const INF_AND_NAN_AS_NULL: Self = Self(1 << 4);
    pub const ALLOW_INVALID_UNICODE: Self = Self(1 << 5);
    pub const PRETTY_TWO_SPACES: Self = Self(1 << 6);
    pub const NEWLINE_AT_END: Self = Self(1 << 7);
    pub const LOWERCASE_HEX: Self = Self(1 << 8);
    pub const PRETTY_TABS: Self = Self(1 << 9);
    pub const TRAILING_COMMAS: Self = Self(1 << 10);
    pub const FP_TO_FLOAT: Self = Self(1 << (32 - 5));

    #[inline(always)]
    pub const fn fp_to_fixed(prec: u32) -> Self {
        Self((prec & 0x0F) << (32 - 4))
    }

    #[inline(always)]
    pub const fn fp_fixed_prec(self) -> u32 {
        (self.0 >> (32 - 4)) & 0x0F
    }

    #[inline(always)]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline(always)]
    pub const fn any(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }
}

impl BitOr for WriteFlags {
    type Output = Self;
    #[inline(always)]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}
impl BitOrAssign for WriteFlags {
    #[inline(always)]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
impl BitAnd for WriteFlags {
    type Output = Self;
    #[inline(always)]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
impl BitAndAssign for WriteFlags {
    #[inline(always)]
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}
impl Not for WriteFlags {
    type Output = Self;
    #[inline(always)]
    fn not(self) -> Self {
        Self(!self.0)
    }
}

// ============================================================================
// Errors & Text Locating
// ============================================================================

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadErrorCode {
    Success = 0,
    InvalidParameter = 1,
    MemoryAllocation = 2,
    EmptyContent = 3,
    UnexpectedContent = 4,
    UnexpectedEnd = 5,
    UnexpectedCharacter = 6,
    JsonStructure = 7,
    InvalidComment = 8,
    InvalidNumber = 9,
    InvalidString = 10,
    Literal = 11,
    FileOpen = 12,
    FileRead = 13,
    More = 14,
    Depth = 15,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadError {
    pub code: ReadErrorCode,
    pub msg: &'static str,
    pub pos: usize,
}

impl ReadError {
    #[inline]
    pub const fn new(code: ReadErrorCode, msg: &'static str, pos: usize) -> Self {
        Self { code, msg, pos }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.msg, self.pos)
    }
}

impl std::error::Error for ReadError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TextPos {
    pub line: usize,
    pub col: usize,
    pub char_pos: usize,
}

/// Locate line, column, and UTF-8 character index for a byte offset `pos` in `data`.
pub fn locate_pos(data: &[u8], pos: usize) -> Option<TextPos> {
    if pos > data.len() {
        return None;
    }
    let slice = &data[..pos];
    let mut line = 1usize;
    let mut col = 1usize;
    let mut char_pos = 0usize;
    let mut i = 0usize;
    while i < slice.len() {
        let b = slice[i];
        if b < 0x80 {
            if b == b'\n' {
                line += 1;
                col = 1;
                i += 1;
            } else if b == b'\r' {
                line += 1;
                col = 1;
                if i + 1 < slice.len() && slice[i + 1] == b'\n' {
                    i += 2;
                    char_pos += 1;
                    continue;
                }
                i += 1;
            } else {
                col += 1;
                i += 1;
            }
            char_pos += 1;
        } else {
            let step = if b < 0xE0 {
                2
            } else if b < 0xF0 {
                3
            } else {
                4
            };
            i = (i + step).min(slice.len());
            col += 1;
            char_pos += 1;
        }
    }
    Some(TextPos { line, col, char_pos })
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WriteErrorCode {
    Success = 0,
    InvalidParameter = 1,
    MemoryAllocation = 2,
    InvalidValueType = 3,
    NanOrInf = 4,
    FileOpen = 5,
    FileWrite = 6,
    InvalidString = 7,
    Depth = 8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    pub code: WriteErrorCode,
    pub msg: &'static str,
}

impl WriteError {
    #[inline]
    pub const fn new(code: WriteErrorCode, msg: &'static str) -> Self {
        Self { code, msg }
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for WriteError {}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PtrErrorCode {
    None = 0,
    Parameter = 1,
    Syntax = 2,
    Resolve = 3,
    Null = 4,
    MemoryAllocation = 5,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtrError {
    pub code: PtrErrorCode,
    pub msg: &'static str,
    pub pos: usize,
}

impl fmt::Display for PtrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.msg, self.pos)
    }
}

impl std::error::Error for PtrError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchError {
    pub msg: &'static str,
    pub idx: usize,
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at patch op {})", self.msg, self.idx)
    }
}

impl std::error::Error for PatchError {}

// ============================================================================
// Character & UTF-8 / Hex Utilities
// ============================================================================

const CLS_WS: u8 = 1 << 0;
const CLS_DIGIT: u8 = 1 << 1;
const CLS_HEX: u8 = 1 << 2;

const CHAR_CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b' ' as usize] |= CLS_WS;
    t[b'\t' as usize] |= CLS_WS;
    t[b'\n' as usize] |= CLS_WS;
    t[b'\r' as usize] |= CLS_WS;
    let mut c = b'0';
    while c <= b'9' {
        t[c as usize] |= CLS_DIGIT | CLS_HEX;
        c += 1;
    }
    let mut c = b'a';
    while c <= b'f' {
        t[c as usize] |= CLS_HEX;
        c += 1;
    }
    let mut c = b'A';
    while c <= b'F' {
        t[c as usize] |= CLS_HEX;
        c += 1;
    }
    t
};

#[inline(always)]
const fn is_ws_std(c: u8) -> bool {
    (CHAR_CLASS[c as usize] & CLS_WS) != 0
}

#[inline(always)]
const fn is_digit(c: u8) -> bool {
    (CHAR_CLASS[c as usize] & CLS_DIGIT) != 0
}

#[inline(always)]
const fn is_hex(c: u8) -> bool {
    (CHAR_CLASS[c as usize] & CLS_HEX) != 0
}

#[inline(always)]
const fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0xFF,
    }
}

/// Validate UTF-8 sequence starting at `bytes[0]`, returning its byte length (1..=4) or None.
#[inline]
fn utf8_char_len(bytes: &[u8]) -> Option<usize> {
    let b0 = *bytes.first()?;
    if b0 < 0x80 {
        return Some(1);
    }
    if b0 < 0xC2 {
        return None;
    }
    if b0 < 0xE0 {
        if bytes.len() < 2 {
            return None;
        }
        let b1 = bytes[1];
        if (b1 & 0xC0) != 0x80 {
            return None;
        }
        return Some(2);
    }
    if b0 < 0xF0 {
        if bytes.len() < 3 {
            return None;
        }
        let b1 = bytes[1];
        let b2 = bytes[2];
        if (b1 & 0xC0) != 0x80 || (b2 & 0xC0) != 0x80 {
            return None;
        }
        if b0 == 0xE0 && b1 < 0xA0 {
            return None; // overlong
        }
        if b0 == 0xED && b1 >= 0xA0 {
            return None; // UTF-16 surrogate U+D800..U+DFFF
        }
        return Some(3);
    }
    if b0 <= 0xF4 {
        if bytes.len() < 4 {
            return None;
        }
        let b1 = bytes[1];
        let b2 = bytes[2];
        let b3 = bytes[3];
        if (b1 & 0xC0) != 0x80 || (b2 & 0xC0) != 0x80 || (b3 & 0xC0) != 0x80 {
            return None;
        }
        if b0 == 0xF0 && b1 < 0x90 {
            return None; // overlong
        }
        if b0 == 0xF4 && b1 > 0x8F {
            return None; // > U+10FFFF
        }
        return Some(4);
    }
    None
}

#[inline]
fn is_valid_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

#[inline]
fn encode_utf8(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push((0xC0 | (cp >> 6)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else if cp < 0x10000 {
        out.push((0xE0 | (cp >> 12)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else {
        out.push((0xF0 | (cp >> 18)) as u8);
        out.push((0x80 | ((cp >> 12) & 0x3F)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    }
}

// ============================================================================
// Exact Floating-Point Parser (Fast Path + Eisel-Lemire + BigInt Fallback)
// ============================================================================

const F64_POW10_FAST: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11,
    1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Fast exact IEEE-754 f64 parser for decimal numbers.
/// `sig` is the parsed integer significand (up to 19 digits), `exp10` is base-10 exponent,
/// `raw` is the full ASCII number slice (without leading `+`) used for exact fallback if needed.
#[inline]
fn parse_f64_exact(sig: u64, sig_digits: usize, exp10: i32, neg: bool, raw: &[u8]) -> Option<f64> {
    if sig == 0 {
        return Some(if neg { -0.0 } else { 0.0 });
    }
    // Fast path: exact IEEE-754 double when sig <= 2^53 - 1 and |exp10| <= 22
    if sig_digits <= 15 && exp10 >= -22 && exp10 <= 22 && sig <= (1u64 << 53) {
        let mut val = sig as f64;
        if exp10 >= 0 {
            val *= F64_POW10_FAST[exp10 as usize];
        } else {
            val /= F64_POW10_FAST[(-exp10) as usize];
        }
        return Some(if neg { -val } else { val });
    }
    // Exact fallback via Rust std's Eisel-Lemire + BigInt parser (guaranteed exact round-to-nearest-even)
    let s = std::str::from_utf8(raw).ok()?;
    let val: f64 = s.parse().ok()?;
    Some(if neg && !val.is_sign_negative() { -val } else { val })
}

// ============================================================================
// Immutable JSON Document & Value Reference
// ============================================================================

/// An immutable JSON document.
///
/// Stores all JSON values in a contiguous `Vec<Val>` in pre-order (matching yyjson's
/// cache-friendly linear layout) and all strings/raw slices in a contiguous byte buffer.
#[derive(Clone, Debug)]
pub struct Doc {
    vals: Vec<Val>,
    buf: Vec<u8>,
    read_size: usize,
}

impl Doc {
    /// Create an empty `Doc` (useful as a reusable target for `PoolAlc::read_into`).
    pub fn empty() -> Self {
        Self {
            vals: Vec::new(),
            buf: Vec::new(),
            read_size: 0,
        }
    }

    /// Parse JSON bytes with the given `ReadFlags`.
    pub fn read(input: &[u8], flags: ReadFlags) -> Result<Self, ReadError> {
        read_json(input, flags)
    }

    /// Parse JSON bytes in-situ (zero-copy for non-escaped strings, in-place unescaping).
    pub fn read_insitu(data: Vec<u8>, flags: ReadFlags) -> Result<Doc, ReadError> {
        read_json_insitu(data, flags | ReadFlags::INSITU)
    }

    /// Parse a JSON `&str` with the given `ReadFlags`.
    pub fn read_str(input: &str, flags: ReadFlags) -> Result<Self, ReadError> {
        read_json(input.as_bytes(), flags)
    }

    /// Read and parse a JSON file (uses zero-copy in-situ parsing on the file buffer).
    pub fn read_file(path: impl AsRef<Path>, flags: ReadFlags) -> Result<Self, ReadError> {
        let data = fs::read(path.as_ref()).map_err(|e| {
            let code = match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    ReadErrorCode::FileOpen
                }
                _ => ReadErrorCode::FileRead,
            };
            ReadError::new(code, "failed to read file", 0)
        })?;
        read_json_insitu(data, flags | ReadFlags::INSITU)
    }

    /// Returns the root JSON value, if the document is non-empty.
    #[inline]
    pub fn root(&self) -> Option<ValRef<'_>> {
        if self.vals.is_empty() {
            None
        } else {
            Some(ValRef { doc: self, idx: 0 })
        }
    }

    /// Returns the number of bytes consumed from the input.
    #[inline]
    pub fn read_size(&self) -> usize {
        self.read_size
    }

    /// Returns the total number of `Val` nodes in the document.
    #[inline]
    pub fn val_count(&self) -> usize {
        self.vals.len()
    }

    /// Returns the raw `Val` slice.
    #[inline]
    pub fn raw_vals(&self) -> &[Val] {
        &self.vals
    }

    /// Serialize the document to a `String`.
    pub fn write(&self, flags: WriteFlags) -> Result<String, WriteError> {
        let bytes = self.write_bytes(flags)?;
        String::from_utf8(bytes).map_err(|_| {
            WriteError::new(WriteErrorCode::InvalidString, "invalid UTF-8 in output")
        })
    }

    /// Serialize the document to UTF-8 bytes.
    pub fn write_bytes(&self, flags: WriteFlags) -> Result<Vec<u8>, WriteError> {
        let root = self.root().ok_or_else(|| {
            WriteError::new(WriteErrorCode::InvalidParameter, "document has no root value")
        })?;
        write_val_to_vec(&root, flags)
    }

    /// Serialize the document to a file.
    pub fn write_file(&self, path: impl AsRef<Path>, flags: WriteFlags) -> Result<(), WriteError> {
        let bytes = self.write_bytes(flags)?;
        let mut f = fs::File::create(path.as_ref()).map_err(|_| {
            WriteError::new(WriteErrorCode::FileOpen, "failed to open file for writing")
        })?;
        f.write_all(&bytes).map_err(|_| {
            WriteError::new(WriteErrorCode::FileWrite, "failed to write to file")
        })
    }

    /// Convert this immutable `Doc` into a `MutDoc`.
    pub fn to_mut(&self) -> MutDoc {
        let mut mdoc = MutDoc::with_capacity(self.vals.len(), self.buf.len());
        if let Some(root) = self.root() {
            let id = mdoc.copy_from_val(&root);
            mdoc.set_root(id);
        }
        mdoc
    }
}

/// A lightweight 16-byte reference to a value inside an immutable `Doc`.
#[derive(Copy, Clone)]
pub struct ValRef<'a> {
    doc: &'a Doc,
    idx: usize,
}

impl<'a> ValRef<'a> {
    #[inline(always)]
    fn val(&self) -> &Val {
        &self.doc.vals[self.idx]
    }

    #[inline(always)]
    fn str_slice(&self) -> &'a [u8] {
        let v = self.val();
        let ofs = v.uni as usize;
        let len = tag_len(v.tag);
        &self.doc.buf[ofs..ofs + len]
    }

    #[inline]
    pub fn get_type(&self) -> Type {
        self.val().get_type()
    }

    #[inline]
    pub fn get_subtype(&self) -> Subtype {
        match tag_subtype_bits(self.val().tag) {
            0 => Subtype::None,
            8 => Subtype::True,
            16 => Subtype::Real,
            _ => Subtype::None,
        }
    }

    pub fn get_type_desc(&self) -> &'static str {
        match (self.get_type(), tag_subtype_bits(self.val().tag)) {
            (Type::None, _) => "none",
            (Type::Raw, _) => "raw",
            (Type::Null, _) => "null",
            (Type::Bool, 0) => "false",
            (Type::Bool, _) => "true",
            (Type::Num, 0) => "uint",
            (Type::Num, 8) => "sint",
            (Type::Num, _) => "real",
            (Type::Str, _) => "string",
            (Type::Arr, _) => "array",
            (Type::Obj, _) => "object",
        }
    }

    #[inline] pub fn is_null(&self) -> bool { self.get_type() == Type::Null }
    #[inline] pub fn is_true(&self) -> bool { (self.val().tag as u8) == TAG_TRUE as u8 }
    #[inline] pub fn is_false(&self) -> bool { (self.val().tag as u8) == TAG_FALSE as u8 }
    #[inline] pub fn is_bool(&self) -> bool { self.get_type() == Type::Bool }
    #[inline] pub fn is_uint(&self) -> bool { self.val().is_uint() }
    #[inline] pub fn is_sint(&self) -> bool { self.val().is_sint() }
    #[inline] pub fn is_int(&self) -> bool { self.is_uint() || self.is_sint() }
    #[inline] pub fn is_real(&self) -> bool { self.val().is_real() }
    #[inline] pub fn is_num(&self) -> bool { self.get_type() == Type::Num }
    #[inline] pub fn is_str(&self) -> bool { self.get_type() == Type::Str }
    #[inline] pub fn is_arr(&self) -> bool { self.get_type() == Type::Arr }
    #[inline] pub fn is_obj(&self) -> bool { self.get_type() == Type::Obj }
    #[inline] pub fn is_ctn(&self) -> bool { matches!(self.get_type(), Type::Arr | Type::Obj) }
    #[inline] pub fn is_raw(&self) -> bool { self.get_type() == Type::Raw }

    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        if self.is_bool() {
            Some(self.is_true())
        } else {
            None
        }
    }

    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        self.val().as_u64()
    }

    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        self.val().as_i64()
    }

    #[inline]
    pub fn as_f64(&self) -> Option<f64> {
        self.val().as_f64()
    }

    /// Converts any number (`uint`, `sint`, or `real`) to `f64`.
    #[inline]
    pub fn as_num_f64(&self) -> Option<f64> {
        let v = self.val();
        if v.is_real() {
            Some(f64::from_bits(v.uni))
        } else if v.is_uint() {
            Some(v.uni as f64)
        } else if v.is_sint() {
            Some((v.uni as i64) as f64)
        } else {
            None
        }
    }

    #[inline]
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        if self.is_str() {
            Some(self.str_slice())
        } else {
            None
        }
    }

    #[inline]
    pub fn as_str(&self) -> Option<&'a str> {
        if self.is_str() {
            std::str::from_utf8(self.str_slice()).ok()
        } else {
            None
        }
    }

    #[inline]
    pub fn as_raw(&self) -> Option<&'a str> {
        if self.is_raw() {
            std::str::from_utf8(self.str_slice()).ok()
        } else {
            None
        }
    }

    #[inline]
    pub fn as_raw_bytes(&self) -> Option<&'a [u8]> {
        if self.is_raw() {
            Some(self.str_slice())
        } else {
            None
        }
    }

    /// Length of string, raw, array, or object.
    #[inline]
    pub fn len(&self) -> usize {
        tag_len(self.val().tag)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Advance past this value to its next sibling index in `doc.vals`.
    #[inline(always)]
    fn next_sibling_idx(&self) -> usize {
        if self.is_ctn() {
            self.idx + (self.val().uni as usize)
        } else {
            self.idx + 1
        }
    }

    /// Get array element at `index`.
    pub fn get_idx(&self, index: usize) -> Option<ValRef<'a>> {
        if !self.is_arr() || index >= self.len() {
            return None;
        }
        let mut cur = self.idx + 1;
        for _ in 0..index {
            cur = ValRef { doc: self.doc, idx: cur }.next_sibling_idx();
        }
        Some(ValRef { doc: self.doc, idx: cur })
    }

    pub fn first(&self) -> Option<ValRef<'a>> {
        self.get_idx(0)
    }

    pub fn last(&self) -> Option<ValRef<'a>> {
        let len = self.len();
        if !self.is_arr() || len == 0 {
            None
        } else {
            self.get_idx(len - 1)
        }
    }

    /// Get object value by key.
    #[inline]
    pub fn get(&self, key: &str) -> Option<ValRef<'a>> {
        self.get_n(key.as_bytes())
    }

    /// Get object value by byte key.
    pub fn get_n(&self, key: &[u8]) -> Option<ValRef<'a>> {
        if !self.is_obj() {
            return None;
        }
        let mut it = self.iter_obj();
        while let Some((k, v)) = it.next() {
            if k.str_slice() == key {
                return Some(v);
            }
        }
        None
    }

    #[inline]
    pub fn iter_arr(&self) -> ArrIter<'a> {
        if self.is_arr() {
            ArrIter {
                doc: self.doc,
                cur: self.idx + 1,
                rem: self.len(),
            }
        } else {
            ArrIter { doc: self.doc, cur: 0, rem: 0 }
        }
    }

    #[inline]
    pub fn iter_obj(&self) -> ObjIter<'a> {
        if self.is_obj() {
            ObjIter {
                doc: self.doc,
                cur: self.idx + 1,
                rem: self.len(),
            }
        } else {
            ObjIter { doc: self.doc, cur: 0, rem: 0 }
        }
    }

    /// Resolve a JSON Pointer (RFC 6901) against this value.
    pub fn ptr(&self, pointer: &str) -> Result<ValRef<'a>, PtrError> {
        ptr_get_val(*self, pointer)
    }

    /// Deep structural equality (objects compared order-independently, numbers by value).
    pub fn equals(&self, other: &ValRef<'_>) -> bool {
        val_equals(*self, *other)
    }

    pub fn write(&self, flags: WriteFlags) -> Result<String, WriteError> {
        let bytes = write_val_to_vec(self, flags)?;
        String::from_utf8(bytes).map_err(|_| {
            WriteError::new(WriteErrorCode::InvalidString, "invalid UTF-8 in output")
        })
    }

    pub fn write_bytes(&self, flags: WriteFlags) -> Result<Vec<u8>, WriteError> {
        write_val_to_vec(self, flags)
    }
}

impl<'a> fmt::Debug for ValRef<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.write(WriteFlags::NOFLAG) {
            Ok(s) => f.write_str(&s),
            Err(_) => write!(f, "<Val:{}>", self.get_type_desc()),
        }
    }
}

pub struct ArrIter<'a> {
    doc: &'a Doc,
    cur: usize,
    rem: usize,
}

impl<'a> Iterator for ArrIter<'a> {
    type Item = ValRef<'a>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.rem == 0 {
            return None;
        }
        let item = ValRef { doc: self.doc, idx: self.cur };
        self.cur = item.next_sibling_idx();
        self.rem -= 1;
        Some(item)
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.rem, Some(self.rem))
    }
}
impl<'a> ExactSizeIterator for ArrIter<'a> {}

pub struct ObjIter<'a> {
    doc: &'a Doc,
    cur: usize,
    rem: usize,
}

impl<'a> Iterator for ObjIter<'a> {
    type Item = (ValRef<'a>, ValRef<'a>);
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.rem == 0 {
            return None;
        }
        let k = ValRef { doc: self.doc, idx: self.cur };
        let v = ValRef { doc: self.doc, idx: self.cur + 1 };
        self.cur = v.next_sibling_idx();
        self.rem -= 1;
        Some((k, v))
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.rem, Some(self.rem))
    }
}
impl<'a> ExactSizeIterator for ObjIter<'a> {}

fn val_equals(a: ValRef<'_>, b: ValRef<'_>) -> bool {
    let ta = a.get_type();
    if ta != b.get_type() {
        return false;
    }
    match ta {
        Type::None | Type::Null => true,
        Type::Bool => a.is_true() == b.is_true(),
        Type::Raw => a.str_slice() == b.str_slice(),
        Type::Str => a.str_slice() == b.str_slice(),
        Type::Num => {
            if let (Some(u1), Some(u2)) = (a.as_u64(), b.as_u64()) {
                u1 == u2
            } else if let (Some(i1), Some(i2)) = (a.as_i64(), b.as_i64()) {
                i1 == i2
            } else if let (Some(f1), Some(f2)) = (a.as_f64(), b.as_f64()) {
                f1 == f2
            } else {
                false
            }
        }
        Type::Arr => {
            if a.len() != b.len() {
                return false;
            }
            a.iter_arr().zip(b.iter_arr()).all(|(x, y)| val_equals(x, y))
        }
        Type::Obj => {
            let len = a.len();
            if len != b.len() {
                return false;
            }
            if len == 0 {
                return true;
            }
            if len <= 64 {
                let mut used: u64 = 0;
                'outer: for (ak, av) in a.iter_obj() {
                    let ak_s = ak.str_slice();
                    for (bi, (bk, bv)) in b.iter_obj().enumerate() {
                        if (used & (1u64 << bi)) == 0
                            && bk.str_slice() == ak_s
                            && val_equals(av, bv)
                        {
                            used |= 1u64 << bi;
                            continue 'outer;
                        }
                    }
                    return false;
                }
                true
            } else {
                let mut used = vec![false; len];
                'outer_big: for (ak, av) in a.iter_obj() {
                    let ak_s = ak.str_slice();
                    for (bi, (bk, bv)) in b.iter_obj().enumerate() {
                        if !used[bi] && bk.str_slice() == ak_s && val_equals(av, bv) {
                            used[bi] = true;
                            continue 'outer_big;
                        }
                    }
                    return false;
                }
                true
            }
        }
    }
}

// ============================================================================
// Mutable JSON Document & Arena-Based Circular Doubly-Linked List
// ============================================================================

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MutValId(pub u32);

const MUT_NONE: u32 = u32::MAX;

#[derive(Clone, Debug)]
struct MutValNode {
    tag: u64,
    uni: u64,
    /// Circular doubly-linked list next sibling (in parent container).
    next: u32,
    /// Circular doubly-linked list prev sibling.
    prev: u32,
    /// For containers: index of the LAST child (so `last.next` is first child!).
    tail: u32,
}

impl MutValNode {
    #[inline]
    fn new(tag: u64, uni: u64) -> Self {
        Self {
            tag,
            uni,
            next: MUT_NONE,
            prev: MUT_NONE,
            tail: MUT_NONE,
        }
    }
}

/// A mutable JSON document backed by a contiguous arena (`Vec<MutValNode>`)
/// and a string pool (`Vec<u8>`), matching `yyjson_mut_doc` semantics.
#[derive(Clone, Debug, Default)]
pub struct MutDoc {
    nodes: Vec<MutValNode>,
    buf: Vec<u8>,
    root: Option<MutValId>,
}

impl MutDoc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(val_cap: usize, str_cap: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(val_cap),
            buf: Vec::with_capacity(str_cap),
            root: None,
        }
    }

    #[inline]
    pub fn root(&self) -> Option<MutValId> {
        self.root
    }

    #[inline]
    pub fn set_root(&mut self, id: MutValId) {
        self.root = Some(id);
    }

    #[inline]
    fn alloc_node(&mut self, tag: u64, uni: u64) -> MutValId {
        let id = self.nodes.len() as u32;
        self.nodes.push(MutValNode::new(tag, uni));
        MutValId(id)
    }

    #[inline]
    fn alloc_bytes(&mut self, bytes: &[u8]) -> (u64, usize) {
        let ofs = self.buf.len() as u64;
        self.buf.extend_from_slice(bytes);
        (ofs, bytes.len())
    }

    // ---- Value Creation ----

    #[inline] pub fn null(&mut self) -> MutValId { self.alloc_node(TAG_NULL, 0) }
    #[inline] pub fn true_val(&mut self) -> MutValId { self.alloc_node(TAG_TRUE, 0) }
    #[inline] pub fn false_val(&mut self) -> MutValId { self.alloc_node(TAG_FALSE, 0) }
    #[inline] pub fn bool(&mut self, v: bool) -> MutValId { if v { self.true_val() } else { self.false_val() } }
    #[inline] pub fn uint(&mut self, v: u64) -> MutValId { self.alloc_node(TAG_UINT, v) }
    #[inline] pub fn sint(&mut self, v: i64) -> MutValId { self.alloc_node(TAG_SINT, v as u64) }
    #[inline] pub fn real(&mut self, v: f64) -> MutValId { self.alloc_node(TAG_REAL, v.to_bits()) }
    #[inline] pub fn arr(&mut self) -> MutValId { self.alloc_node(TAG_ARR, 0) }
    #[inline] pub fn obj(&mut self) -> MutValId { self.alloc_node(TAG_OBJ, 0) }

    pub fn str(&mut self, s: &str) -> MutValId {
        self.strn(s.as_bytes())
    }

    pub fn strn(&mut self, s: &[u8]) -> MutValId {
        let (ofs, len) = self.alloc_bytes(s);
        self.alloc_node(make_tag(TAG_STR, len), ofs)
    }

    pub fn raw(&mut self, s: &str) -> MutValId {
        self.rawn(s.as_bytes())
    }

    pub fn rawn(&mut self, s: &[u8]) -> MutValId {
        let (ofs, len) = self.alloc_bytes(s);
        self.alloc_node(make_tag(TAG_RAW, len), ofs)
    }

    pub fn arr_with_strs(&mut self, vals: &[&str]) -> MutValId {
        let arr = self.arr();
        for s in vals {
            let v = self.str(s);
            self.arr_append(arr, v);
        }
        arr
    }

    pub fn arr_with_u64(&mut self, vals: &[u64]) -> MutValId {
        let arr = self.arr();
        for &n in vals {
            let v = self.uint(n);
            self.arr_append(arr, v);
        }
        arr
    }

    pub fn arr_with_i64(&mut self, vals: &[i64]) -> MutValId {
        let arr = self.arr();
        for &n in vals {
            let v = self.sint(n);
            self.arr_append(arr, v);
        }
        arr
    }

    pub fn arr_with_f64(&mut self, vals: &[f64]) -> MutValId {
        let arr = self.arr();
        for &n in vals {
            let v = self.real(n);
            self.arr_append(arr, v);
        }
        arr
    }

    pub fn arr_with_bool(&mut self, vals: &[bool]) -> MutValId {
        let arr = self.arr();
        for &b in vals {
            let v = self.bool(b);
            self.arr_append(arr, v);
        }
        arr
    }

    /// Deep-copy an immutable `ValRef` into this `MutDoc`.
    pub fn copy_from_val(&mut self, val: &ValRef<'_>) -> MutValId {
        match val.get_type() {
            Type::None => self.alloc_node(TAG_NONE, 0),
            Type::Null => self.null(),
            Type::Bool => self.bool(val.is_true()),
            Type::Raw => self.rawn(val.str_slice()),
            Type::Str => self.strn(val.str_slice()),
            Type::Num => {
                let raw = val.val();
                self.alloc_node(raw.tag, raw.uni)
            }
            Type::Arr => {
                let arr = self.arr();
                for child in val.iter_arr() {
                    let c = self.copy_from_val(&child);
                    self.arr_append(arr, c);
                }
                arr
            }
            Type::Obj => {
                let obj = self.obj();
                for (k, v) in val.iter_obj() {
                    let mk = self.strn(k.str_slice());
                    let mv = self.copy_from_val(&v);
                    self.obj_add(obj, mk, mv);
                }
                obj
            }
        }
    }

    /// Deep-copy another value inside this `MutDoc` (or from another `MutDoc`).
    pub fn copy_from_mut(&mut self, src: &MutDoc, id: MutValId) -> MutValId {
        let ty = src.get_type(id);
        match ty {
            Type::None => self.alloc_node(TAG_NONE, 0),
            Type::Null => self.null(),
            Type::Bool => self.bool(src.is_true(id)),
            Type::Raw => {
                let bytes = src.raw_bytes(id).unwrap_or(&[]);
                self.rawn(bytes)
            }
            Type::Str => {
                let bytes = src.str_bytes(id).unwrap_or(&[]);
                self.strn(bytes)
            }
            Type::Num => {
                let node = &src.nodes[id.0 as usize];
                self.alloc_node(node.tag, node.uni)
            }
            Type::Arr => {
                let arr = self.arr();
                let mut cur = src.arr_first(id);
                while let Some(cid) = cur {
                    let c = self.copy_from_mut(src, cid);
                    self.arr_append(arr, c);
                    cur = src.arr_next(id, cid);
                }
                arr
            }
            Type::Obj => {
                let obj = self.obj();
                let mut cur = src.obj_first_key(id);
                while let Some(kid) = cur {
                    let vid = MutValId(src.nodes[kid.0 as usize].next);
                    let mk = self.copy_from_mut(src, kid);
                    let mv = self.copy_from_mut(src, vid);
                    self.obj_add(obj, mk, mv);
                    cur = src.obj_next_key(id, kid);
                }
                obj
            }
        }
    }

    // ---- Value Inspection & Mutation ----

    #[inline] pub fn get_type(&self, id: MutValId) -> Type { tag_type(self.nodes[id.0 as usize].tag) }
    #[inline] pub fn is_null(&self, id: MutValId) -> bool { self.get_type(id) == Type::Null }
    #[inline] pub fn is_true(&self, id: MutValId) -> bool { (self.nodes[id.0 as usize].tag as u8) == TAG_TRUE as u8 }
    #[inline] pub fn is_false(&self, id: MutValId) -> bool { (self.nodes[id.0 as usize].tag as u8) == TAG_FALSE as u8 }
    #[inline] pub fn is_bool(&self, id: MutValId) -> bool { self.get_type(id) == Type::Bool }
    #[inline] pub fn is_uint(&self, id: MutValId) -> bool { (self.nodes[id.0 as usize].tag as u8) == TAG_UINT as u8 }
    #[inline] pub fn is_sint(&self, id: MutValId) -> bool { (self.nodes[id.0 as usize].tag as u8) == TAG_SINT as u8 }
    #[inline] pub fn is_int(&self, id: MutValId) -> bool { self.is_uint(id) || self.is_sint(id) }
    #[inline] pub fn is_real(&self, id: MutValId) -> bool { (self.nodes[id.0 as usize].tag as u8) == TAG_REAL as u8 }
    #[inline] pub fn is_num(&self, id: MutValId) -> bool { self.get_type(id) == Type::Num }
    #[inline] pub fn is_str(&self, id: MutValId) -> bool { self.get_type(id) == Type::Str }
    #[inline] pub fn is_arr(&self, id: MutValId) -> bool { self.get_type(id) == Type::Arr }
    #[inline] pub fn is_obj(&self, id: MutValId) -> bool { self.get_type(id) == Type::Obj }
    #[inline] pub fn is_ctn(&self, id: MutValId) -> bool { matches!(self.get_type(id), Type::Arr | Type::Obj) }
    #[inline] pub fn is_raw(&self, id: MutValId) -> bool { self.get_type(id) == Type::Raw }
    #[inline] pub fn len(&self, id: MutValId) -> usize { tag_len(self.nodes[id.0 as usize].tag) }

    #[inline]
    pub fn as_bool(&self, id: MutValId) -> Option<bool> {
        if self.is_bool(id) { Some(self.is_true(id)) } else { None }
    }

    #[inline]
    pub fn as_u64(&self, id: MutValId) -> Option<u64> {
        let n = &self.nodes[id.0 as usize];
        if (n.tag as u8) == TAG_UINT as u8 {
            Some(n.uni)
        } else if (n.tag as u8) == TAG_SINT as u8 {
            let v = n.uni as i64;
            if v >= 0 { Some(v as u64) } else { None }
        } else {
            None
        }
    }

    #[inline]
    pub fn as_i64(&self, id: MutValId) -> Option<i64> {
        let n = &self.nodes[id.0 as usize];
        if (n.tag as u8) == TAG_SINT as u8 {
            Some(n.uni as i64)
        } else if (n.tag as u8) == TAG_UINT as u8 {
            if n.uni <= i64::MAX as u64 { Some(n.uni as i64) } else { None }
        } else {
            None
        }
    }

    #[inline]
    pub fn as_f64(&self, id: MutValId) -> Option<f64> {
        let n = &self.nodes[id.0 as usize];
        if (n.tag as u8) == TAG_REAL as u8 {
            Some(f64::from_bits(n.uni))
        } else {
            None
        }
    }

    #[inline]
    pub fn str_bytes(&self, id: MutValId) -> Option<&[u8]> {
        if self.is_str(id) {
            let n = &self.nodes[id.0 as usize];
            let ofs = n.uni as usize;
            Some(&self.buf[ofs..ofs + tag_len(n.tag)])
        } else {
            None
        }
    }

    #[inline]
    pub fn as_str(&self, id: MutValId) -> Option<&str> {
        self.str_bytes(id).and_then(|b| std::str::from_utf8(b).ok())
    }

    #[inline]
    pub fn raw_bytes(&self, id: MutValId) -> Option<&[u8]> {
        if self.is_raw(id) {
            let n = &self.nodes[id.0 as usize];
            let ofs = n.uni as usize;
            Some(&self.buf[ofs..ofs + tag_len(n.tag)])
        } else {
            None
        }
    }

    #[inline]
    pub fn set_null(&mut self, id: MutValId) {
        let n = &mut self.nodes[id.0 as usize];
        n.tag = TAG_NULL;
        n.uni = 0;
        n.tail = MUT_NONE;
    }

    #[inline]
    pub fn set_bool(&mut self, id: MutValId, v: bool) {
        let n = &mut self.nodes[id.0 as usize];
        n.tag = if v { TAG_TRUE } else { TAG_FALSE };
        n.uni = 0;
        n.tail = MUT_NONE;
    }

    #[inline]
    pub fn set_uint(&mut self, id: MutValId, v: u64) {
        let n = &mut self.nodes[id.0 as usize];
        n.tag = TAG_UINT;
        n.uni = v;
        n.tail = MUT_NONE;
    }

    #[inline]
    pub fn set_sint(&mut self, id: MutValId, v: i64) {
        let n = &mut self.nodes[id.0 as usize];
        n.tag = TAG_SINT;
        n.uni = v as u64;
        n.tail = MUT_NONE;
    }

    #[inline]
    pub fn set_real(&mut self, id: MutValId, v: f64) {
        let n = &mut self.nodes[id.0 as usize];
        n.tag = TAG_REAL;
        n.uni = v.to_bits();
        n.tail = MUT_NONE;
    }

    #[inline]
    pub fn set_str(&mut self, id: MutValId, s: &str) {
        let (ofs, len) = self.alloc_bytes(s.as_bytes());
        let n = &mut self.nodes[id.0 as usize];
        n.tag = make_tag(TAG_STR, len);
        n.uni = ofs;
        n.tail = MUT_NONE;
    }

    // ---- Array Operations ----

    #[inline]
    fn set_container_len(&mut self, id: MutValId, len: usize) {
        let ty = self.nodes[id.0 as usize].tag & 0xFF;
        self.nodes[id.0 as usize].tag = make_tag(ty, len);
    }

    pub fn arr_first(&self, arr: MutValId) -> Option<MutValId> {
        if !self.is_arr(arr) || self.len(arr) == 0 {
            return None;
        }
        let tail = self.nodes[arr.0 as usize].tail;
        if tail == MUT_NONE { None } else { Some(MutValId(self.nodes[tail as usize].next)) }
    }

    pub fn arr_last(&self, arr: MutValId) -> Option<MutValId> {
        if !self.is_arr(arr) || self.len(arr) == 0 {
            return None;
        }
        let tail = self.nodes[arr.0 as usize].tail;
        if tail == MUT_NONE { None } else { Some(MutValId(tail)) }
    }

    pub fn arr_next(&self, arr: MutValId, cur: MutValId) -> Option<MutValId> {
        let tail = self.nodes[arr.0 as usize].tail;
        if cur.0 == tail || tail == MUT_NONE {
            None
        } else {
            Some(MutValId(self.nodes[cur.0 as usize].next))
        }
    }

    pub fn arr_get(&self, arr: MutValId, idx: usize) -> Option<MutValId> {
        let len = self.len(arr);
        if !self.is_arr(arr) || idx >= len {
            return None;
        }
        let mut cur = self.arr_first(arr)?;
        for _ in 0..idx {
            cur = MutValId(self.nodes[cur.0 as usize].next);
        }
        Some(cur)
    }

    pub fn arr_append(&mut self, arr: MutValId, val: MutValId) -> bool {
        if !self.is_arr(arr) {
            return false;
        }
        let len = self.len(arr);
        if len == 0 {
            self.nodes[val.0 as usize].next = val.0;
            self.nodes[val.0 as usize].prev = val.0;
            self.nodes[arr.0 as usize].tail = val.0;
        } else {
            let tail = self.nodes[arr.0 as usize].tail;
            let head = self.nodes[tail as usize].next;
            self.nodes[val.0 as usize].next = head;
            self.nodes[val.0 as usize].prev = tail;
            self.nodes[tail as usize].next = val.0;
            self.nodes[head as usize].prev = val.0;
            self.nodes[arr.0 as usize].tail = val.0;
        }
        self.set_container_len(arr, len + 1);
        true
    }

    pub fn arr_prepend(&mut self, arr: MutValId, val: MutValId) -> bool {
        if !self.arr_append(arr, val) {
            return false;
        }
        let len = self.len(arr);
        if len > 1 {
            // Move tail back one step so newly appended val becomes head
            let prev = self.nodes[val.0 as usize].prev;
            self.nodes[arr.0 as usize].tail = prev;
        }
        true
    }

    pub fn arr_insert(&mut self, arr: MutValId, val: MutValId, idx: usize) -> bool {
        if !self.is_arr(arr) {
            return false;
        }
        let len = self.len(arr);
        if idx > len {
            return false;
        }
        if idx == 0 {
            return self.arr_prepend(arr, val);
        }
        if idx == len {
            return self.arr_append(arr, val);
        }
        let target = self.arr_get(arr, idx).unwrap();
        let prev = self.nodes[target.0 as usize].prev;
        self.nodes[val.0 as usize].next = target.0;
        self.nodes[val.0 as usize].prev = prev;
        self.nodes[prev as usize].next = val.0;
        self.nodes[target.0 as usize].prev = val.0;
        self.set_container_len(arr, len + 1);
        true
    }

    pub fn arr_remove(&mut self, arr: MutValId, idx: usize) -> Option<MutValId> {
        let len = self.len(arr);
        if !self.is_arr(arr) || idx >= len {
            return None;
        }
        let target = self.arr_get(arr, idx)?;
        if len == 1 {
            self.nodes[arr.0 as usize].tail = MUT_NONE;
        } else {
            let next = self.nodes[target.0 as usize].next;
            let prev = self.nodes[target.0 as usize].prev;
            self.nodes[prev as usize].next = next;
            self.nodes[next as usize].prev = prev;
            if self.nodes[arr.0 as usize].tail == target.0 {
                self.nodes[arr.0 as usize].tail = prev;
            }
        }
        self.nodes[target.0 as usize].next = MUT_NONE;
        self.nodes[target.0 as usize].prev = MUT_NONE;
        self.set_container_len(arr, len - 1);
        Some(target)
    }

    pub fn arr_replace(&mut self, arr: MutValId, idx: usize, val: MutValId) -> Option<MutValId> {
        let len = self.len(arr);
        if !self.is_arr(arr) || idx >= len {
            return None;
        }
        let old = self.arr_get(arr, idx)?;
        if len == 1 {
            self.nodes[val.0 as usize].next = val.0;
            self.nodes[val.0 as usize].prev = val.0;
            self.nodes[arr.0 as usize].tail = val.0;
        } else {
            let next = self.nodes[old.0 as usize].next;
            let prev = self.nodes[old.0 as usize].prev;
            self.nodes[val.0 as usize].next = next;
            self.nodes[val.0 as usize].prev = prev;
            self.nodes[prev as usize].next = val.0;
            self.nodes[next as usize].prev = val.0;
            if self.nodes[arr.0 as usize].tail == old.0 {
                self.nodes[arr.0 as usize].tail = val.0;
            }
        }
        Some(old)
    }

    pub fn arr_clear(&mut self, arr: MutValId) -> bool {
        if !self.is_arr(arr) {
            return false;
        }
        self.nodes[arr.0 as usize].tail = MUT_NONE;
        self.set_container_len(arr, 0);
        true
    }

    pub fn arr_rotate(&mut self, arr: MutValId, idx: usize) -> bool {
        let len = self.len(arr);
        if !self.is_arr(arr) {
            return false;
        }
        if len <= 1 || idx == 0 {
            return true;
        }
        let idx = idx % len;
        if idx == 0 {
            return true;
        }
        if let Some(new_head) = self.arr_get(arr, idx) {
            let new_tail = self.nodes[new_head.0 as usize].prev;
            self.nodes[arr.0 as usize].tail = new_tail;
            true
        } else {
            false
        }
    }

    // ---- Object Operations ----
    // In an object, each entry is stored as two consecutive linked nodes: key -> val -> next_key -> ...
    // `obj.tail` points to the LAST VALUE node, so `tail.next` is the FIRST KEY node!

    pub fn obj_first_key(&self, obj: MutValId) -> Option<MutValId> {
        if !self.is_obj(obj) || self.len(obj) == 0 {
            return None;
        }
        let tail = self.nodes[obj.0 as usize].tail;
        if tail == MUT_NONE { None } else { Some(MutValId(self.nodes[tail as usize].next)) }
    }

    pub fn obj_next_key(&self, obj: MutValId, key: MutValId) -> Option<MutValId> {
        let val = self.nodes[key.0 as usize].next;
        let tail = self.nodes[obj.0 as usize].tail;
        if val == tail || tail == MUT_NONE {
            None
        } else {
            Some(MutValId(self.nodes[val as usize].next))
        }
    }

    pub fn obj_get(&self, obj: MutValId, key: &str) -> Option<MutValId> {
        self.obj_get_n(obj, key.as_bytes())
    }

    pub fn obj_get_n(&self, obj: MutValId, key: &[u8]) -> Option<MutValId> {
        if !self.is_obj(obj) {
            return None;
        }
        let mut cur = self.obj_first_key(obj);
        while let Some(kid) = cur {
            if self.str_bytes(kid) == Some(key) {
                return Some(MutValId(self.nodes[kid.0 as usize].next));
            }
            cur = self.obj_next_key(obj, kid);
        }
        None
    }

    pub fn obj_add(&mut self, obj: MutValId, key: MutValId, val: MutValId) -> bool {
        if !self.is_obj(obj) || !self.is_str(key) {
            return false;
        }
        let len = self.len(obj);
        self.nodes[key.0 as usize].next = val.0;
        self.nodes[val.0 as usize].prev = key.0;
        if len == 0 {
            self.nodes[key.0 as usize].prev = val.0;
            self.nodes[val.0 as usize].next = key.0;
            self.nodes[obj.0 as usize].tail = val.0;
        } else {
            let tail = self.nodes[obj.0 as usize].tail;
            let head = self.nodes[tail as usize].next;
            self.nodes[key.0 as usize].prev = tail;
            self.nodes[val.0 as usize].next = head;
            self.nodes[tail as usize].next = key.0;
            self.nodes[head as usize].prev = val.0;
            self.nodes[obj.0 as usize].tail = val.0;
        }
        self.set_container_len(obj, len + 1);
        true
    }

    /// Put key-value pair into object (replaces existing key's value if found, else appends).
    pub fn obj_put(&mut self, obj: MutValId, key: MutValId, val: MutValId) -> bool {
        if !self.is_obj(obj) || !self.is_str(key) {
            return false;
        }
        let k_node = &self.nodes[key.0 as usize];
        let k_ofs = k_node.uni as usize;
        let k_len = tag_len(k_node.tag);
        let mut cur = self.obj_first_key(obj);
        while let Some(kid) = cur {
            let c_node = &self.nodes[kid.0 as usize];
            let c_ofs = c_node.uni as usize;
            let c_len = tag_len(c_node.tag);
            if c_len == k_len && self.buf[c_ofs..c_ofs + c_len] == self.buf[k_ofs..k_ofs + k_len] {
                let old_val = self.nodes[kid.0 as usize].next;
                let next_after = self.nodes[old_val as usize].next;
                if self.len(obj) == 1 {
                    self.nodes[kid.0 as usize].next = val.0;
                    self.nodes[val.0 as usize].prev = kid.0;
                    self.nodes[val.0 as usize].next = kid.0;
                    self.nodes[kid.0 as usize].prev = val.0;
                    self.nodes[obj.0 as usize].tail = val.0;
                } else {
                    self.nodes[kid.0 as usize].next = val.0;
                    self.nodes[val.0 as usize].prev = kid.0;
                    self.nodes[val.0 as usize].next = next_after;
                    self.nodes[next_after as usize].prev = val.0;
                    if self.nodes[obj.0 as usize].tail == old_val {
                        self.nodes[obj.0 as usize].tail = val.0;
                    }
                }
                return true;
            }
            cur = self.obj_next_key(obj, kid);
        }
        self.obj_add(obj, key, val)
    }

    pub fn obj_remove_key(&mut self, obj: MutValId, key: &str) -> Option<MutValId> {
        self.obj_remove_key_n(obj, key.as_bytes())
    }

    pub fn obj_remove_key_n(&mut self, obj: MutValId, key: &[u8]) -> Option<MutValId> {
        if !self.is_obj(obj) {
            return None;
        }
        let len = self.len(obj);
        let mut cur = self.obj_first_key(obj);
        while let Some(kid) = cur {
            if self.str_bytes(kid) == Some(key) {
                let vid = MutValId(self.nodes[kid.0 as usize].next);
                if len == 1 {
                    self.nodes[obj.0 as usize].tail = MUT_NONE;
                } else {
                    let prev_val = self.nodes[kid.0 as usize].prev;
                    let next_key = self.nodes[vid.0 as usize].next;
                    self.nodes[prev_val as usize].next = next_key;
                    self.nodes[next_key as usize].prev = prev_val;
                    if self.nodes[obj.0 as usize].tail == vid.0 {
                        self.nodes[obj.0 as usize].tail = prev_val;
                    }
                }
                self.set_container_len(obj, len - 1);
                return Some(vid);
            }
            cur = self.obj_next_key(obj, kid);
        }
        None
    }

    pub fn obj_clear(&mut self, obj: MutValId) -> bool {
        if !self.is_obj(obj) {
            return false;
        }
        self.nodes[obj.0 as usize].tail = MUT_NONE;
        self.set_container_len(obj, 0);
        true
    }

    // Convenience object helpers
    pub fn obj_add_str(&mut self, obj: MutValId, k: &str, v: &str) -> bool {
        let mk = self.str(k); let mv = self.str(v); self.obj_add(obj, mk, mv)
    }
    pub fn obj_add_uint(&mut self, obj: MutValId, k: &str, v: u64) -> bool {
        let mk = self.str(k); let mv = self.uint(v); self.obj_add(obj, mk, mv)
    }
    pub fn obj_add_sint(&mut self, obj: MutValId, k: &str, v: i64) -> bool {
        let mk = self.str(k); let mv = self.sint(v); self.obj_add(obj, mk, mv)
    }
    pub fn obj_add_real(&mut self, obj: MutValId, k: &str, v: f64) -> bool {
        let mk = self.str(k); let mv = self.real(v); self.obj_add(obj, mk, mv)
    }
    pub fn obj_add_bool(&mut self, obj: MutValId, k: &str, v: bool) -> bool {
        let mk = self.str(k); let mv = self.bool(v); self.obj_add(obj, mk, mv)
    }
    pub fn obj_add_null(&mut self, obj: MutValId, k: &str) -> bool {
        let mk = self.str(k); let mv = self.null(); self.obj_add(obj, mk, mv)
    }

    // ---- Conversion & Serialization ----

    /// Convert this `MutDoc` into an immutable `Doc`.
    pub fn to_doc(&self) -> Doc {
        let mut vals = Vec::new();
        let mut buf = Vec::new();
        if let Some(root) = self.root {
            self.flatten_to_doc(root, &mut vals, &mut buf);
        }
        Doc { vals, buf, read_size: 0 }
    }

    fn flatten_to_doc(&self, root: MutValId, vals: &mut Vec<Val>, buf: &mut Vec<u8>) {
        enum Step {
            Visit(MutValId),
            CloseCtn(usize),
        }
        let mut stack = Vec::with_capacity(32);
        stack.push(Step::Visit(root));
        while let Some(step) = stack.pop() {
            match step {
                Step::CloseCtn(slot) => {
                    vals[slot].uni = (vals.len() - slot) as u64;
                }
                Step::Visit(id) => {
                    let node = &self.nodes[id.0 as usize];
                    match tag_type(node.tag) {
                        Type::None | Type::Null | Type::Bool | Type::Num => {
                            vals.push(Val { tag: node.tag, uni: node.uni });
                        }
                        Type::Str | Type::Raw => {
                            let ofs = buf.len() as u64;
                            let src_ofs = node.uni as usize;
                            let len = tag_len(node.tag);
                            buf.extend_from_slice(&self.buf[src_ofs..src_ofs + len]);
                            vals.push(Val { tag: node.tag, uni: ofs });
                        }
                        Type::Arr => {
                            let slot = vals.len();
                            let len = tag_len(node.tag);
                            vals.push(Val { tag: make_tag(TAG_ARR, len), uni: 1 });
                            if len > 0 {
                                stack.push(Step::CloseCtn(slot));
                                // Push children in reverse order so first child is popped first
                                let tail = node.tail;
                                let mut cur = tail;
                                for _ in 0..len {
                                    stack.push(Step::Visit(MutValId(cur)));
                                    cur = self.nodes[cur as usize].prev;
                                }
                            }
                        }
                        Type::Obj => {
                            let slot = vals.len();
                            let len = tag_len(node.tag);
                            vals.push(Val { tag: make_tag(TAG_OBJ, len), uni: 1 });
                            if len > 0 {
                                stack.push(Step::CloseCtn(slot));
                                // Push (key, val) pairs in reverse order
                                let mut cur_val = node.tail;
                                for _ in 0..len {
                                    let cur_key = self.nodes[cur_val as usize].prev;
                                    stack.push(Step::Visit(MutValId(cur_val)));
                                    stack.push(Step::Visit(MutValId(cur_key)));
                                    cur_val = self.nodes[cur_key as usize].prev;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn write(&self, flags: WriteFlags) -> Result<String, WriteError> {
        let bytes = self.write_bytes(flags)?;
        String::from_utf8(bytes).map_err(|_| {
            WriteError::new(WriteErrorCode::InvalidString, "invalid UTF-8 in output")
        })
    }

    pub fn write_bytes(&self, flags: WriteFlags) -> Result<Vec<u8>, WriteError> {
        let root = self.root.ok_or_else(|| {
            WriteError::new(WriteErrorCode::InvalidParameter, "mutable document has no root value")
        })?;
        write_mut_val_to_vec(self, root, flags)
    }

    pub fn write_file(&self, path: impl AsRef<Path>, flags: WriteFlags) -> Result<(), WriteError> {
        let bytes = self.write_bytes(flags)?;
        let mut f = fs::File::create(path.as_ref()).map_err(|_| {
            WriteError::new(WriteErrorCode::FileOpen, "failed to open file for writing")
        })?;
        f.write_all(&bytes).map_err(|_| {
            WriteError::new(WriteErrorCode::FileWrite, "failed to write to file")
        })
    }

    pub fn ptr_get(&self, pointer: &str) -> Result<MutValId, PtrError> {
        let root = self.root.ok_or(PtrError {
            code: PtrErrorCode::Null,
            msg: "empty document",
            pos: 0,
        })?;
        ptr_get_mut(self, root, pointer)
    }

    pub fn ptr_set(&mut self, pointer: &str, val: MutValId) -> Result<Option<MutValId>, PtrError> {
        ptr_set_mut(self, pointer, val)
    }

    pub fn ptr_add(&mut self, pointer: &str, val: MutValId) -> Result<(), PtrError> {
        ptr_add_mut(self, pointer, val)
    }

    pub fn ptr_replace(&mut self, pointer: &str, val: MutValId) -> Result<MutValId, PtrError> {
        ptr_replace_mut(self, pointer, val)
    }

    pub fn ptr_remove(&mut self, pointer: &str) -> Result<MutValId, PtrError> {
        ptr_remove_mut(self, pointer)
    }

    pub fn patch(&mut self, patch_doc: &Doc) -> Result<(), PatchError> {
        json_patch(self, patch_doc)
    }

    pub fn merge_patch(&mut self, patch_val: &ValRef<'_>) -> bool {
        json_merge_patch(self, patch_val)
    }
}

// ============================================================================
// JSON Reader (RFC 8259 + JSON5 / vzjson Extensions)
// ============================================================================

const MAX_READ_DEPTH: usize = 1024;

#[derive(Copy, Clone, Debug)]
struct CtnFrame {
    val_idx: usize,
    is_obj: bool,
    count: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum IncrPhase {
    Init,
    ExpectValue,
    AfterValue,
    Trailer,
}

enum SrcBuf<'a> {
    Slice(&'a [u8]),
    Insitu(Vec<u8>),
}

impl<'a> SrcBuf<'a> {
    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        match self {
            SrcBuf::Slice(s) => s,
            SrcBuf::Insitu(v) => v.as_slice(),
        }
    }

    #[inline(always)]
    fn is_insitu(&self) -> bool {
        matches!(self, SrcBuf::Insitu(_))
    }

    #[inline(always)]
    fn write_byte(&mut self, idx: usize, b: u8) {
        if let SrcBuf::Insitu(v) = self {
            v[idx] = b;
        }
    }
}

#[inline]
fn swar_scan_str_ascii(bytes: &[u8], mut p: usize, quote: u8) -> (usize, bool) {
    let q_mask = u64::from_le_bytes([quote; 8]);
    let bs_mask = u64::from_le_bytes([b'\\'; 8]);
    let sl_mask = u64::from_le_bytes([b'/'; 8]);
    let mut has_slash = false;
    while p + 8 <= bytes.len() {
        let chunk = u64::from_le_bytes([
            bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3],
            bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7],
        ]);
        let hi_or_ctrl = (chunk & 0x8080_8080_8080_8080)
            | (chunk.wrapping_sub(0x2020_2020_2020_2020) & !chunk & 0x8080_8080_8080_8080);
        let xq = chunk ^ q_mask;
        let has_q = xq.wrapping_sub(0x0101_0101_0101_0101) & !xq & 0x8080_8080_8080_8080;
        let xbs = chunk ^ bs_mask;
        let has_bs = xbs.wrapping_sub(0x0101_0101_0101_0101) & !xbs & 0x8080_8080_8080_8080;
        let stop_mask = hi_or_ctrl | has_q | has_bs;
        if stop_mask != 0 {
            let idx = (stop_mask.trailing_zeros() as usize) >> 3;
            let xsl = chunk ^ sl_mask;
            let has_sl = xsl.wrapping_sub(0x0101_0101_0101_0101) & !xsl & 0x8080_8080_8080_8080;
            if has_sl != 0 && ((has_sl.trailing_zeros() as usize) >> 3) < idx {
                has_slash = true;
            }
            return (p + idx, has_slash);
        }
        let xsl = chunk ^ sl_mask;
        let has_sl = xsl.wrapping_sub(0x0101_0101_0101_0101) & !xsl & 0x8080_8080_8080_8080;
        if has_sl != 0 {
            has_slash = true;
        }
        p += 8;
    }
    (p, has_slash)
}

#[inline]
fn swar_parse_8_digits(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 8 {
        return None;
    }
    let val = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let is_digits = (((val & 0xF0F0_F0F0_F0F0_F0F0) ^ 0x3030_3030_3030_3030)
        | ((val.wrapping_add(0x0606_0606_0606_0606) & 0xF0F0_F0F0_F0F0_F0F0) ^ 0x3030_3030_3030_3030))
        == 0;
    if !is_digits {
        return None;
    }
    let mut d = val & 0x0F0F_0F0F_0F0F_0F0F;
    d = (d * 10 + (d >> 8)) & 0x00FF_00FF_00FF_00FF;
    d = (d * 100 + (d >> 16)) & 0x0000_FFFF_0000_FFFF;
    d = (d * 10000 + (d >> 32)) & 0x0000_0000_FFFF_FFFF;
    Some(d)
}

#[inline]
fn swar_parse_4_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 4 {
        return None;
    }
    let val = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let is_digits = (((val & 0xF0F0_F0F0) ^ 0x3030_3030)
        | ((val.wrapping_add(0x0606_0606) & 0xF0F0_F0F0) ^ 0x3030_3030))
        == 0;
    if !is_digits {
        return None;
    }
    let mut d = val & 0x0F0F_0F0F;
    d = (d * 10 + (d >> 8)) & 0x00FF_00FF;
    d = (d * 100 + (d >> 16)) & 0x0000_FFFF;
    Some(d)
}

struct Reader<'a> {
    src: SrcBuf<'a>,
    pos: usize,
    flags: ReadFlags,
    vals: Vec<Val>,
    str_buf: Vec<u8>,
    stack: Vec<CtnFrame>,
    incr_mode: bool,
    is_final: bool,
}

#[inline]
fn estimate_reader_capacities(src: &[u8], insitu: bool) -> (usize, usize) {
    let len = src.len();
    if len <= 256 {
        let est_vals = (len / 4).clamp(16, 1 << 20);
        let est_str = if insitu { 0 } else { len / 2 };
        return (est_vals, est_str);
    }

    let sample = &src[..len.min(128)];
    let mut ws = 0usize;
    let mut quotes = 0usize;
    let mut delims = 0usize;
    for &b in sample {
        match b {
            b' ' | b'\n' | b'\r' | b'\t' => ws += 1,
            b'"' => quotes += 1,
            b',' | b':' | b'[' | b'{' => delims += 1,
            _ => {}
        }
    }

    let slen = sample.len();
    let non_ws = slen - ws;
    // Estimate value count from structural delimiters, with a safe floor for mixed/pretty JSON
    let est_vals = if delims <= 3 {
        (len / 64).clamp(16, 1 << 20)
    } else {
        let by_delims = (delims * len * 5) / (slen * 4);
        let by_non_ws = (non_ws * len) / (slen * 7);
        by_delims
            .max(by_non_ws)
            .clamp(16, (len / 2 + 1).min(1 << 20))
    };

    let est_str = if insitu || quotes == 0 {
        0
    } else if delims <= 3 {
        // Long-string dominated JSON
        (non_ws * len * 7) / (slen * 8)
    } else {
        // General / mixed / indented JSON
        (non_ws * len * 5) / (slen * 8)
    };

    (est_vals, est_str)
}

impl<'a> Reader<'a> {
    fn new(src: &'a [u8], flags: ReadFlags, incr_mode: bool) -> Self {
        let (est_vals, est_str) = estimate_reader_capacities(src, false);
        Self {
            src: SrcBuf::Slice(src),
            pos: 0,
            flags,
            vals: Vec::with_capacity(est_vals),
            str_buf: Vec::with_capacity(est_str),
            stack: Vec::with_capacity(32),
            incr_mode,
            is_final: !incr_mode,
        }
    }

    fn new_insitu(data: Vec<u8>, flags: ReadFlags) -> Self {
        let (est_vals, _) = estimate_reader_capacities(&data, true);
        Self {
            src: SrcBuf::Insitu(data),
            pos: 0,
            flags: flags | ReadFlags::INSITU,
            vals: Vec::with_capacity(est_vals),
            str_buf: Vec::new(), // Zero-copy: strings live in `src`!
            stack: Vec::with_capacity(32),
            incr_mode: false,
            is_final: true,
        }
    }

    #[inline(always)]
    fn src(&self) -> &[u8] {
        self.src.as_slice()
    }

    #[inline(always)]
    fn store_slice(&mut self, start: usize, end: usize) -> u64 {
        if self.src.is_insitu() {
            start as u64
        } else {
            let ofs = self.str_buf.len() as u64;
            self.str_buf.extend_from_slice(&self.src.as_slice()[start..end]);
            ofs
        }
    }

    #[inline(always)]
    fn push_str_byte(&mut self, w: &mut usize, b: u8) {
        if self.src.is_insitu() {
            self.src.write_byte(*w, b);
            *w += 1;
        } else {
            self.str_buf.push(b);
        }
    }

    #[inline]
    fn push_str_utf8(&mut self, w: &mut usize, cp: u32) {
        if cp < 0x80 {
            self.push_str_byte(w, cp as u8);
        } else if cp < 0x800 {
            self.push_str_byte(w, (0xC0 | (cp >> 6)) as u8);
            self.push_str_byte(w, (0x80 | (cp & 0x3F)) as u8);
        } else if cp < 0x10000 {
            self.push_str_byte(w, (0xE0 | (cp >> 12)) as u8);
            self.push_str_byte(w, (0x80 | ((cp >> 6) & 0x3F)) as u8);
            self.push_str_byte(w, (0x80 | (cp & 0x3F)) as u8);
        } else {
            self.push_str_byte(w, (0xF0 | (cp >> 18)) as u8);
            self.push_str_byte(w, (0x80 | ((cp >> 12) & 0x3F)) as u8);
            self.push_str_byte(w, (0x80 | ((cp >> 6) & 0x3F)) as u8);
            self.push_str_byte(w, (0x80 | (cp & 0x3F)) as u8);
        }
    }

    #[inline(always)]
    fn err(&self, code: ReadErrorCode, msg: &'static str) -> ReadError {
        if self.incr_mode && code == ReadErrorCode::UnexpectedEnd {
            ReadError::new(ReadErrorCode::More, "more data required", self.pos)
        } else {
            ReadError::new(code, msg, self.pos)
        }
    }

    #[inline(always)]
    fn eof_err(&self, msg: &'static str) -> ReadError {
        if self.incr_mode {
            ReadError::new(ReadErrorCode::More, "more data required", self.pos)
        } else {
            ReadError::new(ReadErrorCode::UnexpectedEnd, msg, self.pos)
        }
    }

    /// Skip whitespace and optional comments.
    fn skip_ws(&mut self) -> Result<(), ReadError> {
        let ext_ws = self.flags.contains(ReadFlags::ALLOW_EXT_WHITESPACE);
        let comments = self.flags.contains(ReadFlags::ALLOW_COMMENTS);
        loop {
            let s = self.src();
            let len = s.len();
            let mut p = self.pos;
            while p + 8 <= len {
                let chunk = u64::from_le_bytes([
                    s[p], s[p + 1], s[p + 2], s[p + 3],
                    s[p + 4], s[p + 5], s[p + 6], s[p + 7],
                ]);
                if chunk != 0x2020_2020_2020_2020 {
                    break;
                }
                p += 8;
            }
            while p < len && is_ws_std(s[p]) {
                p += 1;
            }
            self.pos = p;
            if p >= len {
                return Ok(());
            }
            let b = self.src()[self.pos];
            if ext_ws {
                if b == 0x0B || b == 0x0C {
                    self.pos += 1;
                    continue;
                }
                if b == 0xC2 && self.pos + 1 < self.src().len() && self.src()[self.pos + 1] == 0xA0 {
                    self.pos += 2;
                    continue;
                }
                if b >= 0xE0 {
                    if let Some(s) = std::str::from_utf8(&self.src()[self.pos..]).ok() {
                        if let Some(ch) = s.chars().next() {
                            if ch == '\u{2028}' || ch == '\u{2029}' || ch == '\u{FEFF}' || ch.is_whitespace() {
                                self.pos += ch.len_utf8();
                                continue;
                            }
                        }
                    }
                }
            }
            if comments && b == b'/' {
                if self.pos + 1 >= self.src().len() {
                    return Err(self.eof_err("unexpected end in comment"));
                }
                let next = self.src()[self.pos + 1];
                if next == b'/' {
                    self.pos += 2;
                    while self.pos < self.src().len() {
                        let c = self.src()[self.pos];
                        if c == b'\n' || c == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                    continue;
                } else if next == b'*' {
                    self.pos += 2;
                    let mut closed = false;
                    while self.pos + 1 < self.src().len() {
                        if self.src()[self.pos] == b'*' && self.src()[self.pos + 1] == b'/' {
                            self.pos += 2;
                            closed = true;
                            break;
                        }
                        self.pos += 1;
                    }
                    if !closed {
                        return Err(self.eof_err("unclosed block comment"));
                    }
                    continue;
                }
            }
            return Ok(());
        }
    }

    fn parse(&mut self) -> Result<Doc, ReadError> {
        if self.src().is_empty() {
            return Err(ReadError::new(
                ReadErrorCode::InvalidParameter,
                "input data is empty",
                0,
            ));
        }

        if self.flags.contains(ReadFlags::ALLOW_BOM)
            && self.src().len() >= 3
            && self.src()[0] == 0xEF
            && self.src()[1] == 0xBB
            && self.src()[2] == 0xBF
        {
            self.pos = 3;
        }

        self.skip_ws()?;
        if self.pos >= self.src().len() {
            return Err(self.err(ReadErrorCode::EmptyContent, "input JSON is empty"));
        }

        self.parse_root()?;

        if !self.flags.contains(ReadFlags::STOP_WHEN_DONE) {
            self.skip_ws()?;
            if self.pos < self.src().len() {
                return Err(self.err(
                    ReadErrorCode::UnexpectedContent,
                    "unexpected content after document",
                ));
            }
        }

        let buf = match std::mem::replace(&mut self.src, SrcBuf::Slice(&[])) {
            SrcBuf::Insitu(v) => v,
            SrcBuf::Slice(_) => std::mem::take(&mut self.str_buf),
        };
        Ok(Doc {
            vals: std::mem::take(&mut self.vals),
            buf,
            read_size: self.pos,
        })
    }

    fn parse_root(&mut self) -> Result<(), ReadError> {
        'main: loop {
            if self.pos >= self.src().len() {
                return Err(self.eof_err("unexpected end of input"));
            }
            let b = self.src()[self.pos];
            match b {
                b'{' => {
                    if self.stack.len() >= MAX_READ_DEPTH {
                        return Err(self.err(ReadErrorCode::Depth, "depth limit exceeded"));
                    }
                    let idx = self.vals.len();
                    self.vals.push(Val { tag: TAG_OBJ, uni: 1 });
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.pos >= self.src().len() {
                        return Err(self.eof_err("unexpected end in object"));
                    }
                    if self.src()[self.pos] == b'}' {
                        self.pos += 1;
                    } else {
                        self.stack.push(CtnFrame { val_idx: idx, is_obj: true, count: 0 });
                        self.parse_obj_key()?;
                        continue 'main;
                    }
                }
                b'[' => {
                    if self.stack.len() >= MAX_READ_DEPTH {
                        return Err(self.err(ReadErrorCode::Depth, "depth limit exceeded"));
                    }
                    let idx = self.vals.len();
                    self.vals.push(Val { tag: TAG_ARR, uni: 1 });
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.pos >= self.src().len() {
                        return Err(self.eof_err("unexpected end in array"));
                    }
                    if self.src()[self.pos] == b']' {
                        self.pos += 1;
                    } else {
                        self.stack.push(CtnFrame { val_idx: idx, is_obj: false, count: 0 });
                        continue 'main;
                    }
                }
                b'"' => {
                    let v = self.read_string(b'"')?;
                    self.vals.push(v);
                }
                b'\'' if self.flags.contains(ReadFlags::ALLOW_SINGLE_QUOTED_STR) => {
                    let v = self.read_string(b'\'')?;
                    self.vals.push(v);
                }
                b't' => {
                    self.read_true()?;
                    self.vals.push(Val { tag: TAG_TRUE, uni: 0 });
                }
                b'f' => {
                    self.read_false()?;
                    self.vals.push(Val { tag: TAG_FALSE, uni: 0 });
                }
                b'n' => {
                    if self.flags.contains(ReadFlags::ALLOW_INF_AND_NAN) {
                        if self.pos + 1 >= self.src().len() {
                            return Err(self.eof_err("unexpected end in literal"));
                        }
                        if self.src()[self.pos + 1] == b'a' || self.src()[self.pos + 1] == b'A' {
                            let v = self.read_inf_or_nan(false)?;
                            self.vals.push(v);
                        } else {
                            self.read_null()?;
                            self.vals.push(Val { tag: TAG_NULL, uni: 0 });
                        }
                    } else {
                        self.read_null()?;
                        self.vals.push(Val { tag: TAG_NULL, uni: 0 });
                    }
                }
                b'0'..=b'9' | b'-' => {
                    let v = self.read_number()?;
                    self.vals.push(v);
                }
                b'+' | b'.' if self.flags.contains(ReadFlags::ALLOW_EXT_NUMBER) => {
                    let v = self.read_number()?;
                    self.vals.push(v);
                }
                b'I' | b'i' | b'N' if self.flags.contains(ReadFlags::ALLOW_INF_AND_NAN) => {
                    let v = self.read_inf_or_nan(false)?;
                    self.vals.push(v);
                }
                _ => {
                    return Err(self.err(ReadErrorCode::UnexpectedCharacter, "unexpected character"));
                }
            }

            // A value was just completed; unwind containers as needed
            loop {
                let Some(top) = self.stack.last_mut() else {
                    return Ok(());
                };
                top.count += 1;
                let is_obj = top.is_obj;
                let ctn_idx = top.val_idx;
                let count = top.count;

                self.skip_ws()?;
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("unexpected end in container"));
                }
                let sep = self.src()[self.pos];
                if sep == b',' {
                    self.pos += 1;
                    self.skip_ws()?;
                    if self.pos >= self.src().len() {
                        return Err(self.eof_err("unexpected end after comma"));
                    }
                    let next_b = self.src()[self.pos];
                    if (is_obj && next_b == b'}') || (!is_obj && next_b == b']') {
                        if self.flags.contains(ReadFlags::ALLOW_TRAILING_COMMAS) {
                            self.pos += 1;
                            self.stack.pop();
                            let ty = if is_obj { TAG_OBJ } else { TAG_ARR };
                            self.vals[ctn_idx].tag = make_tag(ty, count);
                            self.vals[ctn_idx].uni = (self.vals.len() - ctn_idx) as u64;
                            continue;
                        } else {
                            return Err(self.err(
                                ReadErrorCode::JsonStructure,
                                "trailing comma is not allowed",
                            ));
                        }
                    }
                    if is_obj {
                        self.parse_obj_key()?;
                    }
                    continue 'main;
                } else if (is_obj && sep == b'}') || (!is_obj && sep == b']') {
                    self.pos += 1;
                    self.stack.pop();
                    let ty = if is_obj { TAG_OBJ } else { TAG_ARR };
                    self.vals[ctn_idx].tag = make_tag(ty, count);
                    self.vals[ctn_idx].uni = (self.vals.len() - ctn_idx) as u64;
                } else {
                    return Err(self.err(
                        ReadErrorCode::JsonStructure,
                        "expected ',' or closing bracket",
                    ));
                }
            }
        }
    }

    fn parse_obj_key(&mut self) -> Result<(), ReadError> {
        if self.pos >= self.src().len() {
            return Err(self.eof_err("unexpected end before object key"));
        }
        let b = self.src()[self.pos];
        let key_val = if b == b'"' {
            self.read_string(b'"')?
        } else if b == b'\'' && self.flags.contains(ReadFlags::ALLOW_SINGLE_QUOTED_STR) {
            self.read_string(b'\'')?
        } else if self.flags.contains(ReadFlags::ALLOW_UNQUOTED_KEY) {
            self.read_unquoted_key()?
        } else {
            return Err(self.err(
                ReadErrorCode::UnexpectedCharacter,
                "expected string key in object",
            ));
        };
        self.vals.push(key_val);

        self.skip_ws()?;
        if self.pos >= self.src().len() {
            return Err(self.eof_err("unexpected end after object key"));
        }
        if self.src()[self.pos] != b':' {
            return Err(self.err(ReadErrorCode::JsonStructure, "expected ':' after object key"));
        }
        self.pos += 1;
        self.skip_ws()?;
        Ok(())
    }

    #[inline]
    fn read_true(&mut self) -> Result<(), ReadError> {
        if self.pos + 4 > self.src().len() {
            return Err(self.eof_err("unexpected end in literal"));
        }
        if &self.src()[self.pos..self.pos + 4] == b"true" {
            self.pos += 4;
            Ok(())
        } else {
            Err(self.err(ReadErrorCode::Literal, "invalid literal"))
        }
    }

    #[inline]
    fn read_false(&mut self) -> Result<(), ReadError> {
        if self.pos + 5 > self.src().len() {
            return Err(self.eof_err("unexpected end in literal"));
        }
        if &self.src()[self.pos..self.pos + 5] == b"false" {
            self.pos += 5;
            Ok(())
        } else {
            Err(self.err(ReadErrorCode::Literal, "invalid literal"))
        }
    }

    #[inline]
    fn read_null(&mut self) -> Result<(), ReadError> {
        if self.pos + 4 > self.src().len() {
            return Err(self.eof_err("unexpected end in literal"));
        }
        if &self.src()[self.pos..self.pos + 4] == b"null" {
            self.pos += 4;
            Ok(())
        } else {
            Err(self.err(ReadErrorCode::Literal, "invalid literal"))
        }
    }

    fn read_string(&mut self, quote: u8) -> Result<Val, ReadError> {
        self.pos += 1; // skip opening quote
        let start = self.pos;
        let allow_invalid_utf8 = self.flags.contains(ReadFlags::ALLOW_INVALID_UNICODE);
        let ext_esc = self.flags.contains(ReadFlags::ALLOW_EXT_ESCAPE);

        // Fast SWAR 8-byte scan: check if string has no escapes, no control chars, and no non-ASCII
        let (mut p, mut has_slash) = swar_scan_str_ascii(self.src(), start, quote);
        while p < self.src().len() {
            let b = self.src()[p];
            if b == quote {
                let len = p - start;
                let ofs = self.store_slice(start, p);
                self.pos = p + 1;
                let tag = if has_slash {
                    make_tag(TAG_STR, len)
                } else {
                    make_tag(TAG_STR_NOESC, len)
                };
                return Ok(Val { tag, uni: ofs });
            }
            if b == b'\\' {
                break;
            }
            if b == b'/' {
                has_slash = true;
                p += 1;
                continue;
            }
            if b < 0x20 {
                self.pos = p;
                return Err(self.err(
                    ReadErrorCode::InvalidString,
                    "unescaped control character in string",
                ));
            }
            if b >= 0x80 {
                if !allow_invalid_utf8 {
                    match utf8_char_len(&self.src()[p..]) {
                        Some(clen) => {
                            p += clen;
                            continue;
                        }
                        None => {
                            self.pos = p;
                            return Err(self.err(
                                ReadErrorCode::InvalidString,
                                "invalid UTF-8 sequence in string",
                            ));
                        }
                    }
                }
            }
            p += 1;
        }

        if p >= self.src().len() {
            self.pos = p;
            return Err(self.eof_err("unterminated string"));
        }

        // Slow path: string contains escape sequences (in-place if INSITU, else into str_buf)
        let insitu = self.src.is_insitu();
        let buf_start = if insitu { start } else { self.str_buf.len() };
        let mut w = p;
        if !insitu {
            self.str_buf.extend_from_slice(&self.src.as_slice()[start..p]);
        }
        self.pos = p;

        while self.pos < self.src().len() {
            let b = self.src()[self.pos];
            if b == quote {
                self.pos += 1;
                let len = if insitu { w - buf_start } else { self.str_buf.len() - buf_start };
                return Ok(Val {
                    tag: make_tag(TAG_STR, len),
                    uni: buf_start as u64,
                });
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("unterminated escape sequence"));
                }
                let esc = self.src()[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => self.push_str_byte(&mut w, b'"'),
                    b'\\' => self.push_str_byte(&mut w, b'\\'),
                    b'/' => self.push_str_byte(&mut w, b'/'),
                    b'b' => self.push_str_byte(&mut w, 0x08),
                    b'f' => self.push_str_byte(&mut w, 0x0C),
                    b'n' => self.push_str_byte(&mut w, b'\n'),
                    b'r' => self.push_str_byte(&mut w, b'\r'),
                    b't' => self.push_str_byte(&mut w, b'\t'),
                    b'u' => {
                        let cp = self.read_hex4()?;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            // High surrogate: must be followed by \uDC00..=0xDFFF
                            if self.incr_mode && self.pos + 6 > self.src().len() {
                                let rem = &self.src()[self.pos..];
                                if rem.is_empty()
                                    || (rem[0] == b'\\' && (rem.len() == 1 || rem[1] == b'u'))
                                {
                                    return Err(self.eof_err("incomplete surrogate pair"));
                                }
                            }
                            if self.pos + 6 <= self.src().len()
                                && self.src()[self.pos] == b'\\'
                                && self.src()[self.pos + 1] == b'u'
                            {
                                let saved = self.pos;
                                self.pos += 2;
                                let low = self.read_hex4()?;
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let full = 0x10000 + (((cp - 0xD800) << 10) | (low - 0xDC00));
                                    self.push_str_utf8(&mut w, full);
                                } else {
                                    self.pos = saved;
                                    if !allow_invalid_utf8 {
                                        return Err(self.err(
                                            ReadErrorCode::InvalidString,
                                            "unpaired high surrogate",
                                        ));
                                    }
                                    self.push_str_utf8(&mut w, 0xFFFD);
                                }
                            } else if !allow_invalid_utf8 {
                                return Err(self.err(
                                    ReadErrorCode::InvalidString,
                                    "unpaired high surrogate",
                                ));
                            } else {
                                self.push_str_utf8(&mut w, 0xFFFD);
                            }
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            if !allow_invalid_utf8 {
                                return Err(self.err(
                                    ReadErrorCode::InvalidString,
                                    "unpaired low surrogate",
                                ));
                            }
                            self.push_str_utf8(&mut w, 0xFFFD);
                        } else {
                            self.push_str_utf8(&mut w, cp);
                        }
                    }
                    b'\'' if ext_esc => self.push_str_byte(&mut w, b'\''),
                    b'?' if ext_esc => self.push_str_byte(&mut w, b'?'),
                    b'a' if ext_esc => self.push_str_byte(&mut w, 0x07),
                    b'e' if ext_esc => self.push_str_byte(&mut w, 0x1B),
                    b'v' if ext_esc => self.push_str_byte(&mut w, 0x0B),
                    b'0' if ext_esc => self.push_str_byte(&mut w, 0x00),
                    b'x' if ext_esc => {
                        if self.pos + 2 > self.src().len() {
                            return Err(self.eof_err("unterminated \\x escape"));
                        }
                        let h1 = hex_val(self.src()[self.pos]);
                        let h2 = hex_val(self.src()[self.pos + 1]);
                        if h1 == 0xFF || h2 == 0xFF {
                            return Err(self.err(ReadErrorCode::InvalidString, "invalid \\x hex escape"));
                        }
                        self.pos += 2;
                        self.push_str_byte(&mut w, (h1 << 4) | h2);
                    }
                    b'\n' if ext_esc => {}
                    b'\r' if ext_esc => {
                        if self.pos < self.src().len() && self.src()[self.pos] == b'\n' {
                            self.pos += 1;
                        }
                    }
                    c if ext_esc && !(b'1'..=b'9').contains(&c) => {
                        self.push_str_byte(&mut w, c);
                    }
                    _ => {
                        return Err(self.err(
                            ReadErrorCode::InvalidString,
                            "invalid escape sequence in string",
                        ));
                    }
                }
            } else if b < 0x20 {
                return Err(self.err(
                    ReadErrorCode::InvalidString,
                    "unescaped control character in string",
                ));
            } else if b >= 0x80 && !allow_invalid_utf8 {
                match utf8_char_len(&self.src()[self.pos..]) {
                    Some(clen) => {
                        for k in 0..clen {
                            let c = self.src()[self.pos + k];
                            self.push_str_byte(&mut w, c);
                        }
                        self.pos += clen;
                    }
                    None => {
                        return Err(self.err(
                            ReadErrorCode::InvalidString,
                            "invalid UTF-8 sequence in string",
                        ));
                    }
                }
            } else {
                self.push_str_byte(&mut w, b);
                self.pos += 1;
            }
        }

        Err(self.eof_err("unterminated string"))
    }

    #[inline]
    fn read_hex4(&mut self) -> Result<u32, ReadError> {
        if self.pos + 4 > self.src().len() {
            return Err(self.eof_err("unterminated \\u escape"));
        }
        let mut val = 0u32;
        for i in 0..4 {
            let d = hex_val(self.src()[self.pos + i]);
            if d == 0xFF {
                return Err(self.err(ReadErrorCode::InvalidString, "invalid hex digit in \\u escape"));
            }
            val = (val << 4) | (d as u32);
        }
        self.pos += 4;
        Ok(val)
    }

    fn read_unquoted_key(&mut self) -> Result<Val, ReadError> {
        let start = self.pos;
        while self.pos < self.src().len() {
            let b = self.src()[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                self.pos += 1;
            } else if b >= 0x80 {
                match utf8_char_len(&self.src()[self.pos..]) {
                    Some(clen) => {
                        if let Ok(s) = std::str::from_utf8(&self.src()[self.pos..self.pos + clen]) {
                            if let Some(ch) = s.chars().next() {
                                if ch.is_whitespace() {
                                    break;
                                }
                            }
                        }
                        self.pos += clen;
                    }
                    None => {
                        if self.flags.contains(ReadFlags::ALLOW_INVALID_UNICODE) {
                            self.pos += 1;
                        } else {
                            return Err(self.err(ReadErrorCode::InvalidString, "invalid UTF-8 in key"));
                        }
                    }
                }
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err(ReadErrorCode::UnexpectedCharacter, "empty unquoted key"));
        }
        let len = self.pos - start;
        let ofs = self.store_slice(start, self.pos);
        Ok(Val {
            tag: make_tag(TAG_STR, len),
            uni: ofs,
        })
    }

    fn read_inf_or_nan(&mut self, neg: bool) -> Result<Val, ReadError> {
        let start = self.pos;
        let rem = &self.src()[self.pos..];
        let mut is_nan = false;
        let matched_len: usize;
        if rem.len() >= 8 && rem[..8].eq_ignore_ascii_case(b"infinity") {
            matched_len = 8;
        } else if rem.len() >= 3 && rem[..3].eq_ignore_ascii_case(b"inf") {
            if self.incr_mode && rem.len() < 8 && b"infinity"[..rem.len()].eq_ignore_ascii_case(rem) {
                return Err(self.eof_err("incomplete infinity literal"));
            }
            matched_len = 3;
        } else if rem.len() >= 3 && rem[..3].eq_ignore_ascii_case(b"nan") {
            matched_len = 3;
            is_nan = true;
        } else if self.incr_mode
            && rem.len() < 8
            && (b"infinity"[..rem.len().min(8)].eq_ignore_ascii_case(rem)
                || (rem.len() < 3 && b"nan"[..rem.len()].eq_ignore_ascii_case(rem)))
        {
            return Err(self.eof_err("incomplete inf/nan literal"));
        } else {
            return Err(self.err(ReadErrorCode::InvalidNumber, "invalid number literal"));
        }
        self.pos += matched_len;
        if self.flags.contains(ReadFlags::NUMBER_AS_RAW) {
            let raw_start = if neg { start - 1 } else { start };
            let len = self.pos - raw_start;
            let ofs = self.store_slice(raw_start, self.pos);
            return Ok(Val {
                tag: make_tag(TAG_RAW, len),
                uni: ofs,
            });
        }
        let f = if is_nan {
            if neg { -f64::NAN } else { f64::NAN }
        } else if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        Ok(Val { tag: TAG_REAL, uni: f.to_bits() })
    }

    fn read_number(&mut self) -> Result<Val, ReadError> {
        let num_start = self.pos;
        let ext_num = self.flags.contains(ReadFlags::ALLOW_EXT_NUMBER);
        let allow_inf = self.flags.contains(ReadFlags::ALLOW_INF_AND_NAN);
        let num_as_raw = self.flags.contains(ReadFlags::NUMBER_AS_RAW);
        let bignum_as_raw = self.flags.contains(ReadFlags::BIGNUM_AS_RAW);

        let mut neg = false;
        if self.src()[self.pos] == b'-' {
            neg = true;
            self.pos += 1;
        } else if self.src()[self.pos] == b'+' && ext_num {
            self.pos += 1;
        }

        if self.pos >= self.src().len() {
            return Err(self.eof_err("invalid number"));
        }

        // Check -Inf / -Infinity / -NaN / +Inf / +NaN
        if allow_inf
            && matches!(self.src()[self.pos], b'I' | b'i' | b'N' | b'n')
        {
            return self.read_inf_or_nan(neg);
        }

        // Hexadecimal integer (ALLOW_EXT_NUMBER)
        if ext_num
            && self.src()[self.pos] == b'0'
            && self.pos + 1 < self.src().len()
            && (self.src()[self.pos + 1] == b'x' || self.src()[self.pos + 1] == b'X')
        {
            self.pos += 2;
            let hex_start = self.pos;
            let mut val: u64 = 0;
            let mut overflow = false;
            while self.pos < self.src().len() && is_hex(self.src()[self.pos]) {
                let d = hex_val(self.src()[self.pos]) as u64;
                if let Some(nv) = val.checked_mul(16).and_then(|v| v.checked_add(d)) {
                    val = nv;
                } else {
                    overflow = true;
                }
                self.pos += 1;
            }
            if self.pos == hex_start {
                return Err(self.err(ReadErrorCode::InvalidNumber, "invalid hex number"));
            }
            if num_as_raw || (bignum_as_raw && (overflow || (neg && val > (i64::MAX as u64) + 1))) {
                let len = self.pos - num_start;
                let ofs = self.store_slice(num_start, self.pos);
                return Ok(Val { tag: make_tag(TAG_RAW, len), uni: ofs });
            }
            if overflow {
                return Err(self.err(ReadErrorCode::InvalidNumber, "hex number overflow"));
            }
            if neg {
                if val > (i64::MAX as u64) + 1 {
                    return Ok(Val { tag: TAG_REAL, uni: (-(val as f64)).to_bits() });
                }
                let sv = if val == (i64::MAX as u64) + 1 { i64::MIN } else { -(val as i64) };
                return Ok(Val { tag: TAG_SINT, uni: sv as u64 });
            } else {
                return Ok(Val { tag: TAG_UINT, uni: val });
            }
        }

        // Parse integer part
        let int_start = self.pos;
        let mut sig: u64 = 0;
        let mut sig_digits: usize = 0;
        let mut int_overflow = false;

        if self.src()[self.pos] == b'0' {
            self.pos += 1;
            if self.pos < self.src().len() && is_digit(self.src()[self.pos]) {
                return Err(self.err(ReadErrorCode::InvalidNumber, "leading zero in number"));
            }
        } else if is_digit(self.src()[self.pos]) {
            while sig_digits <= 11 {
                if let Some(chunk) = swar_parse_8_digits(&self.src()[self.pos..]) {
                    sig = sig * 100_000_000 + chunk;
                    sig_digits += 8;
                    self.pos += 8;
                } else {
                    break;
                }
            }
            if sig_digits <= 15 {
                if let Some(chunk) = swar_parse_4_digits(&self.src()[self.pos..]) {
                    sig = sig * 10_000 + chunk as u64;
                    sig_digits += 4;
                    self.pos += 4;
                }
            }
            while self.pos < self.src().len() && is_digit(self.src()[self.pos]) {
                let d = (self.src()[self.pos] - b'0') as u64;
                if sig_digits < 19 {
                    sig = sig * 10 + d;
                    sig_digits += 1;
                } else if let Some(nv) = sig.checked_mul(10).and_then(|v| v.checked_add(d)) {
                    sig = nv;
                    sig_digits += 1;
                } else {
                    int_overflow = true;
                }
                self.pos += 1;
            }
        } else if ext_num && self.src()[self.pos] == b'.' {
            // Leading dot allowed in ext_num if followed by digit
            if self.pos + 1 >= self.src().len() || !is_digit(self.src()[self.pos + 1]) {
                return Err(self.err(ReadErrorCode::InvalidNumber, "invalid number"));
            }
        } else {
            return Err(self.err(ReadErrorCode::InvalidNumber, "invalid number"));
        }

        let mut is_float = false;
        let mut frac_digits: i32 = 0;

        // Fractional part
        if self.pos < self.src().len() && self.src()[self.pos] == b'.' {
            is_float = true;
            self.pos += 1;
            let frac_start = self.pos;
            while sig > 0 && sig_digits <= 11 {
                if let Some(chunk) = swar_parse_8_digits(&self.src()[self.pos..]) {
                    sig = sig * 100_000_000 + chunk;
                    sig_digits += 8;
                    frac_digits += 8;
                    self.pos += 8;
                } else {
                    break;
                }
            }
            if sig > 0 && sig_digits <= 15 {
                if let Some(chunk) = swar_parse_4_digits(&self.src()[self.pos..]) {
                    sig = sig * 10_000 + chunk as u64;
                    sig_digits += 4;
                    frac_digits += 4;
                    self.pos += 4;
                }
            }
            while self.pos < self.src().len() && is_digit(self.src()[self.pos]) {
                let d = (self.src()[self.pos] - b'0') as u64;
                if sig == 0 && d == 0 {
                    // leading zero in fraction after 0.
                } else if sig_digits < 19 {
                    sig = sig * 10 + d;
                    sig_digits += 1;
                }
                frac_digits += 1;
                self.pos += 1;
            }
            if self.pos == frac_start && !ext_num {
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("trailing decimal point"));
                }
                return Err(self.err(ReadErrorCode::InvalidNumber, "trailing decimal point"));
            }
        }

        // Exponent part
        let mut exp_val: i32 = 0;
        if self.pos < self.src().len() && (self.src()[self.pos] == b'e' || self.src()[self.pos] == b'E') {
            is_float = true;
            self.pos += 1;
            let mut exp_neg = false;
            if self.pos < self.src().len() {
                if self.src()[self.pos] == b'-' {
                    exp_neg = true;
                    self.pos += 1;
                } else if self.src()[self.pos] == b'+' {
                    self.pos += 1;
                }
            }
            let exp_start = self.pos;
            while self.pos < self.src().len() && is_digit(self.src()[self.pos]) {
                let d = (self.src()[self.pos] - b'0') as i32;
                if exp_val < 100_000 {
                    exp_val = exp_val * 10 + d;
                }
                self.pos += 1;
            }
            if self.pos == exp_start {
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("missing exponent digits"));
                }
                return Err(self.err(ReadErrorCode::InvalidNumber, "missing exponent digits"));
            }
            if exp_neg {
                exp_val = -exp_val;
            }
        }

        if self.incr_mode && !self.is_final && self.pos >= self.src().len() {
            return Err(self.eof_err("number may be incomplete"));
        }

        if self.pos == int_start && !is_float {
            return Err(self.err(ReadErrorCode::InvalidNumber, "invalid number"));
        }

        if num_as_raw {
            let len = self.pos - num_start;
            let ofs = self.store_slice(num_start, self.pos);
            return Ok(Val { tag: make_tag(TAG_RAW, len), uni: ofs });
        }

        if !is_float && !int_overflow {
            if neg {
                if sig <= i64::MAX as u64 {
                    return Ok(Val { tag: TAG_SINT, uni: (-(sig as i64)) as u64 });
                } else if sig == (i64::MAX as u64) + 1 {
                    return Ok(Val { tag: TAG_SINT, uni: i64::MIN as u64 });
                }
                // Negative integer < i64::MIN falls through to float or bignum
            } else {
                return Ok(Val { tag: TAG_UINT, uni: sig });
            }
        }

        if bignum_as_raw && !is_float {
            let len = self.pos - num_start;
            let ofs = self.store_slice(num_start, self.pos);
            return Ok(Val { tag: make_tag(TAG_RAW, len), uni: ofs });
        }

        let raw_slice = if self.src()[num_start] == b'+' {
            &self.src()[num_start + 1..self.pos]
        } else {
            &self.src()[num_start..self.pos]
        };

        let exp10 = exp_val.saturating_sub(frac_digits);
        let f = parse_f64_exact(sig, sig_digits, exp10, neg, raw_slice)
            .ok_or_else(|| self.err(ReadErrorCode::InvalidNumber, "invalid floating-point number"))?;

        if f.is_infinite() {
            if bignum_as_raw {
                let len = self.pos - num_start;
                let ofs = self.store_slice(num_start, self.pos);
                return Ok(Val { tag: make_tag(TAG_RAW, len), uni: ofs });
            }
            if !allow_inf {
                return Err(self.err(ReadErrorCode::InvalidNumber, "number overflow (infinity)"));
            }
        }

        Ok(Val { tag: TAG_REAL, uni: f.to_bits() })
    }
    fn parse_incremental(&mut self, phase: &mut IncrPhase) -> Result<Doc, ReadError> {
        loop {
            match *phase {
                IncrPhase::Init => {
                    if self.src().is_empty() {
                        return Err(if self.is_final {
                            ReadError::new(ReadErrorCode::InvalidParameter, "input data is empty", 0)
                        } else {
                            ReadError::new(ReadErrorCode::More, "more data required", 0)
                        });
                    }
                    if self.flags.contains(ReadFlags::ALLOW_BOM) && self.pos == 0 {
                        if self.src().len() < 3
                            && !self.is_final
                            && self.src() == &b"\xEF\xBB\xBF"[..self.src().len()]
                        {
                            return Err(ReadError::new(
                                ReadErrorCode::More,
                                "more data required",
                                0,
                            ));
                        }
                        if self.src().len() >= 3
                            && self.src()[0] == 0xEF
                            && self.src()[1] == 0xBB
                            && self.src()[2] == 0xBF
                        {
                            self.pos = 3;
                        }
                    }
                    self.skip_ws()?;
                    if self.pos >= self.src().len() {
                        return Err(if self.is_final {
                            self.err(ReadErrorCode::EmptyContent, "input JSON is empty")
                        } else {
                            self.eof_err("more data required")
                        });
                    }
                    *phase = IncrPhase::ExpectValue;
                }
                IncrPhase::ExpectValue => {
                    let snap_pos = self.pos;
                    let snap_vals = self.vals.len();
                    let snap_str = self.str_buf.len();
                    let snap_stack = self.stack.len();
                    let res = self.incr_step_value();
                    match res {
                        Ok(stay_expect) => {
                            if !stay_expect {
                                *phase = IncrPhase::AfterValue;
                            }
                        }
                        Err(e) if e.code == ReadErrorCode::More => {
                            self.pos = snap_pos;
                            self.vals.truncate(snap_vals);
                            self.str_buf.truncate(snap_str);
                            self.stack.truncate(snap_stack);
                            return Err(e);
                        }
                        Err(e) => return Err(e),
                    }
                }
                IncrPhase::AfterValue => {
                    loop {
                        let Some(top) = self.stack.last().copied() else {
                            *phase = IncrPhase::Trailer;
                            break;
                        };
                        let snap_pos = self.pos;
                        let snap_vals = self.vals.len();
                        let snap_str = self.str_buf.len();
                        let snap_stack = self.stack.len();

                        match self.incr_step_after_value() {
                            Ok(need_value) => {
                                if need_value {
                                    *phase = IncrPhase::ExpectValue;
                                    break;
                                }
                            }
                            Err(e) if e.code == ReadErrorCode::More => {
                                self.pos = snap_pos;
                                self.vals.truncate(snap_vals);
                                self.str_buf.truncate(snap_str);
                                self.stack.truncate(snap_stack);
                                if let Some(last) = self.stack.last_mut() {
                                    last.count = top.count;
                                }
                                return Err(e);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                IncrPhase::Trailer => {
                    if !self.flags.contains(ReadFlags::STOP_WHEN_DONE) {
                        self.skip_ws()?;
                        if self.pos < self.src().len() {
                            return Err(self.err(
                                ReadErrorCode::UnexpectedContent,
                                "unexpected content after document",
                            ));
                        }
                    }
                    return Ok(Doc {
                        vals: std::mem::take(&mut self.vals),
                        buf: std::mem::take(&mut self.str_buf),
                        read_size: self.pos,
                    });
                }
            }
        }
    }

    fn incr_step_value(&mut self) -> Result<bool, ReadError> {
        if self.pos >= self.src().len() {
            return Err(self.eof_err("unexpected end of input"));
        }
        let b = self.src()[self.pos];
        match b {
            b'{' => {
                if self.stack.len() >= MAX_READ_DEPTH {
                    return Err(self.err(ReadErrorCode::Depth, "depth limit exceeded"));
                }
                let idx = self.vals.len();
                self.vals.push(Val { tag: TAG_OBJ, uni: 1 });
                self.pos += 1;
                self.skip_ws()?;
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("unexpected end in object"));
                }
                if self.src()[self.pos] == b'}' {
                    self.pos += 1;
                    Ok(false)
                } else {
                    self.stack.push(CtnFrame { val_idx: idx, is_obj: true, count: 0 });
                    self.parse_obj_key()?;
                    Ok(true)
                }
            }
            b'[' => {
                if self.stack.len() >= MAX_READ_DEPTH {
                    return Err(self.err(ReadErrorCode::Depth, "depth limit exceeded"));
                }
                let idx = self.vals.len();
                self.vals.push(Val { tag: TAG_ARR, uni: 1 });
                self.pos += 1;
                self.skip_ws()?;
                if self.pos >= self.src().len() {
                    return Err(self.eof_err("unexpected end in array"));
                }
                if self.src()[self.pos] == b']' {
                    self.pos += 1;
                    Ok(false)
                } else {
                    self.stack.push(CtnFrame { val_idx: idx, is_obj: false, count: 0 });
                    Ok(true)
                }
            }
            b'"' => {
                let v = self.read_string(b'"')?;
                self.vals.push(v);
                Ok(false)
            }
            b'\'' if self.flags.contains(ReadFlags::ALLOW_SINGLE_QUOTED_STR) => {
                let v = self.read_string(b'\'')?;
                self.vals.push(v);
                Ok(false)
            }
            b't' => {
                self.read_true()?;
                self.vals.push(Val { tag: TAG_TRUE, uni: 0 });
                Ok(false)
            }
            b'f' => {
                self.read_false()?;
                self.vals.push(Val { tag: TAG_FALSE, uni: 0 });
                Ok(false)
            }
            b'n' => {
                if self.flags.contains(ReadFlags::ALLOW_INF_AND_NAN) {
                    if self.pos + 1 >= self.src().len() {
                        return Err(self.eof_err("unexpected end in literal"));
                    }
                    if self.src()[self.pos + 1] == b'a' || self.src()[self.pos + 1] == b'A' {
                        let v = self.read_inf_or_nan(false)?;
                        self.vals.push(v);
                    } else {
                        self.read_null()?;
                        self.vals.push(Val { tag: TAG_NULL, uni: 0 });
                    }
                } else {
                    self.read_null()?;
                    self.vals.push(Val { tag: TAG_NULL, uni: 0 });
                }
                Ok(false)
            }
            b'0'..=b'9' | b'-' => {
                let v = self.read_number()?;
                self.vals.push(v);
                Ok(false)
            }
            b'+' | b'.' if self.flags.contains(ReadFlags::ALLOW_EXT_NUMBER) => {
                let v = self.read_number()?;
                self.vals.push(v);
                Ok(false)
            }
            b'I' | b'i' | b'N' if self.flags.contains(ReadFlags::ALLOW_INF_AND_NAN) => {
                let v = self.read_inf_or_nan(false)?;
                self.vals.push(v);
                Ok(false)
            }
            _ => Err(self.err(ReadErrorCode::UnexpectedCharacter, "unexpected character")),
        }
    }

    fn incr_step_after_value(&mut self) -> Result<bool, ReadError> {
        let top = self.stack.last_mut().unwrap();
        top.count += 1;
        let is_obj = top.is_obj;
        let ctn_idx = top.val_idx;
        let count = top.count;

        self.skip_ws()?;
        if self.pos >= self.src().len() {
            return Err(self.eof_err("unexpected end in container"));
        }
        let sep = self.src()[self.pos];
        if sep == b',' {
            self.pos += 1;
            self.skip_ws()?;
            if self.pos >= self.src().len() {
                return Err(self.eof_err("unexpected end after comma"));
            }
            let next_b = self.src()[self.pos];
            if (is_obj && next_b == b'}') || (!is_obj && next_b == b']') {
                if self.flags.contains(ReadFlags::ALLOW_TRAILING_COMMAS) {
                    self.pos += 1;
                    self.stack.pop();
                    let ty = if is_obj { TAG_OBJ } else { TAG_ARR };
                    self.vals[ctn_idx].tag = make_tag(ty, count);
                    self.vals[ctn_idx].uni = (self.vals.len() - ctn_idx) as u64;
                    return Ok(false);
                } else {
                    return Err(self.err(
                        ReadErrorCode::JsonStructure,
                        "trailing comma is not allowed",
                    ));
                }
            }
            if is_obj {
                self.parse_obj_key()?;
            }
            Ok(true)
        } else if (is_obj && sep == b'}') || (!is_obj && sep == b']') {
            self.pos += 1;
            self.stack.pop();
            let ty = if is_obj { TAG_OBJ } else { TAG_ARR };
            self.vals[ctn_idx].tag = make_tag(ty, count);
            self.vals[ctn_idx].uni = (self.vals.len() - ctn_idx) as u64;
            Ok(false)
        } else {
            Err(self.err(
                ReadErrorCode::JsonStructure,
                "expected ',' or closing bracket",
            ))
        }
    }
}

/// Parse JSON bytes into an immutable `Doc`.
pub fn read_json(input: &[u8], flags: ReadFlags) -> Result<Doc, ReadError> {
    let mut reader = Reader::new(input, flags, false);
    reader.parse()
}

/// Parse JSON bytes in-situ (zero-copy for non-escaped strings, in-place unescaping).
pub fn read_json_insitu(data: Vec<u8>, flags: ReadFlags) -> Result<Doc, ReadError> {
    let mut reader = Reader::new_insitu(data, flags);
    reader.parse()
}

/// Returns the maximum memory usage (in bytes) to read a JSON document of `len` bytes.
pub const fn read_max_memory_usage(len: usize, flags: ReadFlags) -> usize {
    let mul = 12usize + if flags.contains(ReadFlags::INSITU) { 0 } else { 1 };
    let pad = 256usize;
    let eff_len = if flags.contains(ReadFlags::STOP_WHEN_DONE) && len < 256 { 256 } else { len };
    if eff_len >= (usize::MAX - pad - mul) / mul {
        0
    } else {
        eff_len * mul + pad
    }
}

/// Reusable pool allocator for zero-allocation repeated JSON parsing and serialization
/// (Rust safe equivalent of `yyjson_alc_pool_init`).
#[derive(Clone, Debug)]
pub struct PoolAlc {
    stack: Vec<CtnFrame>,
    out_buf: Vec<u8>,
}

impl PoolAlc {
    /// Pre-allocate buffers sized for JSON documents up to `max_json_len` bytes.
    pub fn new(max_json_len: usize) -> Self {
        Self {
            stack: Vec::with_capacity(64),
            out_buf: Vec::with_capacity(max_json_len.max(256)),
        }
    }

    /// Pre-allocate a reusable `Doc` sized for documents up to `max_json_len` bytes.
    pub fn alloc_doc(max_json_len: usize) -> Doc {
        let est_vals = (max_json_len / 2 + 1).max(16);
        Doc {
            vals: Vec::with_capacity(est_vals),
            buf: Vec::with_capacity(max_json_len),
            read_size: 0,
        }
    }

    /// Parse `input` into an existing `Doc`, reusing its `vals` and `buf` capacity with zero allocations.
    pub fn read_into(
        &mut self,
        input: &[u8],
        flags: ReadFlags,
        doc: &mut Doc,
    ) -> Result<(), ReadError> {
        doc.vals.clear();
        doc.buf.clear();
        self.stack.clear();
        let mut reader = Reader {
            src: SrcBuf::Slice(input),
            pos: 0,
            flags,
            vals: std::mem::take(&mut doc.vals),
            str_buf: std::mem::take(&mut doc.buf),
            stack: std::mem::take(&mut self.stack),
            incr_mode: false,
            is_final: true,
        };
        let res = reader.parse();
        self.stack = reader.stack;
        match res {
            Ok(parsed) => {
                *doc = parsed;
                Ok(())
            }
            Err(e) => {
                doc.vals = reader.vals;
                doc.buf = reader.str_buf;
                Err(e)
            }
        }
    }

    /// Serialize `doc` into the pool's reusable output buffer without heap allocations.
    pub fn write_opts(&mut self, doc: &Doc, flags: WriteFlags) -> Result<&[u8], WriteError> {
        let root = doc.root().ok_or_else(|| {
            WriteError::new(WriteErrorCode::InvalidParameter, "document has no root value")
        })?;
        self.out_buf.clear();
        let indent = IndentStyle::from_flags(flags);
        write_val_rec(root, flags, indent, 0, &mut self.out_buf)?;
        if flags.contains(WriteFlags::NEWLINE_AT_END) {
            self.out_buf.push(b'\n');
        }
        Ok(&self.out_buf)
    }

    /// Serialize `MutDoc` into the pool's reusable output buffer without heap allocations.
    pub fn write_mut_opts(&mut self, doc: &MutDoc, flags: WriteFlags) -> Result<&[u8], WriteError> {
        let root = doc.root.ok_or_else(|| {
            WriteError::new(WriteErrorCode::InvalidParameter, "mutable document has no root value")
        })?;
        self.out_buf.clear();
        let indent = IndentStyle::from_flags(flags);
        write_mut_val_rec(doc, root, flags, indent, 0, &mut self.out_buf)?;
        if flags.contains(WriteFlags::NEWLINE_AT_END) {
            self.out_buf.push(b'\n');
        }
        Ok(&self.out_buf)
    }
}

// ============================================================================
// Incremental JSON Reader
// ============================================================================

/// Incremental JSON reader state (O(N) total work across chunks).
///
/// Feed progressively larger slices of the input buffer to `read()` or `read_chunk()`.
/// Returns `Err(ReadError { code: ReadErrorCode::More, .. })` when more bytes are needed.
#[derive(Clone, Debug)]
pub struct IncrReader {
    flags: ReadFlags,
    pos: usize,
    vals: Vec<Val>,
    str_buf: Vec<u8>,
    stack: Vec<CtnFrame>,
    phase: IncrPhase,
    finished: bool,
}

impl IncrReader {
    pub fn new(flags: ReadFlags) -> Self {
        Self {
            flags,
            pos: 0,
            vals: Vec::new(),
            str_buf: Vec::new(),
            stack: Vec::new(),
            phase: IncrPhase::Init,
            finished: false,
        }
    }

    /// Attempt to parse `buf` incrementally (assumes more chunks may follow if a root number reaches EOF).
    pub fn read(&mut self, buf: &[u8]) -> Result<Doc, ReadError> {
        self.read_chunk(buf, false)
    }

    /// Attempt to parse `buf` incrementally, with `is_final` indicating no more bytes will follow.
    pub fn read_chunk(&mut self, buf: &[u8], is_final: bool) -> Result<Doc, ReadError> {
        if self.finished {
            return Err(ReadError::new(
                ReadErrorCode::InvalidParameter,
                "incremental reader already finished",
                0,
            ));
        }
        if buf.len() < self.pos {
            return Err(ReadError::new(
                ReadErrorCode::InvalidParameter,
                "incremental buffer shrunk below committed offset",
                self.pos,
            ));
        }
        let mut reader = Reader {
            src: SrcBuf::Slice(buf),
            pos: self.pos,
            flags: self.flags,
            vals: std::mem::take(&mut self.vals),
            str_buf: std::mem::take(&mut self.str_buf),
            stack: std::mem::take(&mut self.stack),
            incr_mode: true,
            is_final,
        };
        let res = reader.parse_incremental(&mut self.phase);
        self.pos = reader.pos;
        if res.is_ok() {
            self.finished = true;
        } else {
            self.vals = reader.vals;
            self.str_buf = reader.str_buf;
            self.stack = reader.stack;
        }
        res
    }
}

// ============================================================================
// JSON Writer (Immutable & Mutable)
// ============================================================================

const MAX_WRITE_DEPTH: usize = 1024;

const DIGIT_PAIRS: &[u8; 200] = b"\
00010203040506070809\
10111213141516171819\
20212223242526272829\
30313233343536373839\
40414243444546474849\
50515253545556575859\
60616263646566676869\
70717273747576777879\
80818283848586878889\
90919293949596979899";

#[inline]
fn write_u64(mut val: u64, out: &mut Vec<u8>) {
    if val < 10 {
        out.push(b'0' + val as u8);
        return;
    }
    if val < 100 {
        let r = (val as usize) * 2;
        out.extend_from_slice(&DIGIT_PAIRS[r..r + 2]);
        return;
    }
    let mut tmp = [0u8; 20];
    let mut pos = 20;
    while val >= 10_000 {
        let q = val / 10_000;
        let r = (val % 10_000) as usize;
        val = q;
        let hi = (r / 100) * 2;
        let lo = (r % 100) * 2;
        pos -= 4;
        tmp[pos] = DIGIT_PAIRS[hi];
        tmp[pos + 1] = DIGIT_PAIRS[hi + 1];
        tmp[pos + 2] = DIGIT_PAIRS[lo];
        tmp[pos + 3] = DIGIT_PAIRS[lo + 1];
    }
    let rem = val as usize;
    if rem >= 1000 {
        let hi = (rem / 100) * 2;
        let lo = (rem % 100) * 2;
        pos -= 4;
        tmp[pos] = DIGIT_PAIRS[hi];
        tmp[pos + 1] = DIGIT_PAIRS[hi + 1];
        tmp[pos + 2] = DIGIT_PAIRS[lo];
        tmp[pos + 3] = DIGIT_PAIRS[lo + 1];
    } else if rem >= 100 {
        let hi = rem / 100;
        let lo = (rem % 100) * 2;
        pos -= 3;
        tmp[pos] = b'0' + hi as u8;
        tmp[pos + 1] = DIGIT_PAIRS[lo];
        tmp[pos + 2] = DIGIT_PAIRS[lo + 1];
    } else if rem >= 10 {
        let r = rem * 2;
        pos -= 2;
        tmp[pos] = DIGIT_PAIRS[r];
        tmp[pos + 1] = DIGIT_PAIRS[r + 1];
    } else {
        pos -= 1;
        tmp[pos] = b'0' + rem as u8;
    }
    out.extend_from_slice(&tmp[pos..]);
}

#[inline]
fn write_i64(val: i64, out: &mut Vec<u8>) {
    if val >= 0 {
        write_u64(val as u64, out);
    } else {
        out.push(b'-');
        write_u64(val.wrapping_neg() as u64, out);
    }
}

const POW10_SIG_MIN_EXP: i32 = -343;
const POW10_SIG_TABLE: [(u64, u64); 668] = [
    (0xBF29DCABA82FDEAE, 0x7432EE873880FC33),
    (0xEEF453D6923BD65A, 0x113FAA2906A13B3F),
    (0x9558B4661B6565F8, 0x4AC7CA59A424C507),
    (0xBAAEE17FA23EBF76, 0x5D79BCF00D2DF649),
    (0xE95A99DF8ACE6F53, 0xF4D82C2C107973DC),
    (0x91D8A02BB6C10594, 0x79071B9B8A4BE869),
    (0xB64EC836A47146F9, 0x9748E2826CDEE284),
    (0xE3E27A444D8D98B7, 0xFD1B1B2308169B25),
    (0x8E6D8C6AB0787F72, 0xFE30F0F5E50E20F7),
    (0xB208EF855C969F4F, 0xBDBD2D335E51A935),
    (0xDE8B2B66B3BC4723, 0xAD2C788035E61382),
    (0x8B16FB203055AC76, 0x4C3BCB5021AFCC31),
    (0xADDCB9E83C6B1793, 0xDF4ABE242A1BBF3D),
    (0xD953E8624B85DD78, 0xD71D6DAD34A2AF0D),
    (0x87D4713D6F33AA6B, 0x8672648C40E5AD68),
    (0xA9C98D8CCB009506, 0x680EFDAF511F18C2),
    (0xD43BF0EFFDC0BA48, 0x0212BD1B2566DEF2),
    (0x84A57695FE98746D, 0x014BB630F7604B57),
    (0xA5CED43B7E3E9188, 0x419EA3BD35385E2D),
    (0xCF42894A5DCE35EA, 0x52064CAC828675B9),
    (0x818995CE7AA0E1B2, 0x7343EFEBD1940993),
    (0xA1EBFB4219491A1F, 0x1014EBE6C5F90BF8),
    (0xCA66FA129F9B60A6, 0xD41A26E077774EF6),
    (0xFD00B897478238D0, 0x8920B098955522B4),
    (0x9E20735E8CB16382, 0x55B46E5F5D5535B0),
    (0xC5A890362FDDBC62, 0xEB2189F734AA831D),
    (0xF712B443BBD52B7B, 0xA5E9EC7501D523E4),
    (0x9A6BB0AA55653B2D, 0x47B233C92125366E),
    (0xC1069CD4EABE89F8, 0x999EC0BB696E840A),
    (0xF148440A256E2C76, 0xC00670EA43CA250D),
    (0x96CD2A865764DBCA, 0x380406926A5E5728),
    (0xBC807527ED3E12BC, 0xC605083704F5ECF2),
    (0xEBA09271E88D976B, 0xF7864A44C633682E),
    (0x93445B8731587EA3, 0x7AB3EE6AFBE0211D),
    (0xB8157268FDAE9E4C, 0x5960EA05BAD82964),
    (0xE61ACF033D1A45DF, 0x6FB92487298E33BD),
    (0x8FD0C16206306BAB, 0xA5D3B6D479F8E056),
    (0xB3C4F1BA87BC8696, 0x8F48A4899877186C),
    (0xE0B62E2929ABA83C, 0x331ACDABFE94DE87),
    (0x8C71DCD9BA0B4925, 0x9FF0C08B7F1D0B14),
    (0xAF8E5410288E1B6F, 0x07ECF0AE5EE44DD9),
    (0xDB71E91432B1A24A, 0xC9E82CD9F69D6150),
    (0x892731AC9FAF056E, 0xBE311C083A225CD2),
    (0xAB70FE17C79AC6CA, 0x6DBD630A48AAF406),
    (0xD64D3D9DB981787D, 0x092CBBCCDAD5B108),
    (0x85F0468293F0EB4E, 0x25BBF56008C58EA5),
    (0xA76C582338ED2621, 0xAF2AF2B80AF6F24E),
    (0xD1476E2C07286FAA, 0x1AF5AF660DB4AEE1),
    (0x82CCA4DB847945CA, 0x50D98D9FC890ED4D),
    (0xA37FCE126597973C, 0xE50FF107BAB528A0),
    (0xCC5FC196FEFD7D0C, 0x1E53ED49A96272C8),
    (0xFF77B1FCBEBCDC4F, 0x25E8E89C13BB0F7A),
    (0x9FAACF3DF73609B1, 0x77B191618C54E9AC),
    (0xC795830D75038C1D, 0xD59DF5B9EF6A2417),
    (0xF97AE3D0D2446F25, 0x4B0573286B44AD1D),
    (0x9BECCE62836AC577, 0x4EE367F9430AEC32),
    (0xC2E801FB244576D5, 0x229C41F793CDA73F),
    (0xF3A20279ED56D48A, 0x6B43527578C1110F),
    (0x9845418C345644D6, 0x830A13896B78AAA9),
    (0xBE5691EF416BD60C, 0x23CC986BC656D553),
    (0xEDEC366B11C6CB8F, 0x2CBFBE86B7EC8AA8),
    (0x94B3A202EB1C3F39, 0x7BF7D71432F3D6A9),
    (0xB9E08A83A5E34F07, 0xDAF5CCD93FB0CC53),
    (0xE858AD248F5C22C9, 0xD1B3400F8F9CFF68),
    (0x91376C36D99995BE, 0x23100809B9C21FA1),
    (0xB58547448FFFFB2D, 0xABD40A0C2832A78A),
    (0xE2E69915B3FFF9F9, 0x16C90C8F323F516C),
    (0x8DD01FAD907FFC3B, 0xAE3DA7D97F6792E3),
    (0xB1442798F49FFB4A, 0x99CD11CFDF41779C),
    (0xDD95317F31C7FA1D, 0x40405643D711D583),
    (0x8A7D3EEF7F1CFC52, 0x482835EA666B2572),
    (0xAD1C8EAB5EE43B66, 0xDA3243650005EECF),
    (0xD863B256369D4A40, 0x90BED43E40076A82),
    (0x873E4F75E2224E68, 0x5A7744A6E804A291),
    (0xA90DE3535AAAE202, 0x711515D0A205CB36),
    (0xD3515C2831559A83, 0x0D5A5B44CA873E03),
    (0x8412D9991ED58091, 0xE858790AFE9486C2),
    (0xA5178FFF668AE0B6, 0x626E974DBE39A872),
    (0xCE5D73FF402D98E3, 0xFB0A3D212DC8128F),
    (0x80FA687F881C7F8E, 0x7CE66634BC9D0B99),
    (0xA139029F6A239F72, 0x1C1FFFC1EBC44E80),
    (0xC987434744AC874E, 0xA327FFB266B56220),
    (0xFBE9141915D7A922, 0x4BF1FF9F0062BAA8),
    (0x9D71AC8FADA6C9B5, 0x6F773FC3603DB4A9),
    (0xC4CE17B399107C22, 0xCB550FB4384D21D3),
    (0xF6019DA07F549B2B, 0x7E2A53A146606A48),
    (0x99C102844F94E0FB, 0x2EDA7444CBFC426D),
    (0xC0314325637A1939, 0xFA911155FEFB5308),
    (0xF03D93EEBC589F88, 0x793555AB7EBA27CA),
    (0x96267C7535B763B5, 0x4BC1558B2F3458DE),
    (0xBBB01B9283253CA2, 0x9EB1AAEDFB016F16),
    (0xEA9C227723EE8BCB, 0x465E15A979C1CADC),
    (0x92A1958A7675175F, 0x0BFACD89EC191EC9),
    (0xB749FAED14125D36, 0xCEF980EC671F667B),
    (0xE51C79A85916F484, 0x82B7E12780E7401A),
    (0x8F31CC0937AE58D2, 0xD1B2ECB8B0908810),
    (0xB2FE3F0B8599EF07, 0x861FA7E6DCB4AA15),
    (0xDFBDCECE67006AC9, 0x67A791E093E1D49A),
    (0x8BD6A141006042BD, 0xE0C8BB2C5C6D24E0),
    (0xAECC49914078536D, 0x58FAE9F773886E18),
    (0xDA7F5BF590966848, 0xAF39A475506A899E),
    (0x888F99797A5E012D, 0x6D8406C952429603),
    (0xAAB37FD7D8F58178, 0xC8E5087BA6D33B83),
    (0xD5605FCDCF32E1D6, 0xFB1E4A9A90880A64),
    (0x855C3BE0A17FCD26, 0x5CF2EEA09A55067F),
    (0xA6B34AD8C9DFC06F, 0xF42FAA48C0EA481E),
    (0xD0601D8EFC57B08B, 0xF13B94DAF124DA26),
    (0x823C12795DB6CE57, 0x76C53D08D6B70858),
    (0xA2CB1717B52481ED, 0x54768C4B0C64CA6E),
    (0xCB7DDCDDA26DA268, 0xA9942F5DCF7DFD09),
    (0xFE5D54150B090B02, 0xD3F93B35435D7C4C),
    (0x9EFA548D26E5A6E1, 0xC47BC5014A1A6DAF),
    (0xC6B8E9B0709F109A, 0x359AB6419CA1091B),
    (0xF867241C8CC6D4C0, 0xC30163D203C94B62),
    (0x9B407691D7FC44F8, 0x79E0DE63425DCF1D),
    (0xC21094364DFB5636, 0x985915FC12F542E4),
    (0xF294B943E17A2BC4, 0x3E6F5B7B17B2939D),
    (0x979CF3CA6CEC5B5A, 0xA705992CEECF9C42),
    (0xBD8430BD08277231, 0x50C6FF782A838353),
    (0xECE53CEC4A314EBD, 0xA4F8BF5635246428),
    (0x940F4613AE5ED136, 0x871B7795E136BE99),
    (0xB913179899F68584, 0x28E2557B59846E3F),
    (0xE757DD7EC07426E5, 0x331AEADA2FE589CF),
    (0x9096EA6F3848984F, 0x3FF0D2C85DEF7621),
    (0xB4BCA50B065ABE63, 0x0FED077A756B53A9),
    (0xE1EBCE4DC7F16DFB, 0xD3E8495912C62894),
    (0x8D3360F09CF6E4BD, 0x64712DD7ABBBD95C),
    (0xB080392CC4349DEC, 0xBD8D794D96AACFB3),
    (0xDCA04777F541C567, 0xECF0D7A0FC5583A0),
    (0x89E42CAAF9491B60, 0xF41686C49DB57244),
    (0xAC5D37D5B79B6239, 0x311C2875C522CED5),
    (0xD77485CB25823AC7, 0x7D633293366B828B),
    (0x86A8D39EF77164BC, 0xAE5DFF9C02033197),
    (0xA8530886B54DBDEB, 0xD9F57F830283FDFC),
    (0xD267CAA862A12D66, 0xD072DF63C324FD7B),
    (0x8380DEA93DA4BC60, 0x4247CB9E59F71E6D),
    (0xA46116538D0DEB78, 0x52D9BE85F074E608),
    (0xCD795BE870516656, 0x67902E276C921F8B),
    (0x806BD9714632DFF6, 0x00BA1CD8A3DB53B6),
    (0xA086CFCD97BF97F3, 0x80E8A40ECCD228A4),
    (0xC8A883C0FDAF7DF0, 0x6122CD128006B2CD),
    (0xFAD2A4B13D1B5D6C, 0x796B805720085F81),
    (0x9CC3A6EEC6311A63, 0xCBE3303674053BB0),
    (0xC3F490AA77BD60FC, 0xBEDBFC4411068A9C),
    (0xF4F1B4D515ACB93B, 0xEE92FB5515482D44),
    (0x991711052D8BF3C5, 0x751BDD152D4D1C4A),
    (0xBF5CD54678EEF0B6, 0xD262D45A78A0635D),
    (0xEF340A98172AACE4, 0x86FB897116C87C34),
    (0x9580869F0E7AAC0E, 0xD45D35E6AE3D4DA0),
    (0xBAE0A846D2195712, 0x8974836059CCA109),
    (0xE998D258869FACD7, 0x2BD1A438703FC94B),
    (0x91FF83775423CC06, 0x7B6306A34627DDCF),
    (0xB67F6455292CBF08, 0x1A3BC84C17B1D542),
    (0xE41F3D6A7377EECA, 0x20CABA5F1D9E4A93),
    (0x8E938662882AF53E, 0x547EB47B7282EE9C),
    (0xB23867FB2A35B28D, 0xE99E619A4F23AA43),
    (0xDEC681F9F4C31F31, 0x6405FA00E2EC94D4),
    (0x8B3C113C38F9F37E, 0xDE83BC408DD3DD04),
    (0xAE0B158B4738705E, 0x9624AB50B148D445),
    (0xD98DDAEE19068C76, 0x3BADD624DD9B0957),
    (0x87F8A8D4CFA417C9, 0xE54CA5D70A80E5D6),
    (0xA9F6D30A038D1DBC, 0x5E9FCF4CCD211F4C),
    (0xD47487CC8470652B, 0x7647C3200069671F),
    (0x84C8D4DFD2C63F3B, 0x29ECD9F40041E073),
    (0xA5FB0A17C777CF09, 0xF468107100525890),
    (0xCF79CC9DB955C2CC, 0x7182148D4066EEB4),
    (0x81AC1FE293D599BF, 0xC6F14CD848405530),
    (0xA21727DB38CB002F, 0xB8ADA00E5A506A7C),
    (0xCA9CF1D206FDC03B, 0xA6D90811F0E4851C),
    (0xFD442E4688BD304A, 0x908F4A166D1DA663),
    (0x9E4A9CEC15763E2E, 0x9A598E4E043287FE),
    (0xC5DD44271AD3CDBA, 0x40EFF1E1853F29FD),
    (0xF7549530E188C128, 0xD12BEE59E68EF47C),
    (0x9A94DD3E8CF578B9, 0x82BB74F8301958CE),
    (0xC13A148E3032D6E7, 0xE36A52363C1FAF01),
    (0xF18899B1BC3F8CA1, 0xDC44E6C3CB279AC1),
    (0x96F5600F15A7B7E5, 0x29AB103A5EF8C0B9),
    (0xBCB2B812DB11A5DE, 0x7415D448F6B6F0E7),
    (0xEBDF661791D60F56, 0x111B495B3464AD21),
    (0x936B9FCEBB25C995, 0xCAB10DD900BEEC34),
    (0xB84687C269EF3BFB, 0x3D5D514F40EEA742),
    (0xE65829B3046B0AFA, 0x0CB4A5A3112A5112),
    (0x8FF71A0FE2C2E6DC, 0x47F0E785EABA72AB),
    (0xB3F4E093DB73A093, 0x59ED216765690F56),
    (0xE0F218B8D25088B8, 0x306869C13EC3532C),
    (0x8C974F7383725573, 0x1E414218C73A13FB),
    (0xAFBD2350644EEACF, 0xE5D1929EF90898FA),
    (0xDBAC6C247D62A583, 0xDF45F746B74ABF39),
    (0x894BC396CE5DA772, 0x6B8BBA8C328EB783),
    (0xAB9EB47C81F5114F, 0x066EA92F3F326564),
    (0xD686619BA27255A2, 0xC80A537B0EFEFEBD),
    (0x8613FD0145877585, 0xBD06742CE95F5F36),
    (0xA798FC4196E952E7, 0x2C48113823B73704),
    (0xD17F3B51FCA3A7A0, 0xF75A15862CA504C5),
    (0x82EF85133DE648C4, 0x9A984D73DBE722FB),
    (0xA3AB66580D5FDAF5, 0xC13E60D0D2E0EBBA),
    (0xCC963FEE10B7D1B3, 0x318DF905079926A8),
    (0xFFBBCFE994E5C61F, 0xFDF17746497F7052),
    (0x9FD561F1FD0F9BD3, 0xFEB6EA8BEDEFA633),
    (0xC7CABA6E7C5382C8, 0xFE64A52EE96B8FC0),
    (0xF9BD690A1B68637B, 0x3DFDCE7AA3C673B0),
    (0x9C1661A651213E2D, 0x06BEA10CA65C084E),
    (0xC31BFA0FE5698DB8, 0x486E494FCFF30A62),
    (0xF3E2F893DEC3F126, 0x5A89DBA3C3EFCCFA),
    (0x986DDB5C6B3A76B7, 0xF89629465A75E01C),
    (0xBE89523386091465, 0xF6BBB397F1135823),
    (0xEE2BA6C0678B597F, 0x746AA07DED582E2C),
    (0x94DB483840B717EF, 0xA8C2A44EB4571CDC),
    (0xBA121A4650E4DDEB, 0x92F34D62616CE413),
    (0xE896A0D7E51E1566, 0x77B020BAF9C81D17),
    (0x915E2486EF32CD60, 0x0ACE1474DC1D122E),
    (0xB5B5ADA8AAFF80B8, 0x0D819992132456BA),
    (0xE3231912D5BF60E6, 0x10E1FFF697ED6C69),
    (0x8DF5EFABC5979C8F, 0xCA8D3FFA1EF463C1),
    (0xB1736B96B6FD83B3, 0xBD308FF8A6B17CB2),
    (0xDDD0467C64BCE4A0, 0xAC7CB3F6D05DDBDE),
    (0x8AA22C0DBEF60EE4, 0x6BCDF07A423AA96B),
    (0xAD4AB7112EB3929D, 0x86C16C98D2C953C6),
    (0xD89D64D57A607744, 0xE871C7BF077BA8B7),
    (0x87625F056C7C4A8B, 0x11471CD764AD4972),
    (0xA93AF6C6C79B5D2D, 0xD598E40D3DD89BCF),
    (0xD389B47879823479, 0x4AFF1D108D4EC2C3),
    (0x843610CB4BF160CB, 0xCEDF722A585139BA),
    (0xA54394FE1EEDB8FE, 0xC2974EB4EE658828),
    (0xCE947A3DA6A9273E, 0x733D226229FEEA32),
    (0x811CCC668829B887, 0x0806357D5A3F525F),
    (0xA163FF802A3426A8, 0xCA07C2DCB0CF26F7),
    (0xC9BCFF6034C13052, 0xFC89B393DD02F0B5),
    (0xFC2C3F3841F17C67, 0xBBAC2078D443ACE2),
    (0x9D9BA7832936EDC0, 0xD54B944B84AA4C0D),
    (0xC5029163F384A931, 0x0A9E795E65D4DF11),
    (0xF64335BCF065D37D, 0x4D4617B5FF4A16D5),
    (0x99EA0196163FA42E, 0x504BCED1BF8E4E45),
    (0xC06481FB9BCF8D39, 0xE45EC2862F71E1D6),
    (0xF07DA27A82C37088, 0x5D767327BB4E5A4C),
    (0x964E858C91BA2655, 0x3A6A07F8D510F86F),
    (0xBBE226EFB628AFEA, 0x890489F70A55368B),
    (0xEADAB0ABA3B2DBE5, 0x2B45AC74CCEA842E),
    (0x92C8AE6B464FC96F, 0x3B0B8BC90012929D),
    (0xB77ADA0617E3BBCB, 0x09CE6EBB40173744),
    (0xE55990879DDCAABD, 0xCC420A6A101D0515),
    (0x8F57FA54C2A9EAB6, 0x9FA946824A12232D),
    (0xB32DF8E9F3546564, 0x47939822DC96ABF9),
    (0xDFF9772470297EBD, 0x59787E2B93BC56F7),
    (0x8BFBEA76C619EF36, 0x57EB4EDB3C55B65A),
    (0xAEFAE51477A06B03, 0xEDE622920B6B23F1),
    (0xDAB99E59958885C4, 0xE95FAB368E45ECED),
    (0x88B402F7FD75539B, 0x11DBCB0218EBB414),
    (0xAAE103B5FCD2A881, 0xD652BDC29F26A119),
    (0xD59944A37C0752A2, 0x4BE76D3346F0495F),
    (0x857FCAE62D8493A5, 0x6F70A4400C562DDB),
    (0xA6DFBD9FB8E5B88E, 0xCB4CCD500F6BB952),
    (0xD097AD07A71F26B2, 0x7E2000A41346A7A7),
    (0x825ECC24C873782F, 0x8ED400668C0C28C8),
    (0xA2F67F2DFA90563B, 0x728900802F0F32FA),
    (0xCBB41EF979346BCA, 0x4F2B40A03AD2FFB9),
    (0xFEA126B7D78186BC, 0xE2F610C84987BFA8),
    (0x9F24B832E6B0F436, 0x0DD9CA7D2DF4D7C9),
    (0xC6EDE63FA05D3143, 0x91503D1C79720DBB),
    (0xF8A95FCF88747D94, 0x75A44C6397CE912A),
    (0x9B69DBE1B548CE7C, 0xC986AFBE3EE11ABA),
    (0xC24452DA229B021B, 0xFBE85BADCE996168),
    (0xF2D56790AB41C2A2, 0xFAE27299423FB9C3),
    (0x97C560BA6B0919A5, 0xDCCD879FC967D41A),
    (0xBDB6B8E905CB600F, 0x5400E987BBC1C920),
    (0xED246723473E3813, 0x290123E9AAB23B68),
    (0x9436C0760C86E30B, 0xF9A0B6720AAF6521),
    (0xB94470938FA89BCE, 0xF808E40E8D5B3E69),
    (0xE7958CB87392C2C2, 0xB60B1D1230B20E04),
    (0x90BD77F3483BB9B9, 0xB1C6F22B5E6F48C2),
    (0xB4ECD5F01A4AA828, 0x1E38AEB6360B1AF3),
    (0xE2280B6C20DD5232, 0x25C6DA63C38DE1B0),
    (0x8D590723948A535F, 0x579C487E5A38AD0E),
    (0xB0AF48EC79ACE837, 0x2D835A9DF0C6D851),
    (0xDCDB1B2798182244, 0xF8E431456CF88E65),
    (0x8A08F0F8BF0F156B, 0x1B8E9ECB641B58FF),
    (0xAC8B2D36EED2DAC5, 0xE272467E3D222F3F),
    (0xD7ADF884AA879177, 0x5B0ED81DCC6ABB0F),
    (0x86CCBB52EA94BAEA, 0x98E947129FC2B4E9),
    (0xA87FEA27A539E9A5, 0x3F2398D747B36224),
    (0xD29FE4B18E88640E, 0x8EEC7F0D19A03AAD),
    (0x83A3EEEEF9153E89, 0x1953CF68300424AC),
    (0xA48CEAAAB75A8E2B, 0x5FA8C3423C052DD7),
    (0xCDB02555653131B6, 0x3792F412CB06794D),
    (0x808E17555F3EBF11, 0xE2BBD88BBEE40BD0),
    (0xA0B19D2AB70E6ED6, 0x5B6ACEAEAE9D0EC4),
    (0xC8DE047564D20A8B, 0xF245825A5A445275),
    (0xFB158592BE068D2E, 0xEED6E2F0F0D56712),
    (0x9CED737BB6C4183D, 0x55464DD69685606B),
    (0xC428D05AA4751E4C, 0xAA97E14C3C26B886),
    (0xF53304714D9265DF, 0xD53DD99F4B3066A8),
    (0x993FE2C6D07B7FAB, 0xE546A8038EFE4029),
    (0xBF8FDB78849A5F96, 0xDE98520472BDD033),
    (0xEF73D256A5C0F77C, 0x963E66858F6D4440),
    (0x95A8637627989AAD, 0xDDE7001379A44AA8),
    (0xBB127C53B17EC159, 0x5560C018580D5D52),
    (0xE9D71B689DDE71AF, 0xAAB8F01E6E10B4A6),
    (0x9226712162AB070D, 0xCAB3961304CA70E8),
    (0xB6B00D69BB55C8D1, 0x3D607B97C5FD0D22),
    (0xE45C10C42A2B3B05, 0x8CB89A7DB77C506A),
    (0x8EB98A7A9A5B04E3, 0x77F3608E92ADB242),
    (0xB267ED1940F1C61C, 0x55F038B237591ED3),
    (0xDF01E85F912E37A3, 0x6B6C46DEC52F6688),
    (0x8B61313BBABCE2C6, 0x2323AC4B3B3DA015),
    (0xAE397D8AA96C1B77, 0xABEC975E0A0D081A),
    (0xD9C7DCED53C72255, 0x96E7BD358C904A21),
    (0x881CEA14545C7575, 0x7E50D64177DA2E54),
    (0xAA242499697392D2, 0xDDE50BD1D5D0B9E9),
    (0xD4AD2DBFC3D07787, 0x955E4EC64B44E864),
    (0x84EC3C97DA624AB4, 0xBD5AF13BEF0B113E),
    (0xA6274BBDD0FADD61, 0xECB1AD8AEACDD58E),
    (0xCFB11EAD453994BA, 0x67DE18EDA5814AF2),
    (0x81CEB32C4B43FCF4, 0x80EACF948770CED7),
    (0xA2425FF75E14FC31, 0xA1258379A94D028D),
    (0xCAD2F7F5359A3B3E, 0x096EE45813A04330),
    (0xFD87B5F28300CA0D, 0x8BCA9D6E188853FC),
    (0x9E74D1B791E07E48, 0x775EA264CF55347D),
    (0xC612062576589DDA, 0x95364AFE032A819D),
    (0xF79687AED3EEC551, 0x3A83DDBD83F52204),
    (0x9ABE14CD44753B52, 0xC4926A9672793542),
    (0xC16D9A0095928A27, 0x75B7053C0F178293),
    (0xF1C90080BAF72CB1, 0x5324C68B12DD6338),
    (0x971DA05074DA7BEE, 0xD3F6FC16EBCA5E03),
    (0xBCE5086492111AEA, 0x88F4BB1CA6BCF584),
    (0xEC1E4A7DB69561A5, 0x2B31E9E3D06C32E5),
    (0x9392EE8E921D5D07, 0x3AFF322E62439FCF),
    (0xB877AA3236A4B449, 0x09BEFEB9FAD487C2),
    (0xE69594BEC44DE15B, 0x4C2EBE687989A9B3),
    (0x901D7CF73AB0ACD9, 0x0F9D37014BF60A10),
    (0xB424DC35095CD80F, 0x538484C19EF38C94),
    (0xE12E13424BB40E13, 0x2865A5F206B06FB9),
    (0x8CBCCC096F5088CB, 0xF93F87B7442E45D3),
    (0xAFEBFF0BCB24AAFE, 0xF78F69A51539D748),
    (0xDBE6FECEBDEDD5BE, 0xB573440E5A884D1B),
    (0x89705F4136B4A597, 0x31680A88F8953030),
    (0xABCC77118461CEFC, 0xFDC20D2B36BA7C3D),
    (0xD6BF94D5E57A42BC, 0x3D32907604691B4C),
    (0x8637BD05AF6C69B5, 0xA63F9A49C2C1B10F),
    (0xA7C5AC471B478423, 0x0FCF80DC33721D53),
    (0xD1B71758E219652B, 0xD3C36113404EA4A8),
    (0x83126E978D4FDF3B, 0x645A1CAC083126E9),
    (0xA3D70A3D70A3D70A, 0x3D70A3D70A3D70A3),
    (0xCCCCCCCCCCCCCCCC, 0xCCCCCCCCCCCCCCCC),
    (0x8000000000000000, 0x0000000000000000),
    (0xA000000000000000, 0x0000000000000000),
    (0xC800000000000000, 0x0000000000000000),
    (0xFA00000000000000, 0x0000000000000000),
    (0x9C40000000000000, 0x0000000000000000),
    (0xC350000000000000, 0x0000000000000000),
    (0xF424000000000000, 0x0000000000000000),
    (0x9896800000000000, 0x0000000000000000),
    (0xBEBC200000000000, 0x0000000000000000),
    (0xEE6B280000000000, 0x0000000000000000),
    (0x9502F90000000000, 0x0000000000000000),
    (0xBA43B74000000000, 0x0000000000000000),
    (0xE8D4A51000000000, 0x0000000000000000),
    (0x9184E72A00000000, 0x0000000000000000),
    (0xB5E620F480000000, 0x0000000000000000),
    (0xE35FA931A0000000, 0x0000000000000000),
    (0x8E1BC9BF04000000, 0x0000000000000000),
    (0xB1A2BC2EC5000000, 0x0000000000000000),
    (0xDE0B6B3A76400000, 0x0000000000000000),
    (0x8AC7230489E80000, 0x0000000000000000),
    (0xAD78EBC5AC620000, 0x0000000000000000),
    (0xD8D726B7177A8000, 0x0000000000000000),
    (0x878678326EAC9000, 0x0000000000000000),
    (0xA968163F0A57B400, 0x0000000000000000),
    (0xD3C21BCECCEDA100, 0x0000000000000000),
    (0x84595161401484A0, 0x0000000000000000),
    (0xA56FA5B99019A5C8, 0x0000000000000000),
    (0xCECB8F27F4200F3A, 0x0000000000000000),
    (0x813F3978F8940984, 0x4000000000000000),
    (0xA18F07D736B90BE5, 0x5000000000000000),
    (0xC9F2C9CD04674EDE, 0xA400000000000000),
    (0xFC6F7C4045812296, 0x4D00000000000000),
    (0x9DC5ADA82B70B59D, 0xF020000000000000),
    (0xC5371912364CE305, 0x6C28000000000000),
    (0xF684DF56C3E01BC6, 0xC732000000000000),
    (0x9A130B963A6C115C, 0x3C7F400000000000),
    (0xC097CE7BC90715B3, 0x4B9F100000000000),
    (0xF0BDC21ABB48DB20, 0x1E86D40000000000),
    (0x96769950B50D88F4, 0x1314448000000000),
    (0xBC143FA4E250EB31, 0x17D955A000000000),
    (0xEB194F8E1AE525FD, 0x5DCFAB0800000000),
    (0x92EFD1B8D0CF37BE, 0x5AA1CAE500000000),
    (0xB7ABC627050305AD, 0xF14A3D9E40000000),
    (0xE596B7B0C643C719, 0x6D9CCD05D0000000),
    (0x8F7E32CE7BEA5C6F, 0xE4820023A2000000),
    (0xB35DBF821AE4F38B, 0xDDA2802C8A800000),
    (0xE0352F62A19E306E, 0xD50B2037AD200000),
    (0x8C213D9DA502DE45, 0x4526F422CC340000),
    (0xAF298D050E4395D6, 0x9670B12B7F410000),
    (0xDAF3F04651D47B4C, 0x3C0CDD765F114000),
    (0x88D8762BF324CD0F, 0xA5880A69FB6AC800),
    (0xAB0E93B6EFEE0053, 0x8EEA0D047A457A00),
    (0xD5D238A4ABE98068, 0x72A4904598D6D880),
    (0x85A36366EB71F041, 0x47A6DA2B7F864750),
    (0xA70C3C40A64E6C51, 0x999090B65F67D924),
    (0xD0CF4B50CFE20765, 0xFFF4B4E3F741CF6D),
    (0x82818F1281ED449F, 0xBFF8F10E7A8921A4),
    (0xA321F2D7226895C7, 0xAFF72D52192B6A0D),
    (0xCBEA6F8CEB02BB39, 0x9BF4F8A69F764490),
    (0xFEE50B7025C36A08, 0x02F236D04753D5B4),
    (0x9F4F2726179A2245, 0x01D762422C946590),
    (0xC722F0EF9D80AAD6, 0x424D3AD2B7B97EF5),
    (0xF8EBAD2B84E0D58B, 0xD2E0898765A7DEB2),
    (0x9B934C3B330C8577, 0x63CC55F49F88EB2F),
    (0xC2781F49FFCFA6D5, 0x3CBF6B71C76B25FB),
    (0xF316271C7FC3908A, 0x8BEF464E3945EF7A),
    (0x97EDD871CFDA3A56, 0x97758BF0E3CBB5AC),
    (0xBDE94E8E43D0C8EC, 0x3D52EEED1CBEA317),
    (0xED63A231D4C4FB27, 0x4CA7AAA863EE4BDD),
    (0x945E455F24FB1CF8, 0x8FE8CAA93E74EF6A),
    (0xB975D6B6EE39E436, 0xB3E2FD538E122B44),
    (0xE7D34C64A9C85D44, 0x60DBBCA87196B616),
    (0x90E40FBEEA1D3A4A, 0xBC8955E946FE31CD),
    (0xB51D13AEA4A488DD, 0x6BABAB6398BDBE41),
    (0xE264589A4DCDAB14, 0xC696963C7EED2DD1),
    (0x8D7EB76070A08AEC, 0xFC1E1DE5CF543CA2),
    (0xB0DE65388CC8ADA8, 0x3B25A55F43294BCB),
    (0xDD15FE86AFFAD912, 0x49EF0EB713F39EBE),
    (0x8A2DBF142DFCC7AB, 0x6E3569326C784337),
    (0xACB92ED9397BF996, 0x49C2C37F07965404),
    (0xD7E77A8F87DAF7FB, 0xDC33745EC97BE906),
    (0x86F0AC99B4E8DAFD, 0x69A028BB3DED71A3),
    (0xA8ACD7C0222311BC, 0xC40832EA0D68CE0C),
    (0xD2D80DB02AABD62B, 0xF50A3FA490C30190),
    (0x83C7088E1AAB65DB, 0x792667C6DA79E0FA),
    (0xA4B8CAB1A1563F52, 0x577001B891185938),
    (0xCDE6FD5E09ABCF26, 0xED4C0226B55E6F86),
    (0x80B05E5AC60B6178, 0x544F8158315B05B4),
    (0xA0DC75F1778E39D6, 0x696361AE3DB1C721),
    (0xC913936DD571C84C, 0x03BC3A19CD1E38E9),
    (0xFB5878494ACE3A5F, 0x04AB48A04065C723),
    (0x9D174B2DCEC0E47B, 0x62EB0D64283F9C76),
    (0xC45D1DF942711D9A, 0x3BA5D0BD324F8394),
    (0xF5746577930D6500, 0xCA8F44EC7EE36479),
    (0x9968BF6ABBE85F20, 0x7E998B13CF4E1ECB),
    (0xBFC2EF456AE276E8, 0x9E3FEDD8C321A67E),
    (0xEFB3AB16C59B14A2, 0xC5CFE94EF3EA101E),
    (0x95D04AEE3B80ECE5, 0xBBA1F1D158724A12),
    (0xBB445DA9CA61281F, 0x2A8A6E45AE8EDC97),
    (0xEA1575143CF97226, 0xF52D09D71A3293BD),
    (0x924D692CA61BE758, 0x593C2626705F9C56),
    (0xB6E0C377CFA2E12E, 0x6F8B2FB00C77836C),
    (0xE498F455C38B997A, 0x0B6DFB9C0F956447),
    (0x8EDF98B59A373FEC, 0x4724BD4189BD5EAC),
    (0xB2977EE300C50FE7, 0x58EDEC91EC2CB657),
    (0xDF3D5E9BC0F653E1, 0x2F2967B66737E3ED),
    (0x8B865B215899F46C, 0xBD79E0D20082EE74),
    (0xAE67F1E9AEC07187, 0xECD8590680A3AA11),
    (0xDA01EE641A708DE9, 0xE80E6F4820CC9495),
    (0x884134FE908658B2, 0x3109058D147FDCDD),
    (0xAA51823E34A7EEDE, 0xBD4B46F0599FD415),
    (0xD4E5E2CDC1D1EA96, 0x6C9E18AC7007C91A),
    (0x850FADC09923329E, 0x03E2CF6BC604DDB0),
    (0xA6539930BF6BFF45, 0x84DB8346B786151C),
    (0xCFE87F7CEF46FF16, 0xE612641865679A63),
    (0x81F14FAE158C5F6E, 0x4FCB7E8F3F60C07E),
    (0xA26DA3999AEF7749, 0xE3BE5E330F38F09D),
    (0xCB090C8001AB551C, 0x5CADF5BFD3072CC5),
    (0xFDCB4FA002162A63, 0x73D9732FC7C8F7F6),
    (0x9E9F11C4014DDA7E, 0x2867E7FDDCDD9AFA),
    (0xC646D63501A1511D, 0xB281E1FD541501B8),
    (0xF7D88BC24209A565, 0x1F225A7CA91A4226),
    (0x9AE757596946075F, 0x3375788DE9B06958),
    (0xC1A12D2FC3978937, 0x0052D6B1641C83AE),
    (0xF209787BB47D6B84, 0xC0678C5DBD23A49A),
    (0x9745EB4D50CE6332, 0xF840B7BA963646E0),
    (0xBD176620A501FBFF, 0xB650E5A93BC3D898),
    (0xEC5D3FA8CE427AFF, 0xA3E51F138AB4CEBE),
    (0x93BA47C980E98CDF, 0xC66F336C36B10137),
    (0xB8A8D9BBE123F017, 0xB80B0047445D4184),
    (0xE6D3102AD96CEC1D, 0xA60DC059157491E5),
    (0x9043EA1AC7E41392, 0x87C89837AD68DB2F),
    (0xB454E4A179DD1877, 0x29BABE4598C311FB),
    (0xE16A1DC9D8545E94, 0xF4296DD6FEF3D67A),
    (0x8CE2529E2734BB1D, 0x1899E4A65F58660C),
    (0xB01AE745B101E9E4, 0x5EC05DCFF72E7F8F),
    (0xDC21A1171D42645D, 0x76707543F4FA1F73),
    (0x899504AE72497EBA, 0x6A06494A791C53A8),
    (0xABFA45DA0EDBDE69, 0x0487DB9D17636892),
    (0xD6F8D7509292D603, 0x45A9D2845D3C42B6),
    (0x865B86925B9BC5C2, 0x0B8A2392BA45A9B2),
    (0xA7F26836F282B732, 0x8E6CAC7768D7141E),
    (0xD1EF0244AF2364FF, 0x3207D795430CD926),
    (0x8335616AED761F1F, 0x7F44E6BD49E807B8),
    (0xA402B9C5A8D3A6E7, 0x5F16206C9C6209A6),
    (0xCD036837130890A1, 0x36DBA887C37A8C0F),
    (0x802221226BE55A64, 0xC2494954DA2C9789),
    (0xA02AA96B06DEB0FD, 0xF2DB9BAA10B7BD6C),
    (0xC83553C5C8965D3D, 0x6F92829494E5ACC7),
    (0xFA42A8B73ABBF48C, 0xCB772339BA1F17F9),
    (0x9C69A97284B578D7, 0xFF2A760414536EFB),
    (0xC38413CF25E2D70D, 0xFEF5138519684ABA),
    (0xF46518C2EF5B8CD1, 0x7EB258665FC25D69),
    (0x98BF2F79D5993802, 0xEF2F773FFBD97A61),
    (0xBEEEFB584AFF8603, 0xAAFB550FFACFD8FA),
    (0xEEAABA2E5DBF6784, 0x95BA2A53F983CF38),
    (0x952AB45CFA97A0B2, 0xDD945A747BF26183),
    (0xBA756174393D88DF, 0x94F971119AEEF9E4),
    (0xE912B9D1478CEB17, 0x7A37CD5601AAB85D),
    (0x91ABB422CCB812EE, 0xAC62E055C10AB33A),
    (0xB616A12B7FE617AA, 0x577B986B314D6009),
    (0xE39C49765FDF9D94, 0xED5A7E85FDA0B80B),
    (0x8E41ADE9FBEBC27D, 0x14588F13BE847307),
    (0xB1D219647AE6B31C, 0x596EB2D8AE258FC8),
    (0xDE469FBD99A05FE3, 0x6FCA5F8ED9AEF3BB),
    (0x8AEC23D680043BEE, 0x25DE7BB9480D5854),
    (0xADA72CCC20054AE9, 0xAF561AA79A10AE6A),
    (0xD910F7FF28069DA4, 0x1B2BA1518094DA04),
    (0x87AA9AFF79042286, 0x90FB44D2F05D0842),
    (0xA99541BF57452B28, 0x353A1607AC744A53),
    (0xD3FA922F2D1675F2, 0x42889B8997915CE8),
    (0x847C9B5D7C2E09B7, 0x69956135FEBADA11),
    (0xA59BC234DB398C25, 0x43FAB9837E699095),
    (0xCF02B2C21207EF2E, 0x94F967E45E03F4BB),
    (0x8161AFB94B44F57D, 0x1D1BE0EEBAC278F5),
    (0xA1BA1BA79E1632DC, 0x6462D92A69731732),
    (0xCA28A291859BBF93, 0x7D7B8F7503CFDCFE),
    (0xFCB2CB35E702AF78, 0x5CDA735244C3D43E),
    (0x9DEFBF01B061ADAB, 0x3A0888136AFA64A7),
    (0xC56BAEC21C7A1916, 0x088AAA1845B8FDD0),
    (0xF6C69A72A3989F5B, 0x8AAD549E57273D45),
    (0x9A3C2087A63F6399, 0x36AC54E2F678864B),
    (0xC0CB28A98FCF3C7F, 0x84576A1BB416A7DD),
    (0xF0FDF2D3F3C30B9F, 0x656D44A2A11C51D5),
    (0x969EB7C47859E743, 0x9F644AE5A4B1B325),
    (0xBC4665B596706114, 0x873D5D9F0DDE1FEE),
    (0xEB57FF22FC0C7959, 0xA90CB506D155A7EA),
    (0x9316FF75DD87CBD8, 0x09A7F12442D588F2),
    (0xB7DCBF5354E9BECE, 0x0C11ED6D538AEB2F),
    (0xE5D3EF282A242E81, 0x8F1668C8A86DA5FA),
    (0x8FA475791A569D10, 0xF96E017D694487BC),
    (0xB38D92D760EC4455, 0x37C981DCC395A9AC),
    (0xE070F78D3927556A, 0x85BBE253F47B1417),
    (0x8C469AB843B89562, 0x93956D7478CCEC8E),
    (0xAF58416654A6BABB, 0x387AC8D1970027B2),
    (0xDB2E51BFE9D0696A, 0x06997B05FCC0319E),
    (0x88FCF317F22241E2, 0x441FECE3BDF81F03),
    (0xAB3C2FDDEEAAD25A, 0xD527E81CAD7626C3),
    (0xD60B3BD56A5586F1, 0x8A71E223D8D3B074),
    (0x85C7056562757456, 0xF6872D5667844E49),
    (0xA738C6BEBB12D16C, 0xB428F8AC016561DB),
    (0xD106F86E69D785C7, 0xE13336D701BEBA52),
    (0x82A45B450226B39C, 0xECC0024661173473),
    (0xA34D721642B06084, 0x27F002D7F95D0190),
    (0xCC20CE9BD35C78A5, 0x31EC038DF7B441F4),
    (0xFF290242C83396CE, 0x7E67047175A15271),
    (0x9F79A169BD203E41, 0x0F0062C6E984D386),
    (0xC75809C42C684DD1, 0x52C07B78A3E60868),
    (0xF92E0C3537826145, 0xA7709A56CCDF8A82),
    (0x9BBCC7A142B17CCB, 0x88A66076400BB691),
    (0xC2ABF989935DDBFE, 0x6ACFF893D00EA435),
    (0xF356F7EBF83552FE, 0x0583F6B8C4124D43),
    (0x98165AF37B2153DE, 0xC3727A337A8B704A),
    (0xBE1BF1B059E9A8D6, 0x744F18C0592E4C5C),
    (0xEDA2EE1C7064130C, 0x1162DEF06F79DF73),
    (0x9485D4D1C63E8BE7, 0x8ADDCB5645AC2BA8),
    (0xB9A74A0637CE2EE1, 0x6D953E2BD7173692),
    (0xE8111C87C5C1BA99, 0xC8FA8DB6CCDD0437),
    (0x910AB1D4DB9914A0, 0x1D9C9892400A22A2),
    (0xB54D5E4A127F59C8, 0x2503BEB6D00CAB4B),
    (0xE2A0B5DC971F303A, 0x2E44AE64840FD61D),
    (0x8DA471A9DE737E24, 0x5CEAECFED289E5D2),
    (0xB10D8E1456105DAD, 0x7425A83E872C5F47),
    (0xDD50F1996B947518, 0xD12F124E28F77719),
    (0x8A5296FFE33CC92F, 0x82BD6B70D99AAA6F),
    (0xACE73CBFDC0BFB7B, 0x636CC64D1001550B),
    (0xD8210BEFD30EFA5A, 0x3C47F7E05401AA4E),
    (0x8714A775E3E95C78, 0x65ACFAEC34810A71),
    (0xA8D9D1535CE3B396, 0x7F1839A741A14D0D),
    (0xD31045A8341CA07C, 0x1EDE48111209A050),
    (0x83EA2B892091E44D, 0x934AED0AAB460432),
    (0xA4E4B66B68B65D60, 0xF81DA84D5617853F),
    (0xCE1DE40642E3F4B9, 0x36251260AB9D668E),
    (0x80D2AE83E9CE78F3, 0xC1D72B7C6B426019),
    (0xA1075A24E4421730, 0xB24CF65B8612F81F),
    (0xC94930AE1D529CFC, 0xDEE033F26797B627),
    (0xFB9B7CD9A4A7443C, 0x169840EF017DA3B1),
    (0x9D412E0806E88AA5, 0x8E1F289560EE864E),
    (0xC491798A08A2AD4E, 0xF1A6F2BAB92A27E2),
    (0xF5B5D7EC8ACB58A2, 0xAE10AF696774B1DB),
    (0x9991A6F3D6BF1765, 0xACCA6DA1E0A8EF29),
    (0xBFF610B0CC6EDD3F, 0x17FD090A58D32AF3),
    (0xEFF394DCFF8A948E, 0xDDFC4B4CEF07F5B0),
    (0x95F83D0A1FB69CD9, 0x4ABDAF101564F98E),
    (0xBB764C4CA7A4440F, 0x9D6D1AD41ABE37F1),
    (0xEA53DF5FD18D5513, 0x84C86189216DC5ED),
    (0x92746B9BE2F8552C, 0x32FD3CF5B4E49BB4),
    (0xB7118682DBB66A77, 0x3FBC8C33221DC2A1),
    (0xE4D5E82392A40515, 0x0FABAF3FEAA5334A),
    (0x8F05B1163BA6832D, 0x29CB4D87F2A7400E),
    (0xB2C71D5BCA9023F8, 0x743E20E9EF511012),
    (0xDF78E4B2BD342CF6, 0x914DA9246B255416),
    (0x8BAB8EEFB6409C1A, 0x1AD089B6C2F7548E),
    (0xAE9672ABA3D0C320, 0xA184AC2473B529B1),
    (0xDA3C0F568CC4F3E8, 0xC9E5D72D90A2741E),
    (0x8865899617FB1871, 0x7E2FA67C7A658892),
    (0xAA7EEBFB9DF9DE8D, 0xDDBB901B98FEEAB7),
    (0xD51EA6FA85785631, 0x552A74227F3EA565),
    (0x8533285C936B35DE, 0xD53A88958F87275F),
    (0xA67FF273B8460356, 0x8A892ABAF368F137),
    (0xD01FEF10A657842C, 0x2D2B7569B0432D85),
    (0x8213F56A67F6B29B, 0x9C3B29620E29FC73),
    (0xA298F2C501F45F42, 0x8349F3BA91B47B8F),
    (0xCB3F2F7642717713, 0x241C70A936219A73),
    (0xFE0EFB53D30DD4D7, 0xED238CD383AA0110),
    (0x9EC95D1463E8A506, 0xF4363804324A40AA),
    (0xC67BB4597CE2CE48, 0xB143C6053EDCD0D5),
    (0xF81AA16FDC1B81DA, 0xDD94B7868E94050A),
    (0x9B10A4E5E9913128, 0xCA7CF2B4191C8326),
    (0xC1D4CE1F63F57D72, 0xFD1C2F611F63A3F0),
    (0xF24A01A73CF2DCCF, 0xBC633B39673C8CEC),
    (0x976E41088617CA01, 0xD5BE0503E085D813),
    (0xBD49D14AA79DBC82, 0x4B2D8644D8A74E18),
    (0xEC9C459D51852BA2, 0xDDF8E7D60ED1219E),
    (0x93E1AB8252F33B45, 0xCABB90E5C942B503),
    (0xB8DA1662E7B00A17, 0x3D6A751F3B936243),
    (0xE7109BFBA19C0C9D, 0x0CC512670A783AD4),
    (0x906A617D450187E2, 0x27FB2B80668B24C5),
    (0xB484F9DC9641E9DA, 0xB1F9F660802DEDF6),
    (0xE1A63853BBD26451, 0x5E7873F8A0396973),
    (0x8D07E33455637EB2, 0xDB0B487B6423E1E8),
    (0xB049DC016ABC5E5F, 0x91CE1A9A3D2CDA62),
    (0xDC5C5301C56B75F7, 0x7641A140CC7810FB),
    (0x89B9B3E11B6329BA, 0xA9E904C87FCB0A9D),
    (0xAC2820D9623BF429, 0x546345FA9FBDCD44),
    (0xD732290FBACAF133, 0xA97C177947AD4095),
    (0x867F59A9D4BED6C0, 0x49ED8EABCCCC485D),
    (0xA81F301449EE8C70, 0x5C68F256BFFF5A74),
    (0xD226FC195C6A2F8C, 0x73832EEC6FFF3111),
    (0x83585D8FD9C25DB7, 0xC831FD53C5FF7EAB),
    (0xA42E74F3D032F525, 0xBA3E7CA8B77F5E55),
    (0xCD3A1230C43FB26F, 0x28CE1BD2E55F35EB),
    (0x80444B5E7AA7CF85, 0x7980D163CF5B81B3),
    (0xA0555E361951C366, 0xD7E105BCC332621F),
    (0xC86AB5C39FA63440, 0x8DD9472BF3FEFAA7),
    (0xFA856334878FC150, 0xB14F98F6F0FEB951),
    (0x9C935E00D4B9D8D2, 0x6ED1BF9A569F33D3),
    (0xC3B8358109E84F07, 0x0A862F80EC4700C8),
    (0xF4A642E14C6262C8, 0xCD27BB612758C0FA),
    (0x98E7E9CCCFBD7DBD, 0x8038D51CB897789C),
    (0xBF21E44003ACDD2C, 0xE0470A63E6BD56C3),
    (0xEEEA5D5004981478, 0x1858CCFCE06CAC74),
    (0x95527A5202DF0CCB, 0x0F37801E0C43EBC8),
    (0xBAA718E68396CFFD, 0xD30560258F54E6BA),
    (0xE950DF20247C83FD, 0x47C6B82EF32A2069),
    (0x91D28B7416CDD27E, 0x4CDC331D57FA5441),
    (0xB6472E511C81471D, 0xE0133FE4ADF8E952),
    (0xE3D8F9E563A198E5, 0x58180FDDD97723A6),
    (0x8E679C2F5E44FF8F, 0x570F09EAA7EA7648),
    (0xB201833B35D63F73, 0x2CD2CC6551E513DA),
    (0xDE81E40A034BCF4F, 0xF8077F7EA65E58D1),
    (0x8B112E86420F6191, 0xFB04AFAF27FAF782),
    (0xADD57A27D29339F6, 0x79C5DB9AF1F9B563),
    (0xD94AD8B1C7380874, 0x18375281AE7822BC),
    (0x87CEC76F1C830548, 0x8F2293910D0B15B5),
    (0xA9C2794AE3A3C69A, 0xB2EB3875504DDB22),
    (0xD433179D9C8CB841, 0x5FA60692A46151EB),
    (0x849FEEC281D7F328, 0xDBC7C41BA6BCD333),
    (0xA5C7EA73224DEFF3, 0x12B9B522906C0800),
    (0xCF39E50FEAE16BEF, 0xD768226B34870A00),
    (0x81842F29F2CCE375, 0xE6A1158300D46640),
    (0xA1E53AF46F801C53, 0x60495AE3C1097FD0),
    (0xCA5E89B18B602368, 0x385BB19CB14BDFC4),
    (0xFCF62C1DEE382C42, 0x46729E03DD9ED7B5),
    (0x9E19DB92B4E31BA9, 0x6C07A2C26A8346D1),
];
fn write_f64(val: f64, flags: WriteFlags, out: &mut Vec<u8>) -> Result<(), WriteError> {
    if !val.is_finite() {
        if flags.contains(WriteFlags::INF_AND_NAN_AS_NULL) {
            out.extend_from_slice(b"null");
            return Ok(());
        }
        if flags.contains(WriteFlags::ALLOW_INF_AND_NAN) {
            if val.is_nan() {
                out.extend_from_slice(b"NaN");
            } else if val.is_sign_negative() {
                out.extend_from_slice(b"-Infinity");
            } else {
                out.extend_from_slice(b"Infinity");
            }
            return Ok(());
        }
        return Err(WriteError::new(
            WriteErrorCode::NanOrInf,
            "NaN or Infinity cannot be serialized in standard JSON",
        ));
    }

    let fixed_prec = flags.fp_fixed_prec();
    if fixed_prec > 0 {
        let start = out.len();
        let _ = write!(out, "{:.*}", fixed_prec as usize, val);
        if out[start..].contains(&b'.') {
            while out.len() > start && *out.last().unwrap() == b'0' {
                out.pop();
            }
            if out.len() > start && *out.last().unwrap() == b'.' {
                out.pop();
            }
        }
        return Ok(());
    }

    if flags.contains(WriteFlags::FP_TO_FLOAT) {
        let f = val as f32;
        if !f.is_finite() {
            if flags.contains(WriteFlags::INF_AND_NAN_AS_NULL) {
                out.extend_from_slice(b"null");
                return Ok(());
            }
            if flags.contains(WriteFlags::ALLOW_INF_AND_NAN) {
                if f.is_nan() {
                    out.extend_from_slice(b"NaN");
                } else if f.is_sign_negative() {
                    out.extend_from_slice(b"-Infinity");
                } else {
                    out.extend_from_slice(b"Infinity");
                }
                return Ok(());
            }
            return Err(WriteError::new(
                WriteErrorCode::NanOrInf,
                "float overflow to Infinity",
            ));
        }
        let start = out.len();
        let _ = write!(out, "{}", f);
        if !out[start..].iter().any(|&b| b == b'.' || b == b'e' || b == b'E') {
            out.extend_from_slice(b".0");
        }
        return Ok(());
    }

    // Shortest round-trip finite f64 via Schubfach algorithm (zero std::fmt, stack-only)
    write_f64_schubfach(val, out);
    Ok(())
}

#[inline(always)]
fn pow10_table_get_sig(exp10: i32) -> (u64, u64) {
    let idx = (exp10 - POW10_SIG_MIN_EXP) as usize;
    POW10_SIG_TABLE[idx]
}

#[inline(always)]
fn u128_round_to_odd(hi: u64, lo: u64, cp: u64) -> u64 {
    let x_hi = ((cp as u128) * (lo as u128)) >> 64;
    let y = (cp as u128) * (hi as u128) + x_hi;
    let y_hi = (y >> 64) as u64;
    let y_lo = y as u64;
    y_hi | u64::from(y_lo > 1)
}

#[inline]
fn f64_bin_to_dec(sig_raw: u64, exp_raw: u32, sig_bin: u64, exp_bin: i32) -> (u64, i32) {
    // Fast path for regular spacing significand
    if sig_raw != 0 {
        let k = (exp_bin.wrapping_mul(315653)) >> 20;
        let h = exp_bin + (((-k).wrapping_mul(217707)) >> 16);
        let (p10_hi, p10_lo) = pow10_table_get_sig(-k);

        let cb = sig_bin << (h + 1);
        let x_hi = ((cb as u128) * (p10_lo as u128)) >> 64;
        let s_full = (cb as u128) * (p10_hi as u128) + x_hi;
        let s_hi = (s_full >> 64) as u64;
        let s_lo = s_full as u64;

        let mod10 = s_hi % 10;
        let dec = s_hi - mod10;
        let c = (mod10 << 60) | (s_lo >> 4);
        let half_ulp = p10_hi >> (4 - h);

        let w1_inside = s_lo >= (1u64 << 63);
        let u0_inside = half_ulp >= c;
        let t0 = 10u64 << 60;
        let t1 = c + half_ulp;
        let w0_inside = t1 >= t0;

        if s_lo != (1u64 << 63) && half_ulp != c && t0.wrapping_sub(t1) > 1 {
            let trim = u0_inside | w0_inside;
            let add_10 = if w0_inside { 10 } else { 0 };
            let add_1 = mod10 + u64::from(w1_inside);
            return (dec + if trim { add_10 } else { add_1 }, k);
        }
    }

    // Full Schubfach algorithm
    let irregular = sig_raw == 0 && exp_raw > 1;
    let is_even = (sig_bin & 1) == 0;
    let cbl = 4 * sig_bin - 2 + u64::from(irregular);
    let cb = 4 * sig_bin;
    let cbr = 4 * sig_bin + 2;

    let k = (exp_bin.wrapping_mul(315653) - if irregular { 131237 } else { 0 }) >> 20;
    let h = exp_bin + (((-k).wrapping_mul(217707)) >> 16) + 1;
    let (p10_hi, mut p10_lo) = pow10_table_get_sig(-k);
    p10_lo = p10_lo.wrapping_add(1);

    let vbl = u128_round_to_odd(p10_hi, p10_lo, cbl << h);
    let vb = u128_round_to_odd(p10_hi, p10_lo, cb << h);
    let vbr = u128_round_to_odd(p10_hi, p10_lo, cbr << h);
    let lower = vbl + u64::from(!is_even);
    let upper = vbr - u64::from(!is_even);

    let s = vb / 4;
    if s >= 10 {
        let sp = s / 10;
        let u0_inside = lower <= 40 * sp;
        let w0_inside = upper >= 40 * sp + 40;
        if u0_inside != w0_inside {
            return (sp * 10 + if w0_inside { 10 } else { 0 }, k);
        }
    }
    let u1_inside = lower <= 4 * s;
    let w1_inside = upper >= 4 * s + 4;
    let mid = 4 * s + 2;
    let round_up = (vb > mid) || (vb == mid && (s & 1) != 0);
    let add = if u1_inside != w1_inside {
        u64::from(w1_inside)
    } else {
        u64::from(round_up)
    };
    (s + add, k)
}

#[inline]
fn write_f64_schubfach(val: f64, out: &mut Vec<u8>) {
    const F64_SIG_BITS: i32 = 52;
    const F64_EXP_BIAS: i32 = 1023;
    const F64_SIG_MASK: u64 = (1u64 << 52) - 1;

    let raw = val.to_bits();
    let sign = (raw >> 63) != 0;
    let sig_raw = raw & F64_SIG_MASK;
    let exp_raw = ((raw >> 52) & 0x7FF) as u32;

    if sign {
        out.push(b'-');
    }
    if (raw << 1) == 0 {
        out.extend_from_slice(b"0.0");
        return;
    }

    let (sig_dec, exp_dec) = if exp_raw != 0 {
        let sig_bin = sig_raw | (1u64 << 52);
        let exp_bin = (exp_raw as i32) - F64_EXP_BIAS - F64_SIG_BITS;
        if (-F64_SIG_BITS..=0).contains(&exp_bin)
            && sig_bin.trailing_zeros() >= (-exp_bin) as u32
        {
            write_u64(sig_bin >> (-exp_bin), out);
            out.extend_from_slice(b".0");
            return;
        }
        f64_bin_to_dec(sig_raw, exp_raw, sig_bin, exp_bin)
    } else {
        let sig_bin = sig_raw;
        let exp_bin = 1 - F64_EXP_BIAS - F64_SIG_BITS;
        f64_bin_to_dec(sig_raw, exp_raw, sig_bin, exp_bin)
    };

    // Format sig_dec digits into stack buffer [u8; 20]
    let mut digits = [0u8; 20];
    let mut dpos = 20usize;
    let mut v = sig_dec;
    while v >= 100 {
        let q = v / 100;
        let r = ((v % 100) as usize) * 2;
        v = q;
        dpos -= 2;
        digits[dpos] = DIGIT_PAIRS[r];
        digits[dpos + 1] = DIGIT_PAIRS[r + 1];
    }
    if v >= 10 {
        let r = (v as usize) * 2;
        dpos -= 2;
        digits[dpos] = DIGIT_PAIRS[r];
        digits[dpos + 1] = DIGIT_PAIRS[r + 1];
    } else {
        dpos -= 1;
        digits[dpos] = b'0' + (v as u8);
    }

    let sig_slice = &digits[dpos..];
    let sig_len = sig_slice.len();
    let mut trim_len = sig_len;
    while trim_len > 1 && sig_slice[trim_len - 1] == b'0' {
        trim_len -= 1;
    }
    let dot_ofs = (sig_len as i32) + exp_dec;

    if (-5..=21).contains(&dot_ofs) {
        if dot_ofs <= 0 {
            out.extend_from_slice(b"0.");
            let zeros = (-dot_ofs) as usize;
            let old = out.len();
            out.resize(old + zeros, b'0');
            out.extend_from_slice(&sig_slice[..trim_len]);
        } else if (dot_ofs as usize) >= trim_len {
            out.extend_from_slice(&sig_slice[..trim_len]);
            let zeros = (dot_ofs as usize) - trim_len;
            let old = out.len();
            out.resize(old + zeros, b'0');
            out.extend_from_slice(b".0");
        } else {
            let d = dot_ofs as usize;
            out.extend_from_slice(&sig_slice[..d]);
            out.push(b'.');
            out.extend_from_slice(&sig_slice[d..trim_len]);
        }
    } else {
        out.push(sig_slice[0]);
        if trim_len > 1 {
            out.push(b'.');
            out.extend_from_slice(&sig_slice[1..trim_len]);
        }
        out.push(b'e');
        let exp_sci = dot_ofs - 1;
        if exp_sci < 0 {
            out.push(b'-');
        }
        write_u64(exp_sci.unsigned_abs() as u64, out);
    }
}

#[inline]
fn hex_char(n: u8, lower: bool) -> u8 {
    if n < 10 {
        b'0' + n
    } else if lower {
        b'a' + (n - 10)
    } else {
        b'A' + (n - 10)
    }
}

#[inline]
fn write_u16_hex(v: u16, lower: bool, out: &mut Vec<u8>) {
    out.push(hex_char(((v >> 12) & 0xF) as u8, lower));
    out.push(hex_char(((v >> 8) & 0xF) as u8, lower));
    out.push(hex_char(((v >> 4) & 0xF) as u8, lower));
    out.push(hex_char((v & 0xF) as u8, lower));
}

fn write_string(
    bytes: &[u8],
    is_noesc: bool,
    flags: WriteFlags,
    out: &mut Vec<u8>,
) -> Result<(), WriteError> {
    let esc_uni = flags.contains(WriteFlags::ESCAPE_UNICODE);
    let esc_slash = flags.contains(WriteFlags::ESCAPE_SLASHES);
    let allow_invalid = flags.contains(WriteFlags::ALLOW_INVALID_UNICODE);
    let lower_hex = flags.contains(WriteFlags::LOWERCASE_HEX);

    out.push(b'"');
    if is_noesc && !esc_uni {
        out.extend_from_slice(bytes);
        out.push(b'"');
        return Ok(());
    }

    let mut i = 0usize;
    let mut plain_start = 0usize;
    while i < bytes.len() {
        let (next_i, has_slash) = swar_scan_str_ascii(bytes, i, b'"');
        if !esc_slash || !has_slash {
            i = next_i;
            if i >= bytes.len() {
                break;
            }
        }
        let b = bytes[i];
        if b < 0x20 || b == b'"' || b == b'\\' || (b == b'/' && esc_slash) {
            if i > plain_start {
                out.extend_from_slice(&bytes[plain_start..i]);
            }
            match b {
                b'"' => out.extend_from_slice(b"\\\""),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'/' => out.extend_from_slice(b"\\/"),
                0x08 => out.extend_from_slice(b"\\b"),
                0x0C => out.extend_from_slice(b"\\f"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                c => {
                    out.extend_from_slice(b"\\u00");
                    out.push(hex_char(c >> 4, lower_hex));
                    out.push(hex_char(c & 0xF, lower_hex));
                }
            }
            i += 1;
            plain_start = i;
        } else if b >= 0x80 {
            if let Some(clen) = utf8_char_len(&bytes[i..]) {
                if esc_uni {
                    if i > plain_start {
                        out.extend_from_slice(&bytes[plain_start..i]);
                    }
                    let cp = std::str::from_utf8(&bytes[i..i + clen])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .map(|c| c as u32)
                        .unwrap_or(0xFFFD);
                    if cp <= 0xFFFF {
                        out.extend_from_slice(b"\\u");
                        write_u16_hex(cp as u16, lower_hex, out);
                    } else {
                        let u = cp - 0x10000;
                        let hi = 0xD800 + ((u >> 10) as u16);
                        let lo = 0xDC00 + ((u & 0x3FF) as u16);
                        out.extend_from_slice(b"\\u");
                        write_u16_hex(hi, lower_hex, out);
                        out.extend_from_slice(b"\\u");
                        write_u16_hex(lo, lower_hex, out);
                    }
                    i += clen;
                    plain_start = i;
                } else {
                    i += clen;
                }
            } else {
                if !allow_invalid {
                    return Err(WriteError::new(
                        WriteErrorCode::InvalidString,
                        "invalid UTF-8 in string",
                    ));
                }
                if esc_uni {
                    if i > plain_start {
                        out.extend_from_slice(&bytes[plain_start..i]);
                    }
                    out.extend_from_slice(b"\\u");
                    write_u16_hex(0xFFFD, lower_hex, out);
                    i += 1;
                    plain_start = i;
                } else {
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    if i > plain_start {
        out.extend_from_slice(&bytes[plain_start..i]);
    }
    out.push(b'"');
    Ok(())
}

#[derive(Copy, Clone)]
struct IndentStyle {
    pretty: bool,
    unit: &'static [u8],
}

impl IndentStyle {
    #[inline]
    fn from_flags(flags: WriteFlags) -> Self {
        if flags.contains(WriteFlags::PRETTY_TABS) {
            Self { pretty: true, unit: b"\t" }
        } else if flags.contains(WriteFlags::PRETTY_TWO_SPACES) {
            Self { pretty: true, unit: b"  " }
        } else if flags.contains(WriteFlags::PRETTY) {
            Self { pretty: true, unit: b"    " }
        } else {
            Self { pretty: false, unit: b"" }
        }
    }

    #[inline]
    fn write_indent(self, depth: usize, out: &mut Vec<u8>) {
        out.push(b'\n');
        for _ in 0..depth {
            out.extend_from_slice(self.unit);
        }
    }
}

#[inline]
fn estimate_writer_capacity(num_vals: usize, buf_len: usize, pretty: bool) -> usize {
    let per_val = if pretty { 14 } else { 7 };
    num_vals
        .saturating_mul(per_val)
        .saturating_add(buf_len)
        .max(256)
}

fn write_val_to_vec(root: &ValRef<'_>, flags: WriteFlags) -> Result<Vec<u8>, WriteError> {
    let indent = IndentStyle::from_flags(flags);
    let cap = estimate_writer_capacity(root.doc.vals.len(), root.doc.buf.len(), indent.pretty);
    let mut out = Vec::with_capacity(cap);
    write_val_rec(*root, flags, indent, 0, &mut out)?;
    if flags.contains(WriteFlags::NEWLINE_AT_END) {
        out.push(b'\n');
    }
    Ok(out)
}

#[inline(always)]
fn write_primitive_val(
    val: ValRef<'_>,
    flags: WriteFlags,
    out: &mut Vec<u8>,
) -> Result<bool, WriteError> {
    match val.get_type() {
        Type::None => Err(WriteError::new(
            WriteErrorCode::InvalidValueType,
            "invalid value type: none",
        )),
        Type::Null => {
            out.extend_from_slice(b"null");
            Ok(true)
        }
        Type::Bool => {
            if val.is_true() {
                out.extend_from_slice(b"true");
            } else {
                out.extend_from_slice(b"false");
            }
            Ok(true)
        }
        Type::Raw => {
            out.extend_from_slice(val.str_slice());
            Ok(true)
        }
        Type::Num => {
            let v = val.val();
            if v.is_uint() {
                write_u64(v.uni, out);
            } else if v.is_sint() {
                write_i64(v.uni as i64, out);
            } else {
                write_f64(f64::from_bits(v.uni), flags, out)?;
            }
            Ok(true)
        }
        Type::Str => {
            let is_noesc = (val.val().tag as u8) == TAG_STR_NOESC as u8;
            write_string(val.str_slice(), is_noesc, flags, out)?;
            Ok(true)
        }
        Type::Arr | Type::Obj => Ok(false),
    }
}

fn write_val_rec(
    val: ValRef<'_>,
    flags: WriteFlags,
    indent: IndentStyle,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), WriteError> {
    if depth > MAX_WRITE_DEPTH {
        return Err(WriteError::new(WriteErrorCode::Depth, "write depth limit exceeded"));
    }
    if write_primitive_val(val, flags, out)? {
        return Ok(());
    }
    match val.get_type() {
        Type::Arr => {
            out.push(b'[');
            let len = val.len();
            if len > 0 {
                let trailing = flags.contains(WriteFlags::TRAILING_COMMAS);
                for (i, child) in val.iter_arr().enumerate() {
                    if indent.pretty {
                        indent.write_indent(depth + 1, out);
                    }
                    if !write_primitive_val(child, flags, out)? {
                        write_val_rec(child, flags, indent, depth + 1, out)?;
                    }
                    if i + 1 < len || trailing {
                        out.push(b',');
                    }
                }
                if indent.pretty {
                    indent.write_indent(depth, out);
                }
            }
            out.push(b']');
        }
        Type::Obj => {
            out.push(b'{');
            let len = val.len();
            if len > 0 {
                let trailing = flags.contains(WriteFlags::TRAILING_COMMAS);
                for (i, (k, v)) in val.iter_obj().enumerate() {
                    if indent.pretty {
                        indent.write_indent(depth + 1, out);
                    }
                    let is_noesc = (k.val().tag as u8) == TAG_STR_NOESC as u8;
                    write_string(k.str_slice(), is_noesc, flags, out)?;
                    out.push(b':');
                    if indent.pretty {
                        out.push(b' ');
                    }
                    if !write_primitive_val(v, flags, out)? {
                        write_val_rec(v, flags, indent, depth + 1, out)?;
                    }
                    if i + 1 < len || trailing {
                        out.push(b',');
                    }
                }
                if indent.pretty {
                    indent.write_indent(depth, out);
                }
            }
            out.push(b'}');
        }
        _ => {}
    }
    Ok(())
}

fn write_mut_val_to_vec(
    doc: &MutDoc,
    root: MutValId,
    flags: WriteFlags,
) -> Result<Vec<u8>, WriteError> {
    let indent = IndentStyle::from_flags(flags);
    let cap = estimate_writer_capacity(doc.nodes.len(), doc.buf.len(), indent.pretty);
    let mut out = Vec::with_capacity(cap);
    write_mut_val_rec(doc, root, flags, indent, 0, &mut out)?;
    if flags.contains(WriteFlags::NEWLINE_AT_END) {
        out.push(b'\n');
    }
    Ok(out)
}

fn write_mut_val_rec(
    doc: &MutDoc,
    id: MutValId,
    flags: WriteFlags,
    indent: IndentStyle,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), WriteError> {
    if depth > MAX_WRITE_DEPTH {
        return Err(WriteError::new(WriteErrorCode::Depth, "write depth limit exceeded"));
    }
    let node = &doc.nodes[id.0 as usize];
    match tag_type(node.tag) {
        Type::None => {
            return Err(WriteError::new(
                WriteErrorCode::InvalidValueType,
                "invalid value type: none",
            ));
        }
        Type::Null => out.extend_from_slice(b"null"),
        Type::Bool => {
            if (node.tag as u8) == TAG_TRUE as u8 {
                out.extend_from_slice(b"true");
            } else {
                out.extend_from_slice(b"false");
            }
        }
        Type::Raw => {
            let ofs = node.uni as usize;
            let len = tag_len(node.tag);
            out.extend_from_slice(&doc.buf[ofs..ofs + len]);
        }
        Type::Num => {
            if (node.tag as u8) == TAG_UINT as u8 {
                write_u64(node.uni, out);
            } else if (node.tag as u8) == TAG_SINT as u8 {
                write_i64(node.uni as i64, out);
            } else {
                write_f64(f64::from_bits(node.uni), flags, out)?;
            }
        }
        Type::Str => {
            let ofs = node.uni as usize;
            let len = tag_len(node.tag);
            let is_noesc = (node.tag as u8) == TAG_STR_NOESC as u8;
            write_string(&doc.buf[ofs..ofs + len], is_noesc, flags, out)?;
        }
        Type::Arr => {
            out.push(b'[');
            let len = tag_len(node.tag);
            if len > 0 {
                let trailing = flags.contains(WriteFlags::TRAILING_COMMAS);
                let mut cur = doc.arr_first(id);
                let mut i = 0;
                while let Some(cid) = cur {
                    if indent.pretty {
                        indent.write_indent(depth + 1, out);
                    }
                    write_mut_val_rec(doc, cid, flags, indent, depth + 1, out)?;
                    if i + 1 < len || trailing {
                        out.push(b',');
                    }
                    i += 1;
                    cur = doc.arr_next(id, cid);
                }
                if indent.pretty {
                    indent.write_indent(depth, out);
                }
            }
            out.push(b']');
        }
        Type::Obj => {
            out.push(b'{');
            let len = tag_len(node.tag);
            if len > 0 {
                let trailing = flags.contains(WriteFlags::TRAILING_COMMAS);
                let mut cur = doc.obj_first_key(id);
                let mut i = 0;
                while let Some(kid) = cur {
                    if indent.pretty {
                        indent.write_indent(depth + 1, out);
                    }
                    let knode = &doc.nodes[kid.0 as usize];
                    let kofs = knode.uni as usize;
                    let klen = tag_len(knode.tag);
                    let kis_noesc = (knode.tag as u8) == TAG_STR_NOESC as u8;
                    write_string(&doc.buf[kofs..kofs + klen], kis_noesc, flags, out)?;
                    out.push(b':');
                    if indent.pretty {
                        out.push(b' ');
                    }
                    let vid = MutValId(knode.next);
                    write_mut_val_rec(doc, vid, flags, indent, depth + 1, out)?;
                    if i + 1 < len || trailing {
                        out.push(b',');
                    }
                    i += 1;
                    cur = doc.obj_next_key(id, kid);
                }
                if indent.pretty {
                    indent.write_indent(depth, out);
                }
            }
            out.push(b'}');
        }
    }
    Ok(())
}

// ============================================================================
// JSON Pointer (RFC 6901)
// ============================================================================

fn parse_ptr_tokens(pointer: &str) -> Result<Vec<Vec<u8>>, PtrError> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = pointer.as_bytes();
    if bytes[0] != b'/' {
        return Err(PtrError {
            code: PtrErrorCode::Syntax,
            msg: "JSON Pointer must be empty or start with '/'",
            pos: 0,
        });
    }
    let mut tokens = Vec::new();
    let mut i = 1usize;
    loop {
        let mut tok = Vec::new();
        while i < bytes.len() && bytes[i] != b'/' {
            if bytes[i] == b'~' {
                if i + 1 >= bytes.len() {
                    return Err(PtrError {
                        code: PtrErrorCode::Syntax,
                        msg: "incomplete '~' escape in JSON Pointer",
                        pos: i,
                    });
                }
                match bytes[i + 1] {
                    b'0' => tok.push(b'~'),
                    b'1' => tok.push(b'/'),
                    _ => {
                        return Err(PtrError {
                            code: PtrErrorCode::Syntax,
                            msg: "invalid '~' escape in JSON Pointer",
                            pos: i,
                        });
                    }
                }
                i += 2;
            } else {
                tok.push(bytes[i]);
                i += 1;
            }
        }
        tokens.push(tok);
        if i >= bytes.len() {
            break;
        }
        i += 1; // skip '/'
    }
    Ok(tokens)
}

fn parse_ptr_array_index(tok: &[u8], pos: usize) -> Result<usize, PtrError> {
    if tok.is_empty() {
        return Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "empty array index",
            pos,
        });
    }
    if tok == b"0" {
        return Ok(0);
    }
    if tok[0] == b'0' {
        return Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "leading zero in array index",
            pos,
        });
    }
    let mut idx: usize = 0;
    for &b in tok {
        if !is_digit(b) {
            return Err(PtrError {
                code: PtrErrorCode::Resolve,
                msg: "invalid array index",
                pos,
            });
        }
        idx = idx
            .checked_mul(10)
            .and_then(|v| v.checked_add((b - b'0') as usize))
            .ok_or(PtrError {
                code: PtrErrorCode::Resolve,
                msg: "array index overflow",
                pos,
            })?;
    }
    Ok(idx)
}

pub fn ptr_get_val<'a>(root: ValRef<'a>, pointer: &str) -> Result<ValRef<'a>, PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    let mut cur = root;
    for tok in &tokens {
        match cur.get_type() {
            Type::Obj => {
                cur = cur.get_n(tok).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "object key not found",
                    pos: 0,
                })?;
            }
            Type::Arr => {
                let idx = parse_ptr_array_index(tok, 0)?;
                cur = cur.get_idx(idx).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "array index out of bounds",
                    pos: 0,
                })?;
            }
            _ => {
                return Err(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "cannot traverse non-container value",
                    pos: 0,
                });
            }
        }
    }
    Ok(cur)
}

pub fn ptr_get_mut(doc: &MutDoc, root: MutValId, pointer: &str) -> Result<MutValId, PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    let mut cur = root;
    for tok in &tokens {
        match doc.get_type(cur) {
            Type::Obj => {
                cur = doc.obj_get_n(cur, tok).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "object key not found",
                    pos: 0,
                })?;
            }
            Type::Arr => {
                let idx = parse_ptr_array_index(tok, 0)?;
                cur = doc.arr_get(cur, idx).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "array index out of bounds",
                    pos: 0,
                })?;
            }
            _ => {
                return Err(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "cannot traverse non-container value",
                    pos: 0,
                });
            }
        }
    }
    Ok(cur)
}

fn ptr_resolve_parent(doc: &MutDoc, tokens: &[Vec<u8>]) -> Result<MutValId, PtrError> {
    let mut cur = doc.root.ok_or(PtrError {
        code: PtrErrorCode::Null,
        msg: "empty document",
        pos: 0,
    })?;
    for tok in tokens {
        match doc.get_type(cur) {
            Type::Obj => {
                cur = doc.obj_get_n(cur, tok).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "parent object key not found",
                    pos: 0,
                })?;
            }
            Type::Arr => {
                let idx = parse_ptr_array_index(tok, 0)?;
                cur = doc.arr_get(cur, idx).ok_or(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "parent array index out of bounds",
                    pos: 0,
                })?;
            }
            _ => {
                return Err(PtrError {
                    code: PtrErrorCode::Resolve,
                    msg: "cannot traverse non-container value",
                    pos: 0,
                });
            }
        }
    }
    Ok(cur)
}

pub fn ptr_set_mut(
    doc: &mut MutDoc,
    pointer: &str,
    val: MutValId,
) -> Result<Option<MutValId>, PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    if tokens.is_empty() {
        let old = doc.root;
        doc.root = Some(val);
        return Ok(old);
    }
    let parent = ptr_resolve_parent(doc, &tokens[..tokens.len() - 1])?;
    let last = &tokens[tokens.len() - 1];
    match doc.get_type(parent) {
        Type::Obj => {
            let old = doc.obj_get_n(parent, last);
            let k = doc.strn(last);
            doc.obj_put(parent, k, val);
            Ok(old)
        }
        Type::Arr => {
            if last == b"-" {
                doc.arr_append(parent, val);
                Ok(None)
            } else {
                let idx = parse_ptr_array_index(last, 0)?;
                let len = doc.len(parent);
                if idx < len {
                    Ok(doc.arr_replace(parent, idx, val))
                } else if idx == len {
                    doc.arr_append(parent, val);
                    Ok(None)
                } else {
                    Err(PtrError {
                        code: PtrErrorCode::Resolve,
                        msg: "array index out of bounds",
                        pos: 0,
                    })
                }
            }
        }
        _ => Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "parent is not a container",
            pos: 0,
        }),
    }
}

pub fn ptr_add_mut(doc: &mut MutDoc, pointer: &str, val: MutValId) -> Result<(), PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    if tokens.is_empty() {
        doc.root = Some(val);
        return Ok(());
    }
    let parent = ptr_resolve_parent(doc, &tokens[..tokens.len() - 1])?;
    let last = &tokens[tokens.len() - 1];
    match doc.get_type(parent) {
        Type::Obj => {
            let k = doc.strn(last);
            doc.obj_put(parent, k, val);
            Ok(())
        }
        Type::Arr => {
            if last == b"-" {
                doc.arr_append(parent, val);
                Ok(())
            } else {
                let idx = parse_ptr_array_index(last, 0)?;
                if idx > doc.len(parent) {
                    return Err(PtrError {
                        code: PtrErrorCode::Resolve,
                        msg: "array index out of bounds",
                        pos: 0,
                    });
                }
                doc.arr_insert(parent, val, idx);
                Ok(())
            }
        }
        _ => Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "parent is not a container",
            pos: 0,
        }),
    }
}

pub fn ptr_replace_mut(
    doc: &mut MutDoc,
    pointer: &str,
    val: MutValId,
) -> Result<MutValId, PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    if tokens.is_empty() {
        let old = doc.root.ok_or(PtrError {
            code: PtrErrorCode::Null,
            msg: "empty document",
            pos: 0,
        })?;
        doc.root = Some(val);
        return Ok(old);
    }
    let parent = ptr_resolve_parent(doc, &tokens[..tokens.len() - 1])?;
    let last = &tokens[tokens.len() - 1];
    match doc.get_type(parent) {
        Type::Obj => {
            let old = doc.obj_get_n(parent, last).ok_or(PtrError {
                code: PtrErrorCode::Resolve,
                msg: "object key not found",
                pos: 0,
            })?;
            let k = doc.strn(last);
            doc.obj_put(parent, k, val);
            Ok(old)
        }
        Type::Arr => {
            let idx = parse_ptr_array_index(last, 0)?;
            doc.arr_replace(parent, idx, val).ok_or(PtrError {
                code: PtrErrorCode::Resolve,
                msg: "array index out of bounds",
                pos: 0,
            })
        }
        _ => Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "parent is not a container",
            pos: 0,
        }),
    }
}

pub fn ptr_remove_mut(doc: &mut MutDoc, pointer: &str) -> Result<MutValId, PtrError> {
    let tokens = parse_ptr_tokens(pointer)?;
    if tokens.is_empty() {
        return Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "cannot remove root document",
            pos: 0,
        });
    }
    let parent = ptr_resolve_parent(doc, &tokens[..tokens.len() - 1])?;
    let last = &tokens[tokens.len() - 1];
    match doc.get_type(parent) {
        Type::Obj => doc.obj_remove_key_n(parent, last).ok_or(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "object key not found",
            pos: 0,
        }),
        Type::Arr => {
            let idx = parse_ptr_array_index(last, 0)?;
            doc.arr_remove(parent, idx).ok_or(PtrError {
                code: PtrErrorCode::Resolve,
                msg: "array index out of bounds",
                pos: 0,
            })
        }
        _ => Err(PtrError {
            code: PtrErrorCode::Resolve,
            msg: "parent is not a container",
            pos: 0,
        }),
    }
}

// ============================================================================
// JSON Patch (RFC 6902) & JSON Merge-Patch (RFC 7386)
// ============================================================================

/// Apply an RFC 6902 JSON Patch document to `target`.
/// Atomic: if any operation fails, `target` is left unmodified.
pub fn json_patch(target: &mut MutDoc, patch_doc: &Doc) -> Result<(), PatchError> {
    let Some(root) = patch_doc.root() else {
        return Err(PatchError { msg: "patch document is empty", idx: 0 });
    };
    if !root.is_arr() {
        return Err(PatchError { msg: "patch document must be an array", idx: 0 });
    }
    let mut work = target.clone();
    for (idx, op_obj) in root.iter_arr().enumerate() {
        if !op_obj.is_obj() {
            return Err(PatchError { msg: "patch operation must be an object", idx });
        }
        let op = op_obj
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or(PatchError { msg: "missing 'op' string", idx })?;
        let path = op_obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or(PatchError { msg: "missing 'path' string", idx })?;

        match op {
            "add" => {
                let val = op_obj
                    .get("value")
                    .ok_or(PatchError { msg: "missing 'value' for add", idx })?;
                let mv = work.copy_from_val(&val);
                work.ptr_add(path, mv)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
            }
            "remove" => {
                work.ptr_remove(path)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
            }
            "replace" => {
                let val = op_obj
                    .get("value")
                    .ok_or(PatchError { msg: "missing 'value' for replace", idx })?;
                let mv = work.copy_from_val(&val);
                work.ptr_replace(path, mv)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
            }
            "move" => {
                let from = op_obj
                    .get("from")
                    .and_then(|v| v.as_str())
                    .ok_or(PatchError { msg: "missing 'from' for move", idx })?;
                if path == from {
                    work.ptr_get(from)
                        .map_err(|e| PatchError { msg: e.msg, idx })?;
                } else {
                    if path.starts_with(from) && path.as_bytes().get(from.len()) == Some(&b'/') {
                        return Err(PatchError { msg: "cannot move into a child of itself", idx });
                    }
                    let moved = work
                        .ptr_remove(from)
                        .map_err(|e| PatchError { msg: e.msg, idx })?;
                    work.ptr_add(path, moved)
                        .map_err(|e| PatchError { msg: e.msg, idx })?;
                }
            }
            "copy" => {
                let from = op_obj
                    .get("from")
                    .and_then(|v| v.as_str())
                    .ok_or(PatchError { msg: "missing 'from' for copy", idx })?;
                let src_id = work
                    .ptr_get(from)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
                let cloned = {
                    let snapshot = work.clone();
                    work.copy_from_mut(&snapshot, src_id)
                };
                work.ptr_add(path, cloned)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
            }
            "test" => {
                let expected = op_obj
                    .get("value")
                    .ok_or(PatchError { msg: "missing 'value' for test", idx })?;
                let actual_id = work
                    .ptr_get(path)
                    .map_err(|e| PatchError { msg: e.msg, idx })?;
                let actual_doc = {
                    let mut tmp = MutDoc::new();
                    let c = tmp.copy_from_mut(&work, actual_id);
                    tmp.set_root(c);
                    tmp.to_doc()
                };
                let actual_val = actual_doc.root().unwrap();
                if !actual_val.equals(&expected) {
                    return Err(PatchError { msg: "test operation failed", idx });
                }
            }
            _ => {
                return Err(PatchError { msg: "unsupported patch op", idx });
            }
        }
    }
    *target = work;
    Ok(())
}

/// Apply an RFC 7386 JSON Merge-Patch to `target`.
pub fn json_merge_patch(target: &mut MutDoc, patch: &ValRef<'_>) -> bool {
    if !patch.is_obj() {
        let new_root = target.copy_from_val(patch);
        target.set_root(new_root);
        return true;
    }
    let root = match target.root() {
        Some(r) if target.is_obj(r) => r,
        _ => {
            let o = target.obj();
            target.set_root(o);
            o
        }
    };
    merge_patch_obj(target, root, patch);
    true
}

fn merge_patch_obj(doc: &mut MutDoc, target_obj: MutValId, patch_obj: &ValRef<'_>) {
    for (k, v) in patch_obj.iter_obj() {
        let key_bytes = k.str_slice();
        if v.is_null() {
            doc.obj_remove_key_n(target_obj, key_bytes);
        } else if v.is_obj() {
            let child_obj = match doc.obj_get_n(target_obj, key_bytes) {
                Some(existing) if doc.is_obj(existing) => existing,
                _ => {
                    let new_obj = doc.obj();
                    let mk = doc.strn(key_bytes);
                    doc.obj_put(target_obj, mk, new_obj);
                    new_obj
                }
            };
            merge_patch_obj(doc, child_obj, &v);
        } else {
            let mv = doc.copy_from_val(&v);
            let mk = doc.strn(key_bytes);
            doc.obj_put(target_obj, mk, mv);
        }
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_read_write() {
        let json = r#"{"name":"vzjson","version":13,"pi":3.141592653589793,"ok":true,"nil":null,"arr":[1,-2,3.5,"hi"]}"#;
        let doc = Doc::read_str(json, ReadFlags::NOFLAG).expect("parse");
        let root = doc.root().unwrap();
        assert!(root.is_obj());
        assert_eq!(root.get("name").unwrap().as_str(), Some("vzjson"));
        assert_eq!(root.get("version").unwrap().as_u64(), Some(13));
        assert_eq!(root.get("pi").unwrap().as_f64(), Some(3.141592653589793));
        assert_eq!(root.get("ok").unwrap().as_bool(), Some(true));
        assert!(root.get("nil").unwrap().is_null());
        let arr = root.get("arr").unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr.get_idx(0).unwrap().as_u64(), Some(1));
        assert_eq!(arr.get_idx(1).unwrap().as_i64(), Some(-2));
        assert_eq!(arr.get_idx(2).unwrap().as_f64(), Some(3.5));
        assert_eq!(arr.get_idx(3).unwrap().as_str(), Some("hi"));

        let out = doc.write(WriteFlags::NOFLAG).unwrap();
        let doc2 = Doc::read_str(&out, ReadFlags::NOFLAG).unwrap();
        assert!(doc.root().unwrap().equals(&doc2.root().unwrap()));
    }

    #[test]
    fn test_numbers_edge_cases() {
        let json = r#"[0, -0, 18446744073709551615, -9223372036854775808, 1e-308, 1.7976931348623157e308]"#;
        let doc = Doc::read_str(json, ReadFlags::NOFLAG).unwrap();
        let arr = doc.root().unwrap();
        assert_eq!(arr.get_idx(0).unwrap().as_u64(), Some(0));
        assert_eq!(arr.get_idx(2).unwrap().as_u64(), Some(u64::MAX));
        assert_eq!(arr.get_idx(3).unwrap().as_i64(), Some(i64::MIN));
        assert_eq!(arr.get_idx(4).unwrap().as_f64(), Some(1e-308));
        assert_eq!(arr.get_idx(5).unwrap().as_f64(), Some(f64::MAX));

        assert!(Doc::read_str("00", ReadFlags::NOFLAG).is_err());
        assert!(Doc::read_str("123.", ReadFlags::NOFLAG).is_err());
        assert!(Doc::read_str("1e", ReadFlags::NOFLAG).is_err());
    }

    #[test]
    fn test_strings_and_surrogates() {
        let json = r#"["hello\n\t\"\\\/", "\u0041\u4E2D\uD83D\uDE00"]"#;
        let doc = Doc::read_str(json, ReadFlags::NOFLAG).unwrap();
        let arr = doc.root().unwrap();
        assert_eq!(arr.get_idx(0).unwrap().as_str(), Some("hello\n\t\"\\/"));
        assert_eq!(arr.get_idx(1).unwrap().as_str(), Some("A中😀"));

        let esc_out = doc.write(WriteFlags::ESCAPE_UNICODE | WriteFlags::ESCAPE_SLASHES).unwrap();
        assert!(esc_out.contains("\\u4E2D\\uD83D\\uDE00"));
        assert!(esc_out.contains("\\/"));
    }

    #[test]
    fn test_json5_and_flags() {
        let json5 = r#"
        // line comment
        {
            unquoted: 'single quoted', /* block */
            hex: 0x2A,
            lead_dot: .5,
            trail_dot: 5.,
            plus: +7,
            inf: Infinity,
            nan: NaN,
            arr: [1, 2, ],
        }
        "#;
        let doc = Doc::read_str(json5, ReadFlags::JSON5).unwrap();
        let r = doc.root().unwrap();
        assert_eq!(r.get("unquoted").unwrap().as_str(), Some("single quoted"));
        assert_eq!(r.get("hex").unwrap().as_u64(), Some(42));
        assert_eq!(r.get("lead_dot").unwrap().as_f64(), Some(0.5));
        assert_eq!(r.get("trail_dot").unwrap().as_f64(), Some(5.0));
        assert_eq!(r.get("plus").unwrap().as_u64(), Some(7));
        assert!(r.get("inf").unwrap().as_f64().unwrap().is_infinite());
        assert!(r.get("nan").unwrap().as_f64().unwrap().is_nan());
        assert_eq!(r.get("arr").unwrap().len(), 2);
    }

    #[test]
    fn test_mut_doc_operations() {
        let mut mdoc = MutDoc::new();
        let root = mdoc.obj();
        mdoc.set_root(root);
        mdoc.obj_add_str(root, "a", "hello");
        mdoc.obj_add_uint(root, "b", 10);
        let arr = mdoc.arr_with_i64(&[1, 2, 3]);
        let k = mdoc.str("c");
        mdoc.obj_add(root, k, arr);

        let v4 = mdoc.sint(4);
        mdoc.arr_append(arr, v4);
        let v0 = mdoc.sint(0);
        mdoc.arr_prepend(arr, v0);
        mdoc.arr_remove(arr, 2); // removes 2 -> [0, 1, 3, 4]
        mdoc.arr_rotate(arr, 1); // -> [1, 3, 4, 0]

        let doc = mdoc.to_doc();
        let r = doc.root().unwrap();
        assert_eq!(r.get("a").unwrap().as_str(), Some("hello"));
        let carr: Vec<i64> = r.get("c").unwrap().iter_arr().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(carr, vec![1, 3, 4, 0]);
    }

    #[test]
    fn test_json_pointer_rfc6901() {
        let json = r#"{
            "foo": ["bar", "baz"],
            "": 0,
            "a/b": 1,
            "c%d": 2,
            "e^f": 3,
            "g|h": 4,
            "i\\j": 5,
            "k\"l": 6,
            " ": 7,
            "m~n": 8
        }"#;
        let doc = Doc::read_str(json, ReadFlags::NOFLAG).unwrap();
        let r = doc.root().unwrap();
        assert_eq!(r.ptr("/foo/0").unwrap().as_str(), Some("bar"));
        assert_eq!(r.ptr("/").unwrap().as_u64(), Some(0));
        assert_eq!(r.ptr("/a~1b").unwrap().as_u64(), Some(1));
        assert_eq!(r.ptr("/c%d").unwrap().as_u64(), Some(2));
        assert_eq!(r.ptr("/e^f").unwrap().as_u64(), Some(3));
        assert_eq!(r.ptr("/g|h").unwrap().as_u64(), Some(4));
        assert_eq!(r.ptr("/i\\j").unwrap().as_u64(), Some(5));
        assert_eq!(r.ptr("/k\"l").unwrap().as_u64(), Some(6));
        assert_eq!(r.ptr("/ ").unwrap().as_u64(), Some(7));
        assert_eq!(r.ptr("/m~0n").unwrap().as_u64(), Some(8));
    }

    #[test]
    fn test_json_patch_rfc6902() {
        let target_doc = Doc::read_str(r#"{"a":"foo","b":[1,2]}"#, ReadFlags::NOFLAG).unwrap();
        let mut mdoc = target_doc.to_mut();
        let patch_doc = Doc::read_str(
            r#"[
                {"op":"test","path":"/a","value":"foo"},
                {"op":"replace","path":"/a","value":"bar"},
                {"op":"add","path":"/b/-","value":3},
                {"op":"copy","from":"/b/0","path":"/c"},
                {"op":"move","from":"/b/1","path":"/d"},
                {"op":"remove","path":"/b/0"}
            ]"#,
            ReadFlags::NOFLAG,
        ).unwrap();
        mdoc.patch(&patch_doc).unwrap();
        let res = mdoc.to_doc();
        let r = res.root().unwrap();
        assert_eq!(r.get("a").unwrap().as_str(), Some("bar"));
        assert_eq!(r.get("c").unwrap().as_u64(), Some(1));
        assert_eq!(r.get("d").unwrap().as_u64(), Some(2));
        assert_eq!(r.get("b").unwrap().len(), 1);
        assert_eq!(r.get("b").unwrap().get_idx(0).unwrap().as_u64(), Some(3));
    }

    #[test]
    fn test_json_merge_patch_rfc7386() {
        let target_doc = Doc::read_str(r#"{"a":"b","c":{"d":"e","f":"g"}}"#, ReadFlags::NOFLAG).unwrap();
        let mut mdoc = target_doc.to_mut();
        let patch_doc = Doc::read_str(r#"{"a":"z","c":{"f":null}}"#, ReadFlags::NOFLAG).unwrap();
        mdoc.merge_patch(&patch_doc.root().unwrap());
        let res = mdoc.to_doc();
        let r = res.root().unwrap();
        assert_eq!(r.get("a").unwrap().as_str(), Some("z"));
        assert_eq!(r.get("c").unwrap().get("d").unwrap().as_str(), Some("e"));
        assert!(r.get("c").unwrap().get("f").is_none());
    }

    #[test]
    fn test_incremental_reader_and_locate() {
        let json = br#"{"hello":[1,2,3]}"#;
        let mut incr = IncrReader::new(ReadFlags::NOFLAG);
        for len in 1..json.len() {
            let err = incr.read(&json[..len]).unwrap_err();
            assert_eq!(err.code, ReadErrorCode::More);
        }
        let doc = incr.read(json).unwrap();
        assert_eq!(doc.root().unwrap().get("hello").unwrap().len(), 3);

        let pos = locate_pos(b"{\n  \"x\": 1\n}", 6).unwrap();
        assert_eq!(pos, TextPos { line: 2, col: 5, char_pos: 6 });
    }

    #[test]
    fn test_nan_and_inf_variants() {
        let json = r#"[nan, -nan, NaN, -NaN, inf, -inf, Infinity, -Infinity]"#;
        let doc = Doc::read_str(json, ReadFlags::ALLOW_INF_AND_NAN).unwrap();
        let arr = doc.root().unwrap();
        assert_eq!(arr.len(), 8);
        for i in 0..4 {
            assert!(arr.get_idx(i).unwrap().as_f64().unwrap().is_nan());
        }
        assert_eq!(arr.get_idx(4).unwrap().as_f64(), Some(f64::INFINITY));
        assert_eq!(arr.get_idx(5).unwrap().as_f64(), Some(f64::NEG_INFINITY));
        assert_eq!(arr.get_idx(6).unwrap().as_f64(), Some(f64::INFINITY));
        assert_eq!(arr.get_idx(7).unwrap().as_f64(), Some(f64::NEG_INFINITY));

        assert!(doc.write(WriteFlags::NOFLAG).is_err());
        let s_inf = doc.write(WriteFlags::ALLOW_INF_AND_NAN).unwrap();
        assert_eq!(s_inf, "[NaN,NaN,NaN,NaN,Infinity,-Infinity,Infinity,-Infinity]");
        let s_null = doc.write(WriteFlags::INF_AND_NAN_AS_NULL).unwrap();
        assert_eq!(s_null, "[null,null,null,null,null,null,null,null]");
    }

    #[test]
    fn test_incremental_surrogate_and_root_number() {
        // Split byte-by-byte across a surrogate pair inside a string
        let json = br#"{"emoji":"\uD83D\uDE00","num":42}"#;
        let mut incr = IncrReader::new(ReadFlags::NOFLAG);
        for len in 1..json.len() {
            let err = incr.read(&json[..len]).unwrap_err();
            assert_eq!(err.code, ReadErrorCode::More, "failed at len {}", len);
        }
        let doc = incr.read(json).unwrap();
        assert_eq!(doc.root().unwrap().get("emoji").unwrap().as_str(), Some("😀"));
        assert_eq!(doc.root().unwrap().get("num").unwrap().as_u64(), Some(42));

        // Root number requires is_final=true to complete
        let mut incr_num = IncrReader::new(ReadFlags::NOFLAG);
        assert_eq!(incr_num.read_chunk(b"123", false).unwrap_err().code, ReadErrorCode::More);
        let doc_num = incr_num.read_chunk(b"12345", true).unwrap();
        assert_eq!(doc_num.root().unwrap().as_u64(), Some(12345));
    }

    #[test]
    fn test_val_equals_duplicate_keys() {
        let a = Doc::read_str(r#"{"a":1,"a":2}"#, ReadFlags::NOFLAG).unwrap();
        let b = Doc::read_str(r#"{"a":1,"b":3}"#, ReadFlags::NOFLAG).unwrap();
        let c = Doc::read_str(r#"{"a":2,"a":1}"#, ReadFlags::NOFLAG).unwrap();
        assert!(!a.root().unwrap().equals(&b.root().unwrap()));
        assert!(a.root().unwrap().equals(&c.root().unwrap()));
    }

    #[test]
    fn test_deep_mut_doc_to_doc_iterative() {
        let mut mdoc = MutDoc::new();
        let mut cur = mdoc.null();
        for _ in 0..1000 {
            let arr = mdoc.arr();
            mdoc.arr_append(arr, cur);
            cur = arr;
        }
        mdoc.set_root(cur);
        let doc = mdoc.to_doc();
        assert_eq!(doc.val_count(), 1001);
    }

    #[test]
    fn test_flags_writer_and_patch_rollback() {
        // BOM + STOP_WHEN_DONE + NUMBER_AS_RAW + BIGNUM_AS_RAW
        let mut bom_json = vec![0xEF, 0xBB, 0xBF];
        bom_json.extend_from_slice(br#"{"big":99999999999999999999999} trailing"#);
        let doc = Doc::read(&bom_json, ReadFlags::ALLOW_BOM | ReadFlags::STOP_WHEN_DONE | ReadFlags::BIGNUM_AS_RAW).unwrap();
        assert_eq!(doc.root().unwrap().get("big").unwrap().as_raw(), Some("99999999999999999999999"));

        // Writer formatting flags
        let d2 = Doc::read_str(r#"{"x":3.14159,"y":[1,2]}"#, ReadFlags::NOFLAG).unwrap();
        let pretty2 = d2.write(WriteFlags::PRETTY_TWO_SPACES | WriteFlags::NEWLINE_AT_END | WriteFlags::fp_to_fixed(2)).unwrap();
        assert_eq!(pretty2, "{\n  \"x\": 3.14,\n  \"y\": [\n    1,\n    2\n  ]\n}\n");

        // JSON Patch atomic rollback on failure
        let mut mdoc = d2.to_mut();
        let bad_patch = Doc::read_str(
            r#"[{"op":"replace","path":"/x","value":99},{"op":"test","path":"/x","value":123}]"#,
            ReadFlags::NOFLAG,
        ).unwrap();
        assert!(mdoc.patch(&bad_patch).is_err());
        assert_eq!(mdoc.to_doc().root().unwrap().get("x").unwrap().as_f64(), Some(3.14159));

        // Depth limit
        let deep = "[".repeat(MAX_READ_DEPTH + 2) + &"]".repeat(MAX_READ_DEPTH + 2);
        let err = Doc::read_str(&deep, ReadFlags::NOFLAG).unwrap_err();
        assert_eq!(err.code, ReadErrorCode::Depth);
    }

    #[test]
    fn test_insitu_pool_swar_and_conformance() {
        // 1. Real zero-copy / in-place unescaping with ReadFlags::INSITU
        let long_ascii = "a".repeat(128);
        let raw_json = format!(r#"{{"plain":"{}","esc":"hello\n\uD83D\uDE00world"}}"#, long_ascii);
        let doc = Doc::read_insitu(raw_json.into_bytes(), ReadFlags::NOFLAG).unwrap();
        let r = doc.root().unwrap();
        assert_eq!(r.get("plain").unwrap().as_str(), Some(long_ascii.as_str()));
        assert_eq!(r.get("esc").unwrap().as_str(), Some("hello\n😀world"));

        // 2. PoolAlc zero-allocation reuse across multiple docs
        assert!(read_max_memory_usage(1024, ReadFlags::NOFLAG) > 1024);
        let mut pool = PoolAlc::new(512);
        let mut reusable_doc = PoolAlc::alloc_doc(512);
        for i in 0..50u64 {
            let s = format!(r#"{{"idx":{},"msg":"item-{}"}}"#, i, i);
            pool.read_into(s.as_bytes(), ReadFlags::NOFLAG, &mut reusable_doc).unwrap();
            assert_eq!(reusable_doc.root().unwrap().get("idx").unwrap().as_u64(), Some(i));
            let out = pool.write_opts(&reusable_doc, WriteFlags::NOFLAG).unwrap();
            assert_eq!(out, s.as_bytes());
        }

        // 3. RFC 8259 / JSONTestSuite conformance vectors
        let must_pass = [
            "null", "true", "false", "0", "-0", "1234567890", "-9876543210",
            "0.0", "1e10", "1E-10", "[]", "{}", "[[]]", "{\"\":[]}",
            r#"["\b\f\n\r\t\"\\\/"]"#,
            r#"{"a":1,"b":[true,false,null,{}]}"#,
        ];
        for case in must_pass {
            assert!(Doc::read_str(case, ReadFlags::NOFLAG).is_ok(), "should pass: {}", case);
        }

        let must_fail = [
            "", "   ", "[1,]", "{\"a\":1,}", "{\"a\":}", "[01]", "[-01]",
            "[.5]", "[1.]", "[+1]", r#"["\u123"]"#, r#"["\x41"]"#, "[\"a\nb\"]",
            r#"{"a":1 "b":2}"#, "[1 2]", "[1,,2]", "[1] extra", "nul", "tru", "fals",
            "NaN", "Infinity", "'single'", "{unquoted:1}",
        ];
        for case in must_fail {
            assert!(Doc::read_str(case, ReadFlags::NOFLAG).is_err(), "should fail: {}", case);
        }
    }

    #[test]
    fn test_schubfach_roundtrip() {
        let vals = [
            0.0_f64,
            -0.0,
            1.0,
            -1.0,
            0.5,
            0.1,
            0.2,
            0.1 + 0.2,
            3.141592653589793,
            -123456.789012345,
            1e-5,
            1e-6,
            1e-7,
            1e20,
            1e21,
            1e22,
            1e-308,
            f64::MIN_POSITIVE,
            5e-324, // smallest positive subnormal
            -5e-324,
            f64::MAX,
            f64::MIN,
            1.7976931348623157e308,
            2.225073858507201e-308, // largest subnormal
        ];
        for &v in &vals {
            let mut out = Vec::new();
            write_f64(v, WriteFlags::NOFLAG, &mut out).unwrap();
            let s = std::str::from_utf8(&out).unwrap();
            let parsed: f64 = s.parse().unwrap();
            assert_eq!(
                parsed.to_bits(),
                v.to_bits(),
                "round-trip mismatch for {:?} -> {:?}",
                v,
                s
            );
        }
    }

    #[test]
    fn test_swar_scan_str_ascii_trailing_zeros() {
        // Exact index of quote / backslash / control / non-ASCII inside 8-byte chunk
        for idx in 0..8 {
            let mut buf = *b"abcdefgh";
            buf[idx] = b'"';
            let (pos, has_slash) = swar_scan_str_ascii(&buf, 0, b'"');
            assert_eq!(pos, idx);
            assert!(!has_slash);
        }
        // Slash before quote vs slash after quote in the same 8-byte chunk
        let (pos1, slash1) = swar_scan_str_ascii(b"a/b\"cdef", 0, b'"');
        assert_eq!((pos1, slash1), (3, true));
        let (pos2, slash2) = swar_scan_str_ascii(b"abc\"/def", 0, b'"');
        assert_eq!((pos2, slash2), (3, false));
        // Multi-chunk with slash in first chunk and backslash in second
        let (pos3, slash3) = swar_scan_str_ascii(b"0123/56789\\bcdef", 0, b'"');
        assert_eq!((pos3, slash3), (10, true));
    }

    #[test]
    fn test_swar_digit_parsing() {
        assert_eq!(swar_parse_8_digits(b"12345678"), Some(12345678));
        assert_eq!(swar_parse_8_digits(b"00000000"), Some(0));
        assert_eq!(swar_parse_8_digits(b"99999999"), Some(99999999));
        assert_eq!(swar_parse_8_digits(b"1234567a"), None);
        assert_eq!(swar_parse_8_digits(b"1234567/"), None);
        assert_eq!(swar_parse_8_digits(b"1234567:"), None);
        assert_eq!(swar_parse_8_digits(b"1234567"), None);

        assert_eq!(swar_parse_4_digits(b"1234"), Some(1234));
        assert_eq!(swar_parse_4_digits(b"0000"), Some(0));
        assert_eq!(swar_parse_4_digits(b"9999"), Some(9999));
        assert_eq!(swar_parse_4_digits(b"123x"), None);
        assert_eq!(swar_parse_4_digits(b"123"), None);

        // Integration through Doc::read_str for 4, 8, 12, 16, 19, 20-digit integers and floats
        let d = Doc::read_str("[1234,12345678,123456789012,18446744073709551615,-9223372036854775808,123456789.12345678]", ReadFlags::NOFLAG).unwrap();
        let root = d.root().unwrap();
        assert_eq!(root.get_idx(0).unwrap().as_u64(), Some(1234));
        assert_eq!(root.get_idx(1).unwrap().as_u64(), Some(12345678));
        assert_eq!(root.get_idx(2).unwrap().as_u64(), Some(123456789012));
        assert_eq!(root.get_idx(3).unwrap().as_u64(), Some(u64::MAX));
        assert_eq!(root.get_idx(4).unwrap().as_i64(), Some(i64::MIN));
        assert_eq!(root.get_idx(5).unwrap().as_f64(), Some(123456789.12345678));
    }
}
