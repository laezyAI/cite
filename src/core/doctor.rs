use std::collections::{HashMap, HashSet};

use serde::Serialize;
use tracing::{error, info, warn};

use crate::core::CiteError;
use crate::core::db::DbManager;
use crate::core::project::ProjectContext;

#[derive(Serialize)]
pub enum DoctorOutcome {
    Clean,
    Findings {
        errors: Vec<String>,
        warnings: Vec<String>,
        infos: Vec<String>,
    },
}

impl DoctorOutcome {
    pub fn has_errors(&self) -> bool {
        match self {
            DoctorOutcome::Findings { errors, .. } => !errors.is_empty(),
            _ => false,
        }
    }

    pub fn has_warnings(&self) -> bool {
        match self {
            DoctorOutcome::Findings { warnings, .. } => !warnings.is_empty(),
            _ => false,
        }
    }

    pub fn merge(&mut self, other: DoctorOutcome) {
        match other {
            DoctorOutcome::Clean => {}
            DoctorOutcome::Findings {
                errors: new_errors,
                warnings: new_warnings,
                infos: new_infos,
            } => {
                for e in new_errors {
                    self.push_error(e);
                }
                for w in new_warnings {
                    self.push_warning(w);
                }
                for i in new_infos {
                    self.push_info(i);
                }
            }
        }
    }

    fn push_error(&mut self, msg: String) {
        match self {
            DoctorOutcome::Findings { errors, .. } => errors.push(msg),
            DoctorOutcome::Clean => {
                *self = DoctorOutcome::Findings {
                    errors: vec![msg],
                    warnings: Vec::new(),
                    infos: Vec::new(),
                }
            }
        }
    }

    fn push_warning(&mut self, msg: String) {
        match self {
            DoctorOutcome::Findings { warnings, .. } => warnings.push(msg),
            DoctorOutcome::Clean => {
                *self = DoctorOutcome::Findings {
                    errors: Vec::new(),
                    warnings: vec![msg],
                    infos: Vec::new(),
                }
            }
        }
    }

    fn push_info(&mut self, msg: String) {
        match self {
            DoctorOutcome::Findings { infos, .. } => infos.push(msg),
            DoctorOutcome::Clean => {
                *self = DoctorOutcome::Findings {
                    errors: Vec::new(),
                    warnings: Vec::new(),
                    infos: vec![msg],
                }
            }
        }
    }

    pub fn emit(&self) {
        match self {
            DoctorOutcome::Clean => {}
            DoctorOutcome::Findings {
                errors,
                warnings,
                infos,
            } => {
                for e in errors {
                    error!("{e}");
                }
                for w in warnings {
                    warn!("{w}");
                }
                for i in infos {
                    info!("{i}");
                }
            }
        }
    }
}

fn collect_findings(
    errors: Vec<String>,
    warnings: Vec<String>,
    infos: Vec<String>,
) -> DoctorOutcome {
    if errors.is_empty() && warnings.is_empty() && infos.is_empty() {
        DoctorOutcome::Clean
    } else {
        DoctorOutcome::Findings {
            errors,
            warnings,
            infos,
        }
    }
}

pub async fn run(db: &DbManager, ctx: &ProjectContext) -> Result<DoctorOutcome, CiteError> {
    info!("Running diagnostics");

    info!("Database: connected");
    let project_id = ctx.project_id();
    if let Ok(Some(_)) = db.load_cache(&project_id).await {
        info!("Cache: present in database");
    }

    let mut outcome = validate_all(db, ctx).await;
    outcome.merge(lint_all(ctx));

    if ctx
        .manifest
        .backend
        .as_ref()
        .and_then(|b| b.staging_url.as_deref())
        .is_some_and(|s| !s.is_empty())
    {
        info!("Backend configured for staging");
    } else {
        outcome.push_warning(
            "No backend configured in cite.toml — deploy will use credentials file or env vars"
                .to_string(),
        );
    }

    if ctx.root.join("cite.toml").exists() {
        info!("cite.toml found");
    }
    let metadata_file = &ctx.manifest.project.metadata_file;
    if ctx.root.join(metadata_file).exists() {
        info!("{metadata_file} found");
    }

    for dir in &["content", "assets/audio", "assets/image", "build"] {
        let d = ctx.root.join(dir);
        if d.is_dir() {
            info!("{dir}/ exists");
        } else if *dir == "build" {
            info!("{dir}/ missing (created by build)");
        } else {
            info!("{dir}/ missing (will be created on init)");
        }
    }

    if ctx.manifest.build.incremental {
        info!("Incremental builds enabled");
    }

    if ctx.manifest.project.artist_id.is_empty() {
        outcome.push_warning("Artist ID is empty — set it in [project] in cite.toml".to_string());
    } else {
        info!("Artist ID: {}", ctx.manifest.project.artist_id);
    }

    Ok(outcome)
}

// ── Comprehensive Validation (PRD Section 12) ──

async fn validate_all(db: &DbManager, ctx: &ProjectContext) -> DoctorOutcome {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut infos = Vec::new();

    validate_project_structure(db, ctx, &mut errors, &mut warnings, &mut infos).await;
    validate_metadata(ctx, &mut errors, &mut warnings);
    validate_markdown(ctx, &mut errors, &mut warnings);
    validate_audio(ctx, &mut errors, &mut warnings);
    validate_images(ctx, &mut errors, &mut warnings);
    validate_bibtex(ctx, &mut errors, &mut warnings);
    validate_urls(ctx, &mut errors, &mut warnings);

    collect_findings(errors, warnings, infos)
}

async fn validate_project_structure(
    db: &DbManager,
    ctx: &ProjectContext,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
    infos: &mut Vec<String>,
) {
    let required = [
        ("cite.toml", ctx.root.join("cite.toml")),
        (
            &ctx.manifest.project.metadata_file,
            ctx.root.join(&ctx.manifest.project.metadata_file),
        ),
    ];
    for (name, path) in &required {
        if !path.exists() {
            errors.push(format!(
                "Required file '{name}' not found at {}",
                path.display()
            ));
        }
    }

    let dirs = [
        ("content", ctx.content_dir()),
        ("assets", ctx.root.join("assets")),
        ("assets/image", ctx.root.join("assets/image")),
        ("assets/audio", ctx.root.join("assets/audio")),
    ];
    for (name, path) in &dirs {
        if !path.is_dir() {
            warnings.push(format!(
                "Directory '{name}' does not exist at {}",
                path.display()
            ));
        }
    }

    let cite_dir = ctx.root.join(".cite");
    if cite_dir.is_dir() {
        infos.push(".cite/ directory found".to_string());
    }

    let project_id = ctx.project_id();
    if let Ok(stats) = db.get_project_stats(&project_id).await {
        infos.push(format!(
            "Database stats: {} builds, {} deployments",
            stats.build_count, stats.deployment_count
        ));
    }
}

fn validate_metadata(ctx: &ProjectContext, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
    let podcasts = &ctx.metadata.podcasts;
    if podcasts.is_empty() {
        warnings.push("No podcasts defined in metadata".to_string());
        return;
    }

    if !ctx.manifest.project.artist_id.is_empty()
        && uuid::Uuid::parse_str(&ctx.manifest.project.artist_id).is_err()
    {
        errors.push("artist_id in cite.toml must be a valid UUID".to_string());
    }

    let mut titles = HashSet::new();
    let mut files = HashSet::new();

    for (i, pod) in podcasts.iter().enumerate() {
        if pod.title.trim().is_empty() {
            errors.push(format!("Podcast #{} has empty title", i + 1));
        }

        if pod.file.trim().is_empty() {
            errors.push(format!("Podcast '{}' has empty file path", pod.title));
        } else {
            if !titles.insert(pod.title.clone()) {
                errors.push(format!("Duplicate podcast title: '{}'", pod.title));
            }
            if !files.insert(pod.file.clone()) {
                errors.push(format!(
                    "Duplicate file reference: '{}' in podcast '{}'",
                    pod.file, pod.title
                ));
            }

            let path = ctx.root.join(&pod.file);
            if !path.exists() {
                errors.push(format!(
                    "Podcast '{}' references file '{}' which does not exist",
                    pod.title, pod.file
                ));
            }
        }

        let mut citation_count = 0usize;
        for item in &pod.timeline {
            match item {
                crate::core::metadata::TimelineItem::Citation(cit) => {
                    citation_count += 1;
                    if !ctx.root.join(cit).exists() {
                        errors.push(format!(
                            "Podcast '{}' references citation file '{cit}' which does not exist",
                            pod.title
                        ));
                    }
                }
                crate::core::metadata::TimelineItem::News(id) => {
                    if *id <= 0 {
                        errors.push(format!(
                            "Podcast '{}' has invalid timeline news id {id} (must be positive)",
                            pod.title
                        ));
                    }
                }
            }
        }
        if citation_count > 1 {
            errors.push(format!(
                "Podcast '{}' declares {citation_count} citation files (at most one)",
                pod.title
            ));
        }

        if let Some(ref audio) = pod.audio {
            let audio_path = ctx.root.join(audio);
            if !audio_path.exists() {
                errors.push(format!(
                    "Podcast '{}' references audio file '{}' which does not exist",
                    pod.title, audio
                ));
            }
        }

        if let Some(ref thumb) = pod.thumbnail {
            let thumb_path = ctx.root.join(thumb);
            if !thumb_path.exists() {
                errors.push(format!(
                    "Podcast '{}' references thumbnail file '{}' which does not exist",
                    pod.title, thumb
                ));
            }
        }
    }
}

fn validate_markdown(ctx: &ProjectContext, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
    for pod in &ctx.metadata.podcasts {
        if pod.file.is_empty() {
            continue;
        }
        let path = ctx.root.join(&pod.file);
        if !path.exists() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if content.trim().is_empty() {
                    errors.push(format!("Podcast '{}' has empty markdown file", pod.title));
                }
                let lines: Vec<&str> = content.lines().collect();
                if let Some(first) = lines.first()
                    && first.starts_with("---")
                {
                    match lines
                        .iter()
                        .skip(1)
                        .position(|l| l.trim() == "---")
                        .map(|i| i + 1)
                    {
                        Some(end) => {
                            let frontmatter: Vec<&&str> = lines[1..end].iter().collect();
                            if frontmatter.is_empty() {
                                warnings.push(format!(
                                    "Podcast '{}' has empty YAML frontmatter",
                                    pod.title
                                ));
                            }
                            let yaml = lines[1..end].join("\n");
                            if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&yaml) {
                                errors.push(format!(
                                    "Podcast '{}' has invalid YAML frontmatter: {e}",
                                    pod.title
                                ));
                            }
                        }
                        None => {
                            warnings.push(format!(
                                "Podcast '{}' has unclosed YAML frontmatter",
                                pod.title
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                errors.push(format!(
                    "Podcast '{}' file '{}' cannot be read: {e}",
                    pod.title, pod.file
                ));
            }
        }
    }
}

fn validate_audio(ctx: &ProjectContext, errors: &mut Vec<String>, _warnings: &mut Vec<String>) {
    let supported = ["mp3", "wav", "flac", "ogg", "m4a"];

    for pod in &ctx.metadata.podcasts {
        let Some(ref audio) = pod.audio else {
            continue;
        };
        let path = ctx.root.join(audio);
        if !path.exists() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if !supported.contains(&ext.as_str()) {
            errors.push(format!(
                "Podcast '{}' has unsupported audio format '.{ext}' (supported: mp3, wav, flac, ogg, m4a)",
                pod.title
            ));
        }

        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        const MAX_AUDIO_SIZE: u64 = 500 * 1024 * 1024;
        if size > MAX_AUDIO_SIZE {
            errors.push(format!(
                "Podcast '{}' audio file exceeds 500 MB ({} bytes)",
                pod.title, size
            ));
        }
    }
}

fn validate_images(ctx: &ProjectContext, errors: &mut Vec<String>, _warnings: &mut Vec<String>) {
    let supported = ["jpg", "jpeg", "png", "webp", "gif"];

    for pod in &ctx.metadata.podcasts {
        let Some(ref thumb) = pod.thumbnail else {
            continue;
        };
        let path = ctx.root.join(thumb);
        if !path.exists() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if !supported.contains(&ext.as_str()) {
            errors.push(format!(
                "Podcast '{}' has unsupported image format '.{ext}' (supported: jpg, png, webp, gif)",
                pod.title
            ));
        }

        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        const MAX_IMAGE_SIZE: u64 = 5 * 1024 * 1024;
        if size > MAX_IMAGE_SIZE {
            errors.push(format!(
                "Podcast '{}' image file exceeds 5 MB ({} bytes)",
                pod.title, size
            ));
        }

        if let Ok(meta) = crate::core::media::inspect_image(&path)
            && meta.width > 0
            && meta.height > 0
            && (meta.width < 100 || meta.height < 100)
        {
            errors.push(format!(
                "Podcast '{}' image is too small ({}x{}), minimum 100x100 pixels",
                pod.title, meta.width, meta.height
            ));
        }
    }
}

fn validate_bibtex(ctx: &ProjectContext, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
    for pod in &ctx.metadata.podcasts {
        let Some(citation) = pod.citation() else {
            continue;
        };
        let path = ctx.root.join(citation);
        if !path.exists() {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if content.trim().is_empty() {
                    warnings.push(format!("Podcast '{}' has empty BibTeX file", pod.title));
                    continue;
                }
                let entries = crate::core::compiler::parse_bibtex(&content);
                if entries.is_empty() {
                    warnings.push(format!(
                        "Podcast '{}' BibTeX file parsed but no entries found",
                        pod.title
                    ));
                } else {
                    let mut keys = HashSet::new();
                    for entry in &entries {
                        if !keys.insert(entry.title.clone()) {
                            warnings.push(format!(
                                "Podcast '{}' BibTeX has duplicate entry: '{}'",
                                pod.title, entry.title
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                errors.push(format!(
                    "Podcast '{}' BibTeX file '{}' cannot be read: {e}",
                    pod.title, citation
                ));
            }
        }
    }
}

fn validate_urls(ctx: &ProjectContext, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
    let mut urls = HashSet::new();

    for pod in &ctx.metadata.podcasts {
        if let Some(ref url) = pod.source_url
            && !url.trim().is_empty()
        {
            if !url.starts_with("http://")
                && !url.starts_with("https://")
                && !url.starts_with("cite://")
            {
                errors.push(format!(
                    "Podcast '{}' has invalid source_url: '{url}' (must start with http://, https://, or cite://)",
                    pod.title
                ));
            }
            if !urls.insert(url.clone()) {
                warnings.push(format!(
                    "Podcast '{}' has duplicate source_url: '{url}'",
                    pod.title
                ));
            }
        }
    }
}

// ── Lint Rules (PRD Section 13) ──

pub fn lint_all(ctx: &ProjectContext) -> DoctorOutcome {
    let mut warnings = Vec::new();
    let mut infos = Vec::new();

    if ctx.metadata.podcasts.is_empty() {
        warnings.push("No podcasts to lint".to_string());
        return collect_findings(Vec::new(), warnings, infos);
    }

    let mut all_content = Vec::new();
    let mut audio_durations = Vec::new();
    let mut audio_formats = Vec::new();
    let mut sample_rates = Vec::new();
    let mut bitrates = Vec::new();
    let mut image_sizes = Vec::new();

    for pod in &ctx.metadata.podcasts {
        let path = ctx.root.join(&pod.file);
        if !path.exists() {
            continue;
        }

        let content_str = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let word_count = content_str.split_whitespace().count();
        let reading_time = word_count.div_ceil(200);

        if word_count < 100 {
            warnings.push(format!(
                "Podcast '{}' has low word count ({} words) — minimum recommended is 100",
                pod.title, word_count
            ));
        }
        if word_count > 50_000 {
            warnings.push(format!(
                "Podcast '{}' has very high word count ({} words) — maximum recommended is 50,000",
                pod.title, word_count
            ));
        }

        if reading_time > 120 {
            warnings.push(format!(
                "Podcast '{}' reading time is over 2 hours",
                pod.title
            ));
        }

        let has_h1 = content_str.lines().any(|l| l.starts_with("# "));
        let has_h2 = content_str.lines().any(|l| l.starts_with("## "));
        if !has_h1 && !has_h2 {
            warnings.push(format!("Podcast '{}' has no H1 or H2 headings", pod.title));
        } else if !has_h1 {
            warnings.push(format!(
                "Podcast '{}' has no H1 heading — consider adding a title",
                pod.title
            ));
        }

        let (paragraph_count, short_paras) = content_str
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .fold((0usize, 0usize), |(total, short), p| {
                let is_short = p.split_whitespace().count() < 20;
                (total + 1, short + usize::from(is_short))
            });
        if short_paras > 0 && paragraph_count > 1 {
            warnings.push(format!(
                "Podcast '{}' has {short_paras} short paragraph(s) (< 20 words) — consider expanding",
                pod.title
            ));
        }

        if word_count > 500 {
            let has_citation = pod.citation().is_some();
            if !has_citation {
                warnings.push(format!(
                    "Podcast '{}' is long ({} words) but has no BibTeX citation file",
                    pod.title, word_count
                ));
            }
        }

        // Audio analysis
        if let Some(ref audio) = pod.audio {
            let audio_path = ctx.root.join(audio);
            if audio_path.exists() {
                infos.push(format!("Podcast '{}' has audio file: {}", pod.title, audio));

                if let Ok(meta) = crate::core::media::inspect_audio(&audio_path) {
                    audio_durations.push(meta.duration_secs);
                    audio_formats.push(meta.format.clone());
                    sample_rates.push(meta.sample_rate_hz);
                    bitrates.push(meta.bitrate_kbps);

                    if meta.duration_secs < 60.0 && meta.duration_secs > 0.0 {
                        warnings.push(format!(
                            "Podcast '{}' audio is very short ({:.0}s)",
                            pod.title, meta.duration_secs
                        ));
                    }
                    if meta.duration_secs > 14400.0 {
                        warnings.push(format!(
                            "Podcast '{}' audio is very long ({:.0}s > 4 hours)",
                            pod.title, meta.duration_secs
                        ));
                    }

                    if meta.bitrate_kbps > 0 && meta.bitrate_kbps < 128 {
                        warnings.push(format!(
                            "Podcast '{}' audio bitrate is low ({} kbps < 128)",
                            pod.title, meta.bitrate_kbps
                        ));
                    }
                    if meta.bitrate_kbps > 320 {
                        warnings.push(format!(
                            "Podcast '{}' audio bitrate is high ({} kbps > 320)",
                            pod.title, meta.bitrate_kbps
                        ));
                    }

                    if meta.size_bytes > 200 * 1024 * 1024 {
                        warnings.push(format!(
                            "Podcast '{}' audio file is large ({} MB > 200 MB)",
                            pod.title,
                            meta.size_bytes / (1024 * 1024)
                        ));
                    }
                }
            }
        } else {
            infos.push(format!(
                "Podcast '{}' has no audio file (audio is optional)",
                pod.title
            ));
        }

        // Image analysis
        if let Some(ref thumb) = pod.thumbnail {
            let thumb_path = ctx.root.join(thumb);
            if thumb_path.exists()
                && let Ok(meta) = crate::core::media::inspect_image(&thumb_path)
            {
                image_sizes.push((meta.width, meta.height, meta.size_bytes));

                if (meta.width > 0 && meta.width < 200) || (meta.height > 0 && meta.height < 200) {
                    warnings.push(format!(
                        "Podcast '{}' thumbnail is small ({}x{}) — minimum 200x200 recommended",
                        pod.title, meta.width, meta.height
                    ));
                }
                if meta.width > 8000 || meta.height > 8000 {
                    warnings.push(format!(
                        "Podcast '{}' thumbnail is very large ({}x{}) — maximum 8000x8000 recommended",
                        pod.title, meta.width, meta.height
                    ));
                }

                if meta.size_bytes > 3 * 1024 * 1024 {
                    warnings.push(format!(
                        "Podcast '{}' thumbnail is large ({} MB > 3 MB)",
                        pod.title,
                        meta.size_bytes / (1024 * 1024)
                    ));
                }
            }
        }

        all_content.push((&pod.title, content_str));
    }

    // Cross-podcast lint checks
    if !audio_formats.is_empty() {
        let mut counts = std::collections::HashMap::new();
        for fmt in &audio_formats {
            *counts.entry(fmt.clone()).or_insert(0) += 1;
        }
        let majority = counts
            .into_iter()
            .max_by_key(|&(_, c)| c)
            .map(|(f, _)| f)
            .unwrap_or_else(|| audio_formats[0].clone());
        for (i, fmt) in audio_formats.iter().enumerate() {
            if fmt != &majority {
                warnings.push(format!(
                    "Audio format inconsistency: podcast {} uses '{}' while most use '{majority}'",
                    i + 1,
                    fmt
                ));
            }
        }
    }

    if sample_rates.len() >= 2 {
        let mut counts = std::collections::HashMap::new();
        for &rate in &sample_rates {
            *counts.entry(rate).or_insert(0) += 1;
        }
        let majority_rate = counts
            .into_iter()
            .max_by_key(|&(_, c)| c)
            .map(|(r, _)| r)
            .unwrap_or(sample_rates[0]);
        for (i, &rate) in sample_rates.iter().enumerate() {
            if rate > 0 && rate != majority_rate {
                warnings.push(format!(
                    "Sample rate inconsistency: podcast {} uses {} Hz while most use {majority_rate} Hz",
                    i + 1,
                    rate
                ));
            }
        }
    }

    // Detect paragraphs repeated across podcasts (single pass, hashed by paragraph text)
    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for (idx, (title, content)) in all_content.iter().enumerate() {
        let mut reported = HashSet::new();
        for (pi, para) in content.split("\n\n").enumerate() {
            let trimmed = para.trim();
            if trimmed.len() <= 20 {
                continue;
            }
            match first_seen.get(trimmed) {
                Some(&owner) if owner != idx => {
                    if reported.insert(trimmed) {
                        warnings.push(format!(
                            "Duplicate paragraph found in '{}' and '{title}' (paragraph {})",
                            all_content[owner].0,
                            pi + 1
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    first_seen.insert(trimmed, idx);
                }
            }
        }
    }

    collect_findings(Vec::new(), warnings, infos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_doctor_outcome_clean() {
        let o = DoctorOutcome::Clean;
        assert!(!o.has_errors());
        assert!(!o.has_warnings());
    }

    #[test]
    fn test_doctor_outcome_push_error() {
        let mut o = DoctorOutcome::Clean;
        o.push_error("err1".into());
        assert!(o.has_errors());
        assert!(!o.has_warnings());
        if let DoctorOutcome::Findings { errors, .. } = &o {
            assert_eq!(errors.len(), 1);
        } else {
            panic!("expected Findings");
        }
    }

    #[test]
    fn test_doctor_outcome_push_warning_on_error() {
        let mut o = DoctorOutcome::Findings {
            errors: vec!["err1".into()],
            warnings: vec![],
            infos: vec![],
        };
        o.push_warning("warn1".into());
        assert!(o.has_errors());
        assert!(o.has_warnings());
    }

    #[test]
    fn test_doctor_outcome_merge() {
        let mut o1 = DoctorOutcome::Clean;
        let o2 = DoctorOutcome::Findings {
            errors: vec!["e1".into(), "e2".into()],
            warnings: vec!["w1".into()],
            infos: vec!["i1".into()],
        };
        o1.merge(o2);
        assert!(o1.has_errors());
        assert!(o1.has_warnings());
        if let DoctorOutcome::Findings {
            errors,
            warnings,
            infos,
        } = &o1
        {
            assert_eq!(errors.len(), 2);
            assert_eq!(warnings.len(), 1);
            assert_eq!(infos.len(), 1);
        } else {
            panic!("expected Findings");
        }
    }

    #[test]
    fn test_doctor_outcome_merge_clean() {
        let mut o1 = DoctorOutcome::Findings {
            errors: vec!["e1".into()],
            warnings: vec![],
            infos: vec![],
        };
        o1.merge(DoctorOutcome::Clean);
        assert!(o1.has_errors());
        assert_eq!(
            match &o1 {
                DoctorOutcome::Findings { errors, .. } => errors.len(),
                _ => 0,
            },
            1
        );
    }
}
