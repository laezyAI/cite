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
cite doctor --path my-project   # check everything, with hints
cite login                      # prints the artist ids you can deploy as
cite deploy --path my-project   # validates, rebuilds if needed, publishes
```

`deploy` rebuilds changed sources itself and refuses to send anything `doctor` reports
as an error; `cite build` is only needed to inspect `build/content.json`.

## Commands

| Command                    | Description                                                                                                            |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `init <name>`              | Create project structure                                                                                               |
| `doctor`                   | Check the project, metadata, content, and media; show errors, warnings, and the last build and deploy                  |
| `build`                    | Compile project → `build/content.json` (incremental)                                                                   |
| `deploy`                   | Validate, rebuild if needed, and publish news, podcasts, timelines and assets; re-deploying updates in place           |
| `deploy --dry-run`         | Preview which episodes would be created or updated, from `cite.lock`, without contacting Supabase                      |
| `clean`                    | Remove `build/` and the incremental build cache (keeps `cite.lock` and deploy records)                                 |
| `rollback <deployment-id>` | Remove the news a deployment created (updated items keep their content)                                                |
| `login`                    | Sign in to Supabase and list the artists you can deploy as                                                             |
| `upgrade`                  | Self-update CLI                                                                                                        |
| `uninstall`                | Remove CLI and local data                                                                                              |

> Global options (all commands): `--path <dir>`, `--json`, `--quiet`, `--verbose`
>
> Command-specific flags: `build --force`, `deploy --dry-run`, `login --email <email>` (the password is asked for without echo, or read from `CITE_PASSWORD`)

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
| Type                | Enter arguments for the selected command in the Commands panel                    |
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
├── cite.toml           # Project manifest (name, artist_id, optional [backend])
├── metadata.yml        # Podcast metadata
├── .gitignore          # Ignores build/ and .cite/
├── content/            # Markdown & BibTeX files
├── assets/
│   ├── audio/          # Podcast audio (optional)
│   └── image/          # Thumbnails (optional)
├── cite.lock           # News id of each deployed episode (commit this)
├── .cite/              # Deploy records for rollback (gitignored)
└── build/              # Generated output (gitignored)
```

## Metadata Model

Each entry in `metadata.yml` becomes one episode (one news item in Supabase):

```yaml
podcasts:
  - title: "My Podcast"
    file: content/my-article.md
    category: "artificial intelligence" # must match a Supabase category (case-insensitive)
    summary: "Up to 50 words shown in the app" # optional; defaults to the first 50 words of `file`
    source_url: www.example.com/story # optional; https:// is added when missing
    audio: assets/audio/episode.mp3 # optional
    thumbnail: assets/image/thumb.jpg # optional
    timeline: # optional; deployed in order as timeline_news rows
      - title: "Bill passes the Senate" # an event written inline
        date: May 22, 2025 # optional; see below for accepted forms
        url: example.com/story # optional (`link` works too)
        description: "What happened" # optional
      - content/my-article.bib # BibTeX citation file -> one event per entry
      - content/earlier-episode.md # another episode of this project -> linked row
      - 26 # news item already published (any artist) -> linked row
```

- `title` (up to 500 characters), `file`, and `category` are required to deploy.
- `timeline` accepts at most one `.bib` file; entries are deployed in the order listed.
  BibTeX dates come from `date` (e.g. `2025-05-22`) or `year` and `month`; links from
  `url`, `link` or `doi`.
- Dates may be written `2025`, `2025-05`, `2025-05-22`, `2025/05/22`, `May 2025`,
  `22 May 2025` or `May 22, 2025`; a missing month or day means the first.
- Links without a scheme (`www.example.com/story`) get `https://`. Anything that is still
  not a web link (spaces, `ftp://`) is an error.
- Surrounding spaces are trimmed and empty optional values are ignored.
- A misspelled key is an error naming the key and line (e.g. ``unknown field `catgory` ``),
  never silently ignored.
- Audio: mp3, wav, m4a or aac, up to 100 MB. Thumbnails: jpg, png, webp or gif, up to 5 MB.
- `cite doctor` checks all of the above; `cite deploy` runs the same checks first.

### Updating episodes

An episode is identified by its `file`. Titles, content, category, and assets can
change freely: `cite deploy` updates the same news item each time. Renaming the
Markdown file publishes a new item.

`cite deploy` records which news item each episode became in `cite.lock`. Commit it
so every machine updates the same items; delete a line to publish that episode as a
new item on the next deploy.

## Authentication

cite finds the Supabase project in this order:

1. `[backend]` in `cite.toml` (`url`, `api_key`)
2. `CITE_SUPABASE_URL` and `CITE_SUPABASE_API_KEY` environment variables
3. `~/.cite/credentials.toml`, written the first time `cite login` asks for them

`cite login` signs in with your email and password and lists the artists you own;
use one of their ids as `artist_id` in `cite.toml`. If you have none, it offers to
create one. The session is refreshed automatically when it expires.

## Local Data

cite keeps build cache, build and deploy history, and a snapshot of each project's
last build in `~/.cite/cite.db` (override with `CITE_DB_PATH`). It powers the TUI
analytics and restoring archived projects, and works offline.

## Tests

```bash
cargo test
```
