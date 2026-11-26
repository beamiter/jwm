//! Bounded CommonMark to Pango markup; no URL use or external resources.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const MAX_MARKUP_BYTES: usize = 2 * 1024 * 1024;

fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            ch if ch <= '\u{1f}' && !matches!(ch, '\t' | '\r' | '\n') => out.push('�'),
            ch => out.push(ch),
        }
    }
}

/// Translate a bounded CommonMark preview to inert Pango markup.
pub fn pango_markup(source: &str, max_source_bytes: usize) -> Option<String> {
    if source.len() > max_source_bytes {
        return None;
    }
    let mut output = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let size = match level {
                        pulldown_cmark::HeadingLevel::H1 => "xx-large",
                        pulldown_cmark::HeadingLevel::H2 => "x-large",
                        _ => "large",
                    };
                    output.push_str(&format!("<span weight=\"bold\" size=\"{size}\">"));
                }
                Tag::Strong => output.push_str("<b>"),
                Tag::Emphasis => output.push_str("<i>"),
                Tag::Strikethrough => output.push_str("<s>"),
                Tag::CodeBlock(_) => output.push_str("<tt>"),
                Tag::Link { .. } => output.push_str("<u>"),
                Tag::Image { .. } => output.push_str("[图片："),
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    output.push_str(&"  ".repeat(lists.len().saturating_sub(1).min(32)));
                    if let Some(Some(number)) = lists.last_mut() {
                        output.push_str(&format!("{number}. "));
                        *number = number.saturating_add(1);
                    } else {
                        output.push_str("• ");
                    }
                }
                Tag::BlockQuote(_) => output.push_str("│ "),
                _ => (),
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => output.push_str("</span>\n\n"),
                TagEnd::Strong => output.push_str("</b>"),
                TagEnd::Emphasis => output.push_str("</i>"),
                TagEnd::Strikethrough => output.push_str("</s>"),
                TagEnd::CodeBlock => output.push_str("</tt>\n"),
                TagEnd::Link => output.push_str("</u>"),
                TagEnd::Image => output.push(']'),
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Paragraph | TagEnd::BlockQuote(_) | TagEnd::Table => {
                    output.push_str("\n\n")
                }
                TagEnd::Item | TagEnd::TableRow | TagEnd::TableHead => output.push('\n'),
                TagEnd::TableCell => output.push_str("  │  "),
                _ => (),
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                push_escaped(&mut output, &text);
            }
            Event::Code(text) => {
                output.push_str("<tt>");
                push_escaped(&mut output, &text);
                output.push_str("</tt>");
            }
            Event::SoftBreak => output.push(' '),
            Event::HardBreak => output.push('\n'),
            Event::Rule => output.push_str("\n────────────────\n"),
            Event::TaskListMarker(done) => output.push_str(if done { "☑ " } else { "☐ " }),
            _ => (),
        }
        if output.len() > MAX_MARKUP_BYTES {
            return None;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pango_preview_remains_inert_and_bounded() {
        let src = "# Title\n\n**bold** [label](https://invalid.example) ![alt](file:///hidden)\n\n<script>bad</script>";
        let actual = pango_markup(src, 1024).unwrap();
        assert!(actual.contains("<b>bold</b>"));
        assert!(actual.contains("[图片：alt]"));
        assert!(actual.contains("&lt;script&gt;"));
        assert!(!actual.contains("https://"));
        assert!(!actual.contains("file:///"));
        assert!(pango_markup(src, src.len() - 1).is_none());
    }
}
