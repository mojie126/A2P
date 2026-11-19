//! 科搭令牌续期：48 小时提前窗口、5 秒去抖、账号名核对与比较写回。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::Credentials;
use super::credentials as jwt;
use super::endpoints;
use super::{client, PROVIDER_ID};
use crate::errors::GatewayError;

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
    let candidate = {
        let proxy = client::account_proxy(ctx, account).await;
        client::refresh(&credentials.access_token, proxy.as_ref(), &ctx.env.proxy).await?
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
