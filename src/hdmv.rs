//! HDMV navigation: the 12-byte command encoding and the `index.bdmv` / `MovieObject.bdmv`
//! writers. Layouts follow libbluray (`hdmv_insn.h`, `indx_parse.c`, `mobj_parse.c`) and were
//! compared with the files tsMuxeR 2.7.0 writes.

use crate::igs::Command;

/// Operand of a command: immediate value or register (GPR `n`, or PSR `n` with bit 31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Imm(u32),
    Gpr(u32),
    Psr(u32),
}

impl Op {
    fn value(self) -> u32 {
        match self {
            Op::Imm(v) | Op::Gpr(v) => v,
            Op::Psr(v) => 0x8000_0000 | v,
        }
    }
    fn imm(self) -> bool {
        matches!(self, Op::Imm(_))
    }
}

/// `grp`: 0 branch, 1 compare, 2 set. `sub`: sub-group. `opt`: branch / compare / set option.
fn cmd(grp: u8, sub: u8, opt: u8, a: Option<Op>, b: Option<Op>) -> Command {
    let cnt = a.is_some() as u8 + b.is_some() as u8;
    let (i1, i2) = (a.is_some_and(Op::imm), b.is_some_and(Op::imm));
    let (b1, b3) = match grp {
        0 => (opt, 0),
        2 => (0, opt),
        _ => (0, 0),
    };
    let b2 = if grp == 1 { opt } else { 0 };
    let mut c = [0u8; 12];
    c[0] = cnt << 5 | grp << 3 | sub;
    c[1] = (i1 as u8) << 7 | (i2 as u8) << 6 | b1;
    c[2] = b2;
    c[3] = b3;
    c[4..8].copy_from_slice(&a.map_or(0, Op::value).to_be_bytes());
    c[8..12].copy_from_slice(&b.map_or(0, Op::value).to_be_bytes());
    c
}

pub fn nop() -> Command {
    cmd(0, 0, 0, None, None)
}
pub fn goto(pos: u32) -> Command {
    cmd(0, 0, 1, Some(Op::Imm(pos)), None)
}
pub fn jump_object(obj: u32) -> Command {
    cmd(0, 1, 0, Some(Op::Imm(obj)), None)
}
pub fn jump_title(title: u32) -> Command {
    cmd(0, 1, 1, Some(Op::Imm(title)), None)
}
/// `JumpObject` / `JumpTitle` to the object / title held in `target` (libbluray fetches every
/// operand with `imm_op1 = 0` from its register, like `PlayPLatMark`'s).
pub fn jump_object_op(target: Op) -> Command {
    cmd(0, 1, 0, Some(target), None)
}
pub fn jump_title_op(target: Op) -> Command {
    cmd(0, 1, 1, Some(target), None)
}
pub fn call_object(obj: u32) -> Command {
    cmd(0, 1, 2, Some(Op::Imm(obj)), None)
}
pub fn resume() -> Command {
    cmd(0, 1, 4, None, None)
}
pub fn play_pl(pl: u32) -> Command {
    cmd(0, 2, 0, Some(Op::Imm(pl)), None)
}
/// Plays playlist `pl` from the chapter mark held in `mark`.
pub fn play_pl_at_mark(pl: Op, mark: Op) -> Command {
    cmd(0, 2, 2, Some(pl), Some(mark))
}

pub fn mov(dst: u32, src: Op) -> Command {
    cmd(2, 0, 1, Some(Op::Gpr(dst)), Some(src))
}
pub fn add(dst: u32, src: Op) -> Command {
    cmd(2, 0, 3, Some(Op::Gpr(dst)), Some(src))
}

/// Comparison options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq = 2,
    Ne = 3,
    Ge = 4,
    Gt = 5,
    Le = 6,
    Lt = 7,
}

/// Compare: the command after it runs only if the comparison holds.
pub fn compare(a: Op, c: Cmp, b: Op) -> Command {
    cmd(1, 0, c as u8, Some(a), Some(b))
}

pub fn and(dst: u32, src: Op) -> Command {
    cmd(2, 0, 9, Some(Op::Gpr(dst)), Some(src))
}

/// `SetStream(dst, src)`. libbluray: `dst` bit 31 + bits 16–27 = primary audio stream number,
/// bit 15 + bits 0–11 = PG stream number, bit 14 = PG display flag (written on every call, so
/// it must always carry the wanted state).
pub fn set_stream(dst: u32) -> Command {
    cmd(2, 1, 1, Some(Op::Imm(dst)), Some(Op::Imm(0)))
}

/// `SetStream` in register form: the flags (31 audio, 15 PG, 14 PG display) stay immediate and the
/// 12-bit number fields name the GPRs holding the audio / PG stream numbers (libbluray
/// `_fetch_operand` with `setstream`), so the operand is not one register's value.
pub fn set_stream_regs(audio: Option<u32>, pg: Option<u32>, display: bool) -> Command {
    let a = audio.map_or(0, |r| 0x8000_0000 | (r & 0xFFF) << 16);
    let p = pg.map_or(0, |r| 0x8000 | r & 0xFFF);
    cmd(2, 1, 1, Some(Op::Gpr(a | p | if display { PG_DISPLAY } else { 0 })), Some(Op::Imm(0)))
}

/// `SetStream` operand selecting audio stream `n` (1-based).
pub fn audio_stream(n: u32) -> u32 {
    0x8000_0000 | (n & 0xFFF) << 16
}

/// `SetStream` operand selecting PG stream `n` (1-based) and showing it.
pub fn pg_stream_on(n: u32) -> u32 {
    0xC000 | (n & 0xFFF)
}

/// Bit 14 of a `SetStream` operand: PG display on.
pub const PG_DISPLAY: u32 = 0x4000;

/// `SetButtonPage` in a movie object: selected button (PSR10) and menu page 0 (PSR11).
pub fn set_button_page(button: u32) -> Command {
    cmd(2, 1, 3, Some(Op::Imm(0x8000_0000 | button & 0xFFFF)), Some(Op::Imm(0x8000_0000)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieObject {
    pub commands: Vec<Command>,
    /// Resume intention flag (the player may return into a title after a menu call).
    pub resume: bool,
}

/// `MovieObject.bdmv`.
pub fn movie_object_bdmv(objects: &[MovieObject]) -> Vec<u8> {
    let mut out = b"MOBJ0200".to_vec();
    out.resize(0x28, 0);
    let mut body = vec![0u8; 4];
    body.extend((objects.len() as u16).to_be_bytes());
    for o in objects {
        body.extend((if o.resume { 0x8000u16 } else { 0 }).to_be_bytes());
        body.extend((o.commands.len() as u16).to_be_bytes());
        o.commands.iter().for_each(|c| body.extend(c));
    }
    out.extend((body.len() as u32).to_be_bytes());
    out.extend(body);
    out
}

/// `index.bdmv`: First Play and Top Menu run movie objects, title `n` runs `titles[n-1]`.
pub fn index_bdmv(first_play: u16, top_menu: u16, titles: &[u16]) -> Vec<u8> {
    let mut out = b"INDX0200".to_vec();
    out.extend(0x4Eu32.to_be_bytes()); // indexes start
    out.resize(0x28, 0); // extension data start 0
    out.extend(34u32.to_be_bytes()); // app info (all zero, as tsMuxeR writes it)
    out.resize(0x4E, 0);
    let item = |access: u8, playback: u16, obj: u16| {
        let mut v = (0x4000_0000u32 | (access as u32) << 28).to_be_bytes().to_vec();
        v.extend((playback << 14).to_be_bytes());
        v.extend(obj.to_be_bytes());
        v.extend([0; 4]);
        v
    };
    let len = 12 + 12 + 2 + 12 * titles.len();
    out.extend((len as u32).to_be_bytes());
    out.extend(item(0, 1, first_play));
    out.extend(item(0, 1, top_menu));
    out.extend((titles.len() as u16).to_be_bytes());
    for &t in titles {
        out.extend(item(0, 0, t));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_operand_jumps() {
        // byte 0: op_cnt 1, group branch, sub-group jump; byte 1: imm_op1 clear, option 0 / 1.
        assert_eq!(jump_object_op(Op::Gpr(5)), [0x21, 0x00, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0]);
        assert_eq!(jump_title_op(Op::Gpr(5)), [0x21, 0x01, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0]);
        assert_eq!(jump_object_op(Op::Imm(7)), jump_object(7));
        assert_eq!(jump_title_op(Op::Imm(3)), jump_title(3));
        // SetStream audio = GPR16, PG = GPR17 shown: imm_op1 clear, flags + register numbers.
        assert_eq!(set_stream_regs(Some(16), Some(17), true), [0x51, 0x40, 0, 1, 0x80, 0x10, 0xC0, 0x11, 0, 0, 0, 0]);
        // Same operand form as the register mark of PlayPLatMark (in use since the BD menus).
        assert_eq!(play_pl_at_mark(Op::Imm(0), Op::Gpr(1))[1] & 0xC0, 0x80);
    }

    #[test]
    fn command_bytes_match_tsmuxer_output() {
        // Taken from tsMuxeR's MovieObject.bdmv: Move GPR10,0 / PlayPL 0 / JumpObject 2 ...
        assert_eq!(mov(10, Op::Imm(0)), [0x50, 0x40, 0, 1, 0, 0, 0, 10, 0, 0, 0, 0]);
        assert_eq!(mov(3, Op::Imm(0xFFFF)), [0x50, 0x40, 0, 1, 0, 0, 0, 3, 0, 0, 0xFF, 0xFF]);
        assert_eq!(play_pl(0), [0x22, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(jump_object(2), [0x21, 0x80, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
        assert_eq!(compare(Op::Gpr(10), Cmp::Eq, Op::Imm(1))[..4], [0x48, 0x40, 2, 0]);
        assert_eq!(play_pl_at_mark(Op::Imm(1), Op::Gpr(1))[..4], [0x42, 0x82, 0, 0]);
    }

    #[test]
    fn index_layout() {
        let i = index_bdmv(2, 1, &[0]);
        assert_eq!(i.len(), 0x78);
        assert_eq!(&i[..8], b"INDX0200");
        assert_eq!(&i[0x4E..0x52], &[0, 0, 0, 38]);
        assert_eq!(&i[0x52..0x5E], &[0x40, 0, 0, 0, 0x40, 0, 0, 2, 0, 0, 0, 0]);
        assert_eq!(&i[0x5E..0x6A], &[0x40, 0, 0, 0, 0x40, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&i[0x6A..0x6C], &[0, 1]);
        assert_eq!(&i[0x6C..], &[0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn movie_object_layout() {
        let m = movie_object_bdmv(&[MovieObject { commands: vec![play_pl(1), resume()], resume: true }]);
        assert_eq!(&m[..8], b"MOBJ0200");
        let len = u32::from_be_bytes(m[0x28..0x2C].try_into().unwrap()) as usize;
        assert_eq!(m.len(), 0x2C + len);
        assert_eq!(&m[0x30..0x36], &[0, 1, 0x80, 0, 0, 2]);
    }
}
