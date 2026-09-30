# cite

Create, validate, build, and deploy podcast content for aoux.

## Installation

### Quick install

(MacOS/Linux only)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/laezyAI/cite/releases/download/v0.1.0-beta.0/cite-installer.sh | sh
```

(Windows only)

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/laezyAI/cite/releases/download/v0.1.0-beta.0/cite-installer.ps1 | iex"
```

### From source

```bash
git clone https://github.com/laezyAI/cite.git
cd cite
cargo build --release
./target/release/cite --help
```

## Quick Start

```bash
cite init my-project
# edit metadata.yml and add content files
cite doctor --path my-project
cite build --path my-project
cite login
cite deploy --path my-project
```

## Commands

| Command                    | Description                                                                                                            |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `init <name>`              | Create project structure                                                                                               |
| `doctor`                   | Validate project, metadata, files, assets, config, content quality, and media; show project health and local analytics |
| `build`                    | Compile project → `build/content.json` (incremental)                                                                   |
| `deploy`                   | Upload bundle to Supabase with verification                                                                            |
| `deploy --staging`         | Deploy to local cite.db instead of Supabase                                                                            |
| `clean`                    | Remove build artifacts and cache                                                                                       |
| `rollback <deployment-id>` | Roll back to a previous deployment                                                                                     |
| `login`                    | Authenticate with Supabase credentials                                                                                 |
| `upgrade`                  | Self-update CLI                                                                                                        |
| `uninstall`                | Remove CLI and local data                                                                                              |

> Global options (all commands): `--path <dir>`, `--json`, `--quiet`, `--verbose`, `--dry-run`
> 
> Command-specific flags: `build --force`, `deploy --staging`, `login --email <email> --password <password>`

## Interactive Terminal UI

Run `cite` with no arguments to enter the TUI:

| Key                 | Action                                                                            |
| ------------------- | --------------------------------------------------------------------------------- |
| `Ctrl+k`            | Toggle command palette                                                            |
| `Tab` / `Shift+Tab` | Cycle focus between panels                                                        |
| `↑` / `↓`           | Navigate lists, scroll logs and analytics                                         |
| `PgUp` / `PgDn`     | Fast-scroll analytics                                                             |
| `←` / `→`           | Navigate commands                                                                 |
| `Enter`             | Execute command (confirms Y/N for deploy/rollback) / select project / expand item |
| Type                | Enter arguments for the selected command in Details panel                         |
| `Ctrl+r`            | Refresh project list                                                              |
| `Ctrl+e`            | Open file editor picker (Projects panel)                                          |
| `Ctrl+l` / `Ctrl+a` | Toggle local / archived projects (Projects panel)                                 |
| `Ctrl+p/t/b/d`      | Expand/collapse podcasts, timelines, builds, deploys (Analytics panel)            |
| `Ctrl+c`            | Cancel a running command, or quit if idle                                         |
| `Esc`               | Close palette / prompt, clear typed args, or quit if nothing to cancel            |
| `Ctrl+q`            | Quit                                                                              |

## Project Structure

```
my-project/
├── cite.toml           # Project manifest (name, language, artist_id)
├── metadata.yml        # Podcast metadata
├── .gitignore          # Ignores build/
├── content/            # Markdown & BibTeX files
├── assets/
│   ├── audio/          # Podcast audio (optional)
│   └── image/          # Thumbnails (optional)
└── build/              # Generated output (gitignored)
```

## Metadata Model

Each entry in `metadata.yml` becomes one episode:

```yaml
podcasts:
  - title: "My Podcast"
    file: content/my-article.md
    source_url: "https://example.com"
    category: "artificial intelligence"
    audio: assets/audio/episode.mp3 # optional
    thumbnail: assets/image/thumb.jpg # optional
    timeline: # optional; deployed in order as timeline_news rows
      - content/my-article.bib # BibTeX citation file -> inline events
      - 26 # existing news item id -> linked row
```

## Authentication

Credentials are stored at `~/.cite/credentials.toml` (via `login`) or read from
the `CITE_SUPABASE_URL` and `CITE_SUPABASE_API_KEY` environment variables.

## Local Analytics

cite maintains a local database at `~/.cite/cite.db` for:

- Compiler cache (file hashes, UUID mappings)
- Build and deployment history
- Project and podcast statistics (word count, reading time, audio duration)
- Asset metadata and usage tracking
- Offline analytics — no network required

Override the database location with `CITE_DB_PATH`.

## Tests

```bash
cargo test
```
