//! 百度搭子的登录：受控浏览器自动抓取 Cookie + 粘贴 Cookie 两条路。
//!
//! ── 为什么是受控浏览器 ──────────────────────────────────────
//! 百度搭子没有面向第三方程序的登录票据接口（参考实现实测过 qianfanproxy
//! 下只有秒哒的 login_ticket，且校验跳转目标必须是 miaoda.cn）；直接读系统
//! 浏览器的 cookie 库也不行——运行中独占文件锁，且新版 Chromium 用
//! App-Bound Encryption。参考实现（dumate2api 的 login-browser.js）的做法
//! 是复用本机已装的 Edge/Chrome 开一个全新临时 profile 的受控窗口，用户在
//! 里面手动登录，再从「自己这个窗口」读 cookie。这里用 Rust 等价物复刻：
//! 带 `--remote-debugging-port=0` 启动，从 profile 目录的 `DevToolsActivePort`
//! 文件拿到调试端口，经 CDP（`Network.getAllCookies`）轮询 cookie。
//!
//! ── 落地条件 ────────────────────────────────────────────────
//! 登录页 `login.bce.baidu.com` 登录后会重定向到百度智能云控制台；
//! `bce-long-term-sessionid` 这类会话 cookie 是访问 `console.bce.baidu.com`
//! 时才种下的，缺了它账号面接口一律 401（参考实现实测）。所以抓取完成
//! 的判据是 BDUSS 与智能云控制台会话同时存在；抓到后现场调换票接口
//! 验证（读接口不计费），再清理浏览器进程与临时 profile。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use super::accounts::{self, Credentials};
use super::client;

/// 登录页（登录后重定向到百度智能云控制台）。
const LOGIN_URL: &str =
    "https://login.bce.baidu.com/?redirect=https%3A%2F%2Fconsole.bce.baidu.com%2F";

/// 完成抓取必须拿到的 cookie 字段（控制台会话在登录落地后自动种下）。
const REQUIRED_COOKIES: &[&str] = &["BDUSS", "bce-long-term-sessionid"];

/// 单次 CDP 请求的超时。
const CDP_TIMEOUT_MS: u64 = 5_000;

/// 整个登录流程的超时（超时后轮询返回失败）。
const FLOW_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// 一轮浏览器登录的进程内状态。
struct BrowserFlow {
    child: tokio::process::Child,
    profile: PathBuf,
    debug_port: Option<u16>,
    proxy: Option<proto::WireProxy>,
    gateway_proxy: String,
    started_at: i64,
}

fn flows() -> &'static Mutex<HashMap<String, BrowserFlow>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, BrowserFlow>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, BrowserFlow>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// 结束一轮登录：杀浏览器进程、删临时 profile、移除流程。
fn cleanup_flow(table: &mut HashMap<String, BrowserFlow>, flow: &str) {
    if let Some(mut entry) = table.remove(flow) {
        let _ = entry.child.start_kill();
        // 浏览器刚退出时文件可能还被占用，参考实现同款处理：延迟重试删除。
        let profile = entry.profile.clone();
        tokio::spawn(async move {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                match std::fs::remove_dir_all(&profile) {
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }
        });
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 候选浏览器（参考实现同序：优先 Edge，其次 Chrome；环境变量可覆盖）。
fn browser_candidates() -> Vec<PathBuf> {
    let mut raw: Vec<String> = Vec::new();
    if let Ok(value) = std::env::var("DUMATE_BROWSER_PATH") {
        raw.push(value);
    }
    #[cfg(target_os = "windows")]
    {
        raw.push("C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe".to_string());
        raw.push("C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe".to_string());
        raw.push("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe".to_string());
        raw.push("C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        raw.push("/usr/bin/microsoft-edge".to_string());
        raw.push("/usr/bin/microsoft-edge-stable".to_string());
        raw.push("/usr/bin/google-chrome".to_string());
        raw.push("/usr/bin/google-chrome-stable".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        raw.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".to_string());
        raw.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string());
    }
    raw.into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .collect()
}

/// 发起一轮受控浏览器登录。
///
/// 返回的 `LoginStart` 地址为空：网关对空地址按「本机 / 自动登录」处理，
/// 只看 `login.poll` 的最终结果（网关侧口径）。
pub async fn start_browser_flow(
    proxy: Option<proto::WireProxy>,
    gateway_proxy: String,
) -> Result<proto::LoginStart, String> {
    let exe = browser_candidates().into_iter().next().ok_or_else(|| {
        "没有找到可用的 Edge 或 Chrome，请安装后重试，或改用「粘贴网页 Cookie」方式".to_string()
    })?;
    let profile = std::env::temp_dir().join(format!("agent2api-dumate-login-{}", now_ms()));
    std::fs::create_dir_all(&profile)
        .map_err(|error| format!("创建临时浏览器目录失败：{error}"))?;
    let mut command = tokio::process::Command::new(&exe);
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        LOGIN_URL.to_string(),
    ];
    // 登录窗口跟随账号代理（与对话同出口）；带认证的代理无法用命令行
    // 参数表达，遇到时如实失败，不静默直连。
    if let Some(wire) = &proxy {
        let Some(proxy_port) = wire.port else {
            return Err("代理配置缺少端口，请检查代理设置后重试".to_string());
        };
        let protocol = match wire.protocol.as_str() {
            "socks5" | "socks5h" => "socks5",
            _ => "http",
        };
        if !wire.username.is_empty() || !wire.password.is_empty() {
            return Err(
                "登录窗口暂不支持带认证的代理，请改用直连或无认证代理，或改用「粘贴网页 Cookie」方式"
                    .to_string(),
            );
        }
        args.insert(1, format!("--proxy-server={protocol}://{}:{proxy_port}", wire.host));
    }
    command.args(&args);
    let child = command
        .spawn()
        .map_err(|error| format!("启动浏览器失败：{error}"))?;
    let flow = format!("dumate-capture-{}", now_ms());
    with_flows(|table| {
        table.insert(
            flow.clone(),
            BrowserFlow {
                child,
                profile,
                debug_port: None,
                proxy,
                gateway_proxy,
                started_at: now_ms(),
            },
        );
    });
    Ok(proto::LoginStart {
        flow,
        url: String::new(),
        instructions: "在弹出的浏览器窗口里登录百度账号；登录成功后浏览器会跳到百度智能云控制台，\
            Cookie 会自动抓取并完成添加。完成后可手动关闭弹出的浏览器窗口。"
            .to_string(),
        poll_interval_ms: 2000,
        external_browser: false,
        allowed_hosts: Vec::new(),
    })
}

/// 从调试端口读全部 cookie（CDP `Network.getAllCookies`，页面级会话返回
/// 整个浏览器的 cookie）。
async fn cdp_cookies(port: u16) -> Result<Vec<(String, String)>, String> {
    let http = crate::http::control_client(None, "");
    let response = tokio::time::timeout(
        Duration::from_millis(CDP_TIMEOUT_MS),
        http.get(format!("http://127.0.0.1:{port}/json/list"))
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .map_err(|_| "读取浏览器调试接口超时".to_string())?
    .and_then(|response| response.error_for_status())
    .map_err(|error| format!("读取浏览器调试接口失败：{error}"))?;
    let list: Value = tokio::time::timeout(Duration::from_millis(CDP_TIMEOUT_MS), response.json())
        .await
        .map_err(|_| "读取浏览器调试接口超时".to_string())?
        .map_err(|error| format!("解析浏览器调试响应失败：{error}"))?;
    let targets = list
        .as_array()
        .ok_or_else(|| "浏览器调试接口返回格式异常".to_string())?;
    let ws_url = targets
        .iter()
        .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|target| target.get("webSocketDebuggerUrl"))
        .and_then(Value::as_str)
        .ok_or_else(|| "浏览器里没有可用的页面目标".to_string())?;
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_millis(CDP_TIMEOUT_MS),
        tokio_tungstenite::connect_async(ws_url),
    )
    .await
    .map_err(|_| "连接浏览器调试通道超时".to_string())?
    .map_err(|error| format!("连接浏览器调试通道失败：{error}"))?;
    socket
        .send(Message::text(
            json!({ "id": 1, "method": "Network.getAllCookies", "params": {} }).to_string(),
        ))
        .await
        .map_err(|error| format!("发送调试请求失败：{error}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(CDP_TIMEOUT_MS);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("读取浏览器 cookie 超时".to_string());
        }
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .map_err(|_| "读取浏览器 cookie 超时".to_string())?
            .ok_or_else(|| "浏览器调试通道已关闭".to_string())?
            .map_err(|error| format!("浏览器调试通道错误：{error}"))?;
        let Message::Text(text) = message else { continue };
        let parsed: Value = serde_json::from_str(&text).map_err(|_| "浏览器调试响应不是 JSON")?;
        if parsed.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        let cookies = parsed
            .pointer("/result/cookies")
            .and_then(Value::as_array)
            .ok_or_else(|| "浏览器调试响应缺少 cookie 列表".to_string())?;
        return Ok(cookies
            .iter()
            .filter_map(|cookie| {
                let name = cookie.get("name").and_then(Value::as_str)?.trim().to_string();
                let value = cookie.get("value").and_then(Value::as_str)?.trim().to_string();
                (!name.is_empty() && !value.is_empty()).then_some((name, value))
            })
            .collect());
    }
}

/// 把 cookie 列表拼成单行 Cookie 头（同名取后值）。
fn cookie_header_of(pairs: Vec<(String, String)>) -> String {
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, value) in pairs {
        if let Some(existing) = seen.iter_mut().find(|(key, _)| key == &name) {
            existing.1 = value;
        } else {
            seen.push((name, value));
        }
    }
    seen.iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 轮询一次：驱动「启动浏览器 → 等登录 → 等控制台会话 → 抓取验证」状态机。
pub async fn poll(ctx: &agent2api_provider_sdk::PluginCtx, flow: &str) -> proto::LoginPoll {
    // 先在锁内取快照并推进端口状态，网络动作放锁外。
    let (debug_port, proxy, gateway_proxy, expired) = with_flows(|table| {
        let Some(entry) = table.get_mut(flow) else {
            return (None, None, String::new(), true);
        };
        if entry.debug_port.is_none() {
            let port_file = entry.profile.join("DevToolsActivePort");
            if let Ok(text) = std::fs::read_to_string(&port_file) {
                if let Some(first) = text.lines().next() {
                    if let Ok(port) = first.trim().parse::<u16>() {
                        entry.debug_port = Some(port);
                    }
                }
            }
        }
        let expired = now_ms() - entry.started_at > FLOW_TIMEOUT_MS;
        (
            entry.debug_port,
            entry.proxy.clone(),
            entry.gateway_proxy.clone(),
            expired,
        )
    });
    if expired {
        with_flows(|table| cleanup_flow(table, flow));
        return proto::LoginPoll::Failed {
            message: "登录等待超时（15 分钟），请重新发起".into(),
        };
    }
    let Some(port) = debug_port else {
        return proto::LoginPoll::Pending {
            hint: Some("正在启动受控浏览器…".into()),
        };
    };
    let Ok(cookies) = cdp_cookies(port).await else {
        return proto::LoginPoll::Pending {
            hint: Some("正在打开登录页…".into()),
        };
    };
    let has = |name: &str| cookies.iter().any(|(key, _)| key == name);
    if !has("BDUSS") {
        return proto::LoginPoll::Pending {
            hint: Some("等待登录百度账号…".into()),
        };
    }
    if !REQUIRED_COOKIES.iter().all(|name| has(name)) {
        return proto::LoginPoll::Pending {
            hint: Some("登录成功，请在浏览器里进入百度智能云控制台页面…".into()),
        };
    }
    // 抓取完成：拼 Cookie、现场换票验证，通过才建号。
    let cookie = cookie_header_of(cookies);
    let credentials = Credentials {
        name: String::new(),
        cookie: cookie.clone(),
    };
    let cache_key = client::cache_key_of(&cookie);
    if let Err(error) =
        client::fetch_token(&cache_key, &credentials, proxy.as_ref(), &gateway_proxy, true).await
    {
        with_flows(|table| cleanup_flow(table, flow));
        return proto::LoginPoll::Failed {
            message: error.message().to_string(),
        };
    }
    let secret = json!({ "cookie": cookie });
    let account = proto::NormalizedAccount {
        identity: accounts::identity(&secret),
        public: accounts::public(&secret),
        secret,
        desktop: false,
    };
    with_flows(|table| cleanup_flow(table, flow));
    let _ = ctx;
    proto::LoginPoll::Done { account }
}

/// 取消一轮登录（关弹窗 / 用户点取消）。
pub fn cancel(flow: &str) {
    with_flows(|table| cleanup_flow(table, flow));
}

/// 界面上的两种登录方式（网关 `auth.methods` 的 camelCase 契约）。
///
/// `localOnly` 用在浏览器抓取这条上：受控窗口开在网关所在电脑，远程部署
/// 时用户看不到它，如实申报而不是让远程用户白等。
pub fn login_methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 1,
                "type": "oauth",
                "label": "浏览器登录",
                "instructions": "插件会新开一个独立的浏览器窗口。请在新窗口里登录百度账号；\
                    登录后浏览器会跳到百度智能云控制台，插件自动抓取 Cookie 并完成添加。",
                "prompts": [],
                "localOnly": true,
                "manualCallback": false,
                "callbackInput": "url"
            },
            {
                "methodIndex": 2,
                "type": "api",
                "label": "粘贴 Cookie",
                "instructions": "打开 https://console.bce.baidu.com 并登录百度账号\
                    （www.dumate.cn 与其共享百度登录态，无需单独登录）；\
                    然后 F12 打开开发者工具 → 网络（Network）→ 刷新页面，\
                    找任意一条发往 dumate.cn 或 bce.baidu.com 的请求，\
                    复制请求头里的完整 Cookie（必须包含 BDUSS 与 bce-long-term-sessionid）。",
                "prompts": [
                    {
                        "key": "cookie",
                        "type": "text",
                        "label": "网页 Cookie",
                        "placeholder": "BDUSS=…; BAIDUID=…; …",
                        "secret": true,
                        "optional": false
                    },
                    {
                        "key": "name",
                        "type": "text",
                        "label": "账号备注名",
                        "placeholder": "可选，用于显示",
                        "optional": true
                    }
                ],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url"
            }
        ]
    })
}

