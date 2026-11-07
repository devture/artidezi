// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Bounded, inert unified-diff preview. No repository operations or patch application.
use super::e;
use std::fmt::Write;

const MAX_BYTES: usize = 512 * 1024;
const MAX_LINES: usize = 4_000;
const MAX_LINE_CHARS: usize = 8_000;

#[derive(Default)]
struct File {
    name: String,
    rows: String,
    added: usize,
    removed: usize,
}

fn range(value: &str, sign: char) -> Option<(u64, u64)> {
    let value = value.strip_prefix(sign)?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}
fn hunk(line: &str) -> Option<(u64, u64, u64, u64)> {
    let mut parts = line.strip_prefix("@@ ")?.split_whitespace();
    let (old, old_left) = range(parts.next()?, '-')?;
    let (new, new_left) = range(parts.next()?, '+')?;
    if parts.next()? != "@@" {
        return None;
    }
    Some((old, new, old_left, new_left))
}

pub(super) fn render(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
    let mut truncated = bytes.len() > MAX_BYTES;
    let mut files = Vec::new();
    let mut file = File {
        name: "Patch".into(),
        ..File::default()
    };
    let mut position = None;
    for (index, line) in text.lines().enumerate() {
        if index == MAX_LINES {
            truncated = true;
            break;
        }
        if let Some(name) = line.strip_prefix("diff --git ") {
            if !file.rows.is_empty() || file.name != "Patch" {
                files.push(file);
            }
            file = File {
                name: name.into(),
                ..File::default()
            };
            position = None;
            continue;
        }
        let mut old_number = String::new();
        let mut new_number = String::new();
        let mut kind = "metadata";
        if let Some(next) = hunk(line) {
            position = Some(next);
            kind = "hunk";
        } else if let Some((old, new, old_left, new_left)) = &mut position {
            match line.as_bytes().first() {
                Some(b'+') if *new_left > 0 => {
                    new_number = new.to_string();
                    *new = new.saturating_add(1);
                    *new_left -= 1;
                    kind = "added";
                    file.added += 1;
                }
                Some(b'-') if *old_left > 0 => {
                    old_number = old.to_string();
                    *old = old.saturating_add(1);
                    *old_left -= 1;
                    kind = "removed";
                    file.removed += 1;
                }
                Some(b' ') if *old_left > 0 && *new_left > 0 => {
                    old_number = old.to_string();
                    new_number = new.to_string();
                    *old = old.saturating_add(1);
                    *new = new.saturating_add(1);
                    *old_left -= 1;
                    *new_left -= 1;
                    kind = "context";
                }
                _ if line.starts_with("\\ No newline") => {}
                _ => {
                    position = None;
                }
            }
        }
        if kind == "metadata" && position.is_none() {
            if let Some(name) = line.strip_prefix("+++ ") {
                if name != "/dev/null" {
                    file.name = name.strip_prefix("b/").unwrap_or(name).into();
                }
                continue;
            }
            if let Some(name) = line.strip_prefix("--- ") {
                if name != "/dev/null" {
                    file.name = name.strip_prefix("a/").unwrap_or(name).into();
                }
                continue;
            }
            if line.starts_with("index ") {
                continue;
            }
        }
        let display: String = line.chars().take(MAX_LINE_CHARS).collect();
        let clipped = display.len() != line.len();
        truncated |= clipped;
        write!(file.rows, r#"<tr class="diff-{kind}"><td class="diff-number">{old_number}</td><td class="diff-number">{new_number}</td><td class="diff-text"><code>{}{}</code></td></tr>"#, e(&display), if clipped { "…" } else { "" }).unwrap();
    }
    if !file.rows.is_empty() || file.name != "Patch" {
        files.push(file);
    }
    let mut body = String::new();
    if truncated {
        body.push_str(
            r#"<p class="notice">Preview truncated. Open the raw patch for the full content.</p>"#,
        );
    }
    if files.is_empty() {
        body.push_str(
            r#"<section class="panel"><p class="muted">This patch is empty.</p></section>"#,
        );
    }
    for file in files {
        write!(body, r#"<section class="panel diff-file"><header class="card-header diff-heading"><h2>{}</h2><span class="diff-stat added" title="Added lines in this preview">+{}</span><span class="diff-stat removed" title="Removed lines in this preview">−{}</span></header><div class="diff-scroll" role="region" aria-label="Changes in {}" tabindex="0"><table class="diff-table"><thead><tr><th scope="col">Old</th><th scope="col">New</th><th scope="col">Change</th></tr></thead><tbody>{}</tbody></table></div></section>"#, e(&file.name), file.added, file.removed, e(&file.name), file.rows).unwrap();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_untrusted_hunks_without_confusing_content_with_file_headers() {
        let patch = b"diff --git a/<img> b/<img>\n--- a/<img>\n+++ b/<img>\n@@ -2,2 +3,2 @@\n---old\n+++new\n <script>alert(1)</script>\n";
        let html = render(patch);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img>"));
        assert!(html.contains("&lt;img&gt;"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains(r#"diff-removed"><td class="diff-number">2</td>"#));
        assert!(html.contains(
            r#"diff-added"><td class="diff-number"></td><td class="diff-number">3</td>"#
        ));
        assert!(html.contains(
            r#"diff-context"><td class="diff-number">3</td><td class="diff-number">4</td>"#
        ));
        assert_eq!(html.matches("<h2>").count(), 1);
    }
    #[test]
    fn previews_are_bounded_and_keep_metadata_and_empty_patches_readable() {
        assert!(render(b"").contains("patch is empty"));
        assert!(render(b"Binary files a/a and b/a differ").contains("Binary files"));
        assert!(render(&vec![b'x'; MAX_BYTES + 1]).contains("Preview truncated"));
        let html = render("x\n".repeat(MAX_LINES + 1).as_bytes());
        assert!(html.contains("Preview truncated"));
        assert_eq!(html.matches("<tr class=").count(), MAX_LINES);
        assert_eq!(hunk("@@ -1 +2 @@ function"), Some((1, 2, 1, 1)));
        assert_eq!(hunk("@@ -1,0 +2,3 @@"), Some((1, 2, 0, 3)));
        assert_eq!(hunk("@@ -18446744073709551616 +1 @@"), None);
    }
}
