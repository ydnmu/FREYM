//! Preserve keypad identity in raw keystrokes; key_char still feeds normal text input.
pub(crate) fn key_name(vk: u16) -> Option<&'static str> {
    Some(match vk {
        0x60 => "numpad0",
        0x61 => "numpad1",
        0x62 => "numpad2",
        0x63 => "numpad3",
        0x64 => "numpad4",
        0x65 => "numpad5",
        0x66 => "numpad6",
        0x67 => "numpad7",
        0x68 => "numpad8",
        0x69 => "numpad9",
        0x6a => "numpadmultiply",
        0x6b => "numpadadd",
        0x6c => "numpadcomma",
        0x6d => "numpadsubtract",
        0x6e => "numpaddecimal",
        0x6f => "numpaddivide",
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keypad_identity_never_aliases_the_number_row_or_punctuation() {
        for vk in 0x60..=0x69 {
            assert!(key_name(vk).unwrap().starts_with("numpad"));
        }
        assert_eq!(key_name(0x31), None);
        assert_eq!(key_name(0xbb), None);
        assert_eq!(key_name(0x6b), Some("numpadadd"));
        assert_eq!(key_name(0x6e), Some("numpaddecimal"));
        // RegisterHotKey cannot distinguish keypad Enter from Return; leave normal
        // Enter behavior intact instead of fabricating a separate global identity.
        assert_eq!(key_name(0x0d), None);
    }
}
