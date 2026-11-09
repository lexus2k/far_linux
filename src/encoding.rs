//! Text encodings offered by the viewer and editor.

use encoding_rs::{Encoding, IBM866, KOI8_R, WINDOWS_1251, WINDOWS_1252};

/// The encodings offered by the viewer and editor. Index 0 is UTF-8; the
/// first `BYTE_ENCODINGS` are cycled with F8, and the editor also offers
/// UTF-16 on Shift+F8.
pub const NAMES: [&str; 7] = [
    "UTF-8", "CP1251", "KOI8-R", "CP866", "CP1252", "UTF-16LE", "UTF-16BE",
];

/// UTF-8 and the single-byte encodings, which the viewer reads line by line.
pub const BYTE_ENCODINGS: usize = 5;
pub const UTF16LE: usize = 5;
pub const UTF16BE: usize = 6;

pub fn is_utf16_index(index: usize) -> bool {
    index == UTF16LE || index == UTF16BE
}

/// The signature a file in this encoding starts with. Single-byte files
/// keep a UTF-8 one, as they did before UTF-16 was offered.
pub fn bom(index: usize) -> &'static [u8] {
    match index {
        UTF16LE => &[0xff, 0xfe],
        UTF16BE => &[0xfe, 0xff],
        _ => &[0xef, 0xbb, 0xbf],
    }
}

fn single_byte(index: usize) -> &'static Encoding {
    match index {
        1 => WINDOWS_1251,
        2 => KOI8_R,
        3 => IBM866,
        _ => WINDOWS_1252,
    }
}

/// Decodes bytes. Returns `None` for UTF-8 input that is not valid UTF-8.
pub fn decode(bytes: &[u8], index: usize) -> Option<String> {
    if is_utf16_index(index) {
        // An odd byte or an unpaired surrogate could not be saved back as it
        // was, so such text is not UTF-16.
        if bytes.len() % 2 != 0 {
            return None;
        }
        let units = bytes.chunks_exact(2).map(|p| {
            let pair = [p[0], p[1]];
            if index == UTF16LE {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        });
        return char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .ok();
    }
    if index == 0 {
        return std::str::from_utf8(bytes).ok().map(str::to_owned);
    }
    let (text, _) = single_byte(index).decode_without_bom_handling(bytes);
    Some(text.into_owned())
}

/// Encodes text. Returns `None` if a character has no representation, so
/// the caller can refuse to save rather than silently change the text.
pub fn encode(text: &str, index: usize) -> Option<Vec<u8>> {
    if is_utf16_index(index) {
        let mut out = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            if index == UTF16LE {
                out.extend_from_slice(&unit.to_le_bytes());
            } else {
                out.extend_from_slice(&unit.to_be_bytes());
            }
        }
        return Some(out);
    }
    if index == 0 {
        return Some(text.as_bytes().to_vec());
    }
    let (bytes, _, had_errors) = single_byte(index).encode(text);
    if had_errors {
        None
    } else {
        Some(bytes.into_owned())
    }
}

/// Maps each byte of a single-byte encoding to a character.
pub fn table(index: usize) -> [char; 256] {
    let mut out = ['?'; 256];
    for (b, slot) in out.iter_mut().enumerate() {
        let byte = [b as u8];
        let (text, _) = single_byte(index).decode_without_bom_handling(&byte);
        *slot = text.chars().next().unwrap_or('?');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_round_trips_in_single_byte_encodings() {
        let all: Vec<u8> = (0..=255).collect();
        for (index, name) in NAMES.iter().enumerate().take(BYTE_ENCODINGS).skip(1) {
            let text = decode(&all, index).unwrap();
            assert_eq!(encode(&text, index).unwrap(), all, "{name}");
        }
    }

    #[test]
    fn invalid_utf8_is_reported() {
        assert!(decode(&[0xff, 0x41], 0).is_none());
        assert_eq!(decode("ж".as_bytes(), 0).unwrap(), "ж");
    }

    #[test]
    fn unmappable_characters_are_refused() {
        assert!(encode("日本語", 1).is_none());
        assert_eq!(encode("Ж", 1).unwrap(), vec![0xc6]);
    }

    #[test]
    fn utf16_round_trips_in_both_byte_orders() {
        let text = "Far ж 日本 🦀\r\n";
        for index in [UTF16LE, UTF16BE] {
            let bytes = encode(text, index).unwrap();
            assert_eq!(bytes.len(), text.encode_utf16().count() * 2);
            assert_eq!(decode(&bytes, index).unwrap(), text);
        }
        assert_eq!(encode("A", UTF16LE).unwrap(), [0x41, 0]);
        assert_eq!(encode("A", UTF16BE).unwrap(), [0, 0x41]);
    }

    #[test]
    fn broken_utf16_is_reported() {
        assert!(decode(&[0x41, 0, 0x42], UTF16LE).is_none());
        // A lone high surrogate.
        assert!(decode(&[0x00, 0xd8, 0x41, 0x00], UTF16LE).is_none());
    }
}
