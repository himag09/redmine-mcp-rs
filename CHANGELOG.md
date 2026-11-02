# Changelog

All notable changes to this project are documented in this file.

## 1.2.0 - 2026-11-02

### Added

- `update_time_entry` write tool for `PUT /time_entries/{id}.json`.
- `delete_time_entry` write tool for `DELETE /time_entries/{id}.json`.
- README instructions for installing prebuilt binaries from a GitHub release,
  without Rust or Cargo.
- Tests now cover 96 cases across unit, mock-HTTP and stdio layers.

## 1.1.0 - 2026-11-02

### Added

- `log_time` write tool for `POST /time_entries.json` with issue, hours,
  activity, date and comments.
- `list_time_entries` read tool for hours, comments, activity and user per issue,
  with pagination.
- `get_metadata` now returns `time_entry_activities`.
- Time-entry validation (hours 0-24, dates, positive ids), read-only enforcement,
  mock HTTP and stdio coverage (91 tests).

## 1.0.0 - 2026-10-30

### Added

- Rust MCP server for the Redmine REST API over stdio.
- 13 tools: issue read/search/create/update/note/transition/attachment,
  projects, versions, categories, trackers/statuses and metadata.
- Global full-text search (`GET /search.json`) with scope, type filters,
  attachment search and pagination.
- Argument validation, pagination, date and range checks, and clear error
  messages.
- Read-only mode (`REDMINE_READ_ONLY`), which hides and rejects every write
  tool.
- API key redaction, same-origin redirect policy, no redirects for writes, GET
  retries only for transient failures.
- Response, MCP message and attachment size caps.
- CLI with `--help`, `--version` and the read-only `--check` health check.
- Official `rmcp` 3.8 SDK for the MCP protocol and stdio transport.
- Unit, mock-HTTP integration and stdio end-to-end tests (85 tests).
- CI for formatting, clippy, tests, documentation, release build and MSRV.
- Dependabot updates and a RustSec advisory audit workflow.
- Multi-platform release workflow with SHA-256 checksums.

### Security

- The API key is sent only in `X-Redmine-API-Key`, never logged, and every
  `api_key` field in a Redmine response is removed before the model sees it.
- Search result URLs outside the configured Redmine origin are returned as
  `null`.
- Write requests are never retried or replayed through redirects.
