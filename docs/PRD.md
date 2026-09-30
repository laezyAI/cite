# cite — Product Requirements

## 1. Overview

**cite** is a CLI and terminal UI for creating, validating, building, and deploying
podcast episodes to the Supabase backend that serves the aoux app.

Authors write episodes by hand: Markdown for the text, `metadata.yml` for everything
else, plus optional audio, a thumbnail, and BibTeX citations. cite checks that input
against the database's rules, compiles it, and publishes it as news, podcast, and
timeline rows with their media in Supabase Storage.

### Users

- **Authors** who write and publish episodes for an artist they own. They edit files
  by hand, so input must be forgiving where intent is clear and every mistake must be
  reported by name before anything is sent.
- **Automation (CI)** that builds and deploys the same projects non-interactively,
  using `--json` output and environment-variable credentials.

### Goals

- One simple, fixed project layout.
- Catch every problem locally, before the first remote write.
- Deploys that are repeatable: deploying again updates the same rows.
- A way back: every deployment can be rolled back by id.
- Works offline for everything except deploy, rollback, login, and upgrade.

### Non-goals

- Editing the database schema or managing categories and domains (server-managed).
- Slugs, audio renditions/tiers, or publishing Markdown files themselves.
- Hosting or rendering content; the aoux app does that.

---

## 2. Core Concepts

### Project

A directory with a fixed layout:

```
my-project/
├── cite.toml       # name, artist_id, optional [backend]
├── metadata.yml    # one entry per episode
├── .gitignore      # ignores build/ and .cite/
├── content/        # Markdown and BibTeX
├── assets/
│   ├── audio/
│   └── image/
├── cite.lock       # episode file -> news id, per Supabase project (committed)
├── .cite/          # deployment records for rollback (gitignored)
└── build/          # content.json (gitignored)
```

`--path` may point at a project or at a folder of projects; project commands then run
for each one.

### Artist

A row in `artists`, owned by the author's account (`artists.user_id`) and set as
`artist_id` in `cite.toml`. `cite login` lists the account's artists or creates one.
Deploy refuses an artist the logged-in account does not own.

### Episode

One `metadata.yml` entry: Markdown content plus optional summary, source link, audio,
thumbnail, and timeline. It becomes one `news` row.

An episode is identified by its Markdown `file` path — the one value that stays fixed
while titles and content change. `cite.lock` maps that path to the news id it was
deployed as, so later deploys update the same row. Renaming the file publishes a new
item.

### Deployment

One run of `cite deploy` against one Supabase project. It gets a unique id, and its
record of created rows and uploads lets `cite rollback <id>` remove them.

---

## 3. Metadata Model (`metadata.yml`)

```yaml
podcasts:
  - title: "My Podcast"
    file: content/my-article.md
    category: "artificial intelligence"
    summary: "Up to 50 words shown in the app"
    source_url: www.example.com/story
    audio: assets/audio/episode.mp3
    thumbnail: assets/image/thumb.jpg
    timeline:
      - title: "Bill passes the Senate" # event written inline
        date: May 22, 2025
        url: example.com/story
      - content/my-article.bib # BibTeX file -> one event per entry
      - content/earlier-episode.md # another episode of this project -> linked row
      - 26 # news item already published -> linked row
```

Fields:

- `title`, `file`, and `category` are required to deploy. `category` must match an
  existing Supabase category, ignoring case.
- `summary` (at most 50 words) defaults to the first 50 words of the Markdown prose.
- `source_url`, `audio`, `thumbnail`, and `timeline` are optional.
- `artist_id` lives in `cite.toml`, not here.

Timeline:

- A single ordered list; each entry's kind follows from its value:
  - a mapping with `title` (optional `date`, `url`, `description`) is an inline event;
  - a `.bib` path is a citation file whose entries become events (at most one per episode);
  - any other path is another episode's `file`;
  - a number (or quoted number) is an existing news id.
- Rows are written in the order listed, sharing one `sort_order` sequence.
- Authors link their own episodes by file path, never by database id; links are
  resolved at deploy time.

Hand-written input rules — forgiving where intent is clear, strict where it is not:

- Values are trimmed; empty optional values count as absent.
- Dates may be `2025`, `2025-05`, `2025-05-22`, `2025/05/22`, `May 2025`,
  `22 May 2025`, or `May 22, 2025`; a missing month or day means the first.
- Links without a scheme get `https://`; anything still not an http(s) link is an error.
- On events, `link` means `url` and `summary` means `description`.
- An unknown (e.g. misspelled) key is an error naming the key and its line.
- BibTeX dates come from `date`, or `year` and `month`; links from `url`, `link`, or `doi`.

---

## 4. Commands

| Command                    | Description                                                                     |
| -------------------------- | ------------------------------------------------------------------------------- |
| `init <name>`              | Create a project with starter files                                             |
| `doctor`                   | Validate and lint the project; show the last build and deploy                   |
| `build`                    | Compile the project into `build/content.json` (incremental; `--force` rebuilds) |
| `deploy`                   | Validate, rebuild if needed, and publish (`--dry-run` previews)                 |
| `clean`                    | Remove `build/` and the build cache (keeps `cite.lock` and deploy records)      |
| `rollback <deployment-id>` | Remove the news items and uploads a deployment created                          |
| `login`                    | Sign in with email and password; list or create the account's artist            |
| `upgrade`                  | Update cite to the latest release                                               |
| `uninstall`                | Remove cite and its local data                                                  |

Global options:

```
--path <path>    Project, or folder of projects (default: current directory)
-v, --verbose    Detailed logs
-q, --quiet      Errors only
--json           Machine-readable JSON on stdout (init, build, deploy, doctor, clean, rollback)
```

`login --email` is optional; the password is read without echo or from `CITE_PASSWORD`.

Exit status is non-zero when a command fails, including doctor finding errors.

---

## 5. Validation (`cite doctor`)

Doctor reports three levels:

- **Error** — must be fixed; `cite deploy` runs the same checks first and refuses to
  write anything while one remains, listing each.
- **Warning** — does not block, but should be reviewed.
- **Info** — context: files found, backend in use, last build and deployment id.

### Errors

Project:

- `cite.toml` or the metadata file is missing
- `artist_id` is set but not a UUID

Metadata:

- empty title, or a title over 500 characters (episodes and timeline events)
- missing `category`
- `summary` over 50 words
- empty, duplicate, or missing `file`; missing `audio` or `thumbnail` file
- duplicate episode titles
- `source_url` or event `url` that is not an http(s) link
- timeline: more than one `.bib` file, a missing `.bib` file, a path that is not
  another episode's file, an episode listing itself, a news id that is not positive

Content and media:

- empty Markdown, or invalid YAML frontmatter
- audio not mp3/wav/m4a/aac, or over 100 MB (the `podcasts` bucket rules)
- thumbnail not jpg/png/webp/gif, over 5 MB, or smaller than 100×100 pixels

### Warnings

- `artist_id` empty; `content/` or `assets/` folders missing
- no episodes defined; duplicate `source_url`
- event date not understood (deployed without a date)
- unclosed or empty frontmatter
- BibTeX file with no entries, or duplicate entry titles

### Lints (warnings, content quality)

- word count under 100 or over 50,000
- long content (over 500 words) whose timeline cites no sources
- no H1/H2 headings; short paragraphs (under 20 words)
- the same paragraph in more than one episode
- audio under 1 minute or over 4 hours; bitrate under 128 or over 320 kbps; over 50 MB
- audio format or sample rate differing from the rest of the project
- thumbnail under 200×200 or over 8000×8000 pixels, or over 3 MB

---

## 6. Build

```
cite.toml + metadata.yml
   → hash every source file (stop if nothing changed)
   → read Markdown, parse BibTeX, extract audio and image metadata
   → derive stable ids from the project and file paths
   → write build/content.json
   → update the build cache and the local project snapshot
```

- Output is deterministic: the same sources and compiler version give the same JSON.
- A change to `cite.toml`, `metadata.yml`, or any referenced file triggers a rebuild;
  a compiler version change invalidates the cache.
- Validation is not part of the build; doctor and deploy run it.
- Audio metadata: duration, format, codec, bitrate, sample rate, channels, size, SHA-256.
  Image metadata: format, dimensions, size, SHA-256.

---

## 7. Deployment

```
Validate: artist_id is a UUID and doctor reports no errors
   → Build if any source changed
   → (--dry-run stops here: previews creates/updates from cite.lock, no network)
   → Connect, refreshing the login session if expired
   → Pre-flight, before any write:
       artist exists and belongs to the logged-in user
       every episode's category exists (or can be created with the service key)
   → Pass 1, per episode:
       find its news row: the id in cite.lock, else the artist's latest news with the same title
       create the news row, or update it in place (created_at and published_at unchanged)
       upload thumbnail and audio under content-hashed names (unchanged files are skipped)
       create, update, or remove the podcast row
       record the news id in cite.lock
   → Pass 2, per episode: rebuild its timeline_news rows
   → Save cite.lock, the deployment record, and local history
```

Guarantees:

- Idempotent: deploying again updates the same rows instead of duplicating them.
- Every created row and upload is recorded as soon as it exists; a failed deploy can be
  resumed by deploying again, or undone with `cite rollback <id>`.
- Uploads retry up to 3 times.
- Replaced media objects are removed from storage.
- Rollback deletes the news rows a deployment created (cascading to their podcast and
  timeline rows) and its uploads. Rows it updated keep their content. It refuses a
  deployment made to a different Supabase project.

Database triggers create the `artists_news` link and follower notifications when a
news row is inserted; updates do not notify again.

### Schema contract (`supabase.sql`)

What cite writes, and the constraints validation enforces before any write:

| metadata.yml                 | Supabase                                                                                    |
| ---------------------------- | ------------------------------------------------------------------------------------------- |
| episode                      | `news` row (`artist_id` from `cite.toml`; `published_at` set on first publish)              |
| `category`                   | `news.category_id` (existing `categories` row)                                              |
| `source_url`                 | `news.url_id` → `urls` row (+ `domains` when permitted)                                     |
| `summary` / Markdown         | `news.summary`                                                                              |
| `thumbnail`                  | `news.thumbnail` = `assets/<artist_id>/news_<id>-<hash>.<ext>`                              |
| `audio`                      | `podcasts` row: `podcast_url` = `podcasts/<artist_id>/podcast_<id>-<hash>.<ext>`, `duration_minutes` |
| timeline event / BibTeX      | `timeline_news` row: `title`, `description`, `url_id`, `event_date`, `sort_order`           |
| timeline episode / news id   | `timeline_news` row: `child_news_id`, `sort_order`                                          |

- Titles are at most 500 characters; `news.summary` at most 50 words
  (`chk_news_summary_word_count`).
- `news.url_id` is required: episodes without `source_url` get an internal
  `cite://<artist>/<project>/<file>` url, which the app does not open.
- `timeline_news` rows are either a link (`child_news_id` only) or an event (`title`,
  with `description` always sent because the app reads it as non-null), per
  `chk_timeline_items_type`; a news item never links itself.
- Storage values are `bucket/object`, which the app's `resolveStorageUrl` turns into a
  public URL. Objects live under the artist's folder, which the storage policy lets the
  artist's owner write.
- Audio MIME types match the `podcasts` bucket allow-list.
- Categories and domains are writable only with the service role key. Authors use
  existing categories; a url whose domain cannot be created is stored without one.
- `cite login` creates artists with `user_id` set to the signed-in user, as the
  `artists_insert_own` policy requires.

---

## 8. Authentication

cite resolves the Supabase project the same way for `login`, `deploy`, and `rollback`:

1. `[backend]` in `cite.toml` (`url`, `api_key`; `staging_url` and
   `staging_service_key` are accepted as older names)
2. `CITE_SUPABASE_URL` and `CITE_SUPABASE_API_KEY`
3. `~/.cite/credentials.toml` (path overridable with `CITE_CREDS_PATH`)

```toml
[supabase]
url = "https://your-project.supabase.co"
api_key = "eyJhbG..."
```

When none is found, `cite login` asks for the URL and key (the key without echo) and
saves them with owner-only permissions.

`cite login` signs in with email and password and stores the session in
`~/.cite/session.json` (owner-only) with its expiry and Supabase URL. Deploy and
rollback refresh an expired session automatically and ignore a session issued by a
different project. Without a session, requests use the configured key — the service
role key for automation.

---

## 9. Local Data

`~/.cite/cite.db` (override with `CITE_DB_PATH`) is created on first run and migrated
automatically. It holds:

- **Projects** — name, path, artist id, metadata file, last sync.
- **Snapshot of the last build** — each episode's metadata entry, content, and word
  count, and its timeline events; used for analytics and to restore archived projects.
- **Build history** — compiler version, duration, counts, incremental or full.
- **Deployment history** — id, time, status, and counts (dry runs are not recorded).
- **Build cache** — SHA-256 of every source file and the compiler version.

Media files are never stored, only their paths. Rollback records (created and updated
news ids, uploads, target Supabase URL) live in the project's `.cite/deployments/`.

---

## 10. Terminal UI

Running `cite` without a command opens the TUI.

Layout:

- **Projects** (left) — local and archived projects; archived ones can be restored
  from the local snapshot.
- **Commands** (middle, top) — init, build, doctor, deploy, rollback, with the selected
  command's description and argument input.
- **Logs** (middle, bottom) — output of the running command.
- **Analytics** (right) — collapsible sections: global summary, project statistics,
  episodes, timelines, build history, deployment history.

Controls:

| Key                   | Action                                                        |
| --------------------- | ------------------------------------------------------------- |
| `Ctrl+K`              | Toggle command palette                                        |
| `Tab` / `Shift+Tab`   | Cycle focus between panels                                    |
| `↑` / `↓`             | Navigate lists / scroll the focused panel                     |
| `PgUp` / `PgDn`       | Fast-scroll analytics                                         |
| `←` / `→`             | Navigate commands                                             |
| typing                | Arguments for the selected command                            |
| `Enter`               | Run command (Y/N confirm for deploy and rollback) / select    |
| `Ctrl+R`              | Refresh projects and analytics                                |
| `Ctrl+E`              | Open a project file in `$VISUAL`/`$EDITOR` (Projects panel)   |
| `Ctrl+L` / `Ctrl+A`   | Collapse local / archived projects (Projects panel)           |
| `Ctrl+P/T/B/D`        | Collapse episodes, timelines, builds, deploys (Analytics)     |
| `Ctrl+C`              | Cancel a running command, or quit if idle                     |
| `Esc`                 | Close a dialog or clear typed arguments, else quit            |
| `Ctrl+Q`              | Quit                                                          |
