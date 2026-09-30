//! Hand-written `metadata.yml` episodes with schema limits enforced (chrono needs full years; empty files mean no episodes).

use std::fmt;

use chrono::{Datelike, NaiveDate};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub const MAX_TITLE_CHARS: usize = 500;
pub const MAX_SUMMARY_WORDS: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(into = "RawTimelineItem")]
pub enum TimelineItem {
    Citation(String),
    Episode(String),
    News(i64),
    Event(TimelineEntry),
}

impl TimelineItem {
    fn from_path(path: &str) -> Self {
        let mut cleaned = path.trim().to_string();
        tidy_path(&mut cleaned);
        if let Ok(id) = cleaned.parse() {
            Self::News(id)
        } else if cleaned.to_lowercase().ends_with(".bib") {
            Self::Citation(cleaned)
        } else {
            Self::Episode(cleaned)
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum RawTimelineItem {
    Path(String),
    News(i64),
    Event(TimelineEntry),
}

impl From<TimelineItem> for RawTimelineItem {
    fn from(item: TimelineItem) -> Self {
        match item {
            TimelineItem::Citation(path) | TimelineItem::Episode(path) => Self::Path(path),
            TimelineItem::News(id) => Self::News(id),
            TimelineItem::Event(entry) => Self::Event(entry),
        }
    }
}

impl<'de> Deserialize<'de> for TimelineItem {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct ItemVisitor;

        impl<'de> Visitor<'de> for ItemVisitor {
            type Value = TimelineItem;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a .bib file, another episode's .md file, a news id, or an event with a title",
                )
            }

            fn visit_str<E: de::Error>(self, path: &str) -> Result<Self::Value, E> {
                Ok(TimelineItem::from_path(path))
            }

            fn visit_i64<E: de::Error>(self, id: i64) -> Result<Self::Value, E> {
                Ok(TimelineItem::News(id))
            }

            fn visit_u64<E: de::Error>(self, id: u64) -> Result<Self::Value, E> {
                i64::try_from(id)
                    .map(TimelineItem::News)
                    .map_err(|_| E::custom(format!("news id {id} is too large")))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                TimelineEntry::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(TimelineItem::Event)
            }
        }

        de.deserialize_any(ItemVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Podcast {
    pub title: String,
    pub file: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<TimelineItem>,
}

impl Podcast {
    pub fn citation(&self) -> Option<&str> {
        self.timeline.iter().find_map(|item| match item {
            TimelineItem::Citation(path) => Some(path.as_str()),
            _ => None,
        })
    }

    pub fn inline_events(&self) -> impl Iterator<Item = &TimelineEntry> {
        self.timeline.iter().filter_map(|item| match item {
            TimelineItem::Event(entry) => Some(entry),
            _ => None,
        })
    }

    fn tidy(&mut self) {
        tidy_text(&mut self.title);
        tidy_path(&mut self.file);
        for field in [
            &mut self.summary,
            &mut self.category,
            &mut self.thumbnail,
            &mut self.audio,
        ] {
            tidy_optional(field);
        }
        tidy_path_optional(&mut self.thumbnail);
        tidy_path_optional(&mut self.audio);
        tidy_url(&mut self.source_url);
        for item in &mut self.timeline {
            match item {
                TimelineItem::Event(entry) => entry.tidy(),
                TimelineItem::Citation(path) | TimelineItem::Episode(path) => {
                    tidy_text(path);
                    tidy_path(path);
                }
                TimelineItem::News(_) => {}
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimelineEntry {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub title: String,
    #[serde(
        deserialize_with = "string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub date: Option<String>,
    #[serde(alias = "summary", skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(alias = "link", skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl TimelineEntry {
    pub fn event_date(&self) -> Option<String> {
        let day = parse_date(self.date.as_deref()?)?;
        Some(format!("{day}T00:00:00Z"))
    }

    fn tidy(&mut self) {
        tidy_text(&mut self.title);
        tidy_optional(&mut self.date);
        tidy_optional(&mut self.description);
        tidy_url(&mut self.url);
    }
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    const FORMATS: [&str; 4] = ["%Y-%m-%d", "%d %B %Y", "%B %d, %Y", "%B %d %Y"];
    let text = text.trim().replace('/', "-");
    if text.len() == 4 && text.bytes().all(|b| b.is_ascii_digit()) {
        return NaiveDate::from_ymd_opt(text.parse().ok()?, 1, 1);
    }
    [text.clone(), format!("{text}-01"), format!("1 {text}")]
        .iter()
        .find_map(|candidate| {
            FORMATS
                .iter()
                .filter_map(|format| NaiveDate::parse_from_str(candidate, format).ok())
                .find(|day| text.contains(&format!("{:04}", day.year())))
        })
}

fn tidy_text(value: &mut String) {
    let trimmed = value.trim();
    if trimmed.len() != value.len() {
        *value = trimmed.to_string();
    }
}

fn tidy_optional(value: &mut Option<String>) {
    if let Some(text) = value {
        tidy_text(text);
    }
    if value.as_deref() == Some("") {
        *value = None;
    }
}

fn tidy_path(value: &mut String) {
    let mut text = value.trim().to_string();
    while let Some(rest) = text.strip_prefix("./").map(str::trim_start) {
        text = rest.to_string();
    }
    while text.contains("//") {
        let mut cleaned = String::with_capacity(text.len());
        let mut last_slash = false;
        for c in text.chars() {
            if c == '/' {
                if !last_slash {
                    cleaned.push(c);
                }
                last_slash = true;
            } else {
                cleaned.push(c);
                last_slash = false;
            }
        }
        if cleaned.len() == text.len() {
            break;
        }
        text = cleaned;
    }
    *value = text;
}

fn tidy_path_optional(value: &mut Option<String>) {
    if let Some(text) = value {
        tidy_text(text);
        tidy_path(text);
    }
    if value.as_deref() == Some("") {
        *value = None;
    }
}

fn tidy_url(value: &mut Option<String>) {
    tidy_optional(value);
    if let Some(url) = value
        && !url.contains("://")
    {
        *url = format!("https://{url}");
    }
}

fn string_or_number<'de, D: Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(i64),
    }
    Ok(Option::<Value>::deserialize(de)?.map(|v| match v {
        Value::Text(s) => s,
        Value::Number(n) => n.to_string(),
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Metadata {
    pub podcasts: Vec<Podcast>,
}

impl Metadata {
    pub fn parse(yaml: &str) -> Result<Self, serde_yaml::Error> {
        if yaml
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
        {
            return Ok(Self::default());
        }
        let mut metadata: Self = serde_yaml::from_str(yaml)?;
        metadata.podcasts.iter_mut().for_each(Podcast::tidy);
        Ok(metadata)
    }

    pub fn referenced_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        for p in &self.podcasts {
            files.push(p.file.clone());
            if let Some(cit) = p.citation() {
                files.push(cit.to_string());
            }
            if let Some(audio) = &p.audio {
                files.push(audio.clone());
            }
            if let Some(thumb) = &p.thumbnail {
                files.push(thumb.clone());
            }
        }
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yaml_parse() {
        let yaml = r#"
podcasts:
  - title: "Test Podcast"
    file: content/test.md
    summary: A short hand-written summary.
    source_url: "https://example.com"
    category: "tech"
    timeline:
      - content/test.bib
      - content/earlier-episode.md
      - 26
      - "27"
      - title: Model released
        date: 2025-05-22
        url: https://example.com/release
        description: The release that started it.
      - title: Founded
        date: 2021
"#;
        let meta: Metadata = serde_yaml::from_str(yaml).unwrap();
        let podcast = &meta.podcasts[0];
        assert_eq!(podcast.title, "Test Podcast");
        assert_eq!(
            podcast.summary.as_deref(),
            Some("A short hand-written summary.")
        );
        assert_eq!(podcast.source_url.as_deref(), Some("https://example.com"));
        let timeline = vec![
            TimelineItem::Citation("content/test.bib".to_string()),
            TimelineItem::Episode("content/earlier-episode.md".to_string()),
            TimelineItem::News(26),
            TimelineItem::News(27),
            TimelineItem::Event(TimelineEntry {
                title: "Model released".into(),
                date: Some("2025-05-22".into()),
                description: Some("The release that started it.".into()),
                url: Some("https://example.com/release".into()),
                ..Default::default()
            }),
            TimelineItem::Event(TimelineEntry {
                title: "Founded".into(),
                date: Some("2021".into()),
                ..Default::default()
            }),
        ];
        assert_eq!(podcast.timeline, timeline);
        assert_eq!(podcast.citation(), Some("content/test.bib"));
        assert_eq!(podcast.inline_events().count(), 2);

        let yaml = serde_yaml::to_string(&meta).unwrap();
        assert!(yaml.contains("- content/earlier-episode.md"), "{yaml}");
        assert!(yaml.contains("- 26"), "{yaml}");
        assert!(yaml.contains("title: Founded"), "{yaml}");
        assert!(
            !yaml.contains("id:"),
            "compiler ids are not authored: {yaml}"
        );
        let reparsed: Metadata = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reparsed.podcasts[0].timeline, timeline);
    }

    #[test]
    fn test_yaml_parse_without_timeline() {
        let meta: Metadata =
            serde_yaml::from_str("podcasts:\n  - title: T\n    file: f.md\n").unwrap();
        assert!(meta.podcasts[0].timeline.is_empty());
        assert_eq!(meta.podcasts[0].citation(), None);
    }

    #[test]
    fn test_event_date() {
        let at = |date: &str| {
            TimelineEntry {
                date: Some(date.into()),
                ..Default::default()
            }
            .event_date()
        };
        assert_eq!(at("2023").as_deref(), Some("2023-01-01T00:00:00Z"));
        assert_eq!(at(" 2023-03 ").as_deref(), Some("2023-03-01T00:00:00Z"));
        assert_eq!(at("2025-05-22").as_deref(), Some("2025-05-22T00:00:00Z"));
        assert_eq!(at("2025/05/22").as_deref(), Some("2025-05-22T00:00:00Z"));
        assert_eq!(at("May 2024").as_deref(), Some("2024-05-01T00:00:00Z"));
        assert_eq!(at("22 May 2025").as_deref(), Some("2025-05-22T00:00:00Z"));
        assert_eq!(at("Sep 3, 2025").as_deref(), Some("2025-09-03T00:00:00Z"));
        assert_eq!(
            at("September 3 2025").as_deref(),
            Some("2025-09-03T00:00:00Z")
        );
        assert_eq!(at("2025-02-30"), None);
        assert_eq!(at("march"), None);
        assert_eq!(at("2023-0"), None);
        assert_eq!(at(""), None);
    }

    #[test]
    fn test_parse_tidies_hand_written_values() {
        let meta = Metadata::parse(
            r#"
podcasts:
  - title: "  Spaced title "
    file: content/a.md
    summary: ""
    source_url: www.example.com/story
    category: " Politics "
    timeline:
      - " content/refs.bib "
      - title: Event
        link: example.com/event
        summary: Old key for description.
"#,
        )
        .unwrap();
        let pod = &meta.podcasts[0];
        assert_eq!(pod.title, "Spaced title");
        assert_eq!(pod.summary, None);
        assert_eq!(pod.category.as_deref(), Some("Politics"));
        assert_eq!(
            pod.source_url.as_deref(),
            Some("https://www.example.com/story")
        );
        assert_eq!(pod.citation(), Some("content/refs.bib"));
        let event = pod.inline_events().next().unwrap();
        assert_eq!(event.url.as_deref(), Some("https://example.com/event"));
        assert_eq!(
            event.description.as_deref(),
            Some("Old key for description.")
        );
    }

    #[test]
    fn test_parse_rejects_misspelled_keys() {
        let err = Metadata::parse("podcasts:\n  - title: T\n    catgory: Politics\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `catgory`"), "{err}");

        let err = Metadata::parse("podcasts:\n  - title: T\n    timeline:\n      - tilte: Event\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `tilte`"), "{err}");
        assert!(err.contains("line 4"), "points at the event: {err}");
    }

    #[test]
    fn test_parse_empty_file() {
        assert!(Metadata::parse("").unwrap().podcasts.is_empty());
        assert!(
            Metadata::parse("# only comments\n")
                .unwrap()
                .podcasts
                .is_empty()
        );
    }

    #[test]
    fn test_parse_normalizes_dot_slash_paths() {
        let meta = Metadata::parse(
            "podcasts:\n  - title: Ep\n    file: ./content//ep.md\n    thumbnail: ./assets/image//t.jpg\n    timeline:\n      - ' ./content/refs.bib '\n      - ' ./content/other.md '\n",
        )
        .unwrap();
        let pod = &meta.podcasts[0];
        assert_eq!(pod.file, "content/ep.md");
        assert_eq!(pod.thumbnail.as_deref(), Some("assets/image/t.jpg"));
        assert_eq!(
            pod.timeline,
            vec![
                TimelineItem::Citation("content/refs.bib".to_string()),
                TimelineItem::Episode("content/other.md".to_string()),
            ]
        );
    }

    #[test]
    fn test_referenced_files_includes_all() {
        let meta = Metadata {
            podcasts: vec![Podcast {
                title: "P".into(),
                file: "content/p.md".into(),
                thumbnail: Some("assets/image/p.jpg".into()),
                audio: Some("assets/audio/p.mp3".into()),
                timeline: vec![TimelineItem::Citation("content/p.bib".into())],
                ..Default::default()
            }],
        };

        let files = meta.referenced_files();
        assert_eq!(files.len(), 4);
        assert!(files.contains(&"content/p.md".to_string()));
        assert!(files.contains(&"content/p.bib".to_string()));
        assert!(files.contains(&"assets/audio/p.mp3".to_string()));
        assert!(files.contains(&"assets/image/p.jpg".to_string()));
    }
}
