use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::core::CiteError;
use crate::core::cache::{UuidCache, hash_files};
use crate::core::db::DbManager;
use crate::core::media::{AudioMeta, ImageMeta, extract_audio, extract_image};
use crate::core::metadata::{Podcast, TimelineEntry, TimelineItem};
use crate::core::project::{BuildRecord, ProjectContext};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBundle {
    pub compiler_version: f64,
    pub project: String,
    pub artist_id: String,
    pub podcasts: Vec<BundlePodcast>,
    pub timelines: Vec<BundleTimeline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundlePodcast {
    pub id: String,
    #[serde(flatten)]
    pub podcast: Podcast,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_meta: Option<AudioMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_meta: Option<ImageMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleTimeline {
    pub id: String,
    pub podcast_id: String,
    pub source: String,
    pub entries: Vec<TimelineEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompileStats {
    pub podcasts: usize,
    pub timelines: usize,
    pub total_words: i64,
    pub duration_ms: i64,
    pub was_incremental: bool,
}

pub enum CompileOutcome {
    UpToDate,
    Complete {
        stats: CompileStats,
        artifact: PathBuf,
    },
}

impl CompileOutcome {
    pub fn emit(&self) {
        match self {
            CompileOutcome::UpToDate => info!("Nothing to rebuild — all files up to date"),
            CompileOutcome::Complete { stats, artifact } => {
                info!(
                    "Built {} podcast(s), {} timeline(s), {} words in {}ms{}",
                    stats.podcasts,
                    stats.timelines,
                    stats.total_words,
                    stats.duration_ms,
                    if stats.was_incremental {
                        " (incremental)"
                    } else {
                        ""
                    }
                );
                info!("Build artifact at {}", artifact.display());
            }
        }
    }
}

pub async fn compile(
    db: &DbManager,
    ctx: &ProjectContext,
    force: bool,
) -> Result<CompileOutcome, CiteError> {
    let start = Instant::now();
    let project_id = ctx.project_id();
    let build = &ctx.manifest.build;

    let current_hashes = hash_files(ctx.content_files()).await?;

    let cache = if force || !build.incremental {
        None
    } else {
        db.load_cache(&project_id)
            .await
            .ok()
            .flatten()
            .filter(|c| c.compiler_version == build.compiler_version)
    };
    if let Some(cache) = &cache
        && cache.changed_since(&current_hashes).is_empty()
    {
        return Ok(CompileOutcome::UpToDate);
    }
    let was_incremental = cache.is_some();

    let mut uuid_cache = UuidCache::load(&ctx.root);
    let bundle = build_bundle(ctx, &project_id, &mut uuid_cache).await?;
    uuid_cache.save(&ctx.root);

    let build_dir = ctx.build_dir();
    let artifact = build_dir.join("content.json");
    tokio::fs::create_dir_all(&build_dir).await?;
    tokio::fs::write(&artifact, serde_json::to_vec_pretty(&bundle)?).await?;

    let duration_ms = start.elapsed().as_millis() as i64;
    let total_words: i64 = bundle
        .podcasts
        .iter()
        .filter_map(|p| p.content.as_deref())
        .map(word_count)
        .sum();
    let timeline_count = bundle
        .timelines
        .iter()
        .map(|t| t.entries.len() as i64)
        .sum();

    // Local analytics are best-effort: a DB hiccup must not fail an otherwise good build.
    let _ = db.save_cache(&project_id, &current_hashes).await;
    let _ = db.sync_project(ctx, &bundle).await;
    let _ = db
        .record_build(&BuildRecord {
            project_id,
            compiler_version: build.compiler_version,
            podcast_count: bundle.podcasts.len() as i64,
            timeline_count,
            total_words,
            duration_ms,
            was_incremental,
        })
        .await;

    Ok(CompileOutcome::Complete {
        stats: CompileStats {
            podcasts: bundle.podcasts.len(),
            timelines: bundle.timelines.len(),
            total_words,
            duration_ms,
            was_incremental,
        },
        artifact,
    })
}

pub fn word_count(content: &str) -> i64 {
    content.split_whitespace().count() as i64
}

async fn build_bundle(
    ctx: &ProjectContext,
    project_id: &str,
    uuid_cache: &mut UuidCache,
) -> Result<ContentBundle, CiteError> {
    let mut podcasts = Vec::with_capacity(ctx.metadata.podcasts.len());
    let mut timelines = Vec::new();

    for p in &ctx.metadata.podcasts {
        let id = uuid_cache.get_or_create(&format!("podcast:{project_id}:{}", p.file));
        let content = read_optional(&ctx.root.join(&p.file), !p.file.is_empty()).await?;

        let audio_path = p.audio.as_ref().map(|a| ctx.root.join(a));
        let thumb_path = p.thumbnail.as_ref().map(|t| ctx.root.join(t));
        let (audio_meta, thumbnail_meta) = tokio::task::spawn_blocking(move || {
            (
                audio_path
                    .filter(|p| p.is_file())
                    .and_then(|p| extract_audio(&p).ok()),
                thumb_path
                    .filter(|p| p.is_file())
                    .and_then(|p| extract_image(&p).ok()),
            )
        })
        .await
        .map_err(|e| CiteError::Config(format!("Media inspection failed: {e}")))?;

        for item in &p.timeline {
            let TimelineItem::Citation(citation) = item else {
                continue;
            };
            let Some(bib) = read_optional(&ctx.root.join(citation), true).await? else {
                continue;
            };
            let mut entries = parse_bibtex(&bib);
            if entries.is_empty() {
                continue;
            }
            let tl_id = uuid_cache.get_or_create(&format!("timeline:{project_id}:{citation}"));
            for (idx, entry) in entries.iter_mut().enumerate() {
                entry.id = format!("{tl_id}-{idx}");
            }
            timelines.push(BundleTimeline {
                id: tl_id,
                podcast_id: id.clone(),
                source: citation.clone(),
                entries,
            });
        }

        podcasts.push(BundlePodcast {
            id,
            podcast: p.clone(),
            content,
            audio_meta,
            thumbnail_meta,
        });
    }

    Ok(ContentBundle {
        compiler_version: ctx.manifest.build.compiler_version,
        project: ctx.manifest.project.name.clone(),
        artist_id: ctx.manifest.project.artist_id.clone(),
        podcasts,
        timelines,
    })
}

/// Read a UTF-8 file if `enabled` and it exists; a missing file is not an error.
async fn read_optional(path: &Path, enabled: bool) -> Result<Option<String>, CiteError> {
    if !enabled || !path.is_file() {
        return Ok(None);
    }
    Ok(Some(tokio::fs::read_to_string(path).await?))
}

pub fn parse_bibtex(content: &str) -> Vec<TimelineEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;
    let bytes = content.as_bytes();

    while pos < bytes.len() {
        if bytes[pos] != b'@' {
            pos += 1;
            continue;
        }
        pos += 1;

        let open = match content[pos..].find('{') {
            Some(i) => pos + i,
            None => break,
        };
        let entry_type = content[pos..open].trim().to_lowercase();
        if matches!(
            entry_type.as_str(),
            "comment" | "string" | "preamble" | "xdata"
        ) {
            let mut depth = 1;
            for (offset, &b) in bytes[open + 1..].iter().enumerate() {
                match b {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            pos = open + 1 + offset + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        pos = open + 1;

        let mut depth = 1;
        let mut close = None;
        for (offset, &b) in bytes[pos..].iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(pos + offset);
                        break;
                    }
                }
                _ => {}
            }
        }

        let end = match close {
            Some(i) => i,
            None => break,
        };

        let body = &content[pos..end];
        pos = end + 1;

        let title = extract_bib_field(body, "title").unwrap_or_default();
        let author = extract_bib_field(body, "author").unwrap_or_default();
        let year = extract_bib_field(body, "year");
        let month = extract_bib_field(body, "month");
        let summary = extract_bib_field(body, "abstract")
            .or_else(|| extract_bib_field(body, "note"))
            .unwrap_or_default();
        let url = extract_bib_field(body, "url")
            .or_else(|| extract_bib_field(body, "doi"))
            .unwrap_or_default();
        let link = extract_bib_field(body, "link").filter(|l| !l.trim().is_empty());
        entries.push(TimelineEntry {
            // Stable ids are assigned by the compiler once the owning timeline is known.
            id: String::new(),
            date: Some(format_bib_date(&year, &month)),
            title: format_title(&title, &author),
            summary: Some(summary),
            url: Some(url),
            link,
        });
    }

    entries
}

fn extract_bib_field(body: &str, field: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut pos = 0;

    loop {
        let fpos = body[pos..].find(field)?;
        let abs_pos = pos + fpos;

        if abs_pos > 0 {
            let prev = bytes[abs_pos - 1];
            if prev != b'\n' && prev != b' ' && prev != b'\t' {
                pos = abs_pos + 1;
                continue;
            }
        }

        let after_field = &body[abs_pos + field.len()..];
        let trimmed = after_field.trim_start();
        if !trimmed.starts_with('=') {
            pos = abs_pos + 1;
            continue;
        }

        let after_eq = trimmed[1..].trim();
        let val: &str = if let Some(inner) = after_eq.strip_prefix('{') {
            let mut depth = 1usize;
            let mut end = None;
            for (i, c) in inner.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            end.map(|i| &inner[..i])?
        } else if let Some(quoted) = after_eq.strip_prefix('"') {
            let close = quoted.find('"')?;
            &quoted[..close]
        } else {
            let delim = after_eq.find([',', '}', '\n'])?;
            after_eq[..delim].trim()
        };

        let cleaned = val.trim().trim_end_matches(',');
        return Some(cleaned.to_string());
    }
}

fn format_bib_date(year: &Option<String>, month: &Option<String>) -> String {
    let y = year.as_deref().unwrap_or("");
    let m = month.as_deref().and_then(|m| {
        let m = m.trim().to_lowercase();
        Some(match m.as_str() {
            "jan" | "january" => "01",
            "feb" | "february" => "02",
            "mar" | "march" => "03",
            "apr" | "april" => "04",
            "may" => "05",
            "jun" | "june" => "06",
            "jul" | "july" => "07",
            "aug" | "august" => "08",
            "sep" | "september" => "09",
            "oct" | "october" => "10",
            "nov" | "november" => "11",
            "dec" | "december" => "12",
            _ => return None,
        })
    });

    match (y, m) {
        (y, Some(m)) if !y.is_empty() => format!("{y}-{m}"),
        (y, _) if !y.is_empty() => y.to_string(),
        _ => String::new(),
    }
}

fn format_title(title: &str, author: &str) -> String {
    if title.is_empty() {
        return author.to_string();
    }
    let cleaned: String = title.chars().filter(|&c| c != '{' && c != '}').collect();
    if author.is_empty() {
        cleaned
    } else {
        format!("{} — {}", cleaned, author)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bibtex_extracts_timeline_entries() {
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
        let entries = parse_bibtex(bib);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].date.as_deref(), Some("1935-05"));
        assert!(entries[0].title.contains("Quantum-Mechanical"));
        assert_eq!(entries[0].url.as_deref(), Some("10.1038/35057060"));
    }

    #[test]
    fn test_parse_bibtex_empty() {
        assert!(parse_bibtex("").is_empty());
    }

    #[test]
    fn test_parse_bibtex_multiple_entries() {
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
        assert_eq!(parse_bibtex(bib).len(), 2);
    }

    #[test]
    fn test_format_title_with_author() {
        assert_eq!(
            format_title("My Paper", "Smith, J."),
            "My Paper — Smith, J."
        );
    }

    #[test]
    fn test_format_title_without_author() {
        assert_eq!(format_title("My Paper", ""), "My Paper");
    }

    #[test]
    fn test_format_title_without_title() {
        assert_eq!(format_title("", "Smith, J."), "Smith, J.");
    }

    #[test]
    fn test_format_title_trims_surrounding_braces() {
        assert_eq!(format_title("{E}nsemble {M}ethods", ""), "Ensemble Methods");
    }

    #[test]
    fn test_format_bib_date_year_only() {
        let year = Some("2023".to_string());
        let month = None;
        assert_eq!(format_bib_date(&year, &month), "2023");
    }

    #[test]
    fn test_format_bib_date_year_month() {
        let year = Some("2023".to_string());
        let month = Some("may".to_string());
        assert_eq!(format_bib_date(&year, &month), "2023-05");
    }

    #[test]
    fn test_format_bib_date_full_month() {
        let year = Some("2023".to_string());
        let month = Some("January".to_string());
        assert_eq!(format_bib_date(&year, &month), "2023-01");
    }

    #[test]
    fn test_format_bib_date_empty() {
        let year = None;
        let month = None;
        assert_eq!(format_bib_date(&year, &month), "");
    }

    #[test]
    fn test_format_bib_date_invalid_month() {
        let year = Some("2023".to_string());
        let month = Some("invalid".to_string());
        assert_eq!(format_bib_date(&year, &month), "2023");
    }

    #[test]
    fn test_parse_bibtex_skips_non_entries() {
        let bib = r#"
@comment{ this should be ignored }
@string{ key = "value" }
@preamble{ "x" }
@article{real,
  title = {Real Entry},
  year = {2023},
}
"#;
        let entries = parse_bibtex(bib);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].title.contains("Real Entry"));
    }

    #[test]
    fn test_word_count() {
        assert_eq!(word_count("one two  three\n"), 3);
        assert_eq!(word_count(""), 0);
    }

    fn write_project(dir: &Path) -> ProjectContext {
        std::fs::write(
            dir.join("cite.toml"),
            "[project]\nname = \"p\"\nartist_id = \"11111111-1111-1111-1111-111111111111\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("content")).unwrap();
        std::fs::write(dir.join("content/ep.md"), "# Episode\nHello world").unwrap();
        std::fs::write(
            dir.join("content/ep.bib"),
            "@article{a, title = {First}, year = {2020}}\n@article{b, title = {First}, year = {2021}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("metadata.yml"),
            "podcasts:\n  - title: Ep\n    file: content/ep.md\n    timeline:\n      - content/ep.bib\n",
        )
        .unwrap();
        ProjectContext::load(dir).unwrap()
    }

    #[tokio::test]
    async fn test_compile_syncs_timelines_to_owning_podcast() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = write_project(dir.path());
        let db = DbManager::open_path(&dir.path().join("test.db"))
            .await
            .unwrap();

        let CompileOutcome::Complete { stats, .. } = compile(&db, &ctx, false).await.unwrap()
        else {
            panic!("first build must compile");
        };
        assert!(
            !stats.was_incremental,
            "no cache yet, so this is a full build"
        );

        // Duplicate BibTeX titles must not collide in the local snapshot.
        let snapshot = db.get_restore_snapshot(&ctx.project_id()).await.unwrap();
        assert_eq!(snapshot.podcasts.len(), 1);
        assert_eq!(snapshot.timelines.len(), 2);
        assert!(
            snapshot
                .timelines
                .iter()
                .all(|t| t.podcast_id == snapshot.podcasts[0].id),
            "restored timelines must reference their podcast"
        );

        assert!(matches!(
            compile(&db, &ctx, false).await.unwrap(),
            CompileOutcome::UpToDate
        ));

        std::fs::write(dir.path().join("content/ep.md"), "# Episode\nChanged").unwrap();
        let CompileOutcome::Complete { stats, .. } = compile(&db, &ctx, false).await.unwrap()
        else {
            panic!("changed content must recompile");
        };
        assert!(stats.was_incremental);
    }
}
