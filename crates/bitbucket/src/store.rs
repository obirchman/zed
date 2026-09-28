use anyhow::{Context as _, Result, anyhow};
use bbt::config::{self, Profile};
use bbt::models::{self, Account, BuildStatus, Comment, DiffstatEntry, Pipeline, PullRequest, Step};
use bbt::read::ReadClient;
use bbt::repo::RepoRef;
use bbt::write::{WriteClient, WriteRequest};
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use serde_json::Value;
use std::sync::Arc;

use crate::ZedHttp;

/// One pull request: repository plus id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrKey {
    pub repo: RepoRef,
    pub id: u64,
}

/// The pipelines of one branch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PipelineKey {
    pub repo: RepoRef,
    pub branch: String,
}

/// Everything the store knows about one pull request, as of the last fetch.
#[derive(Clone, Debug, Default)]
pub struct PrData {
    pub pull_request: Option<PullRequest>,
    pub comments: Vec<Comment>,
    pub statuses: Vec<BuildStatus>,
    pub diffstat: Vec<DiffstatEntry>,
    /// The last fetch's error, shown in place of stale data being fresh.
    pub error: Option<String>,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    PrUpdated(PrKey),
    PipelinesUpdated(PipelineKey),
    StepsUpdated { repo: RepoRef, pipeline_uuid: String },
    UserUpdated,
}

struct GlobalBitbucketStore(Entity<BitbucketStore>);

impl Global for GlobalBitbucketStore {}

pub struct BitbucketStore {
    read: Option<Arc<ReadClient>>,
    write: Option<Arc<WriteClient>>,
    /// Why there is no read client (missing or invalid bbt credentials).
    config_error: Option<String>,
    me: Option<Account>,
    pull_requests: HashMap<PrKey, PrData>,
    pipelines: HashMap<PipelineKey, Vec<Pipeline>>,
    steps: HashMap<(RepoRef, String), Vec<Step>>,
    fetching: HashSet<PrKey>,
    _load_user: Option<Task<()>>,
}

impl EventEmitter<StoreEvent> for BitbucketStore {}

impl BitbucketStore {
    pub fn init_global(cx: &mut App) {
        let store = cx.new(Self::new);
        cx.set_global(GlobalBitbucketStore(store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalBitbucketStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalBitbucketStore>().map(|g| g.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            read: None,
            write: None,
            config_error: None,
            me: None,
            pull_requests: HashMap::default(),
            pipelines: HashMap::default(),
            steps: HashMap::default(),
            fetching: HashSet::default(),
            _load_user: None,
        };
        this.reload_credentials(cx);
        this
    }

    /// Build the clients from bbt's credentials (`bbt auth set read|write`).
    /// Lookups use the read credential, else the write credential.
    pub fn reload_credentials(&mut self, cx: &mut Context<Self>) {
        let http: Arc<dyn bbt::transport::Http> =
            Arc::new(ZedHttp::new(cx.http_client(), cx.background_executor().clone()));
        let base = bbt::transport::api_base();
        let loaded = config::config_dir().and_then(|dir| {
            let read = config::load_credential(&dir, Profile::Read)?;
            let write = config::load_credential(&dir, Profile::Write)?;
            Ok((read, write))
        });
        match loaded {
            Ok((read, write)) => {
                let lookup = read.map(|c| c.auth).or_else(|| write.as_ref().map(|c| c.auth.clone()));
                self.read = lookup.map(|auth| Arc::new(ReadClient::with_http(http.clone(), auth, base.clone())));
                self.write = write.map(|c| Arc::new(WriteClient::with_http(http.clone(), c.auth, base.clone())));
                self.config_error = self
                    .read
                    .is_none()
                    .then(|| config::missing_credential(Profile::Read).to_string());
            }
            Err(e) => {
                self.read = None;
                self.write = None;
                self.config_error = Some(e.to_string());
            }
        }
        self.load_user(cx);
    }

    fn load_user(&mut self, cx: &mut Context<Self>) {
        let Some(read) = self.read.clone() else {
            return;
        };
        let fetch = cx.background_spawn(async move { read.current_user().await });
        self._load_user = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await.and_then(models::decode::<Account>);
            this.update(cx, |this, cx| match result {
                Ok(me) => {
                    this.me = Some(me);
                    cx.emit(StoreEvent::UserUpdated);
                    cx.notify();
                }
                Err(e) => log::warn!("bitbucket: could not load the current user: {e:#}"),
            })
            .ok();
        }));
    }

    pub fn config_error(&self) -> Option<&str> {
        self.config_error.as_deref()
    }

    pub fn can_write(&self) -> bool {
        self.write.is_some()
    }

    /// The account the credential belongs to, once loaded.
    pub fn me(&self) -> Option<&Account> {
        self.me.as_ref()
    }

    fn reader(&self) -> Result<Arc<ReadClient>> {
        self.read
            .clone()
            .ok_or_else(|| anyhow!(self.config_error.clone().unwrap_or_else(|| "no Bitbucket credential".into())))
    }

    /// The Bitbucket repository a git remote URL points to.
    pub fn repo_for_remote(url: &str) -> Option<RepoRef> {
        bbt::repo::parse_remote_url(url)
    }

    /// Open pull requests of `repo`, newest update first. Not cached: a picker
    /// asks once per opening.
    pub fn open_pull_requests(&self, repo: RepoRef, cx: &mut Context<Self>) -> Task<Result<Vec<PullRequest>>> {
        let read = match self.reader() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        cx.background_spawn(async move {
            // The list endpoint leaves out participants unless asked for.
            let path = format!(
                "{}/pullrequests?pagelen=50&sort=-updated_on&state=OPEN&fields=%2Bvalues.participants,%2Bvalues.reviewers",
                bbt::read::repo_path(&repo)
            );
            let rows = read.get_all(&path, Some(100)).await?;
            Ok(models::decode(Value::Array(rows))?)
        })
    }

    pub fn pull_request(&self, key: &PrKey) -> Option<&PrData> {
        self.pull_requests.get(key)
    }

    /// Fetch the pull request, its comments, build statuses and diffstat, and
    /// emit `PrUpdated` when they arrive. A fetch already running for `key` is
    /// not started twice.
    pub fn refresh_pull_request(&mut self, key: PrKey, cx: &mut Context<Self>) {
        let read = match self.reader() {
            Ok(read) => read,
            Err(e) => {
                self.pull_requests.entry(key.clone()).or_default().error = Some(format!("{e:#}"));
                cx.emit(StoreEvent::PrUpdated(key));
                cx.notify();
                return;
            }
        };
        if !self.fetching.insert(key.clone()) {
            return;
        }
        self.pull_requests.entry(key.clone()).or_default().loading = true;
        cx.emit(StoreEvent::PrUpdated(key.clone()));
        let fetch_key = key.clone();
        let fetch = cx.background_spawn(async move {
            let (repo, id) = (&fetch_key.repo, fetch_key.id);
            let (pr, comments, statuses, diffstat) = futures::join!(
                read.pull_request(repo, id),
                read.pull_request_comments(repo, id),
                read.pull_request_statuses(repo, id),
                read.pull_request_diffstat(repo, id),
            );
            anyhow::Ok(PrData {
                pull_request: Some(models::decode(pr?)?),
                comments: models::decode(Value::Array(comments?))?,
                statuses: models::decode(Value::Array(statuses?))?,
                diffstat: models::decode(Value::Array(diffstat?))?,
                error: None,
                loading: false,
            })
        });
        cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.fetching.remove(&key);
                let entry = this.pull_requests.entry(key.clone()).or_default();
                match result {
                    Ok(data) => *entry = data,
                    Err(e) => {
                        entry.error = Some(format!("{e:#}"));
                        entry.loading = false;
                    }
                }
                cx.emit(StoreEvent::PrUpdated(key));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn pipelines(&self, key: &PipelineKey) -> Option<&[Pipeline]> {
        self.pipelines.get(key).map(Vec::as_slice)
    }

    pub fn refresh_pipelines(&mut self, key: PipelineKey, cx: &mut Context<Self>) -> Task<Result<()>> {
        let read = match self.reader() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        let fetch_key = key.clone();
        let fetch = cx.background_spawn(async move {
            let rows = read.branch_pipelines(&fetch_key.repo, &fetch_key.branch, Some(20)).await?;
            anyhow::Ok(models::decode::<Vec<Pipeline>>(Value::Array(rows))?)
        });
        cx.spawn(async move |this, cx| {
            let pipelines = fetch.await?;
            this.update(cx, |this, cx| {
                this.pipelines.insert(key.clone(), pipelines);
                cx.emit(StoreEvent::PipelinesUpdated(key));
                cx.notify();
            })
        })
    }

    pub fn steps(&self, repo: &RepoRef, pipeline_uuid: &str) -> Option<&[Step]> {
        self.steps
            .get(&(repo.clone(), pipeline_uuid.to_string()))
            .map(Vec::as_slice)
    }

    pub fn refresh_steps(&mut self, repo: RepoRef, pipeline_uuid: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let read = match self.reader() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        let (fetch_repo, fetch_uuid) = (repo.clone(), pipeline_uuid.clone());
        let fetch = cx.background_spawn(async move {
            let rows = read.pipeline_steps(&fetch_repo, &fetch_uuid).await?;
            anyhow::Ok(models::decode::<Vec<Step>>(Value::Array(rows))?)
        });
        cx.spawn(async move |this, cx| {
            let steps = fetch.await?;
            this.update(cx, |this, cx| {
                this.steps.insert((repo.clone(), pipeline_uuid.clone()), steps);
                cx.emit(StoreEvent::StepsUpdated { repo, pipeline_uuid });
                cx.notify();
            })
        })
    }

    /// One step's log text. Not cached: a running step's log grows.
    pub fn step_log(&self, repo: RepoRef, pipeline_uuid: String, step_uuid: String, cx: &mut Context<Self>) -> Task<Result<String>> {
        let read = match self.reader() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        cx.background_spawn(async move { Ok(read.step_log(&repo, &pipeline_uuid, &step_uuid).await?) })
    }

    /// Send a write planned by `bbt::write::plan`, then refetch the pull
    /// request it changed, if any.
    pub fn execute(&mut self, request: WriteRequest, refresh: Option<PrKey>, cx: &mut Context<Self>) -> Task<Result<Value>> {
        let Some(write) = self.write.clone() else {
            return Task::ready(Err(anyhow!(
                "no Bitbucket write credential: run `bbt auth set write`"
            )));
        };
        let send = cx.background_spawn(async move { write.execute(&request).await });
        cx.spawn(async move |this, cx| {
            let response = send.await.context("Bitbucket write failed")?;
            if let Some(key) = refresh {
                this.update(cx, |this, cx| this.refresh_pull_request(key, cx))?;
            }
            Ok(response)
        })
    }
}
