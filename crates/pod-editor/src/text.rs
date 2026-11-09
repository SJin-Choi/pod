//! Loading/saving text files: line endings, binary and size guards, and the
//! column conversion between the editor and a language server.
//!
//! The editor buffer always holds `\n` line endings. A file read with CRLF
//! is remembered as CRLF and written back with CRLF, so a Windows checkout is
//! never silently rewritten. Line numbers are unaffected by the conversion
//! and columns only lose the trailing `\r`, so LSP positions stay valid.

use std::path::Path;

/// Files above this open read-only without syntax highlighting or LSP.
pub const LARGE_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Files above this are not opened at all.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// Majority vote over the first lines; ties and files without newlines use LF.
pub fn detect_line_ending(text: &str) -> LineEnding {
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate().take(1 << 20) {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    if crlf > lf { LineEnding::CrLf } else { LineEnding::Lf }
}

pub fn to_lf(text: &str) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n") } else { text.to_string() }
}

pub fn from_lf(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => {
            let mut out = String::with_capacity(text.len() + text.len() / 32);
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(line.strip_suffix('\r').unwrap_or(line));
            }
            out
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    TooLarge(u64),
    Binary,
    Io(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::TooLarge(n) => write!(f, "파일이 너무 큽니다 ({} MB)", n / (1024 * 1024)),
            LoadError::Binary => write!(f, "바이너리 파일은 열 수 없습니다"),
            LoadError::Io(e) => write!(f, "파일을 읽지 못했습니다: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Loaded {
    /// LF-normalised text.
    pub text: String,
    pub ending: LineEnding,
    pub had_bom: bool,
    pub large: bool,
    pub len: u64,
}

pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

pub fn load(path: &Path) -> Result<Loaded, LoadError> {
    let len = std::fs::metadata(path).map_err(|e| LoadError::Io(e.to_string()))?.len();
    if len > MAX_FILE_BYTES {
        return Err(LoadError::TooLarge(len));
    }
    let bytes = std::fs::read(path).map_err(|e| LoadError::Io(e.to_string()))?;
    decode(bytes, len)
}

pub fn decode(mut bytes: Vec<u8>, len: u64) -> Result<Loaded, LoadError> {
    let had_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    if had_bom {
        bytes.drain(..3);
    }
    if looks_binary(&bytes) {
        return Err(LoadError::Binary);
    }
    let raw = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let ending = detect_line_ending(&raw);
    Ok(Loaded { text: to_lf(&raw), ending, had_bom, large: len > LARGE_FILE_BYTES, len })
}

/// Bytes to write for an LF buffer, restoring the file's ending and BOM.
pub fn encode(text: &str, ending: LineEnding, bom: bool) -> Vec<u8> {
    let body = from_lf(text, ending);
    let mut out = Vec::with_capacity(body.len() + 3);
    if bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

/// Column unit negotiated with a server (`general.positionEncodings`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PosEncoding {
    Utf8,
    #[default]
    Utf16,
    /// Unicode scalar values: the unit gpui-component's editor uses.
    Utf32,
}

impl PosEncoding {
    pub fn from_kind(kind: Option<&lsp_types::PositionEncodingKind>) -> Self {
        match kind.map(|k| k.as_str()) {
            Some("utf-8") => PosEncoding::Utf8,
            Some("utf-32") => PosEncoding::Utf32,
            _ => PosEncoding::Utf16,
        }
    }
}

/// Editor column (chars) -> wire column for one line of text.
pub fn char_col_to_wire(line: &str, chars: u32, enc: PosEncoding) -> u32 {
    let mut out = 0u32;
    for (i, c) in line.chars().enumerate() {
        if i as u32 >= chars {
            break;
        }
        out += match enc {
            PosEncoding::Utf8 => c.len_utf8() as u32,
            PosEncoding::Utf16 => c.len_utf16() as u32,
            PosEncoding::Utf32 => 1,
        };
    }
    if enc == PosEncoding::Utf32 { chars } else { out }
}

/// Wire column -> editor column (chars), clamped to the line.
pub fn wire_col_to_char(line: &str, wire: u32, enc: PosEncoding) -> u32 {
    if enc == PosEncoding::Utf32 {
        return wire;
    }
    let mut units = 0u32;
    let mut chars = 0u32;
    for c in line.chars() {
        if units >= wire {
            break;
        }
        units += match enc {
            PosEncoding::Utf8 => c.len_utf8() as u32,
            _ => c.len_utf16() as u32,
        };
        chars += 1;
    }
    chars
}

/// Text of line `row` without its line terminator (LF buffers).
pub fn line_of(text: &str, row: u32) -> &str {
    text.split('\n').nth(row as usize).map(|l| l.strip_suffix('\r').unwrap_or(l)).unwrap_or("")
}

pub fn pos_to_wire(text: &str, p: lsp_types::Position, enc: PosEncoding) -> lsp_types::Position {
    if enc == PosEncoding::Utf32 {
        return p;
    }
    lsp_types::Position::new(p.line, char_col_to_wire(line_of(text, p.line), p.character, enc))
}

pub fn pos_from_wire(text: &str, p: lsp_types::Position, enc: PosEncoding) -> lsp_types::Position {
    if enc == PosEncoding::Utf32 {
        return p;
    }
    lsp_types::Position::new(p.line, wire_col_to_char(line_of(text, p.line), p.character, enc))
}

pub fn range_from_wire(text: &str, r: lsp_types::Range, enc: PosEncoding) -> lsp_types::Range {
    lsp_types::Range::new(pos_from_wire(text, r.start, enc), pos_from_wire(text, r.end, enc))
}

/// Byte offset of an editor (char-column) position in an LF buffer.
pub fn offset_of(text: &str, p: lsp_types::Position) -> usize {
    let mut off = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i as u32 == p.line {
            let col = line.char_indices().nth(p.character as usize).map(|(b, _)| b).unwrap_or(line.len());
            return off + col;
        }
        off += line.len() + 1;
    }
    text.len()
}

/// Editor (char-column) position of a byte offset in an LF buffer.
pub fn position_of(text: &str, offset: usize) -> lsp_types::Position {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    lsp_types::Position::new(line, text[start..offset].chars().count() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position as P;

    #[test]
    fn crlf_round_trip_keeps_bytes() {
        let raw = "a\r\nb\r\n\r\nc";
        let loaded = decode(raw.as_bytes().to_vec(), raw.len() as u64).unwrap();
        assert_eq!(loaded.ending, LineEnding::CrLf);
        assert_eq!(loaded.text, "a\nb\n\nc");
        assert_eq!(encode(&loaded.text, loaded.ending, loaded.had_bom), raw.as_bytes());
        let lf = "x\ny\n";
        let l = decode(lf.as_bytes().to_vec(), 4).unwrap();
        assert_eq!(l.ending, LineEnding::Lf);
        assert_eq!(encode(&l.text, l.ending, false), lf.as_bytes());
    }

    #[test]
    fn bom_and_binary() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(b"hi\r\n");
        let l = decode(b.clone(), b.len() as u64).unwrap();
        assert!(l.had_bom);
        assert_eq!(l.text, "hi\n");
        assert_eq!(encode(&l.text, l.ending, true), b);
        assert_eq!(decode(vec![b'a', 0, b'b'], 3).unwrap_err(), LoadError::Binary);
    }

    #[test]
    fn size_guard_flags() {
        let l = decode(b"x".to_vec(), LARGE_FILE_BYTES + 1).unwrap();
        assert!(l.large);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(matches!(load(&p), Err(LoadError::TooLarge(_))));
    }

    #[test]
    fn utf16_columns_with_astral_and_hangul() {
        // '😀' is 2 UTF-16 units / 4 UTF-8 bytes, '한' is 1 unit / 3 bytes.
        let line = "a😀한b";
        assert_eq!(char_col_to_wire(line, 3, PosEncoding::Utf16), 4);
        assert_eq!(char_col_to_wire(line, 3, PosEncoding::Utf8), 8);
        assert_eq!(char_col_to_wire(line, 3, PosEncoding::Utf32), 3);
        assert_eq!(wire_col_to_char(line, 4, PosEncoding::Utf16), 3);
        assert_eq!(wire_col_to_char(line, 8, PosEncoding::Utf8), 3);
        assert_eq!(wire_col_to_char(line, 99, PosEncoding::Utf16), 4);
        let text = "fn x() {}\nlet s = \"😀\"; s.len()\n";
        let p = P::new(1, 13);
        let w = pos_to_wire(text, p, PosEncoding::Utf16);
        assert_eq!(w, P::new(1, 14));
        assert_eq!(pos_from_wire(text, w, PosEncoding::Utf16), p);
    }

    #[test]
    fn offsets_and_positions() {
        let text = "ab\n한c\n";
        assert_eq!(offset_of(text, P::new(1, 1)), 3 + 3);
        assert_eq!(position_of(text, 6), P::new(1, 1));
        assert_eq!(offset_of(text, P::new(9, 0)), text.len());
        assert_eq!(line_of("a\r\nb", 0), "a");
    }
}
