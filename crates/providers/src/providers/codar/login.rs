//! 科搭的微信扫码登录。
//!
//! ── 流程 ────────────────────────────────────────────────────
//! 1. `POST /v1/wechat/login/qrcode` 取 `scene` 与二维码地址；
//! 2. 把二维码地址作为登录地址交给宿主打开，用户用微信扫码；
//! 3. 插件按 `login.poll` 轮询 `/v1/wechat/login/status`，成功后拿到令牌并
//!    核对账号资料。
//!
//! 手机验证码登录需要宿主提供交互式表单与人机验证，当前网关只对 WASM 插件
//! 开放该能力；原生入口提供「先发送验证码，再按 `sms:手机号:验证码` 导入」的
//! 两段式路径（见 `accounts::sms_login_input`）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::{self, Credentials};
use super::{client, endpoints};

const POLL_INTERVAL_MS: u64 = 2_000;
const FLOW_TTL_MS: i64 = 180_000;

struct Flow {
    scene: String,
    deadline: i64,
    name: String,
    next_poll_at: i64,
    gateway_proxy: String,
    proxy: Option<proto::WireProxy>,
}

enum FlowState {
    Pending(Flow),
    Done(Box<proto::NormalizedAccount>),
    Failed(String),
}

static FLOWS: LazyLock<Mutex<HashMap<String, FlowState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登录方式面板。
///
/// 字段名与取值必须对齐网关的 `LoginMethodMetadata`：`type` 只能是
/// `oauth` / `api`，提示项类型只能是 `text` / `select`，且 `api + localOnly`
/// 的组合会被网关拒绝（本机读取用 `oauth + localOnly`）。
pub fn login_methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 0,
                "type": "oauth",
                "label": "微信扫码登录科搭",
                "instructions": "插件会打开二维码地址，用微信扫码并在手机上确认。",
                "prompts": [],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url",
                "browserMode": "external"
            },
            {
                "methodIndex": 1,
                "type": "api",
                "label": "粘贴访问令牌",
                "instructions": "粘贴科搭签发的 JWT 或包含 accessToken 的凭据 JSON；\
                    短信登录可先通过管理动作发送验证码，再按 sms:手机号:验证码 导入。",
                "prompts": [
                    {
                        "key": "key",
                        "type": "text",
                        "label": "科搭访问令牌",
                        "placeholder": "eyJ… 或 {\"accessToken\":\"…\"}",
                        "secret": true,
                        "optional": false
                    }
                ],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url"
            },
            {
                "methodIndex": 2,
                "type": "oauth",
                "label": "读取本机科搭客户端登录态",
                "instructions": "从 ~/.codar/config.json 读取当前登录令牌（只读，不写回）。",
                "prompts": [],
                "localOnly": true,
                "manualCallback": false,
                "callbackInput": "url"
            }
        ]
    })
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        for (index, slot) in buf.iter_mut().enumerate() {
            *slot = ((nanos >> (8 * (index % 16))) & 0xff) as u8;
        }
    }
    buf.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 发起扫码登录。
pub async fn start_browser_flow(
    ctx: &PluginCtx,
    name: &str,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::LoginStart, String> {
    let gateway_proxy = ctx.env.proxy.clone();
    let (status, body) = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::QR,
        None,
        Some(&json!({})),
        proxy,
        &gateway_proxy,
        12_000,
    )
    .await
    .map_err(|error| error.message)?;
    if !(200..300).contains(&status) || client::failed(&body) {
        return Err(client::upstream_error(status, &body).message);
    }
    let data = client::data(&body);
    let scene = data
        .get("scene")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or_else(|| "科搭没有返回有效扫码标识".to_string())?
        .to_string();
    let raw_url = data
        .get("qr_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "科搭没有返回二维码地址".to_string())?;
    let url = url::Url::parse(raw_url.trim())
        .map_err(|_| "科搭二维码地址无效".to_string())?;
    if !matches!(
        url.host_str(),
        Some("mp.weixin.qq.com" | "api.kuaipaper.com" | "api.claw.speedai.com")
    ) {
        return Err("科搭二维码地址不属于已确认的官方来源".to_string());
    }
    let flow = random_hex(16);
    let now = crate::http::now_ms();
    if let Ok(mut flows) = FLOWS.lock() {
        flows.insert(
            flow.clone(),
            FlowState::Pending(Flow {
                scene,
                deadline: now + FLOW_TTL_MS,
                name: name.to_string(),
                next_poll_at: 0,
                gateway_proxy,
                proxy: proxy.cloned(),
            }),
        );
    }
    Ok(proto::LoginStart {
        flow,
        url: url.as_str().to_string(),
        instructions: "用微信扫描二维码并在手机上确认登录".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: true,
        allowed_hosts: vec![
            "mp.weixin.qq.com".to_string(),
            "api.kuaipaper.com".to_string(),
            "api.claw.speedai.com".to_string(),
        ],
    })
}

/// 一次性完成的本机关联流程：读取客户端登录态、核对资料并直接置为完成。
pub async fn start_desktop_flow(
    ctx: &PluginCtx,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::LoginStart, String> {
    let flow = random_hex(16);
    let outcome = match super::desktop::read() {
        Ok(login) => {
            finish_with_token_source("", &login.token, "desktop", proxy, &ctx.env.proxy).await
        }
        Err(message) => Err(message),
    };
    if let Ok(mut flows) = FLOWS.lock() {
        flows.insert(
            flow.clone(),
            match outcome {
                Ok(account) => FlowState::Done(Box::new(account)),
                Err(message) => FlowState::Failed(message),
            },
        );
    }
    Ok(proto::LoginStart {
        flow,
        url: String::new(),
        instructions: "已读取本机科搭客户端登录态".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: false,
        allowed_hosts: Vec::new(),
    })
}

/// 查询扫码状态。
pub async fn poll(flow: &str) -> proto::LoginPoll {
    let snapshot = {
        let flows = match FLOWS.lock() {
            Ok(flows) => flows,
            Err(poisoned) => poisoned.into_inner(),
        };
        match flows.get(flow) {
            Some(FlowState::Done(account)) => {
                return proto::LoginPoll::Done { account: (**account).clone() }
            }
            Some(FlowState::Failed(message)) => {
                return proto::LoginPoll::Failed { message: message.clone() }
            }
            Some(FlowState::Pending(flow)) => (
                flow.scene.clone(),
                flow.name.clone(),
                flow.deadline,
                flow.next_poll_at,
                flow.gateway_proxy.clone(),
                flow.proxy.clone(),
            ),
            None => {
                return proto::LoginPoll::Failed {
                    message: "科搭登录流程不存在或已取消".to_string(),
                }
            }
        }
    };
    let (scene, name, deadline, next_poll_at, gateway_proxy, proxy) = snapshot;
    let now = crate::http::now_ms();
    if now > deadline {
        set_failed(flow, "科搭扫码登录已过期，请重新发起".to_string());
        return proto::LoginPoll::Failed {
            message: "科搭扫码登录已过期，请重新发起".to_string(),
        };
    }
    if now < next_poll_at {
        return proto::LoginPoll::Pending {
            hint: Some("等待微信扫码确认".to_string()),
        };
    }
    let reply = client::request(
        "POST",
        endpoints::ACCOUNT_BASE,
        endpoints::QR_STATUS,
        None,
        Some(&json!({ "scene": scene })),
        proxy.as_ref(),
        &gateway_proxy,
        65_000,
    )
    .await;
    let (status, body) = match reply {
        Ok(reply) => reply,
        Err(_) => {
            touch_next_poll(flow, now + 2_000);
            return proto::LoginPoll::Pending {
                hint: Some("扫描状态查询失败，稍后重试".to_string()),
            };
        }
    };
    let business_code = body
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| client::data(&body).get("code").and_then(Value::as_i64));
    if status == 404 || business_code == Some(404) {
        let message = "科搭二维码已失效或登录场景已过期，请重新发起登录".to_string();
        set_failed(flow, message.clone());
        return proto::LoginPoll::Failed { message };
    }
    if !(200..300).contains(&status) || client::failed(&body) {
        let message = client::upstream_error(status, &body).message;
        set_failed(flow, message.clone());
        return proto::LoginPoll::Failed { message };
    }
    let data = client::data(&body);
    let state = data
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| body.get("status").and_then(Value::as_str))
        .unwrap_or("")
        .to_ascii_lowercase();
    let code = body
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| data.get("code").and_then(Value::as_i64));
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .or_else(|| body.get("token").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    // 扫码成功后上游可能要求先绑定手机号或验证新 IP（返回临时令牌）。原生入口
    // 没有交互式表单，这里给出明确指引，而不是笼统的「扫码未完成」。
    if matches!(state.as_str(), "bind_phone_required" | "ip_verify_required") || code == Some(201) {
        let message = "扫码已确认，但该账号需要先绑定手机号或验证新 IP；请在官方客户端完成一次登录后，改用「读取本机登录态」或「粘贴访问令牌」添加".to_string();
        set_failed(flow, message.clone());
        return proto::LoginPoll::Failed { message };
    }
    if matches!(state.as_str(), "" | "pending" | "scanned") && token.is_empty() {
        touch_next_poll(flow, crate::http::now_ms() + 2_000);
        return proto::LoginPoll::Pending {
            hint: Some("等待微信扫码确认".to_string()),
        };
    }
    if state != "success" || token.is_empty() {
        let message = match state.as_str() {
            "expired" => "科搭二维码已过期，请重新发起登录".to_string(),
            "canceled" | "cancelled" => "科搭扫码已取消，请重新发起登录".to_string(),
            "failed" | "rejected" => "科搭扫码登录被上游拒绝，请重新发起登录".to_string(),
            "" => "科搭扫码未完成，请重新发起登录".to_string(),
            other => format!("科搭扫码未完成（状态：{other}），请重新发起登录"),
        };
        set_failed(flow, message.clone());
        return proto::LoginPoll::Failed { message };
    }
    match finish_with_token(&name, &token, proxy.as_ref(), &gateway_proxy).await {
        Ok(account) => {
            if let Ok(mut flows) = FLOWS.lock() {
                flows.insert(flow.to_string(), FlowState::Done(Box::new(account.clone())));
            }
            proto::LoginPoll::Done { account }
        }
        Err(message) => {
            set_failed(flow, message.clone());
            proto::LoginPoll::Failed { message }
        }
    }
}

/// 取消登录流程。
pub fn cancel(flow: &str) {
    if let Ok(mut flows) = FLOWS.lock() {
        flows.remove(flow);
    }
}

fn touch_next_poll(flow: &str, at: i64) {
    if let Ok(mut flows) = FLOWS.lock() {
        if let Some(FlowState::Pending(pending)) = flows.get_mut(flow) {
            pending.next_poll_at = at;
        }
    }
}

fn set_failed(flow: &str, message: String) {
    if let Ok(mut flows) = FLOWS.lock() {
        flows.insert(flow.to_string(), FlowState::Failed(message));
    }
}

/// 用扫码得到的令牌核对资料并生成账号。
async fn finish_with_token(
    name: &str,
    token: &str,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<proto::NormalizedAccount, String> {
    finish_with_token_source(name, token, "wechat", proxy, gateway_proxy).await
}

async fn finish_with_token_source(
    name: &str,
    token: &str,
    source: &str,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<proto::NormalizedAccount, String> {
    let token = super::credentials::token(token)?;
    if super::credentials::expires(&token).is_some_and(|expires| expires <= crate::http::now_ms()) {
        return Err("科搭扫码返回的登录凭据已过期，请重新发起".to_string());
    }
    let profile = client::profile(&token, proxy, gateway_proxy)
        .await
        .map_err(|error| error.message)?;
    let data = client::data(&profile);
    let username = ["username", "user_name"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "科搭没有返回有效账号名".to_string())?
        .to_string();
    let claim_user = super::credentials::claim_username(&token);
    if !claim_user.is_empty() && claim_user != username {
        return Err("科搭令牌与账号资料不一致，请重新登录".to_string());
    }
    let nickname = ["display_name", "nickname"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let credentials = Credentials {
        name: if name.trim().is_empty() {
            if nickname.is_empty() { username.clone() } else { nickname.clone() }
        } else {
            name.trim().to_string()
        },
        expires_at: super::credentials::expires(&token),
        access_token: token,
        username,
        uid: data.get("uid").and_then(Value::as_str).unwrap_or("").trim().to_string(),
        nickname,
        source: source.to_string(),
    };
    Ok(accounts::account_of(credentials))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_methods_match_gateway_contract() {
        let value = login_methods();
        let Some(methods) = value["methods"].as_array() else {
            assert!(false, "methods 必须是数组");
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for method in methods {
            let Some(index) = method["methodIndex"].as_u64() else {
                assert!(false, "methodIndex 必须是数字");
                continue;
            };
            assert!(seen.insert(index), "methodIndex 不能重复");
            let kind = method["type"].as_str().unwrap_or("");
            assert!(matches!(kind, "oauth" | "api"), "type 只能是 oauth/api");
            let local = method["localOnly"].as_bool().unwrap_or(false);
            assert!(!(kind == "api" && local), "api 方式不能是本机读取");
            let Some(prompts) = method["prompts"].as_array() else {
                assert!(false, "prompts 必须是数组");
                continue;
            };
            for prompt in prompts {
                let prompt_kind = prompt["type"].as_str().unwrap_or("");
                assert!(matches!(prompt_kind, "text" | "select"), "提示项类型只能是 text/select");
                assert!(!prompt["key"].as_str().unwrap_or("").is_empty());
                assert!(prompt.get("label").is_some(), "提示项必须带 label");
            }
        }
    }
}
