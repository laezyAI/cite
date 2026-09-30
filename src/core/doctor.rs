//! `cite doctor`: validation that fails a deploy (errors) and content-quality
//! lints (warnings), checked locally before anything reaches Supabase.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;

use serde::Serialize;
use tracing::{error, info, warn};

use crate::core::bibtex;
use crate::core::db::DbManager;
use crate::core::markdown::{split_frontmatter, word_count};
use crate::core::media::{
    AUDIO_FORMATS, IMAGE_FORMATS, MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, inspect_audio, inspect_image,
};
use crate::core::metadata::{
    MAX_SUMMARY_WORDS, MAX_TITLE_CHARS, Podcast, TimelineEntry, TimelineItem,
};
use crate::core::project::ProjectContext;

/// Everything doctor found; the project is ready to deploy when `errors` is empty.
#[derive(Debug, Default, Serialize)]
pub struct DoctorOutcome {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub infos: Vec<String>,
}

impl DoctorOutcome {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn emit(&self) {
        self.errors.iter().for_each(|e| error!("{e}"));
        self.warnings.iter().for_each(|w| warn!("{w}"));
        self.infos.iter().for_each(|i| info!("{i}"));
    }

    fn error(&mut self, msg: String) {
        self.errors.push(msg);
    }

    fn warn(&mut self, msg: String) {
        self.warnings.push(msg);
    }

    fn info(&mut self, msg: String) {
        self.infos.push(msg);
    }
}

/// The full report: project setup, validation, content lints and local history.
pub async fn run(db: &DbManager, ctx: &ProjectContext) -> DoctorOutcome {
    let mut out = DoctorOutcome::default();
    check_project(ctx, &mut out);
    validate_into(ctx, &mut out);
    lint(ctx, &mut out);
    history(db, ctx, &mut out).await;
    out
}

/// Only the checks whose failure would break or corrupt a deploy; `deploy` runs
/// these before writing anything.
pub fn validate(ctx: &ProjectContext) -> DoctorOutcome {
    let mut out = DoctorOutcome::default();
    validate_into(ctx, &mut out);
    out
}

fn validate_into(ctx: &ProjectContext, out: &mut DoctorOutcome) {
    check_metadata(ctx, out);
    for pod in &ctx.metadata.podcasts {
        check_markdown(ctx, pod, out);
        check_audio(ctx, pod, out);
        check_image(ctx, pod, out);
        check_bibtex(ctx, pod, out);
    }
}

// ── Validation: problems that fail or corrupt a deploy ──

fn check_project(ctx: &ProjectContext, out: &mut DoctorOutcome) {
    let project = &ctx.manifest.project;
    for name in ["cite.toml", project.metadata_file.as_str()] {
        if ctx.root.join(name).exists() {
            out.info(format!("{name} found"));
        } else {
            out.error(format!(
                "Required file '{name}' not found in {}",
                ctx.root.display()
            ));
        }
    }
    for dir in ["content", "assets/audio", "assets/image"] {
        if !ctx.root.join(dir).is_dir() {
            out.warn(format!("Directory '{dir}/' does not exist"));
        }
    }

    out.info(format!(
        "Project: {} ({})",
        project.name,
        ctx.root.display()
    ));
    out.info(format!("Artist ID: {}", project.artist_id));
    out.info(format!("Podcasts: {}", ctx.metadata.podcasts.len()));
    match project.artist_id.trim() {
        "" => out.warn(
            "Artist ID is empty — set artist_id in [project] in cite.toml to one of the artists 'cite login' lists"
                .into(),
        ),
        id if uuid::Uuid::parse_str(id).is_err() => {
            out.error("artist_id in cite.toml must be a valid UUID".into());
        }
        _ => {}
    }

    match ctx.manifest.backend.as_ref().and_then(|b| b.url.as_deref()) {
        Some(url) if !url.is_empty() => out.info(format!("Backend: {url}")),
        _ => out.info(
            "No [backend] in cite.toml; deploy uses CITE_SUPABASE_* or ~/.cite/credentials.toml"
                .into(),
        ),
    }
}

fn check_metadata(ctx: &ProjectContext, out: &mut DoctorOutcome) {
    let podcasts = &ctx.metadata.podcasts;
    if podcasts.is_empty() {
        out.warn("No podcasts defined in metadata".into());
        return;
    }

    let mut titles = HashSet::new();
    let mut files = HashSet::new();
    let mut source_urls = HashSet::new();
    for (i, pod) in podcasts.iter().enumerate() {
        let name = &pod.title;
        check_title(&format!("Podcast #{}", i + 1), name, out);
        if !titles.insert(name.as_str()) {
            out.error(format!("Duplicate podcast title: '{name}'"));
        }
        if pod.category.is_none() {
            out.error(format!(
                "Podcast '{name}' has no category — set one that exists in Supabase, e.g. 'category: Politics'"
            ));
        }
        if let Some(summary) = &pod.summary {
            let words = word_count(summary);
            if words > MAX_SUMMARY_WORDS as i64 {
                out.error(format!(
                    "Podcast '{name}' summary has {words} words (at most {MAX_SUMMARY_WORDS})"
                ));
            }
        }

        if pod.file.trim().is_empty() {
            out.error(format!("Podcast '{name}' has empty file path"));
        } else if !files.insert(pod.file.as_str()) {
            out.error(format!("Duplicate file '{}' in podcast '{name}'", pod.file));
        }
        let paths = [
            ("file", Some(&pod.file)),
            ("audio", pod.audio.as_ref()),
            ("thumbnail", pod.thumbnail.as_ref()),
        ];
        for (field, path) in paths {
            if let Some(path) = path.filter(|p| !p.trim().is_empty())
                && !ctx.root.join(path).exists()
            {
                out.error(format!("Podcast '{name}' {field} '{path}' does not exist"));
            }
        }

        if let Some(url) = pod.source_url.as_deref() {
            if !is_web_url(url) {
                out.error(format!(
                    "Podcast '{name}' source_url '{url}' is not a web link"
                ));
            }
            if !source_urls.insert(url) {
                out.warn(format!("Podcast '{name}' has duplicate source_url '{url}'"));
            }
        }

        check_timeline(ctx, pod, out);
    }
}

fn check_timeline(ctx: &ProjectContext, pod: &Podcast, out: &mut DoctorOutcome) {
    let name = &pod.title;
    let mut citations = 0;
    for item in &pod.timeline {
        match item {
            TimelineItem::Citation(path) => {
                citations += 1;
                if !ctx.root.join(path).exists() {
                    out.error(format!(
                        "Podcast '{name}' citation file '{path}' does not exist"
                    ));
                }
            }
            TimelineItem::Episode(file) if *file == pod.file => {
                out.error(format!("Podcast '{name}' lists itself in its timeline"));
            }
            TimelineItem::Episode(file) => {
                if !ctx.metadata.podcasts.iter().any(|p| p.file == *file) {
                    out.error(format!(
                        "Podcast '{name}' timeline entry '{file}' is neither a .bib file nor another episode's file"
                    ));
                }
            }
            TimelineItem::News(id) if *id <= 0 => {
                out.error(format!(
                    "Podcast '{name}' timeline news id {id} must be positive"
                ));
            }
            TimelineItem::News(_) => {}
            TimelineItem::Event(event) => {
                check_event(&format!("Podcast '{name}' timeline event"), event, out);
            }
        }
    }
    if citations > 1 {
        out.error(format!(
            "Podcast '{name}' lists {citations} .bib files (at most one)"
        ));
    }
}

/// An inline or BibTeX event: a title within the column limit, a date the app can
/// show, and a link it can open.
fn check_event(context: &str, event: &TimelineEntry, out: &mut DoctorOutcome) {
    check_title(context, &event.title, out);
    let label = format!("{context} '{}'", event.title.trim());
    if let Some(date) = event.date.as_deref()
        && event.event_date().is_none()
    {
        out.warn(format!(
            "{label} date '{date}' is not understood (e.g. 2024, 2024-05, May 2024, 2024-05-22); it deploys without a date"
        ));
    }
    if let Some(url) = event.url.as_deref()
        && !is_web_url(url)
    {
        out.error(format!("{label} url '{url}' is not a web link"));
    }
}

fn check_title(context: &str, title: &str, out: &mut DoctorOutcome) {
    if title.trim().is_empty() {
        out.error(format!("{context} has an empty title"));
    } else if title.chars().count() > MAX_TITLE_CHARS {
        out.error(format!(
            "{context} '{}…' title is longer than {MAX_TITLE_CHARS} characters",
            title.chars().take(40).collect::<String>()
        ));
    }
}

fn check_markdown(ctx: &ProjectContext, pod: &Podcast, out: &mut DoctorOutcome) {
    let Some(content) = read_existing(ctx, &pod.file, &pod.title, out) else {
        return;
    };
    let name = &pod.title;
    if content.trim().is_empty() {
        out.error(format!("Podcast '{name}' has an empty Markdown file"));
    }
    match split_frontmatter(&content) {
        (Some(yaml), _) if yaml.trim().is_empty() => {
            out.warn(format!("Podcast '{name}' has empty YAML frontmatter"));
        }
        (Some(yaml), _) => {
            if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(yaml) {
                out.error(format!(
                    "Podcast '{name}' has invalid YAML frontmatter: {e}"
                ));
            }
        }
        (None, _) if content.starts_with("---") => {
            out.warn(format!("Podcast '{name}' has unclosed YAML frontmatter"));
        }
        (None, _) => {}
    }
}

fn check_audio(ctx: &ProjectContext, pod: &Podcast, out: &mut DoctorOutcome) {
    let Some(audio) = &pod.audio else { return };
    let path = ctx.root.join(audio);
    if path.is_file() {
        check_asset(
            &pod.title,
            "audio",
            &path,
            AUDIO_FORMATS,
            MAX_AUDIO_BYTES,
            out,
        );
    }
}

fn check_image(ctx: &ProjectContext, pod: &Podcast, out: &mut DoctorOutcome) {
    let Some(thumb) = &pod.thumbnail else { return };
    let path = ctx.root.join(thumb);
    if !path.is_file() {
        return;
    }
    check_asset(
        &pod.title,
        "image",
        &path,
        IMAGE_FORMATS,
        MAX_IMAGE_BYTES,
        out,
    );
    if let Ok(meta) = inspect_image(&path)
        && meta.width > 0
        && meta.height > 0
        && (meta.width < 100 || meta.height < 100)
    {
        out.error(format!(
            "Podcast '{}' image is too small ({}x{}), minimum 100x100 pixels",
            pod.title, meta.width, meta.height
        ));
    }
}

/// Extension and size limits of the storage bucket an asset is uploaded to.
fn check_asset(
    title: &str,
    kind: &str,
    path: &Path,
    formats: &[&str],
    max_bytes: u64,
    out: &mut DoctorOutcome,
) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if !formats.contains(&ext.as_str()) {
        out.error(format!(
            "Podcast '{title}' has unsupported {kind} format '.{ext}' (supported: {})",
            formats.join(", ")
        ));
    }
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    if size > max_bytes {
        out.error(format!(
            "Podcast '{title}' {kind} file exceeds {} MB ({size} bytes)",
            max_bytes / (1024 * 1024)
        ));
    }
}

fn check_bibtex(ctx: &ProjectContext, pod: &Podcast, out: &mut DoctorOutcome) {
    let Some(citation) = pod.citation() else {
        return;
    };
    let Some(content) = read_existing(ctx, citation, &pod.title, out) else {
        return;
    };
    let name = &pod.title;
    let entries = bibtex::parse(&content);
    if entries.is_empty() {
        out.warn(format!(
            "Podcast '{name}' BibTeX file '{citation}' has no entries"
        ));
    }
    let mut seen = HashSet::new();
    for entry in &entries {
        if !seen.insert(entry.title.as_str()) {
            out.warn(format!(
                "Podcast '{name}' BibTeX has duplicate entry '{}'",
                entry.title
            ));
        }
        check_event(&format!("'{citation}' entry"), entry, out);
    }
}

/// Contents of a project file that exists; a missing file is reported elsewhere.
fn read_existing(
    ctx: &ProjectContext,
    file: &str,
    title: &str,
    out: &mut DoctorOutcome,
) -> Option<String> {
    let path = ctx.root.join(file);
    if file.trim().is_empty() || !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path)
        .map_err(|e| {
            out.error(format!(
                "Podcast '{title}' file '{file}' cannot be read: {e}"
            ))
        })
        .ok()
}

/// An `http(s)://` link with a host and no spaces, which the app can open.
fn is_web_url(url: &str) -> bool {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next());
    host.is_some_and(|h| !h.is_empty()) && !url.contains(char::is_whitespace)
}

// ── Local history ──

/// Build and deploy history from the local database; the latest deployment id is
/// what `cite rollback` takes.
async fn history(db: &DbManager, ctx: &ProjectContext, out: &mut DoctorOutcome) {
    let project_id = ctx.project_id();
    if let Ok(stats) = db.get_project_stats(&project_id).await
        && let Some(last_built) = &stats.last_built
    {
        out.info(format!(
            "Last build: {last_built} ({} words, {} timeline entries)",
            stats.total_words, stats.timeline_count
        ));
    }
    if let Ok(deploys) = db.get_deployment_history(&project_id).await
        && let Some(last) = deploys.first()
    {
        out.info(format!(
            "Last deploy: {} at {} ({})",
            last.deployment_id,
            last.deployed_at,
            if last.success { "ok" } else { "failed" }
        ));
    }
}

// ── Lints: content quality, never blocking ──

fn lint(ctx: &ProjectContext, out: &mut DoctorOutcome) {
    let mut contents = Vec::new();
    let mut audio = Vec::new();
    for pod in &ctx.metadata.podcasts {
        let name = &pod.title;
        if let Ok(content) = std::fs::read_to_string(ctx.root.join(&pod.file)) {
            lint_content(pod, &content, out);
            contents.push((name.as_str(), content));
        }
        match &pod.audio {
            Some(file) => {
                if let Ok(meta) = inspect_audio(&ctx.root.join(file)) {
                    lint_audio(name, &meta, out);
                    audio.push((name.as_str(), meta));
                }
            }
            None => out.info(format!("Podcast '{name}' has no audio file (optional)")),
        }
        if let Some(file) = &pod.thumbnail
            && let Ok(meta) = inspect_image(&ctx.root.join(file))
        {
            lint_image(name, &meta, out);
        }
    }

    if let Some(format) = majority(audio.iter().map(|(_, m)| &m.format)) {
        for (name, meta) in audio.iter().filter(|(_, m)| &m.format != format) {
            out.warn(format!(
                "Podcast '{name}' audio is '{}' while most episodes use '{format}'",
                meta.format
            ));
        }
    }
    if let Some(&rate) = majority(audio.iter().map(|(_, m)| &m.sample_rate_hz)) {
        for (name, meta) in audio.iter() {
            if meta.sample_rate_hz > 0 && meta.sample_rate_hz != rate {
                out.warn(format!(
                    "Podcast '{name}' audio is {} Hz while most episodes use {rate} Hz",
                    meta.sample_rate_hz
                ));
            }
        }
    }
    lint_duplicate_paragraphs(&contents, out);
}

fn lint_content(pod: &Podcast, content: &str, out: &mut DoctorOutcome) {
    let name = &pod.title;
    let words = word_count(content);
    if words < 100 {
        out.warn(format!(
            "Podcast '{name}' has low word count ({words} words) — minimum recommended is 100"
        ));
    } else if words > 50_000 {
        out.warn(format!(
            "Podcast '{name}' has very high word count ({words} words) — maximum recommended is 50,000"
        ));
    }
    if words > 500 && pod.citation().is_none() && pod.inline_events().next().is_none() {
        out.warn(format!(
            "Podcast '{name}' is long ({words} words) but its timeline cites no sources"
        ));
    }

    let has_h1 = content.lines().any(|l| l.starts_with("# "));
    let has_h2 = content.lines().any(|l| l.starts_with("## "));
    if !has_h1 && !has_h2 {
        out.warn(format!("Podcast '{name}' has no H1 or H2 headings"));
    } else if !has_h1 {
        out.warn(format!(
            "Podcast '{name}' has no H1 heading — consider adding a title"
        ));
    }

    let paragraphs: Vec<&str> = content
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let short = paragraphs.iter().filter(|p| word_count(p) < 20).count();
    if short > 0 && paragraphs.len() > 1 {
        out.warn(format!(
            "Podcast '{name}' has {short} short paragraph(s) (< 20 words) — consider expanding"
        ));
    }
}

fn lint_audio(name: &str, meta: &crate::core::media::AudioMeta, out: &mut DoctorOutcome) {
    let secs = meta.duration_secs;
    if secs > 0.0 && secs < 60.0 {
        out.warn(format!("Podcast '{name}' audio is very short ({secs:.0}s)"));
    } else if secs > 4.0 * 3600.0 {
        out.warn(format!("Podcast '{name}' audio is longer than 4 hours"));
    }
    let kbps = meta.bitrate_kbps;
    if kbps > 0 && kbps < 128 {
        out.warn(format!(
            "Podcast '{name}' audio bitrate is low ({kbps} kbps < 128)"
        ));
    } else if kbps > 320 {
        out.warn(format!(
            "Podcast '{name}' audio bitrate is high ({kbps} kbps > 320)"
        ));
    }
    if meta.size_bytes > 50 * 1024 * 1024 {
        out.warn(format!(
            "Podcast '{name}' audio file is large ({} MB > 50 MB)",
            meta.size_bytes / (1024 * 1024)
        ));
    }
}

fn lint_image(name: &str, meta: &crate::core::media::ImageMeta, out: &mut DoctorOutcome) {
    let (w, h) = (meta.width, meta.height);
    if (w > 0 && w < 200) || (h > 0 && h < 200) {
        out.warn(format!(
            "Podcast '{name}' thumbnail is small ({w}x{h}) — minimum 200x200 recommended"
        ));
    } else if w > 8000 || h > 8000 {
        out.warn(format!(
            "Podcast '{name}' thumbnail is very large ({w}x{h}) — maximum 8000x8000 recommended"
        ));
    }
    if meta.size_bytes > 3 * 1024 * 1024 {
        out.warn(format!(
            "Podcast '{name}' thumbnail is large ({} MB > 3 MB)",
            meta.size_bytes / (1024 * 1024)
        ));
    }
}

/// Paragraphs that appear in more than one episode.
fn lint_duplicate_paragraphs(contents: &[(&str, String)], out: &mut DoctorOutcome) {
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for (idx, (title, content)) in contents.iter().enumerate() {
        let mut reported = HashSet::new();
        for (pi, para) in content.split("\n\n").enumerate() {
            let para = para.trim();
            if para.len() <= 20 {
                continue;
            }
            match first_seen.get(para) {
                Some(&owner) if owner != idx && reported.insert(para) => {
                    out.warn(format!(
                        "Duplicate paragraph found in '{}' and '{title}' (paragraph {})",
                        contents[owner].0,
                        pi + 1
                    ));
                }
                Some(_) => {}
                None => {
                    first_seen.insert(para, idx);
                }
            }
        }
    }
}

/// The most common value, when there are at least two to compare.
fn majority<'a, T: Eq + Hash + 'a>(values: impl Iterator<Item = &'a T>) -> Option<&'a T> {
    let mut counts: HashMap<&T, usize> = HashMap::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    if counts.values().sum::<usize>() < 2 {
        return None;
    }
    counts.into_iter().max_by_key(|&(_, c)| c).map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(metadata: &str) -> (tempfile::TempDir, ProjectContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"p\"\n").unwrap();
        std::fs::write(dir.path().join("metadata.yml"), metadata).unwrap();
        std::fs::create_dir_all(dir.path().join("content")).unwrap();
        std::fs::write(dir.path().join("content/a.md"), "# A\n\nText").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        (dir, ctx)
    }

    fn metadata_findings(metadata: &str) -> DoctorOutcome {
        let (_dir, ctx) = project(metadata);
        let mut out = DoctorOutcome::default();
        check_metadata(&ctx, &mut out);
        out
    }

    #[test]
    fn test_outcome_flags() {
        let mut out = DoctorOutcome::default();
        assert!(!out.has_errors() && !out.has_warnings());
        out.warn("w".into());
        assert!(out.has_warnings() && !out.has_errors());
        out.error("e".into());
        assert!(out.has_errors());
    }

    #[test]
    fn test_inline_events_are_validated() {
        let out = metadata_findings(
            r#"
podcasts:
  - title: A
    file: content/a.md
    category: tech
    timeline:
      - title: Good event
        date: 2025-05-22
        url: https://example.com/a
      - title: ""
      - title: Missing scheme is fine
        url: example.com/b
      - title: Bad link
        url: see the paper
      - title: Vague date
        date: spring 2024
"#,
        );
        assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
        assert!(out.errors[0].contains("empty title"), "{:?}", out.errors);
        assert!(out.errors[1].contains("not a web link"), "{:?}", out.errors);
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(out.warnings[0].contains("spring 2024"));
    }

    #[test]
    fn test_summary_word_limit() {
        let long = "word ".repeat(MAX_SUMMARY_WORDS + 1);
        let out = metadata_findings(&format!(
            "podcasts:\n  - title: A\n    file: content/a.md\n    category: tech\n    summary: {long}\n"
        ));
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].contains("summary"));
    }

    #[test]
    fn test_missing_assets_and_bad_source_url() {
        let out = metadata_findings(
            r#"
podcasts:
  - title: A
    file: content/a.md
    category: tech
    source_url: ftp://example.com
    audio: assets/audio/missing.mp3
"#,
        );
        assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
        assert!(out.errors.iter().any(|e| e.contains("missing.mp3")));
        assert!(out.errors.iter().any(|e| e.contains("source_url")));
    }

    #[test]
    fn test_missing_category_blocks_deploy() {
        let out = metadata_findings("podcasts:\n  - title: A\n    file: content/a.md\n");
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].contains("no category"));
    }

    #[test]
    fn test_is_web_url() {
        assert!(is_web_url("https://example.com/a?b#c"));
        assert!(is_web_url("http://localhost:8080"));
        assert!(!is_web_url("https://"));
        assert!(!is_web_url("https://not a link"));
        assert!(!is_web_url("ftp://example.com"));
    }

    #[test]
    fn test_timeline_structure_and_duplicates() {
        let out = metadata_findings(
            r#"
podcasts:
  - title: A
    file: content/a.md
    category: tech
    timeline:
      - content/a.md
      - content/unknown.md
      - content/one.bib
      - content/two.bib
      - 0
  - title: A
    file: content/a.md
    category: tech
"#,
        );
        let expected = [
            "lists itself",
            "neither a .bib file nor another episode",
            "citation file 'content/one.bib' does not exist",
            "citation file 'content/two.bib' does not exist",
            "news id 0 must be positive",
            "lists 2 .bib files",
            "Duplicate podcast title",
            "Duplicate file",
        ];
        for text in expected {
            assert!(
                out.errors.iter().any(|e| e.contains(text)),
                "missing '{text}' in {:?}",
                out.errors
            );
        }
        assert_eq!(out.errors.len(), expected.len(), "{:?}", out.errors);
    }

    #[test]
    fn test_content_lints() {
        let para = "This paragraph is long enough to be compared across episodes.";
        let (dir, _) = project("podcasts: []");
        std::fs::write(
            dir.path().join("content/a.md"),
            format!("## Only H2\n\n{para}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("content/b.md"), format!("# B\n\n{para}")).unwrap();
        std::fs::write(
            dir.path().join("metadata.yml"),
            "podcasts:\n  - title: A\n    file: content/a.md\n  - title: B\n    file: content/b.md\n",
        )
        .unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        let mut out = DoctorOutcome::default();
        lint(&ctx, &mut out);

        let warned = |text: &str| out.warnings.iter().any(|w| w.contains(text));
        assert!(warned("'A' has no H1 heading"), "{:?}", out.warnings);
        assert!(!warned("'B' has no H1"), "{:?}", out.warnings);
        assert!(warned("low word count"), "{:?}", out.warnings);
        assert!(
            warned("Duplicate paragraph found in 'A' and 'B'"),
            "{:?}",
            out.warnings
        );
        assert!(out.errors.is_empty(), "lints never block: {:?}", out.errors);
        assert!(out.infos.iter().any(|i| i.contains("no audio file")));
    }

    #[test]
    fn test_majority() {
        assert_eq!(majority(["mp3", "mp3", "wav"].iter()), Some(&"mp3"));
        assert_eq!(majority(["mp3"].iter()), None);
    }
}
