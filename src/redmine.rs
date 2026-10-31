//! Thin, typed client for the Redmine REST API.
//!
//! Design notes:
//! * Successful responses are returned as raw JSON. Redmine's schema is large
//!   and version-dependent, so modelling every issue field would only add
//!   breakage; the MCP layer passes the data through untouched (minus secrets).
//! * Only idempotent GET requests are retried.
//! * Every response is capped in size and every `api_key` field is removed
//!   before the data can reach the model (Redmine's `/users/current.json`
//!   response contains the caller's API key).

use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{Client, Method, Response, StatusCode, Url};
use serde_json::{Value, json};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::log_debug;

/// Hard limit for a single Redmine response body (16 MiB).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Total attempts for a retryable GET (first try + two retries).
const MAX_ATTEMPTS: usize = 3;
/// Uploads get at least two minutes even when the normal timeout is shorter.
const UPLOAD_TIMEOUT_FLOOR: Duration = Duration::from_secs(120);

/// Redmine REST client shared by every tool.
pub struct RedmineClient {
    /// Client for idempotent reads; may follow safe same-origin redirects.
    http: Client,
    /// Client for writes; never follows redirects (a redirect could change the
    /// method or send the body to a different endpoint).
    write_http: Client,
    base: Url,
    timeout: Duration,
}

impl RedmineClient {
    pub fn new(config: &Config) -> Result<Self> {
        let api_key = reqwest::header::HeaderValue::from_str(&config.api_key).map_err(|_| {
            Error::config(
                "REDMINE_API_KEY contains characters that are not valid in an HTTP header",
            )
        })?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-redmine-api-key"),
            api_key,
        );
        headers.insert(
            ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        // Never let a redirect carry the API key to another origin. Some HTTP
        // clients forward custom headers such as X-Redmine-API-Key across
        // cross-origin redirects; reqwest keeps default headers too, so the
        // policy has to reject them explicitly.
        let expected_host = config.redmine_url.host_str().map(str::to_owned);
        let expected_scheme = config.redmine_url.scheme().to_string();
        let expected_port = config.redmine_url.port_or_known_default();
        let redirect_policy = Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error(std::io::Error::other("too many redirects"));
            }

            let target = attempt.url();
            let same_host = target.host_str() == expected_host.as_deref();
            let same_origin = target.scheme() == expected_scheme
                && target.port_or_known_default() == expected_port;
            let upgrade_to_https =
                expected_scheme == "http" && target.scheme() == "https" && same_host;

            if same_host && (same_origin || upgrade_to_https) {
                attempt.follow()
            } else {
                attempt.error(std::io::Error::other(
                    "refusing a cross-origin or HTTPS-to-HTTP redirect because the API key would be sent to another origin",
                ))
            }
        });

        let http = build_http(config, &headers, redirect_policy)?;
        let write_http = build_http(config, &headers, Policy::none())?;

        Ok(Self {
            http,
            write_http,
            base: config.redmine_url.clone(),
            timeout: config.timeout,
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    fn url(&self, path: &str) -> Result<Url> {
        let path = path.trim_start_matches('/');
        self.base.join(path).map_err(|err| {
            Error::config(format!(
                "could not build a Redmine URL from path {path:?}: {err}"
            ))
        })
    }

    /// GET with bounded retries for transient failures.
    async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        let url = self.url(path)?;
        let mut attempt = 1;

        loop {
            let started = Instant::now();
            match self.http.get(url.clone()).query(query).send().await {
                Ok(response) => {
                    let status = response.status();
                    log_debug!(
                        "GET {} -> {} ({} ms, attempt {})",
                        path,
                        status.as_u16(),
                        started.elapsed().as_millis(),
                        attempt
                    );

                    if is_retryable_status(status) && attempt < MAX_ATTEMPTS {
                        let delay = retry_delay(&response);
                        drop(response);
                        log_debug!(
                            "retrying GET {} in {} ms after HTTP {}",
                            path,
                            delay.as_millis(),
                            status.as_u16()
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }

                    return self.parse_response(response).await;
                }
                Err(err) => {
                    let error = Error::Http(err);
                    if error.is_retryable() && attempt < MAX_ATTEMPTS {
                        let delay = backoff(attempt);
                        log_debug!(
                            "retrying GET {} in {} ms after transport error",
                            path,
                            delay.as_millis()
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    /// POST/PUT of a JSON body. Never retried: a duplicate write is worse than
    /// an error the user can retry explicitly.
    async fn send_json(&self, method: Method, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path)?;
        let started = Instant::now();
        let response = self
            .write_http
            .request(method.clone(), url)
            .json(body)
            .send()
            .await?;
        log_debug!(
            "{} {} -> {} ({} ms)",
            method,
            path,
            response.status().as_u16(),
            started.elapsed().as_millis()
        );
        self.parse_response(response).await
    }

    async fn parse_response(&self, response: Response) -> Result<Value> {
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let body = read_body(response).await?;

        if !status.is_success() {
            return Err(api_error(status, &body));
        }

        if body.is_empty() {
            return Ok(Value::Null);
        }

        let looks_like_json =
            content_type.contains("json") || matches!(body.first(), Some(b'{') | Some(b'['));
        if !looks_like_json {
            let received = if content_type.is_empty() {
                "an unknown content type"
            } else {
                content_type.as_str()
            };
            return Err(Error::api(
                status.as_u16(),
                format!(
                    "expected JSON but received {received}; check that REDMINE_URL points to the Redmine API and not to a login page or proxy"
                ),
            ));
        }

        let mut value: Value = serde_json::from_slice(&body)
            .map_err(|err| Error::api(status.as_u16(), format!("invalid JSON response: {err}")))?;
        redact_api_key(&mut value);
        Ok(value)
    }

    // --- Issues ---------------------------------------------------------

    pub async fn get_issue(&self, issue_id: u64, include: &[String]) -> Result<Value> {
        let mut query = Vec::new();
        if !include.is_empty() {
            query.push(("include".to_string(), include.join(",")));
        }
        self.get(&format!("issues/{issue_id}.json"), &query).await
    }

    pub async fn find_issues(&self, query: &[(String, String)]) -> Result<Value> {
        self.get("issues.json", query).await
    }

    pub async fn search(&self, query: &[(String, String)]) -> Result<Value> {
        self.get("search.json", query).await
    }

    pub async fn create_issue(&self, issue: &Value) -> Result<Value> {
        self.send_json(Method::POST, "issues.json", &json!({ "issue": issue }))
            .await
    }

    pub async fn update_issue(&self, issue_id: u64, issue: &Value) -> Result<Value> {
        self.send_json(
            Method::PUT,
            &format!("issues/{issue_id}.json"),
            &json!({ "issue": issue }),
        )
        .await
    }

    pub async fn add_note(&self, issue_id: u64, notes: &str, private_notes: bool) -> Result<Value> {
        let mut issue = json!({ "notes": notes });
        if private_notes {
            issue["private_notes"] = json!(true);
        }
        self.update_issue(issue_id, &issue).await
    }

    pub async fn transition_issue(
        &self,
        issue_id: u64,
        status_id: u64,
        notes: Option<&str>,
        private_notes: bool,
    ) -> Result<Value> {
        let mut issue = json!({ "status_id": status_id });
        if let Some(notes) = notes {
            issue["notes"] = json!(notes);
        }
        if private_notes {
            issue["private_notes"] = json!(true);
        }
        self.update_issue(issue_id, &issue).await
    }

    // --- Attachments ----------------------------------------------------

    /// Uploads raw bytes and returns the temporary upload token.
    pub async fn upload(&self, data: &[u8], filename: &str) -> Result<String> {
        let url = self.url("uploads.json")?;
        let started = Instant::now();
        let response = self
            .write_http
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .timeout(self.timeout.max(UPLOAD_TIMEOUT_FLOOR))
            .body(data.to_vec())
            .send()
            .await?;
        log_debug!(
            "POST uploads.json ({} bytes) -> {} ({} ms)",
            data.len(),
            response.status().as_u16(),
            started.elapsed().as_millis()
        );

        let value = self.parse_response(response).await?;
        value
            .pointer("/upload/token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                Error::api(
                    200,
                    format!("upload succeeded but the response did not include a token (filename: {filename})"),
                )
            })
    }

    /// Attaches a previously uploaded token to an issue.
    pub async fn attach(
        &self,
        issue_id: u64,
        token: &str,
        filename: &str,
        content_type: Option<&str>,
        description: Option<&str>,
    ) -> Result<Value> {
        let mut upload = json!({ "token": token, "filename": filename });
        if let Some(content_type) = content_type {
            upload["content_type"] = json!(content_type);
        }
        if let Some(description) = description {
            upload["description"] = json!(description);
        }
        self.update_issue(issue_id, &json!({ "uploads": [upload] }))
            .await
    }

    // --- Metadata -------------------------------------------------------

    pub async fn list_projects(&self, limit: u32, offset: u32) -> Result<Value> {
        self.get("projects.json", &page_query(limit, offset)).await
    }

    pub async fn get_project_versions(&self, project_id: u64) -> Result<Value> {
        self.get(&format!("projects/{project_id}/versions.json"), &[])
            .await
    }

    pub async fn get_project_categories(&self, project_id: u64) -> Result<Value> {
        self.get(&format!("projects/{project_id}/issue_categories.json"), &[])
            .await
    }

    pub async fn get_trackers(&self) -> Result<Value> {
        self.get("trackers.json", &[]).await
    }

    pub async fn get_issue_statuses(&self) -> Result<Value> {
        self.get("issue_statuses.json", &[]).await
    }

    pub async fn get_priorities(&self) -> Result<Value> {
        self.get("enumerations/issue_priorities.json", &[]).await
    }

    pub async fn get_users(&self, limit: u32, offset: u32) -> Result<Value> {
        self.get("users.json", &page_query(limit, offset)).await
    }

    pub async fn get_current_user(&self) -> Result<Value> {
        self.get("users/current.json", &[]).await
    }
}

fn build_http(
    config: &Config,
    headers: &reqwest::header::HeaderMap,
    redirect_policy: Policy,
) -> Result<Client> {
    Client::builder()
        .user_agent(concat!("redmine-mcp-rs/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers.clone())
        .timeout(config.timeout)
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(redirect_policy)
        .build()
        .map_err(Error::Http)
}

fn page_query(limit: u32, offset: u32) -> Vec<(String, String)> {
    vec![
        ("limit".to_string(), limit.to_string()),
        ("offset".to_string(), offset.to_string()),
    ]
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

fn backoff(attempt: usize) -> Duration {
    // 200 ms, 600 ms, 1.8 s, ...
    let factor = 3u64.saturating_pow(attempt.saturating_sub(1) as u32);
    Duration::from_millis(200 * factor.min(9))
}

fn retry_delay(response: &Response) -> Duration {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(5)))
        .unwrap_or_else(|| backoff(1))
}

async fn read_body(mut response: Response) -> Result<Vec<u8>> {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(Error::api(
                status,
                format!(
                    "response exceeds the {} MiB limit",
                    MAX_RESPONSE_BYTES / (1024 * 1024)
                ),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn api_error(status: StatusCode, body: &[u8]) -> Error {
    let code = status.as_u16();
    let detail = extract_error(body);
    let hint = match code {
        401 => Some("check REDMINE_API_KEY"),
        403 => Some("the API key does not have permission for this resource"),
        404 => Some("the resource does not exist or is not visible to this API key"),
        300..=399 => Some(
            "the server returned a redirect; configure REDMINE_URL with the canonical URL (writes never follow redirects)",
        ),
        429 => Some("Redmine rate limit reached, retry later"),
        500..=599 => Some("Redmine returned a server error"),
        _ => None,
    };
    let message = match (detail.as_str(), hint) {
        ("empty response", Some(hint)) => hint.to_string(),
        (_, Some(hint)) => format!("{detail} ({hint})"),
        (_, None) => detail,
    };
    Error::api(code, message)
}

fn extract_error(body: &[u8]) -> String {
    if body.is_empty() {
        return "empty response".to_string();
    }

    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Some(errors) = value.get("errors").and_then(Value::as_array) {
            let messages: Vec<String> = errors
                .iter()
                .map(|error| {
                    error
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string())
                })
                .filter(|message| !message.trim().is_empty())
                .collect();
            if !messages.is_empty() {
                return truncate(&messages.join("; "), 500);
            }
        }
        for key in ["error", "message", "detail"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    return truncate(text, 500);
                }
            }
        }
        return truncate(&value.to_string(), 300);
    }

    let text = String::from_utf8_lossy(body);
    if text.trim_start().starts_with('<') {
        return "server returned an HTML error page (a proxy or login page may be intercepting the API)"
            .to_string();
    }

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, 300)
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Removes every `api_key` field from a JSON value, recursively.
///
/// Redmine returns the caller's API key in `/users/current.json` (and for
/// admins in some user listings). Tool output is sent to the model, so the key
/// must never leave the process.
pub fn redact_api_key(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("api_key");
            for child in map.values_mut() {
                redact_api_key(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_api_key(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_api_key_recursively() {
        let mut value = json!({
            "user": {
                "login": "alice",
                "api_key": "super-secret",
                "nested": [{ "api_key": "another", "ok": true }]
            },
            "api_key": "top-secret",
            "issues": [{ "subject": "keep me" }]
        });

        redact_api_key(&mut value);

        let serialized = value.to_string();
        assert!(!serialized.contains("super-secret"));
        assert!(!serialized.contains("another"));
        assert!(!serialized.contains("top-secret"));
        assert!(serialized.contains("alice"));
        assert!(serialized.contains("keep me"));
    }

    #[test]
    fn extracts_redmine_validation_errors() {
        let body = br#"{"errors":["Subject cannot be blank","Project is invalid"]}"#;
        assert_eq!(
            extract_error(body),
            "Subject cannot be blank; Project is invalid"
        );
    }

    #[test]
    fn extracts_redmine_error_messages() {
        assert_eq!(
            extract_error(br#"{"error":"Internal Server Error"}"#),
            "Internal Server Error"
        );
        assert_eq!(extract_error(br#"{"message":"Not found"}"#), "Not found");
    }

    #[test]
    fn summarizes_html_error_pages() {
        let body = b"<!DOCTYPE html><html><body>login</body></html>";
        let message = extract_error(body);
        assert!(message.contains("HTML error page"));
        assert!(!message.contains("<html>"));
    }

    #[test]
    fn truncates_multibyte_text_safely() {
        let text = "áéíóúáéíóú";
        let truncated = truncate(text, 5);
        assert_eq!(truncated, "áéíóú...");
        assert_eq!(truncate(text, 100), text);
    }

    #[test]
    fn maps_status_codes_to_hints() {
        let error = api_error(StatusCode::UNAUTHORIZED, b"{}");
        assert!(error.to_string().contains("401"));
        assert!(error.to_string().contains("REDMINE_API_KEY"));
    }

    #[test]
    fn page_query_contains_limit_and_offset() {
        let query = page_query(50, 10);
        assert_eq!(query[0], ("limit".to_string(), "50".to_string()));
        assert_eq!(query[1], ("offset".to_string(), "10".to_string()));
    }

    #[test]
    fn retryable_statuses_are_the_transient_ones() {
        for status in [429, 502, 503, 504] {
            assert!(is_retryable_status(
                StatusCode::from_u16(status).expect("status")
            ));
        }
        for status in [200, 400, 401, 403, 404, 422, 500] {
            assert!(!is_retryable_status(
                StatusCode::from_u16(status).expect("status")
            ));
        }
    }
}
