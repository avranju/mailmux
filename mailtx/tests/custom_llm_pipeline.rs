use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(
        count: usize,
        handler: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let saved = requests.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                saved.lock().unwrap().push(request.clone());
                let (status, body) = handler(&request);
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            url,
            requests,
            thread: Some(thread),
        }
    }
    fn finish(mut self) -> Vec<String> {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0;
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        if header_end.is_none()
            && let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n")
        {
            header_end = Some(pos + 4);
            let headers = String::from_utf8_lossy(&bytes[..pos]);
            content_length = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.parse().ok())
                })
                .unwrap_or(0);
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn root() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "mailtx-pipeline-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn firefly(count: usize) -> Fixture {
    let posts = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_posts = posts.clone();
    Fixture::start(count, move |request| {
        if request.starts_with("GET /api/v1/categories") {
            (200, r#"{"data":[{"attributes":{"name":"Food"}}],"meta":{"pagination":{"current_page":1,"total_pages":1}}}"#.into())
        } else if request.starts_with("GET /api/v1/search/transactions/count") {
            let count = seen_posts.lock().unwrap().len();
            (200, format!(r#"{{"count":{}}}"#, u8::from(count > 0)))
        } else if request.starts_with("POST /api/v1/transactions") {
            seen_posts.lock().unwrap().push(request.to_owned());
            (200, r#"{"data":{"id":"firefly-tx-1"}}"#.into())
        } else {
            (404, "{}".into())
        }
    })
}
fn response(body: &str) -> (u16, String) {
    (
        200,
        json!({"choices":[{"message":{"content":body},"finish_reason":"stop"}]}).to_string(),
    )
}
fn run(
    config: &std::path::Path,
    eml: &std::path::Path,
    id: u64,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let input = format!(
        r#"{{"event":{{"id":{id}}},"email":{{"subject":"Bank notice","sender":"allowed@example.test","raw_message_path":"{}"}}}}"#,
        eml.display()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_mailtx"));
    command
        .env("MAILTX_CONFIG", config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
fn setup_config(
    root: &std::path::Path,
    llm_url: &str,
    firefly_url: &str,
    key_line: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let eml = root.join("mail.eml");
    fs::write(&eml, "From: allowed@example.test\nSubject: Bank notice\nContent-Type: text/plain\n\nCard ending 1234: debit at Market.\n").unwrap();
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"
allowed_senders = ["allowed@example.test"]
tag = "mailtx-test-tag"
[llm]
backend = "openai_compatible"
model = "opaque/model::id"
base_url = "{llm_url}"
allow_insecure_http = true
{key_line}
[firefly]
base_url = "{}/api"
access_token = "firefly-secret-sentinel"
allow_insecure_http = true
default_asset_account_id = "acct1"
[[firefly.asset_accounts]]
id = "default"
firefly_account_id = "acct1"
"#,
            firefly_url
        ),
    )
    .unwrap();
    (config, eml)
}

#[test]
fn eligible_found_not_found_invalid_and_auth_isolation_pipeline() {
    let root = root();
    let llm = Fixture::start(1, |_| {
        response(
            r#"{"status":"found","amount":-12.34,"transaction_type":"withdrawal","narration":"Market","transaction_date":"2025-02-03","category":"Food"}"#,
        )
    });
    let ff = firefly(3);
    let (config, eml) = setup_config(&root, &llm.url, &ff.url, "api_key_env = \"CUSTOM_LLM_KEY\"");
    let out = run(
        &config,
        &eml,
        42,
        &[
            ("ANTHROPIC_API_KEY", "inherited-cloud-sentinel"),
            ("CUSTOM_LLM_KEY", "custom-key-sentinel"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["metadata"]["outcome"], "posted");
    assert!(stdout.contains("llm_calls_total"));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr)
            .matches("LLM extraction completed")
            .count(),
        1
    );
    let llm_requests = llm.finish();
    assert_eq!(llm_requests.len(), 1);
    assert!(llm_requests[0].starts_with("POST /v1/chat/completions"));
    assert!(llm_requests[0].contains("Bearer custom-key-sentinel"));
    assert!(llm_requests[0].contains("Food"));
    assert!(!llm_requests[0].contains("firefly-secret-sentinel"));
    let ff_requests = ff.finish();
    assert_eq!(ff_requests.len(), 3);
    assert!(
        ff_requests
            .iter()
            .all(|r| r.contains("Bearer firefly-secret-sentinel"))
    );
    let post: Value = serde_json::from_str(
        ff_requests
            .iter()
            .find(|r| r.starts_with("POST "))
            .unwrap()
            .split("\r\n\r\n")
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    let tx = &post["transactions"][0];
    assert_eq!(tx["amount"], "12.34");
    assert_eq!(tx["source_id"], "acct1");
    assert_eq!(tx["category_name"], "Food");
    assert_eq!(tx["tags"][0], "mailtx-test-tag");
    assert_eq!(tx["external_id"], "mailmux:event:42");
    assert!(tx["date"].as_str().unwrap().starts_with("2025-02-03"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("custom-key-sentinel"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn inherited_provider_key_is_not_sent_without_explicit_custom_auth() {
    let root = root();
    let llm = Fixture::start(1, |_| response(r#"{"status":"not_found","amount":null}"#));
    let ff = firefly(1);
    let (config, eml) = setup_config(&root, &llm.url, &ff.url, "");
    let out = run(
        &config,
        &eml,
        43,
        &[
            ("ANTHROPIC_API_KEY", "inherited-cloud-sentinel"),
            ("OPENAI_API_KEY", "inherited-openai-sentinel"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["metadata"]["outcome"],
        "no_transaction"
    );
    let requests = llm.finish();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    assert_eq!(ff.finish().len(), 1);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("inherited-cloud-sentinel"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn custom_http_error_and_invalid_output_are_redacted_and_logged_once() {
    for (status, body, expected_category) in [
        (401, "http-secret-sentinel", "http_status"),
        (
            200,
            r#"{"choices":[{"message":{"content":"output-secret-sentinel"}}]}"#,
            "invalid_json",
        ),
    ] {
        let root = root();
        let llm = Fixture::start(1, move |_| {
            if status == 200 {
                (200, body.into())
            } else {
                (status, body.into())
            }
        });
        let ff = firefly(1);
        let (config, eml) = setup_config(&root, &llm.url, &ff.url, "");
        let out = run(&config, &eml, 44, &[]);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("secret-sentinel"), "{stderr}");
        assert_eq!(
            stderr.matches("LLM extraction completed").count(),
            1,
            "{stderr}"
        );
        assert!(stderr.contains(expected_category), "{stderr}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["metadata"]["outcome"], "error");
        assert!(
            out.stdout
                .windows(b"llm_calls_total".len())
                .any(|x| x == b"llm_calls_total")
        );
        assert_eq!(llm.finish().len(), 1);
        assert_eq!(ff.finish().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn replay_uses_external_id_and_does_not_post_twice() {
    let root = root();
    let llm = Fixture::start(2, |_| {
        response(
            r#"{"status":"found","amount":5,"transaction_type":"deposit","narration":"Refund","transaction_date":"2025-02-03","category":"Food"}"#,
        )
    });
    let ff = firefly(5);
    let (config, eml) = setup_config(&root, &llm.url, &ff.url, "");
    for _ in 0..2 {
        let output = run(&config, &eml, 70, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(llm.finish().len(), 2);
    let requests = ff.finish();
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("POST ")).count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("GET /api/v1/search/transactions/count"))
            .count(),
        2
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_custom_key_fails_before_llm_request() {
    let root = root();
    let llm = Fixture::start(0, |_| (200, "{}".into()));
    let ff = Fixture::start(0, |_| (200, "{}".into()));
    let (config, eml) = setup_config(
        &root,
        &llm.url,
        &ff.url,
        "api_key_env = \"MISSING_CUSTOM_LLM_KEY\"",
    );
    let output = run(&config, &eml, 71, &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("configured LLM API key is unavailable")
    );
    assert_eq!(llm.finish().len(), 0);
    assert_eq!(ff.finish().len(), 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn timeout_has_one_safe_completion_log() {
    let root = root();
    let llm = Fixture::start(1, |_| {
        thread::sleep(Duration::from_secs(2));
        response(r#"{"status":"not_found"}"#)
    });
    let ff = Fixture::start(1, |_| {
        (
            200,
            r#"{"data":[],"meta":{"pagination":{"current_page":1,"total_pages":1}}}"#.into(),
        )
    });
    let (config, eml) = setup_config(
        &root,
        &llm.url,
        &ff.url,
        "timeout_secs = 1\nconnect_timeout_secs = 1",
    );
    let output = run(&config, &eml, 72, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("LLM extraction completed").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("error_category") && stderr.contains("timeout"),
        "{stderr}"
    );
    assert!(!stderr.contains("secret-sentinel"));
    assert_eq!(llm.finish().len(), 1);
    assert_eq!(ff.finish().len(), 1);
    fs::remove_dir_all(root).unwrap();
}
