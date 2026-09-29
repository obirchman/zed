//! The Bitbucket Cloud REST client: one credential for reads and writes.
//!
//! Reads return Bitbucket's own JSON (`serde_json::Value`); `models::decode`
//! turns them into typed structs. Every write is first described as a
//! `WriteRequest` by a function in `plan`, then sent with `Client::execute`.
//! Reads retry on 429, gateway errors and connection failures; writes only on
//! 429, since a write that timed out may already have taken effect.

use crate::repo::RepoRef;
use anyhow::{Result, anyhow, bail};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use smol::lock::Semaphore;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "https://api.bitbucket.org/2.0";

/// Caps requests in flight, so a fan-out over many repositories stays under
/// Bitbucket's rate limit.
const MAX_CONCURRENT_REQUESTS: usize = 8;
const MAX_RETRIES: u32 = 5;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// How requests authenticate. An Atlassian API token goes with the account
/// email as HTTP Basic; a repository or workspace access token goes alone as Bearer.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Basic { email: String, token: String },
    Bearer { token: String },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Basic { email, .. } => write!(f, "Basic({email}, <token>)"),
            Auth::Bearer { .. } => write!(f, "Bearer(<token>)"),
        }
    }
}

impl Auth {
    /// Basic with `email`, or Bearer when `email` is empty.
    pub fn new(email: &str, token: &str) -> Self {
        if email.trim().is_empty() {
            Auth::Bearer { token: token.to_string() }
        } else {
            Auth::Basic {
                email: email.trim().to_string(),
                token: token.to_string(),
            }
        }
    }

    fn header(&self) -> String {
        use base64::Engine as _;
        match self {
            Auth::Basic { email, token } => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            ),
            Auth::Bearer { token } => format!("Bearer {token}"),
        }
    }
}

/// One HTTP request as the client builds it. Header names are lower-case.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

/// A response with its whole body read.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// The `Retry-After` header in seconds, when present.
    pub retry_after: Option<Duration>,
    pub body: String,
}

/// A request that got no response.
#[derive(Debug, Clone)]
pub struct HttpFailure {
    pub message: String,
    /// The connection failed or timed out, so a GET may be retried.
    pub connect_or_timeout: bool,
}

/// The HTTP stack the client sends through, and the timer between retries.
pub trait Http: Send + Sync {
    fn send(&self, req: HttpRequest) -> BoxFuture<'static, std::result::Result<HttpResponse, HttpFailure>>;
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryPolicy {
    Idempotent,
    RateLimitOnly,
}

pub struct Client {
    http: Arc<dyn Http>,
    base: String,
    auth: Auth,
    sem: Arc<Semaphore>,
}

pub fn repo_path(repo: &RepoRef) -> String {
    format!("/repositories/{}/{}", enc(&repo.workspace), enc(&repo.slug))
}

impl Client {
    pub fn new(http: Arc<dyn Http>, auth: Auth, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            auth,
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        }
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Wait on the client's timer, for callers that poll.
    pub async fn sleep(&self, duration: Duration) {
        self.http.sleep(duration).await
    }

    fn resolve(&self, path: &str) -> Result<String> {
        let base = &self.base;
        if path.starts_with("http://") || path.starts_with("https://") {
            // Pagination `next` links are absolute; credentials go only to the API's own host.
            if path == base || path.starts_with(&format!("{base}/")) || path.starts_with(&format!("{base}?")) {
                return Ok(path.to_string());
            }
            bail!("refusing to send credentials to {path}: not under {base}");
        }
        Ok(if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        })
    }

    async fn send(&self, req: HttpRequest, policy: RetryPolicy) -> Result<String> {
        let mut delay = FIRST_RETRY_DELAY;
        let mut attempt = 0;
        loop {
            let can_retry = attempt < MAX_RETRIES;
            match self.http.send(req.clone()).await {
                Ok(resp) if (200..300).contains(&resp.status) => return Ok(resp.body),
                Ok(resp) if can_retry && is_retryable(resp.status, policy) => {
                    let wait = resp.retry_after.unwrap_or(delay).min(MAX_RETRY_DELAY);
                    self.http.sleep(wait).await;
                }
                Ok(resp) => {
                    bail!(
                        "Bitbucket returned {} for {} {}: {}",
                        resp.status,
                        req.method,
                        req.url,
                        error_message(&resp.body)
                    );
                }
                Err(e) if can_retry && policy == RetryPolicy::Idempotent && e.connect_or_timeout => {
                    self.http.sleep(delay).await;
                }
                Err(e) => bail!("request to {} failed: {}", req.url, e.message),
            }
            attempt += 1;
            delay = (delay * 2).min(MAX_RETRY_DELAY);
        }
    }

    async fn get_body(&self, path: &str, accept: &str) -> Result<String> {
        let url = self.resolve(path)?;
        let _permit = self.sem.acquire().await;
        let req = HttpRequest {
            method: "GET",
            url,
            headers: vec![
                ("authorization", self.auth.header()),
                ("accept", accept.to_string()),
            ],
            body: None,
        };
        self.send(req, RetryPolicy::Idempotent).await
    }

    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let body = self.get_body(path, "application/json").await?;
        Ok(serde_json::from_str(&body)?)
    }

    /// A non-JSON resource: a diff or a step log.
    pub async fn get_text(&self, path: &str) -> Result<String> {
        self.get_body(path, "*/*").await
    }

    /// Every `values` entry of a paginated listing, following `next` links,
    /// stopping once `limit` entries are collected.
    pub async fn get_all(&self, path: &str, limit: Option<usize>) -> Result<Vec<Value>> {
        let mut url = path.to_string();
        let mut out = Vec::new();
        loop {
            let page = self.get_json(&url).await?;
            let values = page
                .get("values")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("{url} is not a paginated listing"))?;
            out.extend(values.iter().cloned());
            if let Some(limit) = limit
                && out.len() >= limit
            {
                out.truncate(limit);
                break;
            }
            match page.get("next").and_then(Value::as_str) {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn current_user(&self) -> Result<Value> {
        self.get_json("/user").await
    }

    pub async fn pull_request(&self, repo: &RepoRef, id: u64) -> Result<Value> {
        self.get_json(&format!("{}/pullrequests/{id}", repo_path(repo))).await
    }

    /// Open pull requests of `repo`, newest update first, with participants.
    pub async fn open_pull_requests(&self, repo: &RepoRef, limit: Option<usize>) -> Result<Vec<Value>> {
        self.get_all(
            &format!(
                "{}/pullrequests?pagelen=50&sort=-updated_on&state=OPEN&fields=%2Bvalues.participants,%2Bvalues.reviewers",
                repo_path(repo)
            ),
            limit,
        )
        .await
    }

    pub async fn pull_request_diffstat(&self, repo: &RepoRef, id: u64) -> Result<Vec<Value>> {
        self.get_all(&format!("{}/pullrequests/{id}/diffstat?pagelen=500", repo_path(repo)), None)
            .await
    }

    /// All comments, oldest first, including deleted ones (`"deleted": true`).
    pub async fn pull_request_comments(&self, repo: &RepoRef, id: u64) -> Result<Vec<Value>> {
        self.get_all(&format!("{}/pullrequests/{id}/comments?pagelen=100", repo_path(repo)), None)
            .await
    }

    /// Build statuses (CI results) reported against the pull request.
    pub async fn pull_request_statuses(&self, repo: &RepoRef, id: u64) -> Result<Vec<Value>> {
        self.get_all(&format!("{}/pullrequests/{id}/statuses?pagelen=100", repo_path(repo)), None)
            .await
    }

    /// Open pull requests authored by `user_uuid` in any repository of `workspace`.
    pub async fn authored_pull_requests(&self, workspace: &str, user_uuid: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!(
                "/workspaces/{}/pullrequests/{}?state=OPEN&pagelen=50&fields=%2Bvalues.participants,%2Bvalues.reviewers",
                enc(workspace),
                enc(user_uuid)
            ),
            None,
        )
        .await
    }

    /// Open pull requests of `repo` that have `user_uuid` as a reviewer.
    /// Bitbucket has no workspace-wide listing for reviewers.
    pub async fn reviewer_pull_requests(&self, repo: &RepoRef, user_uuid: &str) -> Result<Vec<Value>> {
        // The state goes inside the query: a `state=` parameter is ignored once `q` is present.
        let q = format!("reviewers.uuid={} AND state=\"OPEN\"", bbql_str(user_uuid));
        self.get_all(
            &format!(
                "{}/pullrequests?pagelen=50&q={}&fields=%2Bvalues.participants,%2Bvalues.reviewers",
                repo_path(repo),
                enc(&q)
            ),
            None,
        )
        .await
    }

    /// Repositories of `workspace`, most recently pushed first, with only
    /// their slug and `updated_on`.
    pub async fn repositories_by_update(&self, workspace: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!(
                "/repositories/{}?pagelen=100&sort=-updated_on&fields=next,values.slug,values.updated_on",
                enc(workspace)
            ),
            None,
        )
        .await
    }

    /// Pipelines whose target is `branch`, newest first.
    pub async fn branch_pipelines(&self, repo: &RepoRef, branch: &str, limit: Option<usize>) -> Result<Vec<Value>> {
        let pagelen = limit.unwrap_or(30).clamp(1, 100);
        self.get_all(
            &format!(
                "{}/pipelines/?sort=-created_on&pagelen={pagelen}&target.ref_name={}",
                repo_path(repo),
                enc(branch)
            ),
            limit,
        )
        .await
    }

    pub async fn pipeline_steps(&self, repo: &RepoRef, pipeline_uuid: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("{}/pipelines/{}/steps/?pagelen=100", repo_path(repo), enc(pipeline_uuid)),
            None,
        )
        .await
    }

    pub async fn step_log(&self, repo: &RepoRef, pipeline_uuid: &str, step_uuid: &str) -> Result<String> {
        self.get_text(&format!(
            "{}/pipelines/{}/steps/{}/log",
            repo_path(repo),
            enc(pipeline_uuid),
            enc(step_uuid)
        ))
        .await
    }

    /// Send `req`. Returns the response JSON, or `Value::Null` for an empty body (204).
    pub async fn execute(&self, req: &WriteRequest) -> Result<Value> {
        let url = self.resolve(&req.path)?;
        let mut headers = vec![
            ("authorization", self.auth.header()),
            ("accept", "application/json".to_string()),
        ];
        if req.body.is_some() {
            headers.push(("content-type", "application/json".to_string()));
        }
        let http_req = HttpRequest {
            method: req.method.as_str(),
            url,
            headers,
            body: req.body.as_ref().map(Value::to_string),
        };
        let text = self.send(http_req, RetryPolicy::RateLimitOnly).await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }
}

fn is_retryable(status: u16, policy: RetryPolicy) -> bool {
    status == 429 || (policy == RetryPolicy::Idempotent && matches!(status, 500 | 502 | 503 | 504))
}

/// Bitbucket's `{"error": {"message": ..., "detail": ...}}`, or the raw body.
fn error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some(message) = v["error"]["message"].as_str()
    {
        let detail = match &v["error"]["detail"] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        return if detail.is_empty() {
            message.to_string()
        } else {
            format!("{message} ({detail})")
        };
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "(empty body)".to_string()
    } else {
        trimmed.chars().take(500).collect()
    }
}

/// Percent-encode one path segment or query value.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A BBQL string literal: `"value"` with `\` and `"` escaped.
pub fn bbql_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteRequest {
    pub method: Method,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct InlineAnchor {
    pub path: String,
    pub line: u64,
    /// Anchor to the line number in the old version of the file (a removed line).
    pub old_side: bool,
}

#[derive(Debug, Clone)]
pub struct NewComment {
    pub body: String,
    pub inline: Option<InlineAnchor>,
    pub parent: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct NewPipeline {
    pub branch: String,
    /// Run on this commit instead of the branch head.
    pub commit: Option<String>,
    /// Name of a pipeline under `pipelines: custom:` in bitbucket-pipelines.yml.
    pub custom: Option<String>,
}

/// Builders for every write, so the request sent is exactly the one planned.
pub mod plan {
    use super::*;

    pub fn comment(repo: &RepoRef, pr_id: u64, comment: &NewComment) -> WriteRequest {
        let mut body = json!({ "content": { "raw": comment.body } });
        if let Some(inline) = &comment.inline {
            let side = if inline.old_side { "from" } else { "to" };
            let mut anchor = json!({ "path": inline.path });
            anchor[side] = json!(inline.line);
            body["inline"] = anchor;
        }
        if let Some(parent) = comment.parent {
            body["parent"] = json!({ "id": parent });
        }
        WriteRequest {
            method: Method::Post,
            path: format!("{}/pullrequests/{pr_id}/comments", repo_path(repo)),
            body: Some(body),
        }
    }

    /// Mark a comment thread resolved (`resolved: true`) or reopen it.
    pub fn resolve_comment(repo: &RepoRef, pr_id: u64, comment_id: u64, resolved: bool) -> WriteRequest {
        WriteRequest {
            method: if resolved { Method::Post } else { Method::Delete },
            path: format!("{}/pullrequests/{pr_id}/comments/{comment_id}/resolve", repo_path(repo)),
            body: None,
        }
    }

    /// Approve the pull request as the credential's account, or withdraw the approval.
    pub fn approve(repo: &RepoRef, pr_id: u64, approved: bool) -> WriteRequest {
        WriteRequest {
            method: if approved { Method::Post } else { Method::Delete },
            path: format!("{}/pullrequests/{pr_id}/approve", repo_path(repo)),
            body: None,
        }
    }

    /// Request changes on the pull request, or withdraw the request.
    pub fn request_changes(repo: &RepoRef, pr_id: u64, requested: bool) -> WriteRequest {
        WriteRequest {
            method: if requested { Method::Post } else { Method::Delete },
            path: format!("{}/pullrequests/{pr_id}/request-changes", repo_path(repo)),
            body: None,
        }
    }

    pub fn trigger_pipeline(repo: &RepoRef, p: &NewPipeline) -> WriteRequest {
        let mut target = json!({
            "type": "pipeline_ref_target",
            "ref_type": "branch",
            "ref_name": p.branch,
        });
        if let Some(hash) = &p.commit {
            target["commit"] = json!({ "type": "commit", "hash": hash });
        }
        if let Some(name) = &p.custom {
            target["selector"] = json!({ "type": "custom", "pattern": name });
        }
        WriteRequest {
            method: Method::Post,
            path: format!("{}/pipelines/", repo_path(repo)),
            body: Some(json!({ "target": target })),
        }
    }

    pub fn stop_pipeline(repo: &RepoRef, pipeline_uuid: &str) -> WriteRequest {
        WriteRequest {
            method: Method::Post,
            path: format!("{}/pipelines/{}/stopPipeline", repo_path(repo), enc(pipeline_uuid)),
            body: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef::new("ws", "repo")
    }

    #[test]
    fn auth_headers() {
        assert_eq!(Auth::new("a@b.se", "t").header(), "Basic YUBiLnNlOnQ=");
        assert_eq!(Auth::new("", "x").header(), "Bearer x");
    }

    #[test]
    fn error_message_prefers_bitbucket_error_field() {
        let body = r#"{"type":"error","error":{"message":"Bad request","detail":"branch missing"}}"#;
        assert_eq!(error_message(body), "Bad request (branch missing)");
        assert_eq!(error_message("<html>502</html>"), "<html>502</html>");
        assert_eq!(error_message(""), "(empty body)");
    }

    #[test]
    fn enc_and_bbql_escape() {
        assert_eq!(enc("{a-b}"), "%7Ba-b%7D");
        assert_eq!(enc("feature/x y"), "feature%2Fx%20y");
        assert_eq!(bbql_str(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn inline_comment_sides() {
        let new_side = plan::comment(
            &repo(),
            7,
            &NewComment {
                body: "b".into(),
                inline: Some(InlineAnchor {
                    path: "src/a.c".into(),
                    line: 12,
                    old_side: false,
                }),
                parent: None,
            },
        );
        assert_eq!(new_side.body.unwrap()["inline"], json!({ "path": "src/a.c", "to": 12 }));
        let reply = plan::comment(
            &repo(),
            7,
            &NewComment {
                body: "b".into(),
                inline: None,
                parent: Some(99),
            },
        );
        assert_eq!(reply.body.unwrap()["parent"], json!({ "id": 99 }));
    }

    #[test]
    fn review_state_writes() {
        let r = repo();
        let approve = plan::approve(&r, 7, true);
        assert_eq!((approve.method, approve.path.as_str()), (Method::Post, "/repositories/ws/repo/pullrequests/7/approve"));
        assert_eq!(plan::approve(&r, 7, false).method, Method::Delete);
        assert_eq!(plan::request_changes(&r, 7, true).path, "/repositories/ws/repo/pullrequests/7/request-changes");
        let resolve = plan::resolve_comment(&r, 7, 42, true);
        assert_eq!(resolve.path, "/repositories/ws/repo/pullrequests/7/comments/42/resolve");
        assert_eq!(plan::resolve_comment(&r, 7, 42, false).method, Method::Delete);
        assert_eq!(plan::stop_pipeline(&r, "{abc}").path, "/repositories/ws/repo/pipelines/%7Babc%7D/stopPipeline");
    }
}
