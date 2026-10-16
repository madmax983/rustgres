//! v1.94: text-search types — `tsvector` (OID 3614) and `tsquery`
//! (OID 3615), the `@@` match operator, the `to_tsvector` /
//! `to_tsquery` families, `ts_rank` / `ts_rank_cd`, and the tsvector
//! utility functions. A faithful port of the PG19 sources under
//! `src/backend/utils/adt/` (`tsvector.c`, `tsvector_parser.c`,
//! `tsvector_op.c`, `tsquery.c`, `tsquery_cleanup.c`, `tsquery_op.c`,
//! `tsquery_util.c`, `tsrank.c`, `to_tsany.c`, `ts_parse.c`) plus the
//! Snowball english stemmer (`src/backend/snowball/libstemmer/
//! stem_UTF_8_english.c`) and the english stop-word list
//! (`src/backend/snowball/stopwords/english.stop`).
//!
//! Data model (in `storage.rs`): `TsVectorVal` is a sorted list of
//! `(lexeme, positions)` with positions `(pos, weight)`, pos in
//! 1..=16383 and weight 0..=3 (D=0, C=1, B=2, A=3); an empty position
//! list is a stripped entry. `TsQueryVal` is a tree of `TsQueryNode`
//! whose operand `id`s are assigned in polish (prefix) order, exactly
//! like PG's QueryItem array layout.

use super::*;
use crate::storage::{TsQueryNode, TsQueryVal, TsVectorVal};
use std::cmp::Ordering;

/// PG's MAXNUMPOS: maximum positions stored per lexeme.
pub(crate) const MAXNUMPOS: usize = 256;
/// PG's MAXENTRYPOS - 1: positions are clamped to this (`LIMITPOS`).
pub(crate) const MAXPOS: u16 = 16383;
/// PG's MAXSTRLEN: maximum lexeme byte length in a tsvector.
const MAXSTRLEN: usize = 2047;
/// PG's MAXSTRPOS: maximum total tsvector string size.
const MAXSTRPOS: usize = (1 << 20) - 1;

/// PG's `tsCompareString`: bytewise memcmp over the common prefix,
/// shorter string first on a tie.
pub(crate) fn ts_cmp_str(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal => a.len().cmp(&b.len()),
        o => o,
    }
}

/// Weight letter for a position weight (0 renders as no letter).
fn weight_letter(w: u8) -> Option<char> {
    match w {
        3 => Some('A'),
        2 => Some('B'),
        1 => Some('C'),
        _ => None,
    }
}

/// Parse a weight character (tsvector/tsquery input): a/b/c/d in either
/// case, or `*` meaning A (PG's `parse_weight` / tsvector parser).
fn parse_weight_char(c: u8) -> Option<u8> {
    match c {
        b'a' | b'A' | b'*' => Some(3),
        b'b' | b'B' => Some(2),
        b'c' | b'C' => Some(1),
        b'd' | b'D' => Some(0),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// tsvector input / output (tsvector.c, tsvector_parser.c)
// ---------------------------------------------------------------------------

fn tsvector_syntax(input: &str) -> ExecError {
    exec_err("42601", format!("syntax error in tsvector: \"{input}\""))
}

/// Sort one entry's positions by (pos, weight desc), dedupe keeping the
/// higher weight, clamp positions to MAXPOS and cap at MAXNUMPOS
/// (PG's `uniquePos` + `LIMITPOS`).
fn unique_pos(pos: &mut Vec<(u16, u8)>) {
    for p in pos.iter_mut() {
        if p.0 > MAXPOS {
            p.0 = MAXPOS;
        }
    }
    pos.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    pos.dedup_by(|next, keep| {
        if next.0 == keep.0 {
            // `keep` has the higher (or equal) weight after the sort.
            true
        } else {
            false
        }
    });
    if pos.len() > MAXNUMPOS {
        pos.truncate(MAXNUMPOS);
    }
}

/// Build a normalized TsVectorVal from raw (lexeme, positions) pairs:
/// sort by lexeme, merge duplicates (PG's tsvectorin / make_tsvector).
pub(crate) fn build_tsvector(
    mut entries: Vec<(String, Vec<(u16, u8)>)>,
) -> Result<TsVectorVal, ExecError> {
    entries.sort_by(|a, b| ts_cmp_str(a.0.as_bytes(), b.0.as_bytes()));
    let mut out: Vec<(String, Vec<(u16, u8)>)> = Vec::with_capacity(entries.len());
    let mut total: usize = 0;
    for (lex, pos) in entries {
        if lex.len() > MAXSTRLEN {
            return Err(exec_err(
                "54000",
                format!(
                    "word is too long ({} bytes, max {} bytes)",
                    lex.len(),
                    MAXSTRLEN
                ),
            ));
        }
        total += lex.len() + 1;
        if total > MAXSTRPOS {
            return Err(exec_err(
                "54000",
                "string is too long for tsvector".to_string(),
            ));
        }
        if let Some(last) = out.last_mut().filter(|(l, _)| *l == lex) {
            last.1.extend(pos);
        } else {
            out.push((lex, pos));
        }
    }
    for (_, pos) in out.iter_mut() {
        unique_pos(pos);
    }
    Ok(TsVectorVal { entries: out })
}

/// PG's `tsvectorin`: parse the text representation.
pub(crate) fn parse_tsvector(input: &str) -> Result<TsVectorVal, ExecError> {
    let b = input.as_bytes();
    let mut i = 0usize;
    let mut entries: Vec<(String, Vec<(u16, u8)>)> = Vec::new();
    let n = b.len();
    while i < n {
        // Skip inter-lexeme whitespace (WAITENDOPERAND).
        while i < n && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        // One lexeme: quoted ('' and \ escapes) or bare (up to
        // whitespace or ':').
        let mut lex: Vec<u8> = Vec::new();
        if b[i] == b'\'' {
            i += 1;
            loop {
                match b.get(i).copied() {
                    None => return Err(tsvector_syntax(input)),
                    Some(b'\\') => {
                        // Backslash quotes the next character literally.
                        match b.get(i + 1).copied() {
                            None => return Err(tsvector_syntax(input)),
                            Some(c) => {
                                lex.push(c);
                                i += 2;
                            }
                        }
                    }
                    Some(b'\'') => {
                        if b.get(i + 1).copied() == Some(b'\'') {
                            lex.push(b'\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    Some(c) => {
                        lex.push(c);
                        i += 1;
                    }
                }
            }
        } else {
            // Bare lexeme: a backslash escapes the next character
            // literally (even whitespace or ':'), like the quoted
            // branch and PG's tsvector parser; a trailing backslash
            // is an error.
            while i < n && !(b[i] as char).is_whitespace() && b[i] != b':' {
                if b[i] == b'\\' {
                    match b.get(i + 1).copied() {
                        None => {
                            return Err(exec_err(
                                "42601",
                                format!("there is no escaped character: \"{input}\""),
                            ));
                        }
                        Some(c) => {
                            lex.push(c);
                            i += 2;
                        }
                    }
                } else {
                    lex.push(b[i]);
                    i += 1;
                }
            }
        }
        if lex.is_empty() {
            return Err(tsvector_syntax(input));
        }
        let lexeme = String::from_utf8_lossy(&lex).into_owned();
        // Optional :pos{,pos} list.
        let mut positions: Vec<(u16, u8)> = Vec::new();
        if i < n && b[i] == b':' {
            i += 1;
            loop {
                // WAITPOS: at least one digit.
                let start = i;
                let mut val: u32 = 0;
                while i < n && b[i].is_ascii_digit() {
                    val = val.saturating_mul(10).saturating_add((b[i] - b'0') as u32);
                    i += 1;
                }
                if i == start {
                    return Err(tsvector_syntax(input));
                }
                if val == 0 {
                    return Err(exec_err(
                        "42601",
                        format!("wrong position info in tsvector: \"{input}\""),
                    ));
                }
                let pos = if val > MAXPOS as u32 {
                    MAXPOS
                } else {
                    val as u16
                };
                // WAITPOSDELIM: weight letters / '*' set the weight if
                // it is still 0; further digits are swallowed (PG's
                // WAITPOSDELIM consumes them into the same position).
                let mut weight: u8 = 0;
                let mut next_pos = false;
                while i < n {
                    let c = b[i];
                    if c == b',' {
                        i += 1;
                        next_pos = true;
                        break;
                    }
                    if (c as char).is_whitespace() {
                        break;
                    }
                    if let Some(w) = parse_weight_char(c) {
                        if weight == 0 {
                            weight = w;
                        }
                        i += 1;
                        continue;
                    }
                    if c.is_ascii_digit() {
                        i += 1;
                        continue;
                    }
                    return Err(tsvector_syntax(input));
                }
                positions.push((pos, weight));
                if next_pos {
                    continue;
                }
                break;
            }
        }
        // PG's WAITENDOPERAND: whitespace separates lexemes, but a
        // non-delimiter char right after a (quoted) lexeme simply
        // starts the next lexeme ('a'b -> 'a' 'b'); ':' was already
        // consumed as the position list above.
        entries.push((lexeme, positions));
    }
    build_tsvector(entries)
}

/// PG's `tsvectorout`: `'lex':pA,p,...` space-separated; lexemes escape
/// `'` (doubled) and `\` (backslash-doubled).
pub(crate) fn tsvector_to_text(v: &TsVectorVal) -> String {
    let mut out = String::new();
    for (i, (lex, pos)) in v.entries.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('\'');
        for ch in lex.chars() {
            match ch {
                '\'' => out.push_str("''"),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('\'');
        if !pos.is_empty() {
            out.push(':');
            for (j, (p, w)) in pos.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&p.to_string());
                if let Some(l) = weight_letter(*w) {
                    out.push(l);
                }
            }
        }
    }
    out
}

/// Find an entry by exact lexeme (entries are sorted).
pub(crate) fn find_entry<'a>(
    v: &'a TsVectorVal,
    lex: &str,
) -> Option<&'a (String, Vec<(u16, u8)>)> {
    v.entries
        .binary_search_by(|(l, _)| ts_cmp_str(l.as_bytes(), lex.as_bytes()))
        .ok()
        .map(|idx| &v.entries[idx])
}

/// All entries whose lexeme starts with `prefix` (byte prefix, PG's
/// `find_wordentry` prefix scan over the sorted array).
pub(crate) fn prefix_entries<'a>(
    v: &'a TsVectorVal,
    prefix: &str,
) -> Vec<&'a (String, Vec<(u16, u8)>)> {
    v.entries
        .iter()
        .filter(|(l, _)| l.as_bytes().starts_with(prefix.as_bytes()))
        .collect()
}

// ---------------------------------------------------------------------------
// tsvector operations (tsvector_op.c)
// ---------------------------------------------------------------------------

/// PG's serialized-size proxy for `silly_cmp_tsvector` (VARHDRSZ +
/// entries + WordEntry array + lexeme bytes + WordEntryPos array).
fn tsvector_varsize(v: &TsVectorVal) -> usize {
    let mut sz = 8 + 4 * v.entries.len();
    for (lex, pos) in &v.entries {
        sz += lex.len() + 2 * pos.len();
    }
    sz
}

/// PG's `silly_cmp_tsvector`: varsize, entry count, then per-entry
/// haspos (haspos sorts first), lexeme, position count (more sorts
/// first), then per-position (larger pos / larger weight sorts first).
pub(crate) fn tsvector_cmp(a: &TsVectorVal, b: &TsVectorVal) -> Ordering {
    match tsvector_varsize(a).cmp(&tsvector_varsize(b)) {
        Ordering::Equal => {}
        o => return o,
    }
    match a.entries.len().cmp(&b.entries.len()) {
        Ordering::Equal => {}
        o => return o,
    }
    for ((al, ap), (bl, bp)) in a.entries.iter().zip(b.entries.iter()) {
        match (!ap.is_empty()).cmp(&!bp.is_empty()) {
            // haspos > no-pos sorts BEFORE (returns -1): bool cmp gives
            // false < true, so reverse it.
            Ordering::Equal => {}
            o => return o.reverse(),
        }
        match ts_cmp_str(al.as_bytes(), bl.as_bytes()) {
            Ordering::Equal => {}
            o => return o,
        }
        match ap.len().cmp(&bp.len()) {
            Ordering::Equal => {}
            o => return o.reverse(),
        }
        for ((apos, aw), (bpos, bw)) in ap.iter().zip(bp.iter()) {
            match apos.cmp(bpos) {
                Ordering::Equal => {}
                o => return o.reverse(),
            }
            match aw.cmp(bw) {
                Ordering::Equal => {}
                o => return o.reverse(),
            }
        }
    }
    Ordering::Equal
}

/// PG's `tsvector_concat`: the right vector's positions are offset by
/// the left vector's maximum position; shared lexemes merge.
pub(crate) fn tsvector_concat(a: &TsVectorVal, b: &TsVectorVal) -> TsVectorVal {
    let maxpos = a
        .entries
        .iter()
        .flat_map(|(_, p)| p.iter().map(|(pos, _)| *pos))
        .max()
        .unwrap_or(0);
    let mut entries: Vec<(String, Vec<(u16, u8)>)> =
        Vec::with_capacity(a.entries.len() + b.entries.len());
    entries.extend(a.entries.iter().cloned());
    for (lex, pos) in &b.entries {
        let shifted: Vec<(u16, u8)> = pos
            .iter()
            .map(|(p, w)| {
                let np = (*p as u32 + maxpos as u32).min(MAXPOS as u32) as u16;
                (np, *w)
            })
            .collect();
        entries.push((lex.clone(), shifted));
    }
    // build_tsvector cannot fail here: lexemes already satisfy limits,
    // but the total-size guard could trip on huge concats; PG would
    // raise the same error, so propagate by falling back to a manual
    // merge if it does.
    build_tsvector(entries).unwrap_or_else(|_| {
        let mut out: Vec<(String, Vec<(u16, u8)>)> = Vec::new();
        let mut all: Vec<(String, Vec<(u16, u8)>)> = Vec::new();
        all.extend(a.entries.iter().cloned());
        for (lex, pos) in &b.entries {
            let shifted: Vec<(u16, u8)> = pos
                .iter()
                .map(|(p, w)| ((*p as u32 + maxpos as u32).min(MAXPOS as u32) as u16, *w))
                .collect();
            all.push((lex.clone(), shifted));
        }
        all.sort_by(|x, y| ts_cmp_str(x.0.as_bytes(), y.0.as_bytes()));
        for (lex, pos) in all {
            if let Some(last) = out.last_mut().filter(|(l, _)| *l == lex) {
                last.1.extend(pos);
            } else {
                out.push((lex, pos));
            }
        }
        for (_, pos) in out.iter_mut() {
            unique_pos(pos);
        }
        TsVectorVal { entries: out }
    })
}

/// PG's `strip`: drop all position information.
pub(crate) fn tsvector_strip(v: &TsVectorVal) -> TsVectorVal {
    TsVectorVal {
        entries: v
            .entries
            .iter()
            .map(|(l, _)| (l.clone(), Vec::new()))
            .collect(),
    }
}

/// PG's `ts_delete`: remove the named lexeme(s).
pub(crate) fn tsvector_delete(v: &TsVectorVal, lexemes: &[String]) -> TsVectorVal {
    TsVectorVal {
        entries: v
            .entries
            .iter()
            .filter(|(l, _)| !lexemes.iter().any(|x| x == l))
            .cloned()
            .collect(),
    }
}

/// PG's `parse_weight` for setweight's weight argument: the first
/// character of the string decides; anything else is a 22023 error
/// (`\%03o` form for non-printable bytes, like PG).
fn parse_weight_str(s: &str) -> Result<u8, ExecError> {
    match s.as_bytes().first().copied() {
        Some(c) => match parse_weight_char(c) {
            Some(w) if c != b'*' => Ok(w),
            _ => Err(exec_err(
                "22023",
                if (0x20..0x7f).contains(&c) {
                    format!("unrecognized weight: \"{}\"", c as char)
                } else {
                    format!("unrecognized weight: \"\\{:03o}\"", c)
                },
            )),
        },
        None => Err(exec_err(
            "22023",
            "unrecognized weight: \"\\000\"".to_string(),
        )),
    }
}

/// PG's `setweight`: set the weight of all positions (2-arg) or only
/// the listed lexemes' positions (3-arg; NULL lexemes are skipped).
pub(crate) fn tsvector_setweight(
    v: &TsVectorVal,
    weight: &str,
    lexemes: Option<&[Option<String>]>,
) -> Result<TsVectorVal, ExecError> {
    let w = parse_weight_str(weight)?;
    let mut out = v.clone();
    for (lex, pos) in out.entries.iter_mut() {
        let hit = match lexemes {
            None => true,
            Some(list) => list.iter().flatten().any(|x| x == lex),
        };
        if hit {
            for p in pos.iter_mut() {
                p.1 = w;
            }
        }
    }
    Ok(out)
}

/// PG's `ts_filter`: keep only positions whose weight is in the mask
/// built from the weight-letter array (NULL element → error).
pub(crate) fn tsvector_filter(
    v: &TsVectorVal,
    weights: &[Option<String>],
) -> Result<TsVectorVal, ExecError> {
    let mut mask = [false; 4];
    for w in weights {
        match w {
            None => {
                return Err(exec_err(
                    "22004",
                    "weight array may not contain nulls".to_string(),
                ));
            }
            Some(s) => {
                let wv = parse_weight_str(s)?;
                mask[wv as usize] = true;
            }
        }
    }
    let mut out = Vec::new();
    for (lex, pos) in &v.entries {
        if pos.is_empty() {
            continue;
        }
        let kept: Vec<(u16, u8)> = pos
            .iter()
            .copied()
            .filter(|(_, w)| mask[*w as usize])
            .collect();
        if !kept.is_empty() {
            out.push((lex.clone(), kept));
        }
    }
    Ok(TsVectorVal { entries: out })
}

/// PG's `tsvector_to_array`: the lexemes in order.
pub(crate) fn tsvector_to_array(v: &TsVectorVal) -> Vec<String> {
    v.entries.iter().map(|(l, _)| l.clone()).collect()
}

/// PG's `array_to_tsvector`: sorted, deduplicated, positionless.
/// NULL element → 23502-style error; empty string → error (PG raises
/// `lexeme array may not contain empty strings`).
pub(crate) fn array_to_tsvector(elems: &[Option<String>]) -> Result<TsVectorVal, ExecError> {
    let mut lexemes: Vec<String> = Vec::with_capacity(elems.len());
    for e in elems {
        match e {
            None => {
                return Err(exec_err(
                    "22004",
                    "lexeme array may not contain nulls".to_string(),
                ));
            }
            Some(s) if s.is_empty() => {
                return Err(exec_err(
                    "2200F",
                    "lexeme array may not contain empty strings".to_string(),
                ));
            }
            Some(s) => lexemes.push(s.clone()),
        }
    }
    build_tsvector(lexemes.into_iter().map(|l| (l, Vec::new())).collect())
}

/// PG's `tsvector_length`: the number of lexemes.
pub(crate) fn tsvector_length(v: &TsVectorVal) -> i64 {
    v.entries.len() as i64
}

/// Whether a position's weight passes an operand's weight mask
/// (mask 0 = unrestricted).
pub(crate) fn weight_ok(mask: u8, weight: u8) -> bool {
    mask == 0 || (mask >> weight) & 1 == 1
}

// ---------------------------------------------------------------------------
// tsquery input / output (tsquery.c, tsquery_parser.c)
// ---------------------------------------------------------------------------

fn tsquery_syntax(input: &str) -> ExecError {
    exec_err("42601", format!("syntax error in tsquery: \"{input}\""))
}

fn tsquery_no_operand(input: &str) -> ExecError {
    exec_err("42601", format!("no operand in tsquery: \"{input}\""))
}

/// Operator priorities from tsquery.c's `QO_PRIORITY`.
fn node_priority(n: &TsQueryNode) -> u8 {
    match n {
        TsQueryNode::Or(..) => 1,
        TsQueryNode::And(..) => 2,
        TsQueryNode::Phrase(..) => 3,
        TsQueryNode::Not(_) => 4,
        TsQueryNode::Val { .. } | TsQueryNode::Stop => 0,
    }
}

/// Assign operand ids in polish order (operator, right, left) — PG's
/// QueryItem array order.
pub(crate) fn assign_operand_ids(q: &mut TsQueryVal) {
    fn walk(n: &mut TsQueryNode, next: &mut u32) {
        match n {
            TsQueryNode::Val { id, .. } => {
                *id = *next;
                *next += 1;
            }
            TsQueryNode::Stop => {}
            TsQueryNode::Not(c) => walk(c, next),
            TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) => {
                walk(r, next);
                walk(l, next);
            }
            TsQueryNode::Phrase(l, r, _) => {
                walk(r, next);
                walk(l, next);
            }
        }
    }
    if let Some(root) = q.root.as_mut() {
        walk(root, &mut 0);
    }
}

/// How a parsed operand becomes a subtree: verbatim (tsquery input) or
/// morph-expanded through the dictionary (to_tsquery family).
pub(crate) enum OperandMode<'a> {
    Plain,
    Morph(&'a TsConfig),
}

/// A recursive-descent tsquery parser mirroring
/// `gettoken_query_standard` + `makepol`: operands are lexed with the
/// tsvector token rules (bare lexemes end at whitespace or `:`).
pub(crate) struct TsqParser<'a> {
    b: &'a [u8],
    i: usize,
    input: &'a str,
    mode: OperandMode<'a>,
    /// Whether any token has been consumed (distinguishes the
    /// empty-query PT_END case from the "no operand" error).
    seen_any: bool,
}

impl<'a> TsqParser<'a> {
    pub(crate) fn new(input: &'a str, mode: OperandMode<'a>) -> Self {
        TsqParser {
            b: input.as_bytes(),
            i: 0,
            input,
            mode,
            seen_any: false,
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    pub(crate) fn parse_query(&mut self) -> Result<TsQueryVal, ExecError> {
        self.skip_ws();
        if self.i >= self.b.len() {
            // Empty (or whitespace-only) input: PG's empty query.
            return Ok(TsQueryVal { root: None });
        }
        let root = self.parse_or()?;
        self.skip_ws();
        if self.i != self.b.len() {
            return Err(tsquery_syntax(self.input));
        }
        let root = cleanup_stopwords(root);
        let mut q = TsQueryVal { root };
        assign_operand_ids(&mut q);
        Ok(q)
    }

    fn parse_or(&mut self) -> Result<TsQueryNode, ExecError> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') {
                self.i += 1;
                let rhs = self.parse_and()?;
                lhs = TsQueryNode::Or(Box::new(lhs), Box::new(rhs));
            } else {
                return Ok(lhs);
            }
        }
    }

    fn parse_and(&mut self) -> Result<TsQueryNode, ExecError> {
        let mut lhs = self.parse_phrase()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') {
                self.i += 1;
                let rhs = self.parse_phrase()?;
                lhs = TsQueryNode::And(Box::new(lhs), Box::new(rhs));
            } else {
                return Ok(lhs);
            }
        }
    }

    fn parse_phrase(&mut self) -> Result<TsQueryNode, ExecError> {
        let mut lhs = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'<') {
                let dist = self.parse_phrase_op()?;
                let rhs = self.parse_unary()?;
                lhs = TsQueryNode::Phrase(Box::new(lhs), Box::new(rhs), dist);
            } else {
                return Ok(lhs);
            }
        }
    }

    /// Parse `<->` or `<N>` at the cursor (which points at `<`).
    fn parse_phrase_op(&mut self) -> Result<u16, ExecError> {
        let save = self.i;
        self.i += 1;
        if self.peek() == Some(b'-') && self.b.get(self.i + 1) == Some(&b'>') {
            self.i += 2;
            return Ok(1);
        }
        let start = self.i;
        let mut val: i64 = 0;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            val = val.saturating_mul(10).saturating_add((c - b'0') as i64);
            self.i += 1;
        }
        if self.i == start || self.peek() != Some(b'>') {
            self.i = save;
            return Err(tsquery_syntax(self.input));
        }
        self.i += 1;
        if !(0..=16384).contains(&val) {
            return Err(exec_err(
                "22023",
                "distance in phrase operator must be an integer value between zero and 16384 inclusive".to_string(),
            ));
        }
        Ok(val as u16)
    }

    fn parse_unary(&mut self) -> Result<TsQueryNode, ExecError> {
        self.skip_ws();
        if self.peek() == Some(b'!') {
            self.i += 1;
            self.seen_any = true;
            let inner = self.parse_unary()?;
            return Ok(TsQueryNode::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<TsQueryNode, ExecError> {
        self.skip_ws();
        match self.peek() {
            None => Err(if self.seen_any {
                tsquery_no_operand(self.input)
            } else {
                tsquery_syntax(self.input)
            }),
            Some(b'(') => {
                self.i += 1;
                self.seen_any = true;
                let inner = self.parse_or()?;
                self.skip_ws();
                if self.peek() != Some(b')') {
                    return Err(tsquery_syntax(self.input));
                }
                self.i += 1;
                Ok(inner)
            }
            Some(b')') | Some(b'&') | Some(b'|') | Some(b':') => Err(tsquery_syntax(self.input)),
            Some(_) => self.parse_operand(),
        }
    }

    /// Lex one operand with the tsvector token rules, then its
    /// `:weights`/`*` modifiers (`get_modifiers`).
    fn parse_operand(&mut self) -> Result<TsQueryNode, ExecError> {
        let mut lex: Vec<u8> = Vec::new();
        if self.peek() == Some(b'\'') {
            self.i += 1;
            loop {
                match self.b.get(self.i).copied() {
                    None => return Err(tsquery_syntax(self.input)),
                    Some(b'\\') => match self.b.get(self.i + 1).copied() {
                        None => return Err(tsquery_syntax(self.input)),
                        Some(c) => {
                            lex.push(c);
                            self.i += 2;
                        }
                    },
                    Some(b'\'') => {
                        if self.b.get(self.i + 1).copied() == Some(b'\'') {
                            lex.push(b'\'');
                            self.i += 2;
                        } else {
                            self.i += 1;
                            break;
                        }
                    }
                    Some(c) => {
                        lex.push(c);
                        self.i += 1;
                    }
                }
            }
        } else {
            // Bare operand: the tsvector token scan with oprisdelim
            // set (tsvector_parser.c): the word ends at whitespace,
            // `:`, or one of the operator delimiters ! | & ( ); a
            // backslash escapes the next character literally.
            while self.i < self.b.len() {
                let c = self.b[self.i];
                if (c as char).is_whitespace()
                    || c == b':'
                    || matches!(c, b'!' | b'|' | b'&' | b'(' | b')')
                {
                    break;
                }
                if c == b'\\' {
                    match self.b.get(self.i + 1).copied() {
                        None => {
                            return Err(exec_err(
                                "42601",
                                format!("there is no escaped character: \"{}\"", self.input),
                            ));
                        }
                        Some(n) => {
                            lex.push(n);
                            self.i += 2;
                        }
                    }
                    continue;
                }
                lex.push(c);
                self.i += 1;
            }
        }
        if lex.is_empty() {
            return Err(tsquery_syntax(self.input));
        }
        self.seen_any = true;
        let text = String::from_utf8_lossy(&lex).into_owned();
        // Modifiers: ':' followed by any run of [aAbBcCdD*].
        let mut weight: u8 = 0;
        let mut prefix = false;
        if self.peek() == Some(b':') {
            self.i += 1;
            while let Some(c) = self.peek() {
                match c {
                    b'a' | b'A' => weight |= 1 << 3,
                    b'b' | b'B' => weight |= 1 << 2,
                    b'c' | b'C' => weight |= 1 << 1,
                    b'd' | b'D' => weight |= 1,
                    b'*' => prefix = true,
                    _ => break,
                }
                self.i += 1;
            }
        }
        match &self.mode {
            OperandMode::Plain => Ok(TsQueryNode::Val {
                text,
                weight,
                prefix,
                id: 0,
            }),
            OperandMode::Morph(cfg) => Ok(morph_operand(&text, cfg, weight, prefix)),
        }
    }
}

/// PG's `tsqueryin`: parse the standard tsquery syntax verbatim.
pub(crate) fn parse_tsquery(input: &str) -> Result<TsQueryVal, ExecError> {
    TsqParser::new(input, OperandMode::Plain).parse_query()
}

/// PG's `tsqueryout`: the canonical infix form (see tsquery.c
/// `infix`): operands `'lex'` + optional `:*AB...`; binary operators
/// ` | `, ` & `, ` <-> `, ` <N> `; parentheses `( ... )` when a child's
/// priority is below its parent's, or for a phrase node that is the
/// right child of a phrase node; `!` binds directly to its child.
pub(crate) fn tsquery_to_text(q: &TsQueryVal) -> String {
    let mut out = String::new();
    if let Some(root) = &q.root {
        fmt_tsquery_node(root, 0, false, &mut out);
    }
    out
}

fn fmt_tsquery_node(n: &TsQueryNode, parent_priority: u8, right_phrase: bool, out: &mut String) {
    match n {
        TsQueryNode::Val {
            text,
            weight,
            prefix,
            ..
        } => {
            out.push('\'');
            for ch in text.chars() {
                match ch {
                    '\'' => out.push_str("''"),
                    '\\' => out.push_str("\\\\"),
                    c => out.push(c),
                }
            }
            out.push('\'');
            if *weight != 0 || *prefix {
                out.push(':');
                if *prefix {
                    out.push('*');
                }
                for (bit, letter) in [(3u8, 'A'), (2, 'B'), (1, 'C'), (0, 'D')] {
                    if weight & (1 << bit) != 0 {
                        out.push(letter);
                    }
                }
            }
        }
        TsQueryNode::Stop => {}
        TsQueryNode::Not(child) => {
            out.push('!');
            fmt_tsquery_node(child, 4, false, out);
        }
        TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) => {
            let prio = node_priority(n);
            let need = prio < parent_priority;
            if need {
                out.push_str("( ");
            }
            fmt_tsquery_node(l, prio, false, out);
            out.push_str(if matches!(n, TsQueryNode::And(..)) {
                " & "
            } else {
                " | "
            });
            fmt_tsquery_node(r, prio, false, out);
            if need {
                out.push_str(" )");
            }
        }
        TsQueryNode::Phrase(l, r, dist) => {
            let prio = 3;
            let need = prio < parent_priority || right_phrase;
            if need {
                out.push_str("( ");
            }
            fmt_tsquery_node(l, prio, false, out);
            if *dist == 1 {
                out.push_str(" <-> ");
            } else {
                out.push_str(&format!(" <{dist}> "));
            }
            fmt_tsquery_node(r, prio, true, out);
            if need {
                out.push_str(" )");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stopword cleanup (tsquery_cleanup.c)
// ---------------------------------------------------------------------------

/// PG's `clean_NOT_intree` + the phrase-distance fixups of
/// `cleanup_tsquery_stopwords`: remove Stop leaves; a binary node with
/// a removed side collapses to the other side, accumulating the dead
/// side's phrase distance into the surviving phrase edges.
fn cleanup_stopwords(n: TsQueryNode) -> Option<TsQueryNode> {
    fn clean(n: &TsQueryNode) -> (Option<TsQueryNode>, i32, i32) {
        match n {
            TsQueryNode::Val { .. } => (Some(n.clone()), 0, 0),
            TsQueryNode::Stop => (None, 0, 0),
            TsQueryNode::Not(c) => {
                let (cc, l, r) = clean(c);
                match cc {
                    None => (None, l, r),
                    Some(cc) => (Some(TsQueryNode::Not(Box::new(cc))), l, r),
                }
            }
            TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) => {
                let is_phrase = false;
                let (lc, ll, lr) = clean(l);
                let (rc, rl, rr) = clean(r);
                finish_binary(n, is_phrase, 0, lc, ll, lr, rc, rl, rr)
            }
            TsQueryNode::Phrase(l, r, d) => {
                let (lc, ll, lr) = clean(l);
                let (rc, rl, rr) = clean(r);
                finish_binary(n, true, *d as i32, lc, ll, lr, rc, rl, rr)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_binary(
        orig: &TsQueryNode,
        is_phrase: bool,
        dist: i32,
        lc: Option<TsQueryNode>,
        lladd: i32,
        lradd: i32,
        rc: Option<TsQueryNode>,
        rladd: i32,
        rradd: i32,
    ) -> (Option<TsQueryNode>, i32, i32) {
        match (lc, rc) {
            (None, None) => {
                if is_phrase {
                    let both = lladd + dist + rladd;
                    (None, both, both)
                } else {
                    let both = lladd.max(rladd);
                    (None, both, both)
                }
            }
            (None, Some(r)) => {
                if is_phrase {
                    (Some(r), lladd + dist + rladd, rradd)
                } else {
                    (Some(r), rladd, rradd)
                }
            }
            (Some(l), None) => {
                if is_phrase {
                    (Some(l), lladd, lradd + dist + rradd)
                } else {
                    (Some(l), lladd, lradd)
                }
            }
            (Some(l), Some(r)) => {
                let node = match orig {
                    TsQueryNode::And(..) => TsQueryNode::And(Box::new(l), Box::new(r)),
                    TsQueryNode::Or(..) => TsQueryNode::Or(Box::new(l), Box::new(r)),
                    TsQueryNode::Phrase(..) => {
                        let nd = (dist + lradd + rladd).clamp(0, 16384);
                        TsQueryNode::Phrase(Box::new(l), Box::new(r), nd as u16)
                    }
                    _ => unreachable!(),
                };
                (Some(node), lladd, rradd)
            }
        }
    }

    clean(&n).0
}

// ---------------------------------------------------------------------------
// tsquery constructors (tsquery_op.c)
// ---------------------------------------------------------------------------

/// PG's `tsquery_and`: empty side yields the other side.
pub(crate) fn tsquery_and(a: &TsQueryVal, b: &TsQueryVal) -> TsQueryVal {
    combine_queries(a, b, false)
}

/// PG's `tsquery_or`.
pub(crate) fn tsquery_or(a: &TsQueryVal, b: &TsQueryVal) -> TsQueryVal {
    combine_queries(a, b, true)
}

fn combine_queries(a: &TsQueryVal, b: &TsQueryVal, or: bool) -> TsQueryVal {
    match (&a.root, &b.root) {
        (None, _) => b.clone(),
        (_, None) => a.clone(),
        (Some(l), Some(r)) => {
            let root = if or {
                TsQueryNode::Or(Box::new(l.clone()), Box::new(r.clone()))
            } else {
                TsQueryNode::And(Box::new(l.clone()), Box::new(r.clone()))
            };
            let mut q = TsQueryVal { root: Some(root) };
            assign_operand_ids(&mut q);
            q
        }
    }
}

/// PG's `tsquery_not`.
pub(crate) fn tsquery_not(a: &TsQueryVal) -> TsQueryVal {
    match &a.root {
        None => a.clone(),
        Some(r) => {
            let mut q = TsQueryVal {
                root: Some(TsQueryNode::Not(Box::new(r.clone()))),
            };
            assign_operand_ids(&mut q);
            q
        }
    }
}

/// PG's `tsquery_phrase` (with explicit distance validation, 22023).
pub(crate) fn tsquery_phrase(
    a: &TsQueryVal,
    b: &TsQueryVal,
    dist: i64,
) -> Result<TsQueryVal, ExecError> {
    if !(0..=16384).contains(&dist) {
        return Err(exec_err(
            "22023",
            "distance in phrase operator must be an integer value between zero and 16384 inclusive"
                .to_string(),
        ));
    }
    match (&a.root, &b.root) {
        (Some(l), Some(r)) => {
            let mut q = TsQueryVal {
                root: Some(TsQueryNode::Phrase(
                    Box::new(l.clone()),
                    Box::new(r.clone()),
                    dist as u16,
                )),
            };
            assign_operand_ids(&mut q);
            Ok(q)
        }
        // PG's tsquery_phrase on an empty operand yields the empty
        // query (size-0 constructions collapse).
        _ => Ok(TsQueryVal { root: None }),
    }
}

// ---------------------------------------------------------------------------
// tsquery comparison (tsquery_op.c CompareTSQ, tsquery_util.c)
// ---------------------------------------------------------------------------

/// PG's legacy CRC32 (init/final XOR 0xFFFFFFFF) over the operand
/// bytes, as stored in QueryOperand.valcrc.
fn legacy_crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB88320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
        *e = crc;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn tsquery_varsize(q: &TsQueryVal) -> usize {
    fn walk(n: &TsQueryNode, sz: &mut usize) {
        match n {
            TsQueryNode::Val { text, .. } => *sz += text.len() + 1,
            TsQueryNode::Stop => *sz += 1,
            TsQueryNode::Not(c) => walk(c, sz),
            TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) | TsQueryNode::Phrase(l, r, _) => {
                walk(l, sz);
                walk(r, sz);
            }
        }
    }
    let mut sz = 4 + 8 * q.root.as_ref().map(|r| r.numnode() as usize).unwrap_or(0);
    if let Some(r) = &q.root {
        walk(r, &mut sz);
    }
    sz
}

/// PG's `QTNodeCompare` over the tree form. Children are visited in
/// polish order: for a binary node, child[0] is the RIGHT subtree.
fn qt_node_compare(a: &TsQueryNode, b: &TsQueryNode) -> Ordering {
    fn type_rank(n: &TsQueryNode) -> u8 {
        match n {
            TsQueryNode::Val { .. } | TsQueryNode::Stop => 1,
            _ => 2,
        }
    }
    fn oper_rank(n: &TsQueryNode) -> u8 {
        match n {
            TsQueryNode::Not(_) => 1,
            TsQueryNode::And(..) => 2,
            TsQueryNode::Or(..) => 3,
            TsQueryNode::Phrase(..) => 4,
            _ => 0,
        }
    }
    // Larger type/oper sorts first (-1): compare reversed.
    match type_rank(a).cmp(&type_rank(b)) {
        Ordering::Equal => {}
        o => return o.reverse(),
    }
    match (a, b) {
        (TsQueryNode::Val { text: at, .. }, TsQueryNode::Val { text: bt, .. }) => {
            match legacy_crc32(at.as_bytes()).cmp(&legacy_crc32(bt.as_bytes())) {
                Ordering::Equal => ts_cmp_str(at.as_bytes(), bt.as_bytes()),
                o => o.reverse(),
            }
        }
        (TsQueryNode::Not(ac), TsQueryNode::Not(bc)) => qt_node_compare(ac, bc),
        _ => {
            match oper_rank(a).cmp(&oper_rank(b)) {
                Ordering::Equal => {}
                o => return o.reverse(),
            }
            // Children in polish order: right subtree first.
            let (al, ar) = binary_children(a);
            let (bl, br) = binary_children(b);
            match qt_node_compare(ar, br) {
                Ordering::Equal => {}
                o => return o,
            }
            match qt_node_compare(al, bl) {
                Ordering::Equal => {}
                o => return o,
            }
            if let (TsQueryNode::Phrase(_, _, ad), TsQueryNode::Phrase(_, _, bd)) = (a, b) {
                match ad.cmp(bd) {
                    Ordering::Equal => {}
                    o => return o.reverse(),
                }
            }
            Ordering::Equal
        }
    }
}

fn binary_children(n: &TsQueryNode) -> (&TsQueryNode, &TsQueryNode) {
    match n {
        TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) | TsQueryNode::Phrase(l, r, _) => (l, r),
        _ => unreachable!("binary_children on non-binary node"),
    }
}

/// PG's `tsquery_cmp` (CompareTSQ): node count, then varsize, then
/// QTNodeCompare.
pub(crate) fn tsquery_cmp(a: &TsQueryVal, b: &TsQueryVal) -> Ordering {
    let an = a.root.as_ref().map(|r| r.numnode()).unwrap_or(0);
    let bn = b.root.as_ref().map(|r| r.numnode()).unwrap_or(0);
    match an.cmp(&bn) {
        Ordering::Equal => {}
        o => return o,
    }
    match tsquery_varsize(a).cmp(&tsquery_varsize(b)) {
        Ordering::Equal => {}
        o => return o,
    }
    match (&a.root, &b.root) {
        (Some(ar), Some(br)) => qt_node_compare(ar, br),
        _ => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Query execution: TS_execute / TS_phrase_execute (tsvector_op.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tri {
    Yes,
    No,
    Maybe,
}

/// The result of matching one operand against the vector, in phrase
/// form: weight-filtered match positions plus provenance for the
/// MAYBE (stripped-entry) case.
struct OperandMatch {
    state: Tri,
    /// Positions (plain values, sorted, deduped like PG's phrase
    /// merge); only meaningful when `state == Yes`.
    positions: Vec<u16>,
}

/// PG's `checkcondition_str` for one operand against `data` (phrase
/// form): exact or prefix match, positions weight-filtered.
fn operand_match_phrase(v: &TsVectorVal, text: &str, weight: u8, prefix: bool) -> OperandMatch {
    // Collect (pos, weight-key) pairs: weight-key is the stored weight
    // when no filter applies (PG keeps raw WordEntryPos values for the
    // prefix merge dedupe) and 0 when a filter stripped it.
    let mut pairs: Vec<(u16, u8)> = Vec::new();
    let mut any_stripped = false;
    let mut any_yes = false;
    let entries: Vec<&(String, Vec<(u16, u8)>)> = if prefix {
        prefix_entries(v, text)
    } else {
        find_entry(v, text).into_iter().collect()
    };
    for (_, pos) in entries {
        if pos.is_empty() {
            any_stripped = true;
            continue;
        }
        let kept: Vec<(u16, u8)> = pos
            .iter()
            .copied()
            .filter(|(_, w)| weight_ok(weight, *w))
            .map(|(p, w)| (p, if weight == 0 { w } else { 0 }))
            .collect();
        if !kept.is_empty() {
            any_yes = true;
            pairs.extend(kept);
        }
    }
    if prefix && any_stripped {
        // PG: a MAYBE from any prefix-matched stripped entry discards
        // all gathered positions.
        return OperandMatch {
            state: Tri::Maybe,
            positions: Vec::new(),
        };
    }
    if !prefix && any_stripped {
        return OperandMatch {
            state: Tri::Maybe,
            positions: Vec::new(),
        };
    }
    if !any_yes {
        return OperandMatch {
            state: Tri::No,
            positions: Vec::new(),
        };
    }
    pairs.sort();
    pairs.dedup();
    OperandMatch {
        state: Tri::Yes,
        positions: pairs.into_iter().map(|(p, _)| p).collect(),
    }
}

/// PG's `checkcondition_str` without position data (boolean form).
fn operand_match_bool(v: &TsVectorVal, text: &str, weight: u8, prefix: bool) -> Tri {
    let entries: Vec<&(String, Vec<(u16, u8)>)> = if prefix {
        prefix_entries(v, text)
    } else {
        find_entry(v, text).into_iter().collect()
    };
    let mut result = Tri::No;
    for (_, pos) in entries {
        let sub = if pos.is_empty() {
            // Stripped entry: YES in boolean mode (weights unchecked,
            // per PG's historical behavior).
            Tri::Yes
        } else if pos.iter().any(|(_, w)| weight_ok(weight, *w)) {
            Tri::Yes
        } else {
            Tri::No
        };
        match sub {
            Tri::Yes => return Tri::Yes,
            Tri::Maybe => result = Tri::Maybe,
            Tri::No => {}
        }
    }
    result
}

/// ExecPhraseData: emitted match END positions, match width, negate.
#[derive(Clone, Default)]
struct PhraseData {
    pos: Vec<u16>,
    width: i32,
    negate: bool,
}

const TSPO_BOTH: u8 = 1;
const TSPO_L_ONLY: u8 = 2;
const TSPO_R_ONLY: u8 = 4;

/// PG's `TS_phrase_output`: merge-join the two shifted position
/// lists, emitting per the TSPO flags; only positions > 0 are
/// emitted. Returns the emitted positions.
fn phrase_output(l: &PhraseData, r: &PhraseData, emit: u8, loffset: i32, roffset: i32) -> Vec<u16> {
    let mut out = Vec::new();
    let (mut li, mut ri) = (0usize, 0usize);
    loop {
        let lpos = if li < l.pos.len() {
            l.pos[li] as i32 + loffset
        } else {
            if emit & TSPO_R_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let rpos = if ri < r.pos.len() {
            r.pos[ri] as i32 + roffset
        } else {
            if emit & TSPO_L_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let mut output = 0i32;
        if lpos < rpos {
            if emit & TSPO_L_ONLY != 0 {
                output = lpos;
            }
            li += 1;
        } else if lpos == rpos {
            if emit & TSPO_BOTH != 0 {
                output = rpos;
            }
            li += 1;
            ri += 1;
        } else {
            if emit & TSPO_R_ONLY != 0 {
                output = rpos;
            }
            ri += 1;
        }
        if output > 0 {
            out.push(output as u16);
        }
        if li >= l.pos.len() && ri >= r.pos.len() {
            break;
        }
    }
    out
}

/// PG's `TS_phrase_execute` (without TS_EXEC_SKIP_NOT; callers here
/// never set it).
fn phrase_execute(v: &TsVectorVal, n: &TsQueryNode) -> (Tri, PhraseData) {
    match n {
        TsQueryNode::Val {
            text,
            weight,
            prefix,
            ..
        } => {
            let m = operand_match_phrase(v, text, *weight, *prefix);
            (
                m.state,
                PhraseData {
                    pos: m.positions,
                    width: 0,
                    negate: false,
                },
            )
        }
        TsQueryNode::Stop => (Tri::No, PhraseData::default()),
        other => phrase_combine(other, &mut |child| phrase_execute(v, child)),
    }
}

/// PG's `TS_execute` (boolean form; phrase subtrees delegate to
/// TS_phrase_execute with data = NULL, whose MAYBE propagates and so
/// fails the overall match, matching PG's top-level semantics).
fn ts_execute(v: &TsVectorVal, n: &TsQueryNode) -> Tri {
    match n {
        TsQueryNode::Val {
            text,
            weight,
            prefix,
            ..
        } => operand_match_bool(v, text, *weight, *prefix),
        TsQueryNode::Stop => Tri::No,
        TsQueryNode::Not(c) => match ts_execute(v, c) {
            Tri::Yes => Tri::No,
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
        },
        TsQueryNode::And(l, r) => {
            let ls = ts_execute(v, l);
            if ls == Tri::No {
                return Tri::No;
            }
            let rs = ts_execute(v, r);
            match (ls, rs) {
                (Tri::Yes, Tri::Yes) => Tri::Yes,
                (Tri::No, _) | (_, Tri::No) => Tri::No,
                _ => Tri::Maybe,
            }
        }
        TsQueryNode::Or(l, r) => {
            let ls = ts_execute(v, l);
            if ls == Tri::Yes {
                return Tri::Yes;
            }
            let rs = ts_execute(v, r);
            match (ls, rs) {
                (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
                (Tri::No, Tri::No) => Tri::No,
                _ => Tri::Maybe,
            }
        }
        TsQueryNode::Phrase(..) => {
            let (state, _) = phrase_execute(v, n);
            // PG's TS_execute_recurse (tsvector_op.c): a MAYBE from
            // the topmost phrase operator converts to TS_NO — the
            // caller here passes no TS_EXEC_PHRASE_NO_POS flag.
            // (A stripped entry under a phrase thus never matches,
            // while the same entry in boolean mode is a YES.)
            if state == Tri::Maybe { Tri::No } else { state }
        }
    }
}

/// The `@@` operator: tsvector @@ tsquery (either argument order).
/// The empty query matches nothing (PG returns false).
pub(crate) fn ts_match(v: &TsVectorVal, q: &TsQueryVal) -> bool {
    match &q.root {
        None => false,
        Some(root) => ts_execute(v, root) == Tri::Yes,
    }
}

// ---------------------------------------------------------------------------
// Text search configurations, dictionaries, tokenizer (to_tsany.c,
// ts_parse.c, wparser_def.c — an approximation of the default parser)
// ---------------------------------------------------------------------------

/// The two configurations this engine implements. Any other name is a
/// 42704 error at the call site (PG's "text search configuration ...
/// does not exist").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TsConfig {
    English,
    Simple,
}

impl TsConfig {
    /// Resolve a configuration name (PG accepts the bare name or a
    /// schema-qualified `pg_catalog.<name>`, case-insensitively).
    pub(crate) fn from_name(name: &str) -> Option<TsConfig> {
        let lower = name.to_lowercase();
        let bare = lower.strip_prefix("pg_catalog.").unwrap_or(&lower);
        match bare {
            "english" => Some(TsConfig::English),
            "simple" => Some(TsConfig::Simple),
            _ => None,
        }
    }
}

/// The english stop-word list (src/backend/snowball/stopwords/
/// english.stop), checked after lowercasing, before stemming.
const ENGLISH_STOPWORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "did",
    "do",
    "does",
    "doing",
    "don",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "s",
    "same",
    "she",
    "should",
    "so",
    "some",
    "such",
    "t",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

fn is_stopword(w: &str) -> bool {
    ENGLISH_STOPWORDS.binary_search(&w).is_ok()
}

// ---------------------------------------------------------------------------
// Snowball english stemmer (libstemmer/stem_UTF_8_english.c)
// ---------------------------------------------------------------------------

fn stem_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

fn stem_vowel_wxy(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'w' | 'x' | 'Y')
}

/// The generated stemmer's `r_shortv`, evaluated on the chars before
/// the cursor: (non-v_WXY)(vowel)(non-vowel) at the end, or a
/// two-char (vowel)(non-vowel) word, or a word ending in "past".
fn shortv(w: &[char]) -> bool {
    let n = w.len();
    if n >= 3 && !stem_vowel_wxy(w[n - 1]) && stem_vowel(w[n - 2]) && !stem_vowel(w[n - 3]) {
        return true;
    }
    if n == 2 && stem_vowel(w[0]) && !stem_vowel(w[1]) {
        return true;
    }
    w.ends_with(&['p', 'a', 's', 't'])
}

/// mark_regions helper: index just past the first consonant that
/// follows the first vowel run, scanning from `start`.
fn region_end(w: &[char], start: usize) -> usize {
    let mut i = start;
    while i < w.len() && !stem_vowel(w[i]) {
        i += 1;
    }
    while i < w.len() && stem_vowel(w[i]) {
        i += 1;
    }
    if i < w.len() {
        i += 1;
    }
    i
}

fn longest_suffix<'a>(w: &[char], table: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for cand in table {
        let clen = cand.chars().count();
        if clen <= w.len()
            && w[w.len() - clen..].iter().copied().eq(cand.chars())
            && best.map(|b| clen > b.chars().count()).unwrap_or(true)
        {
            best = Some(cand);
        }
    }
    best
}

fn strip_suffix(w: &mut Vec<char>, suffix: &str) -> usize {
    let clen = suffix.chars().count();
    let start = w.len() - clen;
    w.truncate(start);
    start
}

fn replace_suffix(w: &mut Vec<char>, suffix: &str, replacement: &str) {
    strip_suffix(w, suffix);
    w.extend(replacement.chars());
}

/// PG's english Snowball stemmer. Input must already be lowercased
/// (the dictionary lowercases before stemming).
pub(crate) fn english_stem(input: &str) -> String {
    // Exception 1: whole-word matches (checked before everything).
    match input {
        "skis" => return "ski".to_string(),
        "skies" => return "sky".to_string(),
        "idly" => return "idl".to_string(),
        "gently" => return "gentl".to_string(),
        "ugly" => return "ugli".to_string(),
        "early" => return "earli".to_string(),
        "only" => return "onli".to_string(),
        "singly" => return "singl".to_string(),
        "andes" | "atlas" | "bias" | "cosmos" | "howe" | "news" | "sky" => {
            return input.to_string();
        }
        _ => {}
    }
    let mut w: Vec<char> = input.chars().collect();
    if w.len() < 3 {
        return input.to_string();
    }
    // Prelude: drop a leading apostrophe; mark consonantal y as Y.
    if w.first() == Some(&'\'') {
        w.remove(0);
    }
    let mut y_found = false;
    if w.first() == Some(&'y') {
        w[0] = 'Y';
        y_found = true;
    }
    for i in 0..w.len().saturating_sub(1) {
        if stem_vowel(w[i]) && w[i + 1] == 'y' {
            w[i + 1] = 'Y';
            y_found = true;
        }
    }
    // mark_regions.
    let p1 = [
        "arsen", "commun", "emerg", "gener", "inter", "later", "organ", "past", "univers",
    ]
    .iter()
    .filter(|p| input.starts_with(**p))
    .map(|p| p.chars().count())
    .max()
    .unwrap_or_else(|| region_end(&w, 0));
    let p2 = region_end(&w, p1);

    // Step 0: delete trailing 's' / 's / '.
    if let Some(suf) = longest_suffix(&w, &["'s'", "'s", "'"]) {
        strip_suffix(&mut w, suf);
    }
    // Step 1a.
    if let Some(suf) = longest_suffix(&w, &["sses", "ied", "ies", "us", "ss", "s"]) {
        match suf {
            "sses" => replace_suffix(&mut w, suf, "ss"),
            "ied" | "ies" => {
                let start = w.len() - 3;
                if start >= 2 {
                    replace_suffix(&mut w, suf, "i");
                } else {
                    replace_suffix(&mut w, suf, "ie");
                }
            }
            "us" | "ss" => {}
            "s" => {
                // Delete iff a vowel occurs in w[..len-2].
                let upto = w.len().saturating_sub(2);
                if w[..upto].iter().any(|&c| stem_vowel(c)) {
                    strip_suffix(&mut w, suf);
                }
            }
            _ => unreachable!(),
        }
    }
    // Step 1b.
    step_1b(&mut w, p1);
    // Step 1c.
    if matches!(w.last(), Some('y') | Some('Y')) {
        let idx = w.len() - 1;
        if idx > 1 && !stem_vowel(w[idx - 1]) {
            w[idx] = 'i';
        }
    }
    // Step 2.
    step_2(&mut w, p1);
    // Step 3.
    step_3(&mut w, p1, p2);
    // Step 4.
    step_4(&mut w, p2);
    // Step 5.
    step_5(&mut w, p1, p2);
    // Postlude.
    if y_found {
        for c in w.iter_mut() {
            if *c == 'Y' {
                *c = 'y';
            }
        }
    }
    w.into_iter().collect()
}

fn step_1b(w: &mut Vec<char>, p1: usize) {
    let Some(suf) = longest_suffix(w, &["eedly", "eed", "ingly", "ing", "edly", "ed"]) else {
        return;
    };
    let start = w.len() - suf.chars().count();
    match suf {
        "eed" | "eedly" => {
            if start >= p1 {
                // succeed / proceed / exceed stay unchanged.
                let stem: String = w[..start].iter().collect();
                if !(stem == "succ" || stem == "proc" || stem == "exc") {
                    replace_suffix(w, suf, "ee");
                }
            }
        }
        "ing" => {
            // The a_4 specials at the end of the stem.
            let stem = &w[..start];
            let special = ["even", "cann", "earr", "herr", "inn", "out"]
                .iter()
                .find(|p| stem.ends_with(&p.chars().collect::<Vec<_>>()[..]))
                .map(|p| p.chars().count());
            if let Some(plen) = special {
                if start == plen {
                    // inning / outing / canning / herring / earring /
                    // evening: unchanged by step 1b.
                    return;
                }
                return step_1b_delete(w, start, p1);
            }
            if stem.last() == Some(&'y') {
                if start == 2 && !stem_vowel(stem[0]) {
                    // (consonant)ying -> (consonant)ie (dying -> die).
                    w.truncate(1);
                    w.extend(['i', 'e']);
                    return;
                }
                return step_1b_delete(w, start, p1);
            }
            step_1b_delete(w, start, p1);
        }
        _ => step_1b_delete(w, start, p1),
    }
}

/// Step 1b's delete path (ed/edly/ing/ingly after the vowel test):
/// delete the suffix, then (a) at/bl/iz -> append e; (b) a double
/// consonant loses its last char (except a 3-char stem starting with
/// a/e/o); (c) a stem ending exactly at p1 that is shortv gains an e.
fn step_1b_delete(w: &mut Vec<char>, start: usize, p1: usize) {
    if !w[..start].iter().any(|&c| stem_vowel(c)) {
        return;
    }
    w.truncate(start);
    if let Some(suf) = longest_suffix(w, &["at", "bl", "iz"]) {
        let _ = suf;
        w.push('e');
        return;
    }
    if let Some(suf) = longest_suffix(w, &["bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt"]) {
        let _ = suf;
        if !(w.len() == 3 && matches!(w[0], 'a' | 'e' | 'o')) {
            w.pop();
        }
        return;
    }
    if w.len() == p1 && shortv(w) {
        w.push('e');
    }
}

fn step_2(w: &mut Vec<char>, p1: usize) {
    // (suffix, replacement) — longest match wins; all require R1.
    const TABLE: &[(&str, &str)] = &[
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("abli", "able"),
        ("entli", "ent"),
        ("izer", "ize"),
        ("ization", "ize"),
        ("ational", "ate"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alli", "al"),
        ("aliti", "al"),
        ("alism", "al"),
        ("fulli", "ful"),
        ("fulness", "ful"),
        ("ousli", "ous"),
        ("ousness", "ous"),
        ("iviti", "ive"),
        ("iveness", "ive"),
        ("biliti", "ble"),
        ("bli", "ble"),
        ("ogist", "og"),
        ("ogi", "og"), // only when preceded by 'l'
        ("lessli", "less"),
        ("li", ""), // only when preceded by a valid LI letter
    ];
    let mut best: Option<&(&str, &str)> = None;
    for entry in TABLE {
        let clen = entry.0.chars().count();
        if clen <= w.len()
            && w[w.len() - clen..].iter().copied().eq(entry.0.chars())
            && best.map(|b| clen > b.0.chars().count()).unwrap_or(true)
        {
            best = Some(entry);
        }
    }
    let Some(&(suf, repl)) = best else { return };
    let start = w.len() - suf.chars().count();
    if start < p1 {
        return;
    }
    match suf {
        "ogi" => {
            if start == 0 || w[start - 1] != 'l' {
                return;
            }
        }
        "li" => {
            if start == 0 || !"cdeghkmnrt".contains(w[start - 1]) {
                return;
            }
        }
        _ => {}
    }
    replace_suffix(w, suf, repl);
}

fn step_3(w: &mut Vec<char>, p1: usize, p2: usize) {
    const TABLE: &[(&str, &str, bool)] = &[
        // (suffix, replacement, also-requires-R2)
        ("tional", "tion", false),
        ("ational", "ate", false),
        ("alize", "al", false),
        ("icate", "ic", false),
        ("iciti", "ic", false),
        ("ical", "ic", false),
        ("ful", "", false),
        ("ness", "", false),
        ("ative", "", true),
    ];
    let mut best: Option<&(&str, &str, bool)> = None;
    for entry in TABLE {
        let clen = entry.0.chars().count();
        if clen <= w.len()
            && w[w.len() - clen..].iter().copied().eq(entry.0.chars())
            && best.map(|b| clen > b.0.chars().count()).unwrap_or(true)
        {
            best = Some(entry);
        }
    }
    let Some(&(suf, repl, need_r2)) = best else {
        return;
    };
    let start = w.len() - suf.chars().count();
    if start < p1 {
        return;
    }
    if need_r2 && start < p2 {
        return;
    }
    replace_suffix(w, suf, repl);
}

fn step_4(w: &mut Vec<char>, p2: usize) {
    const TABLE: &[&str] = &[
        "ement", "ance", "ence", "able", "ible", "ment", "ate", "ive", "ize", "iti", "al", "ism",
        "er", "ous", "ant", "ent", "ic", "ion",
    ];
    let Some(suf) = longest_suffix(w, TABLE) else {
        return;
    };
    let start = w.len() - suf.chars().count();
    if start < p2 {
        return;
    }
    if suf == "ion" && (start == 0 || !matches!(w[start - 1], 's' | 't')) {
        return;
    }
    strip_suffix(w, suf);
}

fn step_5(w: &mut Vec<char>, p1: usize, p2: usize) {
    match w.last().copied() {
        Some('e') => {
            let start = w.len() - 1;
            if start >= p2 {
                w.pop();
            } else if start >= p1 && !shortv(&w[..w.len() - 1]) {
                w.pop();
            }
        }
        Some('l') => {
            let start = w.len() - 1;
            if start >= p2 && w.len() >= 2 && w[w.len() - 2] == 'l' {
                w.pop();
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The default parser (wparser approximation) + dictionaries
// ---------------------------------------------------------------------------

/// A raw parser token: `wordy` tokens go to the stemming dictionary
/// under english (PG's asciiword/word/hword/hword-part token types);
/// all others go to the simple dictionary (lowercase only). Under the
/// simple configuration everything uses the simple dictionary.
struct RawToken {
    text: String,
    wordy: bool,
}

fn ts_is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Classify one hyphen-free, symbol-free component.
fn classify_component(s: &str) -> RawToken {
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    RawToken {
        text: s.to_string(),
        // PG: pure words (and word parts) stem; anything containing a
        // digit (numword/int/float/version) uses the simple dict.
        wordy: !has_digit,
    }
}

/// Scientific-notation check: digits [.digits] (e|E) [+-] digits.
fn is_scientific(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let mut digits = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits = true;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if !digits || i >= b.len() || (b[i] != b'e' && b[i] != b'E') {
        return false;
    }
    i += 1;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    i == b.len() && i > start
}

/// Emit tokens for one maximal run of word/symbol characters.
fn emit_run(run: &str, out: &mut Vec<RawToken>) {
    let mut r = run.trim_matches(|c| c == '.' || c == '-');
    // A leading '/' marks a file/url path (PG FILE / URLPATH token).
    if r.len() > 1 && r.starts_with('/') {
        out.push(RawToken {
            text: r.to_string(),
            wordy: false,
        });
        return;
    }
    r = r.trim_start_matches('/');
    if r.is_empty() {
        return;
    }
    // Email: local@domain.tld (one token, simple dictionary).
    // PG's wparser requires a plausible TLD: the final domain
    // label must be at least two alphabetic characters (so
    // 'qwe@efd.r' is two tokens, 'qwe@efd.ru' is an email).
    if let Some(at) = r.find('@') {
        let (local, domain) = (&r[..at], &r[at + 1..]);
        let tld = domain.rsplit('.').next().unwrap_or("");
        if !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && tld.len() >= 2
            && tld.chars().all(|c| c.is_alphabetic())
        {
            out.push(RawToken {
                text: r.to_string(),
                wordy: false,
            });
            return;
        }
        // Not an email: '@' acts as a separator.
        for part in r.split('@') {
            emit_run(part, out);
        }
        return;
    }
    // '_' separates words in PG's default parser (no whole-word
    // token, unlike hyphenated words): pg_class -> pg, class.
    if r.contains('_') {
        for part in r.split('_') {
            if !part.is_empty() {
                emit_run(part, out);
            }
        }
        return;
    }
    if r.contains('-') {
        let parts: Vec<&str> = r.split('-').filter(|p| !p.is_empty()).collect();
        if parts.len() >= 2 {
            // PG emits the whole hyphenated word FIRST, then its
            // parts (positions follow that order).
            let mut any_nonword = false;
            let mut toks = Vec::with_capacity(parts.len());
            for p in &parts {
                let t = classify_component(p);
                any_nonword |= !t.wordy;
                toks.push(t);
            }
            out.push(RawToken {
                text: r.to_string(),
                wordy: !any_nonword,
            });
            out.extend(toks);
            return;
        }
        r = parts.first().copied().unwrap_or("");
        if r.is_empty() {
            return;
        }
    }
    if r.contains(':') {
        for part in r.split(':') {
            emit_run(part, out);
        }
        return;
    }
    if r.contains('.') || r.contains('/') {
        // Numbers with dots: float (one dot) or version (many dots).
        let digits_dots = r.chars().all(|c| c.is_ascii_digit() || c == '.');
        if digits_dots {
            out.push(RawToken {
                text: r.to_string(),
                wordy: false,
            });
            return;
        }
        if is_scientific(r) {
            out.push(RawToken {
                text: r.to_string(),
                wordy: false,
            });
            return;
        }
        // host / file / dotted word: one token, simple dictionary.
        out.push(RawToken {
            text: r.to_string(),
            wordy: false,
        });
        return;
    }
    if is_scientific(r) {
        out.push(RawToken {
            text: r.to_string(),
            wordy: false,
        });
        return;
    }
    out.push(classify_component(r));
}

/// Tokenize input text into raw tokens (PG's default parser,
/// approximated: tag and XML-entity tokens are unmapped in both
/// supported configurations and consume no position; a `word://`
/// protocol prefix is likewise skipped and the rest is a url token).
fn ts_tokenize(text: &str) -> Vec<RawToken> {
    let w: Vec<char> = text.chars().collect();
    let n = w.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        let c = w[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '<' && i + 1 < n && (w[i + 1].is_alphabetic() || w[i + 1] == '/') {
            // TAG token: skip to '>'.
            while i < n && w[i] != '>' {
                i += 1;
            }
            if i < n {
                i += 1;
            }
            continue;
        }
        if c == '&' {
            // XMLENTITY (&name; / &#123;) or a plain separator.
            let mut j = i + 1;
            while j < n && j - i <= 12 && (w[j].is_alphanumeric() || w[j] == '#') {
                j += 1;
            }
            if j < n && w[j] == ';' && j > i + 1 {
                i = j + 1;
                continue;
            }
            i += 1;
            continue;
        }
        if ts_is_word_char(c) {
            // URL with protocol: wordchars '://' rest-of-run.
            let mut j = i;
            while j < n && ts_is_word_char(w[j]) {
                j += 1;
            }
            if j + 2 < n && w[j] == ':' && w[j + 1] == '/' && w[j + 2] == '/' {
                // Protocol skipped; URL token runs to whitespace.
                // PG's wparser emits, in order: the full host+path,
                // the host, and the path (host alone when the path
                // is empty or a bare "/").
                let mut k = j + 3;
                while k < n && !w[k].is_whitespace() {
                    k += 1;
                }
                let url: String = w[j + 3..k].iter().collect();
                if !url.is_empty() {
                    let (host, path) = match url.find('/') {
                        Some(p) => (&url[..p], &url[p..]),
                        None => (&url[..], ""),
                    };
                    if path.len() > 1 {
                        out.push(RawToken {
                            text: url.clone(),
                            wordy: false,
                        });
                        out.push(RawToken {
                            text: host.to_string(),
                            wordy: false,
                        });
                        out.push(RawToken {
                            text: path.to_string(),
                            wordy: false,
                        });
                    } else if !host.is_empty() {
                        out.push(RawToken {
                            text: host.to_string(),
                            wordy: false,
                        });
                    }
                }
                i = k;
                continue;
            }
            // General run: word chars plus . - / : @ ~ %
            let mut k = i;
            while k < n
                && (ts_is_word_char(w[k])
                    || matches!(w[k], '.' | '-' | '/' | ':' | '@' | '~' | '%'))
            {
                k += 1;
            }
            let run: String = w[i..k].iter().collect();
            emit_run(&run, &mut out);
            i = k;
            continue;
        }
        // Any other punctuation is a separator.
        i += 1;
    }
    out
}

/// Apply a configuration's dictionary chain to one raw token:
/// `None` = stop word (position still consumed).
fn dict_lexize(tok: &RawToken, cfg: TsConfig) -> Option<String> {
    match cfg {
        TsConfig::Simple => Some(tok.text.to_lowercase()),
        TsConfig::English => {
            if tok.wordy {
                let lw = tok.text.to_lowercase();
                if is_stopword(&lw) {
                    None
                } else if lw.len() > 1000 {
                    Some(lw)
                } else {
                    Some(english_stem(&lw))
                }
            } else {
                Some(tok.text.to_lowercase())
            }
        }
    }
}

/// PG's `parsetext`: (lexeme-or-stop, position) pairs in token order.
pub(crate) fn parsetext(text: &str, cfg: TsConfig) -> Vec<(Option<String>, u32)> {
    let mut out = Vec::new();
    let mut pos: u32 = 0;
    for tok in ts_tokenize(text) {
        pos += 1;
        out.push((dict_lexize(&tok, cfg), pos));
    }
    out
}

/// String values of a jsonb tree in document order (object keys
/// are not indexed; only string scalars contribute text).
fn collect_jsonb_strings(v: &super::json::JsonbVal, out: &mut Vec<String>) {
    match v {
        super::json::JsonbVal::Str(s) => out.push(s.clone()),
        super::json::JsonbVal::Array(items) => {
            for it in items {
                collect_jsonb_strings(it, out);
            }
        }
        super::json::JsonbVal::Object(pairs) => {
            for (_, val) in pairs {
                collect_jsonb_strings(val, out);
            }
        }
        _ => {}
    }
}

/// Same for a parsed `json` document (JsonNode tree).
fn collect_json_strings(n: &super::json::JsonNode, out: &mut Vec<String>) {
    match &n.kind {
        super::json::JsonKind::Str(s) => out.push(s.clone()),
        super::json::JsonKind::Array(items) => {
            for it in items {
                collect_json_strings(it, out);
            }
        }
        super::json::JsonKind::Object(pairs) => {
            for (_, val) in pairs {
                collect_json_strings(val, out);
            }
        }
        _ => {}
    }
}

/// PG's jsonb_to_tsvector_worker (to_tsany.c): each string element
/// is parsetext'd with positions continuing across elements; after
/// an element that produced at least one lexeme, the position
/// advances by one extra (an artificial break so phrase searches
/// don't join across elements). Stopword-only elements consume
/// their token positions but trigger no break.
pub(crate) fn json_strings_to_tsvector(strings: &[String], cfg: TsConfig) -> TsVectorVal {
    let mut by_lexeme: Vec<(String, Vec<(u16, u8)>)> = Vec::new();
    let mut base: u32 = 0;
    for s in strings {
        let pairs = parsetext(s, cfg);
        let ntok = pairs.len() as u32;
        let mut any = false;
        for (lex, p) in pairs {
            if let Some(lex) = lex {
                any = true;
                let abs = base + p;
                let pp = if abs > MAXPOS as u32 {
                    MAXPOS
                } else {
                    abs as u16
                };
                by_lexeme.push((lex, vec![(pp, 0)]));
            }
        }
        base += ntok;
        if any {
            base += 1;
        }
    }
    build_tsvector(by_lexeme).unwrap_or(TsVectorVal {
        entries: Vec::new(),
    })
}

/// The `to_tsvector([cfg,] json)` entry point used by the
/// funcs.rs intercept (the json type rides in Value::Text, so the
/// caller resolved the overload from the argument's static type).
pub(crate) fn to_tsvector_json(cfg_val: Option<&Value>, doc: &Value) -> Result<Value, ExecError> {
    let cfg = match cfg_val {
        Some(v) => ts_config_arg(v)?,
        None => TsConfig::English,
    };
    let text = ts_text_arg(doc).ok_or_else(|| no_ts_func("to_tsvector", &[doc.clone()]))?;
    let parsed = super::json::parse_json(&text)
        .map_err(|e| exec_err("22P02", format!("invalid input syntax for type json: {e}")))?;
    let mut strings = Vec::new();
    collect_json_strings(&parsed.root, &mut strings);
    Ok(Value::tsvector(json_strings_to_tsvector(&strings, cfg)))
}

/// PG's `to_tsvector`: parsetext + make_tsvector (weights all D).
pub(crate) fn text_to_tsvector(text: &str, cfg: TsConfig) -> TsVectorVal {
    let mut by_lexeme: Vec<(String, Vec<(u16, u8)>)> = Vec::new();
    for (lex, pos) in parsetext(text, cfg) {
        if let Some(lex) = lex {
            let p = if pos > MAXPOS as u32 {
                MAXPOS
            } else {
                pos as u16
            };
            by_lexeme.push((lex, vec![(p, 0)]));
        }
    }
    // Input lexemes are within limits by construction (a >2047-byte
    // token would need an absurd input; build_tsvector's error path
    // is unreachable in practice, but fall back to truncation-free
    // empty rather than panic).
    build_tsvector(by_lexeme).unwrap_or(TsVectorVal {
        entries: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Query construction from text (to_tsany.c pushval_morph and friends)
// ---------------------------------------------------------------------------

/// How sibling words inside one morph operand are joined.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MorphJoin {
    Phrase,
    And,
}

/// PG's `pushval_morph`: tokenize one operand's text, then join the
/// per-position word groups with the join operator; stopword positions
/// become Stop leaves (cleaned up later). The operand's weight/prefix
/// apply to every produced lexeme.
pub(crate) fn morph_expand(
    text: &str,
    cfg: &TsConfig,
    weight: u8,
    prefix: bool,
    join: MorphJoin,
) -> TsQueryNode {
    let parsed = parsetext(text, *cfg);
    // Group by position (positions are strictly increasing here, but
    // group defensively like PG's pushval_morph state machine).
    let mut groups: Vec<(u32, Vec<Option<String>>)> = Vec::new();
    for (lex, pos) in parsed {
        match groups.last_mut() {
            Some((p, words)) if *p == pos => words.push(lex),
            _ => groups.push((pos, vec![lex])),
        }
    }
    let mut acc: Option<TsQueryNode> = None;
    for (_, words) in groups {
        // Variants at one position are OR'd; the words of one variant
        // AND'd (this engine's dictionaries yield at most one lexeme
        // per position, so both folds are trivial in practice).
        let mut gnode: Option<TsQueryNode> = None;
        for word in words {
            let leaf = match word {
                Some(l) => TsQueryNode::Val {
                    text: l,
                    weight,
                    prefix,
                    id: 0,
                },
                None => TsQueryNode::Stop,
            };
            gnode = Some(match gnode {
                None => leaf,
                Some(prev) => TsQueryNode::Or(Box::new(prev), Box::new(leaf)),
            });
        }
        let gnode = gnode.unwrap_or(TsQueryNode::Stop);
        acc = Some(match acc {
            None => gnode,
            Some(prev) => match join {
                MorphJoin::Phrase => TsQueryNode::Phrase(Box::new(prev), Box::new(gnode), 1),
                MorphJoin::And => TsQueryNode::And(Box::new(prev), Box::new(gnode)),
            },
        });
    }
    acc.unwrap_or(TsQueryNode::Stop)
}

fn morph_operand(text: &str, cfg: &TsConfig, weight: u8, prefix: bool) -> TsQueryNode {
    morph_expand(text, cfg, weight, prefix, MorphJoin::Phrase)
}

fn finish_query(root: Option<TsQueryNode>) -> TsQueryVal {
    let root = root.and_then(cleanup_stopwords);
    let mut q = TsQueryVal { root };
    assign_operand_ids(&mut q);
    q
}

/// PG's `to_tsquery`: the standard tsquery grammar with dictionary
/// (morph) expansion of each operand.
pub(crate) fn text_to_tsquery(text: &str, cfg: TsConfig) -> Result<TsQueryVal, ExecError> {
    TsqParser::new(text, OperandMode::Morph(&cfg)).parse_query()
}

/// PG's `plainto_tsquery`: the whole input is one operand, words
/// joined by AND.
pub(crate) fn plainto_tsquery(text: &str, cfg: TsConfig) -> TsQueryVal {
    if text.trim().is_empty() {
        return TsQueryVal { root: None };
    }
    let root = morph_expand(text, &cfg, 0, false, MorphJoin::And);
    finish_query(Some(root))
}

/// PG's `phraseto_tsquery`: the whole input is one operand, words
/// joined by the phrase operator.
pub(crate) fn phraseto_tsquery(text: &str, cfg: TsConfig) -> TsQueryVal {
    if text.trim().is_empty() {
        return TsQueryVal { root: None };
    }
    let root = morph_expand(text, &cfg, 0, false, MorphJoin::Phrase);
    finish_query(Some(root))
}

/// PG's `websearch_to_tsquery`: a port of tsquery.c's
/// `gettoken_query_websearch` driving the same fold makepol does.
/// In operand position: each `-` is a NOT (they stack), a
/// double-quoted run is one operand (its words phrase-joined),
/// operator punctuation is skipped, and any other run is one
/// operand (morph-expanded, phrase-joined). In operator position:
/// the keyword `or` (PG's parse_or_operator rule) is OR, other
/// operator punctuation is skipped, and anything else inserts an
/// implicit AND.
pub(crate) fn websearch_to_tsquery(text: &str, cfg: TsConfig) -> TsQueryVal {
    let w: Vec<char> = text.chars().collect();
    let n = w.len();
    let mut i = 0usize;
    let is_op = |c: char| matches!(c, '!' | '&' | '|' | '(' | ')' | '<' | '>' | ':');
    enum Item {
        Operand(TsQueryNode),
        Or,
    }
    let mut items: Vec<Item> = Vec::new();
    let mut pending_nots = 0usize;
    let mut expect_operand = true;
    while i < n {
        if expect_operand {
            let c = w[i];
            if c == '-' {
                pending_nots += 1;
                i += 1;
                continue;
            }
            if is_op(c) || c.is_whitespace() {
                i += 1;
                continue;
            }
            let node = if c == '"' {
                i += 1;
                let start = i;
                while i < n && w[i] != '"' {
                    i += 1;
                }
                let chunk: String = w[start..i].iter().collect();
                if i < n {
                    i += 1; // closing quote
                }
                morph_expand(&chunk, &cfg, 0, false, MorphJoin::Phrase)
            } else {
                let start = i;
                while i < n
                    && (ts_is_word_char(w[i])
                        || matches!(w[i], '.' | '-' | '/' | '@' | '~' | '%' | '_' | '\''))
                {
                    i += 1;
                }
                if i == start {
                    // A separator character that starts no token
                    // (PG's parser skips it the same way).
                    i += 1;
                    continue;
                }
                let chunk: String = w[start..i].iter().collect();
                morph_expand(&chunk, &cfg, 0, false, MorphJoin::Phrase)
            };
            let mut node = node;
            for _ in 0..pending_nots {
                node = TsQueryNode::Not(Box::new(node));
            }
            pending_nots = 0;
            items.push(Item::Operand(node));
            expect_operand = false;
        } else {
            // parse_or_operator at this exact position.
            let or_here = i + 1 < n
                && w[i].eq_ignore_ascii_case(&'o')
                && w[i + 1].eq_ignore_ascii_case(&'r')
                && i + 2 < n
                && !matches!(w[i + 2], '-' | '_')
                && !w[i + 2].is_alphanumeric()
                && {
                    let mut k = i + 3;
                    let mut found = false;
                    while k < n {
                        if !w[k].is_whitespace() {
                            found = true;
                            break;
                        }
                        k += 1;
                    }
                    found
                };
            if or_here {
                items.push(Item::Or);
                i += 2;
                expect_operand = true;
            } else if is_op(w[i]) || w[i].is_whitespace() {
                i += 1;
            } else {
                // Implicit AND before the next operand.
                expect_operand = true;
            }
        }
    }
    // Fold: OR splits groups; within a group, AND (makepol order).
    let mut groups: Vec<TsQueryNode> = Vec::new();
    let mut cur: Option<TsQueryNode> = None;
    for item in items {
        match item {
            Item::Operand(node) => {
                cur = Some(match cur {
                    None => node,
                    Some(prev) => TsQueryNode::And(Box::new(prev), Box::new(node)),
                });
            }
            Item::Or => {
                if let Some(g) = cur.take() {
                    groups.push(g);
                }
            }
        }
    }
    if let Some(g) = cur {
        groups.push(g);
    }
    let mut root: Option<TsQueryNode> = None;
    for g in groups {
        root = Some(match root {
            None => g,
            Some(prev) => TsQueryNode::Or(Box::new(prev), Box::new(g)),
        });
    }
    finish_query(root)
}

// ---------------------------------------------------------------------------
// ts_rank / ts_rank_cd (tsrank.c)
// ---------------------------------------------------------------------------

/// PG's default_weights, indexed by position weight (0=D .. 3=A).
pub(crate) const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];

/// PG's `getWeights` over already-extracted elements: `ndim_ok` is
/// false when the source array was not one-dimensional.
pub(crate) fn get_weights(elems: &[Option<f32>], ndim_ok: bool) -> Result<[f32; 4], ExecError> {
    if !ndim_ok {
        return Err(exec_err(
            "2202E",
            "array of weight must be one-dimensional".to_string(),
        ));
    }
    if elems.len() < 4 {
        return Err(exec_err(
            "2202E",
            "array of weight is too short".to_string(),
        ));
    }
    if elems.iter().any(|e| e.is_none()) {
        return Err(exec_err(
            "22004",
            "array of weight must not contain nulls".to_string(),
        ));
    }
    let mut ws = [0.0f32; 4];
    for i in 0..4 {
        let v = elems[i].unwrap();
        ws[i] = if v >= 0.0 { v } else { DEFAULT_WEIGHTS[i] };
        if ws[i] > 1.0 {
            return Err(exec_err("22023", "weight out of range".to_string()));
        }
    }
    Ok(ws)
}

/// One query operand in polish order (id = its polish index among
/// operands, matching the ids assigned by `assign_operand_ids`).
struct QOperand {
    text: String,
    weight: u8,
    prefix: bool,
}

fn query_operands(q: &TsQueryVal) -> Vec<QOperand> {
    let mut out = Vec::new();
    if let Some(root) = &q.root {
        let mut raw = Vec::new();
        root.operands_polish(&mut raw);
        out = raw
            .into_iter()
            .map(|(_, text, weight, prefix)| QOperand {
                text,
                weight,
                prefix,
            })
            .collect();
    }
    out
}

/// PG's `SortAndUniqItems`: operands sorted by string, deduplicated
/// by string only.
fn dedup_operands(ops: Vec<QOperand>) -> Vec<QOperand> {
    let mut ops = ops;
    ops.sort_by(|a, b| ts_cmp_str(a.text.as_bytes(), b.text.as_bytes()));
    ops.dedup_by(|next, keep| next.text == keep.text);
    ops
}

/// Entries matching an operand (exact, or the prefix range).
fn rank_entries<'a>(v: &'a TsVectorVal, op: &QOperand) -> Vec<&'a (String, Vec<(u16, u8)>)> {
    if op.prefix {
        prefix_entries(v, &op.text)
    } else {
        find_entry(v, &op.text).into_iter().collect()
    }
}

/// PG's `cnt_length`: total position count (positionless entries
/// count 1).
fn cnt_length(v: &TsVectorVal) -> i64 {
    v.entries
        .iter()
        .map(|(_, p)| if p.is_empty() { 1 } else { p.len() as i64 })
        .sum()
}

/// PG's `word_distance` (float result of a double expression).
fn word_distance(w: i32) -> f32 {
    if w > 100 {
        return 1e-30;
    }
    (1.0f64 / (1.005 + 0.05 * ((w as f32 as f64) / 1.5 - 2.0).exp())) as f32
}

fn calc_rank_or(w: &[f32; 4], v: &TsVectorVal, q: &TsQueryVal) -> f32 {
    let items = dedup_operands(query_operands(q));
    let size = items.len();
    let mut res: f32 = 0.0;
    for item in &items {
        for entry in rank_entries(v, item) {
            let dummy: &[(u16, u8)] = &[(0, 0)];
            let post: &[(u16, u8)] = if entry.1.is_empty() { dummy } else { &entry.1 };
            let mut resj: f32 = 0.0;
            let mut wjm: f32 = -1.0;
            let mut jm: i64 = 0;
            for (j, (_, wt)) in post.iter().enumerate() {
                let wp = w[*wt as usize];
                let jj = j as i64 + 1;
                resj += wp / (jj * jj) as f32;
                if wp > wjm {
                    wjm = wp;
                    jm = j as i64;
                }
            }
            let term = resj - wjm / ((jm + 1) * (jm + 1)) as f32 + wjm;
            res = (res as f64 + term as f64 / 1.64493406685) as f32;
        }
    }
    if size > 0 {
        res /= size as f32;
    }
    res
}

fn calc_rank_and(w: &[f32; 4], v: &TsVectorVal, q: &TsQueryVal) -> f32 {
    let items = dedup_operands(query_operands(q));
    if items.len() < 2 {
        return calc_rank_or(w, v, q);
    }
    let mut res: f32 = -1.0;
    // pos[i] / dummy[i]: the position list of item i's current entry.
    let mut pos: Vec<Option<&[(u16, u8)]>> = vec![None; items.len()];
    let mut is_dummy: Vec<bool> = vec![false; items.len()];
    let dummy_pos: &[(u16, u8)] = &[(MAXPOS, 0)];
    for i in 0..items.len() {
        for entry in rank_entries(v, &items[i]) {
            let (post, dum): (&[(u16, u8)], bool) = if entry.1.is_empty() {
                (dummy_pos, true)
            } else {
                (&entry.1, false)
            };
            pos[i] = Some(post);
            is_dummy[i] = dum;
            for k in 0..i {
                let Some(ct) = pos[k] else { continue };
                for (ppos, pw) in post {
                    for (cpos, cw) in ct {
                        let dist = (*ppos as i32 - *cpos as i32).abs();
                        if dist != 0 || is_dummy[i] || is_dummy[k] {
                            let d = if dist == 0 { MAXPOS as i32 + 1 } else { dist };
                            // C computes the product in float, then
                            // takes the (double) sqrt into a float.
                            let prod: f32 = w[*pw as usize] * w[*cw as usize] * word_distance(d);
                            let curw = (prod as f64).sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                1.0 - (1.0 - res) * (1.0 - curw)
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

/// PG's `calc_rank` (the ts_rank family): float4 throughout.
pub(crate) fn ts_rank(w: &[f32; 4], v: &TsVectorVal, q: &TsQueryVal, method: i32) -> f32 {
    if v.entries.is_empty() || q.root.is_none() {
        return 0.0;
    }
    let root_is_and = matches!(
        q.root.as_ref().unwrap(),
        TsQueryNode::And(..) | TsQueryNode::Phrase(..)
    );
    let mut res = if root_is_and {
        calc_rank_and(w, v, q)
    } else {
        calc_rank_or(w, v, q)
    };
    if res < 0.0 {
        res = 1e-20;
    }
    if method & 0x01 != 0 && !v.entries.is_empty() {
        res = (res as f64 / (((cnt_length(v) + 1) as f64).ln() / std::f64::consts::LN_2)) as f32;
    }
    if method & 0x02 != 0 {
        let len = cnt_length(v);
        if len > 0 {
            res /= len as f32;
        }
    }
    // 0x04 (EXTDIST) is not applicable to ts_rank.
    if method & 0x08 != 0 && !v.entries.is_empty() {
        res /= v.entries.len() as f32;
    }
    if method & 0x10 != 0 && !v.entries.is_empty() {
        res = (res as f64 / (((v.entries.len() + 1) as f64).ln() / std::f64::consts::LN_2)) as f32;
    }
    if method & 0x20 != 0 {
        res /= res + 1.0;
    }
    res
}

// --- ts_rank_cd: DocRepresentation / Cover machinery ----------------

#[derive(Clone)]
struct DocEntry {
    pos: u16,
    weight: u8,
    entry_idx: usize,
    items: Vec<u32>,
}

/// QueryRepresentation operand data for Cover's TS_execute.
#[derive(Default)]
struct OpData {
    exists: bool,
    reverse: bool,
    /// (pos, weight) in insertion order.
    pos: Vec<(u16, u8)>,
}

struct QueryRep {
    data: Vec<OpData>,
}

impl QueryRep {
    fn reset(&mut self, reverse: bool) {
        for d in self.data.iter_mut() {
            d.exists = false;
            d.reverse = reverse;
            d.pos.clear();
        }
    }

    fn fill(&mut self, entry: &DocEntry) {
        for &item in &entry.items {
            let d = &mut self.data[item as usize];
            d.exists = true;
            let last_same = d.pos.last().map(|(p, _)| *p == entry.pos).unwrap_or(false);
            if !last_same {
                d.pos.push((entry.pos, entry.weight));
            }
        }
    }

    /// The operand's positions as phrase data (insertion order, or
    /// reversed when filled by the backward scan — PG stores them in
    /// a fixed array read from the tail in reverse mode).
    fn phrase_positions(&self, item: u32) -> Vec<u16> {
        let d = &self.data[item as usize];
        if d.reverse {
            d.pos.iter().rev().map(|(p, _)| *p).collect()
        } else {
            d.pos.iter().map(|(p, _)| *p).collect()
        }
    }
}

fn qr_execute(qr: &QueryRep, n: &TsQueryNode) -> Tri {
    match n {
        TsQueryNode::Val { id, .. } => {
            if qr.data[*id as usize].exists {
                Tri::Yes
            } else {
                Tri::No
            }
        }
        TsQueryNode::Stop => Tri::No,
        TsQueryNode::Not(c) => match qr_execute(qr, c) {
            Tri::Yes => Tri::No,
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
        },
        TsQueryNode::And(l, r) => {
            let ls = qr_execute(qr, l);
            if ls == Tri::No {
                return Tri::No;
            }
            match (ls, qr_execute(qr, r)) {
                (Tri::Yes, Tri::Yes) => Tri::Yes,
                (Tri::No, _) | (_, Tri::No) => Tri::No,
                _ => Tri::Maybe,
            }
        }
        TsQueryNode::Or(l, r) => {
            let ls = qr_execute(qr, l);
            if ls == Tri::Yes {
                return Tri::Yes;
            }
            match (ls, qr_execute(qr, r)) {
                (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
                (Tri::No, Tri::No) => Tri::No,
                _ => Tri::Maybe,
            }
        }
        TsQueryNode::Phrase(..) => qr_phrase_execute(qr, n).0,
    }
}

fn qr_phrase_execute(qr: &QueryRep, n: &TsQueryNode) -> (Tri, PhraseData) {
    match n {
        TsQueryNode::Val { id, .. } => {
            let d = &qr.data[*id as usize];
            if !d.exists {
                return (Tri::No, PhraseData::default());
            }
            (
                Tri::Yes,
                PhraseData {
                    pos: qr.phrase_positions(*id),
                    width: 0,
                    negate: false,
                },
            )
        }
        other => {
            // Operators behave exactly as in phrase_execute; only the
            // operand source differs, so re-run the same combinators
            // by delegating through a tiny recursive copy.
            phrase_combine(other, &mut |child| qr_phrase_execute(qr, child))
        }
    }
}

/// The operator combinators of TS_phrase_execute, factored so the
/// QueryRepresentation variant shares the logic.
fn phrase_combine(
    n: &TsQueryNode,
    rec: &mut dyn FnMut(&TsQueryNode) -> (Tri, PhraseData),
) -> (Tri, PhraseData) {
    match n {
        TsQueryNode::Not(child) => {
            let (state, mut data) = rec(child);
            match state {
                Tri::No => {
                    data.pos.clear();
                    data.negate = true;
                    (Tri::Yes, data)
                }
                Tri::Yes => {
                    if !data.pos.is_empty() {
                        data.negate = !data.negate;
                        (Tri::Yes, data)
                    } else if data.negate {
                        data.negate = false;
                        (Tri::No, data)
                    } else {
                        (Tri::Yes, data)
                    }
                }
                Tri::Maybe => (Tri::Maybe, PhraseData::default()),
            }
        }
        TsQueryNode::And(l, r) | TsQueryNode::Or(l, r) | TsQueryNode::Phrase(l, r, _) => {
            let is_or = matches!(n, TsQueryNode::Or(..));
            let is_phrase = matches!(n, TsQueryNode::Phrase(..));
            let (lstate, mut ldata) = rec(l);
            let (rstate, mut rdata) = rec(r);
            if is_or {
                if lstate == Tri::No && rstate == Tri::No {
                    return (Tri::No, PhraseData::default());
                }
                if lstate == Tri::Maybe || rstate == Tri::Maybe {
                    return (Tri::Maybe, PhraseData::default());
                }
                if lstate == Tri::No {
                    ldata.width = 0;
                }
                if rstate == Tri::No {
                    rdata.width = 0;
                }
            } else {
                if lstate == Tri::No || rstate == Tri::No {
                    return (Tri::No, PhraseData::default());
                }
                if lstate == Tri::Maybe || rstate == Tri::Maybe {
                    return (Tri::Maybe, PhraseData::default());
                }
            }
            let (loffset, roffset, width);
            if is_phrase {
                let dist = match n {
                    TsQueryNode::Phrase(_, _, d) => *d as i32,
                    _ => unreachable!(),
                };
                loffset = dist + rdata.width;
                roffset = 0;
                width = dist + ldata.width + rdata.width;
            } else {
                let maxwidth = ldata.width.max(rdata.width);
                loffset = maxwidth - ldata.width;
                roffset = maxwidth - rdata.width;
                width = maxwidth;
            }
            let mut data = PhraseData {
                pos: Vec::new(),
                width,
                negate: false,
            };
            match (ldata.negate, rdata.negate) {
                (true, true) => {
                    let emit = if is_or {
                        TSPO_BOTH
                    } else {
                        TSPO_BOTH | TSPO_L_ONLY | TSPO_R_ONLY
                    };
                    data.pos = phrase_output(&ldata, &rdata, emit, loffset, roffset);
                    data.negate = true;
                    (Tri::Yes, data)
                }
                (true, false) => {
                    if is_or {
                        data.pos = phrase_output(&ldata, &rdata, TSPO_L_ONLY, loffset, roffset);
                        data.negate = true;
                        (Tri::Yes, data)
                    } else {
                        data.pos = phrase_output(&ldata, &rdata, TSPO_R_ONLY, loffset, roffset);
                        let state = if data.pos.is_empty() {
                            Tri::No
                        } else {
                            Tri::Yes
                        };
                        (state, data)
                    }
                }
                (false, true) => {
                    if is_or {
                        data.pos = phrase_output(&ldata, &rdata, TSPO_R_ONLY, loffset, roffset);
                        data.negate = true;
                        (Tri::Yes, data)
                    } else {
                        data.pos = phrase_output(&ldata, &rdata, TSPO_L_ONLY, loffset, roffset);
                        let state = if data.pos.is_empty() {
                            Tri::No
                        } else {
                            Tri::Yes
                        };
                        (state, data)
                    }
                }
                (false, false) => {
                    let emit = if is_or {
                        TSPO_BOTH | TSPO_L_ONLY | TSPO_R_ONLY
                    } else {
                        TSPO_BOTH
                    };
                    data.pos = phrase_output(&ldata, &rdata, emit, loffset, roffset);
                    let state = if data.pos.is_empty() {
                        Tri::No
                    } else {
                        Tri::Yes
                    };
                    (state, data)
                }
            }
        }
        _ => unreachable!("phrase_combine on operand"),
    }
}

struct CoverExt {
    pos: usize,
    p: i32,
    q: i32,
    begin: usize,
    end: usize,
}

/// PG's `Cover`: find the next cover of the query in the document
/// representation starting at `ext.pos`.
fn cover(doc: &[DocEntry], root: &TsQueryNode, qr: &mut QueryRep, ext: &mut CoverExt) -> bool {
    loop {
        qr.reset(false);
        ext.p = i32::MAX;
        ext.q = 0;
        let mut ptr = ext.pos;
        let mut found = false;
        let mut lastpos = ext.pos;
        while ptr < doc.len() {
            qr.fill(&doc[ptr]);
            if qr_execute(qr, root) == Tri::Yes {
                if doc[ptr].pos as i32 > ext.q {
                    ext.q = doc[ptr].pos as i32;
                    ext.end = ptr;
                    lastpos = ptr;
                    found = true;
                }
                break;
            }
            ptr += 1;
        }
        if !found {
            return false;
        }
        qr.reset(true);
        let mut ptr = lastpos as i64;
        while ptr >= ext.pos as i64 {
            qr.fill(&doc[ptr as usize]);
            if qr_execute(qr, root) == Tri::Yes {
                if (doc[ptr as usize].pos as i32) < ext.p {
                    ext.begin = ptr as usize;
                    ext.p = doc[ptr as usize].pos as i32;
                }
                break;
            }
            ptr -= 1;
        }
        if ext.p <= ext.q {
            ext.pos = (ptr + 1) as usize;
            return true;
        }
        ext.pos += 1;
    }
}

/// PG's `calc_rank_cd` (cover density), computed in double precision
/// and returned as float4.
pub(crate) fn ts_rank_cd(w: &[f32; 4], v: &TsVectorVal, q: &TsQueryVal, method: i32) -> f32 {
    let invws: [f64; 4] = [
        1.0 / w[0] as f64,
        1.0 / w[1] as f64,
        1.0 / w[2] as f64,
        1.0 / w[3] as f64,
    ];
    let Some(root) = &q.root else {
        return 0.0;
    };
    let doc = get_docrep_full(v, q);
    if doc.is_empty() {
        return 0.0;
    }
    let n_operands = query_operands(q).len();
    let mut qr = QueryRep {
        data: (0..n_operands).map(|_| OpData::default()).collect(),
    };
    let mut ext = CoverExt {
        pos: 0,
        p: 0,
        q: 0,
        begin: 0,
        end: 0,
    };
    let mut wdoc: f64 = 0.0;
    let mut sum_dist: f64 = 0.0;
    let mut prev_ext_pos: f64 = 0.0;
    let mut n_extent: i64 = 0;
    while cover(&doc, root, &mut qr, &mut ext) {
        let mut inv_sum: f64 = 0.0;
        for d in &doc[ext.begin..=ext.end] {
            inv_sum += invws[d.weight as usize];
        }
        let cpos = (ext.end - ext.begin + 1) as f64 / inv_sum;
        let mut n_noise = (ext.q - ext.p) - (ext.end - ext.begin) as i32;
        if n_noise < 0 {
            n_noise = (ext.end - ext.begin) as i32 / 2;
        }
        wdoc += cpos / (1 + n_noise) as f64;
        let cur_ext_pos = (ext.q + ext.p) as f64 / 2.0;
        if n_extent > 0 && cur_ext_pos > prev_ext_pos {
            sum_dist += 1.0 / (cur_ext_pos - prev_ext_pos);
        }
        prev_ext_pos = cur_ext_pos;
        n_extent += 1;
    }
    if method & 0x01 != 0 && !v.entries.is_empty() {
        wdoc /= ((cnt_length(v) + 1) as f64).ln();
    }
    if method & 0x02 != 0 {
        let len = cnt_length(v);
        if len > 0 {
            wdoc /= len as f64;
        }
    }
    if method & 0x04 != 0 && n_extent > 0 && sum_dist > 0.0 {
        wdoc /= n_extent as f64 / sum_dist;
    }
    if method & 0x08 != 0 && !v.entries.is_empty() {
        wdoc /= v.entries.len() as f64;
    }
    if method & 0x10 != 0 && !v.entries.is_empty() {
        wdoc /= ((v.entries.len() + 1) as f64).ln() / std::f64::consts::LN_2;
    }
    if method & 0x20 != 0 {
        wdoc /= wdoc + 1.0;
    }
    wdoc as f32
}

/// The full get_docrep with real entry indexes (kept separate so the
/// grouping logic is testable).
fn get_docrep_full(v: &TsVectorVal, q: &TsQueryVal) -> Vec<DocEntry> {
    let mut raw: Vec<DocEntry> = Vec::new();
    for (idx, op) in query_operands(q).iter().enumerate() {
        for entry in rank_entries(v, op) {
            if entry.1.is_empty() {
                continue;
            }
            let entry_idx = (entry as *const (String, Vec<(u16, u8)>)) as usize;
            // Recover the index by scanning (entries are unique).
            let entry_idx = v
                .entries
                .iter()
                .position(|e| std::ptr::eq(e, entry))
                .unwrap_or(entry_idx);
            for (p, wt) in &entry.1 {
                if weight_ok(op.weight, *wt) {
                    raw.push(DocEntry {
                        pos: *p,
                        weight: *wt,
                        entry_idx,
                        items: vec![idx as u32],
                    });
                }
            }
        }
    }
    raw.sort_by(|a, b| {
        a.pos
            .cmp(&b.pos)
            .then(a.weight.cmp(&b.weight))
            .then(a.entry_idx.cmp(&b.entry_idx))
    });
    // Group consecutive (pos, weight, entry) runs.
    let mut out: Vec<DocEntry> = Vec::new();
    for d in raw {
        match out.last_mut() {
            Some(last)
                if last.pos == d.pos
                    && last.weight == d.weight
                    && last.entry_idx == d.entry_idx =>
            {
                last.items.extend(d.items);
            }
            _ => out.push(d),
        }
    }
    out
}

// =====================================================================
// Operator dispatch (@@, && between tsqueries)
// =====================================================================

/// v1.94: the JsonOp-routed text-search operators. `@@` is
/// tsvector @@ tsquery in either operand order (PG has both
/// operator entries); a text operand against a tsvector parses as
/// tsquery, and text against a tsquery is first run through
/// to_tsvector with the default (english) configuration, exactly
/// like PG's text @@ tsquery operator. `&&` between tsqueries is
/// tsquery_and. Everything else is 42883 operator-does-not-exist.
pub(crate) fn eval_ts_op(op: JsonOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    let no_op = |l: &Value, r: &Value| {
        exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                value_type_name(l),
                op.sql(),
                value_type_name(r)
            ),
        )
    };
    match op {
        JsonOp::PathMatch => {
            // Coerce a bare text operand: against a tsvector it is a
            // tsquery; against a tsquery it is a tsvector (document).
            let l = match (left, right) {
                (Value::Text(t) | Value::BpChar(t), Value::TsVector(_)) => {
                    Value::tsquery(parse_tsquery(t)?)
                }
                (Value::Text(t) | Value::BpChar(t), Value::TsQuery(_)) => {
                    Value::tsvector(text_to_tsvector(t, TsConfig::English))
                }
                _ => left.clone(),
            };
            let r = match (right, left) {
                (Value::Text(t) | Value::BpChar(t), Value::TsVector(_)) => {
                    Value::tsquery(parse_tsquery(t)?)
                }
                (Value::Text(t) | Value::BpChar(t), Value::TsQuery(_)) => {
                    Value::tsvector(text_to_tsvector(t, TsConfig::English))
                }
                _ => right.clone(),
            };
            match (&l, &r) {
                (Value::TsVector(v), Value::TsQuery(q)) => Ok(Value::Bool(ts_match(v, q))),
                (Value::TsQuery(q), Value::TsVector(v)) => Ok(Value::Bool(ts_match(v, q))),
                _ => Err(no_op(left, right)),
            }
        }
        JsonOp::Overlaps => {
            // PG coerces an unknown-type literal operand to tsquery
            // (the operator's declared arg type); mirror the @@
            // text handling for a text side facing a tsquery.
            let coerced;
            let (l, r) = match (left, right) {
                (Value::TsQuery(_), Value::Text(t) | Value::BpChar(t)) => {
                    coerced = Value::tsquery(parse_tsquery(t)?);
                    (left, &coerced)
                }
                (Value::Text(t) | Value::BpChar(t), Value::TsQuery(_)) => {
                    coerced = Value::tsquery(parse_tsquery(t)?);
                    (&coerced, right)
                }
                _ => (left, right),
            };
            match (l, r) {
                (Value::TsQuery(a), Value::TsQuery(b)) => Ok(Value::tsquery(tsquery_and(a, b))),
                _ => Err(no_op(left, right)),
            }
        }
        _ => Err(no_op(left, right)),
    }
}

// =====================================================================
// Function dispatch (funcs.rs eval_func_vals hook)
// =====================================================================

/// Names claimed by the text-search dispatch. All are STRICT in PG:
/// any NULL argument yields NULL (handled in the hook).
const TS_FUNC_NAMES: &[&str] = &[
    "to_tsvector",
    "to_tsquery",
    "plainto_tsquery",
    "phraseto_tsquery",
    "websearch_to_tsquery",
    "ts_rank",
    "ts_rank_cd",
    "ts_delete",
    "setweight",
    "strip",
    "length",
    "numnode",
    "tsvector_to_array",
    "array_to_tsvector",
    "ts_filter",
    "tsvectorin",
    "tsvectorout",
    "tsqueryin",
    "tsqueryout",
    "tsquery_phrase",
    "__tsquery_phrase",
    "tsquery_not",
    "__tsquery_not",
    "tsquery_and",
    "tsquery_or",
    "get_current_ts_config",
];

fn no_ts_func(name: &str, vals: &[Value]) -> ExecError {
    let sig = vals
        .iter()
        .map(|v| value_type_name(v).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    exec_err(
        "42883",
        format!("function {}({}) does not exist", name, sig),
    )
}

/// A text-ish argument (text / bpchar), else None.
fn ts_text_arg(v: &Value) -> Option<String> {
    match v {
        Value::Text(t) | Value::BpChar(t) => Some(t.to_string()),
        _ => None,
    }
}

/// A tsvector argument: the value itself, or bare text parsed
/// through tsvectorin (unknown-literal coercion, as in PG).
fn ts_vec_arg(v: &Value) -> Result<Option<TsVectorVal>, ExecError> {
    match v {
        Value::TsVector(x) => Ok(Some((**x).clone())),
        Value::Text(t) | Value::BpChar(t) => parse_tsvector(t).map(Some),
        _ => Ok(None),
    }
}

/// A tsquery argument (see above).
fn ts_query_arg(v: &Value) -> Result<Option<TsQueryVal>, ExecError> {
    match v {
        Value::TsQuery(x) => Ok(Some((**x).clone())),
        Value::Text(t) | Value::BpChar(t) => parse_tsquery(t).map(Some),
        _ => Ok(None),
    }
}

/// A regconfig argument: a configuration name as text, or a
/// regconfig OID as an integer (only `simple`, bootstrap OID 3748,
/// is known besides the session default `english`).
fn ts_config_arg(v: &Value) -> Result<TsConfig, ExecError> {
    match v {
        Value::Text(t) | Value::BpChar(t) => TsConfig::from_name(t).ok_or_else(|| {
            exec_err(
                "42704",
                format!("text search configuration \"{}\" does not exist", t),
            )
        }),
        Value::SmallInt(i) => ts_config_oid(*i as i64),
        Value::Int(i) => ts_config_oid(*i as i64),
        Value::BigInt(i) => ts_config_oid(*i),
        _ => Err(exec_err(
            "42704",
            format!(
                "text search configuration \"{}\" does not exist",
                v.to_text().unwrap_or_default()
            ),
        )),
    }
}

fn ts_config_oid(oid: i64) -> Result<TsConfig, ExecError> {
    // pg_ts_config bootstrap: only `simple` (3748) has a stable OID;
    // `english` is created by initdb without one we can rely on.
    if oid == 3748 {
        Ok(TsConfig::Simple)
    } else {
        Err(exec_err(
            "42704",
            format!("text search configuration \"{}\" does not exist", oid),
        ))
    }
}

/// A text-array argument: an Array value, or a `{...}` literal text
/// (unknown-literal coercion). Elements flatten to Option<String>.
fn ts_text_array_arg(v: &Value) -> Result<Option<Vec<Option<String>>>, ExecError> {
    let arr = match v {
        Value::Array(a) => (**a).clone(),
        Value::Text(t) | Value::BpChar(t) if t.trim_start().starts_with('{') => {
            super::parse_array_literal(t, crate::storage::ArrayElem::Text)?
        }
        _ => return Ok(None),
    };
    let mut out = Vec::with_capacity(arr.elems.len());
    for e in &arr.elems {
        out.push(match e {
            Value::Null => None,
            Value::Text(s) | Value::BpChar(s) => Some(s.to_string()),
            other => other.to_text(),
        });
    }
    Ok(Some(out))
}

/// A float-array (weights) argument: elements plus the
/// one-dimensional flag get_weights wants.
fn ts_float_array_arg(v: &Value) -> Result<Option<(Vec<Option<f32>>, bool)>, ExecError> {
    let arr = match v {
        Value::Array(a) => (**a).clone(),
        Value::Text(t) | Value::BpChar(t) if t.trim_start().starts_with('{') => {
            super::parse_array_literal(t, crate::storage::ArrayElem::Float4)?
        }
        _ => return Ok(None),
    };
    let ndim_ok = arr.dims.len() <= 1;
    let mut out = Vec::with_capacity(arr.elems.len());
    for e in &arr.elems {
        out.push(match e {
            Value::Null => None,
            Value::Float4(f) => Some(*f),
            Value::Float(f) => Some(*f as f32),
            Value::SmallInt(i) => Some(*i as f32),
            Value::Int(i) => Some(*i as f32),
            Value::BigInt(i) => Some(*i as f32),
            Value::Numeric(n) => Some(n.to_f64() as f32),
            Value::Text(s) | Value::BpChar(s) => s.trim().parse::<f32>().ok(),
            _ => None,
        });
    }
    Ok(Some((out, ndim_ok)))
}

fn ts_int_arg(v: &Value) -> Option<i64> {
    match v {
        Value::SmallInt(i) => Some(*i as i64),
        Value::Int(i) => Some(*i as i64),
        Value::BigInt(i) => Some(*i),
        _ => None,
    }
}

/// The eval_func_vals hook (mirrors ranges::try_eval_range_func):
/// Some(result) when `name` is a text-search function we serve,
/// None to let the generic machinery answer (usually 42883).
pub(crate) fn try_eval_ts_func(name: &str, vals: &[Value]) -> Option<Result<Value, ExecError>> {
    // v1.94: the SQL parser's lone-`!` desugar. PG19 keeps `!` as an
    // Op token but defines no such prefix operator, so resolution
    // fails with 42883 naming the operand type (oracle-verified:
    // "operator does not exist: ! tsquery").
    if name == "__prefix_bang" {
        let t = vals
            .first()
            .map(|v| value_type_name(v).into_owned())
            .unwrap_or_default();
        return Some(Err(exec_err(
            "42883",
            format!("operator does not exist: ! {t}"),
        )));
    }
    if !TS_FUNC_NAMES.contains(&name) {
        return None;
    }
    // `length` is claimed only for tsvector operands; the generic
    // string path keeps every other length() call.
    if name == "length" && !matches!(vals.first(), Some(Value::TsVector(_))) {
        return None;
    }
    // All of these are strict in PG.
    if vals.iter().any(|v| matches!(v, Value::Null)) {
        return Some(Ok(Value::Null));
    }
    Some(eval_ts_func(name, vals))
}

fn eval_ts_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    // The hook runs before check_builtin_arity, so guard the arity
    // here (a wrong-arity call must 42883, never panic on indexing).
    let arity_ok = match name {
        "to_tsvector"
        | "to_tsquery"
        | "plainto_tsquery"
        | "phraseto_tsquery"
        | "websearch_to_tsquery" => (1..=2).contains(&vals.len()),
        "ts_rank" | "ts_rank_cd" => (2..=4).contains(&vals.len()),
        "ts_delete" | "ts_filter" | "tsquery_and" | "tsquery_or" => vals.len() == 2,
        "setweight" | "tsquery_phrase" | "__tsquery_phrase" => (2..=3).contains(&vals.len()),
        "get_current_ts_config" => vals.is_empty(),
        _ => vals.len() == 1,
    };
    if !arity_ok {
        return Err(no_ts_func(name, vals));
    }
    match name {
        // The engine's default_text_search_config is english (the
        // 1-arg forms resolve to it); there is no SET support for the
        // GUC yet, so the current config is always english.
        "get_current_ts_config" => Ok(Value::text("english")),
        "to_tsvector" => {
            let (cfg, doc) = match vals {
                [d] => (TsConfig::English, d),
                [c, d] => (ts_config_arg(c)?, d),
                _ => return Err(no_ts_func(name, vals)),
            };
            // A jsonb document walks its string values (the json
            // overload; routed here directly since the variant is
            // unambiguous). The json (text-carried) overload is
            // intercepted in funcs::eval_func, which can see the
            // argument's static type.
            if let Value::Jsonb(t) = doc {
                let tree = super::json::parse_jsonb(t)?;
                let mut strings = Vec::new();
                collect_jsonb_strings(&tree, &mut strings);
                return Ok(Value::tsvector(json_strings_to_tsvector(&strings, cfg)));
            }
            let doc = ts_text_arg(doc).ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsvector(text_to_tsvector(&doc, cfg)))
        }
        "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery" => {
            let (cfg, text) = match vals {
                [t] => (TsConfig::English, t),
                [c, t] => (ts_config_arg(c)?, t),
                _ => return Err(no_ts_func(name, vals)),
            };
            let text = ts_text_arg(text).ok_or_else(|| no_ts_func(name, vals))?;
            let q = match name {
                "to_tsquery" => text_to_tsquery(&text, cfg)?,
                "plainto_tsquery" => plainto_tsquery(&text, cfg),
                "phraseto_tsquery" => phraseto_tsquery(&text, cfg),
                _ => websearch_to_tsquery(&text, cfg),
            };
            Ok(Value::tsquery(q))
        }
        "ts_rank" | "ts_rank_cd" => {
            // Signature shapes: (vec, query), (weights, vec, query),
            // (vec, query, method), (weights, vec, query, method).
            let mut weights = None;
            let mut method: i64 = 0;
            let vec_v: &Value;
            let query_v: &Value;
            match vals.len() {
                2 => {
                    vec_v = &vals[0];
                    query_v = &vals[1];
                }
                3 => {
                    if let Some(w) = ts_float_array_arg(&vals[0])? {
                        weights = Some(w);
                        vec_v = &vals[1];
                        query_v = &vals[2];
                    } else {
                        vec_v = &vals[0];
                        query_v = &vals[1];
                        method = ts_int_arg(&vals[2]).ok_or_else(|| no_ts_func(name, vals))?;
                    }
                }
                4 => {
                    weights = ts_float_array_arg(&vals[0])?;
                    if weights.is_none() {
                        return Err(no_ts_func(name, vals));
                    }
                    vec_v = &vals[1];
                    query_v = &vals[2];
                    method = ts_int_arg(&vals[3]).ok_or_else(|| no_ts_func(name, vals))?;
                }
                _ => return Err(no_ts_func(name, vals)),
            }
            let w = match weights {
                Some((elems, ndim_ok)) => get_weights(&elems, ndim_ok)?,
                None => DEFAULT_WEIGHTS,
            };
            let v = ts_vec_arg(vec_v)?.ok_or_else(|| no_ts_func(name, vals))?;
            let q = ts_query_arg(query_v)?.ok_or_else(|| no_ts_func(name, vals))?;
            let r = if name == "ts_rank" {
                ts_rank(&w, &v, &q, method as i32)
            } else {
                ts_rank_cd(&w, &v, &q, method as i32)
            };
            Ok(Value::Float4(r))
        }
        "ts_delete" => {
            let [vec, arg] = vals else {
                return Err(no_ts_func(name, vals));
            };
            let v = ts_vec_arg(vec)?.ok_or_else(|| no_ts_func(name, vals))?;
            if let Some(lex) = ts_text_arg(arg) {
                return Ok(Value::tsvector(tsvector_delete(&v, &[lex])));
            }
            let arr = ts_text_array_arg(arg)?.ok_or_else(|| no_ts_func(name, vals))?;
            // PG ignores NULL array elements here (they cannot match).
            let lexemes: Vec<String> = arr.into_iter().flatten().collect();
            Ok(Value::tsvector(tsvector_delete(&v, &lexemes)))
        }
        "setweight" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let w = ts_text_arg(&vals[1]).ok_or_else(|| no_ts_func(name, vals))?;
            match vals.len() {
                2 => Ok(Value::tsvector(tsvector_setweight(&v, &w, None)?)),
                3 => {
                    let arr = ts_text_array_arg(&vals[2])?.ok_or_else(|| no_ts_func(name, vals))?;
                    Ok(Value::tsvector(tsvector_setweight(&v, &w, Some(&arr))?))
                }
                _ => Err(no_ts_func(name, vals)),
            }
        }
        "strip" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsvector(tsvector_strip(&v)))
        }
        "length" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::Int(tsvector_length(&v)))
        }
        "numnode" => {
            let q = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let n = q.root.as_ref().map(|r| r.numnode()).unwrap_or(0);
            Ok(Value::Int(n as i64))
        }
        "tsvector_to_array" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let lexemes = tsvector_to_array(&v);
            Ok(Value::Array(Box::new(crate::storage::ArrayVal {
                elem: crate::storage::ArrayElem::Text,
                dims: vec![lexemes.len() as i32],
                lower: vec![1],
                elems: lexemes.into_iter().map(Value::text).collect(),
            })))
        }
        "array_to_tsvector" => {
            let arr = ts_text_array_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsvector(array_to_tsvector(&arr)?))
        }
        "ts_filter" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let arr = ts_text_array_arg(&vals[1])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsvector(tsvector_filter(&v, &arr)?))
        }
        "tsvectorin" => {
            let t = ts_text_arg(&vals[0]).ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsvector(parse_tsvector(&t)?))
        }
        "tsvectorout" => {
            let v = ts_vec_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::text(tsvector_to_text(&v)))
        }
        "tsqueryin" => {
            let t = ts_text_arg(&vals[0]).ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsquery(parse_tsquery(&t)?))
        }
        "tsqueryout" => {
            let q = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::text(tsquery_to_text(&q)))
        }
        "tsquery_phrase" | "__tsquery_phrase" => {
            let a = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let b = ts_query_arg(&vals[1])?.ok_or_else(|| no_ts_func(name, vals))?;
            let dist = match vals.len() {
                2 => 1,
                3 => ts_int_arg(&vals[2]).ok_or_else(|| no_ts_func(name, vals))?,
                _ => return Err(no_ts_func(name, vals)),
            };
            Ok(Value::tsquery(tsquery_phrase(&a, &b, dist)?))
        }
        "tsquery_not" | "__tsquery_not" => {
            let q = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsquery(tsquery_not(&q)))
        }
        "tsquery_and" => {
            let a = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let b = ts_query_arg(&vals[1])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsquery(tsquery_and(&a, &b)))
        }
        "tsquery_or" => {
            let a = ts_query_arg(&vals[0])?.ok_or_else(|| no_ts_func(name, vals))?;
            let b = ts_query_arg(&vals[1])?.ok_or_else(|| no_ts_func(name, vals))?;
            Ok(Value::tsquery(tsquery_or(&a, &b)))
        }
        _ => Err(no_ts_func(name, vals)),
    }
}
