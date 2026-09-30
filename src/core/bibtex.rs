//! BibTeX citation files parsed into timeline events and rendered back (biblatex `date` beats `year`/`month`).

use crate::core::metadata::TimelineEntry;

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

pub fn parse(content: &str) -> Vec<TimelineEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;
    let bytes = content.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] != b'@' {
            pos += 1;
            continue;
        }
        pos += 1;

        let Some(open) = content[pos..].find('{').map(|i| pos + i) else {
            break;
        };
        let entry_type = content[pos..open].trim().to_lowercase();
        let Some(close) = matching_brace(bytes, open) else {
            break;
        };
        pos = close + 1;
        if matches!(
            entry_type.as_str(),
            "comment" | "string" | "preamble" | "xdata"
        ) {
            continue;
        }

        let body = &content[open + 1..close];
        let field = |name| extract_field(body, name).filter(|v| !v.is_empty());
        let url = field("url")
            .or_else(|| field("link"))
            .or_else(|| field("doi").map(|doi| doi_url(&doi)));
        let date = field("date")
            .or_else(|| format_date(field("year").as_deref(), field("month").as_deref()));
        entries.push(TimelineEntry {
            id: String::new(),
            date,
            title: format_title(
                &field("title").unwrap_or_default(),
                &field("author").unwrap_or_default(),
            ),
            description: field("abstract").or_else(|| field("note")),
            url,
        });
    }

    entries
}

pub fn render<'a>(entries: impl IntoIterator<Item = &'a TimelineEntry>) -> String {
    let mut out = String::new();
    for (i, entry) in entries.into_iter().enumerate() {
        let date = entry.date.as_deref().unwrap_or_default();
        out.push_str(&format!("@misc{{restored{i},\n"));
        out.push_str(&format!("  title = {{{}}},\n", sanitize(&entry.title)));
        if date.len() > 7 {
            out.push_str(&format!("  date = {{{date}}},\n"));
        } else if let Some(year) = date.get(..4) {
            out.push_str(&format!("  year = {{{year}}},\n"));
            if let Some(month) = date
                .get(5..7)
                .and_then(|m| m.parse::<usize>().ok())
                .and_then(|m| MONTHS.get(m.checked_sub(1)?))
            {
                out.push_str(&format!("  month = {{{month}}},\n"));
            }
        }
        if let Some(description) = &entry.description {
            out.push_str(&format!("  abstract = {{{}}},\n", sanitize(description)));
        }
        if let Some(url) = &entry.url {
            out.push_str(&format!("  url = {{{url}}},\n"));
        }
        out.push_str("}\n\n");
    }
    out
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_field(body: &str, field: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut pos = 0;

    loop {
        let abs_pos = pos + body[pos..].find(field)?;
        pos = abs_pos + 1;

        let at_word_start =
            abs_pos == 0 || matches!(bytes[abs_pos - 1], b'\n' | b' ' | b'\t' | b',');
        let Some(after_eq) = body[abs_pos + field.len()..]
            .trim_start()
            .strip_prefix('=')
            .map(str::trim)
        else {
            continue;
        };
        if !at_word_start {
            continue;
        }

        let value = if let Some(inner) = after_eq.strip_prefix('{') {
            let close = matching_brace(after_eq.as_bytes(), 0)?;
            &inner[..close - 1]
        } else if let Some(quoted) = after_eq.strip_prefix('"') {
            &quoted[..quoted.find('"')?]
        } else {
            after_eq[..after_eq.find([',', '}', '\n'])?].trim()
        };
        return Some(value.trim().trim_end_matches(',').to_string());
    }
}

fn doi_url(doi: &str) -> String {
    if doi.starts_with("http://") || doi.starts_with("https://") {
        return doi.to_string();
    }
    let doi = doi.trim_start_matches("doi:").trim();
    format!("https://doi.org/{doi}")
}

fn format_date(year: Option<&str>, month: Option<&str>) -> Option<String> {
    let year = year?.trim();
    let month = month.and_then(|m| {
        let m = m.trim().to_lowercase();
        MONTHS
            .iter()
            .position(|name| m.starts_with(name))
            .or_else(|| {
                m.parse::<usize>()
                    .ok()
                    .filter(|n| (1..=12).contains(n))
                    .map(|n| n - 1)
            })
    });
    Some(match month {
        Some(index) => format!("{year}-{:02}", index + 1),
        None => year.to_string(),
    })
}

fn format_title(title: &str, author: &str) -> String {
    if title.is_empty() {
        return author.to_string();
    }
    let cleaned: String = title.chars().filter(|&c| c != '{' && c != '}').collect();
    if author.is_empty() {
        cleaned
    } else {
        format!("{cleaned} — {author}")
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '{' | '}'))
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extracts_timeline_entries() {
        let bib = r#"
@article{einstein1935,
  title = {Can Quantum-Mechanical Description of Physical Reality Be Considered Complete?},
  author = {Einstein, A. and Podolsky, B. and Rosen, N.},
  year = {1935},
  month = may,
  abstract = {A description of physical reality},
  doi = {10.1038/35057060},
}
"#;
        let entries = parse(bib);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date.as_deref(), Some("1935-05"));
        assert!(entries[0].title.contains("Quantum-Mechanical"));
        assert_eq!(
            entries[0].url.as_deref(),
            Some("https://doi.org/10.1038/35057060")
        );
    }

    #[test]
    fn test_parse_missing_fields_are_none() {
        let entries = parse("@misc{a, title = {Only a title}}");
        assert_eq!(entries[0].title, "Only a title");
        assert_eq!(entries[0].date, None);
        assert_eq!(entries[0].description, None);
        assert_eq!(entries[0].url, None);
    }

    #[test]
    fn test_parse_empty() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn test_parse_multiple_entries() {
        let bib = r#"
@article{first,
  title = {First Paper},
  year = {2020},
}
@article{second,
  title = {Second Paper},
  year = {2021},
}
"#;
        assert_eq!(parse(bib).len(), 2);
    }

    #[test]
    fn test_parse_skips_non_entries() {
        let bib = r#"
@comment{ this should be ignored }
@string{ key = "value" }
@preamble{ "x" }
@article{real,
  title = {Real Entry},
  year = {2023},
}
"#;
        let entries = parse(bib);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].title.contains("Real Entry"));
    }

    #[test]
    fn test_render_round_trips_through_parse() {
        let original = parse(
            "@misc{a, title = {A {B}rief Note}, year = {2023}, month = mar, abstract = {Text}, url = {https://x.y}}",
        );
        let reparsed = parse(&render(&original));
        assert_eq!(reparsed[0].title, "A Brief Note");
        assert_eq!(reparsed[0].date.as_deref(), Some("2023-03"));
        assert_eq!(reparsed[0].description.as_deref(), Some("Text"));
        assert_eq!(reparsed[0].url.as_deref(), Some("https://x.y"));
    }

    #[test]
    fn test_parse_prefers_full_date_and_round_trips_it() {
        let entries = parse("@online{a, title = {Launch}, date = {2025-05-22}, year = {2025}}");
        assert_eq!(entries[0].date.as_deref(), Some("2025-05-22"));
        assert_eq!(
            parse(&render(&entries))[0].date.as_deref(),
            Some("2025-05-22")
        );
    }

    #[test]
    fn test_parse_quoted_bare_and_link_values() {
        let entries = parse(
            "@misc{a,\n  title = \"Quoted Title\",\n  year = 2021,\n  month = 11,\n  link = {https://x.y/news}\n}",
        );
        assert_eq!(entries[0].title, "Quoted Title");
        assert_eq!(entries[0].date.as_deref(), Some("2021-11"));
        assert_eq!(
            entries[0].url.as_deref(),
            Some("https://x.y/news"),
            "link used as url"
        );
    }

    #[test]
    fn test_doi_url() {
        assert_eq!(doi_url("10.1/x"), "https://doi.org/10.1/x");
        assert_eq!(doi_url("doi:10.1/x"), "https://doi.org/10.1/x");
        assert_eq!(doi_url("https://doi.org/10.1/x"), "https://doi.org/10.1/x");
    }

    #[test]
    fn test_format_title() {
        assert_eq!(
            format_title("My Paper", "Smith, J."),
            "My Paper — Smith, J."
        );
        assert_eq!(format_title("My Paper", ""), "My Paper");
        assert_eq!(format_title("", "Smith, J."), "Smith, J.");
        assert_eq!(format_title("{E}nsemble {M}ethods", ""), "Ensemble Methods");
    }

    #[test]
    fn test_format_date() {
        assert_eq!(format_date(Some("2023"), None).as_deref(), Some("2023"));
        assert_eq!(
            format_date(Some("2023"), Some("may")).as_deref(),
            Some("2023-05")
        );
        assert_eq!(
            format_date(Some("2023"), Some("January")).as_deref(),
            Some("2023-01")
        );
        assert_eq!(
            format_date(Some("2023"), Some("11")).as_deref(),
            Some("2023-11")
        );
        assert_eq!(
            format_date(Some("2023"), Some("invalid")).as_deref(),
            Some("2023")
        );
        assert_eq!(format_date(None, Some("may")), None);
    }
}
