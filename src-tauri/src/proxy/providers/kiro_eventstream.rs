//! Kiro AWS EventStream decoding and OpenAI-compatible response bridge.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::io;
use uuid::Uuid;

use crate::proxy::ProxyError;

const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
enum KiroEvent {
    Text(String),
    Reasoning(String),
    ToolFragment {
        id: String,
        name: Option<String>,
        input: Option<String>,
        replace_input: bool,
        stop: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        stop_reason: Option<String>,
    },
    /// 上游上报的单请求计费量。API Key 模式实测只回 credits
    /// （`{"usage":0.0149,"unit":"credit"}`），没有逐请求 token。
    Metering {
        usage: f64,
        unit: String,
    },
    Stop(String),
    ContextUsage(f64),
    Error(String),
    Ignore,
}

/// 累计 meteringEvent 的 credits，只有 credit 单位参与合计（单复数都算）。
/// `None` 表示整条响应没出现过 metering 帧，与"出现过但为 0"区分开。
fn add_metering(acc: &mut Option<f64>, usage: f64, unit: &str) {
    if unit.eq_ignore_ascii_case("credit") || unit.eq_ignore_ascii_case("credits") {
        *acc.get_or_insert(0.0) += usage;
    }
}

/// Kiro 的钱不是 token 而是 credits/invocation，所以真实消耗只走日志：
/// 额度重置后把 `[Kiro] metering ... credit=` 逐条相加，与 `GetUsageLimits`
/// 的 `currentUsage` 增量对照即可验证计费链。不要把 credit 反算成 token。
fn log_metered_credit(model: &str, credit: Option<f64>) {
    if let Some(credit) = credit {
        log::info!("[Kiro] metering model={model} credit={credit:.6}");
    }
}

/// API Key responses report credits and `contextUsagePercentage` but no token
/// counts. Estimate from the context share of the model's input window, the
/// same fallback kirocc uses.
#[derive(Default)]
struct UsageEstimate {
    context_percentage: Option<f64>,
    output_chars: u64,
}

impl UsageEstimate {
    fn record_output(&mut self, text: &str) {
        self.output_chars += text.chars().count() as u64;
    }

    fn tokens(&self, model: &str) -> Option<(u64, u64, u64, u64)> {
        let percentage = self.context_percentage.filter(|p| p.is_finite() && *p > 0.0)?;
        let output = if self.output_chars == 0 { 0 } else { (self.output_chars / 4).max(1) };
        let total = (percentage.min(100.0) / 100.0 * context_window(model) as f64).round() as u64;
        Some((total.saturating_sub(output), output, 0, 0))
    }
}

/// `maxInputTokens` from Kiro ListAvailableModels.
fn context_window(model: &str) -> u64 {
    let model = super::kiro::normalize_kiro_model(model);
    match model.as_str() {
        "claude-opus-4.5" | "claude-sonnet-4.5" | "claude-sonnet-4" | "claude-haiku-4.5" | "glm-5" => 200_000,
        "deepseek-3.2" => 164_000,
        "minimax-m2.5" | "minimax-m2.1" => 196_000,
        "qwen3-coder-next" => 256_000,
        m if m == "auto" || m.starts_with("claude-") || m.starts_with("gpt-5.6") => 1_000_000,
        _ => 200_000,
    }
}

#[derive(Default)]
struct ToolAccumulator {
    current_id: String,
    current_name: String,
    current_input: String,
    next_index: usize,
}

#[derive(Debug)]
struct CompletedTool {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

impl ToolAccumulator {
    fn update(
        &mut self,
        id: String,
        name: Option<String>,
        input: Option<String>,
        replace_input: bool,
        stop: bool,
    ) -> Vec<CompletedTool> {
        let mut completed = Vec::new();
        let mut effective_id = id;
        if effective_id.is_empty() && self.current_id.is_empty() && name.is_some() {
            effective_id = Uuid::new_v4().to_string();
        }
        if !effective_id.is_empty()
            && !self.current_id.is_empty()
            && effective_id != self.current_id
        {
            if let Some(tool) = self.flush() {
                completed.push(tool);
            }
        }
        if self.current_id.is_empty() && !effective_id.is_empty() {
            self.current_id = effective_id;
        }
        if let Some(name) = name.filter(|value| !value.is_empty()) {
            self.current_name = name;
        }
        if let Some(input) = input {
            if replace_input {
                self.current_input.clear();
            }
            self.current_input.push_str(&input);
        }
        if stop {
            if let Some(tool) = self.flush() {
                completed.push(tool);
            }
        }
        completed
    }

    fn flush(&mut self) -> Option<CompletedTool> {
        if self.current_id.is_empty() {
            return None;
        }
        let index = self.next_index;
        self.next_index += 1;
        Some(CompletedTool {
            index,
            id: std::mem::take(&mut self.current_id),
            name: std::mem::take(&mut self.current_name),
            arguments: std::mem::take(&mut self.current_input),
        })
    }
}

#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<KiroEvent>, ProxyError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.buffer.len() < 12 {
                break;
            }
            let total_len = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
            let headers_len = u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
            if total_len < 16 || total_len > MAX_FRAME_SIZE {
                return Err(ProxyError::TransformError(format!(
                    "Kiro EventStream frame has invalid length {total_len}"
                )));
            }
            if headers_len > total_len.saturating_sub(16) {
                return Err(ProxyError::TransformError(format!(
                    "Kiro EventStream headers length {headers_len} exceeds frame"
                )));
            }
            if self.buffer.len() < total_len {
                break;
            }
            let frame: Vec<u8> = self.buffer.drain(..total_len).collect();
            validate_frame_crc(&frame)?;
            let headers = &frame[12..12 + headers_len];
            let payload = &frame[12 + headers_len..total_len - 4];
            let (message_type, event_type) = extract_headers(headers);
            events.push(decode_event(&message_type, &event_type, payload));
        }
        Ok(events)
    }

    fn finish(&self) -> Result<(), ProxyError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ProxyError::TransformError(format!(
                "Kiro EventStream ended with {} unparsed bytes",
                self.buffer.len()
            )))
        }
    }
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

fn validate_frame_crc(frame: &[u8]) -> Result<(), ProxyError> {
    if frame.len() < 16 {
        return Err(ProxyError::TransformError(
            "Kiro EventStream frame is too short".to_string(),
        ));
    }
    let expected_prelude = u32::from_be_bytes(frame[8..12].try_into().unwrap());
    let actual_prelude = crc32_ieee(&frame[..8]);
    if actual_prelude != expected_prelude {
        return Err(ProxyError::TransformError(format!(
            "Kiro EventStream prelude CRC mismatch: got {actual_prelude:08x}, expected {expected_prelude:08x}"
        )));
    }
    let expected_message =
        u32::from_be_bytes(frame[frame.len() - 4..].try_into().unwrap());
    let actual_message = crc32_ieee(&frame[..frame.len() - 4]);
    if actual_message != expected_message {
        return Err(ProxyError::TransformError(format!(
            "Kiro EventStream message CRC mismatch: got {actual_message:08x}, expected {expected_message:08x}"
        )));
    }
    Ok(())
}

fn extract_headers(headers: &[u8]) -> (String, String) {
    let mut message_type = String::new();
    let mut event_type = String::new();
    let mut i = 0usize;
    while i < headers.len() {
        let name_len = usize::from(headers[i]);
        i += 1;
        if i + name_len > headers.len() {
            break;
        }
        let name = String::from_utf8_lossy(&headers[i..i + name_len]);
        i += name_len;
        if i >= headers.len() {
            break;
        }
        let value_type = headers[i];
        i += 1;
        let value_len = match value_type {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                if i + 2 > headers.len() {
                    break;
                }
                let len = u16::from_be_bytes([headers[i], headers[i + 1]]) as usize;
                i += 2;
                len
            }
            _ => break,
        };
        if i + value_len > headers.len() {
            break;
        }
        if value_type == 7 {
            let value = String::from_utf8_lossy(&headers[i..i + value_len]);
            match name.as_ref() {
                ":message-type" => message_type = value.into_owned(),
                ":event-type" | ":exception-type" => event_type = value.into_owned(),
                _ => {}
            }
        }
        i += value_len;
    }
    (message_type, event_type)
}

fn unwrap_event<'a>(value: &'a Value, event_type: &str) -> &'a Value {
    value.get(event_type).unwrap_or(value)
}

fn decode_event(message_type: &str, event_type: &str, payload: &[u8]) -> KiroEvent {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(error) => {
            return KiroEvent::Error(format!(
                "Kiro {event_type} payload is not valid JSON: {error}"
            ))
        }
    };
    if matches!(message_type, "exception" | "error") {
        let body = unwrap_event(&value, event_type);
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(event_type)
            .to_string();
        return KiroEvent::Error(message);
    }
    let body = unwrap_event(&value, event_type);
    match event_type {
        "assistantResponseEvent" => body
            .get("content")
            .and_then(Value::as_str)
            .map(|value| KiroEvent::Text(value.to_string()))
            .unwrap_or(KiroEvent::Ignore),
        "reasoningContentEvent" => body
            .get("text")
            .and_then(Value::as_str)
            .map(|value| KiroEvent::Reasoning(value.to_string()))
            .unwrap_or(KiroEvent::Ignore),
        "toolUseEvent" => {
            let id = body
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let (input, replace_input) = match body.get("input") {
                Some(Value::String(value)) => (Some(value.clone()), false),
                Some(other) => (Some(other.to_string()), true),
                None => (None, false),
            };
            let stop = body.get("stop").and_then(Value::as_bool).unwrap_or(false);
            KiroEvent::ToolFragment {
                id,
                name,
                input,
                replace_input,
                stop,
            }
        }
        "metadataEvent" => {
            let usage = body.get("tokenUsage").unwrap_or(body);
            let stop_reason = body.get("stopReason").and_then(Value::as_str).map(str::to_string);
            if !["uncachedInputTokens", "cacheReadInputTokens", "cacheWriteInputTokens", "outputTokens"]
                .iter().any(|key| usage.get(*key).is_some_and(Value::is_u64)) {
                return stop_reason.map(KiroEvent::Stop).unwrap_or(KiroEvent::Ignore);
            }
            let uncached = usage
                .get("uncachedInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_read = usage
                .get("cacheReadInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = usage
                .get("cacheWriteInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = usage
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            KiroEvent::Usage {
                input_tokens: uncached.saturating_add(cache_read).saturating_add(cache_write),
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                stop_reason,
            }
        }
        // meteringEvent 有两种形态：API Key 模式实测只回 credits
        // （{"usage":0.0149,"unit":"credit"}，没有 token 字段），另一种带
        // inputTokens/outputTokens。credits 是唯一真实计费数据，优先取它；
        // 同一帧两者都有时也以 credit 为准——逐请求 token 另有 metadataEvent 提供。
        "meteringEvent" => {
            if let Some(usage) = body.get("usage").and_then(Value::as_f64) {
                KiroEvent::Metering {
                    usage,
                    unit: body
                        .get("unit")
                        .and_then(Value::as_str)
                        .unwrap_or("credit")
                        .to_string(),
                }
            } else if body.get("inputTokens").is_some_and(Value::is_u64)
                || body.get("outputTokens").is_some_and(Value::is_u64)
            {
                KiroEvent::Usage {
                    input_tokens: body.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens: body
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    stop_reason: None,
                }
            } else {
                KiroEvent::Ignore
            }
        }
        "contextUsageEvent" => body
            .get("contextUsagePercentage")
            .and_then(Value::as_f64)
            .map(KiroEvent::ContextUsage)
            .unwrap_or(KiroEvent::Ignore),
        "invalidStateEvent" => {
            let reason = body.get("reason").and_then(Value::as_str).unwrap_or("");
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(reason);
            KiroEvent::Error(if message.is_empty() {
                "Kiro returned invalidStateEvent".to_string()
            } else {
                message.to_string()
            })
        }
        _ => KiroEvent::Ignore,
    }
}

fn openai_chunk(id: &str, model: &str, delta: Value, finish_reason: Option<&str>) -> Bytes {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    });
    Bytes::from(format!("data: {}\n\n", payload))
}

fn finish_reason(reason: Option<&str>, used_tool: bool) -> &'static str {
    match reason {
        Some("MAX_TOKENS" | "max_tokens" | "MAX_OUTPUT_TOKENS") => "length",
        Some("TOOL_USE" | "tool_use") => "tool_calls",
        _ if used_tool => "tool_calls",
        _ => "stop",
    }
}

fn usage_chunk(
    id: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> Bytes {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
            "prompt_tokens_details": {
                "cached_tokens": cache_read_tokens,
                "cache_write_tokens": cache_write_tokens
            }
        }
    });
    Bytes::from(format!("data: {}\n\n", payload))
}

pub fn create_openai_sse_stream_from_kiro<E>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    model: String,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send
where
    E: std::error::Error + Send + 'static,
{
    async_stream::stream! {
        let id = format!("chatcmpl-{}", Uuid::new_v4());
        let mut decoder = Decoder::default();
        let mut tools = ToolAccumulator::default();
        let mut used_tool = false;
        let mut stop_reason = None;
        let mut latest_usage: Option<(u64,u64,u64,u64)> = None;
        let mut metered_credit: Option<f64> = None;
        let mut estimate = UsageEstimate::default();
        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    yield Err(io::Error::other(error.to_string()));
                    return;
                }
            };
            let events = match decoder.push(&chunk) {
                Ok(events) => events,
                Err(error) => {
                    yield Err(io::Error::other(error.to_string()));
                    return;
                }
            };
            for event in events {
                match event {
                    KiroEvent::Text(text) if !text.is_empty() => {
                        estimate.record_output(&text);
                        yield Ok(openai_chunk(&id, &model, json!({"content": text}), None));
                    }
                    KiroEvent::Reasoning(text) if !text.is_empty() => {
                        estimate.record_output(&text);
                        yield Ok(openai_chunk(&id, &model, json!({"reasoning_content": text}), None));
                    }
                    KiroEvent::ContextUsage(percentage) => estimate.context_percentage = Some(percentage),
                    KiroEvent::ToolFragment { id: tool_id, name, input, replace_input, stop } => {
                        for tool in tools.update(tool_id, name, input, replace_input, stop) {
                            estimate.record_output(&tool.name);
                            estimate.record_output(&tool.arguments);
                            used_tool = true;
                            let delta = json!({
                                "tool_calls": [{
                                    "index": tool.index,
                                    "id": tool.id,
                                    "type": "function",
                                    "function": {"name": tool.name, "arguments": tool.arguments}
                                }]
                            });
                            yield Ok(openai_chunk(&id, &model, delta, None));
                        }
                    }
                    KiroEvent::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, stop_reason: reason } => {
                        latest_usage = Some((input_tokens, output_tokens, cache_read_tokens, cache_write_tokens));
                        if reason.is_some() { stop_reason = reason; }
                    }
                    KiroEvent::Metering { usage, unit } => {
                        add_metering(&mut metered_credit, usage, &unit);
                    }
                    KiroEvent::Stop(reason) => stop_reason = Some(reason),
                    KiroEvent::Error(message) => {
                        yield Err(io::Error::other(message));
                        return;
                    }
                    _ => {}
                }
            }
        }
        if let Err(error) = decoder.finish() {
            yield Err(io::Error::other(error.to_string()));
            return;
        }
        if let Some(tool) = tools.flush() {
            estimate.record_output(&tool.name);
            estimate.record_output(&tool.arguments);
            used_tool = true;
            let delta = json!({
                "tool_calls": [{
                    "index": tool.index,
                    "id": tool.id,
                    "type": "function",
                    "function": {"name": tool.name, "arguments": tool.arguments}
                }]
            });
            yield Ok(openai_chunk(&id, &model, delta, None));
        }
        yield Ok(openai_chunk(
            &id,
            &model,
            json!({}),
            Some(finish_reason(stop_reason.as_deref(), used_tool)),
        ));
        if let Some((input, output, cache_read, cache_write)) = latest_usage.or_else(|| estimate.tokens(&model)) {
            yield Ok(usage_chunk(&id, &model, input, output, cache_read, cache_write));
        }
        log_metered_credit(&model, metered_credit);
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    }
}

pub fn eventstream_to_openai_response(body: &[u8], model: &str) -> Result<Value, ProxyError> {
    let mut decoder = Decoder::default();
    let mut tools = ToolAccumulator::default();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut completed_tools = Vec::new();
    let mut latest_usage: Option<(u64,u64,u64,u64)> = None;
    let mut metered_credit: Option<f64> = None;
    let mut estimate = UsageEstimate::default();
    let mut stop_reason = None;

    for event in decoder.push(body)? {
        match event {
            KiroEvent::Text(value) => text.push_str(&value),
            KiroEvent::Reasoning(value) => reasoning.push_str(&value),
            KiroEvent::ContextUsage(percentage) => estimate.context_percentage = Some(percentage),
            KiroEvent::ToolFragment { id, name, input, replace_input, stop } => {
                completed_tools.extend(tools.update(id, name, input, replace_input, stop));
            }
            KiroEvent::Usage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, stop_reason: reason } => {
                latest_usage = Some((input_tokens, output_tokens, cache_read_tokens, cache_write_tokens));
                if reason.is_some() { stop_reason = reason; }
            }
            KiroEvent::Metering { usage, unit } => {
                add_metering(&mut metered_credit, usage, &unit);
            }
            KiroEvent::Stop(reason) => stop_reason = Some(reason),
            KiroEvent::Error(message) => return Err(ProxyError::TransformError(message)),
            KiroEvent::Ignore => {}
        }
    }
    decoder.finish()?;
    if let Some(tool) = tools.flush() {
        completed_tools.push(tool);
    }
    estimate.record_output(&text);
    estimate.record_output(&reasoning);
    for tool in &completed_tools {
        estimate.record_output(&tool.name);
        estimate.record_output(&tool.arguments);
    }

    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !completed_tools.is_empty() {
        message["tool_calls"] = Value::Array(completed_tools.into_iter().map(|tool| {
            json!({
                "id": tool.id,
                "type": "function",
                "function": {"name": tool.name, "arguments": tool.arguments}
            })
        }).collect());
        if message["content"].as_str().is_some_and(str::is_empty) {
            message["content"] = Value::Null;
        }
    }
    let finish_reason = finish_reason(stop_reason.as_deref(), message.get("tool_calls").is_some());
    let mut response = json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}]
    });
    if let Some((input, output, cache_read, cache_write)) = latest_usage.or_else(|| estimate.tokens(model)) {
        response["usage"] = json!({
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
            "prompt_tokens_details": {
                "cached_tokens": cache_read,
                "cache_write_tokens": cache_write
            }
        });
    }
    log_metered_credit(model, metered_credit);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_not_silently_ignored() {
        for message_type in ["exception", "error"] {
            assert!(matches!(
                decode_event(message_type, "ServiceUnavailable", br#"{"message":"retry later"}"#),
                KiroEvent::Error(message) if message == "retry later"
            ));
        }
    }

    fn header(name: &str, value: &str) -> Vec<u8> {
        let mut out = vec![name.len() as u8];
        out.extend_from_slice(name.as_bytes());
        out.push(7);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn frame(event_type: &str, payload: Value) -> Vec<u8> {
        let mut headers = header(":message-type", "event");
        headers.extend(header(":event-type", event_type));
        let payload = payload.to_string().into_bytes();
        let total_len = 12 + headers.len() + payload.len() + 4;
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&(total_len as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let prelude_crc = crc32_ieee(&out[..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(&payload);
        let message_crc = crc32_ieee(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    #[test]
    fn parses_text_tool_and_usage() {
        let mut body = frame("assistantResponseEvent", json!({"content":"hello"}));
        body.extend(frame("toolUseEvent", json!({"toolUseId":"t1","name":"read","input":"{\"path\":\"a\"}"})));
        body.extend(frame("toolUseEvent", json!({"toolUseId":"t1","stop":true})));
        body.extend(frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":10,"cacheReadInputTokens":4,"cacheWriteInputTokens":2,"outputTokens":3}})));
        body.extend(frame("metadataEvent", json!({"stopReason":"TOOL_USE"})));
        body.extend(frame("meteringEvent", json!({"unit":"credit","usage":0.02})));
        let response = eventstream_to_openai_response(&body, "claude-sonnet-5").unwrap();
        assert_eq!(response["choices"][0]["message"]["content"], "hello");
        assert_eq!(response["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(response["usage"]["prompt_tokens"], 16);
        assert_eq!(response["usage"]["completion_tokens"], 3);
    }

    #[tokio::test]
    async fn kiro_stream_preserves_fragmented_tool_arguments() {
        let mut body = frame("toolUseEvent", json!({"toolUseId":"t1","name":"echo"}));
        body.extend(frame("toolUseEvent", json!({"toolUseId":"t1","input":"{\"text\":"})));
        body.extend(frame("toolUseEvent", json!({"toolUseId":"t1","input":"\"OK\"}"})));
        body.extend(frame("toolUseEvent", json!({"toolUseId":"t1","stop":true})));
        body.extend(frame("meteringEvent", json!({"unit":"credit","usage":0.02})));
        let chunks: Vec<Result<Bytes, io::Error>> = body.chunks(7)
            .map(|part| Ok(Bytes::copy_from_slice(part))).collect();
        let stream = create_openai_sse_stream_from_kiro(futures::stream::iter(chunks), "auto".into());
        tokio::pin!(stream);
        let mut output = String::new();
        while let Some(chunk) = stream.next().await {
            output.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        let events: Vec<Value> = output.lines().filter_map(|line| line.strip_prefix("data: "))
            .filter(|line| *line != "[DONE]").map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(events[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"text\":\"OK\"}");
        assert_eq!(events[1]["choices"][0]["finish_reason"], "tool_calls");
        assert!(events.iter().all(|event| event.get("usage").is_none()));
        assert!(output.ends_with("data: [DONE]\n\n"));
    }

    /// API Key responses (captured live) carry only credits and context share.
    fn credit_only_body() -> Vec<u8> {
        let mut body = frame("assistantResponseEvent", json!({"content":"OK, done here"}));
        body.extend(frame("metadataEvent", json!({"stopReason":"END_TURN"})));
        body.extend(frame("contextUsageEvent", json!({"contextUsagePercentage":0.6481000185012817})));
        body.extend(frame("meteringEvent", json!({"unit":"credit","unitPlural":"credits","usage":0.0149})));
        body
    }

    #[test]
    fn kiro_estimates_usage_from_context_share_when_tokens_are_absent() {
        let response = eventstream_to_openai_response(&credit_only_body(), "claude-sonnet-5").unwrap();
        // 0.6481% of the 1M window, minus 13 chars / 4 of output.
        assert_eq!(response["usage"]["completion_tokens"], 3);
        assert_eq!(response["usage"]["prompt_tokens"], 6478);
        let anthropic = super::super::transform::openai_to_anthropic(response).unwrap();
        assert_eq!(anthropic["usage"]["input_tokens"], 6478);
        assert_eq!(anthropic["usage"]["output_tokens"], 3);

        let small_window = eventstream_to_openai_response(&credit_only_body(), "claude-haiku-4.5").unwrap();
        assert_eq!(small_window["usage"]["prompt_tokens"], 1293);
    }

    #[test]
    fn kiro_reported_tokens_win_over_the_estimate() {
        let mut body = credit_only_body();
        body.extend(frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":10,"outputTokens":3}})));
        let response = eventstream_to_openai_response(&body, "claude-sonnet-5").unwrap();
        assert_eq!(response["usage"]["prompt_tokens"], 10);
        assert_eq!(response["usage"]["completion_tokens"], 3);
    }

    /// 估算出的 usage 仍然回给客户端（Claude Code 读它做上下文显示），
    /// 但 `handlers.rs` 的 `usage_is_estimated_only` 会拦住它、不写通用 Token 用量表。
    #[tokio::test]
    async fn kiro_stream_reports_estimated_usage_to_the_client() {
        let chunks: Vec<Result<Bytes, io::Error>> = credit_only_body().chunks(5)
            .map(|part| Ok(Bytes::copy_from_slice(part))).collect();
        let openai = create_openai_sse_stream_from_kiro(futures::stream::iter(chunks), "claude-sonnet-5[1M]".into());
        let anthropic = super::super::streaming::create_anthropic_sse_stream(openai);
        tokio::pin!(anthropic);
        let mut output = String::new();
        while let Some(chunk) = anthropic.next().await {
            output.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        let events: Vec<Value> = output.lines().filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str(line).ok()).collect();
        let usage = crate::proxy::usage::parser::TokenUsage::from_claude_stream_events(&events)
            .expect("collector sees billable usage");
        assert_eq!(usage.input_tokens, 6478);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn kiro_rejects_corrupt_or_truncated_frames() {
        let mut body = frame("assistantResponseEvent", json!({"content":"hello"}));
        let last = body.len() - 1;
        assert!(eventstream_to_openai_response(&body[..last], "auto").is_err());
        body[last] ^= 1;
        assert!(eventstream_to_openai_response(&body, "auto").is_err());
    }

    #[test]
    fn kiro_preserves_output_limit_stop_reason() {
        let mut body = frame("assistantResponseEvent", json!({"content":"partial"}));
        body.extend(frame("metadataEvent", json!({"stopReason":"MAX_TOKENS"})));
        let response = eventstream_to_openai_response(&body, "auto").unwrap();
        let anthropic = super::super::transform::openai_to_anthropic(response).unwrap();
        assert_eq!(anthropic["stop_reason"], "max_tokens");
    }

    /// API Key 模式的 meteringEvent 只带 credits，是唯一真实的计费数据。
    #[test]
    fn kiro_metering_event_records_upstream_credit() {
        // live 采集形态：{"unit":"credit","unitPlural":"credits","usage":0.0149}
        match decode_event(
            "event",
            "meteringEvent",
            br#"{"unit":"credit","unitPlural":"credits","usage":0.0149}"#,
        ) {
            KiroEvent::Metering { usage, unit } => {
                assert!((usage - 0.0149).abs() < 1e-9);
                assert_eq!(unit, "credit");
            }
            other => panic!("expected metering, got {other:?}"),
        }

        // 累计：只有 credit 单位计入（单复数、大小写不敏感），其他单位忽略
        let mut total = None;
        for (usage, unit) in [(0.0149, "credit"), (1.0, "Credit"), (9.0, "USD"), (0.5, "credits")] {
            let payload = json!({"usage": usage, "unit": unit}).to_string();
            match decode_event("event", "meteringEvent", payload.as_bytes()) {
                KiroEvent::Metering { usage, unit } => add_metering(&mut total, usage, &unit),
                other => panic!("expected metering, got {other:?}"),
            }
        }
        assert!(
            (total.expect("two credit frames") - 1.5149).abs() < 1e-9,
            "{total:?}"
        );

        // 带 token 的旧形态仍然走 Usage，行为不变
        assert!(matches!(
            decode_event("event", "meteringEvent", br#"{"inputTokens":10,"outputTokens":3}"#),
            KiroEvent::Usage { input_tokens: 10, output_tokens: 3, .. }
        ));
        // 两者都没有：Ignore（不把空帧当成 0 credit）
        assert!(matches!(
            decode_event("event", "meteringEvent", b"{}"),
            KiroEvent::Ignore
        ));
    }

    /// credit 不能污染回给客户端的 usage——Kiro 的钱是 credits，不是 token。
    #[test]
    fn kiro_metering_credit_never_becomes_tokens() {
        let mut body = frame("meteringEvent", json!({"unit":"credit","usage":12.5}));
        body.extend(frame("metadataEvent", json!({"stopReason":"END_TURN"})));
        let response = eventstream_to_openai_response(&body, "claude-sonnet-5").unwrap();
        assert!(response.get("usage").is_none());
    }
}
