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
//! - The compressor is LZ77 with PGLZ-style parameters (12-bit
//!   window, short matches), but its byte format is NOT bit-compatible
//!   with PG's PGLZ — only rustgres reads it back, and
//!   `pg_column_compression` reports the conventional `'pglz'` name.
//! - `default_toast_compression = lz4` is accepted but maps to pglz:
//!   rustgres has no LZ4 implementation (documented in server.rs).

use crate::storage::{Table, ToastInfo, Value, toast_consts, toast_storage};

// ---------------------------------------------------------------------------
// PGLZ-inspired LZ77 compressor
// ---------------------------------------------------------------------------

/// Minimum input size worth attempting to compress (PG's
/// `PGLZ_min_comp_size` is 64 in recent versions; smaller inputs never
/// win against the header overhead).
const MIN_COMPRESS_SIZE: usize = 64;
/// Sliding window: 12 bits of offset, like PGLZ.
const WINDOW_SIZE: usize = 4096;
/// Minimum match length.
const MIN_MATCH: usize = 3;
/// Maximum match length encoded in one token.
const MAX_MATCH: usize = 130;

/// Compress `input` with a simple LZ77. Returns `None` when the input
/// is too small or the compressed form would not be smaller (PG only
/// keeps the compressed form on a real win).
pub fn compress_pglz(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() < MIN_COMPRESS_SIZE {
        return None;
    }
    // Hash 3-byte sequences to candidate positions (open addressing,
    // keeps the last position per hash).
    let mut tab = vec![usize::MAX; 1 << 16];
    let mut out = Vec::with_capacity(input.len());
    // Header: original length, so the decompressor knows when to stop
    // and can pre-size the output.
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());

    let mut i = 0usize;
    let mut lit_start = 0usize;
    while i < input.len() {
        let mut best_len = 0usize;
        let mut best_off = 0usize;
        if i + MIN_MATCH <= input.len() {
            let h = hash3(&input[i..i + 3.min(input.len() - i)]);
            let cand = tab[h];
            if cand != usize::MAX && i - cand <= WINDOW_SIZE && cand < i {
                // Extend the match.
                let max_len = (input.len() - i).min(MAX_MATCH);
                let mut len = 0;
                while len < max_len && input[cand + len] == input[i + len] {
                    len += 1;
                }
                if len >= MIN_MATCH {
                    best_len = len;
                    best_off = i - cand;
                }
            }
            tab[h] = i;
        }
        if best_len >= MIN_MATCH {
            flush_literals(&mut out, &input[lit_start..i]);
            // Match token: 1LLLLLLL, u16 LE offset; length = L+3.
            out.push(0x80 | ((best_len - MIN_MATCH) as u8));
            out.extend_from_slice(&(best_off as u16).to_le_bytes());
            // Feed the skipped bytes into the table so later matches
            // can reference them.
            for j in (i + 1)..(i + best_len) {
                if j + MIN_MATCH <= input.len() {
                    let h = hash3(&input[j..j + 3.min(input.len() - j)]);
                    tab[h] = j;
                }
            }
            i += best_len;
            lit_start = i;
        } else {
            i += 1;
        }
    }
    flush_literals(&mut out, &input[lit_start..]);

    if out.len() < input.len() {
        Some(out)
    } else {
        None
    }
}

/// Decompress data produced by [`compress_pglz`]. Returns `None` on
/// corrupt input. Test-only: the engine keeps values detoasted inline,
/// so no production path ever decompresses; the unit tests use it to
/// verify compression round-trips.
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
    let mut out = Vec::with_capacity(orig_len);
    let mut i = 4usize;
    while i < input.len() {
        let tag = input[i];
        i += 1;
        if tag & 0x80 == 0 {
            // Literal run: (tag & 0x7F) + 1 bytes follow.
            let n = (tag & 0x7F) as usize + 1;
            if i + n > input.len() {
                return None;
            }
            out.extend_from_slice(&input[i..i + n]);
            i += n;
        } else {
            // Match: length (tag & 0x7F) + 3, u16 LE offset.
            let len = (tag & 0x7F) as usize + MIN_MATCH;
            if i + 2 > input.len() {
                return None;
            }
            let off = u16::from_le_bytes([input[i], input[i + 1]]) as usize;
            i += 2;
            if off == 0 || off > out.len() {
                return None;
            }
            for _ in 0..len {
                let b = out[out.len() - off];
                out.push(b);
            }
        }
    }
    if out.len() == orig_len {
        Some(out)
    } else {
        None
    }
}

fn hash3(b: &[u8]) -> usize {
    // b has at least 1 byte; use up to 3.
    let mut h = 0usize;
    for (i, &byte) in b.iter().take(3).enumerate() {
        h |= (byte as usize) << (8 * i);
    }
    h & 0xFFFF
}

fn flush_literals(out: &mut Vec<u8>, lit: &[u8]) {
    let mut rest = lit;
    while !rest.is_empty() {
        // Literal token holds up to 128 bytes (0x00..=0x7F => 1..=128).
        let n = rest.len().min(128);
        out.push((n - 1) as u8);
        out.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
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

/// Decide the storage plan for every cell of a row, following PG's
/// `toast_insert_or_update` order:
/// 1. try to compress EXTENDED columns;
/// 2. move the biggest EXTENDED/EXTERNAL values out-of-line until the
///    row fits `target` (or nothing toastable remains);
/// 3. try to compress MAIN columns;
/// 4. move MAIN values out-of-line as a last resort (bigger target).
///
/// `compress_ok` mirrors `default_toast_compression`: when false,
/// compression is skipped entirely (PG still externalizes).
/// Returns the per-column plan and the value ids to allocate.
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
    let target = table.toast_target as usize;

    // Sizes as currently planned (compressed sizes once compressed).
    let mut sizes: Vec<usize> = (0..n).map(|i| toastable_size(&values[i])).collect();
    // Compressed payloads, filled in by the compression rounds.
    let mut compressed: Vec<Option<Vec<u8>>> = vec![None; n];

    // Round 1: compress EXTENDED columns.
    if compress_ok {
        for &i in &eligible {
            if table.col_storage[i] != toast_storage::EXTENDED {
                continue;
            }
            if let Some(raw) = toastable_bytes(&values[i]) {
                if let Some(c) = compress_pglz(&raw) {
                    sizes[i] = c.len();
                    compressed[i] = Some(c);
                    plan[i] = ToastPlan::Compressed;
                }
            }
        }
    }

    // Round 2: externalize biggest EXTENDED/EXTERNAL until under target.
    // PG picks the largest eligible attribute each iteration.
    loop {
        let cur: usize = eligible
            .iter()
            .filter(|&&i| !matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal))
            .map(|&i| sizes[i])
            .sum();
        if cur <= target {
            break;
        }
        let mut best: Option<usize> = None;
        for &i in &eligible {
            if !matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal)
                && matches!(
                    table.col_storage[i],
                    toast_storage::EXTENDED | toast_storage::EXTERNAL
                )
            {
                if best.map_or(true, |b| sizes[i] > sizes[b]) {
                    best = Some(i);
                }
            }
        }
        let Some(b) = best else { break };
        plan[b] = if compressed[b].is_some() {
            ToastPlan::CompressedExternal
        } else {
            ToastPlan::External
        };
    }

    // Round 3: compress MAIN columns.
    if compress_ok {
        for &i in &eligible {
            if table.col_storage[i] != toast_storage::MAIN {
                continue;
            }
            if plan[i] != ToastPlan::Plain {
                continue;
            }
            if let Some(raw) = toastable_bytes(&values[i]) {
                if let Some(c) = compress_pglz(&raw) {
                    sizes[i] = c.len();
                    compressed[i] = Some(c);
                    plan[i] = ToastPlan::Compressed;
                }
            }
        }
    }

    // Round 4: externalize MAIN as a last resort (bigger target).
    let main_target = toast_consts::TOAST_TUPLE_TARGET_MAIN as usize;
    loop {
        let cur: usize = eligible
            .iter()
            .filter(|&&i| !matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal))
            .map(|&i| sizes[i])
            .sum();
        if cur <= main_target {
            break;
        }
        let mut best: Option<usize> = None;
        for &i in &eligible {
            if !matches!(plan[i], ToastPlan::External | ToastPlan::CompressedExternal)
                && table.col_storage[i] == toast_storage::MAIN
            {
                if best.map_or(true, |b| sizes[i] > sizes[b]) {
                    best = Some(i);
                }
            }
        }
        let Some(b) = best else { break };
        plan[b] = if compressed[b].is_some() {
            ToastPlan::CompressedExternal
        } else {
            ToastPlan::External
        };
    }

    plan
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
        // 5000 identical bytes should compress well (format uses
        // 3-byte match tokens with max 130-byte matches).
        assert!(c.len() < 200);
        assert_eq!(decompress_pglz(&c).unwrap(), data);
    }
}
