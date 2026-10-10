//! Minimal stdio MCP server for integration tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams, GetPromptResponse,
    GetPromptResult, ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams, Prompt,
    PromptMessage, Resource, Role, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerConfig, Tool, Implementation,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

#[derive(Clone, Default)]
struct Echo {
    extra: Arc<AtomicUsize>,
}

fn schema(props: serde_json::Value) -> Arc<serde_json::Map<String, serde_json::Value>> {
    let serde_json::Value::Object(map) = serde_json::json!({"type": "object", "properties": props}) else {
        unreachable!()
    };
    Arc::new(map)
}

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("echo-server", "1.0.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if std::env::var_os("ECHO_HANG_LIST").is_some() {
            std::future::pending::<()>().await;
        }
        let mut tools = vec![
            Tool::new("ping", "Replies pong", schema(serde_json::json!({}))),
            Tool::new("echo", "Echoes text", schema(serde_json::json!({"text": {"type": "string"}}))),
            Tool::new("grow", "Adds a tool and notifies", schema(serde_json::json!({}))),
            Tool::new("slow", "Sleeps for ms", schema(serde_json::json!({"ms": {"type": "integer"}}))),
        ];
        for i in 0..self.extra.load(Ordering::SeqCst) {
            tools.push(Tool::new(format!("extra_{i}"), "Dynamic", schema(serde_json::json!({}))));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let text = match request.name.as_ref() {
            "ping" => "pong".to_string(),
            "echo" => args.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            "grow" => {
                self.extra.fetch_add(1, Ordering::SeqCst);
                let _ = context.peer.notify_tool_list_changed().await;
                "grown".to_string()
            }
            "slow" => {
                let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(1000);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                "slept".to_string()
            }
            other if other.starts_with("extra_") => other.to_string(),
            other => return Err(ErrorData::invalid_params(format!("unknown tool {other}"), None)),
        };
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(text)])))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new("greet", Some("Greets"), None)]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "greet" {
            return Err(ErrorData::invalid_params("unknown prompt", None));
        }
        Ok(GetPromptResponse::Complete(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            "hello",
        )])))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![Resource::new("echo://readme", "readme")]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(vec![ResourceContents::text(
            "echo readme",
            request.uri,
        )])))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("echo-server ready token=supersecret");
    let service = Echo::default().serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
