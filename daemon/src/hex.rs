//! Hex text, in the one form that the whole daemon writes.

use std::fmt::Write as _;

/// Writes bytes as lower case hex, two digits for each byte.
pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        // A `String` takes every character, so this write cannot fail.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_byte_takes_two_digits() {
        assert_eq!(encode(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn no_byte_gives_no_text() {
        assert_eq!(encode(&[]), "");
    }

    #[test]
    fn the_digits_are_lower_case() {
        assert_eq!(encode(&[0xab, 0xcd, 0xef]), "abcdef");
    }

    #[test]
    fn the_bytes_keep_their_order() {
        assert_eq!(encode(&[1, 2, 3]), "010203");
    }
}
