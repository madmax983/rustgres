//! v0.37: TOAST — compression and out-of-line storage for wide rows.
//!
//! Follows PostgreSQL's `heaptoast.c` strategy (PG19):
//! 1. Compress EXTENDED columns (PGLZ).
//! 2. Move the biggest EXTENDED/EXTERNAL values out-of-line.
//! 3. Compress MAIN columns.
//! 4. As a last resort, move MAIN values out-of-line.
//!
//! Design notes (honest deviations from PG):
//! - Main-table rows always store the full detoasted value; the
//!   `RowVersion::toast` flags are metadata recording which cells PG
//!   would have toasted. This keeps every read path (all functions,
//!   casts, indexes) working unchanged — detoasting is a no-op.
//! - The toast table (`pg_toast.pg_toast_<oid>`) holds derived chunks
//!   of the compressed/relocated bytes, so `pg_class.reltoastrelid`
//!   and chunk counts are real, not constants.
//! - The compressor is a faithful port of PG19's `pglz_compress()` with
//!   the default strategy: for a given input the payload bytes are
//!   identical to PostgreSQL 19's, and compression is refused under
//!   exactly PG's conditions (input < 32 bytes, payload must beat 75%
//!   of the input, give up if no match by 1024 output bytes).
//!   `pg_column_compression` reports the conventional `'pglz'` name.
//!   Framing differs: PG carries the original length in the compressed
//!   varlena header, while rustgres prepends a 4-byte little-endian
//!   length (read back by the test-only decompressor).
//! - v0.41: a pure-std LZ4 block codec, plus per-column `COMPRESSION`
//!   selection. `default_toast_compression` accepts `pglz` (the PG19
//!   default) and `lz4`; the session default steers new writes unless a
//!   column has an explicit `COMPRESSION` method.

use crate::storage::{Table, ToastInfo, Value, toast_consts, toast_storage};
use std::cell::RefCell;

/// PG's `TOAST_POINTER_SIZE` (`detoast.h`): `VARHDRSZ_EXTERNAL` (4) +
/// `sizeof(varatt_external)` (16) — the on-disk footprint of a toasted
/// value's pointer datum once it moves out-of-line.
const TOAST_POINTER_SIZE: usize = 20;
/// `MAXALIGN(TOAST_POINTER_SIZE)`: PG's
/// `toast_tuple_find_biggest_attribute` only considers columns larger
/// than this worth moving or compressing.
const MAXALIGN_TOAST_POINTER: usize = 24;

// ---------------------------------------------------------------------------
// PGLZ compressor — byte-compatible with PostgreSQL 19
// ---------------------------------------------------------------------------
//
// Faithful port of `pglz_compress()` from PG19's `src/common/pg_lzcompress.c`
// (REL_19_STABLE) using the default strategy (`PGLZ_strategy_default`).
// For a given input the emitted payload is byte-identical to PG19's
// `pglz_compress(source, slen, dest, NULL)` output, and compression is
// refused under exactly PG's conditions: input shorter than 32 bytes, or
// the strategy's gates trip (payload must beat 75% of the input; give up
// if no match was found by 1024 output bytes).
//
// Framing: PG carries the original length in the compressed varlena
// header, outside the payload. rustgres has no varlena headers, so it
// prepends a 4-byte little-endian original length (read back by
// [`decompress_pglz`]); the bytes after it are the stock PGLZ stream.
//
// One deliberate structural difference from the C: PG's history lists
// are process-global `static` arrays of pointers. Here they are
// index-based (`0` = null) inside a thread-local scratch struct reused
// across calls — only the `hashsz` active hash heads are cleared per
// call, exactly like PG's `memset(hist_start, 0, hashsz * ...)`.

/// PG19 `PGLZ_MAX_HISTORY_LISTS`.
const PGLZ_HISTORY_LISTS: usize = 8192;
/// PG19 `PGLZ_HISTORY_SIZE`.
const PGLZ_HISTORY_SIZE: usize = 4096;
/// PG19 `PGLZ_MAX_MATCH`.
const PGLZ_MAX_MATCH: usize = 273;
/// Default strategy's `min_input_size`.
const PGLZ_MIN_INPUT: usize = 32;
/// Default strategy's `min_comp_rate`: payload must beat this % of input.
const PGLZ_RESULT_PCT: usize = 75;
/// Default strategy's `first_success_by`.
const PGLZ_FIRST_SUCCESS_BY: usize = 1024;
/// Default strategy's `match_size_good` (PG clamps it into `[17, 273]`).
const PGLZ_MATCH_GOOD: i32 = 128;
/// Default strategy's `match_size_drop`.
const PGLZ_MATCH_DROP: i32 = 10;
/// Match offsets must be `< 0x0fff` (the tag layout's 12-bit window).
const PGLZ_MAX_OFFSET: usize = 0x0fff;

/// Index-based mirror of PG19's `Pglz_HistEntry` history lists. PG uses
/// pointers with `NULL` for "none"; index `0` plays that role here
/// (`hist_next` starts at 1, so entry 0 is never allocated).
struct PglzHist {
    start: [u16; PGLZ_HISTORY_LISTS],
    next: [u16; PGLZ_HISTORY_SIZE + 1],
    prev: [u16; PGLZ_HISTORY_SIZE + 1],
    hindex: [u16; PGLZ_HISTORY_SIZE + 1],
    pos: [u32; PGLZ_HISTORY_SIZE + 1],
    hist_next: u16,
    recycle: bool,
}

impl PglzHist {
    const fn new() -> Self {
        Self {
            start: [0; PGLZ_HISTORY_LISTS],
            next: [0; PGLZ_HISTORY_SIZE + 1],
            prev: [0; PGLZ_HISTORY_SIZE + 1],
            hindex: [0; PGLZ_HISTORY_SIZE + 1],
            pos: [0; PGLZ_HISTORY_SIZE + 1],
            hist_next: 1,
            recycle: false,
        }
    }
}

thread_local! {
    /// Scratch history lists for [`compress_pglz`], reused across calls
    /// on the same thread (PG keeps these as process-global statics).
    static PGLZ_HIST: RefCell<PglzHist> = const { RefCell::new(PglzHist::new()) };
}

/// PG19's `pglz_hist_idx`: hash the 4 bytes at `pos` (fewer at the tail)
/// into `[0, mask]`. The C hashes over `const char *`, which is signed
/// on x86-64 Linux, so bytes `>= 0x80` are sign-extended before the
/// shifts — replicated here so the hash (and hence match choice) is
/// identical to PG's.
fn pglz_hist_idx(input: &[u8], pos: usize, end: usize, mask: i32) -> usize {
    let sb = |b: u8| b as i8 as i32;
    let h = if end - pos < 4 {
        sb(input[pos])
    } else {
        (sb(input[pos]) << 6)
            ^ (sb(input[pos + 1]) << 4)
            ^ (sb(input[pos + 2]) << 2)
            ^ sb(input[pos + 3])
    };
    // `mask` is `2^k - 1`, so this lands in `[0, mask]` even for
    // negative `h` (two's-complement AND), exactly like the C.
    (h & mask) as usize
}

/// PG19's `pglz_hist_add`: record position `pos` in the history lists,
/// recycling the oldest entry once the 4096-entry ring fills.
fn pglz_hist_add(hist: &mut PglzHist, input: &[u8], pos: usize, end: usize, mask: i32) {
    let hindex = pglz_hist_idx(input, pos, end, mask);
    let hn = hist.hist_next as usize;
    if hist.recycle {
        // Delink the recycled entry from its old list.
        let prev = hist.prev[hn] as usize;
        if prev == 0 {
            hist.start[hist.hindex[hn] as usize] = hist.next[hn];
        } else {
            hist.next[prev] = hist.next[hn];
        }
        let nxt = hist.next[hn] as usize;
        if nxt != 0 {
            hist.prev[nxt] = hist.prev[hn];
        }
    }
    // Push to the head of the new list.
    let head = hist.start[hindex] as usize;
    hist.next[hn] = head as u16;
    hist.prev[hn] = 0;
    hist.hindex[hn] = hindex as u16;
    hist.pos[hn] = pos as u32;
    // When the list was empty `head` is 0: scribbling `prev[0]` is
    // harmless scratch, exactly like the C writing through a NULL link.
    hist.prev[head] = hn as u16;
    hist.start[hindex] = hn as u16;
    let mut next = hn + 1;
    if next > PGLZ_HISTORY_SIZE {
        next = 1;
        hist.recycle = true;
    }
    hist.hist_next = next as u16;
}

/// PG19's `pglz_find_match`: the longest match at `dp` in the history
/// lists. Returns `(length, offset)` for matches longer than 2 bytes.
fn pglz_find_match(
    hist: &PglzHist,
    input: &[u8],
    dp: usize,
    end: usize,
    mask: i32,
    good_match_init: i32,
    good_drop: i32,
) -> Option<(usize, usize)> {
    let mut hent = hist.start[pglz_hist_idx(input, dp, end, mask)] as usize;
    let mut len: i32 = 0;
    let mut off: usize = 0;
    let mut good_match = good_match_init;
    while hent != 0 {
        let hp = hist.pos[hent] as usize;
        // Every entry records a position processed before `dp`.
        let thisoff = dp - hp;
        if thisoff >= PGLZ_MAX_OFFSET {
            break;
        }
        let mut thislen: i32 = 0;
        if len >= 16 {
            // Fast path: the candidate must at least repeat the best
            // match found so far at this same `dp`; `memcmp` that first.
            // `hp + n < dp + n <= end`, so both slices are in bounds.
            let n = len as usize;
            if input[dp..dp + n] == input[hp..hp + n] {
                thislen = len;
                let mut ip = dp + n;
                let mut hpp = hp + n;
                while ip < end && input[ip] == input[hpp] && thislen < PGLZ_MAX_MATCH as i32 {
                    thislen += 1;
                    ip += 1;
                    hpp += 1;
                }
            }
        } else {
            let mut ip = dp;
            let mut hpp = hp;
            while ip < end && input[ip] == input[hpp] && thislen < PGLZ_MAX_MATCH as i32 {
                thislen += 1;
                ip += 1;
                hpp += 1;
            }
        }
        if thislen > len {
            len = thislen;
            off = thisoff;
        }
        hent = hist.next[hent] as usize;
        if hent != 0 {
            if len >= good_match {
                break;
            }
            good_match -= (good_match * good_drop) / 100;
        }
    }
    if len > 2 {
        Some((len as usize, off))
    } else {
        None
    }
}

/// Emit one pending-control-byte step: PG's `pglz_out_ctrl`, which opens
/// a fresh control byte when the current one is full and patches the
/// finished byte into the output after the fact.
fn pglz_out_ctrl(out: &mut Vec<u8>, ctrlp: &mut Option<usize>, ctrlb: &mut u8, ctrl: &mut u16) {
    if *ctrl & 0xff == 0 {
        if let Some(p) = *ctrlp {
            out[p] = *ctrlb;
        }
        *ctrlp = Some(out.len());
        out.push(0);
        *ctrlb = 0;
        *ctrl = 1;
    }
}

/// Compress `input` with PG19's PGLZ (default strategy) and return the
/// framed bytes: 4-byte little-endian original length followed by the
/// raw PGLZ payload, which is byte-identical to PG19's
/// `pglz_compress()` output. Returns `None` exactly when PG19 refuses:
/// input shorter than 32 bytes, or the strategy's gates trip (payload
/// must beat 75% of the input; give up if no match by 1024 output
/// bytes).
pub fn compress_pglz(input: &[u8]) -> Option<Vec<u8>> {
    let payload = pglz_compress_payload(input)?;
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Some(out)
}

fn pglz_compress_payload(input: &[u8]) -> Option<Vec<u8>> {
    let slen = input.len();
    if slen < PGLZ_MIN_INPUT {
        return None;
    }
    // Default strategy: the payload must beat 75% of the input.
    let result_max = slen * PGLZ_RESULT_PCT / 100;
    // PG sizes the active hash table from the input length.
    let hashsz = if slen < 128 {
        512
    } else if slen < 256 {
        1024
    } else if slen < 512 {
        2048
    } else if slen < 1024 {
        4096
    } else {
        8192
    };
    let mask = (hashsz - 1) as i32;
    let good_match_init = PGLZ_MATCH_GOOD.clamp(17, PGLZ_MAX_MATCH as i32);
    PGLZ_HIST.with(|cell| {
        let hist = &mut *cell.borrow_mut();
        // PG clears only the active heads (`hashsz` of them); higher
        // slots are never read because every index is `< hashsz`.
        hist.start[..hashsz].fill(0);
        hist.hist_next = 1;
        hist.recycle = false;

        let mut out: Vec<u8> = Vec::with_capacity(slen);
        let mut ctrlp: Option<usize> = None;
        let mut ctrlb: u8 = 0;
        // `u16` so the `<<= 1` past bit 7 stays observable for the
        // `& 0xff` fullness test, like PG's `unsigned char` wraparound.
        let mut ctrl: u16 = 0;
        let mut dp = 0usize;
        let end = slen;
        let mut found_match = false;
        while dp < end {
            // PG's give-up gates, checked before every item.
            if out.len() >= result_max {
                return None;
            }
            if !found_match && out.len() >= PGLZ_FIRST_SUCCESS_BY {
                return None;
            }
            match pglz_find_match(hist, input, dp, end, mask, good_match_init, PGLZ_MATCH_DROP) {
                Some((mlen, moff)) => {
                    pglz_out_ctrl(&mut out, &mut ctrlp, &mut ctrlb, &mut ctrl);
                    // Match item: set this item's control bit...
                    ctrlb |= ctrl as u8;
                    ctrl <<= 1;
                    // ...then PG's `pglz_out_tag`.
                    if mlen > 17 {
                        out.push((((moff & 0xf00) >> 4) | 0x0f) as u8);
                        out.push((moff & 0xff) as u8);
                        out.push((mlen - 18) as u8);
                    } else {
                        out.push((((moff & 0xf00) >> 4) | (mlen - 3)) as u8);
                        out.push((moff & 0xff) as u8);
                    }
                    // Every consumed byte joins the history, like PG.
                    for _ in 0..mlen {
                        pglz_hist_add(hist, input, dp, end, mask);
                        dp += 1;
                    }
                    found_match = true;
                }
                None => {
                    pglz_out_ctrl(&mut out, &mut ctrlp, &mut ctrlb, &mut ctrl);
                    // Literal item: the control bit stays clear.
                    out.push(input[dp]);
                    ctrl <<= 1;
                    pglz_hist_add(hist, input, dp, end, mask);
                    dp += 1;
                }
            }
        }
        // Patch in the final control byte (PG: `*ctrlp = ctrlb`).
        // A non-empty input always emits at least one item, so `ctrlp`
        // is set: the give-up gates cannot fire before the first item
        // (`out` starts empty and both thresholds are positive).
        out[ctrlp.expect("non-empty input emits at least one item")] = ctrlb;
        if out.len() >= result_max {
            return None;
        }
        Some(out)
    })
}

/// Decompress data produced by [`compress_pglz`]. Returns `None` on
/// corrupt input. Test-only: the engine keeps values detoasted inline,
/// so no production path ever decompresses; the unit tests use it to
/// verify compression round-trips.
///
/// This is PG19's `pglz_decompress()` with `check_complete = true`: the
/// payload must decode to exactly the framed original length and consume
/// the entire input, otherwise the data is rejected.
#[cfg(test)]
pub fn decompress_pglz(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < 4 {
        return None;
    }
    let orig_len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
    // Sanity cap: refuse absurd allocation requests from corrupt data.
    if orig_len > 1_000_000_000 {
        return None;
    }
    let src = &input[4..];
    let mut out: Vec<u8> = Vec::with_capacity(orig_len);
    let mut sp = 0usize;
    // PG reads one control byte per group of up to eight items, least
    // significant bit first: clear = literal byte, set = match tag.
    while sp < src.len() && out.len() < orig_len {
        let mut ctrl = src[sp];
        sp += 1;
        for _ in 0..8 {
            if sp >= src.len() || out.len() >= orig_len {
                break;
            }
            if ctrl & 1 != 0 {
                if sp + 2 > src.len() {
                    return None;
                }
                let mut mlen = (src[sp] & 0x0f) as usize + 3;
                let moff = (((src[sp] & 0xf0) as usize) << 4) | src[sp + 1] as usize;
                sp += 2;
                if mlen == 18 {
                    // Long form: low nibble 0x0f, extension byte is len-18.
                    if sp >= src.len() {
                        return None;
                    }
                    mlen += src[sp] as usize;
                    sp += 1;
                }
                if moff == 0 || moff > out.len() {
                    return None;
                }
                // PG clamps the final match to the remaining output.
                let mlen = mlen.min(orig_len - out.len());
                for _ in 0..mlen {
                    let b = out[out.len() - moff];
                    out.push(b);
                }
            } else {
                out.push(src[sp]);
                sp += 1;
            }
            ctrl >>= 1;
        }
    }
    if out.len() == orig_len && sp == src.len() {
        Some(out)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// v0.41: LZ4 block codec (PG19 `lz4_compress_datum` / `lz4_decompress_datum`)
// ---------------------------------------------------------------------------
//
// Pure-std implementation of the LZ4 block format, faithful to the real
// `LZ4_compress_default()` / `LZ4_decompress_safe()` in the vendored
// `lz4.c` (`hidden_files/lz4ref/`, taken from the PG19 tree). Only the
// *format* is compatible: the encoder is a deliberately simple hash-chain
// matcher (no backward catch-up, no lazy evaluation, no skip
// acceleration), so its ratio trails the real compressor. What matters
// for `toast_compression.c` is structural validity, which the real
// `LZ4_decompress_safe()` confirms: a C harness in `hidden_files/lz4ref/`
// decodes this encoder's output with the genuine `lz4.c`.
//
// Encoder invariants (mirroring `lz4.c`'s `LZ4_compress_generic`):
// - matches are >= 4 bytes (`MINMATCH`), offsets in `1..=65535`, and
//   always point into already-written output;
// - a match never starts in the last 12 input bytes (`MFLIMIT`) and
//   never extends into the last 5 (`LASTLITERALS`), so every block ends
//   with a non-empty literals-only tail — exactly what the decoder's
//   end-of-block rules require.

/// Compress one LZ4 block (no framing). Positions are tracked as `u32`,
/// so inputs larger than `u32::MAX` bytes are not supported (the framed
/// wrapper refuses them first).
fn lz4_compress_block(input: &[u8]) -> Vec<u8> {
    const MINMATCH: usize = 4;
    const MFLIMIT: usize = 12;
    const LASTLITERALS: usize = 5;
    const DISTANCE_MAX: usize = 65535;
    const HASH_LOG: u32 = 12;

    let n = input.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + n / 255 + 16);
    if n == 0 {
        // Real LZ4 emits a single empty-literals token for empty input.
        out.push(0);
        return out;
    }
    // Last written position+1 per hash bucket; 0 = empty.
    let mut table = vec![0u32; 1 << HASH_LOG];
    let mut anchor = 0usize;
    let mut pos = 0usize;

    // Emit one LZ4 sequence: `literals`, then optionally a match of
    // `match_len` bytes at `offset` bytes back.
    let emit = |out: &mut Vec<u8>, literals: &[u8], mtch: Option<(usize, usize)>| {
        let ll = literals.len();
        let mut token: u8 = if ll >= 15 { 0xF0 } else { (ll << 4) as u8 };
        let mut ml_ext = 0usize;
        if let Some((_, match_len)) = mtch {
            let ml = match_len - MINMATCH;
            token |= if ml >= 15 { 0x0F } else { ml as u8 };
            ml_ext = ml;
        }
        out.push(token);
        if ll >= 15 {
            let mut rem = ll - 15;
            while rem >= 255 {
                out.push(255);
                rem -= 255;
            }
            out.push(rem as u8);
        }
        out.extend_from_slice(literals);
        if let Some((offset, _)) = mtch {
            out.extend_from_slice(&(offset as u16).to_le_bytes());
            if ml_ext >= 15 {
                let mut rem = ml_ext - 15;
                while rem >= 255 {
                    out.push(255);
                    rem -= 255;
                }
                out.push(rem as u8);
            }
        }
    };

    while pos + MFLIMIT <= n {
        // `pos + 12 <= n`, so reading 4 bytes at `pos` is in bounds.
        let seq = u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        let h = (seq.wrapping_mul(2654435761) >> (32 - HASH_LOG)) as usize;
        let cand_plus1 = table[h];
        table[h] = (pos + 1) as u32;
        let mut found: Option<(usize, usize)> = None;
        if cand_plus1 != 0 {
            let c = (cand_plus1 - 1) as usize;
            if c < pos && pos - c <= DISTANCE_MAX {
                // Cap the match so the last LASTLITERALS bytes stay
                // literals (`pos + 12 <= n`, so the cap is >= 7).
                let max_len = (n - LASTLITERALS) - pos;
                let mut len = 0usize;
                // `c < pos` and `len < max_len` keep `c + len < n`.
                while len < max_len && input[c + len] == input[pos + len] {
                    len += 1;
                }
                if len >= MINMATCH {
                    found = Some((pos - c, len));
                }
            }
        }
        if let Some((offset, match_len)) = found {
            emit(&mut out, &input[anchor..pos], Some((offset, match_len)));
            pos += match_len;
            anchor = pos;
        } else {
            pos += 1;
        }
    }
    // Final literals-only tail. `pos` advances by 1 or by a match ending
    // at most at `n - LASTLITERALS`, so `pos < n` and this is non-empty.
    emit(&mut out, &input[anchor..], None);
    out
}

/// Decompress one LZ4 block into exactly `out_len` bytes, mirroring the
/// acceptance rules of the real `LZ4_decompress_safe()` (vendored
/// `lz4.c`): extension-byte read limits, the last-sequence rule
/// (literals must consume exactly the remaining input), and the hard
/// "last 5 output bytes must be literals" rule. Returns `None` on any
/// corrupt or truncated input.
///
/// Test-only, like [`decompress_pglz`]: the engine keeps values
/// detoasted inline, so no production path decompresses.
#[cfg(test)]
fn lz4_decompress_block(input: &[u8], out_len: usize) -> Option<Vec<u8>> {
    const MINMATCH: usize = 4;
    const RUN_MASK: usize = 15;
    const ML_MASK: usize = 15;
    const LASTLITERALS: usize = 5;
    const MFLIMIT: usize = 12;

    let mut out = vec![0u8; out_len];
    let mut ip = 0usize;
    let mut op = 0usize;
    loop {
        if ip >= input.len() {
            return None; // truncated: expected a token
        }
        let token = input[ip];
        ip += 1;
        let mut lit_len = (token >> 4) as usize;
        if lit_len == RUN_MASK {
            // Extension bytes may not reach into the last RUN_MASK input
            // bytes (mirrors `read_variable_length`'s `iend - RUN_MASK`).
            let ilimit = input.len().checked_sub(RUN_MASK)?;
            loop {
                if ip >= ilimit {
                    return None;
                }
                let b = input[ip];
                ip += 1;
                lit_len = lit_len.checked_add(b as usize)?;
                if ip > ilimit {
                    return None;
                }
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = ip.checked_add(lit_len)?;
        let op_end = op.checked_add(lit_len)?;
        if lit_end > input.len().saturating_sub(2 + 1 + LASTLITERALS)
            || op_end > out_len.saturating_sub(MFLIMIT)
        {
            // Must be the last sequence: the literals consume exactly
            // the remaining input and fit the output.
            if lit_end != input.len() || op_end > out_len {
                return None;
            }
            out[op..op_end].copy_from_slice(&input[ip..lit_end]);
            op = op_end;
            ip = lit_end;
            break;
        }
        out[op..op_end].copy_from_slice(&input[ip..lit_end]);
        ip = lit_end;
        op = op_end;
        // Match offset.
        if ip + 2 > input.len() {
            return None;
        }
        let offset = u16::from_le_bytes([input[ip], input[ip + 1]]) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return None;
        }
        let mut match_len = (token & 0x0F) as usize;
        if match_len == ML_MASK {
            // Match-length extensions may not reach into the last
            // LASTLITERALS - 1 input bytes (`iend - LASTLITERALS + 1`).
            let ilimit = input.len().checked_sub(LASTLITERALS - 1)?;
            loop {
                if ip >= input.len() {
                    return None;
                }
                let b = input[ip];
                ip += 1;
                match_len = match_len.checked_add(b as usize)?;
                if ip > ilimit {
                    return None;
                }
                if b != 255 {
                    break;
                }
            }
        }
        match_len += MINMATCH;
        let match_end = op.checked_add(match_len)?;
        // PG's hard rule: the last LASTLITERALS output bytes must be
        // literals; a match reaching into them is corrupt.
        if out_len < LASTLITERALS || match_end > out_len - LASTLITERALS {
            return None;
        }
        for i in 0..match_len {
            out[op + i] = out[op + i - offset];
        }
        op = match_end;
    }
    if op == out_len && ip == input.len() {
        Some(out)
    } else {
        None
    }
}

/// v0.41: PG19 `lz4_compress_datum()` — LZ4-block compression of a datum,
/// in rustgres's framed form (4-byte LE original length + raw block).
/// Returns `None` when compression does not shrink the input: PG refuses
/// the compressed form when `compressed_len > original_len`
/// (`toast_compression.c`); the additional ">2 bytes net win" rule is
/// `toast_compress_datum()`'s and lives in the planner
/// ([`try_compress_attr`]), exactly like PG splits the two gates.
pub fn compress_lz4(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() > u32::MAX as usize {
        return None;
    }
    let block = lz4_compress_block(input);
    // PG's gate: refuse only when the block is *larger* than the input;
    // equal size still compresses (the >2-byte rule decides in the
    // planner).
    if block.len() > input.len() {
        return None;
    }
    let mut out = Vec::with_capacity(block.len() + 4);
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    out.extend_from_slice(&block);
    Some(out)
}

/// Framed LZ4 decompression (inverse of [`compress_lz4`]).
/// Test-only, like [`decompress_pglz`].
#[cfg(test)]
pub fn decompress_lz4(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < 4 {
        return None;
    }
    let orig_len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
    if orig_len > 1_000_000_000 {
        return None;
    }
    lz4_decompress_block(&input[4..], orig_len)
}

/// Compress with the given method (PG19 `toast_compress_datum()`'s
/// method dispatch). The `None`-when-not-smaller contract is the
/// method's own; the planner applies the shared >2-byte net-win rule.
pub fn compress_value(input: &[u8], method: crate::storage::ToastCompression) -> Option<Vec<u8>> {
    match method {
        crate::storage::ToastCompression::Pglz => compress_pglz(input),
        crate::storage::ToastCompression::Lz4 => compress_lz4(input),
    }
}

// ---------------------------------------------------------------------------
// Toast decision logic (heaptoast.c strategy)
// ---------------------------------------------------------------------------

/// How one cell is stored after toasting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastPlan {
    /// Keep inline as-is.
    Plain,
    /// Keep inline, compressed (method recorded in `toast_info`).
    Compressed,
    /// Move out-of-line (chunks in the toast table), uncompressed.
    External,
    /// Move out-of-line, compressed.
    CompressedExternal,
}

/// Byte size of a value as PG would measure it for toasting
/// (the varlena payload; NULLs are 0 and never toasted).
fn toastable_size(v: &Value) -> usize {
    match v {
        Value::Text(s) => s.len(),
        Value::BpChar(s) => s.len(),
        Value::Bytea(b) => b.len(),
        Value::Numeric(n) => n.toast_len(),
        Value::Null => 0,
        _ => 0,
    }
}

/// Raw bytes of a toastable value, for compression/chunking.
fn toastable_bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::BpChar(s) => Some(s.as_bytes().to_vec()),
        Value::Bytea(b) => Some(b.clone()),
        Value::Numeric(n) => Some(n.toast_bytes()),
        _ => None,
    }
}

/// Decide the storage plan for every cell of a row, following PG19's
/// `heap_toast_insert_or_update` (`toast_helper.c`, REL_19_STABLE):
/// 1. biggest-first over EXTENDED/EXTERNAL columns: try compression on
///    EXTENDED (mark EXTERNAL incompressible); externalize the column
///    immediately when it alone still exceeds the target;
/// 2. externalize the biggest EXTENDED/EXTERNAL column until the row
///    fits `target` (or nothing movable remains);
/// 3. biggest-first compression over MAIN columns;
/// 4. as a last resort, externalize MAIN columns against the bigger
///    `TOAST_TUPLE_TARGET_MAIN`.
///
/// Fit accounting mirrors PG: the limit is `target - hoff` where `hoff`
/// is the tuple header (23 bytes plus a null bitmap when any column is
/// NULL, 8-byte aligned); an externalized column counts
/// `TOAST_POINTER_SIZE` (20) bytes, not zero; and a column is only
/// worth moving when it exceeds `MAXALIGN(TOAST_POINTER_SIZE)` (24).
/// Compression is only kept on a savings of more than 2 bytes (PG's
/// `toast_compress_datum` rule).
///
/// `compress_ok` mirrors `default_toast_compression`: when false,
/// compression is skipped entirely (PG still externalizes).
/// Returns the per-column plan and the value ids to allocate.
///
/// Honest deviation: PG measures the whole tuple (all columns plus
/// header/alignment); here only toastable payloads are summed, so rows
/// mixing wide fixed-width columns with varlenas may toast slightly
/// earlier or later than PG.
///
/// v0.38: rounds 1-4 restructured to PG's per-iteration biggest-first
/// order (was: compress-all then externalize-biggest), externalized
/// columns now count the 20-byte toast pointer (was: zero), and the
/// limit is `target - hoff` (was: `target`).
///
/// v1.05: `reuse` carries the UPDATE fast path from PG19's
/// `toast_tuple_init` (`TOASTCOL_IGNORE`): `reuse[i] = Some(compressed)`
/// means column `i`'s datum is unchanged from the old row version, so
/// the new version keeps the old external toast pointer — the planner
/// presets `External`/`CompressedExternal` (by the old value's
/// compression flag) and every round skips the column. The slice must
/// parallel `values`; `None` toasts normally. Unchanged columns count
/// the toast-pointer width (not their datum bytes) toward the row
/// width, exactly as PG19 measures the new tuple.
pub fn plan_toast(
    table: &Table,
    values: &[Value],
    compress_ok: bool,
    default_method: crate::storage::ToastCompression,
    reuse: &[Option<bool>],
) -> Vec<ToastPlan> {
    let n = values.len();
    let mut plan = vec![ToastPlan::Plain; n];
    // v1.05: preset reused columns before any round runs.
    for (i, r) in reuse.iter().enumerate().take(n) {
        if let Some(compressed) = r {
            plan[i] = if *compressed {
                ToastPlan::CompressedExternal
            } else {
                ToastPlan::External
            };
        }
    }
    // Only toastable columns with toastable values participate.
    let eligible: Vec<usize> = (0..n)
        .filter(|&i| {
            table
                .col_storage
                .get(i)
                .copied()
                .unwrap_or(toast_storage::PLAIN)
                != toast_storage::PLAIN
                && toastable_size(&values[i]) > 0
        })
        .collect();
    if eligible.is_empty() {
        return plan;
    }
    // Row width PG would measure: sum of toastable payloads. v1.05:
    // reused (unchanged) columns already carry a toast pointer in the
    // new tuple, so they count the pointer width, not their datum
    // bytes — mirroring PG19's heap_compute_data_size on the new tuple.
    let width: usize = eligible
        .iter()
        .map(|&i| {
            if reuse.get(i).copied().flatten().is_some() {
                TOAST_POINTER_SIZE
            } else {
                toastable_size(&values[i])
            }
        })
        .sum();
    if width <= toast_consts::TOAST_TUPLE_THRESHOLD as usize {
        return plan;
    }
    // PG's tuple header: SizeofHeapTupleHeader (23) plus a null bitmap
    // when any column is NULL, 8-byte aligned.
    let has_null = values.iter().any(|v| matches!(v, Value::Null));
    let hoff = if has_null {
        (23 + n.div_ceil(8) + 7) & !7
    } else {
        (23 + 7) & !7
    };
    let target = (table.toast_target as usize).saturating_sub(hoff);
    let main_target = (toast_consts::TOAST_TUPLE_TARGET_MAIN as usize).saturating_sub(hoff);

    // Sizes as currently planned (compressed sizes once compressed,
    // TOAST_POINTER_SIZE once externalized).
    let mut sizes: Vec<usize> = (0..n).map(|i| toastable_size(&values[i])).collect();
    // Compressed payloads, filled in by the compression rounds.
    let mut compressed: Vec<Option<Vec<u8>>> = vec![None; n];
    // Columns proven incompressible (PG's TOASTCOL_INCOMPRESSIBLE):
    // skipped by later compression passes, still movable out-of-line.
    let mut incompressible = vec![false; n];
    // v0.41: PG19 resolves a column without an explicit compression
    // method to the session's `default_toast_compression` at compression
    // time (`toast_tuple_try_compression` consults `attcompression`,
    // falling back to the GUC when it is invalid/default).
    let method_for = |i: usize| {
        table
            .col_compression
            .get(i)
            .copied()
            .flatten()
            .unwrap_or(default_method)
    };

    // Current row width: externalized columns count the toast pointer.
    let cur_width = |plan: &[ToastPlan], sizes: &[usize]| -> usize {
        eligible
            .iter()
            .map(|&i| {
                if matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal) {
                    TOAST_POINTER_SIZE
                } else {
                    sizes[i]
                }
            })
            .sum()
    };

    // Round 1 (PG's first loop): biggest-first over EXTENDED/EXTERNAL;
    // compress EXTENDED (mark EXTERNAL incompressible); a column that
    // alone still exceeds the target moves out-of-line immediately.
    loop {
        if cur_width(&plan, &sizes) <= target {
            break;
        }
        let Some(b) = biggest_attr(
            table,
            &eligible,
            &plan,
            &sizes,
            &compressed,
            &incompressible,
            true,
            false,
        ) else {
            break;
        };
        if table.col_storage[b] == toast_storage::EXTENDED {
            try_compress_attr(
                b,
                values,
                compress_ok,
                method_for(b),
                &mut plan,
                &mut sizes,
                &mut compressed,
                &mut incompressible,
            );
        } else {
            incompressible[b] = true;
        }
        if sizes[b] > target {
            externalize_attr(b, &mut plan, &compressed);
        }
    }

    // Round 2 (PG's second loop): externalize the biggest
    // EXTENDED/EXTERNAL column until the row fits.
    loop {
        if cur_width(&plan, &sizes) <= target {
            break;
        }
        let Some(b) = biggest_attr(
            table,
            &eligible,
            &plan,
            &sizes,
            &compressed,
            &incompressible,
            false,
            false,
        ) else {
            break;
        };
        externalize_attr(b, &mut plan, &compressed);
    }

    // Round 3 (PG's third loop): biggest-first compression over MAIN.
    loop {
        if cur_width(&plan, &sizes) <= target {
            break;
        }
        let Some(b) = biggest_attr(
            table,
            &eligible,
            &plan,
            &sizes,
            &compressed,
            &incompressible,
            true,
            true,
        ) else {
            break;
        };
        try_compress_attr(
            b,
            values,
            compress_ok,
            method_for(b),
            &mut plan,
            &mut sizes,
            &mut compressed,
            &mut incompressible,
        );
    }

    // Round 4 (PG's fourth loop): externalize MAIN as a last resort
    // against the bigger MAIN target.
    loop {
        if cur_width(&plan, &sizes) <= main_target {
            break;
        }
        let Some(b) = biggest_attr(
            table,
            &eligible,
            &plan,
            &sizes,
            &compressed,
            &incompressible,
            false,
            true,
        ) else {
            break;
        };
        externalize_attr(b, &mut plan, &compressed);
    }

    plan
}

/// v1.05: the per-column reuse mask for an UPDATE's new row version —
/// PG19's unchanged-column fast path in `toast_tuple_init`
/// (`toast_helper.c`, REL_19_STABLE: columns whose datum is unchanged
/// keep the old external toast pointer, `TOASTCOL_IGNORE`, instead of
/// being re-toasted).
///
/// `old_values`/`old_toast` are the superseded version's detoasted
/// values and per-column toast flags in table column order; `values`
/// is the new version's. Returns `Some(compressed)` for a column whose
/// old value was toasted, whose detoasted bytes are unchanged, and
/// whose value id still has provenance in `toast_info` — the bool is
/// the old value's compression flag, so the planner presets the
/// matching external plan. All other columns return `None` (toast
/// normally; their old chunks are deleted by the caller).
///
/// Byte-equality is a deliberate, unobservable superset of PG19's rule
/// (which keys off the column not being in the UPDATE's SET list):
/// value ids are engine-internal (never exposed in SQL), and a
/// SET-to-the-identical-bytes value reuses the identical chunk rows,
/// so no query can distinguish the two.
pub fn toast_update_reuse(
    old_values: &[Value],
    old_toast: &[u32],
    values: &[Value],
    toast_info: &std::collections::HashMap<u32, ToastInfo>,
) -> Vec<Option<bool>> {
    let n = values.len();
    (0..n)
        .map(|i| {
            let vid = old_toast.get(i).copied().unwrap_or(0);
            if vid == 0 {
                return None;
            }
            let info = toast_info.get(&vid)?;
            let old_bytes = old_values.get(i).and_then(toastable_bytes);
            let new_bytes = values.get(i).and_then(toastable_bytes);
            match (old_bytes, new_bytes) {
                (Some(o), Some(nw)) if o == nw => Some(info.compressed),
                _ => None,
            }
        })
        .collect()
}

/// PG's `toast_tuple_find_biggest_attribute`: index of the largest
/// eligible column bigger than `MAXALIGN(TOAST_POINTER_SIZE)`; for
/// compression passes also skips incompressible and already-compressed
/// columns. `main_only` selects MAIN columns (rounds 3-4) instead of
/// EXTENDED/EXTERNAL (rounds 1-2).
#[allow(clippy::too_many_arguments)]
fn biggest_attr(
    table: &Table,
    eligible: &[usize],
    plan: &[ToastPlan],
    sizes: &[usize],
    compressed: &[Option<Vec<u8>>],
    incompressible: &[bool],
    for_compression: bool,
    main_only: bool,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for &i in eligible {
        if matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal) {
            continue;
        }
        let storage = table.col_storage[i];
        if main_only {
            if storage != toast_storage::MAIN {
                continue;
            }
        } else if !matches!(storage, toast_storage::EXTENDED | toast_storage::EXTERNAL) {
            continue;
        }
        if for_compression && (incompressible[i] || compressed[i].is_some()) {
            continue;
        }
        if sizes[i] <= MAXALIGN_TOAST_POINTER {
            continue;
        }
        if best.map_or(true, |b| sizes[i] > sizes[b]) {
            best = Some(i);
        }
    }
    best
}

/// Try PG-style compression on one column: on success the plan and size
/// update, on failure the column is marked incompressible. PG only
/// keeps the compressed form on a savings of more than 2 bytes
/// (`toast_compress_datum`); the per-method "refuse when not smaller"
/// gate is the compressor's own (`pglz_compress` /
/// `lz4_compress_datum`).
#[allow(clippy::too_many_arguments)]
fn try_compress_attr(
    i: usize,
    values: &[Value],
    compress_ok: bool,
    method: crate::storage::ToastCompression,
    plan: &mut [ToastPlan],
    sizes: &mut [usize],
    compressed: &mut [Option<Vec<u8>>],
    incompressible: &mut [bool],
) {
    if !compress_ok {
        incompressible[i] = true;
        return;
    }
    let raw = match toastable_bytes(&values[i]) {
        Some(raw) => raw,
        None => {
            incompressible[i] = true;
            return;
        }
    };
    match compress_value(&raw, method) {
        Some(c) if c.len() + 2 < raw.len() => {
            sizes[i] = c.len();
            compressed[i] = Some(c);
            plan[i] = ToastPlan::Compressed;
        }
        _ => {
            incompressible[i] = true;
        }
    }
}

/// Move one column out-of-line (PG's `toast_tuple_externalize`).
fn externalize_attr(i: usize, plan: &mut [ToastPlan], compressed: &[Option<Vec<u8>>]) {
    plan[i] = if compressed[i].is_some() {
        ToastPlan::CompressedExternal
    } else {
        ToastPlan::External
    };
}

/// Split bytes into toast-table chunks (`TOAST_MAX_CHUNK_SIZE` each).
pub fn chunk_bytes(data: &[u8]) -> Vec<&[u8]> {
    data.chunks(toast_consts::TOAST_MAX_CHUNK_SIZE).collect()
}

/// Get the bytes to store out-of-line for a cell with an
/// External/CompressedExternal plan. Returns `None` for inline plans.
/// Recompresses deterministically (same result as `plan_toast` saw).
pub fn out_of_line_bytes(
    values: &[Value],
    idx: usize,
    plan: ToastPlan,
    compress_ok: bool,
    method: crate::storage::ToastCompression,
) -> Option<Vec<u8>> {
    match plan {
        ToastPlan::Plain | ToastPlan::Compressed => None,
        ToastPlan::External => toastable_bytes(&values[idx]),
        ToastPlan::CompressedExternal => {
            let raw = toastable_bytes(&values[idx])?;
            if compress_ok {
                Some(compress_value(&raw, method).unwrap_or(raw))
            } else {
                Some(raw)
            }
        }
    }
}

/// v0.37: record a toasted cell in the table's `toast_info`.
/// v0.41: also records which compressor produced the stored bytes.
pub fn record_toast_info(
    table: &mut Table,
    value_id: u32,
    compressed: bool,
    method: crate::storage::ToastCompression,
) {
    table
        .toast_info
        .insert(value_id, ToastInfo { compressed, method });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pglz_roundtrip_repetitive() {
        let data: Vec<u8> = "abcdefgh".repeat(1000).into_bytes();
        let c = compress_pglz(&data).expect("should compress");
        assert!(c.len() < data.len() / 4);
        assert_eq!(decompress_pglz(&c).unwrap(), data);
    }

    #[test]
    fn pglz_roundtrip_random_no_win() {
        // Deterministic pseudo-random bytes: incompressible.
        let mut x: u64 = 0x12345678;
        let data: Vec<u8> = (0..2000)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                (x >> 33) as u8
            })
            .collect();
        assert!(compress_pglz(&data).is_none());
    }

    #[test]
    fn pglz_small_input_no_compress() {
        assert!(compress_pglz(b"hello").is_none());
    }

    #[test]
    fn pglz_decompress_rejects_garbage() {
        assert!(decompress_pglz(b"junk").is_none());
        assert!(decompress_pglz(&[1, 2, 3]).is_none());
    }

    #[test]
    fn pglz_all_same_byte() {
        let data = vec![0xABu8; 5000];
        let c = compress_pglz(&data).expect("should compress");
        // 5000 identical bytes should compress well (PG's long-form
        // match tags encode up to 273 bytes per match).
        assert!(c.len() < 200);
        assert_eq!(decompress_pglz(&c).unwrap(), data);
    }

    /// Golden vectors: the payloads below are the verbatim output of
    /// PG19's `pglz_compress()` (REL_19_STABLE `src/common/pg_lzcompress.c`,
    /// compiled with gcc and run with `PGLZ_strategy_default`), framed
    /// with rustgres's 4-byte little-endian original length.
    #[test]
    fn pglz_golden_vectors_match_pg19() {
        // "ab" * 32: two literals, then one long-form match (len 62, off 2).
        let c = compress_pglz(&b"ab".repeat(32)).expect("should compress");
        assert_eq!(
            c,
            [
                64, 0, 0, 0, // framed original length
                0x04, 0x61, 0x62, 0x0f, 0x02, 0x2c,
            ]
            .as_slice()
        );
        // "X" * 100: one literal, then one long-form match (len 99, off 1).
        let c = compress_pglz(&vec![b'X'; 100]).expect("should compress");
        assert_eq!(c, [100, 0, 0, 0, 0x02, 0x58, 0x0f, 0x01, 0x51].as_slice());
        // 200 bytes of repeated English: 45 literals (a fresh control
        // byte every 8), then a long-form match (len 155, off 45).
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(5)[..200].to_vec();
        let c = compress_pglz(&data).expect("should compress");
        let mut expected = vec![200, 0, 0, 0];
        for chunk in data[..40].chunks(8) {
            expected.push(0x00);
            expected.extend_from_slice(chunk);
        }
        expected.push(0x20); // 5 literals, then a match
        expected.extend_from_slice(&data[40..45]);
        expected.extend_from_slice(&[0x0f, 0x2d, 0x89]);
        assert_eq!(c, expected.as_slice());
        assert_eq!(decompress_pglz(&c).unwrap(), data);
    }

    #[test]
    fn pglz_pg19_refusal_rules() {
        // PG's default strategy needs at least 32 input bytes...
        assert!(compress_pglz(&vec![b'A'; 31]).is_none());
        // ...and now attempts inputs the old 64-byte minimum skipped.
        assert!(compress_pglz(&vec![b'A'; 32]).is_some());
        // ...and refuses unless the payload beats 75% of the input
        // (result_max = 30 here): PG19's C oracle returns -1 for this
        // 40-byte input, so rustgres must too.
        let data = [
            0x67, 0x61, 0x33, 0x99, 0x2d, 0xb3, 0x89, 0x16, 0xc2, 0x16, //
            0x40, 0xba, 0x62, 0x87, 0xaf, 0xb3, 0x89, 0x16, 0xf0, 0x9f, //
            0x7d, 0xd2, 0x63, 0xdc, 0x9c, 0x96, 0x37, 0xca, 0xe6, 0xc9, //
            0x5f, 0xd2, 0x63, 0xdc, 0x9c, 0x96, 0x26, 0x76, 0xa9, 0x2d,
        ];
        assert!(compress_pglz(&data).is_none());
    }

    #[test]
    fn pglz_decompress_rejects_truncated_tags() {
        let data = vec![0xABu8; 500];
        let c = compress_pglz(&data).expect("should compress");
        // Truncated payload.
        assert!(decompress_pglz(&c[..c.len() - 1]).is_none());
        // Trailing garbage after a complete payload.
        let mut d = c.clone();
        d.push(0x00);
        assert!(decompress_pglz(&d).is_none());
        // Wrong framed length.
        let mut d = c.clone();
        d[0] = 0x01;
        assert!(decompress_pglz(&d).is_none());
    }

    // --- v0.41: LZ4 ---

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The Rust decoder must accept genuine `lz4.c`
    /// `LZ4_compress_default()` output (vendored vectors).
    #[test]
    fn lz4_decoder_accepts_real_c_vectors() {
        let fixture = include_str!("../tests/data/lz4_vectors.txt");
        let mut n = 0;
        for line in fixture.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts.len(), 4, "bad vector line: {}", line);
            let orig_len: usize = parts[0].parse().unwrap();
            let comp = hex_decode(parts[2]);
            let orig = hex_decode(parts[3]);
            assert_eq!(orig.len(), orig_len);
            // Frame like compress_lz4 does, then decode.
            let mut framed = (orig_len as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&comp);
            assert_eq!(
                decompress_lz4(&framed).unwrap(),
                orig,
                "vector kind={} len={}",
                parts[1],
                orig_len
            );
            n += 1;
        }
        assert!(n >= 11, "expected 11 vectors, got {}", n);
    }

    #[test]
    fn lz4_encoder_roundtrip_battery() {
        let mut seed: u64 = 0xdeadbeef;
        let mut rnd = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u8
        };
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for &n in &[0usize, 1, 5, 12, 13, 64, 256, 1024, 4096] {
            cases.push((0..n).map(|_| rnd()).collect());
        }
        for &n in &[64usize, 1024, 8192] {
            let t = b"the quick brown fox jumps over the lazy dog. ";
            cases.push((0..n).map(|i| t[i % t.len()]).collect());
        }
        cases.push(vec![b'Q'; 5000]);
        cases.push(
            (0..2000)
                .map(|i| if i % 2 == 0 { b'a' } else { b'b' })
                .collect(),
        );
        cases.push((0..3000).map(|i| (i % 251) as u8).collect());
        for data in &cases {
            match compress_lz4(data) {
                None => {
                    // PG's gate: refused only when the block would be
                    // *larger* than the input.
                    let block = lz4_compress_block(data);
                    assert!(
                        block.len() > data.len(),
                        "refused compressible input of len {}",
                        data.len()
                    );
                }
                Some(c) => {
                    // Framed form: 4-byte LE original length + raw block.
                    assert!(c.len() >= 4);
                    assert_eq!(
                        u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as usize,
                        data.len()
                    );
                    assert!(c.len() - 4 <= data.len());
                    assert_eq!(decompress_lz4(&c).unwrap(), *data);
                }
            }
        }
    }

    #[test]
    fn lz4_decoder_rejects_corrupt() {
        let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog. ".repeat(40);
        let c = compress_lz4(&data).expect("should compress");
        // Truncated block.
        assert!(decompress_lz4(&c[..c.len() - 1]).is_none());
        // Truncated frame.
        assert!(decompress_lz4(&c[..3]).is_none());
        // Garbage token stream.
        let mut bad = (data.len() as u32).to_le_bytes().to_vec();
        bad.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(decompress_lz4(&bad).is_none());
        // Offset pointing before the output (offset > bytes written).
        let mut bad = (data.len() as u32).to_le_bytes().to_vec();
        bad.push(0x10); // 1 literal, match token
        bad.push(b'X'); // the literal
        bad.extend_from_slice(&[0xFF, 0x00]); // offset 255, no history
        assert!(decompress_lz4(&bad).is_none());
        // Wrong framed length.
        let mut bad = c.clone();
        bad[0] ^= 0xFF;
        assert!(decompress_lz4(&bad).is_none());
    }

    #[test]
    fn lz4_method_dispatch_differs_from_pglz() {
        use crate::storage::ToastCompression;
        let data: Vec<u8> = b"The quick brown fox jumps over the lazy dog. ".repeat(60);
        let p = compress_value(&data, ToastCompression::Pglz).expect("pglz");
        let l = compress_value(&data, ToastCompression::Lz4).expect("lz4");
        // Different algorithms: payloads differ (both are valid).
        assert_ne!(p, l);
        assert_eq!(decompress_pglz(&p).unwrap(), data);
        assert_eq!(decompress_lz4(&l).unwrap(), data);
    }
    // --- v1.05: UPDATE toast reuse (PG19 TOASTCOL_IGNORE) ---

    fn wide_text(seed: u64, n: usize) -> String {
        // Deterministic incompressible text (PGLZ gives up on it).
        let mut x: u64 = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                (b'a' + ((x >> 33) % 26) as u8) as char
            })
            .collect()
    }

    fn text_table() -> Table {
        Table::new(
            vec![
                ("a".to_string(), crate::storage::ColType::Text),
                ("b".to_string(), crate::storage::ColType::Text),
            ],
            1,
        )
    }

    #[test]
    fn update_reuse_unchanged_column() {
        let old_values = vec![
            Value::Text(wide_text(1, 6000).into()),
            Value::Text("small".to_string().into()),
        ];
        let new_values = vec![
            Value::Text(wide_text(1, 6000).into()), // identical bytes
            Value::Text("changed".to_string().into()),
        ];
        let mut info = std::collections::HashMap::new();
        info.insert(
            7u32,
            ToastInfo {
                compressed: false,
                method: crate::storage::ToastCompression::Pglz,
            },
        );
        let reuse = toast_update_reuse(&old_values, &[7, 0], &new_values, &info);
        assert_eq!(reuse, vec![Some(false), None]);
    }

    #[test]
    fn update_reuse_changed_column_not_reused() {
        let old_values = vec![Value::Text(wide_text(1, 6000).into())];
        let new_values = vec![Value::Text(wide_text(2, 6000).into())];
        let mut info = std::collections::HashMap::new();
        info.insert(
            7u32,
            ToastInfo {
                compressed: false,
                method: crate::storage::ToastCompression::Pglz,
            },
        );
        let reuse = toast_update_reuse(&old_values, &[7], &new_values, &info);
        assert_eq!(reuse, vec![None]);
    }

    #[test]
    fn update_reuse_missing_provenance_not_reused() {
        // Value id with no toast_info entry (e.g. metadata already
        // pruned): never reuse a pointer we cannot describe.
        let t = text_table();
        let v = vec![Value::Text(wide_text(1, 6000).into())];
        let reuse = toast_update_reuse(&v, &[7], &v, &std::collections::HashMap::new());
        assert_eq!(reuse, vec![None]);
    }

    #[test]
    fn update_reuse_compressed_flag_carried() {
        let v = vec![Value::Text(wide_text(1, 6000).into())];
        let mut info = std::collections::HashMap::new();
        info.insert(
            9u32,
            ToastInfo {
                compressed: true,
                method: crate::storage::ToastCompression::Pglz,
            },
        );
        let reuse = toast_update_reuse(&v, &[9], &v, &info);
        assert_eq!(reuse, vec![Some(true)]);
    }

    #[test]
    fn plan_toast_reuse_presets_external_and_skips_rounds() {
        let t = text_table();
        // Column 0: 6000 incompressible bytes, unchanged (reuse).
        // Column 1: 6000 incompressible bytes, changed.
        let w = wide_text(42, 6000);
        let values = vec![
            Value::Text(w.clone().into()),
            Value::Text(wide_text(43, 6000).into()),
        ];
        let plan = plan_toast(
            &t,
            &values,
            true,
            crate::storage::ToastCompression::Pglz,
            &[Some(false), None],
        );
        // Reused column keeps its external pointer; the changed column
        // is externalized by the rounds.
        assert_eq!(plan[0], ToastPlan::External);
        assert_eq!(plan[1], ToastPlan::External);
    }

    #[test]
    fn plan_toast_reuse_counts_pointer_width() {
        let t = text_table();
        // Only the reused column is wide; the other is small. The row
        // must NOT toast the small column: the reused column counts 20
        // bytes, keeping the width under the threshold.
        let values = vec![
            Value::Text(wide_text(7, 6000).into()),
            Value::Text("tiny".to_string().into()),
        ];
        let plan = plan_toast(
            &t,
            &values,
            true,
            crate::storage::ToastCompression::Pglz,
            &[Some(false), None],
        );
        assert_eq!(plan[0], ToastPlan::External);
        assert_eq!(plan[1], ToastPlan::Plain);
    }

    #[test]
    fn plan_toast_no_reuse_unchanged_behavior() {
        // Without reuse the planner behaves exactly as before: a wide
        // incompressible column externalizes.
        let t = text_table();
        let values = vec![Value::Text(wide_text(11, 6000).into())];
        let plan = plan_toast(
            &t,
            &values,
            true,
            crate::storage::ToastCompression::Pglz,
            &[None],
        );
        assert_eq!(plan[0], ToastPlan::External);
    }
}
