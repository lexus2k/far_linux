//! Text encodings offered by the viewer and editor.

use encoding_rs::{Encoding, IBM866, KOI8_R, WINDOWS_1251, WINDOWS_1252};

/// The encodings cycled with F8. Index 0 is UTF-8.
pub const NAMES: [&str; 5] = ["UTF-8", "CP1251", "KOI8-R", "CP866", "CP1252"];

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
    if index == 0 {
        return std::str::from_utf8(bytes).ok().map(str::to_owned);
    }
    let (text, _) = single_byte(index).decode_without_bom_handling(bytes);
    Some(text.into_owned())
}

/// Encodes text. Returns `None` if a character has no representation, so
/// the caller can refuse to save rather than silently change the text.
pub fn encode(text: &str, index: usize) -> Option<Vec<u8>> {
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

/// Returns true if the bytes start with a UTF-16 byte order mark.
pub fn is_utf16(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_round_trips_in_single_byte_encodings() {
        let all: Vec<u8> = (0..=255).collect();
        for index in 1..NAMES.len() {
            let text = decode(&all, index).unwrap();
            assert_eq!(encode(&text, index).unwrap(), all, "{}", NAMES[index]);
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
}
