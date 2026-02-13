use crate::config::Config;
use crate::memory::{self, Memory, MemoryCategory};
use crate::providers::{self, Provider};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Run a minimal HTTP gateway (webhook + health check)
/// Zero new dependencies — uses raw TCP + tokio.
#[allow(clippy::too_many_lines)]
pub async fn run_gateway(host: &str, port: u16, config: Config) -> Result<()> {
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;

    let provider: Arc<dyn Provider> = Arc::from(providers::create_provider(
        config.default_provider.as_deref().unwrap_or("openrouter"),
        config.api_key.as_deref(),
    )?);
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".into());
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> =
        Arc::from(memory::create_memory(&config.memory, &config.workspace_dir)?);

    println!("🦀 ZeroClaw Gateway listening on http://{addr}");
    println!("  POST /webhook  — {{\"message\": \"your prompt\"}}");
    println!("  GET  /health   — health check");
    println!("  Press Ctrl+C to stop.\n");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        let provider = provider.clone();
        let model = model.clone();
        let mem = mem.clone();
        let auto_save = config.memory.auto_save;

        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };

            let request = String::from_utf8_lossy(&buf[..n]);
            let first_line = request.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();

            if let [method, path, ..] = parts.as_slice() {
                tracing::info!("{peer} → {method} {path}");
                handle_request(&mut stream, method, path, &request, &provider, &model, temperature, &mem, auto_save).await;
            } else {
                let _ = send_response(&mut stream, 400, "Bad Request").await;
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    stream: &mut tokio::net::TcpStream,
    method: &str,
    path: &str,
    request: &str,
    provider: &Arc<dyn Provider>,
    model: &str,
    temperature: f64,
    mem: &Arc<dyn Memory>,
    auto_save: bool,
) {
    match (method, path) {
        ("GET", "/health") => {
            let body = serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "memory": mem.name(),
                "memory_healthy": mem.health_check().await,
            });
            let _ = send_json(stream, 200, &body).await;
        }

        ("POST", "/webhook") => {
            handle_webhook(stream, request, provider, model, temperature, mem, auto_save).await;
        }

        _ => {
            let body = serde_json::json!({
                "error": "Not found",
                "routes": ["GET /health", "POST /webhook"]
            });
            let _ = send_json(stream, 404, &body).await;
        }
    }
}

async fn handle_webhook(
    stream: &mut tokio::net::TcpStream,
    request: &str,
    provider: &Arc<dyn Provider>,
    model: &str,
    temperature: f64,
    mem: &Arc<dyn Memory>,
    auto_save: bool,
) {
    let body_str = request
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| request.split("\n\n").nth(1))
        .unwrap_or("");

    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body_str) else {
        let err = serde_json::json!({"error": "Invalid JSON. Expected: {\"message\": \"...\"}"});
        let _ = send_json(stream, 400, &err).await;
        return;
    };

    let Some(message) = parsed.get("message").and_then(|v| v.as_str()) else {
        let err = serde_json::json!({"error": "Missing 'message' field in JSON"});
        let _ = send_json(stream, 400, &err).await;
        return;
    };

    if auto_save {
        let _ = mem
            .store("webhook_msg", message, MemoryCategory::Conversation)
            .await;
    }

    match provider.chat(message, model, temperature).await {
        Ok(response) => {
            let body = serde_json::json!({"response": response, "model": model});
            let _ = send_json(stream, 200, &body).await;
        }
        Err(e) => {
            let err = serde_json::json!({"error": format!("LLM error: {e}")});
            let _ = send_json(stream, 500, &err).await;
        }
    }
}

async fn send_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

async fn send_json(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let json = serde_json::to_string(body).unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
        json.len()
    );
    stream.write_all(response.as_bytes()).await
}
