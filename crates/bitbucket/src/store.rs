use crate::api::{Auth, Client, DEFAULT_API_BASE, Http, WriteRequest};
use crate::models::{self, Account, BuildStatus, Comment, DiffstatEntry, Pipeline, PullRequest, Step};
use crate::repo::RepoRef;
use crate::ZedHttp;
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task};
use serde_json::Value;
use std::sync::Arc;

/// The key the credential is stored under in Zed's credentials store.
const CREDENTIAL_URL: &str = "https://api.bitbucket.org";

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
    /// Connected, disconnected, or the account was loaded.
    ConnectionChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Connection {
    /// Reading the stored credential.
    Loading,
    Disconnected,
    Connected,
}

struct GlobalBitbucketStore(Entity<BitbucketStore>);

impl Global for GlobalBitbucketStore {}

pub struct BitbucketStore {
    http: Arc<dyn Http>,
    base: String,
    client: Option<Arc<Client>>,
    connection: Connection,
    /// Why the last connection attempt or credential read failed.
    connection_error: Option<String>,
    me: Option<Account>,
    pull_requests: HashMap<PrKey, PrData>,
    pipelines: HashMap<PipelineKey, Vec<Pipeline>>,
    steps: HashMap<(RepoRef, String), Vec<Step>>,
    fetching: HashSet<PrKey>,
    _load: Option<Task<()>>,
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

    /// Make `store` the one `global` returns, for a store built with `with_http`.
    pub fn set_global(store: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalBitbucketStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalBitbucketStore>().map(|g| g.0.clone())
    }

    fn empty(http: Arc<dyn Http>, base: String) -> Self {
        Self {
            http,
            base,
            client: None,
            connection: Connection::Loading,
            connection_error: None,
            me: None,
            pull_requests: HashMap::default(),
            pipelines: HashMap::default(),
            steps: HashMap::default(),
            fetching: HashSet::default(),
            _load: None,
        }
    }

    /// A store on Zed's HTTP client, connected with the credential saved in
    /// Zed's credentials store, if there is one.
    fn new(cx: &mut Context<Self>) -> Self {
        let http: Arc<dyn Http> = Arc::new(ZedHttp::new(cx.http_client(), cx.background_executor().clone()));
        let mut this = Self::empty(http, DEFAULT_API_BASE.to_string());
        let provider = zed_credentials_provider::global(cx);
        this._load = Some(cx.spawn(async move |this, cx| {
            let stored = provider.read_credentials(CREDENTIAL_URL, cx).await;
            this.update(cx, |this, cx| match stored {
                Ok(Some((email, token))) => {
                    let token = String::from_utf8_lossy(&token).to_string();
                    this.set_client(Some(Auth::new(&email, &token)), cx);
                }
                Ok(None) => this.set_client(None, cx),
                Err(e) => {
                    this.connection_error = Some(format!("reading the saved Bitbucket credential: {e:#}"));
                    this.set_client(None, cx);
                }
            })
            .ok();
        }));
        this
    }

    /// A store whose client sends through `http` with `auth`, instead of
    /// Zed's HTTP client and credentials store.
    pub fn with_http(http: Arc<dyn Http>, auth: Option<Auth>, base: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty(http, base);
        this.set_client(auth, cx);
        this
    }

    fn set_client(&mut self, auth: Option<Auth>, cx: &mut Context<Self>) {
        self.client = auth.map(|auth| Arc::new(Client::new(self.http.clone(), auth, self.base.clone())));
        self.connection = if self.client.is_some() {
            Connection::Connected
        } else {
            Connection::Disconnected
        };
        self.me = None;
        self.load_user(cx);
        cx.emit(StoreEvent::ConnectionChanged);
        cx.notify();
    }

    fn load_user(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let fetch = cx.background_spawn(async move { client.current_user().await });
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await.and_then(models::decode::<Account>);
            this.update(cx, |this, cx| match result {
                Ok(me) => {
                    this.me = Some(me);
                    this.connection_error = None;
                    cx.emit(StoreEvent::ConnectionChanged);
                    cx.notify();
                }
                Err(e) => {
                    this.connection_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Check `email` and `token` against Bitbucket, then save them in Zed's
    /// credentials store and use them. An empty email means an access token
    /// (Bearer); otherwise an Atlassian API token with its account email.
    pub fn connect(&mut self, email: String, token: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let auth = Auth::new(&email, &token);
        let client = Client::new(self.http.clone(), auth.clone(), self.base.clone());
        let check = cx.background_spawn(async move { client.current_user().await });
        let provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |this, cx| {
            let me: Account = models::decode(check.await.context("Bitbucket rejected the credential")?)?;
            save_credential(provider.as_ref(), &email, &token, cx).await?;
            this.update(cx, |this, cx| {
                this.set_client(Some(auth), cx);
                this.me = Some(me);
            })
        })
    }

    /// Forget the saved credential.
    pub fn disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |this, cx| {
            provider.delete_credentials(CREDENTIAL_URL, cx).await?;
            this.update(cx, |this, cx| this.set_client(None, cx))
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connection == Connection::Connected
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_error(&self) -> Option<&str> {
        self.connection_error.as_deref()
    }

    /// The account the credential belongs to, once loaded.
    pub fn me(&self) -> Option<&Account> {
        self.me.as_ref()
    }

    pub(crate) fn client(&self) -> Result<Arc<Client>> {
        self.client
            .clone()
            .ok_or_else(|| anyhow!("Not connected to Bitbucket: use Connect in the Pull requests panel"))
    }

    /// The Bitbucket repository a git remote URL points to.
    pub fn repo_for_remote(url: &str) -> Option<RepoRef> {
        crate::repo::parse_remote_url(url)
    }

    /// Open pull requests of `repo`, newest update first. Not cached: a picker
    /// asks once per opening.
    pub fn open_pull_requests(&self, repo: RepoRef, cx: &mut Context<Self>) -> Task<Result<Vec<PullRequest>>> {
        let read = match self.client() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        cx.background_spawn(async move {
            let rows = read.open_pull_requests(&repo, Some(100)).await?;
            models::decode(Value::Array(rows))
        })
    }

    pub fn pull_request(&self, key: &PrKey) -> Option<&PrData> {
        self.pull_requests.get(key)
    }

    /// Fetch the pull request, its comments, build statuses and diffstat, and
    /// emit `PrUpdated` when they arrive. A fetch already running for `key` is
    /// not started twice.
    pub fn refresh_pull_request(&mut self, key: PrKey, cx: &mut Context<Self>) {
        let read = match self.client() {
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
        let read = match self.client() {
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
        let read = match self.client() {
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
        let read = match self.client() {
            Ok(read) => read,
            Err(e) => return Task::ready(Err(e)),
        };
        cx.background_spawn(async move { read.step_log(&repo, &pipeline_uuid, &step_uuid).await })
    }

    /// Send a write planned by `api::plan`, then refetch the pull request it
    /// changed, if any.
    pub fn execute(&mut self, request: WriteRequest, refresh: Option<PrKey>, cx: &mut Context<Self>) -> Task<Result<Value>> {
        let client = match self.client() {
            Ok(client) => client,
            Err(e) => return Task::ready(Err(e)),
        };
        let send = cx.background_spawn(async move { client.execute(&request).await });
        cx.spawn(async move |this, cx| {
            let response = send.await.context("Bitbucket write failed")?;
            if let Some(key) = refresh {
                this.update(cx, |this, cx| this.refresh_pull_request(key, cx))?;
            }
            Ok(response)
        })
    }
}

async fn save_credential(
    provider: &dyn credentials_provider::CredentialsProvider,
    email: &str,
    token: &str,
    cx: &AsyncApp,
) -> Result<()> {
    provider
        .write_credentials(CREDENTIAL_URL, email.trim(), token.as_bytes(), cx)
        .await
        .context("saving the Bitbucket credential")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZedHttp;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use serde_json::json;

    fn listing(values: Value) -> Value {
        json!({ "values": values })
    }

    #[gpui::test]
    async fn refresh_decodes_pull_request_comments_statuses_and_diffstat(cx: &mut TestAppContext) {
        let client = FakeHttpClient::create(|req| async move {
            let body = match req.uri().path() {
                "/2.0/user" => json!({ "display_name": "Me", "uuid": "{me}" }),
                "/2.0/repositories/ws/repo/pullrequests/7" => json!({
                    "id": 7, "title": "Fix reset", "state": "OPEN",
                    "source": { "branch": { "name": "bugfix/reset" }, "commit": { "hash": "aaaaaaaaaaaa" } },
                    "destination": { "branch": { "name": "main" }, "commit": { "hash": "bbbbbbbbbbbb" } },
                    "participants": [{ "user": { "display_name": "Me", "uuid": "{me}" }, "role": "REVIEWER", "approved": true, "state": "approved" }]
                }),
                "/2.0/repositories/ws/repo/pullrequests/7/comments" => listing(json!([
                    { "id": 1, "content": { "raw": "why?" }, "inline": { "path": "src/a.c", "to": 12 } },
                    { "id": 2, "content": { "raw": "because" }, "parent": { "id": 1 } }
                ])),
                "/2.0/repositories/ws/repo/pullrequests/7/statuses" => listing(json!([{ "key": "build", "state": "FAILED" }])),
                "/2.0/repositories/ws/repo/pullrequests/7/diffstat" => listing(json!([
                    { "status": "modified", "lines_added": 3, "lines_removed": 1, "new": { "path": "src/a.c" } }
                ])),
                _ => return Ok(Response::builder().status(404).body(AsyncBody::empty())?),
            };
            Ok(Response::builder().status(200).body(AsyncBody::from(body.to_string()))?)
        });
        let store = cx.new(|cx| {
            let http = Arc::new(ZedHttp::new(client, cx.background_executor().clone()));
            let auth = Auth::Basic {
                email: "me@example.com".into(),
                token: "t".into(),
            };
            BitbucketStore::with_http(http, Some(auth), "http://test.example/2.0".into(), cx)
        });
        let key = PrKey {
            repo: RepoRef::new("ws", "repo"),
            id: 7,
        };
        store.update(cx, |store, cx| store.refresh_pull_request(key.clone(), cx));
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert_eq!(store.me().map(|me| me.display_name.as_str()), Some("Me"));
            let data = store.pull_request(&key).expect("pull request fetched");
            assert_eq!(data.error, None);
            assert!(!data.loading);
            let pr = data.pull_request.as_ref().unwrap();
            assert_eq!(pr.source.commit.as_ref().unwrap().hash, "aaaaaaaaaaaa");
            assert!(pr.participants[0].approved);
            assert_eq!(data.comments.len(), 2);
            assert_eq!(data.comments[0].inline.as_ref().unwrap().to, Some(12));
            assert_eq!(data.statuses[0].state, "FAILED");
            assert_eq!(data.diffstat[0].lines_added, 3);
        });
    }

    #[gpui::test]
    async fn writes_use_the_one_credential_and_a_disconnected_store_sends_nothing(cx: &mut TestAppContext) {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let client = FakeHttpClient::create(move |req| {
            let recorded = recorded.clone();
            async move {
                let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                recorded.lock().push((req.method().to_string(), req.uri().path().to_string(), auth));
                Ok(Response::builder().status(200).body(AsyncBody::from("{}".to_string()))?)
            }
        });
        let http: Arc<dyn Http> = cx.update(|cx| Arc::new(ZedHttp::new(client, cx.background_executor().clone())));
        let connected = cx.new(|cx| {
            BitbucketStore::with_http(http.clone(), Some(Auth::Bearer { token: "t".into() }), "http://test.example/2.0".into(), cx)
        });
        let request = crate::api::plan::approve(&RepoRef::new("ws", "repo"), 7, true);
        connected
            .update(cx, |store, cx| store.execute(request.clone(), None, cx))
            .await
            .unwrap();
        assert!(seen.lock().iter().any(|(method, path, auth)| method == "POST"
            && path == "/2.0/repositories/ws/repo/pullrequests/7/approve"
            && auth == "Bearer t"));

        let disconnected = cx.new(|cx| BitbucketStore::with_http(http, None, "http://test.example/2.0".into(), cx));
        let count = seen.lock().len();
        let result = disconnected.update(cx, |store, cx| store.execute(request, None, cx)).await;
        assert!(result.unwrap_err().to_string().contains("Not connected"));
        assert_eq!(seen.lock().len(), count);
    }
}
