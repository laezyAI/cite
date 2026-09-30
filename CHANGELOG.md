# changelog

## 0.1.0-alpha.5
- Package renamed from `cite-cli` to `cite` (binary, installers, and repo `laezyAI/cite`)
- Fixed `upgrade`, archived-project restore, and staging dry-run writing to the local database
- Faster, lower-memory builds and doctor checks; credentials stored with owner-only permissions

## 0.1.0-alpha.4
- TUI: switched input handling to crossterm's async event stream, removing the background polling task
- TUI: deploy and rollback now show a Y/N confirmation before running
- TUI: fixed Esc quitting the app while a confirmation dialog was open; Esc now closes dialogs and clears in-progress input before quitting
- CLI: deduplicated per-project output/reporting logic in build, deploy, doctor, and clean

## 0.1.0-alpha.3
- `timeline` metadata field: a single ordered list mixing one BibTeX citation file (string path) and existing news item ids (integers)
- Standalone `citation:` field removed - citations are declared inside `timeline`
- Fixed multi-podcast deploys failing when a category had to be created concurrently (categories are re-fetched after insert conflicts)
- Archived projects restore from local database snapshot
- TUI: filterable command palette, Ctrl+C to cancel, safer argument input
- Added interactive TUI with realtime project refresh, log panel, and command execution
- Local database at `~/.cite/cite.db` used for offline analytics
- Added audio metadata extraction (symphonia) and image dimension detection (imagesize)
- Added `--json` flag for machine-parseable output across all commands
- Added credential management module for Supabase authentication
- Fixed thread blocking in TUI by switching to polling with `tokio::spawn`

## 0.1.0-alpha.2
 - TUI added to run all the commands interactively
 - Results from commands are now send in string formats (easy for AI Agents to parse)

## 0.1.0-alpha.1
- scaffold changed to make it more modular
- expanded test coverage
- deployment script updated to perform based on artist and a subscription

## 0.1.0-alpha
- Installer added in release
- cite-cli created with commands (init, validate, lint, build, deploy, status, doctor, clean, upgrade, uninstall)
- Add module test and integration test
