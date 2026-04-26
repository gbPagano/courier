use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::Value;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::pipeline::ErrorPolicy;
use crate::retry::RetryPolicy;
use crate::sinks::{ManagedSink, Sink, WriteOne};

/// HTTP sink. Sends each envelope to a configured endpoint as a JSON body.
///
/// Returns `Err` for any non-2xx response, network error, or timeout — so
/// `ManagedSink` applies the configured retry / `on_error` policy uniformly.
pub struct ApiSink {
    id: String,
    url: String,
    method: Method,
    headers: HeaderMap,
    body_format: BodyFormat,
    client: Client,
}

/// Shape of the JSON body sent to the endpoint.
///
/// `Payload` (default) sends only `env.payload` — the common case for
/// webhook receivers that already know the schema. `Envelope` sends the
/// whole envelope (`meta` + `payload`) for receivers that need the
/// metadata as well.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    #[default]
    Payload,
    Envelope,
}

impl ApiSink {
    pub fn new(
        id: impl Into<String>,
        url: impl Into<String>,
        method: Method,
        headers: HeaderMap,
        body_format: BodyFormat,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let mut builder = Client::builder();
        if let Some(t) = timeout {
            builder = builder.timeout(t);
        }
        let client = builder
            .build()
            .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;
        Ok(Self {
            id: id.into(),
            url: url.into(),
            method,
            headers,
            body_format,
            client,
        })
    }
}

#[async_trait]
impl WriteOne for ApiSink {
    fn id(&self) -> &str {
        &self.id
    }

    async fn write(&self, env: &Envelope) -> Result<()> {
        let body = match self.body_format {
            BodyFormat::Payload => &env.payload,
            BodyFormat::Envelope => &serde_json::to_value(env)?,
        };

        let resp = self
            .client
            .request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            // Surface the response body when present so failures are
            // diagnosable from logs / dead-letter entries.
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("HTTP error {status}: {body}"));
        }

        log::debug!("[{}] {} {} -> {}", self.id, self.method, self.url, status);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ApiSinkConfig {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: BodyFormat,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Registry factory for [`ApiSink`]. Registered by
/// `courier::registry::register_builtin` under kind `"api"`.
///
/// Retry and error policy are managed centrally by the registry and applied
/// to every sink uniformly — no per-sink config needed.
pub fn api_sink_factory(
    id: &str,
    config: Value,
    on_error: ErrorPolicy,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Sink>> {
    let config: ApiSinkConfig = parse_config("api", config)?;

    let method = match config.method.as_deref() {
        None => Method::POST,
        Some(m) => m.parse::<Method>().map_err(|_| {
            anyhow!("invalid config for component type 'api': unsupported HTTP method '{m}'")
        })?,
    };

    let mut headers = HeaderMap::with_capacity(config.headers.len());
    for (k, v) in config.headers {
        let name = HeaderName::try_from(&k).map_err(|_| {
            anyhow!("invalid config for component type 'api': invalid header name '{k}'")
        })?;
        let value = HeaderValue::try_from(&v).map_err(|_| {
            anyhow!("invalid config for component type 'api': invalid value for header '{k}'")
        })?;
        headers.insert(name, value);
    }

    let timeout = config.timeout_secs.map(Duration::from_secs);
    let api = ApiSink::new(id, config.url, method, headers, config.body, timeout)?;

    let mut sink = ManagedSink::new(api).with_error_policy(on_error);
    if let Some(policy) = retry {
        sink = sink.with_retry(policy);
    }
    Ok(Box::new(sink))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method as method_matcher, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_sink(
        url: String,
        method: Method,
        headers: HeaderMap,
        body_format: BodyFormat,
    ) -> ApiSink {
        ApiSink::new("api-sink", url, method, headers, body_format, None).unwrap()
    }

    #[tokio::test]
    async fn posts_payload_as_json_by_default() {
        let server = MockServer::start().await;
        Mock::given(method_matcher("POST"))
            .and(path("/hook"))
            .and(body_json(json!({ "n": 7 })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        let sink = build_sink(
            format!("{}/hook", server.uri()),
            Method::POST,
            HeaderMap::new(),
            BodyFormat::Payload,
        );

        let env = Envelope::new("src", json!({ "n": 7 }));
        sink.write(&env).await.expect("write should succeed");
    }

    #[tokio::test]
    async fn sends_full_envelope_when_body_envelope() {
        let server = MockServer::start().await;
        Mock::given(method_matcher("POST"))
            .and(path("/hook"))
            // Match on a payload field nested under "payload" — proves the
            // envelope wrapper is present, not just the bare payload.
            .and(wiremock::matchers::body_partial_json(json!({
                "payload": { "n": 1 },
                "meta": { "source_id": "src" }
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sink = build_sink(
            format!("{}/hook", server.uri()),
            Method::POST,
            HeaderMap::new(),
            BodyFormat::Envelope,
        );

        let env = Envelope::new("src", json!({ "n": 1 }));
        sink.write(&env).await.unwrap();
    }

    #[tokio::test]
    async fn forwards_custom_headers_and_method() {
        let server = MockServer::start().await;
        Mock::given(method_matcher("PUT"))
            .and(path("/items/42"))
            .and(header("authorization", "Bearer token-123"))
            .and(header("x-courier-source", "courier"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer token-123"),
        );
        headers.insert("x-courier-source", HeaderValue::from_static("courier"));

        let sink = build_sink(
            format!("{}/items/42", server.uri()),
            Method::PUT,
            headers,
            BodyFormat::Payload,
        );

        sink.write(&Envelope::new("src", json!({}))).await.unwrap();
    }

    #[tokio::test]
    async fn non_2xx_response_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method_matcher("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let sink = build_sink(
            format!("{}/hook", server.uri()),
            Method::POST,
            HeaderMap::new(),
            BodyFormat::Payload,
        );

        let err = sink
            .write(&Envelope::new("src", json!({})))
            .await
            .expect_err("expected non-2xx to surface as an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    // -----------------------------------------------------------------
    // Factory / config parsing
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn factory_defaults_method_to_post() {
        let server = MockServer::start().await;
        Mock::given(method_matcher("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sink = api_sink_factory(
            "api",
            json!({ "url": format!("{}/hook", server.uri()) }),
            ErrorPolicy::Drop,
            None,
        )
        .unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn(async move { sink.run(rx, cancel).await });

        tx.send(Envelope::new("src", json!({"hello": "world"})))
            .await
            .unwrap();
        drop(tx);
        handle.await.unwrap();
    }

    #[test]
    fn factory_rejects_invalid_method() {
        let err = api_sink_factory(
            "api",
            json!({ "url": "https://example.test/", "method": "FOO BAR" }),
            ErrorPolicy::Drop,
            None,
        )
        .err()
        .expect("expected invalid-method error");
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported HTTP method"), "{msg}");
    }

    #[test]
    fn factory_rejects_invalid_header_name() {
        let err = api_sink_factory(
            "api",
            json!({
                "url": "https://example.test/",
                "headers": { "bad header": "value" }
            }),
            ErrorPolicy::Drop,
            None,
        )
        .err()
        .expect("expected invalid-header error");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid header name"), "{msg}");
    }

    #[test]
    fn factory_reports_missing_url_with_uniform_prefix() {
        let err = api_sink_factory("api", json!({}), ErrorPolicy::Drop, None)
            .err()
            .expect("expected missing-url error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid config for component type 'api'"),
            "{msg}"
        );
    }
}
