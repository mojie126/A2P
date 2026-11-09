//! 小浣熊桌面客户端 1.1.9 的对话请求指纹。
//!
//! 上游按客户端形态分配额度与限流档位：只带半套指纹的请求（改了 UA 却没有
//! `X-Stainless-*`，或平台头写 Linux 而 SDK 指纹写 Windows）是官方客户端
//! 发不出来的形态，更容易被归到低配额档。因此这里的平台与架构都是
//! **声明值**——描述的是客户端形态，不是宿主系统：插件跑在 Linux 容器里
//! 也必须声明 Windows 桌面端，绝不能用 `cfg!(target_os)` 或
//! `std::env::consts` 探测宿主。
//!
//! 官方 1.1.9 的对话由内置 Python Agent（`box-agent-acp`，openai Python
//! SDK 2.8.0 + httpx）发出，头部形态的官方出处：
//!   - SDK 指纹：`openai/_base_client.py` 的 `properties` 与
//!     `platform.python_version()`；
//!   - 桌面客户端标识：`box_agent/client_info.py::headers_for_url`。

use std::sync::OnceLock;

const CLIENT_NAME: &str = "@raccoon/office-desktop";
/// 官方 1.1.9 起对话链路发小写 `x-client-*`；大写形态属于网页 relay 链路，
/// 发出去等于自报非官方，控制面（余额 / IAM / 签到）才继续用大写。
const CLIENT_PLATFORM: &str = "desktop-windows-x64";
const CLIENT_VERSION: &str = "v1.1.9";
/// 非 Windows 平台取不到注册表时按官方常见 build 兜底，保证这条头恒发。
const OS_VERSION_BASELINE: &str = "10.0.22631";
const RACCOON_TITLE: &str = "codex";
const RACCOON_CALL_KIND: &str = "main";
const SDK_VERSION: &str = "2.8.0";
/// 对齐官方 `timeout: 1200.0` 的恒定声明值，不带小数点会被识别为整型形态。
const READ_TIMEOUT: &str = "1200.0";
const PYTHON_VERSION: &str = "3.12.13";

/// SDK 指纹段（User-Agent 与 6 条 `X-Stainless-*`）。
pub(super) fn sdk_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_string(),
            format!("AsyncOpenAI/Python {SDK_VERSION}"),
        ),
        ("X-Stainless-Lang".to_string(), "python".to_string()),
        (
            "X-Stainless-Package-Version".to_string(),
            SDK_VERSION.to_string(),
        ),
        ("X-Stainless-OS".to_string(), "Windows".to_string()),
        ("X-Stainless-Arch".to_string(), "x64".to_string()),
        ("X-Stainless-Runtime".to_string(), "CPython".to_string()),
        (
            "X-Stainless-Runtime-Version".to_string(),
            PYTHON_VERSION.to_string(),
        ),
    ]
}

/// 对话身份段：从 `X-Stainless-Async` 到 `X-Stainless-Read-Timeout`。
///
/// `org_code` 是组织码：个人 / 空 / 非法都**整条不发**（不是发空值），
/// 非法值在对话链路静默丢弃，不返回错误。
pub(super) fn rest_headers(
    session_id: &str,
    turn_id: &str,
    org_code: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("X-Stainless-Async".to_string(), "async:asyncio".to_string()),
        ("X-RACCOON-Session-ID".to_string(), session_id.to_string()),
        ("X-RACCOON-Turn-ID".to_string(), turn_id.to_string()),
        ("X-RACCOON-Title".to_string(), RACCOON_TITLE.to_string()),
        (
            "X-RACCOON-Call-Kind".to_string(),
            RACCOON_CALL_KIND.to_string(),
        ),
        ("x-client-name".to_string(), CLIENT_NAME.to_string()),
        ("x-client-platform".to_string(), CLIENT_PLATFORM.to_string()),
        ("x-client-version".to_string(), CLIENT_VERSION.to_string()),
        ("x-client-os-version".to_string(), os_version()),
        ("x-client-channel".to_string(), "official".to_string()),
        ("X-Stainless-Retry-Count".to_string(), "0".to_string()),
        ("X-Stainless-Read-Timeout".to_string(), READ_TIMEOUT.to_string()),
    ];
    // `officeIdentity` 的个人身份哨兵值是 "personal"，与空值一样不发。
    if let Some(org) = org_code
        .filter(|value| !value.is_empty() && *value != "personal" && clean(value).is_some())
    {
        headers.insert(10, ("X-Org-Code".to_string(), org.to_string()));
    }
    headers
}

/// 值必须 trim 后非空、纯可见 ASCII、长度不超过 256，不满足整条丢弃。
fn clean(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return None;
    }
    Some(value.to_string())
}

/// Windows 从注册表读 `CurrentBuildNumber` 拼 `10.0.<build>`，进程内只读一次；
/// 其余平台没有对应产物形态，直接用基线值（声明的是 Windows 客户端形态）。
fn os_version() -> String {
    static OS_VERSION: OnceLock<String> = OnceLock::new();
    OS_VERSION
        .get_or_init(|| {
            #[cfg(windows)]
            {
                windows_build_number().unwrap_or_else(|| OS_VERSION_BASELINE.to_string())
            }
            #[cfg(not(windows))]
            {
                OS_VERSION_BASELINE.to_string()
            }
        })
        .clone()
}

#[cfg(windows)]
fn windows_build_number() -> Option<String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let mut buffer = [0u16; 64];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let build = String::from_utf16_lossy(&buffer[..length]);
    build
        .trim()
        .parse::<u32>()
        .ok()
        .map(|build| format!("10.0.{build}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方 1.1.9 的头顺序（含组织码的完整形态）。
    const ORDER: &[&str] = &[
        "User-Agent",
        "X-Stainless-Lang",
        "X-Stainless-Package-Version",
        "X-Stainless-OS",
        "X-Stainless-Arch",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Async",
        "X-RACCOON-Session-ID",
        "X-RACCOON-Turn-ID",
        "X-RACCOON-Title",
        "X-RACCOON-Call-Kind",
        "x-client-name",
        "x-client-platform",
        "x-client-version",
        "x-client-os-version",
        "x-client-channel",
        "X-Org-Code",
        "X-Stainless-Retry-Count",
        "X-Stainless-Read-Timeout",
    ];

    fn rest_names(org: Option<&str>) -> Vec<String> {
        rest_headers("sess-0-0123abcd", "turn", org)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn sdk_headers_carry_the_declared_fingerprint() {
        let headers = sdk_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(&names[..], &ORDER[..7]);
        let values: Vec<&str> = headers.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(values[0], "AsyncOpenAI/Python 2.8.0");
        assert_eq!(values[3], "Windows");
        assert_eq!(values[4], "x64");
        assert_eq!(values[6], "3.12.13");
    }

    #[test]
    fn rest_headers_follow_the_official_order_and_values() {
        let names = rest_names(Some("ORG123"));
        let expected: Vec<&str> = ORDER[7..].to_vec();
        assert_eq!(names, expected);
        let headers = rest_headers("sess-0-0123abcd", "turn", Some("ORG123"));
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap()
        };
        assert_eq!(find("X-Stainless-Async"), "async:asyncio");
        assert_eq!(find("X-RACCOON-Session-ID"), "sess-0-0123abcd");
        assert_eq!(find("X-RACCOON-Title"), "codex");
        assert_eq!(find("X-RACCOON-Call-Kind"), "main");
        assert_eq!(find("x-client-name"), "@raccoon/office-desktop");
        assert_eq!(find("x-client-platform"), "desktop-windows-x64");
        assert_eq!(find("x-client-version"), "v1.1.9");
        assert_eq!(find("x-client-channel"), "official");
        assert_eq!(find("X-Org-Code"), "ORG123");
        assert_eq!(find("X-Stainless-Retry-Count"), "0");
        assert_eq!(find("X-Stainless-Read-Timeout"), "1200.0");
        assert!(find("x-client-os-version").starts_with("10.0."));
    }

    #[test]
    fn personal_identity_omits_the_org_header() {
        assert!(!rest_names(None).contains(&"X-Org-Code".to_string()));
        assert!(!rest_names(Some("personal")).contains(&"X-Org-Code".to_string()));
        // 含控制字符的非法组织码整条丢弃，不返回错误。
        assert!(!rest_names(Some("ORG\n123")).contains(&"X-Org-Code".to_string()));
    }

    #[test]
    fn chat_link_never_sends_relay_or_forbidden_headers() {
        let names = rest_names(Some("ORG123"));
        for forbidden in [
            "X-Request-Id",
            "X-Client-Name",
            "X-Client-Platform",
            "X-Client-Version",
            "X-Client-Page",
            "X-Client-Device-ID",
            "X-Stainless-Raw-Response",
            "X-Stainless-Helper-Method",
        ] {
            assert!(!names.contains(&forbidden.to_string()));
        }
        let headers = rest_headers("sess-0-0123abcd", "turn", Some("ORG123"));
        let values: Vec<&str> = headers
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        assert!(!values.contains(&"raccoon-work"));
        assert!(!values.contains(&"desktop-windows"));
        assert!(!values.contains(&"desktop-linux-x64"));
    }

    #[test]
    fn fingerprint_values_stay_consistent() {
        let sdk = sdk_headers();
        let package_version = sdk
            .iter()
            .find(|(name, _)| name == "X-Stainless-Package-Version")
            .map(|(_, value)| value.as_str())
            .unwrap();
        assert!(sdk.iter().any(|(name, value)| name == "User-Agent"
            && value.ends_with(package_version)));
    }

    #[test]
    fn declared_values_are_http_safe() {
        for (name, value) in sdk_headers()
            .into_iter()
            .chain(rest_headers("sess-0-0123abcd", "turn", Some("ORG123")))
        {
            assert!(!name.is_empty() && name.len() <= 256);
            assert!(
                value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
                "{name} 的值含不可见字符: {value}"
            );
        }
    }
}
