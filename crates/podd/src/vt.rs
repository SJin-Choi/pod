//! VT state shared by the holder and the daemon: an `alacritty_terminal` grid
//! fed with raw PTY bytes, counted by an absolute output offset (`seq`).
//!
//! The holder keeps the authoritative copy (it answers DA/DSR queries and
//! produces the snapshot a reconnecting daemon starts from). The daemon keeps
//! a mirror fed from the same byte stream for `read`, `wait` and attach
//! snapshots. Both sides use [`Vt::snapshot_ansi`] to turn a grid back into
//! bytes that reproduce it on a fresh terminal of the same size.

use std::fmt::Write as _;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};

use crate::protocol::ReadMode;

pub const SCROLLBACK_LINES: usize = 10_000;

/// Receives the terminal's replies to queries (DA, DSR, ...). Only the holder
/// installs one: replies must be sent to the program exactly once.
pub type ReplySink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Reply to the OSC 7501 capability query (`ESC ] 7501 ; ? BEL|ST`), the
/// program-status protocol Claude Code speaks (docs/04 §1.2, §4.4). Claude only
/// starts sending `ESC]7501;state=...` reports after it receives this.
pub const OSC7501_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// Finds OSC 7501 queries in a byte stream split into arbitrary chunks.
/// alacritty ignores unknown OSCs, so the holder answers this one itself.
#[derive(Default)]
pub struct Osc7501Query {
    /// Bytes of `ESC]7501;?` matched so far, or `QUERY.len()` / `+1` while
    /// waiting for the BEL / ST terminator.
    matched: usize,
}

const QUERY: &[u8] = b"\x1b]7501;?";

impl Osc7501Query {
    /// Offsets just past each complete query that ends inside `chunk`.
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<usize> {
        let mut ends = Vec::new();
        for (i, &b) in chunk.iter().enumerate() {
            let q = QUERY.len();
            self.matched = match self.matched {
                m if m < q && b == QUERY[m] => m + 1,
                m if m == q && b == 0x07 => {
                    ends.push(i + 1);
                    0
                }
                m if m == q && b == 0x1b => q + 1,
                m if m == q + 1 && b == b'\\' => {
                    ends.push(i + 1);
                    0
                }
                // Anything else (including other ESC]7501;...  reports): restart,
                // letting this byte begin a new match.
                _ => usize::from(b == 0x1b),
            };
        }
        ends
    }
}

#[derive(Clone)]
pub struct VtListener {
    reply: Option<ReplySink>,
}

impl EventListener for VtListener {
    fn send_event(&self, event: Event) {
        if let (Event::PtyWrite(s), Some(reply)) = (event, &self.reply) {
            reply(s.as_bytes());
        }
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Vt {
    term: Term<VtListener>,
    parser: Processor,
    /// Total output bytes fed so far (absolute offset of the next byte).
    seq: u64,
    /// Last OSC 7501 reports + window title, replayed at the end of snapshots.
    replay: crate::osc_replay::OscReplay,
    /// DECSTBM scroll region (alacritty keeps its own copy private).
    region: RegionTracker,
}

/// Follows the DECSTBM scroll region (`CSI top ; bottom r`) in the byte
/// stream the same way alacritty applies it: invalid regions are ignored, a
/// reset (`ESC c`, `CSI ! p`) or a resize restores the full screen.
#[derive(Default)]
struct RegionTracker {
    st: RegionSt,
    buf: Vec<u8>,
    /// 1-based inclusive (top, bottom); `None` = whole screen.
    region: Option<(usize, usize)>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum RegionSt {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl RegionTracker {
    fn feed(&mut self, bytes: &[u8], rows: usize) {
        for &b in bytes {
            match self.st {
                RegionSt::Ground => {
                    if b == 0x1b {
                        self.st = RegionSt::Esc;
                    }
                }
                RegionSt::Esc => {
                    self.buf.clear();
                    self.st = match b {
                        b'[' => RegionSt::Csi,
                        b'c' => {
                            self.region = None;
                            RegionSt::Ground
                        }
                        0x1b => RegionSt::Esc,
                        _ => RegionSt::Ground,
                    };
                }
                RegionSt::Csi => match b {
                    0x20..=0x3f if self.buf.len() < 32 => self.buf.push(b),
                    0x40..=0x7e => {
                        self.on_csi(b, rows);
                        self.st = RegionSt::Ground;
                    }
                    0x1b => self.st = RegionSt::Esc,
                    _ => self.st = RegionSt::Ground,
                },
            }
        }
    }

    fn on_csi(&mut self, fin: u8, rows: usize) {
        match (fin, self.buf.as_slice()) {
            (b'p', b"!") => self.region = None, // DECSTR
            (b'r', params) if params.iter().all(|c| c.is_ascii_digit() || *c == b';') => {
                let mut it = params.split(|&c| c == b';').map(|p| {
                    std::str::from_utf8(p)
                        .ok()
                        .and_then(|p| p.parse::<usize>().ok())
                        .filter(|&n| n > 0)
                });
                let top = it.next().flatten().unwrap_or(1);
                let bottom = it.next().flatten().unwrap_or(rows);
                if top >= bottom {
                    return; // alacritty ignores invalid regions
                }
                let bottom = bottom.min(rows);
                self.region = (top > 1 || bottom < rows).then_some((top, bottom));
            }
            _ => {}
        }
    }
}

impl Vt {
    pub fn new(cols: u16, rows: u16, reply: Option<ReplySink>) -> Self {
        let size = Size {
            cols: cols.max(2) as usize,
            rows: rows.max(2) as usize,
        };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, VtListener { reply }),
            parser: Processor::new(),
            seq: 0,
            replay: Default::default(),
            region: Default::default(),
        }
    }

    /// A terminal whose state is the given snapshot, positioned at `seq`.
    pub fn from_snapshot(cols: u16, rows: u16, seq: u64, snapshot: &[u8]) -> Self {
        let mut vt = Self::new(cols, rows, None);
        vt.parser.advance(&mut vt.term, snapshot);
        vt.replay.feed(snapshot);
        vt.region.feed(snapshot, rows.max(2) as usize);
        vt.seq = seq;
        vt
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.replay.feed(bytes);
        self.region.feed(bytes, self.term.screen_lines());
        self.seq += bytes.len() as u64;
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.region.region = None; // alacritty resets the region on resize
        self.term.resize(Size {
            cols: cols.max(2) as usize,
            rows: rows.max(2) as usize,
        });
    }

    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// The program turned on bracketed paste (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Text lines of the screen or of scrollback + screen. Trailing spaces and
    /// trailing blank lines are removed; soft-wrapped rows are joined.
    pub fn read_lines(&self, mode: ReadMode) -> (Vec<String>, (u16, u16)) {
        let grid = self.term.grid();
        let screen = grid.screen_lines() as i32;
        let cols = grid.columns();
        let top = match mode {
            ReadMode::Screen => 0,
            ReadMode::History => -(grid.history_size() as i32),
        };
        let mut lines = Vec::new();
        let mut current = String::new();
        for line in top..screen {
            let row = &grid[Line(line)];
            let mut wrapped = false;
            for col in 0..cols {
                let cell = &row[Column(col)];
                if col == cols - 1 && cell.flags.contains(Flags::WRAPLINE) {
                    wrapped = true;
                }
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                current.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    current.extend(zw);
                }
            }
            if !wrapped {
                lines.push(current.trim_end().to_string());
                current.clear();
            }
        }
        if !current.is_empty() {
            lines.push(current.trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let cursor = grid.cursor.point;
        (lines, (cursor.column.0 as u16, cursor.line.0.max(0) as u16))
    }

    /// Bytes that rebuild this terminal (screen, up to `max_scrollback` lines
    /// of history, cursor and the input-relevant modes) on a fresh terminal of
    /// the same size.
    ///
    /// In the alternate screen, the main screen behind it (with its history)
    /// is written first, then `?1049h` and the alternate screen, so leaving
    /// the alternate screen later shows the right main screen. The window
    /// title (and OSC 7501 reports) are replayed; the DECSTBM scroll region
    /// and origin mode are restored.
    ///
    /// Still lossy: tab stops, charsets (G0-G3, e.g. DEC line drawing), the
    /// "pending wrap" cursor state, the DECSC saved cursor other than the one
    /// `?1049` keeps, the cursor shape, the palette (OSC 4/10/11), hyperlinks
    /// (OSC 8) and kitty keyboard modes.
    ///
    /// `&mut` because reading the inactive main screen means swapping the
    /// grids for a moment (alacritty keeps it private); the state is restored.
    pub fn snapshot_ansi(&mut self, max_scrollback: usize) -> Vec<u8> {
        let mode = *self.term.mode();
        let cols = self.term.grid().columns();
        let rows = self.term.grid().screen_lines() as i32;
        let history = self.term.grid().history_size().min(max_scrollback) as i32;

        let mut out = String::with_capacity(((history + rows) as usize) * (cols + 8));
        // Reset attributes, home, clear screen and scrollback.
        out.push_str("\x1b[0m\x1b[H\x1b[2J\x1b[3J");
        if mode.contains(TermMode::ALT_SCREEN) {
            // Swap to the main screen, render it, swap back. The second swap
            // clears the alternate grid, so it is put back from a copy; the
            // main screen's saved cursor it rewrites is the same value.
            let alt = self.term.grid().clone();
            self.term.swap_alt();
            {
                let main = self.term.grid();
                let history = main.history_size().min(max_scrollback) as i32;
                write_grid(&mut out, main, history);
                let c = main.cursor.point;
                let _ = write!(out, "\x1b[{};{}H", c.line.0.max(0) + 1, c.column.0 + 1);
            }
            self.term.swap_alt();
            *self.term.grid_mut() = alt;
            out.push_str("\x1b[?1049h\x1b[H\x1b[2J");
            write_grid(&mut out, self.term.grid(), 0);
        } else {
            write_grid(&mut out, self.term.grid(), history);
        }
        let grid = self.term.grid();

        // Scroll region (homes the cursor), then origin mode, then the cursor
        // (relative to the region top when origin mode is on).
        let mut top_offset = 0;
        if let Some((top, bottom)) = self.region.region {
            let _ = write!(out, "\x1b[{top};{bottom}r");
            if mode.contains(TermMode::ORIGIN) {
                top_offset = top as i32 - 1;
            }
        }
        if mode.contains(TermMode::ORIGIN) {
            out.push_str("\x1b[?6h");
        }
        let cursor = grid.cursor.point;
        let _ = write!(
            out,
            "\x1b[{};{}H",
            (cursor.line.0 - top_offset).max(0) + 1,
            cursor.column.0 + 1
        );
        let flags: [(TermMode, &str); 10] = [
            (TermMode::APP_CURSOR, "\x1b[?1h"),
            (TermMode::APP_KEYPAD, "\x1b="),
            (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
            (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
            (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
            (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
            (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
            (TermMode::SGR_MOUSE, "\x1b[?1006h"),
            (TermMode::UTF8_MOUSE, "\x1b[?1005h"),
            (TermMode::INSERT, "\x1b[4h"),
        ];
        for (flag, seq) in flags {
            if mode.contains(flag) {
                out.push_str(seq);
            }
        }
        if !mode.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        let hidden = !mode.contains(TermMode::SHOW_CURSOR)
            || self.term.cursor_style().shape == CursorShape::Hidden;
        out.push_str(if hidden { "\x1b[?25l" } else { "\x1b[?25h" });
        // Window title and agent status reports (see osc_replay.rs).
        let mut out = out.into_bytes();
        out.extend(self.replay.bytes());
        out
    }
}

/// Write `history` lines of scrollback plus the screen of `grid`.
fn write_grid(out: &mut String, grid: &Grid<Cell>, history: i32) {
    let cols = grid.columns();
    let rows = grid.screen_lines() as i32;
    let mut pen = Pen::default();
    for line in -history..rows {
        let row = &grid[Line(line)];
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE) && line < rows - 1;
        // Wrapped rows are written in full so the terminal's own autowrap
        // reproduces the soft wrap; other rows drop trailing blanks.
        let end = if wrapped {
            cols
        } else {
            (0..cols)
                .rev()
                .find(|&c| !is_blank(&row[Column(c)]))
                .map_or(0, |c| c + 1)
        };
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let next = Pen::of(cell);
            if next != pen {
                next.write_sgr(out);
                pen = next;
            }
            out.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                out.extend(zw);
            }
        }
        if pen != Pen::default() {
            out.push_str("\x1b[0m");
            pen = Pen::default();
        }
        if !wrapped && line < rows - 1 {
            out.push_str("\r\n");
        }
    }
}

fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\0')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
}

/// The SGR-relevant attributes of a cell.
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
}

const PEN_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & PEN_FLAGS,
        }
    }

    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        let f = self.flags;
        for (flag, code) in [
            (Flags::BOLD, ";1"),
            (Flags::DIM, ";2"),
            (Flags::ITALIC, ";3"),
            (Flags::INVERSE, ";7"),
            (Flags::HIDDEN, ";8"),
            (Flags::STRIKEOUT, ";9"),
        ] {
            if f.contains(flag) {
                out.push_str(code);
            }
        }
        if f.contains(Flags::DOUBLE_UNDERLINE) {
            out.push_str(";21");
        } else if f.intersects(Flags::ALL_UNDERLINES) {
            out.push_str(";4");
        }
        color_sgr(out, self.fg, true);
        color_sgr(out, self.bg, false);
        out.push('m');
    }
}

fn color_sgr(out: &mut String, color: Color, fg: bool) {
    let base = if fg { 30 } else { 40 };
    match color {
        Color::Spec(rgb) => {
            let _ = write!(out, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b);
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{}", base + 8, i);
        }
        Color::Named(n) => {
            let idx = n as usize;
            let code = if idx < 8 {
                Some(base + idx)
            } else if idx < 16 {
                Some(base + 60 + idx - 8)
            } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&idx)
            {
                Some(base + idx - NamedColor::DimBlack as usize)
            } else {
                None // Foreground/Background/Cursor...: the default
            };
            if let Some(code) = code {
                let _ = write!(out, ";{code}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(vt: &Vt) -> Vec<String> {
        vt.read_lines(ReadMode::History).0
    }

    #[test]
    fn snapshot_reproduces_text_wraps_and_cursor() {
        let mut vt = Vt::new(20, 5, None);
        for i in 0..12 {
            vt.feed(format!("line {i} \x1b[31mred\x1b[0m\r\n").as_bytes());
        }
        vt.feed(b"a very long line that wraps around twice....\r\n");
        vt.feed("wide: \u{4e2d}\u{6587} end".as_bytes());
        let snap = vt.snapshot_ansi(usize::MAX);
        let copy = Vt::from_snapshot(20, 5, vt.seq(), &snap);
        assert_eq!(screen(&copy), screen(&vt));
        assert_eq!(
            copy.read_lines(ReadMode::Screen),
            vt.read_lines(ReadMode::Screen)
        );
        assert_eq!(copy.seq(), vt.seq());
        // Colors survive.
        let red = &copy.term.grid()[Line(-1)][Column(8)];
        assert_eq!(red.c, 'r');
        assert_eq!(red.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn snapshot_limits_scrollback_and_keeps_modes() {
        let mut vt = Vt::new(10, 3, None);
        for i in 0..50 {
            vt.feed(format!("{i}\r\n").as_bytes());
        }
        vt.feed(b"\x1b[?1h\x1b[?2004h");
        let copy = Vt::from_snapshot(10, 3, 0, &vt.snapshot_ansi(5));
        let lines = screen(&copy);
        assert_eq!(lines.first().map(String::as_str), Some("43"));
        assert!(copy.app_cursor());
        assert!(copy.term.mode().contains(TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn osc7501_query_scanner() {
        let mut s = Osc7501Query::default();
        assert_eq!(s.scan(b"ab\x1b[?u\x1b]7501;?\x07\x1b[c"), vec![15]);
        // Split across chunks, ST terminated.
        assert!(s.scan(b"xx\x1b]75").is_empty());
        assert!(s.scan(b"01;?\x1b").is_empty());
        assert_eq!(s.scan(b"\\rest"), vec![1]);
        // Reports are not queries.
        assert!(
            s.scan(b"\x1b]7501;state=idle:app=claude-code\x07")
                .is_empty()
        );
        assert!(s.scan(b"\x1b]7501;?x\x07").is_empty());
    }

    /// The main screen behind the alternate screen survives a snapshot, the
    /// live terminal is unchanged by taking it, and leaving the alternate
    /// screen on the copy shows the same main screen (and cursor) as the original.
    #[test]
    fn alt_screen_snapshot_keeps_main_screen() {
        let mut vt = Vt::new(12, 4, None);
        for i in 0..6 {
            vt.feed(format!("hist {i}\r\n").as_bytes());
        }
        vt.feed(b"\x1b[32mprompt>\x1b[0m cmd");
        vt.feed(b"\x1b[?1049h\x1b[H\x1b[2Jalt one\r\nalt two\x1b[3;5H");
        let before = vt.read_lines(ReadMode::Screen);
        let snap = vt.snapshot_ansi(100);
        // Taking the snapshot did not disturb the alternate screen.
        assert_eq!(vt.read_lines(ReadMode::Screen), before);
        assert!(vt.term.mode().contains(TermMode::ALT_SCREEN));

        let mut copy = Vt::from_snapshot(12, 4, 0, &snap);
        assert!(copy.term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(copy.read_lines(ReadMode::Screen), before);
        assert_eq!(copy.term.grid().cursor.point, vt.term.grid().cursor.point);
        // A second snapshot of the original is identical (state restored).
        assert_eq!(vt.snapshot_ansi(100), snap);

        vt.feed(b"\x1b[?1049l");
        copy.feed(b"\x1b[?1049l");
        assert_eq!(
            copy.read_lines(ReadMode::History),
            vt.read_lines(ReadMode::History)
        );
        assert_eq!(
            copy.read_lines(ReadMode::Screen).0.last().unwrap(),
            "prompt> cmd"
        );
        assert_eq!(copy.term.grid().cursor.point, vt.term.grid().cursor.point);
        assert_eq!(
            copy.term.grid()[Line(3)][Column(0)].fg,
            Color::Named(NamedColor::Green)
        );
    }

    /// DECSTBM (and origin mode) survive a snapshot: output after it scrolls
    /// only inside the region, exactly as on the original.
    #[test]
    fn scroll_region_survives_snapshot() {
        let mut vt = Vt::new(10, 6, None);
        vt.feed(b"top\r\n\x1b[2;5r\x1b[5;1H");
        for i in 0..3 {
            vt.feed(format!("r{i}\r\n").as_bytes());
        }
        vt.feed(b"\x1b[6;1Hstatus");
        // Origin mode on a second terminal.
        let mut org = Vt::new(10, 6, None);
        org.feed(b"\x1b[3;5r\x1b[?6h\x1b[2;2H");
        for v in [&mut vt, &mut org] {
            let snap = v.snapshot_ansi(100);
            let mut copy = Vt::from_snapshot(10, 6, 0, &snap);
            assert_eq!(copy.term.grid().cursor.point, v.term.grid().cursor.point);
            assert_eq!(copy.region.region, v.region.region);
            assert_eq!(
                copy.term.mode().contains(TermMode::ORIGIN),
                v.term.mode().contains(TermMode::ORIGIN)
            );
            let more = b"\x1b[5;1Hx\r\ny\r\nz\r\n";
            copy.feed(more);
            v.feed(more);
            assert_eq!(
                copy.read_lines(ReadMode::Screen),
                v.read_lines(ReadMode::Screen)
            );
        }
        assert_eq!(vt.region.region, Some((2, 5)));
        assert_eq!(vt.read_lines(ReadMode::Screen).0[0], "top");
        assert_eq!(vt.read_lines(ReadMode::Screen).0[5], "status");
        // Resets.
        vt.feed(b"\x1b[!p");
        assert_eq!(vt.region.region, None);
        vt.feed(b"\x1b[1;3r");
        vt.resize(10, 7);
        assert_eq!(vt.region.region, None);
        vt.feed(b"\x1b[4;2r"); // invalid: ignored
        assert_eq!(vt.region.region, None);
    }

    #[test]
    fn alt_screen_snapshot() {
        let mut vt = Vt::new(10, 3, None);
        vt.feed(b"main\r\n\x1b[?1049h\x1b[2;3Hxy");
        let copy = Vt::from_snapshot(10, 3, 0, &vt.snapshot_ansi(100));
        assert!(copy.term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(
            copy.read_lines(ReadMode::Screen),
            vt.read_lines(ReadMode::Screen)
        );
    }
}
