# Design notes and prior art

This document records the design constraints, the projects reviewed for
inspiration and the decisions that shaped `redmine-mcp-rs`. No code was copied
from the projects below; they were read to compare tool surfaces, security
choices and protocol handling.

## Constraints

- Local **stdio** MCP server spawned by the user's MCP client.
- Single Redmine instance and a single API key.
- Curated tool surface: enough to read and update issues without turning the
  model's context into an API mirror.
- Small, auditable Rust dependency set and a self-contained binary.
- No unsandboxed file access, no shell tools, no general-purpose HTTP client
  exposed to the model.

## Projects reviewed

| Project | What it does | What we took | What we did not take |
| --- | --- | --- | --- |
| [AndreaCadonna/redmine-mcp](https://github.com/AndreaCadonna/redmine-mcp) | Read-only TypeScript server with 35 tools; strong allowlisting, pagination and URL hygiene | The `search` tool design (`GET /search.json` with documented filters and pagination) and the idea of nulling unsafe result URLs | The full 35-tool surface; we keep a smaller set focused on issue work |
| A large Python Redmine MCP server (53 tools) | Multi-user HTTP/OAuth modes, plugin tools, Docker images and prompt-injection boundary tags | Read-only mode, explicit tool descriptions and the lesson that tool sprawl hurts selection and context | OAuth/multi-user auth, HTTP transport, plugin-specific tools and Docker-first deployment; they are out of scope for a local stdio example |
| [modelcontextprotocol/rust-sdk (rmcp)](https://github.com/modelcontextprotocol/rust-sdk) | Official Rust MCP SDK; implements MCP 2026-07-28 and exposes macros, schema generation, transports and structured output | Protocol and testing ideas; the compatibility target to watch | Rewriting to the SDK right now: our manual JSON-RPC core is small, tested and compatible with the client we use (see below) |
| [Redmine REST search](https://www.redmine.org/projects/redmine/wiki/Rest_Search) | Official search endpoint and parameters | The parameter names, limits and result shape for `search` | Undocumented filters or administrator-only controls |

The two third-party servers show the two common deployment models: a read-only
tool set over stdio, and a full multi-user HTTP service with OAuth and Docker.
This project deliberately stays in the first category while keeping a few write
tools for agents that need to update issues.

## Tool surface

The server exposes 13 tools: 8 read-only and 5 write tools. The read side covers
projects, issues, search, reference data, versions and categories. The write
side covers issue creation, updates, notes, transitions and attachments.

`search` was added after the review because `find_issues` only searches the
subject with Redmine's `~` operator. The global search endpoint covers issue
descriptions, comments, wiki pages, news, documents, changesets, messages and
projects. It is read-only, paginated and origin-checked.

We intentionally did **not** add time entries, news, wiki editing, issue
relations management, deletions, watchers or project administration. They are
valid Redmine features, but each one adds schema and context cost, and the
example's goal is a focused issue workflow. Add them as separate tools when a
concrete workflow needs them.

## Protocol compatibility

The server implements the MCP stdio lifecycle directly:

- `initialize` with version negotiation (`2025-11-25`, `2025-06-18`,
  `2025-03-26`, `2024-11-05`, `2024-10-07`);
- `ping`, `tools/list`, `tools/call`;
- JSON-RPC errors, notifications, batch rejection and bounded concurrency.

The official Rust SDK now implements the newer MCP `2026-07-28` lifecycle
(`server/discover`, `_meta`, tasks, stateless HTTP, and so on). Our server does
not advertise that revision; clients that support both negotiate down. This was
verified with the client DSH uses (`@modelcontextprotocol/client` 2.2.0,
`versionNegotiation: auto`): the handshake succeeded, `tools/list` returned 13
tools and `list_projects` read real Redmine data.

If the project needs the 2026-07-28 lifecycle, structured output or a
Streamable HTTP transport, migrating the transport layer to `rmcp` is the
natural next step. The tool handlers, validation and Redmine client can stay.

## Security choices

- API key only in `X-Redmine-API-Key`; never logged; `api_key` fields from
  Redmine responses are removed recursively.
- Redirects: reads may follow same-origin redirects or an `http` → `https`
  upgrade; writes never follow redirects.
- Search result URLs are kept only when they point at the configured Redmine
  origin; otherwise they are returned as `null`.
- `REDMINE_READ_ONLY=true` hides and rejects the write tools.
- POST/PUT are never retried; GET retries only transient status codes.
- Response, message and attachment sizes are capped.
- Issue content is untrusted data. The server says so in `instructions`, but
  only the agent's approval policy can gate writes.

## Deployment

The primary artifact is a native binary launched by the MCP client. A container
would add a Docker daemon, image pulls and startup latency without adding much
isolation for a process that only talks to Redmine. Docker becomes interesting
only for team distribution, strict host isolation, or a future shared HTTP
service. Release binaries are built by GitHub Actions for Linux, macOS and
Windows; see the release workflow.

## Revisit triggers

- The MCP client starts requiring `2026-07-28` features → evaluate `rmcp`.
- The team needs one shared Redmine MCP endpoint → add Streamable HTTP with
  per-user authentication and rate limiting; Docker/Kubernetes then make sense.
- The tool surface grows past roughly 15–20 tools → group or consolidate actions
  and invest in descriptions/selection tests, rather than exposing every
  endpoint.
- Redmine adds a native full-text issue filter → prefer it over `/search.json`
  for issue-only searches.
