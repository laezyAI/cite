use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::core::CiteError;
use crate::core::bibtex;
use crate::core::cache::hash_files;
use crate::core::db::{BuildRecord, DbManager};
use crate::core::markdown::word_count;
use crate::core::media::{AudioMeta, ImageMeta, extract_audio, extract_image};
use crate::core::metadata::{Podcast, TimelineEntry, TimelineItem};
use crate::core::project::ProjectContext;

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

    let current_hashes = hash_files(ctx.source_files()).await?;

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

    let bundle = build_bundle(ctx, &project_id).await?;

    let artifact = ctx.bundle_path();
    tokio::fs::create_dir_all(ctx.build_dir()).await?;
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

/// A local id derived from what the author wrote, so the same episode or citation
/// file always gets the same id without keeping an id cache on disk.
fn stable_id(project_id: &str, kind: &str, file: &str) -> String {
    let key = format!("cite:{project_id}:{kind}:{file}");
    Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes()).to_string()
}

async fn build_bundle(ctx: &ProjectContext, project_id: &str) -> Result<ContentBundle, CiteError> {
    let mut podcasts = Vec::with_capacity(ctx.metadata.podcasts.len());
    let mut timelines = Vec::new();

    for p in &ctx.metadata.podcasts {
        let id = stable_id(project_id, "podcast", &p.file);
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
            let mut entries = bibtex::parse(&bib);
            if entries.is_empty() {
                continue;
            }
            let tl_id = stable_id(project_id, "timeline", citation);
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_stable_id_depends_only_on_inputs() {
        assert_eq!(
            stable_id("p", "podcast", "content/a.md"),
            stable_id("p", "podcast", "content/a.md")
        );
        assert_ne!(
            stable_id("p", "podcast", "content/a.md"),
            stable_id("p", "podcast", "content/b.md")
        );
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

    #[tokio::test]
    async fn test_metadata_edit_triggers_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = write_project(dir.path());
        let db = DbManager::open_path(&dir.path().join("test.db"))
            .await
            .unwrap();
        compile(&db, &ctx, false).await.unwrap();

        std::fs::write(
            dir.path().join("metadata.yml"),
            "podcasts:\n  - title: Renamed\n    file: content/ep.md\n",
        )
        .unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert!(
            matches!(
                compile(&db, &ctx, false).await.unwrap(),
                CompileOutcome::Complete { .. }
            ),
            "a title change in metadata.yml must be rebuilt and redeployed"
        );
    }
}
