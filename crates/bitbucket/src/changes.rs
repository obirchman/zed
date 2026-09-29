//! What changed on a pull request between two moments.
//!
//! A `Snapshot` records the parts of a pull request a person reacts to: head
//! commit, state, title, reviewers' decisions, build statuses and comments.
//! `changes` compares two snapshots. The account the snapshots are taken for
//! (`me`) is left out of comment and review changes, since its own actions
//! are not news to it.

use crate::models::{Account, BuildStatus, Comment, ParticipantState, PullRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewMark {
    pub display_name: String,
    pub role: String,
    pub approved: bool,
    pub changes_requested: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusMark {
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommentMark {
    pub id: u64,
    pub author: String,
    pub author_uuid: Option<String>,
    pub parent: Option<u64>,
    /// The comment mentions the snapshot's account (`@{account_id}`).
    pub mentions_me: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub head: Option<String>,
    pub state: String,
    pub title: String,
    /// Keyed by account UUID, or display name when Bitbucket sends no UUID.
    pub participants: BTreeMap<String, ReviewMark>,
    /// Keyed by build status key.
    pub statuses: BTreeMap<String, StatusMark>,
    /// Comments that are not deleted, oldest first.
    pub comments: Vec<CommentMark>,
}

impl Snapshot {
    pub fn new(pr: &PullRequest, comments: &[Comment], statuses: &[BuildStatus], me: Option<&Account>) -> Self {
        let mention = me
            .and_then(|m| m.account_id.as_deref())
            .map(|id| format!("@{{{id}}}"));
        Self {
            head: pr.source.commit.as_ref().map(|c| c.hash.clone()),
            state: pr.state.clone(),
            title: pr.title.clone(),
            participants: pr
                .participants
                .iter()
                .map(|p| {
                    let key = p.user.uuid.clone().unwrap_or_else(|| p.user.display_name.clone());
                    let mark = ReviewMark {
                        display_name: p.user.display_name.clone(),
                        role: p.role.clone(),
                        approved: p.approved,
                        changes_requested: p.state == Some(ParticipantState::ChangesRequested),
                    };
                    (key, mark)
                })
                .collect(),
            statuses: statuses
                .iter()
                .map(|s| {
                    let name = if s.name.is_empty() { s.key.clone() } else { s.name.clone() };
                    (
                        s.key.clone(),
                        StatusMark {
                            name,
                            state: s.state.clone(),
                        },
                    )
                })
                .collect(),
            comments: comments
                .iter()
                .filter(|c| !c.deleted)
                .map(|c| CommentMark {
                    id: c.id,
                    author: c.user.display_name.clone(),
                    author_uuid: c.user.uuid.clone(),
                    parent: c.parent.as_ref().map(|p| p.id),
                    mentions_me: mention.as_deref().is_some_and(|m| c.content.raw.contains(m)),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    NewCommits { head: String },
    State { from: String, to: String },
    Title { to: String },
    Comments {
        count: usize,
        authors: Vec<String>,
        replies_to_me: usize,
        mentions_me: usize,
    },
    Approved { who: String },
    ApprovalWithdrawn { who: String },
    ChangesRequested { who: String },
    ReviewerAdded { who: String, is_me: bool },
    Build { name: String, state: String },
}

/// Everything in `after` that differs from `before`, leaving out the comments
/// and review decisions of `me_uuid`.
pub fn changes(before: &Snapshot, after: &Snapshot, me_uuid: Option<&str>) -> Vec<Change> {
    let is_me = |uuid: &str| me_uuid == Some(uuid);
    let mut out = Vec::new();

    if after.state != before.state {
        out.push(Change::State {
            from: before.state.clone(),
            to: after.state.clone(),
        });
    }
    if let Some(head) = &after.head
        && after.head != before.head
    {
        out.push(Change::NewCommits { head: head.clone() });
    }
    if after.title != before.title {
        out.push(Change::Title {
            to: after.title.clone(),
        });
    }

    let seen: std::collections::HashSet<u64> = before.comments.iter().map(|c| c.id).collect();
    let mine: std::collections::HashSet<u64> = after
        .comments
        .iter()
        .filter(|c| c.author_uuid.as_deref().is_some_and(is_me))
        .map(|c| c.id)
        .collect();
    let new: Vec<&CommentMark> = after
        .comments
        .iter()
        .filter(|c| !seen.contains(&c.id) && !mine.contains(&c.id))
        .collect();
    if !new.is_empty() {
        let mut authors: Vec<String> = Vec::new();
        for c in &new {
            if !authors.contains(&c.author) {
                authors.push(c.author.clone());
            }
        }
        out.push(Change::Comments {
            count: new.len(),
            authors,
            replies_to_me: new.iter().filter(|c| c.parent.is_some_and(|p| mine.contains(&p))).count(),
            mentions_me: new.iter().filter(|c| c.mentions_me).count(),
        });
    }

    for (key, now) in &after.participants {
        if is_me(key) {
            if now.role == "REVIEWER" && before.participants.get(key).is_none_or(|b| b.role != "REVIEWER") {
                out.push(Change::ReviewerAdded {
                    who: now.display_name.clone(),
                    is_me: true,
                });
            }
            continue;
        }
        let was = before.participants.get(key);
        let was_approved = was.is_some_and(|w| w.approved);
        let was_requested = was.is_some_and(|w| w.changes_requested);
        if now.approved && !was_approved {
            out.push(Change::Approved {
                who: now.display_name.clone(),
            });
        } else if !now.approved && was_approved {
            out.push(Change::ApprovalWithdrawn {
                who: now.display_name.clone(),
            });
        }
        if now.changes_requested && !was_requested {
            out.push(Change::ChangesRequested {
                who: now.display_name.clone(),
            });
        }
        if now.role == "REVIEWER" && was.is_none_or(|w| w.role != "REVIEWER") {
            out.push(Change::ReviewerAdded {
                who: now.display_name.clone(),
                is_me: false,
            });
        }
    }

    for (key, now) in &after.statuses {
        if before.statuses.get(key).is_none_or(|b| b.state != now.state) {
            out.push(Change::Build {
                name: now.name.clone(),
                state: now.state.clone(),
            });
        }
    }
    out
}

/// One line for a list row: `new commits · 2 new comments (1 reply to you) · build: failed`.
pub fn summary(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|c| match c {
            Change::NewCommits { .. } => "new commits".to_string(),
            Change::State { to, .. } => to.to_lowercase(),
            Change::Title { .. } => "title changed".to_string(),
            Change::Comments {
                count,
                replies_to_me,
                mentions_me,
                ..
            } => {
                let mut extra = Vec::new();
                if *replies_to_me > 0 {
                    extra.push(format!("{replies_to_me} repl{} to you", if *replies_to_me == 1 { "y" } else { "ies" }));
                }
                if *mentions_me > 0 {
                    extra.push(format!("{mentions_me} mention{}", if *mentions_me == 1 { "" } else { "s" }));
                }
                let base = format!("{count} new comment{}", if *count == 1 { "" } else { "s" });
                if extra.is_empty() { base } else { format!("{base} ({})", extra.join(", ")) }
            }
            Change::Approved { who } => format!("{who} approved"),
            Change::ApprovalWithdrawn { who } => format!("{who} withdrew approval"),
            Change::ChangesRequested { who } => format!("{who} requested changes"),
            Change::ReviewerAdded { is_me: true, .. } => "you were added as reviewer".to_string(),
            Change::ReviewerAdded { who, .. } => format!("{who} added as reviewer"),
            Change::Build { name, state } => format!("{name}: {}", state.to_lowercase()),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "{me}";

    fn comment(id: u64, author: &str, uuid: &str, parent: Option<u64>) -> CommentMark {
        CommentMark {
            id,
            author: author.into(),
            author_uuid: Some(uuid.into()),
            parent,
            mentions_me: false,
        }
    }

    fn reviewer(name: &str, approved: bool, changes_requested: bool) -> ReviewMark {
        ReviewMark {
            display_name: name.into(),
            role: "REVIEWER".into(),
            approved,
            changes_requested,
        }
    }

    fn base() -> Snapshot {
        Snapshot {
            head: Some("aaa".into()),
            state: "OPEN".into(),
            title: "T".into(),
            participants: BTreeMap::from([("{j}".to_string(), reviewer("Jonas", false, false))]),
            statuses: BTreeMap::from([(
                "b".to_string(),
                StatusMark {
                    name: "build".into(),
                    state: "INPROGRESS".into(),
                },
            )]),
            comments: vec![comment(1, "Me", ME, None)],
        }
    }

    #[test]
    fn identical_snapshots_have_no_changes() {
        assert!(changes(&base(), &base(), Some(ME)).is_empty());
    }

    #[test]
    fn reports_commits_comments_reviews_and_builds() {
        let mut after = base();
        after.head = Some("bbb".into());
        after.comments.push(comment(2, "Jonas", "{j}", Some(1)));
        after.comments.push(CommentMark {
            mentions_me: true,
            ..comment(3, "Maja", "{m}", None)
        });
        after.participants.insert("{j}".into(), reviewer("Jonas", true, false));
        after.statuses.get_mut("b").unwrap().state = "FAILED".into();
        let got = changes(&base(), &after, Some(ME));
        assert_eq!(
            got,
            vec![
                Change::NewCommits { head: "bbb".into() },
                Change::Comments {
                    count: 2,
                    authors: vec!["Jonas".into(), "Maja".into()],
                    replies_to_me: 1,
                    mentions_me: 1,
                },
                Change::Approved { who: "Jonas".into() },
                Change::Build {
                    name: "build".into(),
                    state: "FAILED".into(),
                },
            ]
        );
        assert_eq!(
            summary(&got),
            "new commits · 2 new comments (1 reply to you, 1 mention) · Jonas approved · build: failed"
        );
    }

    #[test]
    fn my_own_comments_and_approvals_are_not_news() {
        let mut after = base();
        after.comments.push(comment(2, "Me", ME, None));
        after.participants.insert(ME.into(), ReviewMark { approved: true, ..reviewer("Me", true, false) });
        let got = changes(&base(), &after, Some(ME));
        assert_eq!(got, vec![Change::ReviewerAdded { who: "Me".into(), is_me: true }]);
    }

    #[test]
    fn being_added_as_reviewer_and_changes_requested() {
        let mut before = base();
        before.participants.clear();
        let mut after = base();
        after.participants.insert("{j}".into(), reviewer("Jonas", false, true));
        let got = changes(&before, &after, Some(ME));
        assert_eq!(
            got,
            vec![
                Change::ChangesRequested { who: "Jonas".into() },
                Change::ReviewerAdded {
                    who: "Jonas".into(),
                    is_me: false
                },
            ]
        );
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let json = serde_json::to_string(&base()).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), base());
    }
}
