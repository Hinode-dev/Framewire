//! Room code and viewer ID generation.

/// Uppercase alphanumeric characters, excluding easily confused ones (0/O, 1/I/L).
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Generates a 6-character room code.
pub fn generate() -> String {
    (0..6)
        .map(|_| {
            let idx = rand::random_range(0..ALPHABET.len());
            ALPHABET[idx] as char
        })
        .collect()
}

/// Internal identifier used to distinguish viewers on the host side.
/// Never shown to users, so it doesn't need the room-code alphabet's
/// readability constraints.
const VIEWER_ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub fn generate_viewer_id() -> String {
    (0..10)
        .map(|_| {
            let idx = rand::random_range(0..VIEWER_ID_ALPHABET.len());
            VIEWER_ID_ALPHABET[idx] as char
        })
        .collect()
}
