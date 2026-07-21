//! Cursor-aware compose-buffer editing. The cursor is a **char index**
//! (0..=char count); all ops are multibyte-safe. Pure; tested.

/// Byte offset of a char index (clamped to the end).
fn byte_at(buf: &str, cursor: usize) -> usize {
    buf.char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(buf.len())
}

/// Number of chars in the buffer.
pub fn char_len(buf: &str) -> usize {
    buf.chars().count()
}

/// Insert one char at the cursor; returns the new cursor.
pub fn insert_char(buf: &mut String, cursor: usize, c: char) -> usize {
    buf.insert(byte_at(buf, cursor), c);
    cursor + 1
}

/// Insert a string at the cursor; returns the new cursor.
pub fn insert_str(buf: &mut String, cursor: usize, s: &str) -> usize {
    buf.insert_str(byte_at(buf, cursor), s);
    cursor + s.chars().count()
}

/// Delete the char before the cursor; returns the new cursor.
pub fn backspace(buf: &mut String, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let start = byte_at(buf, cursor - 1);
    let end = byte_at(buf, cursor);
    buf.replace_range(start..end, "");
    cursor - 1
}

pub fn left(cursor: usize) -> usize {
    cursor.saturating_sub(1)
}

pub fn right(buf: &str, cursor: usize) -> usize {
    (cursor + 1).min(char_len(buf))
}

pub fn home() -> usize {
    0
}

pub fn end(buf: &str) -> usize {
    char_len(buf)
}

/// The buffer with a visible cursor marker inserted at the cursor position,
/// for display only.
pub fn with_cursor_marker(buf: &str, cursor: usize) -> String {
    let mut out = buf.to_owned();
    out.insert(byte_at(buf, cursor), '▏');
    out
}

/// Single-line rendering of a possibly multi-line buffer (notice row):
/// newlines become a visible ⏎ so nothing is silently hidden.
pub fn inline(buf: &str) -> String {
    buf.replace('\n', "⏎")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_mid_text() {
        let mut b = String::from("ac");
        let cur = insert_char(&mut b, 1, 'b');
        assert_eq!((b.as_str(), cur), ("abc", 2));
        let mut b = String::from("abc");
        let cur = backspace(&mut b, 2);
        assert_eq!((b.as_str(), cur), ("ac", 1));
        assert_eq!(backspace(&mut b, 0), 0);
        assert_eq!(b, "ac");
    }

    #[test]
    fn multibyte_safe() {
        let mut b = String::from("КУ");
        let cur = insert_char(&mut b, 1, 'О');
        assert_eq!((b.as_str(), cur), ("КОУ", 2));
        let cur = backspace(&mut b, 3);
        assert_eq!((b.as_str(), cur), ("КО", 2));
        let mut b = String::from("К");
        let cur = insert_str(&mut b, 1, "ОНТУР");
        assert_eq!((b.as_str(), cur), ("КОНТУР", 6));
    }

    #[test]
    fn paste_inserts_verbatim_with_newlines() {
        let mut b = String::from("ab");
        let cur = insert_str(&mut b, 1, "x\ny");
        assert_eq!((b.as_str(), cur), ("ax\nyb", 4));
    }

    #[test]
    fn movement_clamps() {
        let b = "ab";
        assert_eq!(left(0), 0);
        assert_eq!(right(b, 2), 2);
        assert_eq!(home(), 0);
        assert_eq!(end(b), 2);
    }

    #[test]
    fn marker_and_inline_render() {
        assert_eq!(with_cursor_marker("ab", 1), "a▏b");
        assert_eq!(with_cursor_marker("аб", 2), "аб▏");
        assert_eq!(inline("a\nb"), "a⏎b");
    }
}
