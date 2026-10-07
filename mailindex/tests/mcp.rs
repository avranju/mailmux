mod common;

use common::{config, fixture, repository};
use mailindex::{
    http::{AppState, router},
    index::{SearchIndex, worker::IndexWorker},
    ingest::normalize_message,
    search::SearchService,
};
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use schemars::schema_for;
use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[test]
fn mcp_inputs_publish_bounded_limits() {
    let search = schema_for!(mailindex::models::SearchRequest);
    let get = schema_for!(mailindex::mcp::server::MailGetRequest);
    let search_json = serde_json::to_value(search).unwrap().to_string();
    let get_json = serde_json::to_value(get).unwrap().to_string();
    assert!(search_json.contains("minimum"));
    assert!(search_json.contains("maximum"));
    assert!(get_json.contains("minimum"));
}

#[tokio::test]
async fn mcp_transport_discovers_and_calls_tools_with_authentication() {
    exercise_mcp_transport(false).await;
}

#[tokio::test]
async fn mcp_internal_hosts_return_public_citation_urls() {
    exercise_mcp_transport(true).await;
}

async fn exercise_mcp_transport(explicit_hosts: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&dir);
    cfg.server.public_base_url = "http://127.0.0.1".into();
    if explicit_hosts {
        cfg.server.public_base_url = "https://mail.example.com/archive".into();
        cfg.server.mcp_allowed_hosts =
            Some(vec!["mailindex:8090".into(), "mailindex-alias".into()]);
    }
    let expected_view_url = cfg.view_url("src", "mail-1");
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let normalized = normalize_message(
        "src".into(),
        "mail-1".into(),
        serde_json::json!({}),
        fixture("plain.eml"),
        &cfg.content,
    )
    .unwrap();
    repo.upsert(&normalized).await.unwrap();

    let (index, writer) =
        SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let index = Arc::new(index);
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        index.clone(),
        writer,
        Arc::new(Notify::new()),
        cfg.index.batch_size,
        cfg.index.commit_interval_ms,
        cancel.clone(),
    );
    let worker_task = tokio::spawn(worker.run());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let state = AppState {
        repo: repo.clone(),
        search: Arc::new(SearchService {
            repo,
            index,
            config: Arc::new(cfg.clone()),
        }),
        config: Arc::new(cfg),
        notify: Arc::new(Notify::new()),
        ready: Arc::new(AtomicBool::new(true)),
        token: Some("mcp-secret".into()),
        cancel: cancel.clone(),
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server_cancel = cancel.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_cancel.cancelled_owned())
            .await
            .unwrap();
    });
    let url = format!("http://{address}/mcp");
    let host = if explicit_hosts {
        "mailindex:8090".to_owned()
    } else {
        address.to_string()
    };

    let unauthorized = reqwest::Client::new()
        .post(&url)
        .header("host", &host)
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let http_client = reqwest::Client::new();
    let mut rejected_hosts = vec!["untrusted.example.com"];
    if explicit_hosts {
        rejected_hosts.extend(["mailindex:8091", "mailindex", "mail.example.com"]);
    }
    for rejected_host in rejected_hosts {
        let response = http_client
            .post(&url)
            .bearer_auth("mcp-secret")
            .header("host", rejected_host)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN,
            "{rejected_host}"
        );
    }

    let allowed_hosts = if explicit_hosts {
        vec![host, "MAILINDEX-ALIAS:9000".into()]
    } else {
        vec![host]
    };
    for host in allowed_hosts {
        let transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
            .auth_header("mcp-secret")
            .custom_headers([(reqwest::header::HOST, host.parse().unwrap())].into());
        let transport = StreamableHttpClientTransport::from_config(transport_config);
        let client = ().serve(transport).await.unwrap();
        let tools = client.peer().list_tools(None).await.unwrap();
        let mut names = tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["mail_get", "mail_search"]);

        let search_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("mail_search").with_arguments(
                    serde_json::json!({"query": "Distinctive plain"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let search_json = search_result.structured_content.unwrap();
        assert_eq!(search_json["results"][0]["source_id"], "mail-1");
        assert_eq!(search_json["results"][0]["view_url"], expected_view_url);

        let get_result = client
            .peer()
            .call_tool(
                CallToolRequestParams::new("mail_get").with_arguments(
                    serde_json::json!({"source": "src", "source_id": "mail-1", "max_chars": 12})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        let get_json = get_result.structured_content.unwrap();
        assert!(get_json["body"].as_str().unwrap().chars().count() <= 12);
        assert_eq!(get_json["view_url"], expected_view_url);

        let invalid = client
            .peer()
            .call_tool_once(
                CallToolRequestParams::new("mail_search").with_arguments(
                    serde_json::json!({"query": " "})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(matches!(
            invalid,
            rmcp::model::CallToolResponse::Complete(result) if result.is_error == Some(true)
        ));

        client.cancel().await.unwrap();
    }
    cancel.cancel();
    let _ = server_task.await;
    let _ = worker_task.await;
}
