pub mod tools;

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::circadian::CircadianEngine;
use crate::config::types::AppConfig;
use crate::event::EventBus;
use crate::mqtt::publish::Publisher;
use crate::state::{SharedState, StateCommand};

use tools::McpToolHandler;

pub async fn start_mcp_server(
    bind_addr: &str,
    state: SharedState,
    state_tx: mpsc::Sender<StateCommand>,
    publisher: Arc<Publisher>,
    config: Arc<AppConfig>,
    event_bus: EventBus,
    circadian_engine: Option<Arc<CircadianEngine>>,
) -> Result<()> {
    info!("Starting MCP server on {}", bind_addr);

    let handler = Arc::new(McpToolHandler::new(state, state_tx, publisher, config, event_bus, circadian_engine));

    let addr: std::net::SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("MCP server listening on {}", addr);

    loop {
        let (stream, _remote) = listener.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = service_fn(move |req| {
                let handler = handler.clone();
                async move { handle_request(req, handler).await }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                warn!("MCP connection error: {}", e);
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    handler: Arc<McpToolHandler>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() != Method::POST {
        return Ok(json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "Method not allowed"}),
        ));
    }

    let body = http_body_util::BodyExt::collect(req.into_body())
        .await
        .map(|c| c.to_bytes());

    let body = match body {
        Ok(b) => b,
        Err(_) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": "Failed to read body"}),
            ));
        }
    };

    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(None, -32700, "Parse error"),
            ));
        }
    };

    let id = request.get("id").cloned();
    let method = request["method"].as_str().unwrap_or("");

    let response = match method {
        "initialize" => json_rpc_result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "jeha",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "notifications/initialized" => {
            return Ok(json_response(StatusCode::OK, json!({})));
        }
        "tools/list" => {
            let tools = handler.tool_definitions();
            json_rpc_result(id, json!({ "tools": tools }))
        }
        "tools/call" => {
            let tool_name = request["params"]["name"].as_str().unwrap_or("");
            let args = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));

            match handler.call_tool(tool_name, &args).await {
                Ok(result) => json_rpc_result(
                    id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&result).unwrap_or_default()
                        }]
                    }),
                ),
                Err(e) => json_rpc_result(
                    id,
                    json!({
                        "isError": true,
                        "content": [{
                            "type": "text",
                            "text": e
                        }]
                    }),
                ),
            }
        }
        "ping" => json_rpc_result(id, json!({})),
        _ => json_rpc_error(id, -32601, &format!("Method not found: {}", method)),
    };

    Ok(json_response(StatusCode::OK, response))
}

fn json_rpc_result(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn json_rpc_error(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn json_response(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body_str)))
        .unwrap()
}
