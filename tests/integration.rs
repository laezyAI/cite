use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ARTIST: &str = "11111111-1111-1111-1111-111111111111";

struct ProjectHarness {
    _dir: tempfile::TempDir,
    project: PathBuf,
    _db_dir: tempfile::TempDir,
    db_path: PathBuf,
}

impl ProjectHarness {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("cite.db");
        let project = dir.path().join(name);

        Self::cmd_ok(
            &["init", "--path", dir.path().to_str().unwrap(), name],
            &db_path,
        );
        Self {
            _dir: dir,
            project,
            _db_dir: db_dir,
            db_path,
        }
    }

    fn cmd(args: &[&str], db_path: &Path) -> (String, String, bool) {
        let home = db_path.parent().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cite"))
            .args(args)
            .env("CITE_DB_PATH", db_path.to_str().unwrap())
            .env("HOME", home)
            .env("CITE_CREDS_PATH", home.join("no-credentials.toml"))
            .env_remove("CITE_SUPABASE_URL")
            .env_remove("CITE_SUPABASE_API_KEY")
            .output()
            .expect("Failed to run cite");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        (stdout, stderr, output.status.success())
    }

    fn cmd_ok(args: &[&str], db_path: &Path) {
        let (_, stderr, ok) = Self::cmd(args, db_path);
        assert!(ok, "cite {} failed: {stderr}", args.join(" "));
    }

    fn run(&self, args: &[&str]) -> (String, String, bool) {
        let mut full = args.to_vec();
        full.extend_from_slice(&["--path", self.project.to_str().unwrap()]);
        Self::cmd(&full, &self.db_path)
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let (_, stderr, ok) = self.run(args);
        assert!(ok, "cite {} failed: {stderr}", args.join(" "));
        stderr
    }

    fn write_metadata(&self, yaml: &str) {
        fs::write(self.project.join("metadata.yml"), yaml).unwrap();
    }

    fn write_content(&self, relative: &str, text: &str) {
        let path = self.project.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn set_artist(&self) {
        let toml = format!(
            "[project]\nname = \"{}\"\nartist_id = \"{ARTIST}\"\n",
            self.name()
        );
        fs::write(self.project.join("cite.toml"), toml).unwrap();
    }

    fn name(&self) -> &str {
        self.project.file_name().unwrap().to_str().unwrap()
    }

    fn read_bundle(&self) -> serde_json::Value {
        let content = fs::read_to_string(self.project.join("build/content.json")).unwrap();
        serde_json::from_str(&content).unwrap()
    }
}

#[test]
fn init_creates_project_structure() {
    let h = ProjectHarness::new("my-project");

    assert!(h.project.join("cite.toml").exists(), "cite.toml");
    assert!(h.project.join("metadata.yml").exists(), "metadata.yml");
    assert!(h.project.join("content").is_dir(), "content/");
    assert!(h.project.join("assets/audio").is_dir(), "assets/audio/");
    assert!(h.project.join("assets/image").is_dir(), "assets/image/");
    assert!(!h.project.join("build").exists());
    assert!(h.project.join(".gitignore").exists(), ".gitignore");
}

#[test]
fn init_is_idempotent_on_existing_project() {
    let h = ProjectHarness::new("existing");
    let (_, stderr, ok) = ProjectHarness::cmd(
        &[
            "init",
            "--path",
            h.project.parent().unwrap().to_str().unwrap(),
            "existing",
        ],
        &h.db_path,
    );
    assert!(ok);
    assert!(stderr.contains("ready"));
    assert!(stderr.contains("Skipped"));
}

#[test]
fn doctor_catches_missing_file() {
    let h = ProjectHarness::new("missing-file");
    h.write_metadata(
        r#"
podcasts:
  - title: "Broken"
    file: content/nonexistent.md
"#,
    );
    let (_, stderr, ok) = h.run(&["doctor"]);
    assert!(!ok);
    assert!(stderr.contains("does not exist"));
}

#[test]
fn doctor_catches_missing_metadata() {
    let h = ProjectHarness::new("no-meta");
    fs::remove_file(h.project.join("metadata.yml")).unwrap();
    let (_, stderr, ok) = h.run(&["doctor"]);
    assert!(!ok);
    assert!(stderr.contains("not found"));
}

#[test]
fn doctor_warns_on_short_content() {
    let h = ProjectHarness::new("short-content");
    h.write_content("content/a.md", "Hi");
    h.write_metadata(
        r#"
podcasts:
  - title: "A"
    file: content/a.md
    category: tech
"#,
    );
    let (_, stderr, ok) = h.run(&["doctor"]);
    assert!(ok);
    assert!(stderr.contains("low word count"), "{stderr}");
}

#[test]
fn build_produces_valid_content_json() {
    let h = ProjectHarness::new("build-test");
    h.write_content("content/article.md", "# Hello World");
    h.write_metadata(
        r#"
podcasts:
  - title: "My Podcast"
    file: content/article.md
    category: tech
"#,
    );

    h.set_artist();
    h.run_ok(&["build"]);

    let bundle = h.read_bundle();
    assert!(h.project.join("build").exists());
    assert_eq!(bundle["project"], "build-test");
    assert_eq!(bundle["compiler_version"], 1.0);
    assert_eq!(bundle["artist_id"], ARTIST);
    assert_eq!(bundle["podcasts"].as_array().unwrap().len(), 1);
    let pod = &bundle["podcasts"][0];
    assert!(!pod["id"].as_str().unwrap().is_empty());
    assert_eq!(pod["title"], "My Podcast");
    assert_eq!(
        pod["content"], "# Hello World",
        "Markdown embedded verbatim"
    );
}

#[test]
fn build_generates_timelines_from_bib_citations() {
    let h = ProjectHarness::new("bib-timeline-test");
    h.write_content("content/release.md", "# v1.0 Released");
    h.write_content(
        "content/papers.bib",
        r#"
@article{paper2023,
  title = {Breakthrough in Materials},
  author = {Smith, J.},
  year = {2023},
  month = jun,
  abstract = {A major breakthrough.},
}
@inproceedings{paper2024,
  title = {Follow-up Study},
  author = {Smith, J. and Doe, A.},
  year = {2024},
  month = jan,
  abstract = {Extended results.},
  url = {https://example.com/follow-up},
  link = {https://example.com/news/related-story},
}
"#,
    );
    h.write_metadata(
        r#"
podcasts:
  - title: "Release 1"
    file: content/release.md
    timeline:
      - content/papers.bib
"#,
    );

    h.run_ok(&["build"]);
    let bundle = h.read_bundle();

    let timelines = bundle["timelines"].as_array().unwrap();
    assert_eq!(
        timelines.len(),
        1,
        "should generate one timeline from citation"
    );

    let tl = &timelines[0];
    assert!(!tl["id"].as_str().unwrap().is_empty());

    let entries = tl["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["date"], "2023-06");
    assert!(
        entries[0]["title"]
            .as_str()
            .unwrap()
            .contains("Breakthrough")
    );
    assert_eq!(entries[1]["date"], "2024-01");
    assert_eq!(entries[1]["url"], "https://example.com/follow-up");
}

#[test]
fn build_skips_unchanged_sources_and_rebuilds_changed_ones() {
    let h = ProjectHarness::new("cached-build");
    h.write_content("content/a.md", "v1");
    h.write_metadata("podcasts:\n  - title: A\n    file: content/a.md\n");

    let stderr = h.run_ok(&["build"]);
    assert!(stderr.contains("Built 1 podcast"), "{stderr}");
    assert!(
        !stderr.contains("(incremental)"),
        "first build is full: {stderr}"
    );

    let stderr = h.run_ok(&["build"]);
    assert!(stderr.contains("Nothing to rebuild"), "{stderr}");

    h.write_content("content/a.md", "v2");
    let stderr = h.run_ok(&["build"]);
    assert!(stderr.contains("(incremental)"), "{stderr}");
    assert_eq!(h.read_bundle()["podcasts"][0]["content"], "v2");

    let stderr = h.run_ok(&["build", "--force"]);
    assert!(stderr.contains("Built 1 podcast"), "{stderr}");
}

#[test]
fn build_empty_project_succeeds() {
    let h = ProjectHarness::new("empty-build");
    h.run_ok(&["build"]);
    let bundle = h.read_bundle();
    let pods = bundle["podcasts"].as_array().unwrap();
    assert_eq!(pods.len(), 0, "template has no default podcast");
}

#[test]
fn doctor_shows_project_info_with_status() {
    let h = ProjectHarness::new("status-test");
    h.write_content("content/a.md", "# Content");
    h.write_metadata(
        r#"
podcasts:
  - title: "A"
    file: content/a.md
    category: tech
"#,
    );

    let stderr = h.run_ok(&["doctor"]);
    assert!(stderr.contains("status-test"));
    assert!(stderr.contains("Artist ID"));

    h.run_ok(&["build"]);

    let stderr = h.run_ok(&["doctor"]);
    assert!(stderr.contains("Podcasts: 1"));
    assert!(stderr.contains("Last build:"));
}

#[test]
fn doctor_detects_missing_project() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("cite.db");
    let (_, stderr, ok) = ProjectHarness::cmd(
        &["doctor", "--path", "/tmp/nonexistent-project-test-12345"],
        &db_path,
    );
    assert!(ok);
    assert!(stderr.contains("cite.toml: missing"));
}

#[test]
fn doctor_passes_on_new_project() {
    let h = ProjectHarness::new("doctor-test");
    let stderr = h.run_ok(&["doctor"]);
    assert!(stderr.contains("cite.toml found"));
    assert!(stderr.contains("metadata.yml found"));
}

#[test]
fn clean_removes_artifacts_and_is_idempotent() {
    let h = ProjectHarness::new("clean-test");
    h.write_content("content/a.md", "# A");
    h.write_metadata(
        r#"
podcasts:
  - title: "A"
    file: content/a.md
"#,
    );
    h.run_ok(&["build"]);
    assert!(h.project.join("build").exists());

    h.run_ok(&["clean"]);
    assert!(!h.project.join("build").exists(), "build/ removed");

    h.run_ok(&["clean"]);
}

#[test]
fn deploy_fails_without_backend() {
    let h = ProjectHarness::new("no-backend");
    fs::write(
        h.project.join("cite.toml"),
        "[project]\nname = \"no-backend\"\nartist_id = \"11111111-1111-1111-1111-111111111111\"\n",
    )
    .unwrap();
    let (_, stderr, ok) = h.run(&["deploy"]);
    assert!(!ok);
    assert!(stderr.contains("No credentials found"), "{stderr}");
}

#[test]
fn deploy_requires_valid_artist_id() {
    let h = ProjectHarness::new("no-artist");
    fs::write(
        h.project.join("cite.toml"),
        "[project]\nname = \"no-build\"\nversion = \"0.1.0\"\nlanguage = \"en\"\nmetadata_file = \"metadata.yml\"\nartist_id = \"\"\n\n[build]\ncompiler_version = 0.0\nincremental = true\noutput_format = \"json\"\n\n[backend]\nstaging_url = \"https://test.supabase.co\"\nstaging_service_key = \"test-key\"\n",
    ).unwrap();
    let (_, stderr, ok) = h.run(&["deploy"]);
    assert!(!ok);
    assert!(stderr.contains("artist_id"), "{stderr}");
}

#[test]
fn deploy_refuses_invalid_metadata_before_connecting() {
    let h = ProjectHarness::new("invalid-deploy");
    h.set_artist();
    h.write_content("content/a.md", "# A");
    h.write_metadata(&format!(
        "podcasts:\n  - title: A\n    file: content/a.md\n    summary: {}\n",
        "word ".repeat(51)
    ));
    let (_, stderr, ok) = h.run(&["deploy"]);
    assert!(!ok);
    assert!(stderr.contains("Fix 2 problem(s)"), "{stderr}");
    assert!(stderr.contains("no category"), "{stderr}");
    assert!(stderr.contains("summary has 51 words"), "{stderr}");
    assert!(
        !stderr.contains("credentials"),
        "validated before connecting: {stderr}"
    );
    assert!(!h.project.join("build").exists(), "nothing built");
}

#[test]
fn misspelled_metadata_key_is_reported_with_its_line() {
    let h = ProjectHarness::new("typo");
    h.write_metadata("podcasts:\n  - title: A\n    file: content/a.md\n    catgory: tech\n");
    for command in ["doctor", "build", "deploy"] {
        let (_, stderr, ok) = h.run(&[command]);
        assert!(!ok, "{command} must fail");
        assert!(
            stderr.contains("metadata.yml: podcasts[0]: unknown field `catgory`")
                && stderr.contains("line 4"),
            "{command}: {stderr}"
        );
    }
}

#[test]
fn rollback_fails_without_backend() {
    let h = ProjectHarness::new("no-backend-rb");
    let (_, stderr, ok) = h.run(&["rollback", "some-id"]);
    assert!(!ok);
    assert!(
        stderr.contains("No [backend]")
            || stderr.contains("credentials")
            || stderr.contains("No credentials")
    );
}

#[test]
fn full_workflow_end_to_end() {
    let h = ProjectHarness::new("e2e");
    h.set_artist();
    h.write_content("content/ai.md", "# AI Article\nSome content about AI.");
    h.write_content("content/ml.md", "# ML Article\nSome content about ML.");
    h.write_metadata(
        r#"
podcasts:
  - title: "AI Article"
    file: content/ai.md
    category: tech
    source_url: www.example.com/ai
    timeline:
      - content/ml.md
      - title: Launch
        date: May 22, 2025
        link: example.com/launch
  - title: "ML Article"
    file: content/ml.md
    category: tech
"#,
    );

    h.run_ok(&["doctor"]);
    let (stdout, stderr, ok) = h.run(&["deploy", "--dry-run"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("Dry run complete"), "{stdout}");
    assert!(
        stderr.contains("Dry run: 2 podcast(s), 0 update(s), 2 new"),
        "{stderr}"
    );

    let bundle = h.read_bundle();
    let ai = &bundle["podcasts"][0];
    assert_eq!(ai["source_url"], "https://www.example.com/ai");
    assert_eq!(ai["timeline"][0], "content/ml.md");
    assert_eq!(ai["timeline"][1]["url"], "https://example.com/launch");
    assert!(
        bundle["podcasts"][1]["content"]
            .as_str()
            .unwrap()
            .contains("ML")
    );

    h.run_ok(&["clean"]);
    assert!(!h.project.join("build").exists());
}

#[test]
fn help_prints_usage() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("cite.db");
    let (stdout, _, ok) = ProjectHarness::cmd(&["--help"], &db_path);
    assert!(ok);
    assert!(stdout.contains("Usage: cite"));
    assert!(stdout.contains("rollback"));
    assert!(stdout.contains("deploy"));
}

#[test]
fn verbose_flag_works() {
    let h = ProjectHarness::new("verbose-test");
    h.run_ok(&["doctor", "--verbose"]);
}

#[test]
fn json_flag_produces_valid_json() {
    let h = ProjectHarness::new("json-test");
    h.write_content("content/a.md", "# JSON Test");
    h.write_metadata(
        r#"
podcasts:
  - title: "JSON Article"
    file: content/a.md
    category: tech
"#,
    );
    let (stdout, _, ok) = h.run(&["doctor", "--json"]);
    assert!(ok);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.is_array());
}
