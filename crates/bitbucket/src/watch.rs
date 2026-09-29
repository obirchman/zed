//! The open pull requests the user authored or reviews, and what changed on
//! each since the user last marked it handled.
//!
//! A pull request that changes after being handled produces one
//! `WatchEvent::BecameChanged`, however many more changes follow, until it is
//! marked handled again.

use crate::persistence::BitbucketDb;
use crate::settings::BitbucketSettings;
use crate::store::{BitbucketStore, PrKey};
use anyhow::{Context as _, Result};
use crate::changes::{self, Change, Snapshot};
use crate::models::{self, Account, BuildStatus, Comment, PullRequest};
use crate::api::Client;
use crate::repo::RepoRef;
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task, TaskExt as _};
use serde_json::Value;
use settings::Settings as _;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SEEDED_KEY: &str = "bitbucket_pr_watch_seeded";

#[derive(Clone, Debug)]
pub struct WatchedPr {
    pub key: PrKey,
    pub pull_request: PullRequest,
    pub author: bool,
    pub reviewer: bool,
    /// Unix time of the last handled mark.
    pub handled_at: Option<i64>,
    /// What differs from the snapshot taken when it was last marked handled.
    pub changes: Vec<Change>,
    current: Snapshot,
}

impl WatchedPr {
    pub fn is_changed(&self) -> bool {
        !self.changes.is_empty()
    }

    pub fn summary(&self) -> String {
        changes::summary(&self.changes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Updated,
    /// The pull request changed after being handled. Sent once per handled mark.
    BecameChanged(PrKey),
}

struct Handled {
    at: i64,
    snapshot: Snapshot,
    notified: bool,
}

struct GlobalPrWatch(Entity<PrWatch>);

impl Global for GlobalPrWatch {}

pub struct PrWatch {
    store: Entity<BitbucketStore>,
    prs: Vec<WatchedPr>,
    handled: HashMap<PrKey, Handled>,
    /// Each repository's `updated_on` at the last check, which moves on push.
    repo_pushes: HashMap<RepoRef, String>,
    last_full_scan: Option<Instant>,
    seeded: bool,
    checking: bool,
    last_checked: Option<SystemTime>,
    error: Option<String>,
    _poll: Task<()>,
}

impl EventEmitter<WatchEvent> for PrWatch {}

/// What one check found, computed off the main thread.
struct CheckResult {
    me: Account,
    prs: Vec<(PrKey, PullRequest, bool, bool, Snapshot)>,
    repo_pushes: HashMap<RepoRef, String>,
    full_scan: bool,
}

impl PrWatch {
    pub fn init_global(cx: &mut App) {
        let watch = cx.new(Self::new);
        cx.set_global(GlobalPrWatch(watch));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalPrWatch>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalPrWatch>().map(|g| g.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let handled = BitbucketDb::global(cx)
            .all_handled()
            .map_err(|e| log::error!("bitbucket: reading handled marks: {e:#}"))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(workspace, slug, id, at, snapshot, notified)| {
                let snapshot = serde_json::from_str(&snapshot).ok()?;
                let key = PrKey {
                    repo: RepoRef::new(workspace, slug),
                    id: id as u64,
                };
                Some((key, Handled { at, snapshot, notified }))
            })
            .collect();
        let seeded = db::kvp::KeyValueStore::global(cx)
            .read_kvp(SEEDED_KEY)
            .ok()
            .flatten()
            .is_some();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(check) = this.update(cx, |this, cx| this.check(cx)) else {
                    break;
                };
                check.await;
                let interval = cx.update(|cx| BitbucketSettings::get_global(cx).poll_interval);
                cx.background_executor().timer(interval).await;
            }
        });
        Self {
            store: BitbucketStore::global(cx),
            prs: Vec::new(),
            handled,
            repo_pushes: HashMap::default(),
            last_full_scan: None,
            seeded,
            checking: false,
            last_checked: None,
            error: None,
            _poll: poll,
        }
    }

    /// Open pull requests, most recently updated first.
    pub fn prs(&self) -> &[WatchedPr] {
        &self.prs
    }

    pub fn get(&self, key: &PrKey) -> Option<&WatchedPr> {
        self.prs.iter().find(|p| p.key == *key)
    }

    pub fn changed_count(&self) -> usize {
        self.prs.iter().filter(|p| p.is_changed()).count()
    }

    pub fn is_checking(&self) -> bool {
        self.checking
    }

    pub fn last_checked(&self) -> Option<SystemTime> {
        self.last_checked
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Check now instead of waiting for the next poll.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        self.check(cx).detach();
    }

    /// Record the pull request's current state as handled: its changes are
    /// cleared, and the next change after this notifies again.
    pub fn mark_handled(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(pr) = self.prs.iter_mut().find(|p| p.key == *key) else {
            return;
        };
        let at = unix_now();
        pr.changes.clear();
        pr.handled_at = Some(at);
        let snapshot = pr.current.clone();
        self.save_handled(key, at, snapshot, cx);
        cx.emit(WatchEvent::Updated);
        cx.notify();
    }

    fn save_handled(&mut self, key: &PrKey, at: i64, snapshot: Snapshot, cx: &mut Context<Self>) {
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        self.handled.insert(
            key.clone(),
            Handled {
                at,
                snapshot,
                notified: false,
            },
        );
        let db = BitbucketDb::global(cx);
        let (workspace, slug, id) = (key.repo.workspace.clone(), key.repo.slug.clone(), key.id as i64);
        cx.background_spawn(async move { db.save_handled(workspace, slug, id, at, json).await })
            .detach_and_log_err(cx);
    }

    fn check(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.checking {
            return Task::ready(());
        }
        let read = match self.store.read(cx).client() {
            Ok(read) => read,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
                return Task::ready(());
            }
        };
        self.checking = true;
        cx.emit(WatchEvent::Updated);
        cx.notify();

        let settings = BitbucketSettings::get_global(cx).clone();
        let full_scan = self
            .last_full_scan
            .is_none_or(|at| at.elapsed() >= settings.full_scan_interval);
        let me = self.store.read(cx).me().cloned();
        let previous: HashMap<PrKey, (Option<String>, Snapshot)> = self
            .prs
            .iter()
            .map(|p| (p.key.clone(), (p.pull_request.updated_on.clone(), p.current.clone())))
            .collect();
        let repos_with_mine: HashSet<RepoRef> = self.prs.iter().map(|p| p.key.repo.clone()).collect();
        let repo_pushes = self.repo_pushes.clone();
        let fetch = cx.background_spawn(async move {
            fetch_mine(
                read,
                me,
                settings.workspaces,
                previous,
                repo_pushes,
                repos_with_mine,
                full_scan,
            )
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.checking = false;
                this.last_checked = Some(SystemTime::now());
                match result {
                    Ok(result) => {
                        this.error = None;
                        this.apply(result, cx);
                    }
                    Err(e) => this.error = Some(format!("{e:#}")),
                }
                cx.emit(WatchEvent::Updated);
                cx.notify();
            })
            .ok();
        })
    }

    fn apply(&mut self, result: CheckResult, cx: &mut Context<Self>) {
        if result.full_scan {
            self.last_full_scan = Some(Instant::now());
        }
        self.repo_pushes = result.repo_pushes;
        let me_uuid = result.me.uuid.clone();
        let first_run = !self.seeded;
        let mut prs = Vec::new();
        let mut became_changed = Vec::new();
        for (key, pull_request, author, reviewer, current) in result.prs {
            if !self.handled.contains_key(&key) {
                // Nothing is news on the very first run, nor your own new pull
                // request. Any other pull request seen for the first time gets
                // a baseline (handled at 0) and counts as a review request.
                let at = if first_run || (author && !reviewer) { unix_now() } else { 0 };
                self.save_handled(&key, at, current.clone(), cx);
            }
            let handled = self.handled.get(&key);
            let mut changes = Vec::new();
            if handled.is_some_and(|h| h.at == 0) {
                changes.push(Change::ReviewerAdded {
                    who: result.me.display_name.clone(),
                    is_me: true,
                });
            }
            if let Some(handled) = handled {
                changes.extend(changes::changes(&handled.snapshot, &current, me_uuid.as_deref()));
            }
            let handled_at = handled.map(|h| h.at).filter(|at| *at > 0);
            if !changes.is_empty() && handled.is_some_and(|h| !h.notified) {
                became_changed.push(key.clone());
            }
            prs.push(WatchedPr {
                key,
                pull_request,
                author,
                reviewer,
                handled_at,
                changes,
                current,
            });
        }
        prs.sort_by(|a, b| b.pull_request.updated_on.cmp(&a.pull_request.updated_on));
        self.prs = prs;
        if first_run {
            self.seeded = true;
            let kvp = db::kvp::KeyValueStore::global(cx);
            cx.background_spawn(async move { kvp.write_kvp(SEEDED_KEY.into(), "1".into()).await })
                .detach_and_log_err(cx);
        }
        let db = BitbucketDb::global(cx);
        for key in became_changed {
            if let Some(handled) = self.handled.get_mut(&key) {
                handled.notified = true;
                let db = db.clone();
                let (workspace, slug, id) = (key.repo.workspace.clone(), key.repo.slug.clone(), key.id as i64);
                cx.background_spawn(async move { db.set_notified(workspace, slug, id).await })
                    .detach_and_log_err(cx);
            }
            cx.emit(WatchEvent::BecameChanged(key));
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn key_of(pr: &PullRequest) -> Option<PrKey> {
    let full_name = &pr.destination.repository.as_ref()?.full_name;
    Some(PrKey {
        repo: full_name.parse().ok()?,
        id: pr.id,
    })
}

/// Find the open pull requests `me` authored or reviews, and snapshot each.
/// Comments and statuses are refetched only for a pull request that was
/// updated since the last check, or whose builds were still running.
async fn fetch_mine(
    read: Arc<Client>,
    me: Option<Account>,
    workspaces: Vec<String>,
    previous: HashMap<PrKey, (Option<String>, Snapshot)>,
    old_pushes: HashMap<RepoRef, String>,
    repos_with_mine: HashSet<RepoRef>,
    full_scan: bool,
) -> Result<CheckResult> {
    let me = match me {
        Some(me) => me,
        None => models::decode(read.current_user().await?)?,
    };
    let uuid = me.uuid.clone().context("the Bitbucket account has no uuid")?;

    let mut found: Vec<(PrKey, PullRequest, bool, bool)> = Vec::new();
    let mut add = |pr: PullRequest, author: bool, reviewer: bool| {
        let Some(key) = key_of(&pr) else { return };
        match found.iter_mut().find(|(k, ..)| *k == key) {
            Some((_, _, a, r)) => {
                *a |= author;
                *r |= reviewer;
            }
            None => found.push((key, pr, author, reviewer)),
        }
    };

    let mut repo_pushes = HashMap::default();
    for workspace in &workspaces {
        let authored: Vec<PullRequest> =
            models::decode(Value::Array(read.authored_pull_requests(workspace, &uuid).await?))?;
        for pr in authored {
            add(pr, true, false);
        }
        let repos = read.repositories_by_update(workspace).await?;
        let mut to_search = Vec::new();
        for repo in &repos {
            let (Some(slug), Some(updated)) = (repo["slug"].as_str(), repo["updated_on"].as_str()) else {
                continue;
            };
            let repo_ref = RepoRef::new(workspace.as_str(), slug);
            let pushed = old_pushes.get(&repo_ref).is_none_or(|old| old != updated);
            if full_scan || repos_with_mine.contains(&repo_ref) || (pushed && !old_pushes.is_empty()) {
                to_search.push(repo_ref.clone());
            }
            repo_pushes.insert(repo_ref, updated.to_string());
        }
        let lists = futures::future::join_all(to_search.iter().map(|r| read.reviewer_pull_requests(r, &uuid))).await;
        for list in lists {
            let prs: Vec<PullRequest> = models::decode(Value::Array(list?))?;
            for pr in prs {
                add(pr, false, true);
            }
        }
    }

    let snapshots = futures::future::join_all(found.iter().map(|(key, pr, _, _)| {
        let read = read.clone();
        let previous = previous.get(key).cloned();
        let me = me.clone();
        async move {
            let reuse = previous.filter(|(updated, snapshot)| {
                *updated == pr.updated_on && !snapshot.statuses.values().any(|s| s.state == "INPROGRESS")
            });
            if let Some((_, snapshot)) = reuse {
                let mut snapshot = snapshot;
                // Participants and head come with the listing, so they are always current.
                let fresh = Snapshot::new(pr, &[], &[], Some(&me));
                snapshot.participants = fresh.participants;
                snapshot.head = fresh.head;
                snapshot.state = fresh.state;
                snapshot.title = fresh.title;
                return anyhow::Ok(snapshot);
            }
            let (comments, statuses) = futures::join!(
                read.pull_request_comments(&key.repo, key.id),
                read.pull_request_statuses(&key.repo, key.id),
            );
            let comments: Vec<Comment> = models::decode(Value::Array(comments?))?;
            let statuses: Vec<BuildStatus> = models::decode(Value::Array(statuses?))?;
            Ok(Snapshot::new(pr, &comments, &statuses, Some(&me)))
        }
    }))
    .await;

    let mut prs = Vec::new();
    for ((key, pr, author, reviewer), snapshot) in found.into_iter().zip(snapshots) {
        prs.push((key, pr, author, reviewer, snapshot?));
    }
    Ok(CheckResult {
        me,
        prs,
        repo_pushes,
        full_scan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZedHttp;
    use crate::api::Auth;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A Bitbucket with one workspace, one repository and one open pull
    /// request that `{me}` reviews, whose comments the test can add to.
    struct FakeBitbucket {
        comments: Vec<Value>,
        updated_on: String,
    }

    fn pull_request(updated_on: &str) -> Value {
        json!({
            "id": 7, "title": "Fix reset", "state": "OPEN", "updated_on": updated_on,
            "author": { "display_name": "Malin", "uuid": "{malin}" },
            "source": { "branch": { "name": "fix" }, "commit": { "hash": "aaaaaaaaaaaa" } },
            "destination": { "branch": { "name": "main" }, "repository": { "full_name": "ws/repo" } },
            "participants": [{ "user": { "display_name": "Me", "uuid": "{me}" }, "role": "REVIEWER", "approved": false }]
        })
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<PrWatch>, Arc<Mutex<FakeBitbucket>>, Rc<RefCell<Vec<WatchEvent>>>) {
        let fake = Arc::new(Mutex::new(FakeBitbucket {
            comments: Vec::new(),
            updated_on: "2026-09-29T08:00:00".into(),
        }));
        let server = fake.clone();
        let client = FakeHttpClient::create(move |req| {
            let server = server.clone();
            async move {
                let state = server.lock();
                let body = match req.uri().path() {
                    "/2.0/user" => json!({ "display_name": "Me", "uuid": "{me}", "account_id": "me-1" }),
                    "/2.0/workspaces/ws/pullrequests/%7Bme%7D" => json!({ "values": [] }),
                    "/2.0/repositories/ws" => json!({ "values": [{ "slug": "repo", "updated_on": "2026-09-29T07:00:00" }] }),
                    "/2.0/repositories/ws/repo/pullrequests" => json!({ "values": [pull_request(&state.updated_on)] }),
                    "/2.0/repositories/ws/repo/pullrequests/7/comments" => json!({ "values": state.comments }),
                    "/2.0/repositories/ws/repo/pullrequests/7/statuses" => json!({ "values": [] }),
                    _ => return Ok(Response::builder().status(404).body(AsyncBody::empty())?),
                };
                Ok(Response::builder().status(200).body(AsyncBody::from(body.to_string()))?)
            }
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let watch = cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.bitbucket.get_or_insert_default().workspaces = Some(vec!["ws".into()]);
                });
            });
            let store = cx.new(|cx| {
                let http = Arc::new(ZedHttp::new(client, cx.background_executor().clone()));
                let auth = Auth::Bearer { token: "t".into() };
                BitbucketStore::with_http(http, Some(auth), "http://test.example/2.0".into(), cx)
            });
            BitbucketStore::set_global(store, cx);
            let watch = cx.new(PrWatch::new);
            let events = events.clone();
            cx.subscribe(&watch, move |_, event: &WatchEvent, _| {
                if matches!(event, WatchEvent::BecameChanged(_)) {
                    events.borrow_mut().push(event.clone());
                }
            })
            .detach();
            watch
        });
        (watch, fake, events)
    }

    fn add_comment(fake: &Mutex<FakeBitbucket>, id: u64, updated_on: &str) {
        let mut state = fake.lock();
        state.comments.push(json!({ "id": id, "content": { "raw": "why?" }, "user": { "display_name": "Malin", "uuid": "{malin}" } }));
        state.updated_on = updated_on.into();
    }

    fn check(watch: &Entity<PrWatch>, cx: &mut TestAppContext) {
        watch.update(cx, |watch, cx| watch.check_now(cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn notifies_once_per_handled_mark(cx: &mut TestAppContext) {
        let (watch, fake, events) = setup(cx);
        cx.run_until_parked();
        watch.read_with(cx, |watch, _| {
            assert_eq!(watch.error(), None);
            assert_eq!(watch.prs().len(), 1);
            assert!(watch.prs()[0].reviewer);
            assert_eq!(watch.changed_count(), 0, "the first run marks everything handled");
        });

        add_comment(&fake, 1, "2026-09-29T09:00:00");
        check(&watch, cx);
        watch.read_with(cx, |watch, _| {
            assert_eq!(watch.changed_count(), 1);
            assert_eq!(watch.prs()[0].summary(), "1 new comment");
        });
        assert_eq!(events.borrow().len(), 1);

        add_comment(&fake, 2, "2026-09-29T10:00:00");
        check(&watch, cx);
        watch.read_with(cx, |watch, _| assert_eq!(watch.prs()[0].summary(), "2 new comments"));
        assert_eq!(events.borrow().len(), 1, "no second notification before it is handled");

        let key = watch.read_with(cx, |watch, _| watch.prs()[0].key.clone());
        watch.update(cx, |watch, cx| watch.mark_handled(&key, cx));
        watch.read_with(cx, |watch, _| assert_eq!(watch.changed_count(), 0));

        add_comment(&fake, 3, "2026-09-29T11:00:00");
        check(&watch, cx);
        watch.read_with(cx, |watch, _| assert_eq!(watch.prs()[0].summary(), "1 new comment"));
        assert_eq!(events.borrow().len(), 2, "notifies again after being handled");
    }
}
