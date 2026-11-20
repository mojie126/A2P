//! 科搭令牌续期：48 小时提前窗口、5 秒去抖、账号名核对与比较写回。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::credentials as jwt;
use super::endpoints;
use super::{client, PROVIDER_ID};
use crate::errors::GatewayError;

/// 上游拒绝续期的「还没到可续期窗口」：`400 Token is not close to expiration`。
///
/// 这不是故障 —— 令牌没到期，只是续得太早（本地 48 小时窗口比上游允许的窗口宽）。
/// 调用方按「令牌仍有效、无需续期」处理，不再把这条记成续期失败。
/// 上游告知「未到续期窗口」后，这段时间内不再请求续期（令牌仍在有效期内）。
const DEFER_MS: i64 = 30 * 60 * 1000;

fn not_close_to_expiration(error: &GatewayError) -> bool {
    error.status_code == 400 && {
        let text = error.message.to_ascii_lowercase();
        text.contains("not close to expiration") || text.contains("not close to expire")
    }
}

/// 取一份可用凭证：临期时续期并写回。
pub async fn ensure(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<Credentials, GatewayError> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| GatewayError::with_status(500, format!("读取科搭账号失败：{error}")))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.access_token.is_empty() && !credentials.is_desktop() {
        return Err(GatewayError::with_status(401, "科搭账号缺少访问令牌，请重新粘贴"));
    }
    // 本机关联：续期就是重读官方客户端当前登录令牌，不调用上游续期接口
    if credentials.is_desktop() {
        let login = super::desktop::read()
            .map_err(|message| GatewayError::with_status(401, message))?;
        if !credentials.username.is_empty() && login.username != credentials.username {
            return Err(GatewayError::with_status(
                409,
                "科搭客户端已切换到其他账号，请重新导入本机登录态",
            ));
        }
        if jwt::expires(&login.token).is_some_and(|expires| expires <= crate::http::now_ms()) {
            return Err(GatewayError::with_status(
                401,
                "科搭客户端登录已过期，请先在官方客户端重新登录",
            ));
        }
        if login.token == credentials.access_token && !force {
            return Ok(credentials);
        }
        let mut secret = stored.secret.clone();
        secret["accessToken"] = json!(login.token);
        secret["expiresAt"] = jwt::expires(&login.token).map_or(Value::Null, |value| json!(value));
        secret["refreshedAt"] = json!(crate::http::now_ms());
        if let Err(error) = ctx.store_update(&stored.id, &stored.secret, &secret).await {
            return Err(GatewayError::with_status(
                502,
                format!("科搭读取本机登录态后保存失败：{error}"),
            ));
        }
        return Ok(Credentials::from_secret(&secret));
    }
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    let recent = stored.secret.get("refreshedAt").and_then(Value::as_i64).unwrap_or(0);
    if recent > 0 && crate::http::now_ms().saturating_sub(recent) < endpoints::REFRESH_DEBOUNCE_MS {
        return Ok(credentials);
    }
    // 上一次上游已明确「未到可续期窗口」：窗口期内令牌不会到期，不再重复打上游，
    // 只推进 refreshedAt 让网关的凭证维护看到一次检查（否则每轮维护都白打一次）。
    let deferred = stored.secret.get("refreshDeferredAt").and_then(Value::as_i64).unwrap_or(0);
    if deferred > crate::http::now_ms() && !credentials.expiring() {
        let mut secret = stored.secret.clone();
        secret["refreshedAt"] = json!(crate::http::now_ms());
        if let Err(error) = ctx.store_update(&stored.id, &stored.secret, &secret).await {
            return Err(GatewayError::with_status(502, format!("科搭续期状态保存失败：{error}")));
        }
        return Ok(Credentials::from_secret(&secret));
    }
    let proxy = client::account_proxy(ctx, account).await;
    let candidate = match client::refresh(&credentials.access_token, proxy.as_ref(), &ctx.env.proxy).await {
        Ok(candidate) => candidate,
        // 上游只在临到期时才受理续期，令牌此时仍然有效：按「已检查、无需续期」处理，
        // 写回 refreshedAt 让网关的凭证维护看到一次推进，而不是记成续期失败。
        Err(error) if not_close_to_expiration(&error) => {
            let mut secret = stored.secret.clone();
            let now = crate::http::now_ms();
            secret["refreshedAt"] = json!(now);
            secret["refreshDeferredAt"] = json!(now + DEFER_MS);
            if let Err(store_error) = ctx.store_update(&stored.id, &stored.secret, &secret).await {
                return Err(GatewayError::with_status(
                    502,
                    format!("科搭续期状态保存失败：{store_error}"),
                ));
            }
            return Ok(Credentials::from_secret(&secret));
        }
        Err(error) => return Err(error),
    };
    let token = jwt::token(&candidate)
        .map_err(|_| GatewayError::with_status(502, "科搭续期没有返回有效令牌"))?;
    if jwt::expires(&token).is_some_and(|expires| expires <= crate::http::now_ms()) {
        return Err(GatewayError::with_status(401, "科搭续期返回的令牌已经过期，请重新登录"));
    }
    let claim_user = jwt::claim_username(&token);
    if !claim_user.is_empty()
        && !credentials.username.is_empty()
        && claim_user != credentials.username
    {
        return Err(GatewayError::with_status(409, "科搭续期返回了不同账号，旧令牌未被覆盖"));
    }
    let mut secret = stored.secret.clone();
    secret["accessToken"] = json!(token);
    secret["expiresAt"] = jwt::expires(&token).map_or(Value::Null, |value| json!(value));
    secret["refreshedAt"] = json!(crate::http::now_ms());
    if credentials.username.is_empty() && !claim_user.is_empty() {
        secret["username"] = json!(claim_user);
    }
    if let Err(error) = ctx.store_update(&stored.id, &stored.secret, &secret).await {
        return Err(GatewayError::with_status(
            502,
            format!("科搭续期后保存凭证失败：{error}"),
        ));
    }
    Ok(Credentials::from_secret(&secret))
}
