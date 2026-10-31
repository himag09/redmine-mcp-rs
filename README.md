# redmine-mcp-rs

A fast, safe [Model Context Protocol](https://modelcontextprotocol.io) server for
the [Redmine](https://www.redmine.org) REST API, written in Rust.

It exposes Redmine projects, issues, metadata, notes, status transitions and
attachments as MCP tools over stdio, so any MCP client that can launch a local
process (Claude Desktop, Cursor, DeepSeek Harness, ...) can work with Redmine.

## Highlights

- **Fast.** ~8 ms to complete the MCP handshake and ~5 MB RSS at startup in a
  local benchmark, with a self-contained ~5.6 MB binary.
- **Safe by design.** The API key is never logged or forwarded to another
  origin, `api_key` fields are redacted from every response, an optional
  read-only mode disables all writes, write requests are never retried, and
  response/attachment sizes are capped.
- **Correct.** Arguments are validated before any request, list tools expose
  pagination, text search uses Redmine's native `~` operator, `read_issue`
  supports journals and allowed statuses, and dates/ranges are checked.
- **Official MCP SDK.** The protocol and stdio transport use
  [`rmcp`](https://crates.io/crates/rmcp) 3.8; the Redmine client, validation and
  tool handlers stay in this crate and are independently tested.
- **Small dependency surface.** tokio, rmcp, reqwest (rustls), serde, base64,
  dotenvy and url.
- **Tested.** Unit, mock-HTTP integration and real stdio end-to-end tests. The
  suite does not need a Redmine instance, an API key or external network access.

Startup benchmark: process start through `initialize` and `tools/list`, median
of 10 runs on Apple Silicon macOS with Rust 1.99.1 and rmcp 3.8, with no network
requests.

## Requirements

- Rust **1.88+** (edition 2024).
- A Redmine 4.x/5.x/6.x instance with the REST API enabled and an API key
  (Redmine: *My account → API access key*).

TLS uses rustls with bundled roots; OpenSSL is not required.

## Build and install

```bash
cargo build --release
./target/release/redmine-mcp-rs --version

# Optional: put the binary on your PATH
cargo install --path .
```

The release binary is at `target/release/redmine-mcp-rs`. It is self-contained;
copy it anywhere and point your MCP client at its absolute path.

Tagged releases (`v*`) are built by GitHub Actions for Linux (x86_64 and
aarch64, musl), macOS (arm64 and x86_64) and Windows (x86_64), with SHA-256
checksums. macOS artifacts are not code-signed; Gatekeeper may require
right-click → Open or an explicit `xattr` change.

## Configuration

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `REDMINE_URL` | yes | — | Base URL, `http` or `https`. A trailing slash is optional. |
| `REDMINE_API_KEY` | yes | — | Redmine API key. Sent only in the `X-Redmine-API-Key` header. |
| `REDMINE_DEFAULT_PROJECT_ID` | no | — | Project used by `create_issue` when `project_id` is omitted. |
| `REDMINE_TIMEOUT_MS` | no | `30000` | HTTP timeout in ms, between `1000` and `300000`. |
| `REDMINE_READ_ONLY` | no | `false` | `true`/`1`/`yes`/`on` hides and rejects every write tool. |
| `REDMINE_LOG` | no | `error` | `off`, `error`, `warn`, `info` or `debug`. Logs go to stderr. |

**Precedence:** process environment > `.env` in the current working directory >
built-in defaults. A `.env` file never overrides values supplied by your MCP
client's `env` block; it only fills variables that are missing. `.env` is read
from the directory the server is started in, so either pass the variables
through the client or start the server from the directory that contains `.env`.

```bash
cp .env.example .env
# edit .env
```

### Health check

```bash
REDMINE_URL=https://redmine.example.com/ \
REDMINE_API_KEY=... \
./target/release/redmine-mcp-rs --check
# OK: connected to https://redmine.example.com/ as alice (id 35); read-only: no
```

`--check` performs a single read-only `GET /users/current.json`, validates the
URL/API key and prints the authenticated user. It never prints the API key.

## MCP client configuration

Use the absolute path to the binary. GUI clients often start servers with a
minimal `PATH`, so a bare `redmine-mcp-rs` may not resolve.

### DeepSeek Harness (`~/.dsh/mcp-center.json`)

```json
{
  "servers": [
    {
      "id": "redmine",
      "name": "redmine",
      "type": "stdio",
      "command": "/Users/you/bin/redmine-mcp-rs",
      "args": [],
      "env": {
        "REDMINE_URL": "https://redmine.example.com/",
        "REDMINE_API_KEY": "YOUR_REDMINE_API_KEY",
        "REDMINE_DEFAULT_PROJECT_ID": "1",
        "REDMINE_READ_ONLY": "true"
      }
    }
  ]
}
```

### Claude Desktop / Cursor

```json
{
  "mcpServers": {
    "redmine": {
      "command": "/absolute/path/to/redmine-mcp-rs",
      "args": [],
      "env": {
        "REDMINE_URL": "https://redmine.example.com/",
        "REDMINE_API_KEY": "YOUR_REDMINE_API_KEY"
      }
    }
  }
}
```

Set `REDMINE_READ_ONLY=true` when the client only needs to read. It removes the
five write tools from `tools/list` and rejects direct calls to them.

## Tools

| Tool | Type | Purpose |
| --- | --- | --- |
| `read_issue` | read | Read one issue, optionally with journals, relations, attachments, children, changesets or allowed statuses. |
| `find_issues` | read | Search issues with Redmine filters and pagination. |
| `search` | read | Global full-text search across issues, wiki pages, news, documents, changesets, messages and projects. |
| `list_projects` | read | List projects with pagination. |
| `list_trackers_statuses` | read | List trackers and issue statuses. |
| `get_project_versions` | read | List a project's versions (milestones). |
| `get_project_categories` | read | List a project's issue categories. |
| `get_metadata` | read | Projects, trackers, statuses, users and priorities in one call. |
| `create_issue` | **write** | Create an issue. |
| `update_issue` | **write** | Update fields and/or add a comment. |
| `add_issue_note` | **write** | Add a comment. |
| `transition_issue` | **write** | Change the status (workflow rules apply). |
| `add_attachment` | **write** | Upload one file (base64) and attach it. |

Every tool returns compact JSON in a single `text` content block. Failures use
`isError: true` with an actionable message. Tools carry MCP annotations
(`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) so clients
can apply their own approval policies.

### Details worth knowing

- **`read_issue`** accepts `include: ["journals", "relations", "attachments",
  "children", "changesets", "allowed_statuses"]`. Use `journals` for comments and
  history, and `allowed_statuses` before calling `transition_issue`.
- **`find_issues`** supports `project_id`, `tracker_id`, `status_id`
  (id, `open`, `closed` or `*`), `assigned_to_id` (id or `me`), `author_id`,
  `subject` (exact), `subject_contains` (Redmine's `~` operator), `updated_since`,
  `sort`, `limit` (1–100, default 25) and `offset`. It returns
  `{issues, total_count, offset, limit}`. Use `subject_contains` for
  subject-only search and `search` for full-text across descriptions and
  comments; unknown filter names are not exposed because Redmine rejects them.
- **`search`** performs Redmine's global search (`GET /search.json`) across
  issues, wiki pages, news, documents, changesets, messages and projects. It
  supports `query`, `scope`, the type flags, `open_issues`, `all_words`,
  `titles_only`, `attachments`, `limit` (1–100, default 25) and `offset`.
  Results return `{results, total_count, offset, limit}`; URLs outside the
  configured Redmine origin are replaced with `null`.
- **`list_projects`** returns `{projects, total_count, offset, limit}` and
  supports `limit`/`offset` (1–100).
- **`get_metadata`** fetches the reference data concurrently. `users` requires
  admin rights in Redmine; when unavailable it is `[]` and `warnings` explains
  why. Projects and users are limited to the first 100; the warning explains
  the truncation (and points to `list_projects` for project pagination).
- **`add_attachment`** accepts standard base64 or a `data:` URI, up to 25 MiB
  decoded. File names cannot contain path separators or control characters.
- **Writes** are followed by a re-read so the model sees the updated issue. If
  the write succeeds but the re-read fails, the tool still reports success (with
  a warning); the write is never repeated automatically.
- Dates use `YYYY-MM-DD`. On update, an empty string clears `start_date` or
  `due_date`.

## Safety model

- **Secret handling.** The API key is used only in `X-Redmine-API-Key`. It is
  never logged, and every `api_key` field is removed from responses before the
  model sees them (Redmine's `/users/current.json` returns the caller's key).
- **Redirects.** Read requests may follow same-origin redirects (or an
  `http` → `https` upgrade on the same host); cross-origin and HTTPS→HTTP
  redirects are refused. Writes never follow redirects, so a write cannot be
  replayed at a different endpoint.
- **Read-only mode.** `REDMINE_READ_ONLY=true` removes and rejects the five write
  tools. Recommended for production instances that agents should not modify.
- **Confirmation is the client's job.** Mutating tools carry MCP annotations
  (`destructiveHint` for updates/transitions) and the MCP `instructions` warn
  the model, but only your agent's approval policy can guarantee that a human
  approved a specific change.
- **Untrusted content.** Issue descriptions, comments and user data are data,
  not instructions. The server tells the model this, but prompt injection is
  still possible; keep your agent's confirmation and sandboxing policies.
- **Bounded I/O.** Responses are capped at 16 MiB and attachments at 25 MiB.
  The rmcp stdio transport is configured to 64 MiB per message (above its 16 MiB
  default) so a 25 MiB attachment fits; HTML error pages are summarized instead
  of being dumped into the model context.
- **Search result links.** URLs returned by `search` are kept only when they
  point at the configured Redmine origin; anything else becomes `null`.
- **Retries.** Only GET requests are retried (up to three attempts) for
  `429/502/503/504`, honoring `Retry-After`. POST/PUT are never retried, so a
  transient failure cannot duplicate a note or an issue.
- **TLS.** rustls with bundled roots; request and connect timeouts are enforced.

See [docs/SECURITY.md](docs/SECURITY.md) for the full threat model and
least-privilege recommendations, and [docs/DESIGN.md](docs/DESIGN.md) for the
prior-art review and design decisions.

## Testing

```bash
cargo test --locked
```

The suite has three layers:

- unit tests inside `src/` for configuration, validation, error mapping and the
  rmcp server adapter;
- `tests/api_tools.rs`, which runs all tools against a local mock Redmine and
  asserts the exact HTTP paths, query parameters, headers and payloads;
- `tests/stdio.rs`, which spawns the real binary and speaks newline-delimited
  JSON-RPC over stdin/stdout (initialize, tools/list, tools/call, errors,
  read-only mode, `--check`).

No Redmine instance, API key or external network access is required. The test
matrix, what is covered and what is intentionally not covered are documented in
[docs/TESTING.md](docs/TESTING.md). Dependency changes are scanned with the
RustSec advisory database in CI. Release notes live in
[CHANGELOG.md](CHANGELOG.md).

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo doc --no-deps --locked
cargo build --release --locked
```

### Layout

```text
src/
  main.rs      CLI and startup
  lib.rs       library surface for tests and embedding
  config.rs    environment/.env validation
  error.rs     error types and user-facing messages
  log.rs       minimal stderr logger
  server.rs    rmcp ServerHandler adapter for the tool registry
  redmine.rs   HTTP client, retries, redaction, error mapping
  tools.rs     tool registry, schemas, handlers and validation
tests/
  common/mod.rs  dependency-free mock Redmine HTTP server
  api_tools.rs   tool-level integration tests
  stdio.rs       end-to-end MCP protocol tests
docs/
  DESIGN.md      prior art, tool-surface rationale and protocol notes
  SECURITY.md    threat model and operational hardening
  TESTING.md     test matrix and manual verification
```

### Live check

With real credentials, this is a read-only smoke test:

```bash
REDMINE_URL=https://redmine.example.com/ \
REDMINE_API_KEY=... \
cargo run --release -- --check
```

## Limitations

- stdio transport only (no Streamable HTTP/SSE).
- No attachment download; `add_attachment` is upload-only.
- No issue deletion, time entries, watcher removal or project administration.
- Setting nullable ids (assignee, version, category) is supported; clearing them
  is not exposed. Dates can be cleared with an empty string.
- API-key authentication only (no OAuth).
- No caching; every tool call goes to Redmine.

## License

MIT. See [LICENSE](LICENSE).
