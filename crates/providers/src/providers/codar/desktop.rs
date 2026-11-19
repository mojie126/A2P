//! 科搭本机关联：只读官方客户端 `~/.codar/config.json` 里的当前登录令牌。
//!
//! 上游客户端把当前账号写在 `provider.active_profile`（缺省 `speedai`）指向的
//! 档案里，令牌位于 `speedai.token`（JWT）。本模块只读该文件，不写回、
//! 不跟随链接，令牌本体不进入持久账号记录（由调用方决定保存策略）。
//!
//! `CODAR_HOME` 可覆盖根目录（必须是绝对路径），与官方 helper 口径一致。

use std::path::PathBuf;

use serde_json::Value;

/// 本机读取到的登录态。
pub struct DesktopLogin {
    pub token: String,
    pub username: String,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn root_dir() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("CODAR_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("CODAR_HOME 必须是绝对路径".to_string());
        }
        return Ok(path);
    }
    home_dir()
        .map(|home| home.join(".codar"))
        .ok_or_else(|| "无法确定用户主目录，读不到科搭客户端登录态".to_string())
}

/// 读取本机科搭客户端登录态。
pub fn read() -> Result<DesktopLogin, String> {
    let path = root_dir()?.join("config.json");
    let metadata = std::fs::metadata(&path).map_err(|_| {
        "没有找到科搭客户端配置（~/.codar/config.json），请先在官方客户端登录".to_string()
    })?;
    if !metadata.is_file() || metadata.len() > 256 * 1024 {
        return Err("科搭客户端配置不是可读文件或超过 256 KiB".to_string());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| "读取科搭客户端配置失败".to_string())?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| "科搭客户端配置不是有效 JSON".to_string())?;
    let provider = &value["provider"];
    let active = provider["active_profile"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .unwrap_or("speedai")
        .to_string();
    let profiles = provider["provider_profiles"]
        .as_object()
        .or_else(|| value["provider_profiles"].as_object());
    let selected = profiles
        .and_then(|profiles| profiles.get(&active))
        .unwrap_or(provider);
    let token = selected["speedai"]["token"]
        .as_str()
        .map(str::trim)
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 8192
                && token.split('.').count() == 3
                && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
        .ok_or_else(|| "科搭客户端配置里没有有效的访问令牌，请先在官方客户端登录".to_string())?
        .to_string();
    let username = super::credentials::claim_username(&token);
    if username.is_empty() {
        return Err("科搭客户端令牌缺少账号标识，请重新登录官方客户端".to_string());
    }
    Ok(DesktopLogin { token, username })
}
