use super::SlackWorkspace;
use anyhow::{Context, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout},
};

const RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(10);
const METHODS: &[&str] = &[
    "auth.test",
    "users.conversations",
    "users.info",
    "conversations.info",
    "conversations.history",
    "conversations.replies",
];

#[derive(Debug)]
pub(super) enum ApiError {
    Authentication(String),
    RateLimited(Duration),
    Request { method: String, code: String },
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication(code) => write!(
                f,
                "Slack session is invalid ({code}); replace the session token and matching cookies"
            ),
            Self::RateLimited(delay) => write!(
                f,
                "Slack rate limit; retry after {} seconds",
                delay.as_secs()
            ),
            Self::Request { method, code } => {
                write!(f, "Slack {method}: {}", crate::limits::log_text(code, 256))
            }
        }
    }
}
impl std::error::Error for ApiError {}

pub(super) fn retry_after(headers: &http::HeaderMap) -> Duration {
    Duration::from_secs(
        headers
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60)
            .clamp(1, 86400),
    )
}

pub(super) fn authentication_error(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ApiError>(),
        Some(ApiError::Authentication(_))
    )
}

#[derive(Debug)]
pub(crate) struct SessionApi {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
    token: http::HeaderValue,
    pub cookie: http::HeaderValue,
    // All requests share the server's Retry-After pause, including metadata lookups.
    pause: Mutex<Instant>,
    base: String,
}

impl SessionApi {
    #[cfg(test)]
    pub(super) fn test_endpoint(address: std::net::SocketAddr) -> Arc<Self> {
        crate::config::install_crypto_provider().unwrap();
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .build();
        Arc::new(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            token: "Bearer xoxc-test".parse().unwrap(),
            cookie: "d=xoxd-test".parse().unwrap(),
            pause: Mutex::new(Instant::now()),
            base: format!("http://{address}/api"),
        })
    }

    pub fn new(config: &SlackWorkspace) -> anyhow::Result<Arc<Self>> {
        crate::config::install_crypto_provider()?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_only()
            .enable_http1()
            .enable_http2()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Ok(Arc::new(Self {
            client,
            token: format!("Bearer {}", config.api_token).parse()?,
            cookie: config.cookie.parse()?,
            pause: Mutex::new(Instant::now()),
            base: "https://slack.com/api".into(),
        }))
    }

    /// Only explicitly allowed, read-only methods can be invoked. Hyper does
    /// not follow redirects, so credentials never follow an arbitrary Location.
    pub async fn call(&self, method: &str, params: &[(&str, &str)]) -> anyhow::Result<Value> {
        ensure!(
            METHODS.contains(&method),
            "session API method is not allowed: {method}"
        );
        let pause = *self.pause.lock().await;
        let remaining = pause.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            return Err(ApiError::RateLimited(remaining).into());
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().copied())
            .finish();
        let request = http::Request::post(format!("{}/{method}", self.base))
            .header(http::header::AUTHORIZATION, self.token.clone())
            .header(http::header::COOKIE, self.cookie.clone())
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(Full::new(Bytes::from(body)))?;
        timeout(API_TIMEOUT, async {
            let response = self.client.request(request).await?;
            if matches!(response.status().as_u16(), 401 | 403) {
                return Err(ApiError::Authentication(format!("HTTP {}", response.status())).into());
            }
            if response.status() == http::StatusCode::TOO_MANY_REQUESTS {
                let delay = retry_after(response.headers());
                *self.pause.lock().await = Instant::now() + delay;
                return Err(ApiError::RateLimited(delay).into());
            }
            ensure!(
                response.status().is_success(),
                "Slack {method} returned HTTP {}",
                response.status()
            );
            ensure!(
                response
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<usize>().ok())
                    .is_none_or(|n| n <= RESPONSE_BYTES),
                "Slack response exceeds 4 MiB"
            );
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Ok(chunk) = frame?.into_data() {
                    ensure!(
                        bytes.len().saturating_add(chunk.len()) <= RESPONSE_BYTES,
                        "Slack response exceeds 4 MiB"
                    );
                    bytes.extend_from_slice(&chunk);
                }
            }
            let result: Value =
                serde_json::from_slice(&bytes).context("invalid Slack response JSON")?;
            if result["ok"] != true {
                let code = result["error"].as_str().unwrap_or("unknown_error");
                if matches!(
                    code,
                    "invalid_auth"
                        | "not_authed"
                        | "token_expired"
                        | "token_revoked"
                        | "account_inactive"
                        | "session_expired"
                        | "org_login_required"
                ) {
                    return Err(ApiError::Authentication(code.into()).into());
                }
                return Err(ApiError::Request {
                    method: method.into(),
                    code: code.into(),
                }
                .into());
            }
            Ok(result)
        })
        .await
        .context("Slack session API request timed out")?
    }

    pub async fn recovery_call(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        // Rate-limited recovery resumes in a later pass; live reception continues.
        sleep(Duration::from_millis(250)).await;
        self.call(method, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    async fn fake(
        status: &str,
        headers: &str,
        body: &str,
    ) -> (SessionApi, tokio::task::JoinHandle<String>) {
        crate::config::install_crypto_provider().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let length = if headers.to_ascii_lowercase().contains("content-length:")
            || headers.to_ascii_lowercase().contains("transfer-encoding:")
        {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let response =
            format!("HTTP/1.1 {status}\r\nConnection: close\r\n{length}{headers}\r\n{body}");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    length = line.split(':').nth(1).unwrap().trim().parse().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            let _ = stream.get_mut().write_all(response.as_bytes()).await;
            request
        });
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .unwrap()
            .https_or_http()
            .enable_http1()
            .build();
        let api = SessionApi {
            client: Client::builder(TokioExecutor::new()).build(connector),
            token: "Bearer xoxc-test".parse().unwrap(),
            cookie: "d=xoxd-test%2F".parse().unwrap(),
            pause: Mutex::new(Instant::now()),
            base: format!("http://{address}/api"),
        };
        (api, task)
    }

    #[tokio::test]
    async fn sends_cookie_and_form_without_writing_read_markers() {
        let (api, task) = fake("200 OK", "", r#"{"ok":true}"#).await;
        assert!(api.call("conversations.mark", &[]).await.is_err());
        api.call(
            "conversations.history",
            &[("channel", "C1"), ("oldest", "1234.000001")],
        )
        .await
        .unwrap();
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("cookie: d=xoxd-test%2f"));
        assert!(request.contains("authorization: bearer xoxc-test"));
        assert!(request.contains("oldest=1234.000001"));
    }

    #[tokio::test]
    async fn does_not_follow_redirects_and_classifies_auth_and_rate_limits() {
        let (api, task) = fake("302 Found", "Location: https://example.com/\r\n", "").await;
        assert!(api.call("auth.test", &[]).await.is_err());
        task.await.unwrap();
        let (api, task) = fake("200 OK", "", r#"{"ok":false,"error":"token_revoked"}"#).await;
        assert!(authentication_error(
            &api.call("auth.test", &[]).await.unwrap_err()
        ));
        task.await.unwrap();
        let (api, task) = fake("429 Too Many Requests", "Retry-After: 60\r\n", "").await;
        assert!(matches!(
            api.call("auth.test", &[])
                .await
                .unwrap_err()
                .downcast_ref::<ApiError>(),
            Some(ApiError::RateLimited(_))
        ));
        task.await.unwrap();
        assert!(api.call("users.info", &[]).await.is_err());
    }

    #[tokio::test]
    async fn rejects_oversize_responses_before_reading_body() {
        let (api, task) = fake("200 OK", "Content-Length: 99999999\r\n", "").await;
        assert!(
            api.call("auth.test", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 4 MiB")
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn bounds_chunked_body_without_a_content_length() {
        let body = format!(
            "{:x}\r\n{}\r\n0\r\n\r\n",
            RESPONSE_BYTES + 1,
            "x".repeat(RESPONSE_BYTES + 1)
        );
        let (api, task) = fake("200 OK", "Transfer-Encoding: chunked\r\n", &body).await;
        assert!(
            api.call("auth.test", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 4 MiB")
        );
        task.await.unwrap();
    }
}
