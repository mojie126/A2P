//! DeepAI Code 插件实现。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! DeepAI 的编程模型服务 `api.deepai.org/deepai_code`：OpenAI Chat 兼容的
//! 对话端点，鉴权走 `Api-Key` 头，按 `(机器指纹, 账号)` 每天发放免费 Theos，
//! 付费模型按请求扣 Theos。
//!
//! ── 能力 ────────────────────────────────────────────────────
//! ```text
//!   有：对话（流式 / 工具 / 图片输入，Chat 通道）、动态模型目录、每日额度、
//!       签到（调用 /theos 触发）、粘贴 API Key 与本机桌面端凭据导入
//!   无：Responses 通道（gpt-6-astra 等）、联网搜索、语音转写、云端会话
//! ```
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考实现 `2951461586/DeepAICode2API`（MIT）与 `aqiu817/magpie-deepaicode`
//! （MIT，附 PROTOCOL.md 实测记录）：端点、请求头、错误语义与签到机制均取自
//! 两者，详见各子模块的模块头。

pub mod accounts;
pub mod checkin;
pub mod client;
pub mod endpoints;
pub mod forward;
pub mod login;
pub mod models;
pub mod search;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = endpoints::PROVIDER_ID;

/// 补齐首条 system 消息时使用的兜底提示词。
///
/// 上游自 2026-11-07 起校验「请求必须像桌面端发出的」：`messages` 首条不是
/// `system` / `developer` 时回 400 `This request is not one DeepAI Code sends.`
/// （内容不检查）。缺失时补一条最简提示词，已有内容不改写。
const DEFAULT_SYSTEM: &str = "You are a helpful AI assistant.";

/// DeepAI Code 适配器。
pub struct DeepAiCodeProvider;

impl Provider for DeepAiCodeProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, endpoints::DISPLAY_NAME)
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: true,
                // API Key 不会过期，也没有续期接口
                refresh: false,
                // 目录接口要求 Api-Key，账号可用时联网刷新
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: false,
                supports_default_model: true,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Token {
                    label: "粘贴 DeepAI API Key（可在 deepai.org 账户页获取；填 auto 读取本机桌面端登录态）".into(),
                    placeholder: "API Key（UUID 形态）或 auto".into(),
                    extra_fields: Vec::new(),
                },
                proto::LoginMethod::Desktop {
                    button: "读取本机 DeepAI Code 登录态".into(),
                    hint: Some(
                        "从 ~/.deepai-code/userdata/secrets/deepai-account.bin 读取（明文 JSON）".into(),
                    ),
                },
                proto::LoginMethod::Browser {
                    button: "打开浏览器登录 DeepAI".into(),
                    hint: Some(
                        "在浏览器里完成 DeepAI 登录，插件在随机本机端口等待回调并自动换取 API Key".into(),
                    ),
                },
            ])
            .with_models(models::fallback())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: false,
                credit_expiry: false,
                usage_detail: false,
                edition: false,
                identifier: vec!["name".into()],
                expiry: Vec::new(),
                long_lived: false,
                claim: false,
                plan_channel: false,
                email_as_name: false,
                concurrency_default: None,
                default_limiters: None,
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "DeepAI Code 每日免费 Theos：调用 /theos 触发并锁定当日额度；\
                     上游对当日重复签到有限流，命中时停止重试"
                        .into(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            });
        meta.order = 50;
        meta
    }

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { accounts::normalize(ctx, input).await })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::public(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity(secret))
    }

    /// 管理动作：联网搜索（只读，不创建对话）。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(login::login_methods()),
                crate::native_search::CAPABILITIES | crate::native_search::SEARCH => {
                    search::dispatch(ctx, account, action, payload).await
                }
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "DeepAI Code 不支持管理动作「{other}」"
                ))),
            }
        })
    }

    // ── 登录（粘贴 / 本机凭据 / 浏览器回调）────────────────────

    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            let index = payload.get("methodIndex").and_then(Value::as_u64);
            // 登录对话框里选的出网代理（登录阶段还没有账号记录）
            let proxy = payload
                .get("proxy")
                .filter(|value| !value.is_null())
                .and_then(|value| serde_json::from_value::<proto::WireProxy>(value.clone()).ok());
            // 3 = 读取本机 DeepAI Code 登录态，点一下直接完成
            if index == Some(3) {
                return login::start_desktop_flow(ctx, proxy.as_ref()).await;
            }
            // 兼容按 methodIndex 选择的旧入口：1 = 浏览器登录
            let browser = method == "browser" || method.is_empty();
            let by_index = index == Some(1);
            if !browser && !by_index {
                return Err("DeepAI 的浏览器登录请使用「打开浏览器登录」方式".to_string());
            }
            login::start_browser_flow(ctx, proxy.as_ref()).await
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { Ok(login::poll(flow)) })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
        })
    }

    /// 服务端部署时用户手动回填回调地址里的 `code`。
    fn login_callback<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            let code = extract_code(&callback.code, &callback.url);
            if code.is_empty() {
                return Ok(proto::LoginPoll::Failed {
                    message: "回调地址里没有找到 code".to_string(),
                });
            }
            Ok(login::complete_with_code(&callback.flow, &code, &ctx.env.proxy).await)
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let (credentials, proxy) = account_context(ctx, account).await;
            let list = models::list(credentials.as_ref(), proxy.as_ref(), &ctx.env.proxy).await;
            Ok(proto::ModelList {
                models: list,
                meta: models::meta(),
            })
        })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let (credentials, proxy) = account_context(ctx, account).await;
            Ok(models::refresh(credentials.as_ref(), proxy.as_ref(), &ctx.env.proxy).await)
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move { usage::query(ctx, account).await })
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move { checkin::claim(ctx, account).await })
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let stored = ctx
                .store_get(PROVIDER_ID, account)
                .await
                .map_err(|error| format!("读取 DeepAI 账号失败：{error}"))?;
            let credentials = accounts::Credentials::from_secret(&stored.secret);
            if credentials.api_key.is_empty() {
                return Err("DeepAI 账号缺少 API Key，请重新粘贴".to_string());
            }
            Ok(credentials.api_key)
        })
    }

    fn refresh_token<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            Err("DeepAI API Key 不能自动续期，请在 deepai.org 重新获取后粘贴".to_string())
        })
    }

    fn token_expiring<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move { Ok(false) })
    }

    // ── 转发 ──────────────────────────────────────────────────

    /// 会话式转发：Chat 通道原样透传，Responses 通道翻译成 chat SSE。
    ///
    /// 目录里 `api = openai-responses` 的模型只在 `/responses` 上可用，
    /// 而网关对外只讲 Chat，因此统一走 `conversation`。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move { forward::conversation(ctx, req, sink).await })
    }

    /// 上游失败分类。
    ///
    /// `409 session_busy` 是容量拒绝（转 429 换号）；`403 theos_exhausted`
    /// 是当日额度用完（冷却到 `refresh_at`）；`426` 是车道版本不受理；
    /// 其余 `403`（邮箱未验证、需社交登录、缺机器指纹）是账号终态。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let code = client::error_code(body);
        let message = client::error_message(body);
        let text = if message.is_empty() {
            format!("DeepAI 上游返回 HTTP {status}")
        } else {
            format!("DeepAI 上游返回 HTTP {status}：{message}")
        };
        match code.as_str() {
            endpoints::CODE_SESSION_BUSY => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message: text,
                upstream_code: None,
                status: 429,
            },
            endpoints::CODE_THEOS_EXHAUSTED => proto::ErrorClass::QuotaLimited {
                reset_at: parse_time_ms(body.get("refresh_at")),
                message: text,
                upstream_code: None,
                status: 403,
            },
            endpoints::CODE_LANE_UNSUPPORTED => proto::ErrorClass::Fatal {
                status: 426,
                message: text,
                upstream_code: None,
            },
            endpoints::CODE_EMAIL_UNVERIFIED
            | endpoints::CODE_SOCIAL_LOGIN
            | endpoints::CODE_MACHINE_REQUIRED => proto::ErrorClass::Fatal {
                status,
                message: text,
                upstream_code: None,
            },
            _ => match status {
                401 => proto::ErrorClass::TokenExpired { message: text },
                429 => proto::ErrorClass::QuotaLimited {
                    reset_at: None,
                    message: text,
                    upstream_code: None,
                    status,
                },
                409 => proto::ErrorClass::QuotaLimited {
                    reset_at: None,
                    message: text,
                    upstream_code: None,
                    status: 429,
                },
                _ => proto::ErrorClass::Fatal {
                    status,
                    message: text,
                    upstream_code: None,
                },
            },
        }
    }
}

/// 取账号凭证与出网代理（账号为空时返回 `None`）。
async fn account_context(
    ctx: &PluginCtx,
    account: &str,
) -> (Option<accounts::Credentials>, Option<proto::WireProxy>) {
    if account.trim().is_empty() {
        return (None, None);
    }
    let proxy = client::account_proxy(ctx, account).await;
    let credentials = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .ok()
        .map(|stored| accounts::Credentials::from_secret(&stored.secret))
        .filter(|credentials| !credentials.api_key.is_empty());
    (credentials, proxy)
}

/// 出站请求体：改写模型名、补齐首条 system/developer，流式时请求 usage。
fn prepare_body(mut body: Value, wire_model: &str, stream: bool) -> Value {
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    object.insert("model".into(), Value::String(wire_model.to_string()));
    if stream {
        object.insert("stream".into(), Value::Bool(true));
        let options = object
            .entry("stream_options".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !options.is_object() {
            *options = Value::Object(serde_json::Map::new());
        }
        options["include_usage"] = Value::Bool(true);
    }
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        let leading_system = messages
            .first()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            .is_some_and(|role| matches!(role, "system" | "developer"));
        if !leading_system {
            messages.insert(
                0,
                json!({ "role": "system", "content": DEFAULT_SYSTEM }),
            );
        }
    }
    body
}

/// ISO 时间 → 毫秒时间戳（解析失败返回 `None`）。
fn parse_time_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?.trim();
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

/// 从用户回填的 code 或回调地址里取一次性授权码。
fn extract_code(code: &str, url: &str) -> String {
    let code = code.trim();
    if !code.is_empty() {
        return code.to_string();
    }
    url::Url::parse(url.trim())
        .ok()
        .and_then(|parsed| {
            parsed
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.trim().to_string())
        })
        .filter(|code| !code.is_empty() && code.len() <= 4096)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_body_inserts_leading_system() {
        let body = prepare_body(
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            "glm-5.3-flash",
            false,
        );
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn prepare_body_keeps_existing_system() {
        let body = prepare_body(
            json!({ "messages": [
                { "role": "system", "content": "custom" },
                { "role": "user", "content": "hi" }
            ]}),
            "glm-5.3-flash",
            false,
        );
        assert_eq!(body["messages"][0]["content"], "custom");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn prepare_body_injects_usage_for_stream() {
        let body = prepare_body(
            json!({ "messages": [{ "role": "system", "content": "s" }], "stream": true }),
            "glm-5.3-flash",
            true,
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
}
