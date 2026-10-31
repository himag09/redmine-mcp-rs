//! MCP tool definitions and their Redmine operations.
//!
//! Design notes:
//! * Arguments are validated before any HTTP request; unknown fields are
//!   rejected so a typo cannot silently change a write.
//! * Tools return compact JSON; failures become `isError` tool results.
//! * Read-only mode hides and rejects every write tool.
//! * Text search uses Redmine's native `subject=~...` operator through
//!   `subject_contains`; unknown filter names such as `query` are not exposed
//!   because Redmine answers HTTP 500 for them.

use std::collections::BTreeSet;

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::error::{Error, Result};
use crate::redmine::RedmineClient;

/// Maximum decoded attachment size (25 MiB).
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const DEFAULT_PAGE_SIZE: u32 = 25;
const MAX_PAGE_SIZE: u32 = 100;
const SUBJECT_MAX_CHARS: usize = 255;
const FILENAME_MAX_CHARS: usize = 255;

/// Shared application state.
pub struct App {
    pub client: RedmineClient,
    pub default_project_id: Option<u64>,
    pub read_only: bool,
}

impl App {
    pub fn new(config: crate::config::Config) -> Result<Self> {
        Ok(Self {
            client: RedmineClient::new(&config)?,
            default_project_id: config.default_project_id,
            read_only: config.read_only,
        })
    }
}

/// Tools that modify Redmine. Everything else is read-only.
pub fn is_write(name: &str) -> bool {
    matches!(
        name,
        "create_issue" | "update_issue" | "add_issue_note" | "transition_issue" | "add_attachment"
    )
}

pub fn is_known(name: &str) -> bool {
    matches!(
        name,
        "create_issue"
            | "update_issue"
            | "list_projects"
            | "list_trackers_statuses"
            | "read_issue"
            | "add_issue_note"
            | "transition_issue"
            | "find_issues"
            | "search"
            | "add_attachment"
            | "get_project_versions"
            | "get_project_categories"
            | "get_metadata"
    )
}

/// Tool descriptors for `tools/list`.
pub fn specs(read_only: bool) -> Vec<Value> {
    let mut tools = vec![
        create_issue_spec(),
        update_issue_spec(),
        list_projects_spec(),
        list_trackers_statuses_spec(),
        read_issue_spec(),
        add_issue_note_spec(),
        transition_issue_spec(),
        find_issues_spec(),
        search_spec(),
        add_attachment_spec(),
        get_project_versions_spec(),
        get_project_categories_spec(),
        get_metadata_spec(),
    ];
    if read_only {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(|name| !is_write(name))
                .unwrap_or(false)
        });
    }
    tools
}

/// Executes a tool call. Errors become `isError` tool results in the MCP layer.
pub async fn call(app: &App, name: &str, args: Value) -> Result<Value> {
    if app.read_only && is_write(name) {
        return Err(Error::validation(format!(
            "tool {name} is disabled because REDMINE_READ_ONLY is enabled"
        )));
    }

    match name {
        "create_issue" => create_issue(app, args).await,
        "update_issue" => update_issue(app, args).await,
        "list_projects" => list_projects(app, args).await,
        "list_trackers_statuses" => list_trackers_statuses(app, args).await,
        "read_issue" => read_issue(app, args).await,
        "add_issue_note" => add_issue_note(app, args).await,
        "transition_issue" => transition_issue(app, args).await,
        "find_issues" => find_issues(app, args).await,
        "search" => search(app, args).await,
        "add_attachment" => add_attachment(app, args).await,
        "get_project_versions" => get_project_versions(app, args).await,
        "get_project_categories" => get_project_categories(app, args).await,
        "get_metadata" => get_metadata(app, args).await,
        other => Err(Error::protocol(format!("unknown tool: {other}"))),
    }
}

// --- Handlers -----------------------------------------------------------

async fn create_issue(app: &App, args: Value) -> Result<Value> {
    let args: CreateIssueArgs = parse_args("create_issue", args)?;
    let issue = build_create_issue(args, app.default_project_id)?;
    let response = app.client.create_issue(&issue).await?;
    Ok(unwrap_issue(response))
}

async fn update_issue(app: &App, args: Value) -> Result<Value> {
    let args: UpdateIssueArgs = parse_args("update_issue", args)?;
    ensure_positive("issue_id", args.issue_id)?;
    let issue = build_update_issue(&args)?;
    app.client.update_issue(args.issue_id, &issue).await?;
    refetch_issue(app, args.issue_id, "update_issue").await
}

async fn list_projects(app: &App, args: Value) -> Result<Value> {
    let args: PageArgs = parse_args("list_projects", args)?;
    let limit = validate_limit(args.limit)?;
    let offset = args.offset.unwrap_or(0);
    app.client.list_projects(limit, offset).await
}

async fn list_trackers_statuses(app: &App, args: Value) -> Result<Value> {
    let _: EmptyArgs = parse_args("list_trackers_statuses", args)?;
    let (trackers, statuses) =
        tokio::join!(app.client.get_trackers(), app.client.get_issue_statuses());
    Ok(json!({
        "trackers": array_field(trackers?, "trackers"),
        "statuses": array_field(statuses?, "issue_statuses"),
    }))
}

async fn read_issue(app: &App, args: Value) -> Result<Value> {
    let args: ReadIssueArgs = parse_args("read_issue", args)?;
    ensure_positive("issue_id", args.issue_id)?;
    let include = match args.include {
        Some(include) => validate_includes(&include)?,
        None => Vec::new(),
    };
    let response = app.client.get_issue(args.issue_id, &include).await?;
    Ok(unwrap_issue(response))
}

async fn add_issue_note(app: &App, args: Value) -> Result<Value> {
    let args: AddNoteArgs = parse_args("add_issue_note", args)?;
    ensure_positive("issue_id", args.issue_id)?;
    let notes = validate_non_empty("notes", &args.notes)?;
    app.client
        .add_note(args.issue_id, &notes, args.private_notes.unwrap_or(false))
        .await?;
    refetch_issue(app, args.issue_id, "add_issue_note").await
}

async fn transition_issue(app: &App, args: Value) -> Result<Value> {
    let args: TransitionArgs = parse_args("transition_issue", args)?;
    ensure_positive("issue_id", args.issue_id)?;
    ensure_positive("status_id", args.status_id)?;

    let notes = match &args.notes {
        Some(notes) => Some(validate_non_empty("notes", notes)?),
        None => None,
    };
    let private_notes = args.private_notes.unwrap_or(false);
    if private_notes && notes.is_none() {
        return Err(Error::validation("private_notes requires notes"));
    }

    app.client
        .transition_issue(
            args.issue_id,
            args.status_id,
            notes.as_deref(),
            private_notes,
        )
        .await?;
    refetch_issue(app, args.issue_id, "transition_issue").await
}

async fn find_issues(app: &App, args: Value) -> Result<Value> {
    let args: FindIssuesArgs = parse_args("find_issues", args)?;
    let query = build_issue_query(&args)?;
    app.client.find_issues(&query).await
}

async fn search(app: &App, args: Value) -> Result<Value> {
    let args: SearchArgs = parse_args("search", args)?;
    let query = build_search_query(&args)?;
    let mut response = app.client.search(&query).await?;
    sanitize_search_urls(&mut response, app.client.base_url());
    Ok(response)
}

async fn add_attachment(app: &App, args: Value) -> Result<Value> {
    let args: AddAttachmentArgs = parse_args("add_attachment", args)?;
    ensure_positive("issue_id", args.issue_id)?;
    let filename = validate_filename(&args.filename)?;
    let content_type = match args.content_type.as_deref() {
        Some(value) if !value.trim().is_empty() => Some(validate_content_type(value)?),
        _ => None,
    };
    let description = args
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let data = decode_attachment(&args.data_base64)?;
    let token = app.client.upload(&data, &filename).await?;
    app.client
        .attach(
            args.issue_id,
            &token,
            &filename,
            content_type.as_deref(),
            description,
        )
        .await?;
    refetch_issue(app, args.issue_id, "add_attachment").await
}

async fn get_project_versions(app: &App, args: Value) -> Result<Value> {
    let args: ProjectArgs = parse_args("get_project_versions", args)?;
    ensure_positive("project_id", args.project_id)?;
    app.client.get_project_versions(args.project_id).await
}

async fn get_project_categories(app: &App, args: Value) -> Result<Value> {
    let args: ProjectArgs = parse_args("get_project_categories", args)?;
    ensure_positive("project_id", args.project_id)?;
    app.client.get_project_categories(args.project_id).await
}

async fn get_metadata(app: &App, args: Value) -> Result<Value> {
    let _: EmptyArgs = parse_args("get_metadata", args)?;

    let (projects, trackers, statuses, users, priorities) = tokio::join!(
        app.client.list_projects(MAX_PAGE_SIZE, 0),
        app.client.get_trackers(),
        app.client.get_issue_statuses(),
        app.client.get_users(MAX_PAGE_SIZE, 0),
        app.client.get_priorities(),
    );

    // Fundamental reference data: fail loudly if unavailable.
    let projects = projects?;
    let trackers = trackers?;
    let statuses = statuses?;

    let mut warnings = Vec::new();

    let users = match users {
        Ok(value) => {
            warn_if_truncated(&value, "users", None, &mut warnings);
            array_field(value, "users")
        }
        Err(error) => {
            warnings.push(format!("users unavailable: {error}"));
            json!([])
        }
    };

    let priorities = match priorities {
        Ok(value) => array_field(value, "issue_priorities"),
        Err(error) => {
            warnings.push(format!("priorities unavailable: {error}"));
            json!([])
        }
    };

    warn_if_truncated(
        &projects,
        "projects",
        Some("use list_projects with offset for the rest"),
        &mut warnings,
    );

    Ok(json!({
        "projects": array_field(projects, "projects"),
        "trackers": array_field(trackers, "trackers"),
        "statuses": array_field(statuses, "issue_statuses"),
        "users": users,
        "priorities": priorities,
        "warnings": warnings,
    }))
}

async fn refetch_issue(app: &App, issue_id: u64, action: &str) -> Result<Value> {
    match app.client.get_issue(issue_id, &[]).await {
        Ok(value) => Ok(unwrap_issue(value)),
        Err(error) => Ok(json!({
            "id": issue_id,
            "succeeded": true,
            "warning": format!("{action} succeeded but the issue could not be re-read: {error}"),
        })),
    }
}

// --- Argument types -----------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageArgs {
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    project_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadIssueArgs {
    issue_id: u64,
    include: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateIssueArgs {
    project_id: Option<u64>,
    subject: String,
    description: Option<String>,
    tracker_id: Option<u64>,
    status_id: Option<u64>,
    priority_id: Option<u64>,
    assigned_to_id: Option<u64>,
    start_date: Option<String>,
    due_date: Option<String>,
    parent_issue_id: Option<u64>,
    fixed_version_id: Option<u64>,
    category_id: Option<u64>,
    estimated_hours: Option<f64>,
    done_ratio: Option<u32>,
    custom_fields: Option<Vec<CustomFieldArg>>,
    watcher_user_ids: Option<Vec<u64>>,
    is_private: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateIssueArgs {
    issue_id: u64,
    subject: Option<String>,
    description: Option<String>,
    tracker_id: Option<u64>,
    status_id: Option<u64>,
    priority_id: Option<u64>,
    assigned_to_id: Option<u64>,
    start_date: Option<String>,
    due_date: Option<String>,
    parent_issue_id: Option<u64>,
    fixed_version_id: Option<u64>,
    category_id: Option<u64>,
    estimated_hours: Option<f64>,
    done_ratio: Option<u32>,
    custom_fields: Option<Vec<CustomFieldArg>>,
    watcher_user_ids: Option<Vec<u64>>,
    is_private: Option<bool>,
    notes: Option<String>,
    private_notes: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddNoteArgs {
    issue_id: u64,
    notes: String,
    private_notes: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionArgs {
    issue_id: u64,
    status_id: u64,
    notes: Option<String>,
    private_notes: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindIssuesArgs {
    project_id: Option<u64>,
    tracker_id: Option<u64>,
    status_id: Option<StatusFilter>,
    assigned_to_id: Option<AssigneeFilter>,
    author_id: Option<u64>,
    subject: Option<String>,
    subject_contains: Option<String>,
    updated_since: Option<String>,
    sort: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    scope: Option<String>,
    all_words: Option<bool>,
    titles_only: Option<bool>,
    issues: Option<bool>,
    news: Option<bool>,
    documents: Option<bool>,
    changesets: Option<bool>,
    wiki_pages: Option<bool>,
    messages: Option<bool>,
    projects: Option<bool>,
    open_issues: Option<bool>,
    attachments: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StatusFilter {
    Id(u64),
    Keyword(String),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AssigneeFilter {
    Id(u64),
    Keyword(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAttachmentArgs {
    issue_id: u64,
    filename: String,
    data_base64: String,
    content_type: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CustomFieldArg {
    id: u64,
    value: Value,
}

fn parse_args<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|error| {
        Error::validation(format!(
            "invalid arguments for {tool}: {error}. Check the tool inputSchema."
        ))
    })
}

// --- Payload builders ---------------------------------------------------

fn build_create_issue(args: CreateIssueArgs, default_project_id: Option<u64>) -> Result<Value> {
    let project_id = args.project_id.or(default_project_id).ok_or_else(|| {
        Error::validation("project_id is required when REDMINE_DEFAULT_PROJECT_ID is not set")
    })?;
    ensure_positive("project_id", project_id)?;
    let subject = validate_subject(&args.subject)?;

    let mut issue = Map::new();
    issue.insert("project_id".to_string(), json!(project_id));
    issue.insert("subject".to_string(), json!(subject));

    if let Some(description) = args.description {
        issue.insert("description".to_string(), json!(description));
    }
    insert_id(&mut issue, "tracker_id", args.tracker_id)?;
    insert_id(&mut issue, "status_id", args.status_id)?;
    insert_id(&mut issue, "priority_id", args.priority_id)?;
    insert_id(&mut issue, "assigned_to_id", args.assigned_to_id)?;
    insert_id(&mut issue, "parent_issue_id", args.parent_issue_id)?;
    insert_id(&mut issue, "fixed_version_id", args.fixed_version_id)?;
    insert_id(&mut issue, "category_id", args.category_id)?;

    if let Some(date) = args.start_date {
        validate_date("start_date", &date)?;
        issue.insert("start_date".to_string(), json!(date));
    }
    if let Some(date) = args.due_date {
        validate_date("due_date", &date)?;
        issue.insert("due_date".to_string(), json!(date));
    }
    if let Some(hours) = args.estimated_hours {
        issue.insert("estimated_hours".to_string(), estimated_hours_value(hours)?);
    }
    if let Some(ratio) = args.done_ratio {
        validate_done_ratio(ratio)?;
        issue.insert("done_ratio".to_string(), json!(ratio));
    }
    if let Some(fields) = &args.custom_fields {
        validate_custom_fields(fields)?;
        issue.insert("custom_fields".to_string(), serde_json::to_value(fields)?);
    }
    if let Some(ids) = &args.watcher_user_ids {
        validate_watchers(ids)?;
        issue.insert("watcher_user_ids".to_string(), json!(dedup_ids(ids)));
    }
    if let Some(private) = args.is_private {
        issue.insert("is_private".to_string(), json!(private));
    }

    Ok(Value::Object(issue))
}

fn build_update_issue(args: &UpdateIssueArgs) -> Result<Value> {
    let mut issue = Map::new();

    if let Some(subject) = &args.subject {
        issue.insert("subject".to_string(), json!(validate_subject(subject)?));
    }
    if let Some(description) = &args.description {
        // An empty string is how Redmine clears a description.
        issue.insert("description".to_string(), json!(description));
    }
    insert_id(&mut issue, "tracker_id", args.tracker_id)?;
    insert_id(&mut issue, "status_id", args.status_id)?;
    insert_id(&mut issue, "priority_id", args.priority_id)?;
    insert_id(&mut issue, "assigned_to_id", args.assigned_to_id)?;
    insert_id(&mut issue, "parent_issue_id", args.parent_issue_id)?;
    insert_id(&mut issue, "fixed_version_id", args.fixed_version_id)?;
    insert_id(&mut issue, "category_id", args.category_id)?;

    if let Some(date) = &args.start_date {
        validate_date_or_empty("start_date", date)?;
        issue.insert("start_date".to_string(), json!(date));
    }
    if let Some(date) = &args.due_date {
        validate_date_or_empty("due_date", date)?;
        issue.insert("due_date".to_string(), json!(date));
    }
    if let Some(hours) = args.estimated_hours {
        issue.insert("estimated_hours".to_string(), estimated_hours_value(hours)?);
    }
    if let Some(ratio) = args.done_ratio {
        validate_done_ratio(ratio)?;
        issue.insert("done_ratio".to_string(), json!(ratio));
    }
    if let Some(fields) = &args.custom_fields {
        validate_custom_fields(fields)?;
        issue.insert("custom_fields".to_string(), serde_json::to_value(fields)?);
    }
    if let Some(ids) = &args.watcher_user_ids {
        validate_watchers(ids)?;
        issue.insert("watcher_user_ids".to_string(), json!(dedup_ids(ids)));
    }
    if let Some(private) = args.is_private {
        issue.insert("is_private".to_string(), json!(private));
    }

    match &args.notes {
        Some(notes) => {
            let notes = validate_non_empty("notes", notes)?;
            issue.insert("notes".to_string(), json!(notes));
            if args.private_notes.unwrap_or(false) {
                issue.insert("private_notes".to_string(), json!(true));
            }
        }
        None if args.private_notes.is_some() => {
            return Err(Error::validation("private_notes requires notes"));
        }
        None => {}
    }

    if issue.is_empty() {
        return Err(Error::validation(
            "nothing to update: provide at least one field or notes",
        ));
    }

    Ok(Value::Object(issue))
}

fn build_issue_query(args: &FindIssuesArgs) -> Result<Vec<(String, String)>> {
    if args.subject.is_some() && args.subject_contains.is_some() {
        return Err(Error::validation(
            "use either subject (exact) or subject_contains (substring), not both",
        ));
    }

    let mut query = Vec::new();

    if let Some(id) = args.project_id {
        ensure_positive("project_id", id)?;
        query.push(("project_id".to_string(), id.to_string()));
    }
    if let Some(id) = args.tracker_id {
        ensure_positive("tracker_id", id)?;
        query.push(("tracker_id".to_string(), id.to_string()));
    }
    if let Some(id) = args.author_id {
        ensure_positive("author_id", id)?;
        query.push(("author_id".to_string(), id.to_string()));
    }
    if let Some(status) = &args.status_id {
        let value = match status {
            StatusFilter::Id(id) => {
                ensure_positive("status_id", *id)?;
                id.to_string()
            }
            StatusFilter::Keyword(keyword) => match keyword.to_ascii_lowercase().as_str() {
                "open" | "closed" | "*" => keyword.to_ascii_lowercase(),
                other => {
                    return Err(Error::validation(format!(
                        "status_id keyword must be one of open, closed or * (got {other:?})"
                    )));
                }
            },
        };
        query.push(("status_id".to_string(), value));
    }
    if let Some(assignee) = &args.assigned_to_id {
        let value = match assignee {
            AssigneeFilter::Id(id) => {
                ensure_positive("assigned_to_id", *id)?;
                id.to_string()
            }
            AssigneeFilter::Keyword(keyword) => {
                if keyword.eq_ignore_ascii_case("me") {
                    "me".to_string()
                } else {
                    return Err(Error::validation(format!(
                        "assigned_to_id keyword must be me (got {keyword:?}); use a numeric user id otherwise"
                    )));
                }
            }
        };
        query.push(("assigned_to_id".to_string(), value));
    }
    if let Some(subject) = &args.subject {
        let subject = validate_non_empty("subject", subject)?;
        query.push(("subject".to_string(), subject));
    }
    if let Some(subject) = &args.subject_contains {
        let subject = validate_non_empty("subject_contains", subject)?;
        // Redmine's "~" operator means "contains".
        query.push(("subject".to_string(), format!("~{subject}")));
    }
    if let Some(date) = &args.updated_since {
        validate_date("updated_since", date)?;
        query.push(("updated_on".to_string(), format!(">={date}")));
    }
    if let Some(sort) = &args.sort {
        let sort = validate_sort(sort)?;
        query.push(("sort".to_string(), sort));
    }

    query.push(("limit".to_string(), validate_limit(args.limit)?.to_string()));
    query.push(("offset".to_string(), args.offset.unwrap_or(0).to_string()));
    Ok(query)
}

fn build_search_query(args: &SearchArgs) -> Result<Vec<(String, String)>> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(Error::validation("query must not be empty"));
    }
    if query.chars().count() > 500 {
        return Err(Error::validation("query must be at most 500 characters"));
    }

    let mut params = vec![("q".to_string(), query.to_string())];

    if let Some(scope) = &args.scope {
        let scope = match scope.to_ascii_lowercase().as_str() {
            "all" | "subprojects" | "bookmarks" => scope.to_ascii_lowercase(),
            // Redmine's wiki documents "my_project"; current controllers use
            // "my_projects". Accept both and send the current spelling.
            "my_project" | "my_projects" => "my_projects".to_string(),
            other => {
                return Err(Error::validation(format!(
                    "scope must be one of all, my_projects, subprojects or bookmarks (got {other:?})"
                )));
            }
        };
        params.push(("scope".to_string(), scope));
    }

    for (name, enabled) in [
        ("issues", args.issues),
        ("news", args.news),
        ("documents", args.documents),
        ("changesets", args.changesets),
        ("wiki_pages", args.wiki_pages),
        ("messages", args.messages),
        ("projects", args.projects),
    ] {
        if enabled == Some(true) {
            params.push((name.to_string(), "1".to_string()));
        }
    }

    match args.all_words {
        Some(true) => params.push(("all_words".to_string(), "1".to_string())),
        // Redmine's controller treats a present empty value as false.
        Some(false) => params.push(("all_words".to_string(), String::new())),
        None => {}
    }
    if args.titles_only == Some(true) {
        params.push(("titles_only".to_string(), "1".to_string()));
    }
    if args.open_issues == Some(true) {
        params.push(("open_issues".to_string(), "1".to_string()));
    }
    if let Some(attachments) = &args.attachments {
        let attachments = attachments.to_ascii_lowercase();
        if !matches!(attachments.as_str(), "0" | "1" | "only") {
            return Err(Error::validation("attachments must be 0, 1 or only"));
        }
        params.push(("attachments".to_string(), attachments));
    }

    params.push(("limit".to_string(), validate_limit(args.limit)?.to_string()));
    params.push(("offset".to_string(), args.offset.unwrap_or(0).to_string()));
    Ok(params)
}

/// Keeps only search-result URLs that point at the configured Redmine origin.
/// Redmine normally returns absolute same-origin links; anything else becomes
/// `null` so a compromised or misconfigured server cannot hand the model an
/// arbitrary external URL.
fn sanitize_search_urls(value: &mut Value, base_url: &str) {
    let Ok(base) = Url::parse(base_url) else {
        return;
    };
    let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) else {
        return;
    };

    for result in results {
        let Some(result) = result.as_object_mut() else {
            continue;
        };
        let Some(raw) = result.get("url").and_then(Value::as_str) else {
            continue;
        };
        let safe = Url::parse(raw).is_ok_and(|candidate| {
            candidate.scheme() == base.scheme()
                && candidate.host_str() == base.host_str()
                && candidate.port_or_known_default() == base.port_or_known_default()
                && candidate.path().starts_with(base.path())
                && candidate.username().is_empty()
                && candidate.password().is_none()
        });
        if !safe {
            result.insert("url".to_string(), Value::Null);
        }
    }
}

fn insert_id(issue: &mut Map<String, Value>, name: &str, value: Option<u64>) -> Result<()> {
    if let Some(id) = value {
        ensure_positive(name, id)?;
        issue.insert(name.to_string(), json!(id));
    }
    Ok(())
}

fn unwrap_issue(value: Value) -> Value {
    match value {
        Value::Object(mut map) => match map.remove("issue") {
            Some(issue) => issue,
            None => Value::Object(map),
        },
        other => other,
    }
}

fn array_field(value: Value, key: &str) -> Value {
    value
        .get(key)
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

fn warn_if_truncated(
    response: &Value,
    collection: &str,
    hint: Option<&str>,
    warnings: &mut Vec<String>,
) {
    let total = response.get("total_count").and_then(Value::as_u64);
    if let Some(total) = total
        && total > u64::from(MAX_PAGE_SIZE)
    {
        let hint = hint.map(|hint| format!("; {hint}")).unwrap_or_default();
        warnings.push(format!(
            "only the first {MAX_PAGE_SIZE} of {total} {collection} were returned{hint}"
        ));
    }
}

// --- Validation ---------------------------------------------------------

fn ensure_positive(name: &str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(Error::validation(format!("{name} must be greater than 0")));
    }
    Ok(())
}

fn validate_limit(limit: Option<u32>) -> Result<u32> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(Error::validation(format!(
            "limit must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(limit)
}

fn validate_subject(subject: &str) -> Result<String> {
    let subject = subject.trim();
    if subject.is_empty() {
        return Err(Error::validation("subject must not be empty"));
    }
    if subject.chars().count() > SUBJECT_MAX_CHARS {
        return Err(Error::validation(format!(
            "subject must be at most {SUBJECT_MAX_CHARS} characters"
        )));
    }
    Ok(subject.to_string())
}

fn validate_non_empty(name: &str, value: &str) -> Result<String> {
    if value.trim().is_empty() {
        return Err(Error::validation(format!("{name} must not be empty")));
    }
    Ok(value.to_string())
}

fn validate_done_ratio(ratio: u32) -> Result<()> {
    if ratio > 100 {
        return Err(Error::validation("done_ratio must be between 0 and 100"));
    }
    Ok(())
}

fn estimated_hours_value(hours: f64) -> Result<Value> {
    if !hours.is_finite() || hours < 0.0 {
        return Err(Error::validation(
            "estimated_hours must be a finite number greater than or equal to 0",
        ));
    }
    serde_json::Number::from_f64(hours)
        .map(Value::Number)
        .ok_or_else(|| Error::validation("estimated_hours is not a valid number"))
}

fn validate_date(name: &str, value: &str) -> Result<()> {
    if is_valid_date(value) {
        Ok(())
    } else {
        Err(Error::validation(format!(
            "{name} must be a valid date in YYYY-MM-DD format (got {value:?})"
        )))
    }
}

fn validate_date_or_empty(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        Ok(())
    } else {
        validate_date(name, value)
    }
}

fn is_valid_date(value: &str) -> bool {
    if !value.is_ascii() || value.len() != 10 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        value[0..4].parse::<u32>(),
        value[5..7].parse::<u32>(),
        value[8..10].parse::<u32>(),
    ) else {
        return false;
    };
    if year == 0 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

fn validate_sort(sort: &str) -> Result<String> {
    let sort = sort.trim();
    if sort.is_empty() || sort.len() > 200 {
        return Err(Error::validation(
            "sort must be a non-empty Redmine sort expression, for example updated_on:desc",
        ));
    }
    for part in sort.split(',') {
        let mut pieces = part.split(':');
        let field = pieces.next().unwrap_or("");
        if field.is_empty()
            || !field
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(Error::validation(format!(
                "invalid sort field {field:?}; use field names such as updated_on or priority"
            )));
        }
        match pieces.next() {
            None => {}
            Some(direction)
                if direction.eq_ignore_ascii_case("asc")
                    || direction.eq_ignore_ascii_case("desc") => {}
            Some(direction) => {
                return Err(Error::validation(format!(
                    "invalid sort direction {direction:?}; use asc or desc"
                )));
            }
        }
        if pieces.next().is_some() {
            return Err(Error::validation(format!(
                "invalid sort expression {part:?}"
            )));
        }
    }
    Ok(sort.to_string())
}

fn validate_includes(values: &[String]) -> Result<Vec<String>> {
    const ALLOWED: [&str; 6] = [
        "journals",
        "relations",
        "attachments",
        "children",
        "changesets",
        "allowed_statuses",
    ];
    let mut seen = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if !ALLOWED.contains(&value) {
            return Err(Error::validation(format!(
                "include values must be one of {} (got {value:?})",
                ALLOWED.join(", ")
            )));
        }
        seen.insert(value.to_string());
    }
    Ok(seen.into_iter().collect())
}

fn validate_custom_fields(fields: &[CustomFieldArg]) -> Result<()> {
    for field in fields {
        ensure_positive("custom_fields[].id", field.id)?;
        let valid = match &field.value {
            Value::String(_) | Value::Number(_) => true,
            Value::Array(items) => items.iter().all(Value::is_string),
            _ => false,
        };
        if !valid {
            return Err(Error::validation(format!(
                "custom field {} value must be a string, a number or an array of strings",
                field.id
            )));
        }
    }
    Ok(())
}

fn validate_watchers(ids: &[u64]) -> Result<()> {
    for id in ids {
        ensure_positive("watcher_user_ids[]", *id)?;
    }
    Ok(())
}

fn dedup_ids(ids: &[u64]) -> Vec<u64> {
    let mut seen = BTreeSet::new();
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

fn validate_filename(filename: &str) -> Result<String> {
    let filename = filename.trim();
    if filename.is_empty() {
        return Err(Error::validation("filename must not be empty"));
    }
    if filename.chars().count() > FILENAME_MAX_CHARS {
        return Err(Error::validation(format!(
            "filename must be at most {FILENAME_MAX_CHARS} characters"
        )));
    }
    if filename == "." || filename == ".." {
        return Err(Error::validation("filename is not a valid file name"));
    }
    if filename
        .chars()
        .any(|character| character == '/' || character == '\\' || character.is_control())
    {
        return Err(Error::validation(
            "filename must not contain path separators or control characters",
        ));
    }
    Ok(filename.to_string())
}

fn validate_content_type(content_type: &str) -> Result<String> {
    let content_type = content_type.trim();
    if content_type.is_empty()
        || content_type.len() > FILENAME_MAX_CHARS
        || !content_type.contains('/')
        || content_type.chars().any(char::is_control)
    {
        return Err(Error::validation(format!(
            "content_type must be a valid MIME type such as text/plain (got {content_type:?})"
        )));
    }
    Ok(content_type.to_string())
}

fn decode_attachment(data_base64: &str) -> Result<Vec<u8>> {
    let raw = data_base64.trim();
    let raw = if raw.len() >= 5 && raw.as_bytes()[..5].eq_ignore_ascii_case(b"data:") {
        let (metadata, payload) = raw[5..].split_once(',').ok_or_else(|| {
            Error::validation("data_base64 looks like a data URI but has no comma separator")
        })?;
        if !metadata.to_ascii_lowercase().contains(";base64") {
            return Err(Error::validation(
                "data_base64 data URI must be base64-encoded (missing ;base64)",
            ));
        }
        payload
    } else {
        raw
    };

    let encoded: String = raw
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if encoded.is_empty() {
        return Err(Error::validation("data_base64 must not be empty"));
    }

    // Fail before allocating the decoded buffer when the payload is clearly
    // too large. The exact check happens after decoding.
    let max_encoded = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 8;
    if encoded.len() > max_encoded {
        return Err(Error::validation(format!(
            "attachment is too large (limit {} MiB decoded)",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        )));
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded.as_bytes()))
        .map_err(|error| Error::validation(format!("data_base64 is not valid base64: {error}")))?;

    if bytes.is_empty() {
        return Err(Error::validation("attachment is empty"));
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(Error::validation(format!(
            "attachment is too large (limit {} MiB decoded)",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

// --- JSON Schemas -------------------------------------------------------

fn id_prop(description: &str) -> Value {
    json!({ "type": "integer", "minimum": 1, "description": description })
}

fn date_prop(description: &str) -> Value {
    json!({
        "type": "string",
        "pattern": "^\\d{4}-\\d{2}-\\d{2}$",
        "description": description
    })
}

fn limit_prop() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_PAGE_SIZE,
        "default": DEFAULT_PAGE_SIZE,
        "description": format!("Page size (1-{MAX_PAGE_SIZE}, default {DEFAULT_PAGE_SIZE}).")
    })
}

fn offset_prop() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "default": 0,
        "description": "Number of records to skip."
    })
}

fn custom_fields_prop() -> Value {
    json!({
        "type": "array",
        "description": "Custom field values. Use an array for multi-value fields.",
        "items": {
            "type": "object",
            "properties": {
                "id": { "type": "integer", "minimum": 1 },
                "value": {
                    "oneOf": [
                        { "type": "string" },
                        { "type": "number" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                }
            },
            "required": ["id", "value"],
            "additionalProperties": false
        }
    })
}

fn watchers_prop() -> Value {
    json!({
        "type": "array",
        "description": "User ids to add as watchers (they receive notifications).",
        "items": { "type": "integer", "minimum": 1 }
    })
}

fn project_id_prop() -> Value {
    id_prop("Project id. Defaults to REDMINE_DEFAULT_PROJECT_ID when omitted.")
}

fn read_annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": true
    })
}

fn write_annotations(destructive: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint": false,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": true
    })
}

fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn create_issue_spec() -> Value {
    json!({
        "name": "create_issue",
        "title": "Create issue",
        "description": "Create a Redmine issue. WARNING: writes to the configured Redmine instance; call it only after the user explicitly confirms the exact issue.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project_id": project_id_prop(),
                "subject": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": SUBJECT_MAX_CHARS,
                    "description": "Issue subject."
                },
                "description": {
                    "type": "string",
                    "description": "Issue description (Redmine supports Textile/Markdown)."
                },
                "tracker_id": id_prop("Tracker id (see list_trackers_statuses)."),
                "status_id": id_prop("Initial status id. Workflow rules apply."),
                "priority_id": id_prop("Priority id (see get_metadata)."),
                "assigned_to_id": id_prop("Assignee user id (see get_metadata)."),
                "start_date": date_prop("Start date in YYYY-MM-DD format."),
                "due_date": date_prop("Due date in YYYY-MM-DD format."),
                "parent_issue_id": id_prop("Parent issue id."),
                "fixed_version_id": id_prop("Target version (milestone) id; see get_project_versions."),
                "category_id": id_prop("Issue category id; see get_project_categories."),
                "estimated_hours": { "type": "number", "minimum": 0, "description": "Estimated hours (non-negative)." },
                "done_ratio": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Completion percentage (0-100)." },
                "custom_fields": custom_fields_prop(),
                "watcher_user_ids": watchers_prop(),
                "is_private": { "type": "boolean", "description": "Create a private issue (Redmine 5.1+)." }
            },
            "required": ["subject"],
            "additionalProperties": false
        },
        "annotations": write_annotations(false, false)
    })
}

fn update_issue_spec() -> Value {
    json!({
        "name": "update_issue",
        "title": "Update issue",
        "description": "Update issue fields and/or add a comment. Only the fields you send are changed; at least one field or notes is required. WARNING: writes to the configured Redmine instance; call it only after the user explicitly confirms the exact change.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_id": id_prop("Issue id to update."),
                "subject": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": SUBJECT_MAX_CHARS,
                    "description": "New subject."
                },
                "description": {
                    "type": "string",
                    "description": "New description. An empty string clears it."
                },
                "tracker_id": id_prop("New tracker id."),
                "status_id": id_prop("New status id. Workflow rules apply; see transition_issue."),
                "priority_id": id_prop("New priority id."),
                "assigned_to_id": id_prop("New assignee user id."),
                "start_date": date_prop("Start date (YYYY-MM-DD); an empty string clears it."),
                "due_date": date_prop("Due date (YYYY-MM-DD); an empty string clears it."),
                "parent_issue_id": id_prop("New parent issue id."),
                "fixed_version_id": id_prop("New target version id."),
                "category_id": id_prop("New issue category id."),
                "estimated_hours": { "type": "number", "minimum": 0, "description": "Estimated hours (non-negative)." },
                "done_ratio": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Completion percentage (0-100)." },
                "custom_fields": custom_fields_prop(),
                "watcher_user_ids": watchers_prop(),
                "is_private": { "type": "boolean", "description": "Make the issue private or public (Redmine 5.1+)." },
                "notes": { "type": "string", "minLength": 1, "description": "Comment to add together with the field changes." },
                "private_notes": { "type": "boolean", "description": "Make the comment private. Requires notes." }
            },
            "required": ["issue_id"],
            "additionalProperties": false
        },
        "annotations": write_annotations(true, true)
    })
}

fn list_projects_spec() -> Value {
    json!({
        "name": "list_projects",
        "title": "List projects",
        "description": "List Redmine projects with pagination. Returns {projects, total_count, offset, limit}.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": limit_prop(),
                "offset": offset_prop()
            },
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn list_trackers_statuses_spec() -> Value {
    json!({
        "name": "list_trackers_statuses",
        "title": "List trackers and statuses",
        "description": "List all trackers and issue statuses in one call. Use the returned ids in create_issue, update_issue and transition_issue.",
        "inputSchema": empty_schema(),
        "annotations": read_annotations()
    })
}

fn read_issue_spec() -> Value {
    json!({
        "name": "read_issue",
        "title": "Read issue",
        "description": "Read a single issue by id. Use include=[\"journals\"] to fetch comments and history, [\"attachments\"] for attachment metadata, or [\"allowed_statuses\"] to discover which status transitions the workflow allows.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_id": id_prop("Issue id."),
                "include": {
                    "type": "array",
                    "description": "Optional related data to embed in the response.",
                    "items": {
                        "type": "string",
                        "enum": ["journals", "relations", "attachments", "children", "changesets", "allowed_statuses"]
                    }
                }
            },
            "required": ["issue_id"],
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn add_issue_note_spec() -> Value {
    json!({
        "name": "add_issue_note",
        "title": "Add issue note",
        "description": "Add a comment to an issue. WARNING: writes to the configured Redmine instance; call it only after the user explicitly confirms the exact comment.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_id": id_prop("Issue id."),
                "notes": { "type": "string", "minLength": 1, "description": "Comment text." },
                "private_notes": { "type": "boolean", "description": "Make the comment private." }
            },
            "required": ["issue_id", "notes"],
            "additionalProperties": false
        },
        "annotations": write_annotations(false, false)
    })
}

fn transition_issue_spec() -> Value {
    json!({
        "name": "transition_issue",
        "title": "Transition issue",
        "description": "Change an issue status (and optionally add a comment). Redmine enforces its workflow; use read_issue with include=[\"allowed_statuses\"] first. WARNING: writes to the configured Redmine instance; call it only after the user explicitly confirms the change.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_id": id_prop("Issue id."),
                "status_id": id_prop("Target status id."),
                "notes": { "type": "string", "minLength": 1, "description": "Optional comment to add with the transition." },
                "private_notes": { "type": "boolean", "description": "Make the comment private. Requires notes." }
            },
            "required": ["issue_id", "status_id"],
            "additionalProperties": false
        },
        "annotations": write_annotations(true, false)
    })
}

fn find_issues_spec() -> Value {
    json!({
        "name": "find_issues",
        "title": "Find issues",
        "description": "Search issues with Redmine filters and pagination. Text search: use subject_contains (Redmine's subject=~... operator). Returns {issues, total_count, offset, limit}.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project_id": id_prop("Filter by project id."),
                "tracker_id": id_prop("Filter by tracker id."),
                "status_id": {
                    "description": "Filter by status: an id, or open, closed or * (all). Defaults to Redmine's default (open).",
                    "oneOf": [
                        { "type": "integer", "minimum": 1 },
                        { "type": "string", "enum": ["open", "closed", "*"] }
                    ]
                },
                "assigned_to_id": {
                    "description": "Filter by assignee: a user id or \"me\".",
                    "oneOf": [
                        { "type": "integer", "minimum": 1 },
                        { "type": "string", "enum": ["me"] }
                    ]
                },
                "author_id": id_prop("Filter by author user id."),
                "subject": { "type": "string", "minLength": 1, "description": "Exact subject match." },
                "subject_contains": { "type": "string", "minLength": 1, "description": "Substring of the subject (Redmine ~ operator). Use this for text search." },
                "updated_since": date_prop("Only issues updated on or after this date (YYYY-MM-DD)."),
                "sort": { "type": "string", "description": "Redmine sort expression, for example \"updated_on:desc\" or \"priority:desc,id\"." },
                "limit": limit_prop(),
                "offset": offset_prop()
            },
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn search_spec() -> Value {
    json!({
        "name": "search",
        "title": "Search Redmine",
        "description": "Search all Redmine content visible to the API user with GET /search.json: issues, wiki pages, news, documents, changesets, messages and projects. Use it for full-text search; use find_issues for structured issue filters. Returns {results, total_count, offset, limit}; result URLs outside the configured Redmine origin are nulled.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 500,
                    "description": "Search text (Redmine q). Space-separated terms are accepted."
                },
                "scope": {
                    "type": "string",
                    "enum": ["all", "my_projects", "my_project", "subprojects", "bookmarks"],
                    "description": "Search scope. my_project is accepted as an alias for my_projects."
                },
                "issues": { "type": "boolean", "description": "Include issue results." },
                "news": { "type": "boolean", "description": "Include news results." },
                "documents": { "type": "boolean", "description": "Include document results." },
                "changesets": { "type": "boolean", "description": "Include changeset results." },
                "wiki_pages": { "type": "boolean", "description": "Include wiki page results." },
                "messages": { "type": "boolean", "description": "Include forum message results." },
                "projects": { "type": "boolean", "description": "Include project results." },
                "open_issues": { "type": "boolean", "description": "Restrict issue results to open issues." },
                "all_words": { "type": "boolean", "description": "Require every query term (Redmine default: true)." },
                "titles_only": { "type": "boolean", "description": "Search titles only." },
                "attachments": {
                    "type": "string",
                    "enum": ["0", "1", "only"],
                    "description": "Attachment search: 0 descriptions only, 1 descriptions and attachments, only attachment names."
                },
                "limit": limit_prop(),
                "offset": offset_prop()
            },
            "required": ["query"],
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn add_attachment_spec() -> Value {
    json!({
        "name": "add_attachment",
        "title": "Add attachment",
        "description": "Upload one file (base64, maximum 25 MiB decoded) and attach it to an issue. WARNING: writes to the configured Redmine instance; call it only after the user explicitly confirms the attachment.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_id": id_prop("Issue id."),
                "filename": { "type": "string", "minLength": 1, "maxLength": FILENAME_MAX_CHARS, "description": "File name stored by Redmine (no path separators)." },
                "data_base64": { "type": "string", "minLength": 1, "description": "Standard base64 content. A data: URI prefix is accepted." },
                "content_type": { "type": "string", "description": "MIME type such as text/plain or application/pdf." },
                "description": { "type": "string", "description": "Optional attachment description." }
            },
            "required": ["issue_id", "filename", "data_base64"],
            "additionalProperties": false
        },
        "annotations": write_annotations(false, false)
    })
}

fn get_project_versions_spec() -> Value {
    json!({
        "name": "get_project_versions",
        "title": "Get project versions",
        "description": "List a project's versions (milestones). Use the ids in fixed_version_id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project_id": id_prop("Project id.")
            },
            "required": ["project_id"],
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn get_project_categories_spec() -> Value {
    json!({
        "name": "get_project_categories",
        "title": "Get project categories",
        "description": "List a project's issue categories. Use the ids in category_id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project_id": id_prop("Project id.")
            },
            "required": ["project_id"],
            "additionalProperties": false
        },
        "annotations": read_annotations()
    })
}

fn get_metadata_spec() -> Value {
    json!({
        "name": "get_metadata",
        "title": "Get metadata",
        "description": "Fetch projects, trackers, statuses, users and priorities in one call. Users require admin rights in Redmine; when unavailable, users is empty and warnings explains why. Only the first 100 projects/users are returned.",
        "inputSchema": empty_schema(),
        "annotations": read_annotations()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_create(value: Value) -> CreateIssueArgs {
        serde_json::from_value(value).expect("valid create args")
    }

    fn parse_update(value: Value) -> UpdateIssueArgs {
        serde_json::from_value(value).expect("valid update args")
    }

    fn parse_find(value: Value) -> FindIssuesArgs {
        serde_json::from_value(value).expect("valid find args")
    }

    #[test]
    fn validates_dates() {
        assert!(is_valid_date("2024-02-29"));
        assert!(is_valid_date("2000-02-29"));
        assert!(!is_valid_date("1900-02-29"));
        assert!(!is_valid_date("2025-02-29"));
        assert!(!is_valid_date("2025-13-01"));
        assert!(!is_valid_date("2025-04-31"));
        assert!(!is_valid_date("2025-1-01"));
        assert!(!is_valid_date("2025-01-01T00:00"));
        assert!(!is_valid_date("0000-01-01"));
        assert!(!is_valid_date("á025-01-01"));
    }

    #[test]
    fn validates_includes_and_deduplicates() {
        let includes = vec![
            "journals".to_string(),
            "journals".to_string(),
            "attachments".to_string(),
        ];
        assert_eq!(
            validate_includes(&includes).expect("valid"),
            vec!["attachments".to_string(), "journals".to_string()]
        );
        assert!(validate_includes(&["comments".to_string()]).is_err());
    }

    #[test]
    fn validates_sort_expressions() {
        assert_eq!(
            validate_sort("updated_on:desc,id").expect("valid"),
            "updated_on:desc,id"
        );
        assert!(validate_sort("").is_err());
        assert!(validate_sort("updated_on:sideways").is_err());
        assert!(validate_sort("updated_on;drop table").is_err());
        assert!(validate_sort("a:b:c").is_err());
    }

    #[test]
    fn decodes_plain_and_data_uri_base64() {
        assert_eq!(
            decode_attachment("aGVsbG8=").expect("valid"),
            b"hello".to_vec()
        );
        assert_eq!(
            decode_attachment("data:text/plain;base64,aGVsbG8=").expect("valid"),
            b"hello".to_vec()
        );
        assert_eq!(
            decode_attachment("aGVs\nbG8=").expect("whitespace is ignored"),
            b"hello".to_vec()
        );
        assert!(decode_attachment("").is_err());
        assert!(decode_attachment("not base64!").is_err());
        assert!(decode_attachment("data:text/plain,hello").is_err());
    }

    #[test]
    fn rejects_unsafe_filenames() {
        assert_eq!(
            validate_filename(" report final.pdf ").expect("valid"),
            "report final.pdf"
        );
        assert!(validate_filename("").is_err());
        assert!(validate_filename("../secret").is_err());
        assert!(validate_filename("dir/file.txt").is_err());
        assert!(validate_filename("line\nbreak").is_err());
    }

    #[test]
    fn builds_create_payload_with_defaults() {
        let args = parse_create(json!({ "subject": "  Hello  " }));
        let issue = build_create_issue(args, Some(7)).expect("valid");
        assert_eq!(issue["project_id"], json!(7));
        assert_eq!(issue["subject"], json!("Hello"));
        assert!(issue.get("description").is_none());
    }

    #[test]
    fn create_requires_project_when_no_default() {
        let args = parse_create(json!({ "subject": "Hello" }));
        assert!(build_create_issue(args, None).is_err());
    }

    #[test]
    fn create_rejects_invalid_ranges() {
        let args = parse_create(json!({ "subject": "Hello", "project_id": 1, "done_ratio": 101 }));
        assert!(build_create_issue(args, None).is_err());

        let args =
            parse_create(json!({ "subject": "Hello", "project_id": 1, "estimated_hours": -1.0 }));
        assert!(build_create_issue(args, None).is_err());

        let args =
            parse_create(json!({ "subject": "Hello", "project_id": 1, "due_date": "2025-02-29" }));
        assert!(build_create_issue(args, None).is_err());
    }

    #[test]
    fn update_requires_at_least_one_change() {
        let args = parse_update(json!({ "issue_id": 1 }));
        assert!(build_update_issue(&args).is_err());
    }

    #[test]
    fn update_allows_empty_description_and_date_but_not_empty_subject() {
        let args = parse_update(json!({
            "issue_id": 1,
            "description": "",
            "due_date": ""
        }));
        let issue = build_update_issue(&args).expect("valid");
        assert_eq!(issue["description"], json!(""));
        assert_eq!(issue["due_date"], json!(""));

        let args = parse_update(json!({ "issue_id": 1, "subject": "   " }));
        assert!(build_update_issue(&args).is_err());
    }

    #[test]
    fn private_notes_require_notes_on_update() {
        let args = parse_update(json!({ "issue_id": 1, "private_notes": true }));
        let error = build_update_issue(&args).expect_err("must fail");
        assert!(error.to_string().contains("private_notes"));
    }

    #[test]
    fn builds_issue_query_with_text_search_and_filters() {
        let args = parse_find(json!({
            "project_id": 11,
            "status_id": "*",
            "assigned_to_id": "me",
            "subject_contains": "bug",
            "updated_since": "2025-01-01",
            "sort": "updated_on:desc",
            "limit": 10,
            "offset": 20
        }));
        let query = build_issue_query(&args).expect("valid");
        let get = |key: &str| {
            query
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("project_id"), Some("11"));
        assert_eq!(get("status_id"), Some("*"));
        assert_eq!(get("assigned_to_id"), Some("me"));
        assert_eq!(get("subject"), Some("~bug"));
        assert_eq!(get("updated_on"), Some(">=2025-01-01"));
        assert_eq!(get("sort"), Some("updated_on:desc"));
        assert_eq!(get("limit"), Some("10"));
        assert_eq!(get("offset"), Some("20"));
    }

    #[test]
    fn validates_page_size_bounds() {
        assert_eq!(validate_limit(None).expect("default"), 25);
        assert_eq!(validate_limit(Some(1)).expect("minimum"), 1);
        assert_eq!(validate_limit(Some(100)).expect("maximum"), 100);
        assert!(validate_limit(Some(0)).is_err());
        assert!(validate_limit(Some(101)).is_err());
    }

    #[test]
    fn builds_exact_subject_filter() {
        let args = parse_find(json!({ "subject": "Exact title" }));
        let query = build_issue_query(&args).expect("valid");
        let subject = query
            .iter()
            .find(|(key, _)| key == "subject")
            .map(|(_, value)| value.as_str());
        assert_eq!(subject, Some("Exact title"));
    }

    #[test]
    fn builds_search_query_with_filters() {
        let args: SearchArgs = serde_json::from_value(json!({
            "query": "  release notes  ",
            "scope": "my_project",
            "issues": true,
            "wiki_pages": true,
            "open_issues": true,
            "all_words": false,
            "titles_only": true,
            "attachments": "only",
            "limit": 10,
            "offset": 5
        }))
        .expect("valid search args");

        let query = build_search_query(&args).expect("valid");
        let get = |key: &str| {
            query
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("q"), Some("release notes"));
        assert_eq!(get("scope"), Some("my_projects"));
        assert_eq!(get("issues"), Some("1"));
        assert_eq!(get("wiki_pages"), Some("1"));
        assert_eq!(get("open_issues"), Some("1"));
        assert_eq!(get("all_words"), Some(""));
        assert_eq!(get("titles_only"), Some("1"));
        assert_eq!(get("attachments"), Some("only"));
        assert_eq!(get("limit"), Some("10"));
        assert_eq!(get("offset"), Some("5"));
    }

    #[test]
    fn rejects_both_subject_filters() {
        let args = parse_find(json!({ "subject": "a", "subject_contains": "b" }));
        assert!(build_issue_query(&args).is_err());
    }

    #[test]
    fn rejects_invalid_status_and_assignee_keywords() {
        let args = parse_find(json!({ "status_id": "pending" }));
        assert!(build_issue_query(&args).is_err());

        let args = parse_find(json!({ "assigned_to_id": "you" }));
        assert!(build_issue_query(&args).is_err());
    }

    #[test]
    fn rejects_unknown_query_parameter() {
        let error = serde_json::from_value::<FindIssuesArgs>(json!({ "query": "text" }))
            .expect_err("query must be rejected");
        assert!(error.to_string().contains("query"));
    }

    #[test]
    fn read_only_mode_hides_write_tools() {
        let all = specs(false);
        let reads = specs(true);
        assert_eq!(all.len(), 13);
        assert_eq!(reads.len(), 8);
        assert!(
            reads
                .iter()
                .all(|tool| !is_write(tool.get("name").and_then(Value::as_str).unwrap_or("")))
        );
    }

    #[test]
    fn every_tool_spec_is_an_object_with_a_schema() {
        for tool in specs(false) {
            assert!(tool.is_object());
            assert!(tool.get("name").and_then(Value::as_str).is_some());
            assert!(tool.get("description").and_then(Value::as_str).is_some());
            let schema = tool.get("inputSchema").expect("schema");
            assert_eq!(schema["type"], json!("object"));
            assert!(schema.get("properties").is_some());
        }
    }
}
