//! Unified-diff text helpers shared by the server (per-file wire caps) and
//! the TUI (per-file viewing).

/// One file's section of a unified diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub diff: String,
}

/// Split a unified diff into per-file sections.
///
/// Sections start at `diff --git` lines. The path is taken from the section's
/// `+++ b/<path>` line; for deletions (`+++ /dev/null`) it falls back to the
/// `--- a/<path>` line. Text before the first `diff --git` line (if any) is
/// dropped — git does not emit any.
pub fn split_file_diffs(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(section) = current.take() {
                out.push(section_to_file_diff(section));
            }
            current = Some(String::new());
        }
        if let Some(ref mut section) = current {
            section.push_str(line);
            section.push('\n');
        }
    }
    if let Some(section) = current {
        out.push(section_to_file_diff(section));
    }
    out
}

fn section_to_file_diff(section: String) -> FileDiff {
    let path = section_path(&section).unwrap_or_else(|| "?".to_owned());
    FileDiff {
        path,
        diff: section,
    }
}

fn section_path(section: &str) -> Option<String> {
    let strip = |rest: &str| {
        let p = rest
            .strip_prefix("b/")
            .or_else(|| rest.strip_prefix("a/"))
            .unwrap_or(rest);
        (p != "/dev/null" && !p.is_empty()).then(|| p.to_owned())
    };
    // Prefer the post-image path; fall back to the pre-image path (deletions).
    section
        .lines()
        .find_map(|l| l.strip_prefix("+++ ").and_then(strip))
        .or_else(|| {
            section
                .lines()
                .find_map(|l| l.strip_prefix("--- ").and_then(strip))
        })
}

/// Like [`split_file_diffs`], but if the text carries no `diff --git`
/// markers (the in-memory workspace freezes a canonical `path\0contents`
/// encoding, not a unified diff) the whole text becomes one section under
/// `fallback_path`, so it still reaches the operator instead of vanishing.
pub fn split_file_diffs_or_whole(diff: &str, fallback_path: &str) -> Vec<FileDiff> {
    let sections = split_file_diffs(diff);
    if sections.is_empty() && !diff.is_empty() {
        return vec![FileDiff {
            path: fallback_path.to_owned(),
            diff: diff.to_owned(),
        }];
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = "diff --git a/src/a.rs b/src/a.rs\n\
index 111..222 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n\
diff --git a/package-lock.json b/package-lock.json\n\
index 333..444 100644\n\
--- a/package-lock.json\n\
+++ b/package-lock.json\n\
@@ -1 +1,2 @@\n\
 {}\n\
+bulk\n";

    #[test]
    fn splits_into_one_section_per_file() {
        let sections = split_file_diffs(TWO_FILES);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].path, "src/a.rs");
        assert_eq!(sections[1].path, "package-lock.json");
        assert!(sections[0].diff.starts_with("diff --git a/src/a.rs"));
        assert!(sections[0].diff.contains("+new"));
        assert!(!sections[0].diff.contains("bulk"));
        assert!(sections[1].diff.contains("+bulk"));
    }

    #[test]
    fn deletion_falls_back_to_pre_image_path() {
        let d = "diff --git a/gone.rs b/gone.rs\n\
deleted file mode 100644\n\
--- a/gone.rs\n\
+++ /dev/null\n\
@@ -1 +0,0 @@\n\
-fn gone() {}\n";
        let sections = split_file_diffs(d);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "gone.rs");
    }

    #[test]
    fn empty_diff_yields_no_sections() {
        assert!(split_file_diffs("").is_empty());
        assert!(split_file_diffs_or_whole("", "x").is_empty());
    }

    #[test]
    fn non_unified_text_falls_back_to_one_whole_section() {
        let sections = split_file_diffs_or_whole("src/guard.rs\0contents\n", "src/guard.rs");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "src/guard.rs");
        assert!(sections[0].diff.contains("contents"));
    }

    #[test]
    fn new_file_uses_post_image_path() {
        let d = "diff --git a/fresh.rs b/fresh.rs\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/fresh.rs\n\
@@ -0,0 +1 @@\n\
+fn fresh() {}\n";
        let sections = split_file_diffs(d);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "fresh.rs");
    }
}
