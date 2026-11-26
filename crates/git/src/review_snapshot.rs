use std::collections::BTreeSet;
use std::fmt::Write;

pub(super) const MAX_BYTES: usize = 64 * 1024;
pub(super) const MAX_PARTS: usize = 32;

pub(super) struct Part<'a> {
    pub workspace: &'a str,
    pub path: &'a str,
    pub staged: bool,
    pub diff: &'a str,
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewSnapshotError {
    EmptySelection,
    WrongWorkspace,
    DuplicatePart(usize),
    InvalidPath(usize),
    Truncated(usize),
    UnsupportedDiff(usize),
    TooLarge,
}

pub struct ReviewSnapshot(String);

impl ReviewSnapshot {
    pub fn text(&self) -> &str {
        &self.0
    }

    pub(super) fn freeze<'a>(
        workspace: &str,
        parts: impl IntoIterator<Item = Part<'a>>,
    ) -> Result<Self, ReviewSnapshotError> {
        let mut text = String::new();
        let mut seen = BTreeSet::new();
        for (index, part) in parts.into_iter().enumerate() {
            if part.workspace != workspace {
                return Err(ReviewSnapshotError::WrongWorkspace);
            }
            if index >= MAX_PARTS || part.path.len() > MAX_BYTES || part.diff.len() > MAX_BYTES {
                return Err(ReviewSnapshotError::TooLarge);
            }
            if part.path.is_empty() || part.path.chars().any(char::is_control) {
                return Err(ReviewSnapshotError::InvalidPath(index));
            }
            if !seen.insert((part.path, part.staged)) {
                return Err(ReviewSnapshotError::DuplicatePart(index));
            }
            if part.truncated {
                return Err(ReviewSnapshotError::Truncated(index));
            }
            if !single_text_patch(part.diff, part.path) {
                return Err(ReviewSnapshotError::UnsupportedDiff(index));
            }
            let side = if part.staged { "staged" } else { "unstaged" };
            let header = format!("[{side}] {}\n", part.path);
            let separator = if index == 0 { "" } else { "\n\n" };
            if text.len() + separator.len() + header.len() + part.diff.len() > MAX_BYTES {
                return Err(ReviewSnapshotError::TooLarge);
            }
            text.push_str(separator);
            text.push_str(&header);
            text.push_str(part.diff);
        }
        if seen.is_empty() {
            return Err(ReviewSnapshotError::EmptySelection);
        }
        Ok(Self(text))
    }
}

pub(super) fn git_header_path(prefix: &str, path: &str, quote_unicode: bool) -> String {
    let mut encoded = format!("{prefix}/{path}");
    if encoded.contains(['"', '\\']) || (quote_unicode && !encoded.is_ascii()) {
        let mut quoted = String::from("\"");
        for ch in encoded.chars() {
            match ch {
                '"' | '\\' => {
                    quoted.push('\\');
                    quoted.push(ch);
                }
                _ if quote_unicode && !ch.is_ascii() => {
                    for byte in ch.encode_utf8(&mut [0; 4]).bytes() {
                        write!(quoted, "\\{byte:03o}").expect("writing to a String");
                    }
                }
                _ => quoted.push(ch),
            }
        }
        quoted.push('"');
        encoded = quoted;
    }
    encoded
}

fn single_text_patch(diff: &str, path: &str) -> bool {
    if diff.contains('\0') {
        return false;
    }
    let mut lines = diff.lines();
    let Some(header) = lines
        .next()
        .and_then(|line| line.strip_prefix("diff --git "))
    else {
        return false;
    };
    if ![true, false].into_iter().any(|quote| {
        let old = git_header_path("a", path, quote);
        let new = git_header_path("b", path, quote);
        header
            .strip_prefix(&old)
            .and_then(|rest| rest.strip_prefix(' '))
            == Some(new.as_str())
    }) {
        return false;
    }
    let mut has_hunk = false;
    for line in lines {
        if line.starts_with("diff --")
            || line == "GIT binary patch"
            || line.starts_with("Binary files ")
            || line.starts_with("Submodule ")
        {
            return false;
        }
        has_hunk |= line.starts_with("@@ -");
    }
    has_hunk
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str =
        "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";

    fn part(diff: &str, staged: bool) -> Part<'_> {
        Part {
            workspace: "ws",
            path: "file",
            diff,
            staged,
            truncated: false,
        }
    }

    #[test]
    fn freezes_exact_bytes_from_the_selected_sides() {
        let mut draft = PATCH.replace("+new\n", "+未暂存\r\n");
        let snapshot =
            ReviewSnapshot::freeze("ws", [part(PATCH, true), part(&draft, false)]).unwrap();
        let expected = format!("[staged] file\n{PATCH}\n\n[unstaged] file\n{draft}");
        draft.clear();
        assert_eq!(snapshot.text(), expected);
    }

    #[test]
    fn rejects_duplicate_sides_empty_or_foreign_selections() {
        assert_eq!(
            ReviewSnapshot::freeze("ws", []).err(),
            Some(ReviewSnapshotError::EmptySelection)
        );
        assert_eq!(
            ReviewSnapshot::freeze("other", [part(PATCH, true)]).err(),
            Some(ReviewSnapshotError::WrongWorkspace)
        );
        assert_eq!(
            ReviewSnapshot::freeze("ws", [part(PATCH, true), part(PATCH, true)]).err(),
            Some(ReviewSnapshotError::DuplicatePart(1))
        );
    }

    #[test]
    fn refuses_partial_binary_or_ambiguous_input() {
        let truncated = Part {
            truncated: true,
            ..part(PATCH, false)
        };
        assert_eq!(
            ReviewSnapshot::freeze("ws", [truncated]).err(),
            Some(ReviewSnapshotError::Truncated(0))
        );
        for diff in [
            "",
            "diff --git a/img b/img\nBinary files a/img and b/img differ",
            "diff --git a/img b/img\nGIT binary patch\nliteral 16",
            &format!("{PATCH}{PATCH}"),
        ] {
            assert_eq!(
                ReviewSnapshot::freeze("ws", [part(diff, false)]).err(),
                Some(ReviewSnapshotError::UnsupportedDiff(0))
            );
        }
        assert_eq!(
            ReviewSnapshot::freeze(
                "ws",
                [Part {
                    path: "file\nother",
                    ..part(PATCH, true)
                }]
            )
            .err(),
            Some(ReviewSnapshotError::InvalidPath(0))
        );
    }

    #[test]
    fn checks_the_file_identity_including_git_quoted_chinese_names() {
        let mismatched = Part {
            path: "different",
            ..part(PATCH, true)
        };
        assert_eq!(
            ReviewSnapshot::freeze("ws", [mismatched]).err(),
            Some(ReviewSnapshotError::UnsupportedDiff(0))
        );
        let path = "文 件.md";
        for names in [
            r#""a/\346\226\207 \344\273\266.md" "b/\346\226\207 \344\273\266.md""#,
            "a/文 件.md b/文 件.md",
        ] {
            let diff = PATCH.replace("a/file b/file", names);
            assert!(
                ReviewSnapshot::freeze(
                    "ws",
                    [Part {
                        path,
                        ..part(&diff, false)
                    }]
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn accounts_for_the_exact_encoded_size_including_metadata() {
        let patch = format!(
            "{PATCH}+{}",
            "x".repeat(MAX_BYTES - PATCH.len() - 1 - "[staged] file\n".len())
        );
        assert_eq!(
            ReviewSnapshot::freeze("ws", [part(&patch, true)])
                .unwrap()
                .text()
                .len(),
            MAX_BYTES
        );
        assert_eq!(
            ReviewSnapshot::freeze("ws", [part(&format!("{patch}x"), true)]).err(),
            Some(ReviewSnapshotError::TooLarge)
        );
        assert_eq!(
            ReviewSnapshot::freeze("ws", [part(&patch, true), part(PATCH, false)]).err(),
            Some(ReviewSnapshotError::TooLarge)
        );
    }
}
