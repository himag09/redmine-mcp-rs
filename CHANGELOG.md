# Changelog

All notable changes to this project are documented in this file.

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
