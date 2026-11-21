//! 小浣熊的登录方式声明与本机登录态读取（原生插件入口）
//!
//! 网关已不再回退旧版静态入口，原生插件必须自己回答 `auth.methods`，否则
//! 添加账号会报「该提供商未声明动态登录方式」。这里声明四种方式：
//!   · 网页登录：应用内窗口完成登录，壳侧捕获 `office-raccoon://` 回调后换码
//!   · 填写令牌：accessToken / refreshToken / officeIdentity
//!   · 导入登录态 JSON：整份 auth.json 粘进来
//!   · 读取本机客户端登录态：只读网关所在电脑上的 auth.json，不写回
//!
//! `type: desktop` 只对 WASM 包要求声明 `desktop.credentials.v1`，原生插件可
//! 直接使用；这里仍用 `oauth + localOnly` 表达本机读取（与科搭同一口径）：
//! 登录启动时插件自己读文件并把结果放进流程，轮询一次即可 Done

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::oauth;

/// 本机登录流程的有效期：读取是本地动作，留足界面轮询的余量即可
const DESKTOP_FLOW_TTL_MS: i64 = 10 * 60 * 1000;
/// 本机登录的轮询间隔（网关在 `poll_interval_ms > 0` 时才起轮询任务）
const POLL_INTERVAL_MS: u64 = 500;

struct Flow {
    deadline: i64,
    outcome: Result<Box<proto::NormalizedAccount>, String>,
}

static FLOWS: LazyLock<Mutex<HashMap<String, Flow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登录方式面板
///
/// 字段名与取值必须对齐网关的 `LoginMethodMetadata`：`type` 只能是
/// `oauth` / `api` / `sms` / `desktop`，提示项类型只能是 `text` / `select`
/// 本机读取不能用 `api + localOnly`（网关拒绝）；`desktop` 虽已对原生插件开放，
/// 但这里沿用既有的读取后轮询形态，因此走 `oauth + localOnly`
pub fn methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 0,
                "type": "oauth",
                "label": "网页登录",
                "instructions": "在桌面应用内打开的小浣熊官方页面完成登录，网关会自动接回并添加账号，无需粘贴回调地址",
                "prompts": [],
                "manualCallback": false,
                "callbackInput": "url",
                "validatesCallback": true,
                "embeddedOnly": true,
                "desktopOnly": true,
                "browserMode": "choice"
            },
            {
                "methodIndex": 1,
                "type": "api",
                "label": "填写令牌",
                "instructions": "粘贴小浣熊的访问令牌；填写刷新令牌后到期可自动续期",
                "prompts": [
                    { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true, "optional": false },
                    { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
                    { "key": "officeIdentity", "type": "text", "label": "Office Identity", "optional": true }
                ]
            },
            {
                "methodIndex": 2,
                "type": "api",
                "label": "导入登录态 JSON",
                "instructions": "把小浣熊客户端登录态的 auth.json 内容整份粘贴进来",
                "prompts": [
                    { "key": "credentialJson", "type": "text", "label": "登录态 JSON", "secret": true, "optional": false }
                ]
            },
            {
                "methodIndex": 3,
                "type": "oauth",
                "label": "读取本机小浣熊客户端登录态",
                "instructions": "从网关所在电脑的小浣熊客户端登录态文件读取凭证（只读，不写回）",
                "prompts": [],
                "localOnly": true,
                "manualCallback": false,
                "callbackInput": "url"
            }
        ]
    })
}

/// 网页登录的主机白名单，涵盖官方登录页与其跳转所需的资源域
pub const BROWSER_ALLOWED_HOSTS: &[&str] = &[
    "xiaohuanxiong.com",
    "sensetime.com",
    "aliyun.com",
    "aliyuncs.com",
    "alicdn.com",
    "qq.com",
    "baidu.com",
];

/// 发起本机登录态读取：读文件、归一账号，结果放进一次性流程
///
/// 失败也写成流程结果而不是直接报错，让界面在轮询里拿到可读原因
/// （与科搭本机关联同一形态）
pub fn start_desktop_flow(
    account: Result<proto::NormalizedAccount, String>,
) -> proto::LoginStart {
    let flow = oauth::new_login_state();
    let outcome = account.map(Box::new);
    if let Ok(mut flows) = FLOWS.lock() {
        let now = crate::logging::now_ms();
        flows.retain(|_, entry| entry.deadline > now);
        flows.insert(flow.clone(), Flow {
            deadline: now + DESKTOP_FLOW_TTL_MS,
            outcome,
        });
    }
    proto::LoginStart {
        flow,
        url: String::new(),
        instructions: "正在读取本机小浣熊客户端登录态".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        external_browser: false,
        allowed_hosts: Vec::new(),
    }
}

/// 查询一次本机登录流程的结果
pub fn poll(flow: &str) -> proto::LoginPoll {
    let entry = {
        let flows = match FLOWS.lock() {
            Ok(flows) => flows,
            Err(poisoned) => poisoned.into_inner(),
        };
        flows.get(flow).map(|entry| (entry.deadline, entry.outcome.clone()))
    };
    let Some((deadline, outcome)) = entry else {
        return proto::LoginPoll::Failed {
            message: "小浣熊登录流程不存在或已结束，请重新发起".to_string(),
        };
    };
    if deadline <= crate::logging::now_ms() {
        if let Ok(mut flows) = FLOWS.lock() {
            flows.remove(flow);
        }
        return proto::LoginPoll::Failed {
            message: "小浣熊登录已过期，请重新发起".to_string(),
        };
    }
    match outcome {
        Ok(account) => proto::LoginPoll::Done { account: *account },
        Err(message) => proto::LoginPoll::Failed { message },
    }
}

/// 取消本机登录流程
pub fn cancel(flow: &str) {
    if let Ok(mut flows) = FLOWS.lock() {
        flows.remove(flow);
    }
}
