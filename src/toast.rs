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
//! - `default_toast_compression = lz4` is accepted but maps to pglz:
//!   rustgres has no LZ4 implementation (documented in server.rs).

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
// Toast decision logic (heaptoast.c strategy)
// ---------------------------------------------------------------------------

/// How one cell is stored after toasting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastPlan {
    /// Keep inline as-is.
    Plain,
    /// Keep inline, compressed (pglz).
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
pub fn plan_toast(table: &Table, values: &[Value], compress_ok: bool) -> Vec<ToastPlan> {
    let n = values.len();
    let mut plan = vec![ToastPlan::Plain; n];
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
    // Row width PG would measure: sum of toastable payloads.
    let width: usize = eligible.iter().map(|&i| toastable_size(&values[i])).sum();
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
/// (`toast_compress_datum`).
#[allow(clippy::too_many_arguments)]
fn try_compress_attr(
    i: usize,
    values: &[Value],
    compress_ok: bool,
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
    match compress_pglz(&raw) {
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
) -> Option<Vec<u8>> {
    match plan {
        ToastPlan::Plain | ToastPlan::Compressed => None,
        ToastPlan::External => toastable_bytes(&values[idx]),
        ToastPlan::CompressedExternal => {
            let raw = toastable_bytes(&values[idx])?;
            if compress_ok {
                Some(compress_pglz(&raw).unwrap_or(raw))
            } else {
                Some(raw)
            }
        }
    }
}

/// v0.37: record a toasted cell in the table's `toast_info`.
pub fn record_toast_info(table: &mut Table, value_id: u32, compressed: bool) {
    table.toast_info.insert(value_id, ToastInfo { compressed });
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
}
