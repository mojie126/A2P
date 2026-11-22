//! 科搭的会话式转发：Chat 原样透传，Anthropic 与 Responses 翻译成 chat SSE。
//!
//! ── 为什么走 `conversation` ────────────────────────────────
//! 目录里的 Claude 模型只支持 Anthropic Messages，Responses 模型只支持
//! `/v1/responses`，网关对外只讲 Chat。插件按目录声明的协议构造请求并把
//! 上游事件翻成 chat 帧。

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};

use super::{client, endpoints, models, refresh};
use crate::responses::api as responses;
use crate::sse::{LineBuffer, SseLine};
use crate::{http, ids, logging};

const TIMEOUT_MS: u64 = 600_000;

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

fn usage_chunk(model: &str, id: &str, reason: &str, input: i64, output: i64) -> Vec<u8> {
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

/// 通用 JSON SSE 读取器（一行一个 `data:` JSON）。
struct JsonSse {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    lines: LineBuffer,
    pending: std::collections::VecDeque<Value>,
    ended: bool,
}

impl JsonSse {
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
                    return Err(format!("读取科搭响应失败：{}", http::describe(&error)));
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

pub async fn conversation(
    ctx: &PluginCtx,
    request: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    match forward(ctx, &request, &mut sink).await {
        Ok(()) => Ok(()),
        Err((status, message)) => {
            logging::verbose("codar", &format!("转发失败（{status}）：{message}"));
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
    let credentials = refresh::ensure(ctx, &request.account, false)
        .await
        .map_err(|error| (error.status_code as u16, error.message))?;
    let model = if request.wire_model.trim().is_empty() {
        request
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string()
    } else {
        request.wire_model.clone()
    };
    let protocol = models::protocol_of_model(&model);
    // 业务接口优先使用 API Key：登录凭据被上游限制为科搭官方客户端
    let token = credentials.business_token().to_string();
    let (url, body, headers) = match protocol.as_str() {
        "anthropic" => {
            let mut body = crate::anthropic::anthropic_outbound::anthropic_request_from_chat(&request.body, &model)
                .map_err(|message| (400, message))?;
            body["stream"] = json!(true);
            let mut headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
                ("User-Agent".to_string(), endpoints::CHAT_USER_AGENT.to_string()),
                (
                    "anthropic-version".to_string(),
                    request
                        .body
                        .pointer("/protocolHeaders/anthropic-version")
                        .and_then(Value::as_str)
                        .unwrap_or("2023-06-01")
                        .to_string(),
                ),
            ];
            // 上游 anthropic 端点（/v1/messages）对 `x-api-key` 头回 client 门禁
            // 403，`Authorization: Bearer` 同值放行（2026-11-22 按模型分头对照确认）——
            // 鉴权统一走 Bearer，不再按目录的 authentication 标注分头型
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
            (
                format!("{}{}", endpoints::API_BASE, endpoints::ANTHROPIC),
                body,
                headers,
            )
        }
        "responses" => {
            let body = responses::build_request(&request.body, &model, true);
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
                ("User-Agent".to_string(), endpoints::CHAT_USER_AGENT.to_string()),
                ("Authorization".to_string(), format!("Bearer {token}")),
            ];
            (
                format!("{}{}", endpoints::API_BASE, endpoints::RESPONSES),
                body,
                headers,
            )
        }
        _ => {
            let body = super::prepare_body(request.body.clone(), &model, true);
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
                ("User-Agent".to_string(), endpoints::CHAT_USER_AGENT.to_string()),
                ("Authorization".to_string(), format!("Bearer {token}")),
            ];
            (
                format!("{}{}", endpoints::API_BASE, endpoints::CHAT),
                body,
                headers,
            )
        }
    };
    let client = http::stream_client(request.proxy.as_ref(), &ctx.env.proxy);
    let mut builder = client
        .post(&url)
        .timeout(std::time::Duration::from_millis(TIMEOUT_MS))
        .json(&body);
    for (name, value) in &headers {
        // `.json()` 已设置 Content-Type，重复发送会产生两个同名头
        if name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    let response = builder
        .send()
        .await
        .map_err(|error| (502, format!("请求科搭上游失败：{}", http::describe(&error))))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        return Err((status, client::with_credential_hint(client::message(&payload))));
    }
    let id = format!("chatcmpl-{}", ids::new_message_id());
    match protocol.as_str() {
        "anthropic" => translate_anthropic(response, &model, &id, sink).await,
        "responses" => translate_responses(response, &model, &id, sink).await,
        _ => passthrough(response, sink).await,
    }
}

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

async fn translate_anthropic(
    response: reqwest::Response,
    model: &str,
    id: &str,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let mut stream = JsonSse::new(response);
    let mut started = false;
    let mut input_tokens = 0i64;
    let mut output_tokens = 0i64;
    let mut tool_index: i64 = -1;
    let mut blocks: HashMap<i64, i64> = HashMap::new();
    loop {
        let event = match stream.next().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if !started {
                    return Err((502, "科搭在开始回答前就结束了响应".to_string()));
                }
                sink.send(&usage_chunk(model, id, "stop", input_tokens, output_tokens))
                    .await
                    .map_err(|message| (502, message))?;
                sink.send(b"data: [DONE]\n\n").await.map_err(|message| (502, message))?;
                return Ok(());
            }
            Err(message) => return Err((502, message)),
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "error" {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("科搭返回了一个错误")
                .to_string();
            if !started {
                return Err((classify_message(&message), message));
            }
            sink.send(&sse(&json!({ "error": { "message": message, "type": "upstream_error" } })))
                .await
                .map_err(|error| (502, error))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|error| (502, error))?;
            return Ok(());
        }
        if kind == "message_start" {
            input_tokens = event
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            continue;
        }
        if kind == "message_delta" {
            output_tokens = event
                .pointer("/usage/output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(output_tokens);
            continue;
        }
        if kind == "message_stop" {
            if !started {
                head(sink, 200).await.map_err(|message| (502, message))?;
                sink.send(&chunk(model, id, json!({ "role": "assistant", "content": "" }), None))
                    .await
                    .map_err(|message| (502, message))?;
            }
            sink.send(&usage_chunk(model, id, "stop", input_tokens, output_tokens))
                .await
                .map_err(|message| (502, message))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|message| (502, message))?;
            return Ok(());
        }
        if !started {
            head(sink, 200).await.map_err(|message| (502, message))?;
            sink.send(&chunk(model, id, json!({ "role": "assistant", "content": "" }), None))
                .await
                .map_err(|message| (502, message))?;
            started = true;
        }
        match kind {
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(-1);
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    tool_index += 1;
                    blocks.insert(index, tool_index);
                    let call_id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
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
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(-1);
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                sink.send(&chunk(model, id, json!({ "content": text }), None))
                                    .await
                                    .map_err(|message| (502, message))?;
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            if !text.is_empty() {
                                sink.send(&chunk(model, id, json!({ "reasoning_content": text }), None))
                                    .await
                                    .map_err(|message| (502, message))?;
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(args) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some(tool) = blocks.get(&index).copied() {
                                if !args.is_empty() {
                                    sink.send(&chunk(
                                        model,
                                        id,
                                        json!({
                                            "tool_calls": [{ "index": tool, "function": { "arguments": args } }]
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
            _ => {}
        }
    }
}

async fn translate_responses(
    response: reqwest::Response,
    model: &str,
    id: &str,
    sink: &mut ConversationSink<'_>,
) -> Result<(), (u16, String)> {
    let mut stream = JsonSse::new(response);
    let mut started = false;
    let mut text_sent = false;
    let mut tool_index: i64 = -1;
    let mut tool_args_sent = false;
    loop {
        let event = match stream.next().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if !started {
                    return Err((502, "科搭在开始回答前就结束了响应".to_string()));
                }
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
                .unwrap_or("科搭返回了一个错误")
                .to_string();
            if !started {
                return Err((classify_message(&message), message));
            }
            sink.send(&sse(&json!({ "error": { "message": message, "type": "upstream_error" } })))
                .await
                .map_err(|error| (502, error))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|error| (502, error))?;
            return Ok(());
        }
        if kind == "response.completed" || kind == "response.done" || kind == "response.incomplete" {
            let (input, output, _) = responses::usage_of(&event);
            let reason = if kind == "response.incomplete" { "length" } else { "stop" };
            if !started {
                head(sink, 200).await.map_err(|message| (502, message))?;
                sink.send(&chunk(model, id, json!({ "role": "assistant", "content": "" }), None))
                    .await
                    .map_err(|message| (502, message))?;
            }
            sink.send(&usage_chunk(model, id, reason, input, output))
                .await
                .map_err(|message| (502, message))?;
            sink.send(b"data: [DONE]\n\n").await.map_err(|message| (502, message))?;
            return Ok(());
        }
        if !started {
            head(sink, 200).await.map_err(|message| (502, message))?;
            sink.send(&chunk(model, id, json!({ "role": "assistant", "content": "" }), None))
                .await
                .map_err(|message| (502, message))?;
            started = true;
        }
        match kind {
            "response.output_item.added" => {
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    tool_index += 1;
                    tool_args_sent = false;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("id").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
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
                            json!({ "tool_calls": [{ "index": tool_index, "function": { "arguments": args } }] }),
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
                                json!({ "tool_calls": [{ "index": tool_index, "function": { "arguments": args } }] }),
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

fn classify_message(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("quota")
        || lower.contains("balance")
        || lower.contains("credit")
        || lower.contains("余额")
        || lower.contains("积分")
    {
        return 429;
    }
    502
}
