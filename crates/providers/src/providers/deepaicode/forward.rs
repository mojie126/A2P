//! DeepAI Code 的会话式转发：Chat 通道原样透传，Responses 通道翻译成 chat SSE。
//!
//! ── 为什么走 `conversation` ────────────────────────────────
//! 目录里 `api = openai-responses` 的模型（如 `gpt-6-astra`）只在
//! `/deepai_code/responses` 上可用，而网关对外只讲 Chat。插件读 Responses 的
//! SSE 事件并翻成 chat 帧，账号轮换、冷却与统计仍由网关负责。
//!
//! ── 非流式请求也按流式发 ────────────────────────────────────
//! 与 AStudio 同一处置：插件统一推 SSE 帧，非流式请求由网关侧聚合。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use super::accounts::Credentials;
use super::models;
use super::{client, endpoints, PROVIDER_ID};
use crate::responses::api as responses;
use crate::sse::{LineBuffer, SseLine};
use crate::{http, ids, logging};

const REQUEST_TIMEOUT_MS: u64 = 600_000;

fn sse(payload: &Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

fn chunk(model: &str, id: &str, delta: Value, finish: Option<&str>) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    }))
}

fn usage_chunk(model: &str, id: &str, reason: &str, input: i64, output: i64, cached: i64) -> Vec<u8> {
    sse(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
        "usage": {
            "prompt_tokens": input.max(0),
            "completion_tokens": output.max(0),
            "total_tokens": (input + output).max(0),
            "prompt_tokens_details": { "cached_tokens": cached.max(0) },
        },
    }))
}

async fn head(sink: &mut ConversationSink<'_>, status: u16) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status,
        headers: BTreeMap::from([(
            "content-type".to_string(),
            if status == 200 { "text/event-stream" } else { "application/json" }.to_string(),
        )]),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
}

/// Responses 的 SSE 事件读取器。
struct EventStream {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    lines: LineBuffer,
    pending: std::collections::VecDeque<Value>,
    ended: bool,
}

impl EventStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            lines: LineBuffer::new(),
            pending: std::collections::VecDeque::new(),
            ended: false,
        }
    }

    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            if self.ended {
                return Ok(None);
            }
            match self.stream.next().await {
                Some(Ok(bytes)) => {
                    for line in self.lines.push(&bytes) {
                        self.handle(&line);
                    }
                }
                Some(Err(error)) => {
                    return Err(format!("读取 DeepAI 的响应失败：{}", http::describe(&error)));
                }
                None => {
                    self.ended = true;
                    if let Some(line) = self.lines.finish() {
                        self.handle(&line);
                    }
                }
            }
        }
    }

    fn handle(&mut self, line: &str) {
        let parsed = SseLine::parse(line);
        let Some(payload) = parsed.data() else { return };
        if payload == "[DONE]" {
            self.ended = true;
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            self.pending.push_back(value);
        }
    }
}

/// 一次完整转发。
pub async fn conversation(
    ctx: &PluginCtx,
    request: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    match forward(ctx, &request, &mut sink).await {
        Ok(()) => Ok(()),
        Err((status, message)) => {
            logging::verbose("deepaicode", &format!("转发失败（{status}）：{message}"));
            head(&mut sink, status).await?;
            let body = json!({ "error": { "message": message, "type": "upstream_error" } }).to_string();
            sink.send(body.as_bytes()).await
        }
    }
}

async fn forward(
    ctx: &PluginCtx,
    request: &proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let stored = ctx
        .store_get(PROVIDER_ID, &request.account)
        .await
        .map_err(|error| (502, format!("读取 DeepAI 账号失败：{error}")))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.api_key.is_empty() {
        return Err((401, "DeepAI 账号缺少 API Key，请重新粘贴".to_string()));
    }
    let model = if request.wire_model.trim().is_empty() {
        request
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("glm-5.3-flash")
            .to_string()
    } else {
        request.wire_model.clone()
    };
    let responses_channel = models::is_responses_model(&model);
    let (url, body) = if responses_channel {
        (
            format!("{}{}", endpoints::BASE_ORIGIN, endpoints::RESPONSES_PATH),
            responses::build_request(&request.body, &model, true),
        )
    } else {
        (
            format!("{}{}", endpoints::BASE_ORIGIN, endpoints::CHAT_PATH),
            super::prepare_body(request.body.clone(), &model, true),
        )
    };
    let mut headers = client::chat_headers(&credentials, true);
    if let Some(affinity) = request.headers.get("x-session-affinity") {
        if !affinity.trim().is_empty() {
            headers.push((endpoints::HEADER_AFFINITY.to_string(), affinity.trim().to_string()));
        }
    }
    let client = http::stream_client(request.proxy.as_ref(), &ctx.env.proxy);
    let mut builder = client
        .post(&url)
        .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
        .json(&body);
    for (name, value) in &headers {
        // `.json()` 已设置 Content-Type，重复发送会产生两个同名头
        if name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    let response = builder.send().await.map_err(|error| {
        (502, format!("请求 DeepAI 上游失败：{}", http::describe(&error)))
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        return Err((status, client::error_message(&payload)));
    }
    let id = format!("chatcmpl-{}", ids::new_message_id());
    if responses_channel {
        translate(response, &model, &id, sink).await
    } else {
        passthrough(response, sink).await
    }
}

/// Chat 通道：原样透传上游 SSE。
async fn passthrough(
    response: reqwest::Response,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    head(sink, 200).await.map_err(|message| (502, message))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| (502, http::describe(&error)))?;
        sink.send(&chunk).await.map_err(|message| (502, message))?;
    }
    Ok(())
}

/// Responses 通道：把事件翻成 chat 帧。
async fn translate(
    response: reqwest::Response,
    model: &str,
    id: &str,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let mut stream = EventStream::new(response);
    let mut tool_index: i64 = -1;
    let mut tool_args_sent = false;
    let mut text_sent = false;
    let mut started = false;
    let mut usage = (0i64, 0i64, 0i64);
    loop {
        let event = match stream.next().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if !started {
                    return Err((502, "DeepAI 在开始回答前就结束了响应".to_string()));
                }
                sink.send(&usage_chunk(model, id, "stop", usage.0, usage.1, usage.2))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n").await.map_err(|message| (502, message))?;
                return Ok(());
            }
            Err(message) => return Err((502, message)),
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "response.created" || kind == "response.in_progress" {
            continue;
        }
        if kind == "error" || kind == "response.failed" {
            let message = event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .or_else(|| event.get("message").and_then(Value::as_str))
                .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("DeepAI 返回了一个错误")
                .to_string();
            let status = classify_message(&message);
            if !started {
                return Err((status, message));
            }
            sink.send(&sse(&json!({ "error": { "message": message, "type": "upstream_error" } })))
                .await
                .map_err(|error| (502, error))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|error| (502, error))?;
            return Ok(());
        }
        if kind == "response.completed" || kind == "response.done" || kind == "response.incomplete" {
            let (input, output, cached) = responses::usage_of(&event);
            usage = (input, output, cached);
            let reason = if kind == "response.incomplete" { "length" } else { "stop" };
            if !started {
                head(sink, 200).await.map_err(|message| (502, message))?;
                sink.send(&chunk(
                    model,
                    id,
                    json!({ "role": "assistant", "content": "" }),
                    None,
                ))
                .await
                .map_err(|message| (502, message))?;
            }
            sink.send(&usage_chunk(model, id, reason, usage.0, usage.1, usage.2))
                .await
                .map_err(|message| (502, message))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|message| (502, message))?;
            return Ok(());
        }
        if !started {
            head(sink, 200).await.map_err(|message| (502, message))?;
            sink.send(&chunk(
                model,
                id,
                json!({ "role": "assistant", "content": "" }),
                None,
            ))
            .await
            .map_err(|message| (502, message))?;
            started = true;
        }
        match kind {
            "response.output_item.added" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    tool_index += 1;
                    tool_args_sent = false;
                    sink.send(&chunk(
                        model,
                        id,
                        json!({
                            "tool_calls": [{
                                "index": tool_index,
                                "id": call_id,
                                "type": "function",
                                "function": { "name": name, "arguments": "" },
                            }]
                        }),
                        None,
                    ))
                    .await
                    .map_err(|message| (502, message))?;
                }
            }
            "response.output_text.delta" | "response.text.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text_sent = true;
                        sink.send(&chunk(model, id, json!({ "content": text }), None))
                            .await
                            .map_err(|message| (502, message))?;
                    }
                }
            }
            "response.output_text.done" | "response.text.done" => {
                if !text_sent {
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text_sent = true;
                            sink.send(&chunk(model, id, json!({ "content": text }), None))
                                .await
                                .map_err(|message| (502, message))?;
                        }
                    }
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning.delta" => {
                if let Some(text) = event.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        sink.send(&chunk(model, id, json!({ "reasoning_content": text }), None))
                            .await
                            .map_err(|message| (502, message))?;
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(args) = event.get("delta").and_then(Value::as_str) {
                    if !args.is_empty() && tool_index >= 0 {
                        tool_args_sent = true;
                        sink.send(&chunk(
                            model,
                            id,
                            json!({
                                "tool_calls": [{ "index": tool_index, "function": { "arguments": args } }]
                            }),
                            None,
                        ))
                        .await
                        .map_err(|message| (502, message))?;
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if !tool_args_sent && tool_index >= 0 {
                    if let Some(args) = event.get("arguments").and_then(Value::as_str) {
                        if !args.is_empty() {
                            tool_args_sent = true;
                            sink.send(&chunk(
                                model,
                                id,
                                json!({
                                    "tool_calls": [{ "index": tool_index, "function": { "arguments": args } }]
                                }),
                                None,
                            ))
                            .await
                            .map_err(|message| (502, message))?;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// 错误文案 → HTTP 状态（尽力而为）。
fn classify_message(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("quota")
        || lower.contains("balance")
        || lower.contains("credit")
    {
        return 429;
    }
    if lower.contains("context")
        || lower.contains("too long")
        || lower.contains("maximum length")
        || lower.contains("too large")
    {
        return 400;
    }
    502
}
