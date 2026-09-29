//! Typed views of the Bitbucket objects a user interface shows.
//!
//! The client returns Bitbucket's own JSON (`serde_json::Value`); `decode`
//! turns one object or a list into these structs. Every field Bitbucket may
//! omit is optional or defaulted, so an object with fewer fields still decodes.

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `value` decoded as `T`, for example `decode::<Vec<Comment>>(Value::Array(comments))`.
pub fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("unexpected Bitbucket response")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Href {
    #[serde(default)]
    pub href: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Links {
    pub html: Option<Href>,
    pub avatar: Option<Href>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub display_name: String,
    pub uuid: Option<String>,
    pub account_id: Option<String>,
    pub nickname: Option<String>,
    #[serde(default)]
    pub links: Links,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Named {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommitRef {
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepositoryRef {
    #[serde(default)]
    pub full_name: String,
}

/// The source or destination side of a pull request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub branch: Named,
    pub commit: Option<CommitRef>,
    pub repository: Option<RepositoryRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rendered {
    #[serde(default)]
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantState {
    Approved,
    ChangesRequested,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    #[serde(default)]
    pub user: Account,
    /// `PARTICIPANT` or `REVIEWER`.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub approved: bool,
    pub state: Option<ParticipantState>,
    pub participated_on: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// `OPEN`, `MERGED`, `DECLINED` or `SUPERSEDED`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub author: Account,
    #[serde(default)]
    pub source: Endpoint,
    #[serde(default)]
    pub destination: Endpoint,
    pub merge_commit: Option<CommitRef>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub reviewers: Vec<Account>,
    #[serde(default)]
    pub comment_count: u64,
    #[serde(default)]
    pub task_count: u64,
    pub created_on: Option<String>,
    pub updated_on: Option<String>,
    #[serde(default)]
    pub links: Links,
}

/// Where an inline comment sits: `to` is a line in the new file, `from` a line
/// in the old file (a removed line).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inline {
    #[serde(default)]
    pub path: String,
    pub from: Option<u64>,
    pub to: Option<u64>,
    #[serde(default)]
    pub outdated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParentRef {
    pub id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub user: Option<Account>,
    pub created_on: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    #[serde(default)]
    pub content: Rendered,
    #[serde(default)]
    pub user: Account,
    pub created_on: Option<String>,
    pub updated_on: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    pub inline: Option<Inline>,
    pub parent: Option<ParentRef>,
    pub resolution: Option<Resolution>,
    #[serde(default)]
    pub links: Links,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildStatus {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub name: String,
    /// `SUCCESSFUL`, `FAILED`, `INPROGRESS` or `STOPPED`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub description: String,
    pub updated_on: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineState {
    /// `PENDING`, `IN_PROGRESS` or `COMPLETED`.
    #[serde(default)]
    pub name: String,
    /// Set once completed: `SUCCESSFUL`, `FAILED`, `ERROR`, `STOPPED`.
    pub result: Option<Named>,
    /// Set while in progress: `RUNNING`, `PAUSED`.
    pub stage: Option<Named>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineSelector {
    #[serde(default, rename = "type")]
    pub kind: String,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineTarget {
    pub ref_name: Option<String>,
    pub commit: Option<CommitRef>,
    pub selector: Option<PipelineSelector>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub build_number: u64,
    #[serde(default)]
    pub state: PipelineState,
    #[serde(default)]
    pub target: PipelineTarget,
    pub creator: Option<Account>,
    pub trigger: Option<Named>,
    pub created_on: Option<String>,
    pub completed_on: Option<String>,
    pub duration_in_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: PipelineState,
    pub started_on: Option<String>,
    pub completed_on: Option<String>,
    pub duration_in_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PathRef {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiffstatEntry {
    /// `added`, `removed`, `modified` or `renamed`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub lines_added: u64,
    #[serde(default)]
    pub lines_removed: u64,
    pub old: Option<PathRef>,
    pub new: Option<PathRef>,
}

impl PipelineState {
    /// One word for the state: the result once completed, else the stage or state name.
    pub fn label(&self) -> &str {
        self.result
            .as_ref()
            .or(self.stage.as_ref())
            .map(|n| n.name.as_str())
            .unwrap_or(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_a_pull_request_with_participants() {
        let pr: PullRequest = decode(json!({
            "id": 214, "title": "Reset signal", "state": "OPEN",
            "author": { "display_name": "M A", "uuid": "{a}" },
            "source": { "branch": { "name": "bugfix/x" }, "commit": { "hash": "abc" } },
            "destination": { "branch": { "name": "main" }, "commit": { "hash": "def" } },
            "participants": [
                { "user": { "display_name": "O B" }, "role": "REVIEWER", "approved": false, "state": "changes_requested" },
                { "user": { "display_name": "C D" }, "role": "PARTICIPANT", "approved": true, "state": "approved" },
                { "user": { "display_name": "E F" }, "role": "REVIEWER", "approved": false, "state": null }
            ],
            "links": { "html": { "href": "https://bitbucket.org/ws/r/pull-requests/214" } },
            "unknown_field": 1
        }))
        .unwrap();
        assert_eq!(pr.source.commit.unwrap().hash, "abc");
        assert_eq!(pr.participants[0].state, Some(ParticipantState::ChangesRequested));
        assert_eq!(pr.participants[1].state, Some(ParticipantState::Approved));
        assert_eq!(pr.participants[2].state, None);
        assert_eq!(pr.links.html.unwrap().href, "https://bitbucket.org/ws/r/pull-requests/214");
    }

    #[test]
    fn decodes_inline_and_reply_comments() {
        let comments: Vec<Comment> = decode(json!([
            { "id": 1, "content": { "raw": "a" }, "inline": { "path": "src/a.c", "to": 12, "from": null } },
            { "id": 2, "content": { "raw": "b" }, "parent": { "id": 1 }, "deleted": true },
            { "id": 3, "content": { "raw": "c" }, "resolution": { "created_on": "2026-09-01" } }
        ]))
        .unwrap();
        assert_eq!(comments[0].inline.as_ref().unwrap().to, Some(12));
        assert_eq!(comments[1].parent.as_ref().unwrap().id, 1);
        assert!(comments[1].deleted);
        assert!(comments[2].resolution.is_some());
    }

    #[test]
    fn pipeline_state_label() {
        let p: Pipeline = decode(json!({
            "uuid": "{p}", "build_number": 26,
            "state": { "name": "COMPLETED", "result": { "name": "FAILED" } },
            "target": { "ref_name": "feature/x" }
        }))
        .unwrap();
        assert_eq!(p.state.label(), "FAILED");
        let running: PipelineState = decode(json!({ "name": "IN_PROGRESS", "stage": { "name": "RUNNING" } })).unwrap();
        assert_eq!(running.label(), "RUNNING");
    }
}
