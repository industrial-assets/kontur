//! Pure, unit-testable helpers for in-console plan editing.
//!
//! None of these functions perform I/O.  The TUI loop drives mutations through
//! these helpers and sends the resulting list to the server via `client.edit_plan`.

/// Replace the task at `idx` with `text`. Returns the list unchanged when
/// `idx` is out of range.
pub fn edit_task(mut list: Vec<String>, idx: usize, text: String) -> Vec<String> {
    if idx < list.len() {
        list[idx] = text;
    }
    list
}

/// Remove the task at `idx`.
///
/// Returns `Err` if deleting would empty the list — a plan needs at least one
/// task; operators must abandon the session instead of deleting all tasks.
pub fn delete_task(list: Vec<String>, idx: usize) -> Result<Vec<String>, &'static str> {
    if list.len() <= 1 {
        return Err("a plan needs at least one task — abandon instead");
    }
    let mut out = list;
    if idx < out.len() {
        out.remove(idx);
    }
    Ok(out)
}

/// Move the task at `idx` up (`up = true`) or down (`up = false`) by one
/// position. Returns `(new_list, new_selection_idx)`. If the move is already
/// at the boundary the list and index are returned unchanged.
pub fn move_task(mut list: Vec<String>, idx: usize, up: bool) -> (Vec<String>, usize) {
    if list.is_empty() {
        return (list, 0);
    }
    if up && idx > 0 {
        list.swap(idx - 1, idx);
        (list, idx - 1)
    } else if !up && idx + 1 < list.len() {
        list.swap(idx, idx + 1);
        (list, idx + 1)
    } else {
        (list, idx)
    }
}

/// Clamp `idx` so it stays within `[0, len - 1]`. Returns `0` when `len == 0`.
pub fn clamp_sel(idx: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    idx.min(len - 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- edit_task ---

    #[test]
    fn edit_task_replaces_at_idx() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let out = edit_task(list, 1, "B".into());
        assert_eq!(out, vec!["a", "B", "c"]);
    }

    #[test]
    fn edit_task_out_of_range_is_noop() {
        let list = vec!["a".into(), "b".into()];
        let out = edit_task(list.clone(), 5, "z".into());
        assert_eq!(out, list);
    }

    #[test]
    fn edit_task_first_element() {
        let list = vec!["first".into(), "second".into()];
        let out = edit_task(list, 0, "FIRST".into());
        assert_eq!(out[0], "FIRST");
    }

    #[test]
    fn edit_task_last_element() {
        let list = vec!["a".into(), "b".into(), "last".into()];
        let out = edit_task(list, 2, "LAST".into());
        assert_eq!(out[2], "LAST");
    }

    // --- delete_task ---

    #[test]
    fn delete_task_removes_middle() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let out = delete_task(list, 1).unwrap();
        assert_eq!(out, vec!["a", "c"]);
    }

    #[test]
    fn delete_task_removes_first() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let out = delete_task(list, 0).unwrap();
        assert_eq!(out, vec!["b", "c"]);
    }

    #[test]
    fn delete_task_removes_last() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let out = delete_task(list, 2).unwrap();
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn delete_task_single_item_errors() {
        let list = vec!["only".into()];
        let err = delete_task(list, 0).unwrap_err();
        assert!(err.contains("at least one task"), "error message unexpected: {err}");
    }

    #[test]
    fn delete_task_out_of_range_is_noop() {
        let list = vec!["a".into(), "b".into()];
        let out = delete_task(list.clone(), 99).unwrap();
        assert_eq!(out, list);
    }

    // --- move_task ---

    #[test]
    fn move_task_up_swaps_with_predecessor() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let (out, new_idx) = move_task(list, 1, true);
        assert_eq!(out, vec!["b", "a", "c"]);
        assert_eq!(new_idx, 0);
    }

    #[test]
    fn move_task_down_swaps_with_successor() {
        let list = vec!["a".into(), "b".into(), "c".into()];
        let (out, new_idx) = move_task(list, 1, false);
        assert_eq!(out, vec!["a", "c", "b"]);
        assert_eq!(new_idx, 2);
    }

    #[test]
    fn move_task_up_at_boundary_is_noop() {
        let list = vec!["a".into(), "b".into()];
        let (out, new_idx) = move_task(list.clone(), 0, true);
        assert_eq!(out, list);
        assert_eq!(new_idx, 0);
    }

    #[test]
    fn move_task_down_at_boundary_is_noop() {
        let list = vec!["a".into(), "b".into()];
        let (out, new_idx) = move_task(list.clone(), 1, false);
        assert_eq!(out, list);
        assert_eq!(new_idx, 1);
    }

    #[test]
    fn move_task_empty_list() {
        let (out, new_idx) = move_task(vec![], 0, true);
        assert!(out.is_empty());
        assert_eq!(new_idx, 0);
    }

    // --- clamp_sel ---

    #[test]
    fn clamp_sel_within_range() {
        assert_eq!(clamp_sel(2, 5), 2);
    }

    #[test]
    fn clamp_sel_at_last() {
        assert_eq!(clamp_sel(4, 5), 4);
    }

    #[test]
    fn clamp_sel_over_range_clamps_to_last() {
        assert_eq!(clamp_sel(10, 5), 4);
    }

    #[test]
    fn clamp_sel_zero_len_returns_zero() {
        assert_eq!(clamp_sel(0, 0), 0);
        assert_eq!(clamp_sel(5, 0), 0);
    }
}
