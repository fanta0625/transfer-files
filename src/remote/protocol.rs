//! UTF-16LE command encoding used by Windows adapter tests.
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[cfg(windows)]
pub fn encode_utf16le(value: &str) -> String {
    STANDARD.encode(
        value
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}
