//! Creating a pull request: what the form needs to know first, reviewer
//! names, and the create call itself, which returns the already-open pull
//! request between the two branches instead of opening a second one.

use crate::api::{Client, NewPullRequest, plan};
use crate::models::{self, Account, PullRequest};
use crate::repo::RepoRef;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct CreateContext {
    /// The repository's main branch, the default destination.
    pub main_branch: Option<String>,
    /// Bitbucket's head commit of the source branch; `None` when the branch
    /// has not been pushed.
    pub source_remote_head: Option<String>,
    /// Reviewers Bitbucket adds to every pull request of the repository.
    pub default_reviewers: Vec<Account>,
    /// Workspace members, for matching reviewer names.
    pub members: Vec<Account>,
}

pub async fn load_context(client: &Client, repo: &RepoRef, source: &str) -> Result<CreateContext> {
    let (repository, branch, defaults, members) = futures::join!(
        client.repository(repo),
        client.branch(repo, source),
        client.default_reviewers(repo),
        client.workspace_members(&repo.workspace),
    );
    let repository = repository?;
    Ok(CreateContext {
        main_branch: repository["mainbranch"]["name"].as_str().map(str::to_string),
        source_remote_head: branch?.and_then(|b| b["target"]["hash"].as_str().map(str::to_string)),
        // Default reviewers need a permission some tokens lack; the form works without them.
        default_reviewers: defaults.map(|rows| models::decode(Value::Array(rows)).unwrap_or_default()).unwrap_or_default(),
        members: members.map(|rows| models::decode(Value::Array(rows)).unwrap_or_default()).unwrap_or_default(),
    })
}

/// Account UUIDs for comma-separated `names`, each matched (case-insensitive,
/// exact) against a member's nickname or display name, or given as a `{uuid}`.
pub fn resolve_names(names: &str, members: &[Account]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if name.starts_with('{') && name.ends_with('}') {
            out.push(name.to_string());
            continue;
        }
        let wanted = name.to_lowercase();
        let hits: Vec<&Account> = members
            .iter()
            .filter(|m| {
                m.display_name.to_lowercase() == wanted
                    || m.nickname.as_deref().is_some_and(|n| n.to_lowercase() == wanted)
            })
            .collect();
        match hits.as_slice() {
            [one] => out.push(
                one.uuid
                    .clone()
                    .ok_or_else(|| anyhow!("member {name:?} has no uuid"))?,
            ),
            [] => bail!("no member of the workspace is named {name:?}"),
            many => bail!(
                "{name:?} matches {} members; use the display name exactly or a {{uuid}}",
                many.len()
            ),
        }
    }
    Ok(out)
}

/// Create the pull request, or return the one already open between the same
/// branches. The flag says whether it was created now.
pub async fn create(client: &Client, repo: &RepoRef, pr: NewPullRequest) -> Result<(PullRequest, bool)> {
    if pr.source_branch == pr.destination_branch {
        bail!("source and destination are both {:?}", pr.source_branch);
    }
    if client.branch(repo, &pr.source_branch).await?.is_none() {
        bail!("{} is not on Bitbucket yet; push it first", pr.source_branch);
    }
    if client.branch(repo, &pr.destination_branch).await?.is_none() {
        bail!("{} has no branch {:?}", repo, pr.destination_branch);
    }
    if let Some(existing) = client
        .open_pull_requests_between(repo, &pr.source_branch, &pr.destination_branch)
        .await?
        .into_iter()
        .next()
    {
        return Ok((models::decode(existing)?, false));
    }
    let created = client.execute(&plan::create_pull_request(repo, &pr)).await?;
    Ok((models::decode(created)?, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZedHttp;
    use crate::api::Auth;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::sync::Arc;

    /// A fake Bitbucket where `feature/x` and `main` exist, with `open`
    /// pull requests between them; records each POST.
    fn fake(open: Vec<Value>, posts: Arc<Mutex<Vec<Value>>>, cx: &mut TestAppContext) -> Client {
        let client = FakeHttpClient::create(move |mut req| {
            let (open, posts) = (open.clone(), posts.clone());
            async move {
                let path = req.uri().path().to_string();
                let query = req.uri().query().unwrap_or_default().to_string();
                let body = if req.method() == "POST" {
                    let mut text = String::new();
                    futures::AsyncReadExt::read_to_string(req.body_mut(), &mut text).await?;
                    posts.lock().push(serde_json::from_str(&text)?);
                    json!({ "id": 9, "title": "new" })
                } else if path.ends_with("/refs/branches") {
                    let exists = query.contains("feature%2Fx") || query.contains("main");
                    json!({ "values": if exists { vec![json!({ "name": if query.contains("main") { "main" } else { "feature/x" } })] } else { vec![] } })
                } else if path.ends_with("/pullrequests") {
                    json!({ "values": open })
                } else {
                    return Ok(Response::builder().status(404).body(AsyncBody::empty())?);
                };
                Ok(Response::builder().status(200).body(AsyncBody::from(body.to_string()))?)
            }
        });
        let http = cx.update(|cx| Arc::new(ZedHttp::new(client, cx.background_executor().clone())));
        Client::new(http, Auth::Bearer { token: "t".into() }, "http://test.example/2.0")
    }

    fn new_pr(source: &str) -> NewPullRequest {
        NewPullRequest {
            title: "Add x".into(),
            description: String::new(),
            source_branch: source.into(),
            destination_branch: "main".into(),
            close_source_branch: true,
            draft: false,
            reviewers: vec!["{r}".into()],
        }
    }

    #[gpui::test]
    async fn returns_the_open_pull_request_instead_of_a_second_one(cx: &mut TestAppContext) {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let client = fake(vec![json!({ "id": 3, "title": "already" })], posts.clone(), cx);
        let repo = RepoRef::new("ws", "repo");
        let (pr, created) = cx
            .background_executor
            .spawn(async move { create(&client, &repo, new_pr("feature/x")).await })
            .await
            .unwrap();
        assert_eq!((pr.id, created), (3, false));
        assert!(posts.lock().is_empty());
    }

    #[gpui::test]
    async fn creates_when_none_is_open_and_refuses_an_unpushed_branch(cx: &mut TestAppContext) {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(fake(Vec::new(), posts.clone(), cx));
        let repo = RepoRef::new("ws", "repo");
        let (c, r) = (client.clone(), repo.clone());
        let (pr, created) = cx
            .background_executor
            .spawn(async move { create(&c, &r, new_pr("feature/x")).await })
            .await
            .unwrap();
        assert_eq!((pr.id, created), (9, true));
        assert_eq!(posts.lock()[0]["reviewers"], json!([{ "uuid": "{r}" }]));

        let error = cx
            .background_executor
            .spawn(async move { create(&client, &repo, new_pr("unpushed")).await })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not on Bitbucket yet"));
    }

    fn member(name: &str, nickname: &str, uuid: &str) -> Account {
        Account {
            display_name: name.into(),
            nickname: Some(nickname.into()),
            uuid: Some(uuid.into()),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_names_nicknames_and_uuids() {
        let members = vec![member("Malin Aldin", "malin", "{m}"), member("Jonas Ek", "jonas", "{j}")];
        assert_eq!(
            resolve_names("malin aldin, JONAS, {x}", &members).unwrap(),
            vec!["{m}".to_string(), "{j}".to_string(), "{x}".to_string()]
        );
        assert!(resolve_names("nobody", &members).is_err());
        assert_eq!(resolve_names(" , ", &members).unwrap(), Vec::<String>::new());
    }
}
