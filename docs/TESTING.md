# Testing

## Quick start

```bash
cargo test --locked
```

The suite needs no Redmine instance, API key or external network access. It uses
a local mock HTTP server and spawns the real binary for the protocol tests.

Useful subsets:

```bash
cargo test --locked --lib                 # unit tests
cargo test --locked --test api_tools      # tools against the mock Redmine
cargo test --locked --test stdio          # end-to-end MCP over stdio
cargo test --locked -- --nocapture        # show stdout from failing tests
```

## Suite at a glance

| Layer | Location | Tests | Network |
| --- | --- | ---: | --- |
| Unit | `src/**` `#[cfg(test)]` modules | 44 | none |
| CLI | `src/main.rs` tests | 1 | none |
| Integration | `tests/api_tools.rs` | 27 | localhost mock only |
| End-to-end | `tests/stdio.rs` | 13 | localhost mock only |
| **Total** | | **85** | |

Counts are from the current suite; use `cargo test --locked` to see the exact
numbers on your checkout.

## What is covered

### Configuration (`src/config.rs`)

- Required variables and blank values.
- Optional values: default project, timeout bounds, read-only, log level.
- URL normalization: trailing slash, base paths, percent-encoded paths,
  credentials/query/fragment rejection, `http`/`https` only.
- API key redaction in `Debug` output.

### Server adapter (`src/server.rs`)

- Tool specs deserialize into rmcp `Tool` values; read-only mode filters them.
- Server identity, capabilities and instructions are advertised.

### HTTP client (`src/redmine.rs`)

- Redmine error extraction: `errors` arrays, `error`/`message` fields, HTML error
  pages and empty bodies.
- Recursive `api_key` redaction.
- Safe truncation of multibyte text.
- Retryable status classification and pagination query construction.
- Integration: `X-Redmine-API-Key`/`Accept` headers, query parameters, paths.
- Retries for `503` with `Retry-After`; no retries for writes.
- Same-origin redirects followed for reads; cross-origin redirects refused
  without reaching the target; writes never follow redirects.
- Oversized response rejection (16 MiB cap).
- Raw upload (`application/octet-stream`) and attachment payload construction.

### Tools (`src/tools.rs` and `tests/api_tools.rs`)

- `read_issue`: id validation, include list, query parameter, redaction.
- `find_issues`: project/tracker/author/status/assignee filters, text search
  (`subject_contains` → `subject=~...`), `updated_since`, sort, pagination,
  rejection of unknown filter names, rejection of conflicting subject filters.
- `search`: query/scope/type filters, `all_words`, `titles_only`,
  `open_issues`, `attachments`, pagination, URL origin sanitization and
  pre-HTTP validation.
- `list_projects`: pagination and response envelope.
- `list_trackers_statuses`, `get_project_versions`, `get_project_categories`.
- `get_metadata`: concurrent fetches, users/priorities degradation with warnings,
  truncation warnings.
- `create_issue`: default project, omitted fields, custom fields, watcher
  deduplication, date/range validation.
- `update_issue`: partial payloads, empty description/date handling, notes,
  private notes, re-read fallback that preserves write success.
- `add_issue_note` / `transition_issue`: payloads and pre-HTTP validation.
- `add_attachment`: plain base64 and data URI, filename/content-type validation,
  upload token flow, invalid base64 rejection.
- Page size bounds (1–100, default 25) and exact-subject filtering.
- Read-only mode: tools hidden from `tools/list` and rejected at call time.
- Tool schemas: every spec has a name, description, object input schema and
  annotations.

### MCP transport (`src/server.rs` and `tests/stdio.rs`)

- The protocol/transport layer is the official `rmcp` 3.8 SDK; the tests drive
  it through the real binary over stdio.
- `initialize`/version negotiation, `tools/list` and read-only filtering.
- `tools/call`: success content, tool errors as `isError`, unknown tools and
  methods, null arguments.
- Malformed input does not kill the server; batch requests are rejected.
- stdout carries only protocol JSON; debug logs go to stderr.
- CLI: `--help`, `--version`, `--check` success and failure.
- `.env` precedence: process environment wins, and `.env` fills missing values.
- A 13 MiB attachment (~17 MiB base64 line) is accepted, proving the transport's
  line limit is above the SDK's 16 MiB default for the 25 MiB attachment cap.
- Child process exits cleanly enough for the test harness to kill it.

## Mock Redmine

`tests/common/mod.rs` implements a dependency-free HTTP server:

- binds `127.0.0.1:0` (ephemeral port);
- accepts one connection per thread so concurrent metadata requests work;
- routes with a closure that returns a status, headers and body;
- records method, path, query, headers and body for assertions;
- forces blocking sockets because accepted sockets can inherit a non-blocking
  listener on macOS;
- never contacts an external host.

This makes it possible to assert exact HTTP behavior without a Redmine instance
and without risking writes to a real server.

## Intentionally not covered

These gaps are deliberate; document or add tests if the relevant code changes.

- **Real Redmine writes.** Create/update/note/transition/attachment payloads are
  asserted against the mock. Running them against a production instance would be
  unsafe. Use a staging Redmine and a human review for write-path changes.
- **Search ranking.** `search` is tested for parameter mapping, result shape and
  URL sanitization; Redmine's relevance ranking is upstream behavior and is not
  asserted.
- **Real TLS, proxies and certificate errors.** The mock speaks plain HTTP.
  rustls is compiled and used in production, but the test suite does not perform
  a real TLS handshake.
- **Timeouts and network flakiness.** Retry behavior is tested with `503`; a
  real timeout path is not exercised.
- **Maximum-size boundaries.** The 16 MiB response cap and a >16 MiB stdio line
  are exercised with real payloads; the exact 25 MiB attachment and 64 MiB
  message boundaries are not allocated in CI to keep the suite memory-bounded.
- **`.env` parsing edge cases.** Precedence and missing-value filling are covered
  end-to-end; `dotenvy`'s own parser is trusted and is not fuzzed.
- **Platform matrix.** CI runs on Linux; development and the measured benchmark
  were on macOS. Windows is supported by the dependencies but not exercised in
  this repository's CI.
- **Not implemented:** OAuth, HTTP/SSE transport, attachment download, time
  entries, watcher removal and issue deletion.

## Manual read-only verification

With real credentials:

```bash
REDMINE_URL=https://redmine.example.com/ \
REDMINE_API_KEY=... \
cargo run --release -- --check
```

`--check` performs a single `GET /users/current.json` and prints the
authenticated user. It never prints the key.

For a broader smoke test, call the read-only tools (`list_projects`,
`read_issue`, `find_issues`, `get_metadata`, ...) through your MCP client. Do not
run write tools against production unless you have explicit approval and a
rollback plan.

## Continuous integration

The GitHub Actions workflow runs:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- `cargo doc --no-deps --locked` with `RUSTDOCFLAGS=-D warnings`
- `cargo build --release --locked`
- a minimum-supported-Rust job (`1.88`) with `cargo check --locked`
- a RustSec advisory audit on `Cargo.toml`/`Cargo.lock` changes

Dependabot is configured for Cargo and GitHub Actions updates, and the RustSec
audit workflow runs when dependencies change. See [SECURITY.md](SECURITY.md) for
the supply-chain recommendations.

## Coverage

No coverage threshold is enforced. If you want a local report:

```bash
cargo install cargo-llvm-cov --locked
cargo llvm-cov --locked --html
```

The integration and end-to-end layers exercise the public behavior; unit tests
cover the parsers, validators and protocol state machine where isolated edge
cases are easier to test.
