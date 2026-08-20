use crate::{
    models::{DocumentResponse, SearchRequest, SearchResponse},
    search::{SearchError, SearchService},
};
use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MailGetRequest {
    pub source: String,
    pub source_id: String,
    #[schemars(range(min = 1))]
    pub max_chars: Option<usize>,
}

#[derive(Clone)]
pub struct MailMcpServer {
    pub search: Arc<SearchService>,
    pub tool_router: ToolRouter<Self>,
}

fn mcp_error(error: SearchError) -> String {
    match error {
        SearchError::Invalid(message) => message,
        SearchError::NotFound => "document not found".into(),
        SearchError::Internal(error) => {
            tracing::error!(%error, "MCP request failed internally");
            "internal server error".into()
        }
    }
}

impl MailMcpServer {
    pub fn new(search: Arc<SearchService>) -> Self {
        Self {
            search,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl MailMcpServer {
    #[tool(
        name = "mail_search",
        description = "Search a private historical email archive. Iterative searches are encouraged; retrieve an important result before relying on it."
    )]
    pub async fn mail_search(
        &self,
        Parameters(input): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, String> {
        self.search.search(input).await.map(Json).map_err(mcp_error)
    }
    #[tool(
        name = "mail_get",
        description = "Retrieve a normalized email from the archive. Call this before relying on a short search snippet for an important factual claim."
    )]
    pub async fn mail_get(
        &self,
        Parameters(input): Parameters<MailGetRequest>,
    ) -> Result<Json<DocumentResponse>, String> {
        self.search
            .get(&input.source, &input.source_id, input.max_chars)
            .await
            .map(Json)
            .map_err(mcp_error)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mailindex", "0.1.0"))
            .with_instructions("Read-only historical email search and retrieval.")
    }
}

pub fn service(
    search: Arc<SearchService>,
    cancellation: CancellationToken,
    allowed_hosts: Vec<String>,
) -> StreamableHttpService<MailMcpServer, LocalSessionManager> {
    let cfg = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_allowed_hosts(allowed_hosts)
        .with_cancellation_token(cancellation);
    let server = MailMcpServer::new(search);
    StreamableHttpService::new(move || Ok(server.clone()), Default::default(), cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn internal_mcp_errors_are_generic() {
        let message = mcp_error(SearchError::Internal(anyhow!("/secret/storage/path")));
        assert_eq!(message, "internal server error");
        assert!(!message.contains("storage"));
    }
}
