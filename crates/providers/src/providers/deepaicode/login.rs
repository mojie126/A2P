//! DeepAI Code 的浏览器授权登录。
//!
//! ── 流程 ────────────────────────────────────────────────────
//! 1. 打开 `https://api.deepai.org/deepai_code/desktop_auth/?state=…&port=…`；
//! 2. 官方登录完成后重定向到 `http://127.0.0.1:<port>/?code=…`；
//! 3. 插件在本机随机端口接收回调，再 `POST …/desktop_auth/exchange/` 换取
//!    `{api_key, username, is_pro}`；
//! 4. 网关按 `login.poll` 取回账号。
//!
//! 服务端部署时浏览器与插件不在同一台机器，本机回调不可达；此时用户可以把
//! 回调地址里的 `code` 通过 `login.callback` 交给插件完成同一套换取。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::accounts::{self, Credentials};
use super::{client, endpoints};
use agent2api_provider_protocol as proto;

/// 回调等待上限。
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// 轮询间隔。
const POLL_INTERVAL_MS: u64 = 1_000;

enum FlowState {
    Pending,
    Done(Box<proto::NormalizedAccount>),
    Failed(String),
}

static FLOWS: LazyLock<Mutex<HashMap<String, FlowState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登录流程选中的出网代理（登录阶段还没有账号记录，代理来自 `login.start` 的选项）。
static FLOW_PROXIES: LazyLock<Mutex<HashMap<String, proto::WireProxy>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登录方式面板（与 `auth.methods` 管理动作同形）。
///
/// 字段名与取值必须对齐网关的 `LoginMethodMetadata`：`type` 只能是
/// `oauth` / `api`，提示项类型只能是 `text` / `select`，且 `api + localOnly`
/// 的组合会被网关拒绝（本机读取用 `oauth + localOnly`）。
pub fn login_methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 1,
                "type": "oauth",
                "label": "打开浏览器登录 DeepAI",
                "instructions": "插件会打开系统浏览器。完成 DeepAI 登录后会自动回调本机随机端口，\
                    换取 API Key 并添加账号。服务端部署时若回调不可达，可直接粘贴回调地址里的 code。",
                "prompts": [],
                "localOnly": false,
                "manualCallback": true,
                "callbackInput": "url",
                "validatesCallback": true,
                "browserMode": "external"
            },
            {
                "methodIndex": 2,
                "type": "api",
                "label": "粘贴 API Key",
                "instructions": "在 deepai.org 账户页获取 API Key 后粘贴。",
                "prompts": [
                    {
                        "key": "key",
                        "type": "text",
                        "label": "DeepAI API Key",
                        "placeholder": "API Key（UUID 形态）",
                        "secret": true,
                        "optional": false
                    }
                ],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url"
            },
            {
                "methodIndex": 3,
                "type": "oauth",
                "label": "读取本机 DeepAI Code 登录态",
                "instructions": "从 ~/.deepai-code/userdata/secrets/deepai-account.bin 读取（明文 JSON），\
                    不写回客户端文件。",
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

/// 发起浏览器登录：绑定随机本机端口并等待回调。
pub async fn start_browser_flow(
    ctx: &PluginCtx,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::LoginStart, String> {
    let state = random_hex(16);
    let gateway_proxy = ctx.env.proxy.clone();
    let flow_proxy = proxy.cloned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("无法在本机监听登录回调端口：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("读取回调端口失败：{error}"))?
        .port();
    let flow = state.clone();
    if let Ok(mut flows) = FLOWS.lock() {
        flows.insert(flow.clone(), FlowState::Pending);
    }
    if let Some(proxy) = &flow_proxy {
        if let Ok(mut proxies) = FLOW_PROXIES.lock() {
            proxies.insert(flow.clone(), proxy.clone());
        }
    }
    let flow_for_task = flow.clone();
    tokio::spawn(async move {
        let outcome = tokio::time::timeout(CALLBACK_TIMEOUT, accept_callback(listener)).await;
        let state = match outcome {
            Ok(Ok(code)) => exchange_to_account(&code, flow_proxy.as_ref(), &gateway_proxy).await,
            Ok(Err(message)) => Err(message),
            Err(_) => Err("浏览器登录等待回调超时，请重试".to_string()),
        };
        let mut flows = match FLOWS.lock() {
            Ok(flows) => flows,
            Err(poisoned) => poisoned.into_inner(),
        };
        // 已被取消的流程不再写回结果
        if matches!(flows.get(&flow_for_task), Some(FlowState::Pending)) {
            flows.insert(
                flow_for_task,
                match state {
                    Ok(account) => FlowState::Done(Box::new(account)),
                    Err(message) => FlowState::Failed(message),
                },
            );
        }
    });
    Ok(proto::LoginStart {
        flow,
        url: format!(
            "{}{}?state={state}&port={port}",
            endpoints::BASE_ORIGIN,
            endpoints::SIGNIN_PATH
        ),
        instructions: "在打开的浏览器里完成 DeepAI 登录，完成后会自动回到面板".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: true,
        allowed_hosts: vec!["api.deepai.org".to_string(), "deepai.org".to_string()],
    })
}

/// 一次性完成的本机读取流程：读取、验活并直接置为完成。
pub async fn start_desktop_flow(
    ctx: &PluginCtx,
    proxy: Option<&proto::WireProxy>,
) -> Result<proto::LoginStart, String> {
    let flow = random_hex(16);
    let outcome = match super::accounts::read_desktop_credentials() {
        Some(credentials) => match client::verify_key(&credentials, proxy, &ctx.env.proxy).await {
            Ok(()) => Ok(accounts::account_of(credentials)),
            Err(message) => Err(message),
        },
        None => Err(
            "没有找到本机 DeepAI Code 登录态（~/.deepai-code/userdata/secrets/deepai-account.bin），\
             请先在 DeepAI Code 桌面端登录"
                .to_string(),
        ),
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
        instructions: "已读取本机 DeepAI Code 登录态".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: false,
        allowed_hosts: Vec::new(),
    })
}

/// 接收一次带 `code` 的回调请求。
async fn accept_callback(listener: tokio::net::TcpListener) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|error| format!("接收登录回调失败：{error}"))?;
        let mut buf = vec![0u8; 8192];
        let read = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let Some(code) = code_from_target(target) else {
            let _ = socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        };
        let body = "<html><body>DeepAI 登录完成，可以关闭此页面回到面板。</body></html>";
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        let _ = socket.shutdown().await;
        return Ok(code);
    }
}

/// 从请求目标里取 `code`（只接受本机路径）。
fn code_from_target(target: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.trim().to_string())
        .filter(|code| !code.is_empty() && code.len() <= 4096)
}

/// 用回调 `code` 换取账号；`login.callback` 也走这里。
pub async fn complete_with_code(flow: &str, code: &str, gateway_proxy: &str) -> proto::LoginPoll {
    let proxy = FLOW_PROXIES
        .lock()
        .ok()
        .and_then(|mut proxies| proxies.remove(flow));
    match exchange_to_account(code.trim(), proxy.as_ref(), gateway_proxy).await {
        Ok(account) => {
            if let Ok(mut flows) = FLOWS.lock() {
                flows.insert(flow.to_string(), FlowState::Done(Box::new(account.clone())));
            }
            proto::LoginPoll::Done { account }
        }
        Err(message) => {
            if let Ok(mut flows) = FLOWS.lock() {
                flows.insert(flow.to_string(), FlowState::Failed(message.clone()));
            }
            proto::LoginPoll::Failed { message }
        }
    }
}

async fn exchange_to_account(
    code: &str,
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
) -> Result<proto::NormalizedAccount, String> {
    if code.is_empty() {
        return Err("DeepAI 登录回调缺少 code".to_string());
    }
    let url = format!("{}{}", endpoints::BASE_ORIGIN, endpoints::EXCHANGE_PATH);
    let body = json!({ "code": code });
    let client = crate::http::control_client(proxy, gateway_proxy);
    let response = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(30))
        .header("Accept", "application/json")
        .header("User-Agent", endpoints::USER_AGENT)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("DeepAI 登录换取 API Key 失败：{}", crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        return Err(client::error_message(&payload));
    }
    let data = if payload.get("data").is_some_and(|value| value.is_object()) {
        &payload["data"]
    } else {
        &payload
    };
    let api_key = ["api_key", "apiKey", "key"]
        .iter()
        .find_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "DeepAI 登录没有返回 API Key".to_string())?
        .to_string();
    let username = data
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let is_pro = data.get("is_pro").and_then(Value::as_bool).unwrap_or(false);
    let mut credentials = Credentials {
        name: if username.is_empty() { "DeepAI Code".to_string() } else { username.clone() },
        api_key,
        username,
        is_pro,
        machine_id: String::new(),
    };
    if let Err(message) = client::verify_key(&credentials, proxy, gateway_proxy).await {
        return Err(message);
    }
    credentials.name = if credentials.name.is_empty() { "DeepAI Code".to_string() } else { credentials.name };
    Ok(accounts::account_of(credentials))
}

/// 查询登录流程状态。
pub fn poll(flow: &str) -> proto::LoginPoll {
    let flows = match FLOWS.lock() {
        Ok(flows) => flows,
        Err(poisoned) => poisoned.into_inner(),
    };
    match flows.get(flow) {
        Some(FlowState::Done(account)) => proto::LoginPoll::Done { account: (**account).clone() },
        Some(FlowState::Failed(message)) => proto::LoginPoll::Failed { message: message.clone() },
        _ => proto::LoginPoll::Pending {
            hint: Some("等待浏览器完成登录".to_string()),
        },
    }
}

/// 取消并丢弃流程状态。
pub fn cancel(flow: &str) {
    if let Ok(mut flows) = FLOWS.lock() {
        flows.remove(flow);
    }
    if let Ok(mut proxies) = FLOW_PROXIES.lock() {
        proxies.remove(flow);
    }
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
