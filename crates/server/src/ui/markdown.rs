// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Markdown with inert raw HTML and explicit, non-fetching links.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

pub(super) fn render(source: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut links = Vec::new();
    let events = Parser::new_ext(source, options).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        })
        | Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            // Images become explicit links, avoiding automatic requests to untrusted locations.
            // Nested image/link syntax keeps its label inside the outer link.
            let url = dest_url.to_ascii_lowercase();
            let allowed = !links.contains(&true)
                && (url.starts_with("https://")
                    || url.starts_with("http://")
                    || url.starts_with("mailto:"))
                && !dest_url.chars().any(char::is_control);
            links.push(allowed);
            if allowed {
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
            } else {
                Event::InlineHtml("<span>".into())
            }
        }
        Event::End(TagEnd::Link | TagEnd::Image) => {
            if links.pop().unwrap_or(false) {
                Event::End(TagEnd::Link)
            } else {
                Event::InlineHtml("</span>".into())
            }
        }
        event => event,
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_markdown_without_active_html_unsafe_links_or_image_requests() {
        let html = render(
            "## Heading\n\n**Important** and `code`.\n\n- [x] checked\n- ordinary\n\n<script>alert(1)</script>\n\n[unsafe](javascript:alert%281%29) [entity](jav&#x61;script:alert%281%29) ![picture](https://example.invalid/image.png)\n\n[![nested](https://example.invalid/inner)](https://example.invalid/outer)\n\n```html\n<img src=x onerror=alert(1)>\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |",
        );
        assert!(html.contains("<h2>Heading</h2>"));
        assert!(html.contains("<strong>Important</strong>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("type=\"checkbox\""));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("<span>unsafe</span>"));
        assert!(html.contains("href=\"https://example.invalid/image.png\""));
        assert_eq!(html.matches("<a ").count(), 2);
        assert_eq!(html.matches("</a>").count(), 2);
    }
}
