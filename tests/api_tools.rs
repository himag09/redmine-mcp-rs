mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use common::{MockRedmine, MockResponse, test_app, test_app_with_env};
use redmine_mcp_rs::tools::{self, App};

async fn call(app: &App, name: &str, args: Value) -> Value {
    tools::call(app, name, args)
        .await
        .unwrap_or_else(|error| panic!("tool {name} failed: {error}"))
}

#[tokio::test]
async fn read_issue_sends_auth_and_include_and_redacts_api_key() {
    let mock = MockRedmine::new(|_| {
        MockResponse::json(
            200,
            r#"{"issue":{"id":42,"subject":"Hello","api_key":"leak-me","journals":[]}}"#,
        )
    });
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "read_issue",
        json!({ "issue_id": 42, "include": ["journals", "allowed_statuses"] }),
    )
    .await;

    assert_eq!(result["id"], json!(42));
    assert_eq!(result["subject"], json!("Hello"));
    assert!(result.get("api_key").is_none());

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/issues/42.json");
    assert_eq!(
        requests[0].query_value("include").as_deref(),
        Some("allowed_statuses,journals")
    );
    assert_eq!(
        requests[0].header("x-redmine-api-key"),
        Some("test-api-key")
    );
    assert_eq!(requests[0].header("accept"), Some("application/json"));
}

#[tokio::test]
async fn find_issues_maps_filters_to_redmine_query_parameters() {
    let mock = MockRedmine::new(|_| {
        MockResponse::json(
            200,
            r#"{"issues":[],"total_count":0,"offset":20,"limit":10}"#,
        )
    });
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "find_issues",
        json!({
            "project_id": 11,
            "tracker_id": 7,
            "author_id": 35,
            "status_id": "*",
            "assigned_to_id": "me",
            "subject_contains": "campaña",
            "updated_since": "2025-01-01",
            "sort": "updated_on:desc",
            "limit": 10,
            "offset": 20
        }),
    )
    .await;

    assert_eq!(result["total_count"], json!(0));

    let request = mock.last_request();
    assert_eq!(request.path, "/issues.json");
    assert_eq!(request.query_value("project_id").as_deref(), Some("11"));
    assert_eq!(request.query_value("tracker_id").as_deref(), Some("7"));
    assert_eq!(request.query_value("author_id").as_deref(), Some("35"));
    assert_eq!(request.query_value("status_id").as_deref(), Some("*"));
    assert_eq!(request.query_value("assigned_to_id").as_deref(), Some("me"));
    assert_eq!(request.query_value("subject").as_deref(), Some("~campaña"));
    assert_eq!(
        request.query_value("updated_on").as_deref(),
        Some(">=2025-01-01")
    );
    assert_eq!(
        request.query_value("sort").as_deref(),
        Some("updated_on:desc")
    );
    assert_eq!(request.query_value("limit").as_deref(), Some("10"));
    assert_eq!(request.query_value("offset").as_deref(), Some("20"));
    assert!(request.query_value("query").is_none());
}

#[tokio::test]
async fn find_issues_rejects_the_removed_query_parameter_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(500, r#"{"error":"boom"}"#));
    let app = test_app(&mock, false);

    let error = tools::call(&app, "find_issues", json!({ "query": "text" }))
        .await
        .expect_err("query must be rejected");

    assert!(error.to_string().contains("query"));
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn create_issue_uses_default_project_and_omits_absent_fields() {
    let mock =
        MockRedmine::new(|_| MockResponse::json(201, r#"{"issue":{"id":100,"subject":"New"}}"#));
    let app = test_app_with_env(&mock, &[("REDMINE_DEFAULT_PROJECT_ID", "7")]);

    let result = call(
        &app,
        "create_issue",
        json!({
            "subject": "  New  ",
            "description": "Body",
            "done_ratio": 25,
            "custom_fields": [{ "id": 3, "value": "abc" }],
            "watcher_user_ids": [5, 5, 6]
        }),
    )
    .await;

    assert_eq!(result["id"], json!(100));

    let request = mock.last_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/issues.json");
    let body = request.json_body();
    assert_eq!(body["issue"]["project_id"], json!(7));
    assert_eq!(body["issue"]["subject"], json!("New"));
    assert_eq!(body["issue"]["description"], json!("Body"));
    assert_eq!(body["issue"]["done_ratio"], json!(25));
    assert_eq!(body["issue"]["watcher_user_ids"], json!([5, 6]));
    assert!(body["issue"].get("tracker_id").is_none());
    assert!(body["issue"].get("notes").is_none());
}

#[tokio::test]
async fn create_issue_without_project_or_default_fails_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(201, "{}"));
    let app = test_app(&mock, false);

    let error = tools::call(&app, "create_issue", json!({ "subject": "x" }))
        .await
        .expect_err("project is required");

    assert!(error.to_string().contains("project_id"));
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn update_issue_sends_only_provided_fields_and_refetches() {
    let mock = MockRedmine::new(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("PUT", "/issues/5.json") => MockResponse::empty(204),
            ("GET", "/issues/5.json") => MockResponse::json(
                200,
                r#"{"issue":{"id":5,"subject":"New","status":{"id":2,"name":"In Progress"}}}"#,
            ),
            _ => MockResponse::json(404, r#"{"errors":["unexpected request"]}"#),
        },
    );
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "update_issue",
        json!({
            "issue_id": 5,
            "subject": "New",
            "notes": "Done",
            "private_notes": true
        }),
    )
    .await;

    assert_eq!(result["subject"], json!("New"));
    assert_eq!(result["status"]["name"], json!("In Progress"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "PUT");
    let body = requests[0].json_body();
    assert_eq!(body["issue"]["subject"], json!("New"));
    assert_eq!(body["issue"]["notes"], json!("Done"));
    assert_eq!(body["issue"]["private_notes"], json!(true));
    assert!(body["issue"].get("description").is_none());
    assert_eq!(requests[1].method, "GET");
}

#[tokio::test]
async fn update_without_changes_does_not_call_redmine() {
    let mock = MockRedmine::new(|_| MockResponse::empty(204));
    let app = test_app(&mock, false);

    let error = tools::call(&app, "update_issue", json!({ "issue_id": 5 }))
        .await
        .expect_err("nothing to update");

    assert!(error.to_string().contains("nothing to update"));
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn update_reports_success_when_refetch_fails_but_never_retries_the_write() {
    let mock = MockRedmine::new(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("PUT", "/issues/5.json") => MockResponse::empty(204),
            ("GET", "/issues/5.json") => MockResponse::json(500, r#"{"error":"boom"}"#),
            _ => MockResponse::json(404, "{}"),
        },
    );
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "update_issue",
        json!({ "issue_id": 5, "subject": "New" }),
    )
    .await;

    assert_eq!(result["succeeded"], json!(true));
    assert!(
        result["warning"]
            .as_str()
            .expect("warning")
            .contains("could not be re-read")
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| request.method == "PUT")
            .count(),
        1
    );
}

#[tokio::test]
async fn add_note_and_transition_hit_the_issue_endpoint() {
    let mock = MockRedmine::new(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("PUT", "/issues/5.json") => MockResponse::empty(204),
            ("GET", "/issues/5.json") => {
                MockResponse::json(200, r#"{"issue":{"id":5,"subject":"Issue"}}"#)
            }
            _ => MockResponse::json(404, "{}"),
        },
    );
    let app = test_app(&mock, false);

    call(
        &app,
        "add_issue_note",
        json!({ "issue_id": 5, "notes": "Hello", "private_notes": true }),
    )
    .await;
    call(
        &app,
        "transition_issue",
        json!({ "issue_id": 5, "status_id": 3, "notes": "Closing" }),
    )
    .await;

    let requests = mock.requests();
    let puts: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0].json_body()["issue"]["notes"], json!("Hello"));
    assert_eq!(puts[0].json_body()["issue"]["private_notes"], json!(true));
    assert_eq!(puts[1].json_body()["issue"]["status_id"], json!(3));
    assert_eq!(puts[1].json_body()["issue"]["notes"], json!("Closing"));
}

#[tokio::test]
async fn metadata_degrades_when_users_are_forbidden() {
    let mock = MockRedmine::new(|request| match request.path.as_str() {
        "/projects.json" => MockResponse::json(
            200,
            r#"{"projects":[{"id":1,"name":"P"}],"total_count":1,"offset":0,"limit":100}"#,
        ),
        "/trackers.json" => MockResponse::json(200, r#"{"trackers":[{"id":1,"name":"Bug"}]}"#),
        "/issue_statuses.json" => {
            MockResponse::json(200, r#"{"issue_statuses":[{"id":1,"name":"New"}]}"#)
        }
        "/users.json" => MockResponse::html(403, "<html>Forbidden</html>"),
        "/enumerations/issue_priorities.json" => {
            MockResponse::json(200, r#"{"issue_priorities":[{"id":2,"name":"Normal"}]}"#)
        }
        _ => MockResponse::json(404, "{}"),
    });
    let app = test_app(&mock, false);

    let result = call(&app, "get_metadata", json!({})).await;

    assert_eq!(result["projects"][0]["name"], json!("P"));
    assert_eq!(result["users"], json!([]));
    assert_eq!(result["priorities"][0]["name"], json!("Normal"));
    let warnings = result["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(
            |warning| warning.as_str().unwrap().contains("users unavailable")
                && warning.as_str().unwrap().contains("403")
        ),
        "warnings were: {warnings:?}"
    );
    assert_eq!(mock.request_count(), 5);
}

#[tokio::test]
async fn metadata_warns_when_projects_are_truncated() {
    let mock = MockRedmine::new(|request| match request.path.as_str() {
        "/projects.json" => MockResponse::json(
            200,
            r#"{"projects":[],"total_count":150,"offset":0,"limit":100}"#,
        ),
        "/trackers.json" => MockResponse::json(200, r#"{"trackers":[]}"#),
        "/issue_statuses.json" => MockResponse::json(200, r#"{"issue_statuses":[]}"#),
        "/users.json" => MockResponse::json(200, r#"{"users":[],"total_count":0}"#),
        "/enumerations/issue_priorities.json" => {
            MockResponse::json(200, r#"{"issue_priorities":[]}"#)
        }
        _ => MockResponse::json(404, "{}"),
    });
    let app = test_app(&mock, false);

    let result = call(&app, "get_metadata", json!({})).await;

    let warnings = result["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("150")),
        "warnings were: {warnings:?}"
    );
}

#[tokio::test]
async fn retries_transient_get_failures() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_router = Arc::clone(&calls);
    let mock = MockRedmine::new(move |_| {
        let attempt = calls_for_router.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            MockResponse::json(503, r#"{"error":"busy"}"#).with_header("Retry-After", "0")
        } else {
            MockResponse::json(
                200,
                r#"{"projects":[],"total_count":0,"offset":0,"limit":25}"#,
            )
        }
    });
    let app = test_app(&mock, false);

    let result = call(&app, "list_projects", json!({})).await;

    assert_eq!(result["total_count"], json!(0));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(mock.request_count(), 2);
}

#[tokio::test]
async fn does_not_retry_writes() {
    let mock = MockRedmine::new(|_| MockResponse::json(503, r#"{"error":"busy"}"#));
    let app = test_app(&mock, false);

    let error = tools::call(
        &app,
        "update_issue",
        json!({ "issue_id": 5, "subject": "New" }),
    )
    .await
    .expect_err("write must fail");

    assert!(error.to_string().contains("503"));
    assert_eq!(mock.request_count(), 1);
}

#[tokio::test]
async fn surfaces_redmine_validation_errors() {
    let mock = MockRedmine::new(|_| {
        MockResponse::json(
            422,
            r#"{"errors":["Subject cannot be blank","Project is invalid"]}"#,
        )
    });
    let app = test_app(&mock, false);

    let error = tools::call(
        &app,
        "create_issue",
        json!({ "project_id": 1, "subject": "x" }),
    )
    .await
    .expect_err("must fail");

    let text = error.to_string();
    assert!(text.contains("422"));
    assert!(text.contains("Subject cannot be blank"));
    assert!(text.contains("Project is invalid"));
}

#[tokio::test]
async fn uploads_and_attaches_a_file() {
    let mock = MockRedmine::new(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/uploads.json") => {
                MockResponse::json(201, r#"{"upload":{"id":1,"token":"tok123"}}"#)
            }
            ("PUT", "/issues/9.json") => MockResponse::empty(204),
            ("GET", "/issues/9.json") => MockResponse::json(
                200,
                r#"{"issue":{"id":9,"attachments":[{"id":1,"filename":"hello.txt"}]}}"#,
            ),
            _ => MockResponse::json(404, "{}"),
        },
    );
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "add_attachment",
        json!({
            "issue_id": 9,
            "filename": "hello.txt",
            "data_base64": "data:text/plain;base64,aGVsbG8=",
            "content_type": "text/plain",
            "description": "Greeting"
        }),
    )
    .await;

    assert_eq!(result["attachments"][0]["filename"], json!("hello.txt"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/uploads.json");
    assert_eq!(
        requests[0].header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(requests[0].body, b"hello".to_vec());

    let attach = requests[1].json_body();
    assert_eq!(attach["issue"]["uploads"][0]["token"], json!("tok123"));
    assert_eq!(
        attach["issue"]["uploads"][0]["filename"],
        json!("hello.txt")
    );
    assert_eq!(
        attach["issue"]["uploads"][0]["content_type"],
        json!("text/plain")
    );
    assert_eq!(
        attach["issue"]["uploads"][0]["description"],
        json!("Greeting")
    );
    assert_eq!(requests[2].method, "GET");
}

#[tokio::test]
async fn refuses_redirects_that_would_leak_the_api_key() {
    let target = MockRedmine::new(|_| MockResponse::json(200, r#"{"projects":[]}"#));
    let target_url = target.base_url.clone();
    let source = MockRedmine::new(move |_| {
        MockResponse::empty(302).with_header("Location", target_url.clone())
    });
    let app = test_app(&source, false);

    let error = tools::call(&app, "list_projects", json!({}))
        .await
        .expect_err("cross-origin redirect must be refused");

    let text = error.to_string();
    assert!(
        text.contains("redirect") || text.contains("cross-origin"),
        "unexpected error: {text}"
    );
    assert_eq!(source.request_count(), 1);
    assert_eq!(
        target.request_count(),
        0,
        "the API key must never reach the redirect target"
    );
}

#[tokio::test]
async fn list_projects_sends_pagination() {
    let mock = MockRedmine::new(|_| {
        MockResponse::json(
            200,
            r#"{"projects":[],"total_count":0,"offset":50,"limit":25}"#,
        )
    });
    let app = test_app(&mock, false);

    call(&app, "list_projects", json!({ "limit": 25, "offset": 50 })).await;

    let request = mock.last_request();
    assert_eq!(request.query_value("limit").as_deref(), Some("25"));
    assert_eq!(request.query_value("offset").as_deref(), Some("50"));
}

#[tokio::test]
async fn read_only_rejects_write_tools_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(201, "{}"));
    let app = test_app(&mock, true);

    let error = tools::call(&app, "create_issue", json!({ "subject": "x" }))
        .await
        .expect_err("read-only server");

    assert!(error.to_string().contains("REDMINE_READ_ONLY"));
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn invalid_include_fails_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(200, "{}"));
    let app = test_app(&mock, false);

    let error = tools::call(
        &app,
        "read_issue",
        json!({ "issue_id": 1, "include": ["comments"] }),
    )
    .await
    .expect_err("invalid include");

    assert!(error.to_string().contains("include"));
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn reads_project_versions_categories_and_reference_data() {
    let mock = MockRedmine::new(|request| match request.path.as_str() {
        "/projects/11/versions.json" => {
            MockResponse::json(200, r#"{"versions":[{"id":1,"name":"v1"}]}"#)
        }
        "/projects/11/issue_categories.json" => {
            MockResponse::json(200, r#"{"issue_categories":[{"id":2,"name":"Backend"}]}"#)
        }
        "/trackers.json" => MockResponse::json(200, r#"{"trackers":[{"id":1,"name":"Bug"}]}"#),
        "/issue_statuses.json" => {
            MockResponse::json(200, r#"{"issue_statuses":[{"id":1,"name":"New"}]}"#)
        }
        _ => MockResponse::json(404, "{}"),
    });
    let app = test_app(&mock, false);

    let versions = call(&app, "get_project_versions", json!({ "project_id": 11 })).await;
    assert_eq!(versions["versions"][0]["name"], json!("v1"));

    let categories = call(&app, "get_project_categories", json!({ "project_id": 11 })).await;
    assert_eq!(categories["issue_categories"][0]["name"], json!("Backend"));

    let reference = call(&app, "list_trackers_statuses", json!({})).await;
    assert_eq!(reference["trackers"][0]["name"], json!("Bug"));
    assert_eq!(reference["statuses"][0]["name"], json!("New"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].path, "/projects/11/versions.json");
    assert_eq!(requests[1].path, "/projects/11/issue_categories.json");
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/trackers.json")
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/issue_statuses.json")
    );
}

#[tokio::test]
async fn invalid_notes_transitions_and_attachments_fail_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(200, "{}"));
    let app = test_app(&mock, false);

    let error = tools::call(
        &app,
        "add_issue_note",
        json!({ "issue_id": 1, "notes": "   " }),
    )
    .await
    .expect_err("empty notes must be rejected");
    assert!(error.to_string().contains("notes"));

    let error = tools::call(
        &app,
        "transition_issue",
        json!({ "issue_id": 1, "status_id": 2, "private_notes": true }),
    )
    .await
    .expect_err("private notes without notes must be rejected");
    assert!(error.to_string().contains("private_notes"));

    let error = tools::call(
        &app,
        "add_attachment",
        json!({
            "issue_id": 1,
            "filename": "x.txt",
            "data_base64": "not base64!"
        }),
    )
    .await
    .expect_err("invalid base64 must be rejected");
    assert!(error.to_string().contains("base64"));

    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn follows_same_origin_redirects() {
    let mock = MockRedmine::new(|request| match request.path.as_str() {
        "/projects.json" => {
            MockResponse::empty(302).with_header("Location", "/redirected-projects.json")
        }
        "/redirected-projects.json" => MockResponse::json(
            200,
            r#"{"projects":[],"total_count":0,"offset":0,"limit":25}"#,
        ),
        _ => MockResponse::json(404, "{}"),
    });
    let app = test_app(&mock, false);

    let result = call(&app, "list_projects", json!({})).await;

    assert_eq!(result["total_count"], json!(0));
    assert_eq!(mock.request_count(), 2);
    assert_eq!(mock.last_request().path, "/redirected-projects.json");
}

#[tokio::test]
async fn metadata_reports_unavailable_priorities() {
    let mock = MockRedmine::new(|request| match request.path.as_str() {
        "/projects.json" => MockResponse::json(
            200,
            r#"{"projects":[],"total_count":0,"offset":0,"limit":100}"#,
        ),
        "/trackers.json" => MockResponse::json(200, r#"{"trackers":[]}"#),
        "/issue_statuses.json" => MockResponse::json(200, r#"{"issue_statuses":[]}"#),
        "/users.json" => MockResponse::json(200, r#"{"users":[],"total_count":0}"#),
        "/enumerations/issue_priorities.json" => {
            MockResponse::json(403, r#"{"errors":["Forbidden"]}"#)
        }
        _ => MockResponse::json(404, "{}"),
    });
    let app = test_app(&mock, false);

    let result = call(&app, "get_metadata", json!({})).await;

    assert_eq!(result["priorities"], json!([]));
    let warnings = result["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|warning| {
            let warning = warning.as_str().unwrap();
            warning.contains("priorities unavailable") && warning.contains("403")
        }),
        "warnings were: {warnings:?}"
    );
}

#[tokio::test]
async fn writes_never_follow_redirects() {
    let target = MockRedmine::new(|_| MockResponse::json(200, r#"{"issue":{"id":5}}"#));
    let target_url = target.base_url.clone();
    let source = MockRedmine::new(move |_| {
        MockResponse::empty(307).with_header("Location", target_url.clone())
    });
    let app = test_app(&source, false);

    let error = tools::call(
        &app,
        "update_issue",
        json!({ "issue_id": 5, "subject": "New" }),
    )
    .await
    .expect_err("write redirect must be refused");

    let text = error.to_string();
    assert!(
        text.contains("307") || text.contains("redirect"),
        "unexpected error: {text}"
    );
    assert_eq!(source.request_count(), 1);
    assert_eq!(
        target.request_count(),
        0,
        "a write must never be replayed at a redirect target"
    );
}

#[tokio::test]
async fn rejects_oversized_redmine_responses() {
    let big = "x".repeat(16 * 1024 * 1024 + 1);
    let body = format!(r#"{{"data":"{big}"}}"#);
    let mock = MockRedmine::new(move |_| MockResponse::json(200, body.clone()));
    let app = test_app(&mock, false);

    let error = tools::call(&app, "list_projects", json!({}))
        .await
        .expect_err("oversized response must be rejected");

    let text = error.to_string();
    assert!(
        text.contains("16 MiB") || text.contains("exceeds"),
        "unexpected error: {text}"
    );
}

#[tokio::test]
async fn search_maps_filters_and_sanitizes_result_urls() {
    let mock = MockRedmine::new(|request| {
        let host = request.header("host").expect("host header").to_string();
        let body = format!(
            r#"{{"results":[
                {{"id":1,"title":"Safe","type":"issue","url":"http://{host}/issues/1","datetime":"2026-01-01T00:00:00Z"}},
                {{"id":2,"title":"External","type":"issue","url":"https://evil.example/steal","datetime":null}}
            ],"total_count":2,"offset":0,"limit":25}}"#
        );
        MockResponse::json(200, body)
    });
    let app = test_app(&mock, false);

    let result = call(
        &app,
        "search",
        json!({
            "query": "camp",
            "scope": "my_project",
            "issues": true,
            "wiki_pages": true,
            "open_issues": true,
            "all_words": false,
            "titles_only": true,
            "attachments": "only",
            "limit": 10,
            "offset": 5
        }),
    )
    .await;

    assert_eq!(result["total_count"], json!(2));
    assert_eq!(
        result["results"][0]["url"],
        json!(format!("{}issues/1", mock.base_url))
    );
    assert_eq!(result["results"][1]["url"], Value::Null);

    let request = mock.last_request();
    assert_eq!(request.path, "/search.json");
    assert_eq!(request.query_value("q").as_deref(), Some("camp"));
    assert_eq!(request.query_value("scope").as_deref(), Some("my_projects"));
    assert_eq!(request.query_value("issues").as_deref(), Some("1"));
    assert_eq!(request.query_value("wiki_pages").as_deref(), Some("1"));
    assert_eq!(request.query_value("open_issues").as_deref(), Some("1"));
    assert_eq!(request.query_value("all_words").as_deref(), Some(""));
    assert_eq!(request.query_value("titles_only").as_deref(), Some("1"));
    assert_eq!(request.query_value("attachments").as_deref(), Some("only"));
    assert_eq!(request.query_value("limit").as_deref(), Some("10"));
    assert_eq!(request.query_value("offset").as_deref(), Some("5"));
}

#[tokio::test]
async fn search_rejects_invalid_arguments_before_http() {
    let mock = MockRedmine::new(|_| MockResponse::json(200, r#"{"results":[]}"#));
    let app = test_app(&mock, false);

    for args in [
        json!({ "query": "   " }),
        json!({ "query": "x", "scope": "everything" }),
        json!({ "query": "x", "attachments": "2" }),
    ] {
        assert!(tools::call(&app, "search", args).await.is_err());
    }

    assert_eq!(mock.request_count(), 0);
}
