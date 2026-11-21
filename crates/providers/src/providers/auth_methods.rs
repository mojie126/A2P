//! 原生插件共用的动态登录方式（`auth.methods`）构造。
//!
//! 网关已不再回退旧版静态登录入口：原生插件必须在 `admin_action` 里回答
//! `auth.methods`，否则「添加账号」会报「该提供商未声明动态登录方式」。
//! 各家的静态声明（[`ProviderMeta::login_methods`]）已经表达「有哪几种登录」，
//! 这里把它翻成网关要的元数据形状：
//!   · Browser → oauth（应用内窗口由插件在 `login_start` 里声明打开方式）
//!   · Token → api（主输入键由调用方给出，与 normalize 读取的字段一致）
//!   · Desktop → desktop（本机导入，网关侧转成 `importDesktop`）
//!   · Sms 不在这里生成 —— 它需要各家自己的区号与长度配置，由需要的家自行声明
//!
//! 方法顺序固定为「网页登录 → 填写令牌 → 导入本机登录态」：部分插件的
//! `login_start` 用 methodIndex 0 认浏览器登录，网页方式必须排在最前。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ProviderMeta;
use serde_json::{json, Value};

/// 构造参数（每个家按自己的凭证字段与登录窗口约束填写）。
pub struct Options<'a> {
    /// Token 方式的主输入键，必须与这家 normalize 读取的字段一致
    pub primary: &'a str,
    /// 网页登录必须在应用内窗口完成（Cookie 捕获、自定义协议回调这类）
    pub embedded_browser: bool,
    /// 网页登录允许远程面板粘贴本次完整回调地址
    pub manual_callback: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self { primary: "token", embedded_browser: false, manual_callback: false }
    }
}

fn prompt(key: &str, label: &str, placeholder: &str, secret: bool, optional: bool) -> Value {
    json!({
        "key": key,
        "type": "text",
        "label": label,
        "placeholder": placeholder,
        "secret": secret,
        "optional": optional,
    })
}

/// 把静态登录声明翻成动态登录方式。
pub fn from_static(meta: &ProviderMeta, options: &Options) -> Value {
    let mut methods = Vec::new();
    let mut index = 0u64;

    for method in &meta.login_methods {
        if let proto::LoginMethod::Browser { button, hint } = method {
            methods.push(json!({
                "methodIndex": index,
                "type": "oauth",
                "label": button,
                "instructions": hint.clone().unwrap_or_default(),
                "prompts": [],
                "browserMode": "choice",
                "manualCallback": options.manual_callback,
                "callbackInput": "url",
                "embeddedOnly": options.embedded_browser,
                "desktopOnly": options.embedded_browser,
            }));
            index += 1;
        }
    }

    for method in &meta.login_methods {
        if let proto::LoginMethod::Token { label, placeholder, extra_fields } = method {
            let mut prompts = vec![prompt(
                options.primary,
                if label.is_empty() { options.primary } else { label },
                placeholder,
                true,
                false,
            )];
            for field in extra_fields {
                prompts.push(prompt(
                    &field.key,
                    &field.label,
                    &field.placeholder,
                    false,
                    field.optional,
                ));
            }
            // 备注名：多数家的 normalize 都认 name，界面也一直有这一格
            if !extra_fields.iter().any(|field| field.key == "name") {
                prompts.push(prompt("name", "备注名", "可选，用于显示", false, true));
            }
            methods.push(json!({
                "methodIndex": index,
                "type": "api",
                "label": label,
                "prompts": prompts,
            }));
            index += 1;
        }
    }

    for method in &meta.login_methods {
        if let proto::LoginMethod::Desktop { button, hint } = method {
            methods.push(json!({
                "methodIndex": index,
                "type": "desktop",
                "label": button,
                "instructions": hint.clone().unwrap_or_default(),
                "prompts": [],
                "localOnly": true,
                "browserMode": "none",
                "manualCallback": false,
                "callbackInput": "url",
            }));
            index += 1;
        }
    }

    json!({ "methods": methods })
}
