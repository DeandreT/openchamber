//! COMPOUND_TEXT encoding and strict decoding for UTF-8, Latin-1, GB2312 and JIS X0208.

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use std::io::{self, Write};

const UTF8_START: &[u8] = &[0x1B, 0x25, 0x47];
const UTF8_END: &[u8] = &[0x1B, 0x25, 0x40];

/// Wrapper for reduce allocation
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CText<'s> {
    utf8: &'s str,
}

impl<'s> fmt::Debug for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> fmt::Display for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> CText<'s> {
    pub const fn new(utf8: &'s str) -> Self {
        Self { utf8 }
    }

    pub const fn len(self) -> usize {
        self.utf8.len() + UTF8_START.len() + UTF8_END.len()
    }

    #[cfg(feature = "std")]
    pub fn write(self, mut out: impl Write) -> io::Result<usize> {
        let mut writed = 0;
        writed += out.write(UTF8_START)?;
        writed += out.write(self.utf8.as_bytes())?;
        writed += out.write(UTF8_END)?;
        Ok(writed)
    }
}

/// Encoding utf8 to COMPOUND_TEXT with utf8 escape
pub fn utf8_to_compound_text(text: &str) -> Vec<u8> {
    let mut ret = Vec::with_capacity(text.len() + 6);
    ret.extend_from_slice(UTF8_START);
    ret.extend_from_slice(text.as_bytes());
    ret.extend_from_slice(UTF8_END);
    ret
}

#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidEncoding,
    UnsupportedEncoding,
    Utf8Error(alloc::string::FromUtf8Error),
}

impl From<alloc::string::FromUtf8Error> for DecodeError {
    fn from(err: alloc::string::FromUtf8Error) -> Self {
        DecodeError::Utf8Error(err)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "Invalid compound text"),
            Self::UnsupportedEncoding => write!(f, "This encoding is not supported yet"),
            Self::Utf8Error(e) => write!(f, "Not a valid utf8 {}", e),
        }
    }
}

#[derive(Clone, Copy)]
enum Charset {
    Ascii,
    Latin1,
    Gb2312,
    Jis0208,
}

// GBK includes unassigned GB2312 cells and has different mappings for 2 symbols.
// Restrict its decoder to the GB2312 repertoire before using its mapping tables.
fn gb2312_assigned(first: u8, second: u8) -> bool {
    match first {
        0xa1 | 0xa3 | 0xb0..=0xd6 | 0xd8..=0xf7 => true,
        0xa2 => matches!(second, 0xb1..=0xe2 | 0xe5..=0xee | 0xf1..=0xfc),
        0xa4 => second <= 0xf3,
        0xa5 => second <= 0xf6,
        0xa6 => matches!(second, 0xa1..=0xb8 | 0xc1..=0xd8),
        0xa7 => matches!(second, 0xa1..=0xc1 | 0xd1..=0xf1),
        0xa8 => matches!(second, 0xa1..=0xba | 0xc5..=0xe9),
        0xa9 => matches!(second, 0xa4..=0xef),
        0xd7 => second <= 0xf9,
        _ => false,
    }
}

/// Decode the supported COMPOUND_TEXT character sets without replacing invalid text.
/// Unknown designations return `UnsupportedEncoding`; malformed sequences return an error.
pub fn compound_text_to_utf8(mut bytes: &[u8]) -> Result<String, DecodeError> {
    let mut out = String::new();
    let mut gl = Charset::Ascii;
    let mut gr = Charset::Latin1;

    while let Some(&first) = bytes.first() {
        if bytes.starts_with(UTF8_START) {
            bytes = &bytes[UTF8_START.len()..];
            let end = bytes
                .iter()
                .position(|&byte| byte == 0x1b)
                .unwrap_or(bytes.len());
            out.push_str(&String::from_utf8(bytes[..end].to_vec())?);
            bytes = &bytes[end..];
            if !bytes.is_empty() {
                if !bytes.starts_with(UTF8_END) {
                    return Err(DecodeError::InvalidEncoding);
                }
                bytes = &bytes[UTF8_END.len()..];
            }
            continue;
        }

        if first == 0x1b {
            // Designations change GL and GR independently and can occur after text.
            let (target, charset, length) = match bytes {
                [0x1b, b'(', b'B', ..] => (&mut gl, Charset::Ascii, 3),
                [0x1b, b')', b'B', ..] => (&mut gr, Charset::Ascii, 3),
                [0x1b, b'-', b'A', ..] => (&mut gr, Charset::Latin1, 3),
                [0x1b, b'$', side @ (b'(' | b')'), name, ..] => {
                    let charset = match name {
                        b'A' => Charset::Gb2312,
                        b'B' => Charset::Jis0208,
                        _ => return Err(DecodeError::UnsupportedEncoding),
                    };
                    (if *side == b'(' { &mut gl } else { &mut gr }, charset, 4)
                }
                _ if bytes.len() < 3 || bytes == b"\x1b$(" || bytes == b"\x1b$)" => {
                    return Err(DecodeError::InvalidEncoding);
                }
                _ => return Err(DecodeError::UnsupportedEncoding),
            };
            *target = charset;
            bytes = &bytes[length..];
            continue;
        }

        // TAB and NEWLINE are the only literal COMPOUND_TEXT control characters.
        if matches!(first, b'\t' | b'\n' | b' ') {
            out.push(char::from(first));
            bytes = &bytes[1..];
            continue;
        }
        if first < 0x20 || (0x7f..0xa0).contains(&first) {
            return Err(DecodeError::InvalidEncoding);
        }

        let high = first >= 0x80;
        let charset = if high { gr } else { gl };
        match charset {
            Charset::Latin1 => {
                out.push(char::from(first));
                bytes = &bytes[1..];
            }
            Charset::Ascii => {
                let ascii = first & 0x7f;
                if !(0x21..=0x7e).contains(&ascii) {
                    return Err(DecodeError::InvalidEncoding);
                }
                out.push(char::from(ascii));
                bytes = &bytes[1..];
            }
            Charset::Gb2312 | Charset::Jis0208 => {
                let second = *bytes.get(1).ok_or(DecodeError::InvalidEncoding)?;
                let range = if high { 0xa1..=0xfe } else { 0x21..=0x7e };
                if !range.contains(&first) || !range.contains(&second) {
                    return Err(DecodeError::InvalidEncoding);
                }
                let pair = [first | 0x80, second | 0x80];
                let encoding = match charset {
                    Charset::Gb2312 => {
                        if !gb2312_assigned(pair[0], pair[1]) {
                            return Err(DecodeError::InvalidEncoding);
                        }
                        match pair {
                            [0xa1, 0xa4] => {
                                out.push('\u{30fb}');
                                bytes = &bytes[2..];
                                continue;
                            }
                            [0xa1, 0xaa] => {
                                out.push('\u{2015}');
                                bytes = &bytes[2..];
                                continue;
                            }
                            _ => encoding_rs::GBK,
                        }
                    }
                    _ => encoding_rs::EUC_JP,
                };
                let text = encoding
                    .decode_without_bom_handling_and_without_replacement(&pair)
                    .ok_or(DecodeError::InvalidEncoding)?;
                out.push_str(&text);
                bytes = &bytes[2..];
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_designations_and_ascii_transitions() {
        // Synthetic IBus/libpinyin PreeditDraw after the first key in "nihao".
        assert_eq!(
            compound_text_to_utf8(&[27, 36, 40, 65, 68, 106]).unwrap(),
            "年"
        );
        assert_eq!(compound_text_to_utf8(b"\x1b$(ADc:C").unwrap(), "你好");
        assert_eq!(
            compound_text_to_utf8(b"\x1b$)A\xc4\xe3\xba\xc3").unwrap(),
            "你好"
        );
        assert_eq!(
            compound_text_to_utf8(b"before \x1b$(ADc:C\x1b(B after").unwrap(),
            "before 你好 after"
        );
        assert_eq!(
            compound_text_to_utf8(b"\x1b$)Ahello \xc4\xe3\xba\xc3!").unwrap(),
            "hello 你好!"
        );
        assert_eq!(compound_text_to_utf8(b"\x1b$(A!$!*").unwrap(), "・―");
    }

    #[test]
    fn latin1_is_the_default_and_designations_are_independent() {
        assert_eq!(compound_text_to_utf8(b"caf\xe9\t\n").unwrap(), "café\t\n");
        assert_eq!(
            compound_text_to_utf8(b"\x1b$(ADc\xe9\x1b(B!").unwrap(),
            "你é!"
        );
    }

    #[test]
    fn utf8_can_end_at_input_end_or_return_to_compound_text() {
        for text in ["", "a", "你好", "가나다", "🙂 café"] {
            let encoded = utf8_to_compound_text(text);
            assert_eq!(compound_text_to_utf8(&encoded).unwrap(), text);
            assert_eq!(
                compound_text_to_utf8(&encoded[..encoded.len() - 3]).unwrap(),
                text
            );
        }
        assert_eq!(
            compound_text_to_utf8(b"prefix \x1b%G\xf0\x9f\x99\x82\x1b%@\xe9").unwrap(),
            "prefix 🙂é"
        );
    }

    #[test]
    fn invalid_text_is_an_error_without_partial_output_or_replacement() {
        let invalid: &[&[u8]] = &[
            b"\x1b",
            b"\x1b%",
            b"\x1b$(",
            b"\x1b$)",
            b"\x1b%G\xff\x1b%@",
            b"\x1b%Ga\x1b",
            b"\x1b%Ga\x1b%X",
            b"\x1b$(AD",
            b"\x1b$(AD\xff",
            b"\x1b$)A\xc4c",
            b"\x1b$(A!!D",
            b"\x1b$(A\x7f!",
            b"\x1b$(A*$", // Unassigned GB2312 cell, although GBK decodes it.
            b"\x1b$(B!!D",
            b"\x1b$(B\x7e\x7e",
            b"\0",
            b"hello\x01",
            b"\x9b1]hello",
        ];
        for bytes in invalid {
            assert!(
                compound_text_to_utf8(bytes).is_err(),
                "accepted invalid bytes {:?}",
                bytes
            );
        }
        assert!(matches!(
            compound_text_to_utf8(b"\x1b$(C!!"),
            Err(DecodeError::UnsupportedEncoding)
        ));
    }

    #[test]
    fn arbitrary_short_sequences_never_panic() {
        for first in 0..=255u8 {
            for second in 0..=255u8 {
                let _ = compound_text_to_utf8(&[first, second]);
                let _ = compound_text_to_utf8(&[0x1b, b'$', b'(', b'A', first, second]);
                let _ = compound_text_to_utf8(&[0x1b, b'%', b'G', first, second]);
            }
        }
    }

    #[test]
    fn gb2312_repertoire_matches_the_independent_unicode_mapping() {
        // Oracle generated with Python's gb2312 codec over all 94 x 94 cells.
        // FNV-1a covers each cell's 2 bytes followed by UTF-8, or 0xff if unassigned.
        let mut fingerprint = 0xcbf29ce484222325u64;
        let mut assigned = 0;
        for first in 0xa1..=0xfe {
            for second in 0xa1..=0xfe {
                for byte in [first, second] {
                    fingerprint = (fingerprint ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
                match compound_text_to_utf8(&[0x1b, b'$', b')', b'A', first, second]) {
                    Ok(text) => {
                        assigned += 1;
                        for byte in text.bytes() {
                            fingerprint =
                                (fingerprint ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                        }
                    }
                    Err(DecodeError::InvalidEncoding) => {
                        fingerprint = (fingerprint ^ 0xff).wrapping_mul(0x100000001b3);
                    }
                    Err(error) => panic!("unexpected GB2312 error: {}", error),
                }
            }
        }
        assert_eq!(assigned, 7445);
        assert_eq!(
            fingerprint, 0x44756e02aaa1887f,
            "GB2312 Unicode mapping changed"
        );
    }

    #[test]
    fn korean() {
        const UTF8: &str = "가나다";
        const COMP: &[u8] = &[
            27, 37, 71, 234, 176, 128, 235, 130, 152, 235, 139, 164, 27, 37, 64,
        ];
        assert_eq!(crate::utf8_to_compound_text(UTF8), COMP);
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    #[test]
    fn iso_2011_jp() {
        const UTF8: &str = "東京";
        const COMP: &[u8] = &[27, 36, 40, 66, 69, 108, 53, 126];
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }
}
