use crate::api::{Http, HttpFailure, HttpRequest, HttpResponse};
use futures::AsyncReadExt as _;
use futures::future::BoxFuture;
use gpui::BackgroundExecutor;
use http_client::{AsyncBody, HttpClient, Request};
use std::sync::Arc;
use std::time::Duration;

/// The Bitbucket client's HTTP stack, on Zed's HTTP client and background timer.
pub struct ZedHttp {
    client: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
}

impl ZedHttp {
    pub fn new(client: Arc<dyn HttpClient>, executor: BackgroundExecutor) -> Self {
        Self { client, executor }
    }
}

impl Http for ZedHttp {
    fn send(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpFailure>> {
        let mut builder = Request::builder().method(req.method).uri(req.url.as_str());
        for (name, value) in &req.headers {
            builder = builder.header(*name, value.as_str());
        }
        let body = match req.body {
            Some(body) => AsyncBody::from(body),
            None => AsyncBody::empty(),
        };
        let request = builder.body(body);
        let client = self.client.clone();
        Box::pin(async move {
            let failure = |e: anyhow::Error| HttpFailure {
                message: format!("{e:#}"),
                // Zed's client does not say whether the server saw the request;
                // the client only retries such a failure for a GET.
                connect_or_timeout: true,
            };
            let request = request.map_err(|e| failure(e.into()))?;
            let mut response = client.send(request).await.map_err(failure)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let mut body = String::new();
            response
                .body_mut()
                .read_to_string(&mut body)
                .await
                .map_err(|e| failure(e.into()))?;
            Ok(HttpResponse {
                status,
                retry_after,
                body,
            })
        })
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(self.executor.timer(duration))
    }
}
