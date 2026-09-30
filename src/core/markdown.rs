//! Markdown helpers shared by the compiler, deploy summaries, and doctor.

pub fn word_count(content: &str) -> i64 {
    content.split_whitespace().count() as i64
}

/// Splits a leading `---` YAML frontmatter block from the body. Returns
/// `(None, markdown)` when there is no frontmatter or it is never closed.
pub fn split_frontmatter(markdown: &str) -> (Option<&str>, &str) {
    let Some(rest) = markdown
        .strip_prefix("---")
        .and_then(|r| r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n")))
    else {
        return (None, markdown);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, markdown)
}

/// Readable prose from Markdown: drops frontmatter, headings, code blocks and images,
/// and unwraps links and emphasis. Words are joined by single spaces.
pub fn plain_text(markdown: &str) -> String {
    let (_, body) = split_frontmatter(markdown);
    let mut in_code = false;
    let mut words = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.starts_with('#') {
            continue;
        }
        let line = line.trim_start_matches(['>', '-', '*', '+', ' ']);
        words.extend(unwrap_inline(line).split_whitespace().map(str::to_string));
    }
    words.join(" ")
}

/// `[text](url)` becomes `text`, images are dropped, and `*` / `` ` `` markers removed.
fn unwrap_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(close) = rest[mid..].find(')').map(|i| mid + i) else {
            break;
        };
        let is_image = rest[..open].ends_with('!');
        out.push_str(&rest[..open - usize::from(is_image)]);
        if !is_image {
            out.push_str(&rest[open + 1..mid]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out.retain(|c| c != '*' && c != '`');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_count() {
        assert_eq!(word_count("one two  three\n"), 3);
        assert_eq!(word_count(""), 0);
    }

    #[test]
    fn test_split_frontmatter() {
        assert_eq!(
            split_frontmatter("---\ntitle: Ep\n---\n# Body\n"),
            (Some("title: Ep\n"), "# Body\n")
        );
        assert_eq!(split_frontmatter("---\n---\nBody"), (Some(""), "Body"));
        assert_eq!(
            split_frontmatter("---\nnever closed"),
            (None, "---\nnever closed")
        );
        assert_eq!(
            split_frontmatter("# No frontmatter"),
            (None, "# No frontmatter")
        );
    }

    #[test]
    fn test_plain_text_strips_markdown() {
        let md = "---\ntitle: Ep\n---\n# Episode One\n\nWelcome to **the** `show`.\n\n\
                  ```rust\nlet x = 1;\n```\n- See [the docs](https://x.y) ![cover](c.png)\n> quoted";
        assert_eq!(plain_text(md), "Welcome to the show. See the docs quoted");
        assert_eq!(plain_text("--- not frontmatter"), "not frontmatter");
    }
}
