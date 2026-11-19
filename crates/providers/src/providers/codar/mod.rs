//! 科搭（Codar）插件实现。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! 科搭（Codar / SpeedAI）的编程模型聚合服务：业务接口
//! `api.claw.speedai.com`，账号、积分与签到接口 `api.kuaipaper.com`，
//! 鉴权统一 `Authorization: Bearer <JWT>`。
//!
//! ── 能力 ────────────────────────────────────────────────────
//! ```text
//!   有：对话（OpenAI Chat 兼容，含工具与图片输入）、动态模型目录、
//!       令牌续期（48 小时提前窗口）、点数查询、每日签到、粘贴令牌导入
//!   无：微信扫码 / 手机验证码登录、本机关联、Anthropic 与 Responses 协议、
//!       文生图、活动领取
//! ```
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考官方独立插件 `wasm/codar`（MIT）：端点、请求头与响应信封取自其实现，
//! 详见各子模块的模块头。

pub mod accounts;
pub mod activities;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod desktop;
pub mod endpoints;
pub mod forward;
pub mod login;
pub mod models;
pub mod refresh;
pub mod usage;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = endpoints::PROVIDER_ID;

/// 科搭 Code 适配器。
pub struct CodarProvider;

impl Provider for CodarProvider {
    fn meta(&self) -> ProviderMeta {
        let mut meta = ProviderMeta::new(PROVIDER_ID, endpoints::DISPLAY_NAME)
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: true,
                refresh: true,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: true,
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Token {
                    label: "粘贴科搭访问令牌（JWT，或包含 accessToken 的凭据 JSON）".into(),
                    placeholder: "eyJ… 或 {\"accessToken\":\"…\"}".into(),
                    extra_fields: Vec::new(),
                },
                proto::LoginMethod::Desktop {
                    button: "读取本机科搭客户端登录态".into(),
                    hint: Some("从 ~/.codar/config.json 读取当前登录令牌（只读，不写回）".into()),
                },
                proto::LoginMethod::Browser {
                    button: "微信扫码登录科搭".into(),
                    hint: Some("插件打开二维码地址，用微信扫码并在手机上确认".into()),
                },
            ])
            .with_models(models::fallback())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                credit_expiry: true,
                usage_detail: false,
                edition: false,
                identifier: vec!["name".into()],
                expiry: vec!["expiresAt".into()],
                long_lived: false,
                claim: false,
                plan_channel: false,
                email_as_name: false,
                concurrency_default: None,
                default_limiters: None,
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "科搭每日签到：先查状态，未签再提交，提交后回查确认；按滚动 24 小时窗口发放"
                        .into(),
                ),
                modes: vec![proto::CheckinMode::Checkin],
                ..Default::default()
            });
        meta.order = 51;
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

    /// 管理动作：登录方式、短信验证码发送与活动查询/领取。
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
                "auth.sms.send" => sms_send(ctx, payload).await,
                "activities.preview" => activities::preview(ctx, account).await,
                "activities.claim" => {
                    let id = payload.get("campaignId").and_then(Value::as_str).unwrap_or("");
                    activities::claim(ctx, account, id).await
                }
                "activities.mine" => activities::mine(ctx, account).await,
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "科搭不支持管理动作「{other}」"
                ))),
            }
        })
    }

    // ── 登录（扫码 / 粘贴 / 本机关联）──────────────────────────

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
            // 2 = 读取本机科搭客户端登录态，点一下直接完成
            if index == Some(2) {
                return login::start_desktop_flow(ctx, proxy.as_ref()).await;
            }
            if method == "browser" || index == Some(0) {
                let name = payload
                    .get("inputs")
                    .and_then(|inputs| inputs.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                login::start_browser_flow(ctx, name, proxy.as_ref()).await
            } else {
                Err("科搭的浏览器登录请使用「微信扫码登录」方式；本机关联与粘贴令牌不需要发起登录流程".to_string())
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { Ok(login::poll(flow).await) })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::cancel(flow);
            Ok(())
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let token = stored_token(ctx, account).await;
            let proxy = client::account_proxy(ctx, account).await;
            let list = models::list(token.as_deref(), proxy.as_ref(), &ctx.env.proxy).await;
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
            let token = stored_token(ctx, account).await;
            let proxy = client::account_proxy(ctx, account).await;
            Ok(models::refresh(token.as_deref(), proxy.as_ref(), &ctx.env.proxy).await)
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
            let credentials = refresh::ensure(ctx, account, false)
                .await
                .map_err(|error| error.message)?;
            Ok(credentials.access_token)
        })
    }

    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = refresh::ensure(ctx, account, true)
                .await
                .map_err(|error| error.message)?;
            Ok(credentials.access_token)
        })
    }

    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let stored = match ctx.store_get(PROVIDER_ID, account).await {
                Ok(stored) => stored,
                Err(_) => return Ok(false),
            };
            Ok(accounts::Credentials::from_secret(&stored.secret).expiring())
        })
    }

    // ── 转发 ──────────────────────────────────────────────────

    /// 会话式转发：Chat 原样透传，Anthropic 与 Responses 翻译成 chat SSE。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move { forward::conversation(ctx, req, sink).await })
    }

    /// 上游失败分类：凭证失效、点数/并发限额、内容审核与其它。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = format!(
            "科搭上游返回 {status}：{}",
            client::message(body)
        );
        match client::kind(status, body) {
            "token_expired" => proto::ErrorClass::TokenExpired { message },
            "quota_limited" => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status: 429,
            },
            "content_blocked" => proto::ErrorClass::ContentBlocked {
                status: 403,
                message,
                upstream_code: None,
            },
            _ => proto::ErrorClass::Fatal {
                status,
                message,
                upstream_code: None,
            },
        }
    }
}

/// 取账号记录的访问令牌（不触发续期，用于目录查询）。
async fn stored_token(ctx: &PluginCtx, account: &str) -> Option<String> {
    if account.trim().is_empty() {
        return None;
    }
    let stored = ctx.store_get(PROVIDER_ID, account).await.ok()?;
    let token = accounts::Credentials::from_secret(&stored.secret).access_token;
    (!token.is_empty()).then_some(token)
}

/// 发送登录短信验证码。
///
/// 上游要求阿里云人机验证的四元组（`lot_number` / `captcha_output` /
/// `pass_token` / `gen_time`）；本动作原样转发，不自行破解验证码。
/// 验证码发送后，用 `sms:手机号:验证码` 作为令牌导入即可完成登录。
async fn sms_send(ctx: &PluginCtx, payload: &Value) -> Result<Value, String> {
    let phone = payload
        .get("phone")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let valid_phone = phone.len() == 11
        && phone.starts_with('1')
        && phone.as_bytes().get(1).is_some_and(|digit| (b'3'..=b'9').contains(digit))
        && phone.bytes().all(|byte| byte.is_ascii_digit());
    if !valid_phone {
        return Err("请填写 11 位中国大陆手机号".to_string());
    }
    let captcha = payload.get("captcha").filter(|value| value.is_object()).ok_or_else(|| {
        "请先完成人机验证，并提供 lot_number、captcha_output、pass_token、gen_time".to_string()
    })?;
    let mut body = json!({});
    for key in ["lot_number", "captcha_output", "pass_token", "gen_time"] {
        let value = captcha
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 8192)
            .ok_or_else(|| format!("人机验证缺少 {key}"))?;
        body[key] = json!(value);
    }
    body["phone"] = json!(phone);
    body["name"] = json!("user");
    body["type"] = json!("login");
    let (status, reply) = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::SMS,
        None,
        Some(&body),
        None,
        &ctx.env.proxy,
        12_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&status) || client::failed(&reply) {
        return Err(client::upstream_error(status, &reply).message);
    }
    Ok(json!({
        "sent": true,
        "message": "验证码已发送；用 sms:手机号:验证码 作为访问令牌导入即可完成登录",
    }))
}

/// 出站请求体：改写模型名，流式时请求 usage。
fn prepare_body(mut body: Value, wire_model: &str, stream: bool) -> Value {
    let Some(object) = body.as_object_mut() else {
        return body;
    };
    object.insert("model".into(), Value::String(wire_model.to_string()));
    object.insert("stream".into(), Value::Bool(stream));
    if stream {
        let options = object
            .entry("stream_options".to_string())
            .or_insert_with(|| json!({}));
        if !options.is_object() {
            *options = json!({});
        }
        options["include_usage"] = Value::Bool(true);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_body_sets_model_and_stream() {
        let body = prepare_body(json!({ "messages": [] }), "auto", true);
        assert_eq!(body["model"], "auto");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn prepare_body_non_stream_has_no_usage_options() {
        let body = prepare_body(json!({ "messages": [] }), "glm-5.3", false);
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }
}
