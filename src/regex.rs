//! Pure-std regular expression engine for PostgreSQL-compatible regex
//! functions (v0.19).
//!
//! Supports the POSIX-ish syntax used by PG's regexp_* functions:
//! literals, `.`, `*`, `+`, `?`, `{m,n}`, `(...)` capture groups,
//! `|` alternation, `^`/`$` anchors, `[...]` character classes (with
//! ranges, negation, and `[:alpha:]`-style POSIX classes), `\d` `\D`
//! `\s` `\S` `\w` `\W` shorthands, and `\1`-`\9` backreferences.
//!
//! The matcher is a backtracking VM over a compiled instruction list.
//! Pathological patterns may be slow, but they cannot panic.

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Lit(char),
    Dot,
    Class(CharClass),
    AnchorStart,
    AnchorEnd,
    Seq(Vec<Ast>),
    Alt(Vec<Ast>),
    // v0.31: `lazy` selects non-greedy (lazy) repetition, e.g. `*?`, `+?`,
    // `??`, `{m,n}?` (PostgreSQL ARE semantics; used by SIMILAR TO's
    // SUBSTRING translation).
    Repeat(Box<Ast>, u32, Option<u32>, bool),
    Group(Box<Ast>, usize),
    BackRef(usize),
}

#[derive(Clone, Debug)]
pub struct CharClass {
    neg: bool,
    ranges: Vec<(char, char)>,
    singles: Vec<char>,
}

impl CharClass {
    fn matches(&self, c: char) -> bool {
        let hit =
            self.singles.contains(&c) || self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi);
        if self.neg { !hit } else { hit }
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    groups: usize,
    /// v0.24: expanded (`x`) mode — ignore unescaped whitespace and
    /// `#` comments, like PostgreSQL's ARE `x` flag.
    expanded: bool,
}

pub struct Compiled {
    insns: Vec<Insn>,
    groups: usize,
    ci: bool,
    /// v0.24: newline-sensitive (`n`/`m`) mode — `.` and `[^...]` do not
    /// match `\n`, and `^`/`$` also match at newlines.
    newline_sensitive: bool,
}

/// v0.24: regex compile options for PostgreSQL's `n`/`s`/`x` flags.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegexOptions {
    pub case_insensitive: bool,
    /// `n` (or historical synonym `m`): newline-sensitive matching.
    pub newline_sensitive: bool,
    /// `x`: expanded syntax — ignore whitespace and `#` comments.
    pub expanded: bool,
}

pub struct Captures {
    pub groups: Vec<Option<(usize, usize)>>,
}

pub fn compile(pattern: &str, case_insensitive: bool) -> Result<Compiled, String> {
    compile_opts(
        pattern,
        RegexOptions {
            case_insensitive,
            ..RegexOptions::default()
        },
    )
}

/// v0.24: compile with explicit flag options.
pub fn compile_opts(pattern: &str, opts: RegexOptions) -> Result<Compiled, String> {
    let mut p = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        groups: 0,
        expanded: opts.expanded,
    };
    let ast = p.parse_alt()?;
    if p.pos != p.chars.len() {
        return Err(format!("unexpected '{}' in pattern", p.chars[p.pos]));
    }
    let mut c = Compiler {
        insns: Vec::new(),
        ci: opts.case_insensitive,
        lazy_ctx: false,
    };
    c.compile_ast(&ast);
    c.emit(Insn::Match);
    Ok(Compiled {
        insns: c.insns,
        groups: p.groups,
        ci: opts.case_insensitive,
        newline_sensitive: opts.newline_sensitive,
    })
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// v0.24: in expanded (`x`) mode, skip whitespace and `#`-to-newline
    /// comments. Only called at atom boundaries — never inside bracket
    /// expressions, escapes, or repetition counts, where whitespace stays
    /// significant.
    fn skip_ignored(&mut self) {
        if !self.expanded {
            return;
        }
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.next();
                }
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.next();
                    }
                }
                _ => break,
            }
        }
    }

    fn parse_alt(&mut self) -> Result<Ast, String> {
        let mut alts = vec![self.parse_seq()?];
        while self.peek() == Some('|') {
            self.next();
            alts.push(self.parse_seq()?);
        }
        if alts.len() == 1 {
            Ok(alts.pop().unwrap())
        } else {
            Ok(Ast::Alt(alts))
        }
    }

    fn parse_seq(&mut self) -> Result<Ast, String> {
        let mut seq = Vec::new();
        loop {
            self.skip_ignored();
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => seq.push(self.parse_repeat()?),
            }
        }
        match seq.len() {
            0 => Ok(Ast::Empty),
            1 => Ok(seq.pop().unwrap()),
            _ => Ok(Ast::Seq(seq)),
        }
    }

    fn parse_repeat(&mut self) -> Result<Ast, String> {
        let atom = self.parse_atom()?;
        match self.peek() {
            Some('*') => {
                self.next();
                Ok(Ast::Repeat(Box::new(atom), 0, None, self.eat_lazy()))
            }
            Some('+') => {
                self.next();
                Ok(Ast::Repeat(Box::new(atom), 1, None, self.eat_lazy()))
            }
            Some('?') => {
                self.next();
                Ok(Ast::Repeat(Box::new(atom), 0, Some(1), self.eat_lazy()))
            }
            Some('{') => {
                self.next();
                let min = self.parse_num()?;
                let (min, max) = if self.peek() == Some(',') {
                    self.next();
                    if self.peek() == Some('}') {
                        (min, None)
                    } else {
                        let max = self.parse_num()?;
                        (min, Some(max))
                    }
                } else {
                    (min, Some(min))
                };
                if self.next() != Some('}') {
                    return Err("expected '}' in pattern".to_string());
                }
                if let Some(mx) = max {
                    if mx < min {
                        return Err("invalid repetition range".to_string());
                    }
                }
                Ok(Ast::Repeat(Box::new(atom), min, max, self.eat_lazy()))
            }
            _ => Ok(atom),
        }
    }

    /// v0.31: consume a trailing '?' that makes a quantifier lazy.
    fn eat_lazy(&mut self) -> bool {
        if self.peek() == Some('?') {
            self.next();
            true
        } else {
            false
        }
    }

    fn parse_num(&mut self) -> Result<u32, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.next();
        }
        if self.pos == start {
            return Err("expected number in pattern".to_string());
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| "bad number in pattern".to_string())
    }

    fn parse_atom(&mut self) -> Result<Ast, String> {
        self.skip_ignored();
        match self.next() {
            None => Err("unexpected end of pattern".to_string()),
            Some('(') => {
                if self.peek() == Some('?') && self.chars.get(self.pos + 1) == Some(&':') {
                    self.pos += 2;
                    let inner = self.parse_alt()?;
                    if self.next() != Some(')') {
                        return Err("unclosed '(' in pattern".to_string());
                    }
                    Ok(inner)
                } else {
                    self.groups += 1;
                    let idx = self.groups;
                    let inner = self.parse_alt()?;
                    if self.next() != Some(')') {
                        return Err("unclosed '(' in pattern".to_string());
                    }
                    Ok(Ast::Group(Box::new(inner), idx))
                }
            }
            Some(')') => Err("unmatched ')' in pattern".to_string()),
            Some('^') => Ok(Ast::AnchorStart),
            Some('$') => Ok(Ast::AnchorEnd),
            Some('.') => Ok(Ast::Dot),
            Some('[') => self.parse_class(),
            Some('\\') => self.parse_escape(),
            Some(c) => Ok(Ast::Lit(c)),
        }
    }

    fn parse_escape(&mut self) -> Result<Ast, String> {
        match self.next() {
            None => Err("trailing backslash in pattern".to_string()),
            Some('d') => Ok(Ast::Class(CharClass {
                neg: false,
                ranges: vec![('0', '9')],
                singles: vec![],
            })),
            Some('D') => Ok(Ast::Class(CharClass {
                neg: true,
                ranges: vec![('0', '9')],
                singles: vec![],
            })),
            Some('s') => Ok(Ast::Class(CharClass {
                neg: false,
                ranges: vec![],
                singles: vec![' ', '\t', '\n', '\r', '\x0c', '\x0b'],
            })),
            Some('S') => Ok(Ast::Class(CharClass {
                neg: true,
                ranges: vec![],
                singles: vec![' ', '\t', '\n', '\r', '\x0c', '\x0b'],
            })),
            Some('w') => Ok(Ast::Class(CharClass {
                neg: false,
                ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9')],
                singles: vec!['_'],
            })),
            Some('W') => Ok(Ast::Class(CharClass {
                neg: true,
                ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9')],
                singles: vec!['_'],
            })),
            Some(c) if c.is_ascii_digit() && c != '0' => {
                Ok(Ast::BackRef(c.to_digit(10).unwrap() as usize))
            }
            Some(c) => Ok(Ast::Lit(c)),
        }
    }

    fn parse_class(&mut self) -> Result<Ast, String> {
        let neg = if self.peek() == Some('^') {
            self.next();
            true
        } else {
            false
        };
        let mut ranges = Vec::new();
        let mut singles = Vec::new();
        let mut first = true;
        loop {
            match self.next() {
                None => return Err("unclosed '[' in pattern".to_string()),
                Some(']') if !first => break,
                Some('[') if self.peek() == Some(':') => {
                    self.next();
                    let start = self.pos;
                    while !matches!(self.peek(), Some(':') | None) {
                        self.next();
                    }
                    let name: String = self.chars[start..self.pos].iter().collect();
                    if self.next() != Some(':') || self.next() != Some(']') {
                        return Err("bad POSIX class".to_string());
                    }
                    let (mut r, mut s) = posix_class(&name)?;
                    ranges.append(&mut r);
                    singles.append(&mut s);
                }
                Some('\\') => match self.parse_escape()? {
                    Ast::Class(cc) => {
                        ranges.extend(cc.ranges);
                        singles.extend(cc.singles);
                    }
                    Ast::Lit(c) => singles.push(c),
                    _ => return Err("bad escape in class".to_string()),
                },
                Some(c) => {
                    if self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']') {
                        self.next();
                        match self.next() {
                            Some(']') => {
                                singles.push(c);
                                singles.push('-');
                                break;
                            }
                            Some(end) => {
                                if end < c {
                                    return Err("invalid range in class".to_string());
                                }
                                ranges.push((c, end));
                            }
                            None => return Err("unclosed '[' in pattern".to_string()),
                        }
                    } else {
                        singles.push(c);
                    }
                }
            }
            first = false;
        }
        Ok(Ast::Class(CharClass {
            neg,
            ranges,
            singles,
        }))
    }
}

fn posix_class(name: &str) -> Result<(Vec<(char, char)>, Vec<char>), String> {
    match name {
        "alpha" => Ok((vec![('a', 'z'), ('A', 'Z')], vec![])),
        "digit" => Ok((vec![('0', '9')], vec![])),
        "alnum" => Ok((vec![('a', 'z'), ('A', 'Z'), ('0', '9')], vec![])),
        "space" => Ok((vec![], vec![' ', '\t', '\n', '\r', '\x0c', '\x0b'])),
        "upper" => Ok((vec![('A', 'Z')], vec![])),
        "lower" => Ok((vec![('a', 'z')], vec![])),
        "xdigit" => Ok((vec![('0', '9'), ('a', 'f'), ('A', 'F')], vec![])),
        "punct" => Ok((
            vec![],
            "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~".chars().collect(),
        )),
        "graph" => Ok((vec![('!', '~')], vec![])),
        "print" => Ok((vec![(' ', '~')], vec![])),
        "cntrl" => Ok((vec![('\x00', '\x1f'), ('\x7f', '\x7f')], vec![])),
        _ => Err(format!("unknown POSIX class '{}'", name)),
    }
}

#[derive(Clone, Debug)]
enum Insn {
    Lit(char),
    Dot,
    Class(CharClass),
    AnchorStart,
    AnchorEnd,
    Jump(usize),
    Split(usize, usize),
    SaveStart(usize),
    SaveEnd(usize),
    BackRef(usize),
    Match,
}

struct Compiler {
    insns: Vec<Insn>,
    ci: bool,
    /// v0.31: when true, all nested quantifiers compile as lazy
    /// (non-greedy). Set while compiling the body of a lazy `Repeat`,
    /// matching PostgreSQL's semantics where `{1,1}?` on a group makes the
    /// whole group (including inner quantifiers) non-greedy.
    lazy_ctx: bool,
}

impl Compiler {
    fn emit(&mut self, insn: Insn) -> usize {
        self.insns.push(insn);
        self.insns.len() - 1
    }

    fn compile_ast(&mut self, ast: &Ast) {
        match ast {
            Ast::Empty => {}
            Ast::Lit(c) => {
                let c = if self.ci {
                    c.to_lowercase().next().unwrap_or(*c)
                } else {
                    *c
                };
                self.emit(Insn::Lit(c));
            }
            Ast::Dot => {
                self.emit(Insn::Dot);
            }
            Ast::Class(cc) => {
                self.emit(Insn::Class(cc.clone()));
            }
            Ast::AnchorStart => {
                self.emit(Insn::AnchorStart);
            }
            Ast::AnchorEnd => {
                self.emit(Insn::AnchorEnd);
            }
            Ast::Seq(v) => {
                for a in v {
                    self.compile_ast(a);
                }
            }
            Ast::Alt(v) => {
                // v0.31: fix — the last alternative must NOT be wrapped in a
                // Split whose "else" branch falls through to `end`; if the
                // last branch fails, the whole alternation must fail (and
                // backtrack), not succeed with an empty match.
                let mut jumps = Vec::new();
                let n = v.len();
                for (idx, a) in v.iter().enumerate() {
                    if idx + 1 < n {
                        let split = self.emit(Insn::Split(0, 0));
                        self.compile_ast(a);
                        jumps.push(self.emit(Insn::Jump(0)));
                        let here = self.insns.len();
                        if let Insn::Split(ref mut x, ref mut y) = self.insns[split] {
                            *x = split + 1;
                            *y = here;
                        }
                    } else {
                        // Last alternative: compile directly; failure
                        // backtracks naturally.
                        self.compile_ast(a);
                        jumps.push(self.emit(Insn::Jump(0)));
                    }
                }
                let here = self.insns.len();
                for j in jumps {
                    if let Insn::Jump(ref mut t) = self.insns[j] {
                        *t = here;
                    }
                }
            }
            Ast::Repeat(inner, min, max, lazy) => {
                // v0.31: a lazy quantifier makes its entire body non-greedy,
                // including nested quantifiers (PostgreSQL `{1,1}?` semantics
                // for SIMILAR TO's SUBSTRING part1).
                let eff_lazy = *lazy || self.lazy_ctx;
                let save_lazy = self.lazy_ctx;
                self.lazy_ctx = eff_lazy;
                for _ in 0..*min {
                    self.compile_ast(inner);
                }
                // v0.31: lazy repetition swaps the Split targets so the
                // "skip the body" branch is preferred (non-greedy).
                match max {
                    Some(mx) => {
                        for _ in 0..(mx - min) {
                            let split = self.emit(Insn::Split(0, 0));
                            self.compile_ast(inner);
                            let here = self.insns.len();
                            if let Insn::Split(ref mut x, ref mut y) = self.insns[split] {
                                if eff_lazy {
                                    *x = here;
                                    *y = split + 1;
                                } else {
                                    *x = split + 1;
                                    *y = here;
                                }
                            }
                        }
                    }
                    None => {
                        let split = self.emit(Insn::Split(0, 0));
                        self.compile_ast(inner);
                        self.emit(Insn::Jump(split));
                        let here = self.insns.len();
                        if let Insn::Split(ref mut x, ref mut y) = self.insns[split] {
                            if eff_lazy {
                                *x = here;
                                *y = split + 1;
                            } else {
                                *x = split + 1;
                                *y = here;
                            }
                        }
                    }
                }
                self.lazy_ctx = save_lazy;
            }
            Ast::Group(inner, idx) => {
                self.emit(Insn::SaveStart(*idx));
                self.compile_ast(inner);
                self.emit(Insn::SaveEnd(*idx));
            }
            Ast::BackRef(idx) => {
                self.emit(Insn::BackRef(*idx));
            }
        }
    }
}

impl Compiled {
    pub fn group_count(&self) -> usize {
        self.groups
    }

    /// Leftmost match at or after `start` (char indices).
    pub fn find_at(&self, s: &[char], start: usize) -> Option<(usize, usize, Captures)> {
        for st in start..=s.len() {
            let mut caps = vec![None; self.groups + 1];
            caps[0] = Some((st, st));
            let mut stack = Vec::new();
            // Step budget: bounds the backtracking VM so a pathological
            // pattern (e.g. nested quantifiers over nullable bodies) degrades
            // to "no match" instead of hanging the backend. Scales with input
            // and program size; generous enough for legitimate matches.
            let mut budget: u64 = (s.len() as u64)
                .saturating_mul(self.insns.len() as u64)
                .saturating_mul(100)
                .saturating_add(100_000);
            if self.run(s, st, &mut caps, &mut stack, &mut budget) {
                let (ms, me) = caps[0].unwrap();
                return Some((ms, me, Captures { groups: caps }));
            }
        }
        None
    }

    pub fn is_match(&self, s: &[char]) -> bool {
        self.find_at(s, 0).is_some()
    }

    fn run(
        &self,
        s: &[char],
        mut si: usize,
        caps: &mut Vec<Option<(usize, usize)>>,
        stack: &mut Vec<(usize, usize, Vec<Option<(usize, usize)>>)>,
        budget: &mut u64,
    ) -> bool {
        let mut pc = 0;
        loop {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            if pc >= self.insns.len() {
                return false;
            }
            match &self.insns[pc] {
                Insn::Lit(c) => {
                    if si < s.len() && chars_eq(s[si], *c, self.ci) {
                        si += 1;
                        pc += 1;
                    } else if !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::Dot => {
                    // v0.24: with `n` (newline-sensitive), `.` does not
                    // match `\n`; with `s` (the default) it does.
                    if si < s.len() && !(self.newline_sensitive && s[si] == '\n') {
                        si += 1;
                        pc += 1;
                    } else if !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::Class(cc) => {
                    // v0.24: with `n`, a negated class does not match `\n`
                    // (PG newline-sensitive semantics).
                    if si < s.len()
                        && cc.matches(s[si])
                        && !(self.newline_sensitive && cc.neg && s[si] == '\n')
                    {
                        si += 1;
                        pc += 1;
                    } else if !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::AnchorStart => {
                    // v0.24: with `n`, `^` also matches after a newline.
                    if si == 0 || (self.newline_sensitive && si > 0 && s[si - 1] == '\n') {
                        pc += 1;
                    } else if !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::AnchorEnd => {
                    // v0.24: with `n`, `$` also matches before a newline.
                    if si == s.len() || (self.newline_sensitive && si < s.len() && s[si] == '\n') {
                        pc += 1;
                    } else if !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::Jump(t) => {
                    pc = *t;
                }
                Insn::Split(a, b) => {
                    stack.push((*b, si, caps.clone()));
                    pc = *a;
                }
                Insn::SaveStart(g) => {
                    if *g < caps.len() {
                        caps[*g] = Some((si, si));
                    }
                    pc += 1;
                }
                Insn::SaveEnd(g) => {
                    if *g < caps.len() {
                        if let Some((st, _)) = caps[*g] {
                            caps[*g] = Some((st, si));
                        }
                    }
                    pc += 1;
                }
                Insn::BackRef(g) => {
                    let matched = match caps.get(*g).copied().flatten() {
                        Some((gs, ge)) => {
                            let len = ge - gs;
                            if si + len <= s.len() {
                                let a = &s[gs..ge];
                                let b = &s[si..si + len];
                                let eq = if self.ci {
                                    a.iter()
                                        .zip(b.iter())
                                        .all(|(x, y)| x.to_lowercase().eq(y.to_lowercase()))
                                } else {
                                    a == b
                                };
                                if eq {
                                    si += len;
                                    pc += 1;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        }
                        None => false,
                    };
                    if !matched && !backtrack(&mut pc, &mut si, caps, stack) {
                        return false;
                    }
                }
                Insn::Match => {
                    if let Some((st, _)) = caps[0] {
                        caps[0] = Some((st, si));
                    }
                    return true;
                }
            }
        }
    }
}

fn backtrack(
    pc: &mut usize,
    si: &mut usize,
    caps: &mut Vec<Option<(usize, usize)>>,
    stack: &mut Vec<(usize, usize, Vec<Option<(usize, usize)>>)>,
) -> bool {
    if let Some((bpc, bsi, bcaps)) = stack.pop() {
        *pc = bpc;
        *si = bsi;
        *caps = bcaps;
        true
    } else {
        false
    }
}

fn chars_eq(a: char, b: char, ci: bool) -> bool {
    if ci {
        a.to_lowercase().eq(b.to_lowercase())
    } else {
        a == b
    }
}
