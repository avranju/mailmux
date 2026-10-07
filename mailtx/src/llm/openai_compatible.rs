use crate::{
    config::{LlmResponseFormat, ResolvedLlmConfig},
    llm::{ExtractionPrompt, LlmFailure, transaction_schema},
};
use reqwest::{
    header::{AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub struct OpenAiCompatibleClient {
    http: reqwest::Client,
    config: ResolvedLlmConfig,
    authorization: Option<HeaderValue>,
}
impl OpenAiCompatibleClient {
    pub fn from_config(config: &ResolvedLlmConfig) -> anyhow::Result<Self> {
        Self::from_config_with_env(config, |name| std::env::var(name))
    }
    pub fn from_config_with_env(
        config: &ResolvedLlmConfig,
        lookup: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> anyhow::Result<Self> {
        let custom = config
            .custom
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("custom LLM configuration missing"))?;
        let authorization = if let Some(name) = &custom.api_key_env {
            let key = lookup(name)
                .map_err(|_| anyhow::anyhow!("configured LLM API key is unavailable"))?;
            if key.trim().is_empty() {
                anyhow::bail!("configured LLM API key is empty");
            }
            let mut h = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| anyhow::anyhow!("configured LLM API key is invalid"))?;
            h.set_sensitive(true);
            Some(h)
        } else {
            None
        };
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| anyhow::anyhow!("could not initialize LLM HTTP client"))?;
        Ok(Self {
            http,
            config: config.clone(),
            authorization,
        })
    }
    pub fn timeout(&self) -> Duration {
        self.config.timeout
    }
    pub fn model(&self) -> &str {
        &self.config.model
    }
    pub async fn complete(&self, prompt: &ExtractionPrompt) -> anyhow::Result<String> {
        let custom = self.config.custom.as_ref().unwrap();
        let mut request = self
            .http
            .post(custom.url.clone())
            .json(&build_request_body(&self.config, prompt));
        if let Some(value) = &self.authorization {
            request = request.header(AUTHORIZATION, value.clone());
        }
        let mut response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                anyhow::Error::new(LlmFailure::new("timeout", "LLM request timed out"))
            } else if e.is_connect() {
                anyhow::Error::new(LlmFailure::new("connection", "LLM connection failed"))
            } else {
                anyhow::Error::new(LlmFailure::new("transport", "LLM transport failed"))
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::new(LlmFailure::with_message(
                "http_status",
                format!("LLM HTTP request failed with status {}", status.as_u16()),
            )));
        }
        if response.content_length().is_some_and(|n| n > 1_048_576) {
            return Err(anyhow::Error::new(LlmFailure::new(
                "response_too_large",
                "LLM response too large",
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            if e.is_timeout() {
                anyhow::Error::new(LlmFailure::new("timeout", "LLM response read timed out"))
            } else {
                anyhow::Error::new(LlmFailure::new(
                    "transport",
                    "LLM response transport failed",
                ))
            }
        })? {
            if body.len() + chunk.len() > 1_048_576 {
                return Err(anyhow::Error::new(LlmFailure::new(
                    "response_too_large",
                    "LLM response too large",
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let envelope: ChatCompletionResponse = serde_json::from_slice(&body).map_err(|_| {
            anyhow::Error::new(LlmFailure::new(
                "invalid_envelope",
                "invalid LLM response envelope",
            ))
        })?;
        extract_response_text(envelope).map_err(|category| {
            anyhow::Error::new(LlmFailure::new(category, "LLM response content is invalid"))
        })
    }
}
fn build_request_body(config: &ResolvedLlmConfig, prompt: &ExtractionPrompt) -> Value {
    let mut body = json!({"model":config.model,"messages":[{"role":"system","content":prompt.system},{"role":"user","content":prompt.user}],"stream":false});
    if let Some(n) = config.max_tokens {
        body["max_tokens"] = json!(n);
    }
    if let Some(t) = config.temperature {
        body["temperature"] = json!(t);
    }
    match config.response_format {
        LlmResponseFormat::JsonSchema => {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"transaction","strict":true,"schema":transaction_schema()}})
        }
        LlmResponseFormat::JsonObject => body["response_format"] = json!({"type":"json_object"}),
        LlmResponseFormat::Prompt => {}
    }
    if let (Some(object), Some(custom)) = (body.as_object_mut(), config.custom.as_ref()) {
        for (k, v) in &custom.extra_body {
            object.insert(k.clone(), v.clone());
        }
    }
    body
}
#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}
#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: Option<Value>,
    refusal: Option<Value>,
    tool_calls: Option<Value>,
    function_call: Option<Value>,
}
fn extract_response_text(response: ChatCompletionResponse) -> Result<String, &'static str> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or("invalid_envelope")?;
    if choice.finish_reason.as_deref().is_some_and(|x| x != "stop") {
        return Err("incomplete_or_filtered");
    }
    let msg = choice.message;
    if msg.refusal.as_ref().is_some_and(|x| !x.is_null()) {
        return Err("refusal");
    }
    if msg.tool_calls.as_ref().is_some_and(|x| !x.is_null())
        || msg.function_call.as_ref().is_some_and(|x| !x.is_null())
    {
        return Err("tool_call");
    }
    match msg.content {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s),
        _ => Err("invalid_envelope"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LlmBackend, ResolvedOpenAiCompatibleConfig};
    use serde_json::Map;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config(format: LlmResponseFormat) -> ResolvedLlmConfig {
        ResolvedLlmConfig {
            backend: LlmBackend::OpenAiCompatible,
            model: "vendor/prefix::opaque-reasoning".into(),
            response_format: format,
            timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            max_tokens: Some(1024),
            temperature: Some(0.0),
            custom: Some(ResolvedOpenAiCompatibleConfig {
                url: reqwest::Url::parse("http://127.0.0.1:9000/prefix/v1/chat/completions")
                    .unwrap(),
                api_key_env: None,
                extra_body: Map::new(),
            }),
        }
    }

    #[test]
    fn request_body_preserves_model_and_modes() {
        let prompt = ExtractionPrompt {
            system: "sys".into(),
            user: "user".into(),
        };
        for (format, expected) in [
            (LlmResponseFormat::JsonSchema, "json_schema"),
            (LlmResponseFormat::JsonObject, "json_object"),
        ] {
            let body = build_request_body(&config(format), &prompt);
            assert_eq!(body["model"], "vendor/prefix::opaque-reasoning");
            assert_eq!(body["stream"], false);
            assert_eq!(body["messages"][0]["content"], "sys");
            assert_eq!(body["response_format"]["type"], expected);
        }
        let body = build_request_body(&config(LlmResponseFormat::Prompt), &prompt);
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn response_envelope_rejects_unsafe_shapes() {
        for json in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{"content":null},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"message":{"content":"x"},"finish_reason":"length"}]}"#,
            r#"{"choices":[{"message":{"content":"secret","refusal":"no"},"finish_reason":"stop"}]}"#,
        ] {
            let parsed: ChatCompletionResponse = serde_json::from_str(json).unwrap();
            assert!(extract_response_text(parsed).is_err());
        }
        let parsed: ChatCompletionResponse = serde_json::from_str(
            r#"{"usage":{"x":1},"choices":[{"message":{"content":"{}","reasoning_content":"ignored"},"finish_reason":null}]}"#,
        ).unwrap();
        assert_eq!(extract_response_text(parsed).unwrap(), "{}");
    }

    #[tokio::test]
    async fn posts_once_to_prefix_path() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + content_length {
                        break;
                    }
                }
            }
            let head = String::from_utf8_lossy(&request);
            assert!(head.starts_with("POST /prefix/v1/chat/completions HTTP/1.1"));
            assert!(head.to_ascii_lowercase().contains("content-length:"));
            let body = br#"{"choices":[{"message":{"content":"{}"},"finish_reason":"stop"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        let mut config = config(LlmResponseFormat::JsonSchema);
        config.custom.as_mut().unwrap().url =
            reqwest::Url::parse(&format!("http://{addr}/prefix/v1/chat/completions")).unwrap();
        let client = OpenAiCompatibleClient::from_config_with_env(&config, |_| {
            panic!("no key lookup expected")
        })
        .unwrap();
        let result = client
            .complete(&ExtractionPrompt {
                system: "sys".into(),
                user: "mail".into(),
            })
            .await;
        assert!(result.is_ok());
        let request = server.await.unwrap();
        assert!(request.contains("vendor/prefix::opaque-reasoning"));
    }

    #[tokio::test]
    async fn http_failure_is_categorized_and_response_body_is_redacted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await.unwrap();
            stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 23\r\nConnection: close\r\n\r\nsecret-response-sentinel").await.unwrap();
        });
        let mut config = config(LlmResponseFormat::Prompt);
        config.custom.as_mut().unwrap().url =
            reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap();
        let client =
            OpenAiCompatibleClient::from_config_with_env(&config, |_| panic!("unexpected lookup"))
                .unwrap();
        let error = client
            .complete(&ExtractionPrompt {
                system: "s".into(),
                user: "u".into(),
            })
            .await
            .unwrap_err();
        let failure = error.downcast_ref::<LlmFailure>().unwrap();
        assert_eq!(failure.category(), "http_status");
        assert!(!format!("{error:#}").contains("secret-response-sentinel"));
        server.await.unwrap();
    }

    #[test]
    fn explicit_auth_is_sensitive_and_only_looks_up_named_variable() {
        let mut config = config(LlmResponseFormat::Prompt);
        config.custom.as_mut().unwrap().api_key_env = Some("ONLY_THIS_KEY".into());
        let client = OpenAiCompatibleClient::from_config_with_env(&config, |name| {
            assert_eq!(name, "ONLY_THIS_KEY");
            Ok("secret-value".to_string())
        })
        .unwrap();
        assert!(client.authorization.unwrap().is_sensitive());
        assert!(
            OpenAiCompatibleClient::from_config_with_env(&config, |_| Ok(String::new())).is_err()
        );
        for value in ["   ", "bad\nheader"] {
            let error = OpenAiCompatibleClient::from_config_with_env(&config, |_| Ok(value.into()))
                .err()
                .expect("invalid key must fail");
            assert!(!format!("{error:#}").contains(value));
        }
        let error = OpenAiCompatibleClient::from_config_with_env(&config, |_| {
            Err(std::env::VarError::NotPresent)
        })
        .err()
        .unwrap();
        assert!(!format!("{error:#}").contains("ONLY_THIS_KEY"));
        let error = OpenAiCompatibleClient::from_config_with_env(&config, |_| {
            Err(std::env::VarError::NotUnicode("secret-nonunicode".into()))
        })
        .err()
        .unwrap();
        assert!(!format!("{error:#}").contains("secret-nonunicode"));
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .expect("fixture request timed out")
                .unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
    }

    async fn invoke(url: reqwest::Url, timeout: Duration) -> anyhow::Result<String> {
        let mut cfg = config(LlmResponseFormat::Prompt);
        cfg.timeout = timeout;
        cfg.connect_timeout = timeout;
        cfg.custom.as_mut().unwrap().url = url;
        let client =
            OpenAiCompatibleClient::from_config_with_env(&cfg, |_| panic!("no auth lookup"))?;
        tokio::time::timeout(
            timeout + Duration::from_secs(1),
            client.complete(&ExtractionPrompt {
                system: "s".into(),
                user: "u".into(),
            }),
        )
        .await
        .expect("bounded client call timed out")
    }

    #[tokio::test]
    async fn status_codes_are_safe_and_fail_without_retry() {
        for status in [404, 429, 503] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 {status} Failure\r\nContent-Length: 24\r\nConnection: close\r\n\r\nsecret-http-body-sentinel"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(150), listener.accept())
                        .await
                        .is_err(),
                    "retried request"
                );
            });
            let error = invoke(
                reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains(&status.to_string()));
            assert!(!format!("{error:#}").contains("secret-http-body-sentinel"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_connection_failures_are_safe() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest_addr = destination.local_addr().unwrap();
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            read_request(&mut stream).await;
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{dest_addr}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        let error = invoke(
            reqwest::Url::parse(&format!("http://{source_addr}/go")).unwrap(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<LlmFailure>().unwrap().category(),
            "http_status"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), destination.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = unused.local_addr().unwrap();
        drop(unused);
        let error = invoke(
            reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<LlmFailure>().unwrap().category(),
            "connection"
        );
    }

    #[tokio::test]
    async fn accepts_body_exactly_at_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            let prefix =
                br#"{"choices":[{"message":{"content":"{}"},"finish_reason":"stop"}],"padding":""#;
            let suffix = b"\"}";
            let mut body = prefix.to_vec();
            body.extend(std::iter::repeat_n(
                b'x',
                1_048_576 - prefix.len() - suffix.len(),
            ));
            body.extend_from_slice(suffix);
            assert_eq!(body.len(), 1_048_576);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        let text = invoke(
            reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap(),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(text, "{}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn header_and_body_deadlines_and_body_size_are_enforced() {
        for mode in ["headers", "body"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                if mode == "body" {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n")
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            });
            let error = invoke(
                reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap(),
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<LlmFailure>().unwrap().category(),
                "timeout"
            );
            server.await.unwrap();
        }
        for header in [
            "Content-Length: 1048577\r\n",
            "Transfer-Encoding: chunked\r\n",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                if header.starts_with("Content-Length") {
                    let response = format!("HTTP/1.1 200 OK\r\n{header}Connection: close\r\n\r\n");
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n100001\r\n").await.unwrap();
                    stream.write_all(&vec![b'x'; 1_048_577]).await.unwrap();
                    stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
                }
            });
            let error = invoke(
                reqwest::Url::parse(&format!("http://{addr}/chat")).unwrap(),
                Duration::from_secs(2),
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<LlmFailure>().unwrap().category(),
                "response_too_large"
            );
            server.await.unwrap();
        }
    }
}
