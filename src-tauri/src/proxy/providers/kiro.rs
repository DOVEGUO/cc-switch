//! Kiro Runtime request conversion helpers.

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::proxy::ProxyError;

const SYNTHETIC_ACK: &str = "I will fully incorporate this information when generating my responses, and explicitly acknowledge relevant parts of the summary when answering questions.";

pub const KIRO_DEFAULT_REGION: &str = "us-east-1";
pub const KIRO_DEFAULT_BASE_URL: &str = "https://runtime.us-east-1.kiro.dev";
pub const KIRO_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
pub const KIRO_USER_AGENT: &str = "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.17593 os/windows lang/rust/1.92.0 md/appVersion-2.10.0 app/AmazonQ-For-CLI";
pub const KIRO_AMZ_USER_AGENT: &str = "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.17593 os/windows lang/rust/1.92.0 m/F app/AmazonQ-For-CLI";
pub const KIRO_ORIGIN_API_KEY: &str = "AI_EDITOR";
pub const KIRO_ORIGIN_OAUTH: &str = "KIRO_CLI";

pub fn runtime_base_url(region: &str) -> String {
    let region = region.trim();
    let region = if region.is_empty() { KIRO_DEFAULT_REGION } else { region };
    format!("https://runtime.{region}.kiro.dev")
}

fn normalize_kiro_model(raw: &str) -> String {
    let trimmed = raw.trim();
    let has_one_m = trimmed.len() >= 4
        && trimmed.get(trimmed.len() - 4..).is_some_and(|suffix| suffix.eq_ignore_ascii_case("[1m]"));
    let base = if has_one_m {
        &trimmed[..trimmed.len() - 4]
    } else {
        trimmed
    };
    let mapped = match base {
        "claude-opus-5" => "claude-opus-5",
        "claude-opus-5-5" => "claude-opus-5.5",
        "claude-opus-4-8" => "claude-opus-4.8",
        "claude-opus-4-7" => "claude-opus-4.7",
        "claude-opus-4-6" => "claude-opus-4.6",
        "claude-opus-4.5" => "claude-opus-4.5",
        "claude-sonnet-5" => "claude-sonnet-5",
        "claude-sonnet-4-6" if has_one_m => "claude-sonnet-4.6-1m",
        "claude-sonnet-4-6" => "claude-sonnet-4.6",
        "claude-sonnet-4.5" if has_one_m => "claude-sonnet-4.5-1m",
        "claude-sonnet-4.5" => "claude-sonnet-4.5",
        "claude-fable-5-1" => "claude-fable-5.1",
        "claude-haiku-4.5" => "claude-haiku-4.5",
        "claude-gpt-5.6-sol" => "gpt-5.6-sol",
        "claude-gpt-5.6-terra" => "gpt-5.6-terra",
        "claude-gpt-5.6-luna" => "gpt-5.6-luna",
        "claude-auto" => "auto",
        other => other,
    };
    mapped.to_string()
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => part.get("text").and_then(Value::as_str).map(ToString::to_string),
                    "thinking" => part.get("thinking").and_then(Value::as_str).map(ToString::to_string),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn system_text(system: Option<&Value>) -> String {
    content_text(system)
}

fn image_from_anthropic(part: &Value) -> Option<Value> {
    if part.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let source = part.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let data = source.get("data")?.as_str()?;
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    let format = media_type
        .split_once('/')
        .map(|(_, suffix)| suffix)
        .unwrap_or("png")
        .replace("jpeg", "jpg");
    Some(json!({"format": format, "source": {"bytes": data}}))
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text").and_then(Value::as_str).map(ToString::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn user_parts(message: &Value, model: &str, include_tools: Option<&[Value]>, origin: &str) -> Value {
    let mut text = String::new();
    let mut results = Vec::new();
    let mut images = Vec::new();
    match message.get("content") {
        Some(Value::String(value)) => text.push_str(value),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() { text.push('\n'); }
                            text.push_str(value);
                        }
                    }
                    "tool_result" => {
                        let id = part.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                        if !id.is_empty() {
                            results.push(json!({
                                "toolUseId": id,
                                "status": if part.get("is_error").and_then(Value::as_bool).unwrap_or(false) { "error" } else { "success" },
                                "content": [{"text": tool_result_text(part.get("content"))}]
                            }));
                        }
                    }
                    "image" => {
                        if let Some(image) = image_from_anthropic(part) { images.push(image); }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    let mut user = json!({
        "content": text,
        "modelId": model,
        "origin": origin
    });
    if !images.is_empty() {
        user["images"] = Value::Array(images);
    }
    let mut context = Map::new();
    if let Some(tools) = include_tools.filter(|tools| !tools.is_empty()) {
        context.insert("tools".to_string(), Value::Array(tools.to_vec()));
    }
    if !results.is_empty() {
        context.insert("toolResults".to_string(), Value::Array(results));
    }
    if !context.is_empty() {
        user["userInputMessageContext"] = Value::Object(context);
    }
    user
}

fn assistant_history(message: &Value) -> Value {
    let mut text = String::new();
    let mut tool_uses = Vec::new();
    if let Some(Value::Array(parts)) = message.get("content") {
        for part in parts {
            match part.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(value) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() { text.push('\n'); }
                        text.push_str(value);
                    }
                }
                "tool_use" => {
                    let id = part.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = part.get("name").and_then(Value::as_str).unwrap_or("");
                    if !id.is_empty() && !name.is_empty() {
                        tool_uses.push(json!({
                            "toolUseId": id,
                            "name": name,
                            "input": part.get("input").cloned().unwrap_or_else(|| json!({}))
                        }));
                    }
                }
                _ => {}
            }
        }
    } else {
        text = content_text(message.get("content"));
    }
    let mut response = json!({
        "messageId": Uuid::new_v4().to_string(),
        "content": text
    });
    if !tool_uses.is_empty() {
        response["toolUses"] = Value::Array(tool_uses);
    }
    json!({"assistantResponseMessage": response})
}

fn history_user(message: &Value, model: &str, origin: &str) -> Value {
    json!({"userInputMessage": user_parts(message, model, None, origin)})
}

fn convert_tools(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.trim();
            if name.is_empty() { return None; }
            let description = tool.get("description").and_then(Value::as_str).unwrap_or("");
            let schema = tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}));
            Some(json!({
                "toolSpecification": {
                    "name": name,
                    "description": description,
                    "inputSchema": {"json": schema}
                }
            }))
        })
        .collect()
}

fn effort_from_body(body: &Value) -> Option<String> {
    if let Some(effort) = body.pointer("/output_config/effort").and_then(Value::as_str) {
        let normalized = effort.trim().to_ascii_lowercase();
        if !normalized.is_empty() { return Some(normalized); }
    }
    let thinking = body.get("thinking")?;
    if thinking.get("type").and_then(Value::as_str) == Some("disabled") {
        return Some("none".to_string());
    }
    thinking.get("budget_tokens").and_then(Value::as_u64).map(|budget| {
        match budget {
            0..=4095 => "low",
            4096..=15999 => "medium",
            _ => "high",
        }.to_string()
    }).or_else(|| Some("high".to_string()))
}

pub fn anthropic_to_kiro(
    body: Value,
    session_id: Option<&str>,
    origin: &str,
) -> Result<Value, ProxyError> {
    let raw_model = body.get("model").and_then(Value::as_str).ok_or_else(|| {
        ProxyError::TransformError("Kiro request is missing model".to_string())
    })?;
    let model = normalize_kiro_model(raw_model);
    let origin = if origin.trim().is_empty() {
        KIRO_ORIGIN_OAUTH
    } else {
        origin
    };
    let tools = convert_tools(&body);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut history = Vec::new();
    let system = system_text(body.get("system"));
    if !system.trim().is_empty() {
        history.push(json!({"userInputMessage": {"content": system, "origin": origin}}));
        history.push(json!({"assistantResponseMessage": {
            "messageId": Uuid::new_v4().to_string(),
            "content": SYNTHETIC_ACK
        }}));
    }

    let current_index = messages.iter().rposition(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
    });
    let current_index = current_index.filter(|index| {
        messages.iter().skip(index + 1).all(|message| {
            message.get("role").and_then(Value::as_str) != Some("user")
        })
    });

    let mut current_message = None;
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        if Some(index) == current_index && index == messages.len().saturating_sub(1) {
            current_message = Some(user_parts(message, &model, Some(&tools), origin));
            continue;
        }
        match role {
            "assistant" => history.push(assistant_history(message)),
            "user" => history.push(history_user(message, &model, origin)),
            _ => {}
        }
    }

    let mut current = current_message.unwrap_or_else(|| json!({
        "content": "Continue",
        "modelId": model,
        "origin": origin,
        "userInputMessageContext": if tools.is_empty() { Value::Null } else { json!({"tools": tools}) }
    }));
    if current.get("content").and_then(Value::as_str).unwrap_or("").is_empty()
        && current.pointer("/userInputMessageContext/toolResults").is_none()
    {
        current["content"] = json!("Continue");
    }
    if current.pointer("/userInputMessageContext").is_some_and(Value::is_null) {
        current.as_object_mut().unwrap().remove("userInputMessageContext");
    }

    let conversation_id = session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut conversation_state = json!({
        "conversationId": conversation_id,
        "chatTriggerType": "MANUAL",
        "agentTaskType": "vibe",
        "currentMessage": {"userInputMessage": current}
    });
    if !history.is_empty() {
        conversation_state["history"] = Value::Array(history);
    }
    let mut result = json!({"conversationState": conversation_state});
    if let Some(effort) = effort_from_body(&body) {
        if model.starts_with("gpt-5.6") {
            result["additionalModelRequestFields"] = json!({"reasoning": {"effort": effort}});
        } else {
            result["additionalModelRequestFields"] = json!({"output_config": {"effort": effort}});
        }
    }
    Ok(result)
}

pub fn apply_profile_arn(body: &mut Value, profile_arn: Option<&str>) {
    if let Some(profile_arn) = profile_arn.map(str::trim).filter(|value| !value.is_empty()) {
        body["profileArn"] = json!(profile_arn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_anthropic_tools_and_tool_results() {
        let body = json!({
            "model": "claude-sonnet-5[1M]",
            "system": [{"type":"text","text":"Be concise."}],
            "tools": [{"name":"read_file","description":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "messages": [
                {"role":"user","content":"read a"},
                {"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"read_file","input":{"path":"a"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"ok"}]}
            ]
        });
        let kiro = anthropic_to_kiro(body, None, KIRO_ORIGIN_API_KEY).unwrap();
        assert_eq!(kiro.pointer("/conversationState/currentMessage/userInputMessage/modelId").and_then(Value::as_str), Some("claude-sonnet-5"));
        assert_eq!(kiro.pointer("/conversationState/currentMessage/userInputMessage/origin").and_then(Value::as_str), Some(KIRO_ORIGIN_API_KEY));
        assert!(kiro.pointer("/conversationState/conversationId").and_then(Value::as_str).is_some());
        assert_eq!(kiro.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(kiro.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/toolResults/0/toolUseId").and_then(Value::as_str), Some("tool_1"));
        assert!(kiro.pointer("/conversationState/history/0/userInputMessage/content").is_some());
    }

    #[test]
    fn normalizes_claude_code_model_ids_for_kiro() {
        assert_eq!(normalize_kiro_model("claude-opus-5-5"), "claude-opus-5.5");
        assert_eq!(normalize_kiro_model("claude-opus-5-5[1M]"), "claude-opus-5.5");
        assert_eq!(normalize_kiro_model("claude-sonnet-4-6"), "claude-sonnet-4.6");
        assert_eq!(normalize_kiro_model("claude-sonnet-4-6[1m]"), "claude-sonnet-4.6-1m");
        assert_eq!(normalize_kiro_model("claude-gpt-5.6-sol"), "gpt-5.6-sol");
    }

    #[test]
    fn applies_profile_arn_only_when_present() {
        let mut body = json!({"conversationState": {}});
        apply_profile_arn(&mut body, Some("arn:aws:codewhisperer:us-east-1:123:profile/test"));
        assert_eq!(body["profileArn"], "arn:aws:codewhisperer:us-east-1:123:profile/test");
    }
}
