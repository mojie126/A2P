//! 小浣熊的凭证处理（Agent2API 改造 W3-T4；移植来源 `raccoon-local-auth.mjs`）。
//!
//! ── 本文件负责什么 ──────────────────────────────────────────
//!   1. **凭证来源**：账号记录里的 token；或桌面端**实时登录态**
//!      （`~/.box-agent/config/auth.json`，小浣熊生态共享文件，客户端重新登录后
//!      下次请求即生效）。桌面端文件带 mtime 缓存，避免每个请求都读盘。
//!   2. **登录态重读**：上游已无旧版刷新接口（IAM 迁移后 `/refresh` 不再发放
//!      刷新令牌，残留的刷新令牌一律作废），`refresh` 只做「重读当前登录态」——
//!      账号记录或 auth.json 被客户端更新后拿回最新凭证，读不到更新就按现状
//!      放行（非强制）或明确要求重新登录（强制）。
//!   JWT 解码（不验签，只读 exp/name/iss）在 `jwt.rs`，本文件只用它的结果。
//!
//! ── 为什么与 workbuddy 的 auth.rs 不共用 ─────────────────────
//! 两者的凭证形态完全不同：workbuddy 是「账号记录 + /auth/token/refresh 的
//! X-Refresh-Token 头 + 企业条件头」，小浣熊是「JWT + 桌面端登录态文件」。
//! 共用会变成一堆 `if provider == ...`。共用的只有**不变量**：持账号锁期间
//! 绝不做网络请求、绝不 unwrap/expect。
//!
//! ── 与源实现的差异（有意）──────────────────────────────────
//!   - 源实现把解析出的凭证缓存在闭包里；这里每次都从账号存储取快照（账号文件
//!     本来就是「实时走磁盘」的语义），只有 auth.json 那一路有 mtime 缓存。
//!
//! ── 关于 Send 的硬约束（踩过的坑）───────────────────────────
//! 适配器的 `ensure_access_token` / `refresh_access_token` 返回的 future 必须是
//! `Send`（转发跑在多线程运行时上，future 会跨 await 点被持有）。因此本文件
//! **任何 async fn 内部都不得持有非 Send 的守卫**：`auth_cache()` 的
//! `MutexGuard<Option<Map>>` 就不是 Send。做法是让「读 auth.json」这条链上
//! 的每个 async 函数都只调用**同步**函数（同步函数内的守卫在返回前必然析构），
//! 绝不在 async 块里直接 `if let Ok(guard) = cache.lock()`。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{json, Map, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::jwt;

/// 桌面端实时账号的固定 id（架构文档 §3.2：导入后为 raccoon 的一个普通列表项）
pub const DESKTOP_ACCOUNT_ID: &str = "raccoon-desktop";

/// 桌面端登录态文件的大小上限（照抄 `raccoon-local-auth.mjs` 的
/// `MAX_AUTH_FILE_SIZE`）。超限说明这个路径上是个异常文件，直接拒绝比把它
/// 整体读进内存安全。
const MAX_AUTH_FILE_SIZE: u64 = 256 * 1024;

/// 鉴权 API 请求超时（源实现 `REQUEST_TIMEOUT_MS`；鉴权接口是普通短请求，
/// 15 分钟的 LLM 超时在这里不适用）。
pub(super) const AUTH_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 一份解析好的小浣熊凭证（源实现 `credentials` 对象的等价物）。
#[derive(Clone, Debug, Default)]
pub struct RaccoonCredentials {
    /// 账号 id（`raccoon-desktop` 或 `user-<userId>`）
    pub id: String,
    pub token: String,
    /// 过期时间（毫秒）。JWT 解不出 exp 时为 None
    pub expires_at: Option<f64>,
    /// 用户 ID（JWT 的十六进制 id 约定，见 `jwt::extract_user_id`）
    pub user_id: String,
    pub org_code: String,
    /// 凭证来源：决定登录态重读从哪里取
    pub origin: CredentialOrigin,
}

/// 凭证来源（决定登录态重读从哪里取）
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CredentialOrigin {
    /// 账号记录里的 token（手动添加/旧数据导入）→ 重读账号存储
    #[default]
    AccountStore,
    /// `~/.box-agent/config/auth.json`（桌面端实时登录态）→ 重读 auth.json
    AuthFile,
}

// ─── 桌面端登录态文件（~/.box-agent/config/auth.json）──────────

/// 桌面端登录态的读取结果：JSON 对象 + 文件修改时间
struct AuthFileSnapshot {
    json: Map<String, Value>,
    modified_at: i64,
}

pub fn desktop_auth_file() -> PathBuf {
    super::desktop::desktop_auth_file()
}

/// mtime 缓存：`(路径, mtime, 文件内容)`。
///
/// 源实现按 mtime 做缓存（`cacheKey = "auth:${modifiedAt}"`）：桌面端/box-agent
/// 会刷新并回写这个文件，所以「按 mtime 实时读盘」既能拿到最新登录态，
/// 又不会让每个转发请求都做一次文件 IO。缓存是**进程级**的：多个调用点共享
/// 同一份，避免各自持有过期快照。
fn auth_cache() -> &'static Mutex<Option<(String, i64, Map<String, Value>)>> {
    static CACHE: OnceLock<Mutex<Option<(String, i64, Map<String, Value>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 文件形态防御（源实现照抄）：路径必须是**普通文件**（不是符号链接）、大小在
/// `(0, 256KB]` 之间。返回文件 mtime（毫秒）。
fn auth_file_meta(path: &Path) -> Result<i64, String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("无法访问小浣熊登录态 {}: {error}", path.display()))?;
    // 符号链接与非常规文件一律拒绝（symlink_metadata 不会被解引用）
    if meta.file_type().is_symlink() {
        return Err("小浣熊登录态路径是符号链接，已拒绝读取".to_string());
    }
    if !meta.is_file() {
        return Err("小浣熊登录态路径不是普通文件".to_string());
    }
    if meta.len() == 0 || meta.len() > MAX_AUTH_FILE_SIZE {
        return Err("小浣熊登录态文件大小异常".to_string());
    }
    Ok(mtime_ms(&meta))
}

/// 读 auth.json（**不带缓存**）：形态防御 + 解析。
///
/// 登录态重读的兜底（`latest_credentials`）必须走这条路径：缓存按 mtime 命中，
/// 而「同一毫秒内的两次写入」在 Windows 上可能拿到相同 mtime，只有真读盘才能
/// 看到内容变化。
fn read_auth_file_raw(path: &Path) -> Result<AuthFileSnapshot, String> {
    let modified_at = auth_file_meta(path)?;
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("读取小浣熊登录态失败: {error}"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| "小浣熊登录态无法解析（不是有效 JSON）".to_string())?;
    let Value::Object(json) = value else {
        return Err("小浣熊登录态根节点不是 JSON 对象".to_string());
    };
    Ok(AuthFileSnapshot { json, modified_at })
}

/// 读 auth.json（带 mtime 缓存与文件形态防御；防御项见 `auth_file_meta`）。
///
/// **同步函数**（见模块头关于 Send 的说明）：守卫在本函数返回前析构，
/// 调用它的 async 函数因此不会持有非 Send 的跨 await 状态。
fn read_auth_file() -> Result<AuthFileSnapshot, String> {
    let path = desktop_auth_file();
    let modified_at = auth_file_meta(&path)?;
    let key = path.to_string_lossy().to_string();
    // 缓存命中：只做一次「取锁 → 比较 → 克隆」；锁在本表达式结束前释放
    let cached = match auth_cache().lock() {
        Ok(guard) => guard
            .as_ref()
            .filter(|(cached_key, cached_mtime, _)| {
                *cached_key == key && *cached_mtime == modified_at
            })
            .map(|(_, _, json)| json.clone()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .filter(|(cached_key, cached_mtime, _)| {
                *cached_key == key && *cached_mtime == modified_at
            })
            .map(|(_, _, json)| json.clone()),
    };
    if let Some(json) = cached {
        return Ok(AuthFileSnapshot { json, modified_at });
    }
    let snapshot = read_auth_file_raw(&path)?;
    if let Ok(mut guard) = auth_cache().lock() {
        *guard = Some((key, snapshot.modified_at, snapshot.json.clone()));
    }
    Ok(snapshot)
}

/// 文件的修改时间（毫秒）；取不到给 0
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 auth.json 的字段读成凭证（源实现 `getCredentials` 的字段口径）：
/// `access_token` ?? `token` ?? `auth_token`。刷新令牌不再读取 —— 上游已无
/// 旧版刷新接口，磁盘上残留的刷新令牌一律作废。
fn credentials_from_auth_file(snapshot: &AuthFileSnapshot) -> Result<RaccoonCredentials, String> {
    let pick = |keys: &[&str]| -> String {
        for key in keys {
            if let Some(Value::String(text)) = snapshot.json.get(*key) {
                let value = jwt::strip_bearer(text);
                if !value.is_empty() {
                    return value;
                }
            }
        }
        String::new()
    };
    let token = pick(&["access_token", "token", "auth_token"]);
    if token.is_empty() {
        return Err("小浣熊登录态里没有 access_token（请在客户端重新登录）".to_string());
    }
    let claims = jwt::decode_jwt_claims(&token);
    let user_id = claims
        .as_ref()
        .map(jwt::extract_user_id)
        .unwrap_or_default();
    Ok(RaccoonCredentials {
        id: DESKTOP_ACCOUNT_ID.to_string(),
        expires_at: jwt::jwt_expiry_ms(&token),
        token,
        user_id,
        org_code: super::headers::org_code(&Value::Object(snapshot.json.clone())),
        origin: CredentialOrigin::AuthFile,
    })
}

/// 桌面端登录态的**摘要**（导入/刷新账号记录时写进账号字段）。
///
/// 返回含 `userId` / `tokenTail` / `tokenExpiresAt` / `mtime` 的对象；读不到时
/// Err。**不含 token 本身**：桌面端账号的凭证不落账号记录（架构文档 §3.2）。
pub fn desktop_summary() -> Result<Value, String> {
    let snapshot = read_auth_file()?;
    let credentials = credentials_from_auth_file(&snapshot)?;
    Ok(json!({
        "userId": credentials.user_id,
        "tokenTail": token_tail(&credentials.token),
        "tokenExpiresAt": credentials
            .expires_at
            .map(Value::from)
            .unwrap_or(Value::Null),
        "officeIdentity": snapshot.json.get("office_identity"),
        "officeOrgName": snapshot.json.get("office_org_name"),
        "officeOrgRole": snapshot.json.get("office_org_role"),
        "mtime": snapshot.modified_at,
    }))
}

/// token 尾 4 字符（源实现 `token.slice(-4)`）
pub(super) fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

// ─── 凭证快照（从账号存储或 auth.json 取）──────────────────────

/// 取指定账号的凭证快照（**异步**：账号来自网关的 `store.get` 回调）。
///
/// 仅未指定账号时允许回落到本机登录态；显式账号缺失必须失败。
///
/// ── 为什么判据是记录里的 `desktop` 标记而不是只看 id ──────────
/// id `raccoon-desktop` 只是缺省值。判据落在标记上更稳，id 只作兜底。
pub async fn snapshot(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<RaccoonCredentials, GatewayError> {
    let record = match crate::store::record(ctx, super::ID, account_id).await {
        Ok(record) => record,
        Err(_) if account_id.is_empty() => return desktop_credentials(),
        Err(error) => return Err(error),
    };
    let is_desktop = record
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || record.get("id").and_then(Value::as_str) == Some(DESKTOP_ACCOUNT_ID);
    if is_desktop {
        return desktop_credentials();
    }
    let token = record
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            format!(
                "账号 {} 没有可用凭证，请重新添加",
                record.get("id").and_then(Value::as_str).unwrap_or("(未知)")
            ),
        ));
    }
    // 刷新令牌不再读取：上游已无旧版刷新接口，记录里残留的刷新令牌一律作废
    let expires_at = record
        .get("expiresAt")
        .and_then(Value::as_f64)
        .or_else(|| jwt::jwt_expiry_ms(&token));
    Ok(RaccoonCredentials {
        id: record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        token,
        expires_at,
        user_id: record
            .get("userId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        org_code: super::headers::org_code(&record),
        origin: CredentialOrigin::AccountStore,
    })
}

/// 桌面端实时登录态的凭证（读不到时给 401，文案说明该怎么做）。
/// 公开（`pub`）是为了让账号存储构造会话时能取到实时 token（见 `store.rs`）。
pub fn desktop_credentials() -> Result<RaccoonCredentials, GatewayError> {
    let snapshot = read_auth_file().map_err(|reason| {
        GatewayError::with_status(401, format!("没有可用的小浣熊登录态：{reason}"))
    })?;
    credentials_from_auth_file(&snapshot).map_err(|reason| GatewayError::with_status(401, reason))
}

// ─── 登录态重读 ───────────────────────────────────────────────

/// 刷新凭证（兼容旧签名）：上游已无旧版刷新接口（IAM 迁移后 `/refresh` 不再
/// 发放刷新令牌），本函数只做「重读当前登录态」——账号记录或 auth.json 被客户端
/// 更新后拿回最新凭证；读不到比当前更新的凭证时按原凭证放行（非强制）或明确
/// 要求重新登录（强制，401 后被拒的 token 重读也拿不回来）。
///
/// **持锁边界**：网络请求与等待都在锁外 —— 与账号锁的硬约束同一精神。
pub async fn refresh(
    ctx: &PluginCtx,
    credentials: &RaccoonCredentials,
    force: bool,
) -> Result<RaccoonCredentials, GatewayError> {
    // 旧 JWT 登录态同样没有可续期的来源（残留的刷新令牌已随接口下线作废），
    // 与 IAM 同一处置：只能重读登录态
    reread(ctx, credentials, force).await
}

/// 重读当前登录态：与手上凭证不同则返回最新值（IAM 需要仍然有效）；
/// 与手上凭证相同则按 force 分流 —— 非强制放行原凭证，强制明确报错。
async fn reread(
    ctx: &PluginCtx,
    credentials: &RaccoonCredentials,
    force: bool,
) -> Result<RaccoonCredentials, GatewayError> {
    let latest = match latest_credentials(ctx, credentials).await {
        Ok(latest) => latest,
        Err(error) => {
            // 记录刚被删除 / 登录态文件刚被移走：非强制时保留原凭证放行，
            // 强制（401 后）如实报错
            if !force { return Ok(credentials.clone()); }
            return Err(error);
        }
    };
    if latest.token != credentials.token {
        if jwt::is_iam_token(&latest.token) {
            jwt::validate_iam_token(&latest.token)?;
        }
        return Ok(latest);
    }
    if force {
        return Err(GatewayError::with_status(
            401,
            if credentials.origin == CredentialOrigin::AuthFile {
                "小浣熊桌面端登录态已过期，请打开小浣熊客户端重新登录"
            } else {
                "小浣熊账号登录态已被上游拒绝且没有可续期的接口，请重新登录该账号"
            },
        ));
    }
    Ok(credentials.clone())
}

/// 重新取当前凭证（登录态重读的兜底）。
///
/// 桌面端来源**绕过 mtime 缓存重新读盘**（文件刚被改写，缓存里可能还是旧内容；
/// 顺便失效缓存，让后续读取也走新内容）；账号来源重新读账号记录。
///
/// 账号来源额外要求记录**仍然存在**：`snapshot` 在没有账号记录时会回落到
/// 桌面端登录态，而「账号已被用户删除」绝不能变成「拿另一个账号的凭证继续」。
async fn latest_credentials(
    ctx: &PluginCtx,
    previous: &RaccoonCredentials,
) -> Result<RaccoonCredentials, GatewayError> {
    match previous.origin {
        CredentialOrigin::AuthFile => {
            if let Ok(mut guard) = auth_cache().lock() {
                *guard = None;
            }
            let snapshot = read_auth_file_raw(&desktop_auth_file()).map_err(|reason| {
                GatewayError::with_status(401, format!("没有可用的小浣熊登录态：{reason}"))
            })?;
            credentials_from_auth_file(&snapshot)
                .map_err(|reason| GatewayError::with_status(401, reason))
        }
        CredentialOrigin::AccountStore => {
            if previous.id.is_empty()
                || crate::store::record(ctx, super::ID, &previous.id)
                    .await
                    .is_err()
            {
                return Err(GatewayError::with_status(
                    401,
                    format!("账号 {} 已被删除，请重新登录该账号", previous.id),
                ));
            }
            snapshot(ctx, &previous.id).await
        }
    }
}
