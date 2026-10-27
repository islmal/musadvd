//! DVD virtual-machine command compiler: the command subset of dvdauthor's `<pre>`, `<post>`,
//! `<button>` and `<fpc>` strings (see `author.rs`) compiled to 8-byte VM instructions.
//!
//! A port of dvdauthor's `dvdcompile.c` (+ the grammar of `dvdvmy.y`, `dvdvml.l`) restricted to what
//! this program emits: register assignment from a constant or register, `audio`/`subtitle`
//! assignment, `if (<reg|num> <cmp> <reg|num>) <statement> [else <statement>]`, `jump`, `call`,
//! `exit`, `resume`. Anything else is a [`VmError::Unsupported`]. The output has to be byte-identical
//! to the original (including `vm_optimize`), so the structure follows the C code.
//!
//! GPL-2.0 (derived from dvdauthor).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmError {
    /// Syntax error or syntax outside the supported subset.
    Parse(String),
    /// Semantic error dvdauthor reports while compiling (`ERR:  ...`).
    Compile(String),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::Parse(m) | VmError::Compile(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for VmError {}

fn perr<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError::Parse(msg.into()))
}

fn cerr<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError::Compile(msg.into()))
}

// ---------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------

/// Operand: a register (`reg - 256`, so negative like in dvdauthor) or a literal (>= 0).
pub type Val = i32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Gte,
    Gt,
    Lte,
    Lt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `i1` = destination register number as in dvdauthor (GPRM 0..15, S registers `0x80 + n`,
    /// counter mode `0x20 + n`), `val` the assigned operand.
    Set { reg: i32, val: Val },
    If { l: Val, cmp: Cmp, r: Val, then: Vec<Stmt>, els: Option<Vec<Stmt>> },
    Exit,
    /// `resume;` and the `jump ... top/next ...` forms: `LinkXXX` with sub-command `i1`.
    Link(u8),
    Jump { ts: i32, menu: i32, extra: i32 },
    Call { ts: i32, menu: i32, chapter: i32, resume_cell: i32 },
}

pub type Program = Vec<Stmt>;

// ---------------------------------------------------------------------------
// Lexer / parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Num(u32),
    G(u32),
    S(u32),
    Id(String),
    Semi,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Set,
    Eq,
    Ne,
    Ge,
    Gt,
    Le,
    Lt,
}

fn lex(src: &str) -> Result<Vec<Tok>, VmError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = src[i + 2..].find("*/").ok_or_else(|| VmError::Parse("EOF in comment".into()))?;
                i += end + 4;
            }
            b';' => {
                out.push(Tok::Semi);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'{' => {
                out.push(Tok::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Tok::RBrace);
                i += 1;
            }
            b'=' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Eq);
                i += 2;
            }
            b'=' => {
                out.push(Tok::Set);
                i += 1;
            }
            b'!' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Ne);
                i += 2;
            }
            b'>' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Ge);
                i += 2;
            }
            b'>' => {
                out.push(Tok::Gt);
                i += 1;
            }
            b'<' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Le);
                i += 2;
            }
            b'<' => {
                out.push(Tok::Lt);
                i += 1;
            }
            b'0'..=b'9' => {
                let start = i;
                let hex = c == b'0' && b.get(i + 1) == Some(&b'x');
                if hex {
                    i += 2;
                    while i < b.len() && b[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                } else {
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let mut v = if hex { u32::from_str_radix(&src[start + 2..i], 16) } else { src[start..i].parse::<u32>() }
                    .map_err(|_| VmError::Parse(format!("bad number '{}'", &src[start..i])))?;
                // `<n>k`: multiples of 1024 (button numbers).
                if !hex && b.get(i) == Some(&b'k') {
                    i += 1;
                    if v >= 64 {
                        return perr(format!("ERR:  Integers must be between 0 and 65535, inclusive ({}k)", v));
                    }
                    v *= 1024;
                } else if v >= 65536 {
                    return perr(format!("ERR:  Integers must be between 0 and 65535, inclusive ({v})"));
                }
                out.push(Tok::Num(v));
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let w = &src[start..i];
                let reg = |p: u8| w.len() > 1 && w.as_bytes()[0] == p && w[1..].bytes().all(|c| c.is_ascii_digit());
                if reg(b'g') {
                    let n: u32 = w[1..].parse().map_err(|_| VmError::Parse(format!("bad register {w}")))?;
                    if n >= 13 {
                        return perr(format!("ERR:  Can only access g0-g12 ({w})"));
                    }
                    out.push(Tok::G(n));
                } else if reg(b's') {
                    let n: u32 = w[1..].parse().map_err(|_| VmError::Parse(format!("bad register {w}")))?;
                    if n > 23 {
                        return perr(format!("ERR:  Can only access s0-s23 ({w})"));
                    }
                    out.push(Tok::S(n));
                } else {
                    out.push(Tok::Id(w.to_ascii_lowercase()));
                }
            }
            _ => return perr(format!("unexpected character '{}'", c as char)),
        }
    }
    Ok(out)
}

struct Parser {
    t: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        self.i += 1;
        t
    }

    fn word(&mut self, w: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Id(x)) if x == w) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok) -> Result<(), VmError> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            x => perr(format!("expected {t:?}, found {x:?}")),
        }
    }

    fn num(&mut self, what: &str, lo: u32, hi: u32) -> Result<u32, VmError> {
        match self.next() {
            Some(Tok::Num(n)) if (lo..=hi).contains(&n) => Ok(n),
            Some(Tok::Num(_)) => perr(format!("{what} number out of range")),
            x => perr(format!("expected {what} number, found {x:?}")),
        }
    }

    fn statements(&mut self, until_brace: bool) -> Result<Vec<Stmt>, VmError> {
        let mut v = Vec::new();
        loop {
            match self.peek() {
                None if !until_brace => return Ok(v),
                Some(Tok::RBrace) if until_brace => return Ok(v),
                None => return perr("missing }"),
                _ => v.extend(self.statement()?),
            }
        }
    }

    /// One `statement`; a block yields its statements.
    fn statement(&mut self) -> Result<Vec<Stmt>, VmError> {
        match self.peek().cloned() {
            Some(Tok::LBrace) => {
                self.i += 1;
                let v = self.statements(true)?;
                self.expect(Tok::RBrace)?;
                Ok(v)
            }
            Some(Tok::Id(w)) => match w.as_str() {
                "exit" => {
                    self.i += 1;
                    self.expect(Tok::Semi)?;
                    Ok(vec![Stmt::Exit])
                }
                "resume" => {
                    self.i += 1;
                    self.expect(Tok::Semi)?;
                    Ok(vec![Stmt::Link(16)])
                }
                "jump" => {
                    self.i += 1;
                    self.jump()
                }
                "call" => {
                    self.i += 1;
                    self.call()
                }
                "if" => {
                    self.i += 1;
                    self.if_stmt()
                }
                "audio" | "subtitle" => self.set(),
                _ => perr(format!("unsupported statement '{w}'")),
            },
            Some(Tok::G(_) | Tok::S(_)) => self.set(),
            x => perr(format!("unexpected token {x:?}")),
        }
    }

    /// `reg`: GPRM n, S register `0x80 + n`, `audio` 0x81, `subtitle` 0x82.
    fn reg(&mut self) -> Option<i32> {
        match self.peek()? {
            Tok::G(n) => {
                let r = *n as i32;
                self.i += 1;
                Some(r)
            }
            Tok::S(n) => {
                let r = *n as i32 + 0x80;
                self.i += 1;
                Some(r)
            }
            Tok::Id(w) if w == "audio" => {
                self.i += 1;
                Some(0x81)
            }
            Tok::Id(w) if w == "subtitle" => {
                self.i += 1;
                Some(0x82)
            }
            _ => None,
        }
    }

    /// `regornum`: registers become `reg - 256`.
    fn val(&mut self) -> Result<Val, VmError> {
        if let Some(r) = self.reg() {
            return Ok(r - 256);
        }
        match self.next() {
            Some(Tok::Num(n)) => Ok(n as i32),
            x => perr(format!("expected register or number, found {x:?}")),
        }
    }

    fn set(&mut self) -> Result<Vec<Stmt>, VmError> {
        let reg = self.reg().ok_or_else(|| VmError::Parse("expected register".into()))?;
        self.expect(Tok::Set)?;
        let val = self.val()?;
        if !matches!(self.peek(), Some(Tok::Semi)) {
            return perr("only plain register/number assignments are supported");
        }
        self.expect(Tok::Semi)?;
        Ok(vec![Stmt::Set { reg, val }])
    }

    fn if_stmt(&mut self) -> Result<Vec<Stmt>, VmError> {
        self.expect(Tok::LParen)?;
        let l = self.val()?;
        let cmp = match self.next() {
            Some(Tok::Eq) => Cmp::Eq,
            Some(Tok::Ne) => Cmp::Ne,
            Some(Tok::Ge) => Cmp::Gte,
            Some(Tok::Gt) => Cmp::Gt,
            Some(Tok::Le) => Cmp::Lte,
            Some(Tok::Lt) => Cmp::Lt,
            Some(Tok::Id(w)) => match w.as_str() {
                "eq" => Cmp::Eq,
                "ne" => Cmp::Ne,
                "ge" => Cmp::Gte,
                "gt" => Cmp::Gt,
                "le" => Cmp::Lte,
                "lt" => Cmp::Lt,
                _ => return perr(format!("unsupported condition operator '{w}'")),
            },
            x => return perr(format!("expected comparison, found {x:?}")),
        };
        let r = self.val()?;
        self.expect(Tok::RParen)?;
        let then = self.statement()?;
        let els = if self.word("else") { Some(self.statement()?) } else { None };
        Ok(vec![Stmt::If { l, cmp, r, then, els }])
    }

    fn jtsl(&mut self) -> Result<i32, VmError> {
        if self.word("titleset") {
            Ok(self.num("titleset", 1, 99)? as i32 + 1)
        } else if self.word("vmgm") {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn jtml(&mut self) -> Result<i32, VmError> {
        if self.word("menu") {
            if self.word("entry") {
                for (name, v) in [("title", 122), ("root", 123), ("subtitle", 124), ("audio", 125), ("angle", 126), ("ptt", 127)] {
                    if self.word(name) {
                        return Ok(v);
                    }
                }
                return perr("unknown menu entry");
            }
            if matches!(self.peek(), Some(Tok::Num(_))) {
                return Ok(self.num("menu", 1, 99)? as i32);
            }
            return Ok(120);
        }
        if self.word("fpc") {
            return Ok(121);
        }
        if self.word("title") {
            return Ok(self.num("title", 1, 99)? as i32 | 128);
        }
        Ok(0)
    }

    fn jump(&mut self) -> Result<Vec<Stmt>, VmError> {
        // The `pgc`/`program`/`cell` forms (`jump cell N;` is the one we emit).
        let w = match self.peek() {
            Some(Tok::Id(w)) => w.clone(),
            _ => String::new(),
        };
        let link = |s: &mut Self, v: u8| -> Result<Vec<Stmt>, VmError> {
            s.expect(Tok::Semi)?;
            Ok(vec![Stmt::Link(v)])
        };
        match w.as_str() {
            "pgc" | "program" | "cell" => {
                self.i += 1;
                let kind = match w.as_str() {
                    "pgc" => 0,
                    "program" => 2,
                    _ => 3,
                };
                if self.word("top") {
                    return link(self, [9, 5, 1][[0, 2, 3].iter().position(|&k| k == kind).unwrap()]);
                }
                if kind == 0 && self.word("tail") {
                    return link(self, 13);
                }
                let n = self.num(&w, 1, 65535)? as i32;
                self.expect(Tok::Semi)?;
                return Ok(vec![Stmt::Jump { ts: 0, menu: 0, extra: kind * 65536 + n }]);
            }
            "next" | "prev" => {
                self.i += 1;
                let up = w == "next";
                let what = match self.next() {
                    Some(Tok::Id(x)) => x,
                    x => return perr(format!("unexpected {x:?}")),
                };
                let v = match (what.as_str(), up) {
                    ("cell", true) => 2,
                    ("cell", false) => 3,
                    ("program", true) => 6,
                    ("program", false) => 7,
                    ("pgc", true) => 10,
                    ("pgc", false) => 11,
                    _ => return perr("unsupported jump"),
                };
                return link(self, v);
            }
            "up" => {
                self.i += 1;
                if !self.word("pgc") {
                    return perr("unsupported jump");
                }
                return link(self, 12);
            }
            _ => {}
        }
        let ts = self.jtsl()?;
        let menu = self.jtml()?;
        let chapter = if self.word("chapter") { self.num("chapter", 1, 65535)? as i32 } else { 0 };
        self.expect(Tok::Semi)?;
        Ok(vec![Stmt::Jump { ts, menu, extra: 65536 + chapter }])
    }

    fn call(&mut self) -> Result<Vec<Stmt>, VmError> {
        let ts = self.jtsl()?;
        let menu = self.jtml()?;
        let chapter = if self.word("chapter") { self.num("chapter", 1, 65535)? as i32 } else { 0 };
        let resume_cell = if self.word("resume") { self.num("resume cell", 1, 65535)? as i32 } else { 0 };
        self.expect(Tok::Semi)?;
        Ok(vec![Stmt::Call { ts, menu, chapter, resume_cell }])
    }
}

/// Parses a command string. An empty string is an empty program (dvdauthor's `VM_NOP`).
pub fn parse(src: &str) -> Result<Program, VmError> {
    let mut p = Parser { t: lex(src)?, i: 0 };
    let v = p.statements(false)?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

/// Where a command runs (`vtypes` in dvdauthor; FPC is compiled as `Vmgm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Vts,
    Vtsm,
    Vmgm,
}

/// What the compiler needs to know about the PGC being compiled and its surroundings
/// (`ws`, `curgroup`, `curpgc` of the C code).
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Number of PGCs and their `entries` bit masks in the current menu group (`None`: no menus).
    pub group_entries: Option<Vec<u32>>,
    /// Entry masks of the PGCs of every menu group of the domain (for `call menu entry`).
    pub all_menu_entries: Vec<u32>,
    /// Current PGC: number of chapters, programs, cells. `None` in the FPC.
    pub pgc: Option<[i32; 3]>,
    /// Chapter counts of the titles of the titleset (`ws->titles`); `None` for the VMGM.
    pub titles: Option<Vec<i32>>,
}

fn write8(code: &mut Vec<u8>, at: usize, d: [u8; 8]) {
    if code.len() < at + 8 {
        code.resize(at + 8, 0);
    }
    code[at..at + 8].copy_from_slice(&d);
}

fn negate_compare(op: u8) -> u8 {
    op ^ 1 ^ ((op & 4) >> 1)
}

fn swap_compare(op: u8) -> u8 {
    if op < 4 {
        op
    } else {
        op ^ 3
    }
}

fn nexttarget(t: i32) -> Result<i32, VmError> {
    // `allowallreg` is off: only g13..g15 are reserved for temporaries.
    if t < 13 {
        return Ok(13);
    }
    if t + 1 < 16 {
        return Ok(t + 1);
    }
    cerr("ERR:  Expression is too complicated, ran out of registers")
}

fn nextval(t: i32) -> Result<i32, VmError> {
    if t < -128 {
        Ok(nexttarget(t + 256)? - 256)
    } else {
        Ok(nexttarget(-1)? - 256)
    }
}

/// `compileop` for plain assignment (`VM_VAL`): `target = val`.
fn compile_assign(code: &mut Vec<u8>, buf: usize, target: i32, val: Val) -> usize {
    if target == val + 256 {
        return buf; // setting a register to itself
    }
    let b0 = if val >= 0 { 0x70 } else { 0x60 } | 1;
    write8(code, buf, [b0, 0, 0, target as u8, if val >= 0 { (val >> 8) as u8 } else { 0 }, val as u8, 0, 0]);
    buf + 8
}

struct Compiler<'a> {
    code: &'a mut Vec<u8>,
    obuf: usize,
    ctx: &'a Context,
    domain: Domain,
}

impl Compiler<'_> {
    fn line(&self, pos: usize) -> u8 {
        ((pos - self.obuf) / 8 + 1) as u8
    }

    /// Compiles one comparison; branches to `iftrue` / `iffalse` (byte offsets in `code`).
    fn compilebool(&mut self, mut buf: usize, l: Val, cmp: Cmp, r: Val, mut iftrue: usize, mut iffalse: usize) -> Result<usize, VmError> {
        let mut op = match cmp {
            Cmp::Eq => 2,
            Cmp::Ne => 3,
            Cmp::Gte => 4,
            Cmp::Gt => 5,
            Cmp::Lte => 6,
            Cmp::Lt => 7,
        };
        let mut r1 = l;
        let mut r2 = r;
        if r1 >= 0 && r2 >= 0 {
            // Both literal: dvdauthor first loads the second into a temporary register.
            r2 = nextval(r1)?;
            buf = compile_assign(self.code, buf, r2 & 15, r);
        }
        if r1 >= 0 {
            std::mem::swap(&mut r1, &mut r2);
            op = swap_compare(op);
        }
        if iffalse > iftrue {
            std::mem::swap(&mut iftrue, &mut iffalse);
            op = negate_compare(op);
        }
        let mut ins = if r2 >= 0 {
            [0x00, 0x81 | (op << 4), 0x00, r1 as u8, (r2 >> 8) as u8, r2 as u8, 0x00, 0x00]
        } else {
            [0x00, 0x01 | (op << 4), 0x00, r1 as u8, 0x00, r2 as u8, 0x00, 0x00]
        };
        ins[7] = self.line(iftrue);
        write8(self.code, buf, ins);
        buf += 8;
        if iffalse > buf {
            let mut g = [0x00, 0x01, 0, 0, 0, 0, 0, 0];
            g[7] = self.line(iffalse);
            write8(self.code, buf, g);
            buf += 8;
        }
        Ok(buf)
    }

    fn compile(&mut self, mut buf: usize, stmts: &[Stmt]) -> Result<usize, VmError> {
        let mut lastif = false;
        for cs in stmts {
            lastif = false;
            match cs {
                Stmt::Set { reg, val } => buf = self.set(buf, *reg, *val)?,
                Stmt::If { l, cmp, r, then, els } => {
                    let mut iftrue = buf + 8;
                    let mut iffalse = buf + 16;
                    let mut end = buf + 16;
                    loop {
                        let mut lp = self.compile(iftrue, then)?;
                        let e;
                        if let Some(els) = els {
                            e = self.compile(lp + 8, els)?;
                            let mut g = [0x00, 0x01, 0, 0, 0, 0, 0, 0];
                            g[7] = self.line(e);
                            write8(self.code, lp, g);
                            lp += 8;
                        } else {
                            e = lp;
                        }
                        let ib = self.compilebool(buf, *l, *cmp, *r, iftrue, iffalse)?;
                        if ib == iftrue && lp == iffalse {
                            break;
                        }
                        iftrue = ib;
                        iffalse = lp;
                        end = e;
                    }
                    buf = end;
                    lastif = true;
                }
                Stmt::Exit => {
                    write8(self.code, buf, [0x30, 0x01, 0, 0, 0, 0, 0, 0]);
                    buf += 8;
                }
                Stmt::Link(v) => {
                    write8(self.code, buf, [0x20, 0x01, 0, 0, 0, 0, 0, *v]);
                    buf += 8;
                }
                Stmt::Jump { ts, menu, extra } => buf = self.jump(buf, *ts, *menu, *extra)?,
                Stmt::Call { ts, menu, chapter, resume_cell } => buf = self.call(buf, *ts, *menu, *chapter, *resume_cell)?,
            }
        }
        if lastif {
            write8(self.code, buf, [0; 8]); // NOP: target for the last branch
            buf += 8;
        }
        Ok(buf)
    }

    fn set(&mut self, buf: usize, reg: i32, val: Val) -> Result<usize, VmError> {
        match reg {
            0..=15 => Ok(compile_assign(self.code, buf, reg, val)),
            32..=47 => {
                let ins = if val < 0 {
                    [0x43, 0, 0, val as u8, 0, 0x80 | (reg - 32) as u8, 0, 0]
                } else {
                    [0x53, 0, (val / 256) as u8, val as u8, 0, 0x80 | (reg - 32) as u8, 0, 0]
                };
                write8(self.code, buf, ins);
                Ok(buf + 8)
            }
            129..=131 => {
                if (-128..0).contains(&val) {
                    // SPRM operand: goes through a temporary register.
                    let r = nexttarget(0)?;
                    let b = compile_assign(self.code, buf, r, val);
                    let mut ins = [0x41, 0, 0, 0, 0, 0, 0, 0];
                    ins[(reg - 128) as usize + 2] = 128 | r as u8;
                    write8(self.code, b, ins);
                    return Ok(b + 8);
                }
                let mut ins = [if val < 0 { 0x41 } else { 0x51 }, 0, 0, 0, 0, 0, 0, 0];
                ins[(reg - 128) as usize + 2] = 128 | val as u8;
                write8(self.code, buf, ins);
                Ok(buf + 8)
            }
            136 => {
                let ins = if val < 0 {
                    [0x46, 0, 0, 0, 0, val as u8, 0, 0]
                } else {
                    [0x56, 0, 0, 0, (val / 256) as u8, val as u8, 0, 0]
                };
                write8(self.code, buf, ins);
                Ok(buf + 8)
            }
            _ => cerr(format!("ERR:  Cannot set SPRM {}", reg - 128)),
        }
    }

    fn jump(&mut self, mut buf: usize, mut i1: i32, mut i2: i32, i3: i32) -> Result<usize, VmError> {
        let ismenu = self.domain;
        if i1 == 1 && ismenu == Domain::Vmgm {
            i1 = 0;
        }
        if ((i2 > 0 && i2 < 128) || (i2 == 0 && i1 == 1)) && ismenu == Domain::Vts {
            return cerr("ERR:  Cannot jump to a menu from a title, use 'call' instead");
        }
        if i2 > 0 && i2 < 128 && (i3 & 65535) != 0 && ismenu != Domain::Vts {
            return cerr("ERR:  Cannot specify chapter when jumping to another menu");
        }
        if i1 != 0 && i2 == 0 {
            return cerr("ERR:  Cannot omit menu/title if specifying vmgm/titleset");
        }
        if i1 == 0 && i2 == 0 && (i3 & 65535) == 0 {
            return cerr("ERR:  Nop jump statement");
        }
        if i2 == 121 && (i1 >= 2 || (i1 == 0 && ismenu != Domain::Vmgm)) {
            return cerr("ERR:  VMGM must be specified with FPC");
        }
        let w = |code: &mut Vec<u8>, at, d| write8(code, at, d);
        if i1 >= 2 && (120..128).contains(&i2) {
            if i2 == 120 {
                i2 = 123;
            }
            w(self.code, buf, [0x30, 0x06, 0, 0x01, (i1 - 1) as u8, 0x80 + (i2 - 120) as u8, 0, 0]);
            buf += 8;
        } else if i1 >= 2
            || (i1 == 1 && i2 >= 128)
            || (ismenu == Domain::Vmgm && i2 >= 128 && (i3 & 65535) != 0)
        {
            // Needs the jump pad, which is never enabled here.
            return cerr("ERR:  That form of jumping is not allowed");
        } else if i1 == 1 || i2 == 121 {
            if i2 == 0 || i2 == 120 {
                i2 = 122;
            }
            if i2 < 120 {
                w(self.code, buf, [0x30, 0x06, 0, i2 as u8, 0, 0xC0, 0, 0]);
            } else {
                w(self.code, buf, [0x30, 0x06, 0, 0, 0, if i2 == 121 { 0 } else { 0x40 + (i2 - 120) as u8 }, 0, 0]);
            }
            buf += 8;
        } else if i1 == 0 && i2 == 0 && (i3 & 65535) != 0 {
            let Some([chapters, programs, cells]) = self.ctx.pgc else {
                return cerr("ERR:  Cannot jump to a chapter from FPC");
            };
            if i3 >> 16 == 1 && ismenu != Domain::Vts {
                return cerr("ERR:  Menus do not have chapters");
            }
            let (numc, des) = match i3 >> 16 {
                0 => (self.ctx.group_entries.as_ref().map_or(0, |g| g.len() as i32), "pgc"),
                1 => (chapters, "chapter"),
                2 => (programs, "program"),
                3 => (cells, "cell"),
                _ => (0, "<err>"),
            };
            if (i3 & 65535) > numc {
                return cerr(format!("ERR:  Cannot jump to {des} {}, only {numc} exist", i3 & 65535));
            }
            w(self.code, buf, [0x20, 0x04 + (i3 >> 16) as u8, 0, 0, 0, 0, 0, i3 as u8]);
            buf += 8;
        } else if i2 < 128 {
            let Some(group) = &self.ctx.group_entries else {
                return cerr("ERR:  Cannot jump to menu; none exist");
            };
            if (120..128).contains(&i2) {
                match group.iter().position(|e| e & (1 << (i2 - 120)) != 0) {
                    Some(i) => i2 = i as i32 + 1,
                    None => return cerr(format!("ERR:  Cannot find PGC with entry {}", ENTRY_NAMES[(i2 - 120) as usize])),
                }
            } else if i2 > group.len() as i32 {
                return cerr(format!("ERR:  Cannot jump to menu PGC #{i2}, only {} exist", group.len()));
            }
            if ismenu == Domain::Vmgm {
                w(self.code, buf, [0x30, 0x06, 0, (i2 & 127) as u8, 0, 0xC0, 0, 0]);
            } else {
                w(self.code, buf, [0x20, 0x04, 0, 0, 0, 0, 0, (i2 & 127) as u8]);
            }
            buf += 8;
        } else {
            if ismenu != Domain::Vmgm {
                let titles = self.ctx.titles.as_deref().unwrap_or(&[]);
                if i2 - 128 > titles.len() as i32 {
                    return cerr(format!("ERR:  Cannot jump to title #{}, only {} exist", i2 - 128, titles.len()));
                }
                if (i3 & 65535) != 0 && (i3 & 65535) > titles[(i2 - 128 - 1) as usize] {
                    return cerr(format!(
                        "ERR:  Cannot jump to chapter {} of title {}, only {} exist",
                        i3 & 65535,
                        i2 - 128,
                        titles[(i2 - 128 - 1) as usize]
                    ));
                }
            }
            let sub = if ismenu == Domain::Vmgm {
                0x02
            } else if (i3 & 65535) != 0 {
                0x05
            } else {
                0x03
            };
            w(self.code, buf, [0x30, sub, 0, i3 as u8, 0, (i2 - 128) as u8, 0, 0]);
            buf += 8;
        }
        Ok(buf)
    }

    fn call(&mut self, mut buf: usize, i1: i32, mut i2: i32, i3: i32, i4: i32) -> Result<usize, VmError> {
        let ismenu = self.domain;
        let i4 = if i4 == 0 { 1 } else { i4 };
        if ismenu != Domain::Vts {
            return cerr("ERR:  Cannot 'call' a menu from another menu, use 'jump' instead");
        }
        if i2 == 0 || i2 >= 128 {
            return cerr("ERR:  Cannot 'call' another title, use 'jump' instead");
        }
        if i3 != 0 {
            return cerr("ERR:  Cannot 'call' a chapter within a menu");
        }
        if i2 == 121 && (i1 >= 2 || i1 == 0) {
            return cerr("ERR:  VMGM must be specified with FPC");
        }
        if i1 >= 2 || (i1 == 0 && i2 < 120) {
            return cerr("ERR:  Cannot call to a menu in another titleset / a specific menu PGC, only an entry");
        } else if i1 == 1 {
            if i2 == 120 {
                i2 = 122;
            }
            if i2 < 120 {
                write8(self.code, buf, [0x30, 0x08, 0, i2 as u8, i4 as u8, 0xC0, 0, 0]);
            } else {
                write8(self.code, buf, [0x30, 0x08, 0, 0, i4 as u8, if i2 == 121 { 0 } else { 0x40 + (i2 - 120) as u8 }, 0, 0]);
            }
            buf += 8;
        } else {
            if i2 == 120 {
                i2 = 127;
            }
            if !self.ctx.all_menu_entries.iter().any(|e| e & (1 << (i2 - 120)) != 0) {
                return cerr(format!("ERR:  Cannot find PGC with entry {}", ENTRY_NAMES[(i2 - 120) as usize]));
            }
            write8(self.code, buf, [0x30, 0x08, 0, 0, i4 as u8, 0x80 + (i2 - 120) as u8, 0, 0]);
            buf += 8;
        }
        Ok(buf)
    }
}

pub const ENTRY_NAMES: [&str; 8] = ["", "", "title", "root", "subtitle", "audio", "angle", "ptt"];

// ---------------------------------------------------------------------------
// Peephole optimizer
// ---------------------------------------------------------------------------

fn is_referenced(code: &[u8], buf: usize, end: usize, line: usize) -> bool {
    let mut b = buf;
    while b != end {
        if code[b] == 0 && (code[b + 1] & 15) == 1 && code[b + 7] as usize == line {
            return true;
        }
        b += 8;
    }
    false
}

fn delete_instruction(code: &mut [u8], obuf: usize, buf: usize, end: &mut usize, b: usize) {
    let line = (b - obuf) / 8 + 1;
    let mut b2 = buf;
    while b2 < *end {
        if code[b2] == 0 && (code[b2 + 1] & 15) == 1 && code[b2 + 7] as usize > line {
            code[b2 + 7] -= 1;
        }
        b2 += 8;
    }
    code.copy_within(b + 8..*end, b);
    *end -= 8;
    code[*end..*end + 8].fill(0);
}

fn extract_if(b: &[u8]) -> Result<u32, VmError> {
    match b[0] >> 4 {
        0..=2 => Ok(((b[1] >> 4) as u32) << 24 | (b[3] as u32) << 16 | (b[4] as u32) << 8 | b[5] as u32),
        x => cerr(format!("ERR:  Unhandled extractif scenario ({x:x}), file bug")),
    }
}

fn negate_if(ifs: u32) -> u32 {
    (ifs & 0x8ff_ffff) | (negate_compare(((ifs >> 24) & 7) as u8) as u32) << 24
}

fn apply_if(b: &mut [u8], ifs: u32) -> Result<(), VmError> {
    match b[0] >> 4 {
        0..=2 => {
            b[5] = ifs as u8;
            b[4] = (ifs >> 8) as u8;
            b[3] = (ifs >> 16) as u8;
            b[1] |= ((ifs >> 24) as u8) << 4;
        }
        3..=5 => {
            b[7] = ifs as u8;
            b[6] = (ifs >> 16) as u8;
            b[1] |= ((ifs >> 24) as u8) << 4;
        }
        6 | 7 => {
            b[7] = ifs as u8;
            b[6] = (ifs >> 8) as u8;
            b[2] = (ifs >> 16) as u8;
            b[1] = ((ifs >> 24) as u8) << 4;
        }
        x => return cerr(format!("ERR:  Unhandled applyif scenario ({x:x}), file bug")),
    }
    Ok(())
}

fn if_combinable(b0: u8, b1: u8, b8: u8) -> bool {
    let iftype = match b0 >> 4 {
        0 | 1 | 2 | 6 | 7 => (b1 >> 7) as i32,
        3..=5 => 0,
        _ => return false,
    };
    match b8 >> 4 {
        0 | 1 | 2 | 6 | 7 => true,
        3..=5 => iftype == 0,
        _ => false,
    }
}

/// `vm_optimize`: peephole optimizations on `code[buf..*end]`; `obuf` is the base of branch targets.
pub fn optimize(code: &mut [u8], obuf: usize, buf: usize, end: &mut usize) -> Result<(), VmError> {
    'again: loop {
        let mut b = buf;
        while b < *end {
            let curline = (b - obuf) / 8 + 1;
            // jump over one statement => inverse conditional
            if code[b] == 0
                && (code[b + 1] & 0x70) != 0
                && (code[b + 1] & 15) == 1
                && code[b + 7] as usize == curline + 2
                && b + 9 < code.len()
                && (code[b + 9] & 0x70) == 0
                && ((code[b + 8] & 15) == 0 || (code[b + 9] & 15) == 0)
                && if_combinable(code[b], code[b + 1], code[b + 8])
                && !is_referenced(code, buf, *end, curline + 1)
            {
                let ifs = negate_if(extract_if(&code[b..b + 8])?);
                code.copy_within(b + 8..b + 16, b);
                code[b + 8..b + 16].fill(0);
                apply_if(&mut code[b..b + 8], ifs)?;
                continue 'again;
            }
            // NOP with following instructions or without references
            if code[b..b + 8] == [0; 8] && (b + 8 != *end || !is_referenced(code, buf, *end, curline)) {
                delete_instruction(code, obuf, buf, end, b);
                continue 'again;
            }
            // dead code after an unconditional transfer
            if b > buf && (code[b - 8] >> 4) <= 3 && (code[b - 7] & 0x70) == 0 && (code[b - 7] & 15) != 0 && !is_referenced(code, buf, *end, curline) {
                delete_instruction(code, obuf, buf, end, b);
                continue 'again;
            }
            // merge consecutive SetSTN
            if b + 8 != *end
                && (code[b] & 0xEF) == 0x41
                && code[b + 1] == 0
                && code[b] == code[b + 8]
                && code[b + 1] == code[b + 9]
                && !is_referenced(code, buf, *end, curline + 1)
            {
                for k in 3..=5 {
                    if code[b + 8 + k] != 0 {
                        code[b + k] = code[b + 8 + k];
                    }
                }
                delete_instruction(code, obuf, buf, end, b + 8);
                continue 'again;
            }
            // SetHL_BTNN + link
            if b + 8 != *end
                && code[b] == 0x56
                && code[b + 1] == 0
                && code[b + 8] == 0x20
                && (matches!(code[b + 9] & 0xf, 5..=7) || ((code[b + 9] & 0xf) == 1 && (code[b + 15] & 0x1f) != 0))
                && !is_referenced(code, buf, *end, curline + 1)
            {
                if code[b + 14] == 0 {
                    code[b + 14] = code[b + 4];
                }
                delete_instruction(code, obuf, buf, end, b);
                continue 'again;
            }
            // set register + link
            if b + 8 != *end
                && ((code[b] & 0xE0) == 0x40 || (code[b] & 0xE0) == 0x60)
                && (code[b + 1] & 0x7f) == 0
                && code[b + 8] == 0x20
                && (matches!(code[b + 9] & 0x7f, 4..=7) || ((code[b + 9] & 0x7f) == 1 && (code[b + 15] & 0x1f) != 0))
                && !is_referenced(code, buf, *end, curline + 1)
            {
                code[b + 1] = code[b + 9];
                code[b + 6] = code[b + 14];
                code[b + 7] = code[b + 15];
                delete_instruction(code, obuf, buf, end, b + 8);
                continue 'again;
            }
            b += 8;
        }
        return Ok(());
    }
}

/// `vm_compile`: compiles `program` into `code` starting at byte offset `buf`; `obuf` is the base
/// of branch targets. Returns the end offset (code is optimized). `code` is grown as needed.
pub fn compile(code: &mut Vec<u8>, obuf: usize, buf: usize, program: &[Stmt], ctx: &Context, domain: Domain) -> Result<usize, VmError> {
    // Room for the look-ahead dvdauthor's compiler writes beyond the generated code.
    if code.len() < buf + 8 * 192 {
        code.resize(buf + 8 * 192, 0);
    }
    let mut c = Compiler { code, obuf, ctx, domain };
    let mut end = c.compile(buf, program)?;
    optimize(c.code, obuf, buf, &mut end)?;
    Ok(end)
}

/// Compiles a standalone command string into instructions (tests / buttons): branch targets are
/// relative to the first instruction.
pub fn compile_str(src: &str, ctx: &Context, domain: Domain) -> Result<Vec<[u8; 8]>, VmError> {
    let prog = parse(src)?;
    let mut code = Vec::new();
    let end = compile(&mut code, 0, 0, &prog, ctx, domain)?;
    Ok(code[..end].chunks_exact(8).map(|c| c.try_into().expect("8-byte chunk")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context { group_entries: Some(vec![0x80, 0, 0]), all_menu_entries: vec![0x80], pgc: Some([3, 3, 2]), titles: Some(vec![5, 3]) }
    }

    fn hex(v: Vec<[u8; 8]>) -> Vec<String> {
        v.iter().map(|i| i.iter().map(|b| format!("{b:02x}")).collect()).collect()
    }

    fn build(src: &str, domain: Domain) -> Vec<String> {
        hex(compile_str(src, &ctx(), domain).unwrap())
    }

    #[test]
    fn simple_commands() {
        assert_eq!(build("exit;", Domain::Vts), ["3001000000000000"]);
        assert_eq!(build("resume;", Domain::Vts), ["2001000000000010"]);
        assert_eq!(build("jump cell 2;", Domain::Vtsm), ["2007000000000002"]);
        assert_eq!(build("call vmgm menu 3;", Domain::Vts), ["3008000301c00000"]);
        assert_eq!(build("jump titleset 2 menu entry audio;", Domain::Vmgm), ["3006000102850000"]);
        assert_eq!(build("subtitle = 64;", Domain::Vtsm), ["51000000c0000000"]);
    }

    #[test]
    fn set_and_jump_title() {
        // `g1 = 0` is a register set, the following jump keeps its own instruction.
        assert_eq!(build("g1 = 0; jump title 2;", Domain::Vtsm), ["7100000100000000", "3003000000020000"]);
        assert_eq!(build("audio = 1; subtitle = 64;", Domain::Vtsm), ["51000081c0000000"]);
    }

    #[test]
    fn conditional_jump() {
        // `if (g1 eq 1) jump title 3` negates to `if g1 != 1 goto next`.
        assert_eq!(build("if (g1 eq 1) jump title 3; jump menu 2;", Domain::Vmgm), ["00b1000100010003", "3002000000030000", "3006000200c00000"]);
    }

    #[test]
    fn unsupported_is_an_error() {
        assert!(parse("g1 = g2 + 1;").is_err());
        assert!(parse("goto x;").is_err());
    }
}
