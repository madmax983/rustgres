// v1.93: PG19 bit/varbit operators and helpers (varbit.c), over the
// storage `BitString` value (MSB-first bytes, pad bits zero). All
// operations preserve `bitlen` except concat (sum), substring
// (window), and overlay (defined by substring + concat, like PG).
use super::*;

/// PG19 `VARBITMAXLEN` (varbit.h): the largest legal bit-string length.
pub(crate) const BIT_MAX_LEN: i64 = 838_860_800;

/// The bits of `bs` as 0/1 bytes, most significant first.
pub(crate) fn bit_bits(bs: &BitString) -> Vec<u8> {
    let mut out = Vec::with_capacity(bs.bitlen as usize);
    for i in 0..bs.bitlen as usize {
        out.push(if bs.bytes[i / 8] & (1 << (7 - (i % 8))) != 0 {
            1
        } else {
            0
        });
    }
    out
}

/// Pack 0/1 bytes (most significant first) into a `BitString`.
pub(crate) fn bit_from_bits(bits: &[u8]) -> BitString {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b != 0 {
            bytes[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    BitString {
        bitlen: bits.len() as u32,
        bytes,
    }
}

/// PG19 varbit.c `bitand` / `bitor` / `bitxor`: equal bit lengths are
/// required (22026, with the per-operator message).
fn bit_zip(
    a: &BitString,
    b: &BitString,
    what: &str,
    f: impl Fn(u8, u8) -> u8,
) -> Result<BitString, ExecError> {
    if a.bitlen != b.bitlen {
        return Err(exec_err(
            "22026",
            format!("cannot {} bit strings of different sizes", what),
        ));
    }
    let bytes = a
        .bytes
        .iter()
        .zip(b.bytes.iter())
        .map(|(x, y)| f(*x, *y))
        .collect();
    Ok(BitString {
        bitlen: a.bitlen,
        bytes,
    })
}

pub(crate) fn bit_and(a: &BitString, b: &BitString) -> Result<BitString, ExecError> {
    bit_zip(a, b, "AND", |x, y| x & y)
}

pub(crate) fn bit_or(a: &BitString, b: &BitString) -> Result<BitString, ExecError> {
    bit_zip(a, b, "OR", |x, y| x | y)
}

pub(crate) fn bit_xor(a: &BitString, b: &BitString) -> Result<BitString, ExecError> {
    bit_zip(a, b, "XOR", |x, y| x ^ y)
}

/// PG19 varbit.c `bitnot`: invert the bytes, then re-zero the pad bits.
pub(crate) fn bit_not(a: &BitString) -> BitString {
    let mut bytes: Vec<u8> = a.bytes.iter().map(|b| !b).collect();
    let rem = a.bitlen % 8;
    if rem != 0 {
        if let Some(last) = bytes.last_mut() {
            *last &= 0xFFu8 << (8 - rem);
        }
    }
    BitString {
        bitlen: a.bitlen,
        bytes,
    }
}

/// PG19 varbit.c `bitshiftleft` / `bitshiftright`: a shift toward the
/// start (`left = true`) or end of the string, zero-filled, with the
/// bit length preserved. A negative count reverses the direction; a
/// count beyond VARBITMAXLEN clamps like PG's negation guard, and any
/// |count| >= bitlen yields all zeros.
pub(crate) fn bit_shift(a: &BitString, mut shft: i64, mut left: bool) -> BitString {
    if shft < 0 {
        // PG clamps the reversal at VARBITMAXLEN before negating.
        if shft < -BIT_MAX_LEN {
            shft = -BIT_MAX_LEN;
        }
        shft = -shft;
        left = !left;
    }
    let n = a.bitlen as usize;
    if shft as i64 >= a.bitlen as i64 {
        return BitString {
            bitlen: a.bitlen,
            bytes: vec![0u8; a.bytes.len()],
        };
    }
    let k = shft as usize;
    let bits = bit_bits(a);
    let mut out = vec![0u8; n];
    if left {
        out[..n - k].copy_from_slice(&bits[k..]);
    } else {
        out[k..].copy_from_slice(&bits[..n - k]);
    }
    bit_from_bits(&out)
}

/// PG19 varbit.c `bit_catenate`: append `b`'s bits to `a`'s, splicing
/// at a non-byte boundary when `a`'s length is not a multiple of 8.
pub(crate) fn bit_concat(a: &BitString, b: &BitString) -> Result<BitString, ExecError> {
    if a.bitlen as i64 + b.bitlen as i64 > BIT_MAX_LEN {
        return Err(exec_err(
            "54000",
            format!(
                "bit string length exceeds the maximum allowed ({})",
                BIT_MAX_LEN
            ),
        ));
    }
    let mut bits = bit_bits(a);
    bits.extend_from_slice(&bit_bits(b));
    Ok(bit_from_bits(&bits))
}

/// PG19 varbit.c `bitsubstring`: 1-based window. `len: None` runs to
/// the end of the string; a negative length is 22011; an `s + l` that
/// overflows int32 also runs to the end. Windows outside the string
/// produce a zero-length bit string.
pub(crate) fn bit_substring(
    a: &BitString,
    s: i64,
    len: Option<i64>,
) -> Result<BitString, ExecError> {
    let bitlen = a.bitlen as i64;
    let s1 = s.max(1);
    let e1 = match len {
        None => bitlen + 1,
        Some(l) => {
            if l < 0 {
                return Err(exec_err("22011", "negative substring length not allowed"));
            }
            match s.checked_add(l) {
                Some(e) if e <= i32::MAX as i64 => e.min(bitlen + 1),
                // int32 overflow (or beyond): substring runs to the end.
                _ => bitlen + 1,
            }
        }
    };
    if s1 > bitlen || e1 <= s1 {
        return Ok(BitString {
            bitlen: 0,
            bytes: Vec::new(),
        });
    }
    let bits = bit_bits(a);
    Ok(bit_from_bits(&bits[(s1 - 1) as usize..(e1 - 1) as usize]))
}

/// PG19 varbit.c `bit_overlay`: `sp <= 0` is 22011, an `sp + sl` int32
/// overflow is 22003; otherwise the result is
/// `substr(t, 1, sp-1) || placing || substr(t, sp+sl)`.
pub(crate) fn bit_overlay(
    t: &BitString,
    placing: &BitString,
    sp: i64,
    sl: i64,
) -> Result<BitString, ExecError> {
    if sp <= 0 {
        return Err(exec_err("22011", "negative substring length not allowed"));
    }
    let sp_pl_sl = match sp.checked_add(sl) {
        Some(v) if v <= i32::MAX as i64 && v >= i32::MIN as i64 => v,
        _ => return Err(exec_err("22003", "integer out of range")),
    };
    let s1 = bit_substring(t, 1, Some(sp - 1))?;
    let s2 = bit_substring(t, sp_pl_sl, None)?;
    let head = bit_concat(&s1, placing)?;
    bit_concat(&head, &s2)
}

/// PG19 varbit.c `bitposition`: 1-based index of the first occurrence
/// of `sub` in `s`, 0 when absent. An empty substring matches at 1,
/// except that an empty string itself yields 0 (checked first).
pub(crate) fn bit_position(sub: &BitString, s: &BitString) -> i64 {
    if s.bitlen == 0 || sub.bitlen > s.bitlen {
        return 0;
    }
    if sub.bitlen == 0 {
        return 1;
    }
    let hay = bit_bits(s);
    let needle = bit_bits(sub);
    hay.windows(needle.len())
        .position(|w| w == needle.as_slice())
        .map(|i| i as i64 + 1)
        .unwrap_or(0)
}

/// PG19 varbit.c range check shared by `bitgetbit` / `bitsetbit`
/// (2202E, 0-based left-to-right numbering).
fn bit_index_check(a: &BitString, n: i64) -> Result<usize, ExecError> {
    if n < 0 || n >= a.bitlen as i64 {
        return Err(exec_err(
            "2202E",
            format!(
                "bit index {} out of valid range (0..{})",
                n,
                a.bitlen as i64 - 1
            ),
        ));
    }
    Ok(n as usize)
}

/// PG19 varbit.c `bitgetbit`: bit `n`, numbered 0-based from the left
/// (MSB first — the opposite of the bytea form in funcs.rs).
pub(crate) fn bit_get_bit(a: &BitString, n: i64) -> Result<i64, ExecError> {
    let n = bit_index_check(a, n)?;
    Ok(((a.bytes[n / 8] >> (7 - (n % 8))) & 1) as i64)
}

/// PG19 varbit.c `bitsetbit`: a copy of `a` with bit `n` set to `v`
/// (the range is checked before the value, like PG).
pub(crate) fn bit_set_bit(a: &BitString, n: i64, v: i64) -> Result<BitString, ExecError> {
    let n = bit_index_check(a, n)?;
    if v != 0 && v != 1 {
        return Err(exec_err("22023", "new bit must be 0 or 1"));
    }
    let mut out = a.clone();
    if v == 1 {
        out.bytes[n / 8] |= 1 << (7 - (n % 8));
    } else {
        out.bytes[n / 8] &= !(1 << (7 - (n % 8)));
    }
    Ok(out)
}

/// PG19 varbit.c `bit_bit_count`: population count over the stored
/// bytes (pad bits are zero, so they never contribute).
pub(crate) fn bit_count(a: &BitString) -> i64 {
    a.bytes.iter().map(|b| b.count_ones() as i64).sum()
}

/// v1.93: PG19 varbit.c `bit` / `varbit` as an EXPLICIT cast: fixed bit
/// keeps the FIRST `n` bits, right-padding with zeros when short;
/// varying only truncates (never pads, never errors). Distinct from
/// the assignment/input semantics in `sj_check_bit_len` /
/// `sj_coerce_bit_typmod` (SQL/JSON), which drop leading bits.
pub(crate) fn bit_cast_typmod(a: &BitString, n: i32, varying: bool) -> BitString {
    if n < 0 {
        return a.clone();
    }
    let n = n as usize;
    let mut chars: Vec<char> = a.to_bit_chars().chars().collect();
    chars.truncate(n);
    if !varying {
        while chars.len() < n {
            chars.push('0');
        }
    }
    let s: String = chars.into_iter().collect();
    BitString::parse('b', &s).expect("bit chars are valid binary digits")
}

/// v1.93: PG19 varbit.c `bitfromint4` / `bitfromint8`: an integer's
/// two's-complement bits at the target width `n` — the low `n` bits,
/// sign-filled when `n` exceeds the source width. (The varying flag
/// does not change the result: bitfromint always produces exactly
/// the typmod width.)
pub(crate) fn bit_from_int(i: i64, _width: u32, n: i32, _varying: bool) -> BitString {
    if n <= 0 {
        return BitString {
            bitlen: 0,
            bytes: Vec::new(),
        };
    }
    let n = n as usize;
    let mut s = String::with_capacity(n);
    for pos in (0..n).rev() {
        let bit = if pos < 64 {
            (i >> pos) & 1
        } else if i < 0 {
            1
        } else {
            0
        };
        s.push(if bit == 1 { '1' } else { '0' });
    }
    BitString::parse('b', &s).expect("generated bit chars are valid")
}

/// The `placing` argument of a bit `overlay(...)`: PG resolves the
/// unknown literal `'001'` to bit, so a text value whose contents are
/// bit digits (or a `B'...'`/`X'...'` literal form) parses as a bit
/// string. Anything else is the assignment-cast error path — reuse the
/// plain `bit_in` failure (22P02 with PG's digit message).
pub(crate) fn bit_placing_arg(v: &Value) -> Result<BitString, ExecError> {
    match v {
        Value::BitString(b) => Ok(b.clone()),
        Value::Text(s) => BitString::parse_input(s.trim()).map_err(|m| exec_err("22P02", m)),
        other => Err(exec_err(
            "42883",
            format!(
                "function overlay(bit, {}) does not exist",
                other.type_name()
            ),
        )),
    }
}
