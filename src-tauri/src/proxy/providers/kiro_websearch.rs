//! Claude Code's standalone WebSearch subrequest uses a server tool, not a
//! Kiro inference tool. Execute it against regional Kiro MCP and return the
//! Anthropic server-tool envelope. No LLM tokens are estimated or billed here.
use axum::{response::IntoResponse, Json};
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::{ClaudeAdapter, ProviderAdapter};
use crate::{provider::Provider, proxy::ProxyError};

fn is_search(tool: &Value) -> bool {
    tool.get("type").and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("web_search_"))
}

pub fn has_native_search(body: &Value) -> bool {
    body.get("tools").and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(is_search))
}

fn invalid(message: &str) -> ProxyError { ProxyError::InvalidRequest(message.into()) }

fn query_and_tool(body: &Value) -> Result<(String, &Value), ProxyError> {
    let tools = body["tools"].as_array().ok_or_else(|| invalid("Missing web search tool"))?;
    if tools.len() != 1 || !is_search(&tools[0]) {
        return Err(invalid("Kiro native WebSearch currently requires a standalone search request; mixed tools are not supported"));
    }
    let tool = &tools[0];
    if tool["type"] != "web_search_20250305" {
        return Err(invalid("Kiro currently supports web_search_20250305 only"));
    }
    if tool.get("user_location").is_some() {
        return Err(invalid("Kiro WebSearch does not support user_location"));
    }
    if body.pointer("/tool_choice/type").and_then(Value::as_str) == Some("none") {
        return Err(invalid("WebSearch cannot execute with tool_choice none"));
    }
    if let Some(max) = tool.get("max_uses") {
        if max.as_u64().is_none_or(|n| n == 0) {
            return Err(invalid("WebSearch max_uses must be a positive integer"));
        }
    }
    for key in ["allowed_domains", "blocked_domains"] {
        if let Some(value) = tool.get(key) {
            let domains = value.as_array().ok_or_else(|| invalid("WebSearch domains must be arrays"))?;
            if domains.iter().any(|d| d.as_str().is_none_or(|s| s.is_empty() || s.contains('/') || s.contains(':') || s.contains('*'))) {
                return Err(invalid("WebSearch domain filters must contain plain hostnames"));
            }
        }
    }
    if tool.get("allowed_domains").is_some() && tool.get("blocked_domains").is_some() {
        return Err(invalid("Use allowed_domains or blocked_domains, not both"));
    }
    let messages = body["messages"].as_array().ok_or_else(|| invalid("Missing search messages"))?;
    let text = messages.iter().rev().filter(|m| m["role"] == "user").find_map(|m| {
        let text = match &m["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts.iter().filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        };
        (!text.trim().is_empty()).then_some(text)
    }).ok_or_else(|| invalid("Missing web search query"))?;
    let query = text.trim().strip_prefix("Perform a web search for the query:")
        .unwrap_or(text.trim()).trim();
    if query.is_empty() || query.chars().count() > 4000 {
        return Err(invalid("WebSearch query must contain 1 to 4000 characters"));
    }
    Ok((query.to_string(), tool))
}

fn mcp_url(base: &str) -> Result<String, ProxyError> {
    let url = reqwest::Url::parse(base).map_err(|_| invalid("Invalid Kiro endpoint"))?;
    let region = url.host_str().and_then(|h| h.strip_prefix("runtime."))
        .and_then(|h| h.strip_suffix(".kiro.dev"))
        .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        .ok_or_else(|| invalid("Expected a Kiro regional runtime endpoint"))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err(invalid("Expected an HTTPS Kiro regional runtime endpoint"));
    }
    Ok(format!("https://q.{region}.amazonaws.com/mcp"))
}

fn permitted(url: &str, tool: &Value) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else { return false; };
    if !matches!(url.scheme(), "https" | "http") { return false; }
    let Some(host) = url.host_str() else { return false; };
    let matches = |domain: &Value| domain.as_str().is_some_and(|d| {
        let d = d.trim_end_matches('.').to_ascii_lowercase();
        host == d || host.ends_with(&format!(".{d}"))
    });
    tool["allowed_domains"].as_array().is_none_or(|list| list.iter().any(matches))
        && !tool["blocked_domains"].as_array().is_some_and(|list| list.iter().any(matches))
}

async fn search(client: &reqwest::Client, url: &str, key: &str, query: &str) -> Result<Vec<Value>, &'static str> {
    let response = client.post(url).bearer_auth(key).header("TokenType", "API_KEY")
        .header("User-Agent", super::kiro::KIRO_USER_AGENT)
        .header("x-amz-user-agent", super::kiro::KIRO_AMZ_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .json(&json!({"jsonrpc":"2.0","id":Uuid::new_v4().to_string(),"method":"tools/call",
            "params":{"name":"web_search","arguments":{"query":query}}}))
        .send().await.map_err(|_| "unavailable")?;
    if response.status().as_u16() == 429 { return Err("too_many_requests"); }
    if !response.status().is_success() { return Err("unavailable"); }
    let mut data = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "unavailable")?;
        if data.len() + chunk.len() > 4 * 1024 * 1024 { return Err("unavailable"); }
        data.extend_from_slice(&chunk);
    }
    parse_results(&data)
}

fn parse_results(data: &[u8]) -> Result<Vec<Value>, &'static str> {
    let rpc: Value = serde_json::from_slice(data).map_err(|_| "unavailable")?;
    if rpc.get("error").is_some() || rpc.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
        return Err("unavailable");
    }
    let text = rpc.pointer("/result/content/0/text").and_then(Value::as_str).ok_or("unavailable")?;
    let result: Value = serde_json::from_str(text).map_err(|_| "unavailable")?;
    result["results"].as_array().cloned().ok_or("unavailable")
}

fn message(model: &str, query: &str, tool: &Value, results: Result<Vec<Value>, &'static str>) -> Value {
    let id = format!("srvtoolu_{}", Uuid::new_v4().simple());
    let (content, summary) = match results {
        Ok(results) => {
            let mut content = Vec::new();
            let mut summary = String::new();
            for result in results.iter().take(50) {
                let Some(url) = result["url"].as_str().filter(|u| permitted(u, tool)) else { continue; };
                let title = result["title"].as_str().unwrap_or(url);
                let snippet = result["snippet"].as_str().unwrap_or("");
                content.push(json!({"type":"web_search_result","url":url,"title":title,
                    "encrypted_content":snippet}));
                summary.push_str(&format!("{}\n{}\n{}\n\n", title, url, snippet));
            }
            if summary.is_empty() { summary = "No search results found.".into(); }
            (Value::Array(content), summary)
        }
        Err(code) => (json!({"type":"web_search_tool_result_error","error_code":code}), "Web search is temporarily unavailable.".into()),
    };
    json!({"id":format!("msg_{}", Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,
        "content":[{"type":"server_tool_use","id":id,"name":"web_search","input":{"query":query}},
            {"type":"web_search_tool_result","tool_use_id":id,"content":content},
            {"type":"text","text":summary}],
        "stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":0,"output_tokens":0,"server_tool_use":{"web_search_requests":1}}})
}

fn sse(message: &Value) -> String {
    let mut output = String::new();
    let mut emit = |value: Value| {
        output.push_str(&format!("event: {}\ndata: {}\n\n", value["type"].as_str().unwrap_or("error"), value));
    };
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"] = json!({"input_tokens":0,"output_tokens":0});
    emit(json!({"type":"message_start","message":start}));
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let delta = match block["type"].as_str() {
            Some("server_tool_use") => {
                initial["input"] = json!({});
                Some(json!({"type":"input_json_delta","partial_json":block["input"].to_string()}))
            }
            Some("text") => {
                initial["text"] = json!("");
                Some(json!({"type":"text_delta","text":block["text"]}))
            }
            _ => None,
        };
        emit(json!({"type":"content_block_start","index":index,"content_block":initial}));
        if let Some(delta) = delta { emit(json!({"type":"content_block_delta","index":index,"delta":delta})); }
        emit(json!({"type":"content_block_stop","index":index}));
    }
    emit(json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":message["usage"]}));
    emit(json!({"type":"message_stop"}));
    output
}

pub async fn handle(provider: &Provider, body: &Value) -> Result<axum::response::Response, ProxyError> {
    let (query, tool) = query_and_tool(body)?;
    if provider.uses_kiro_managed_auth() {
        return Err(invalid("Kiro WebSearch currently requires API Key authentication"));
    }
    let adapter = ClaudeAdapter::new();
    let auth = adapter.extract_auth(provider).ok_or_else(|| ProxyError::AuthError("Missing Kiro API Key".into()))?;
    let url = mcp_url(&adapter.extract_base_url(provider)?)?;
    let key = auth.api_key.trim();
    if !key.starts_with("ksk_") { return Err(ProxyError::AuthError("Kiro API Key must start with ksk_".into())); }
    let model = body["model"].as_str().ok_or_else(|| invalid("Missing model"))?;
    let result = search(&crate::proxy::http_client::get(), &url, key, &query).await;
    let response = message(model, &query, tool, result);
    if body["stream"].as_bool().unwrap_or(false) {
        Ok(([("content-type", "text/event-stream"), ("cache-control", "no-cache")], sse(&response)).into_response())
    } else {
        Ok(Json(response).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"model":"claude-sonnet-4-6","tools":[{"type":"web_search_20250305","name":"web_search"}],
            "messages":[{"role":"user","content":[{"type":"text","text":"Perform a web search for the query: Kiro docs"}]}]})
    }

    #[test]
    fn kiro_search_validates_query_tools_and_endpoint() {
        let mut body = request();
        assert!(has_native_search(&body));
        assert_eq!(query_and_tool(&body).unwrap().0, "Kiro docs");
        body["tools"].as_array_mut().unwrap().push(json!({"name":"read_file"}));
        assert!(query_and_tool(&body).is_err());
        assert!(!has_native_search(&json!({"tools":[{"name":"web_search","input_schema":{}}]})));
        assert_eq!(mcp_url("https://runtime.eu-central-1.kiro.dev").unwrap(), "https://q.eu-central-1.amazonaws.com/mcp");
        for base in ["https://runtime.us-east-1.kiro.dev.evil.test", "http://runtime.us-east-1.kiro.dev", "https://user@runtime.us-east-1.kiro.dev"] {
            assert!(mcp_url(base).is_err());
        }
    }

    #[test]
    fn kiro_search_pairs_blocks_filters_domains_and_finishes_sse() {
        let tool = json!({"allowed_domains":["kiro.dev"]});
        assert!(permitted("https://docs.kiro.dev/page", &tool));
        assert!(!permitted("https://kiro.dev.evil.test/page", &tool));
        assert!(!permitted("javascript:alert(1)", &json!({})));
        let response = message("test", "Kiro", &tool, Ok(vec![json!({"title":"Kiro","url":"https://kiro.dev","snippet":"Docs"})]));
        assert_eq!(response["content"][0]["id"], response["content"][1]["tool_use_id"]);
        assert_eq!(response["content"][1]["content"][0]["type"], "web_search_result");
        let stream = sse(&response);
        assert!(stream.contains("event: message_start\n"));
        assert!(stream.contains("input_json_delta"));
        assert!(stream.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        let error = message("test", "Kiro", &tool, Err("unavailable"));
        assert_eq!(error["content"][1]["content"]["error_code"], "unavailable");
        assert!(!sse(&error).contains("API_KEY"));
    }

    #[test]
    fn kiro_search_parses_nested_mcp_and_rejects_errors() {
        let rpc = json!({"result":{"isError":false,"content":[{"type":"text","text":json!({"results":[]}).to_string()}]}});
        assert!(parse_results(rpc.to_string().as_bytes()).unwrap().is_empty());
        for value in [json!({"error":{"message":"secret"}}), json!({"result":{"isError":true}}), json!({})] {
            assert_eq!(parse_results(value.to_string().as_bytes()).unwrap_err(), "unavailable");
        }
    }

    #[tokio::test]
    async fn kiro_search_http_contract_uses_api_key_and_rpc() {
        use axum::{routing::post, Router};
        let app = Router::new().route("/mcp", post(|headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer ksk_test");
            assert_eq!(headers["tokentype"], "API_KEY");
            assert!(!headers.contains_key("x-amz-target"));
            assert_eq!(body["method"], "tools/call");
            assert_eq!(body["params"]["name"], "web_search");
            assert_eq!(body["params"]["arguments"]["query"], "Kiro docs");
            Json(json!({"result":{"content":[{"type":"text","text":
                json!({"results":[{"title":"Kiro","url":"https://kiro.dev","snippet":"Docs"}]}).to_string()}]}}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let results = search(&client, &format!("http://{address}/mcp"), "ksk_test", "Kiro docs").await;
        server.abort();
        assert_eq!(results.unwrap()[0]["url"], "https://kiro.dev");
    }
}
