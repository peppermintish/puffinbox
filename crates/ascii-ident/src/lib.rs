//! Identifier checks for the ASCII source identifiers used by this workspace.
//!
//! This is an independent build adapter, not a Unicode XID implementation.
//! Non-ASCII identifiers are rejected. Runtime text is unaffected.
#![no_std]
#![forbid(unsafe_code)]

pub const fn is_xid_start(character: char) -> bool {
    character.is_ascii_alphabetic()
}

pub const fn is_xid_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ascii_identifier_characters_only() {
        for byte in 0_u8..=127 {
            let character = char::from(byte);
            assert_eq!(is_xid_start(character), character.is_ascii_alphabetic());
            assert_eq!(
                is_xid_continue(character),
                character.is_ascii_alphanumeric() || character == '_'
            );
        }
        for character in ['é', '中', '\u{301}', '💻'] {
            assert!(!is_xid_start(character));
            assert!(!is_xid_continue(character));
        }
    }
}
