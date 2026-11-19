//! 共享 OpenAI Responses 转换。
//!
//! 实现体取自 `providers/astudio/responses.rs`（Chat 请求 → Responses 请求、
//! Responses 事件的用量与片段读取），供多个使用 Responses 通道的提供商复用。

#[path = "providers/astudio/responses.rs"]
pub mod api;
