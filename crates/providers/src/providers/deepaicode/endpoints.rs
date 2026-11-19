//! DeepAI Code 的上游端点与请求头常量。
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 协议事实取自参考实现 `2951461586/DeepAICode2API`（MIT，Go 网关）与
//! `aqiu817/magpie-deepaicode`（MIT，magpie 插件，附 PROTOCOL.md 实测记录）：
//!
//!   * 鉴权走 `Api-Key` 头，不是 `Authorization`；
//!   * 计费车道头 `X-DeepAI-Code-Theos`，当前默认 `4`，错值回 426；
//!   * 机器指纹头 `X-DeepAI-Code-Machine` = sha256(小写原始标识)，
//!     上游按它发放每日免费额度；
//!   * 无效 Key 会 302 跳登录页，所有请求都必须禁止跟随重定向。
//!
//! 直连地址 `https://api.deepai.org`，代码接口在 `/deepai_code` 之下，
//! 用量摘要 `/get_usage_summary` 在根路径。

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = "deepaicode";
/// 展示名。
pub const DISPLAY_NAME: &str = "DeepAI Code";
/// 默认上游地址。
pub const BASE_ORIGIN: &str = "https://api.deepai.org";

/// 模型目录。
pub const MODELS_PATH: &str = "/deepai_code/models";
/// 对话（OpenAI Chat 兼容）。
pub const CHAT_PATH: &str = "/deepai_code/chat/completions";
/// 对话（仅 `api = openai-responses` 的模型）。
pub const RESPONSES_PATH: &str = "/deepai_code/responses";
/// 联网搜索。
pub const WEB_SEARCH_PATH: &str = "/deepai_code/web_search";
/// 浏览器登录起点。
pub const SIGNIN_PATH: &str = "/deepai_code/desktop_auth/";
/// 浏览器登录换取 API Key。
pub const EXCHANGE_PATH: &str = "/deepai_code/desktop_auth/exchange/";
/// 免费额度 / 每日签到通道。
pub const THEOS_PATH: &str = "/deepai_code/theos";
/// 订阅与用量摘要（根路径）。
pub const USAGE_SUMMARY_PATH: &str = "/get_usage_summary";

/// 鉴权头。
pub const HEADER_KEY: &str = "Api-Key";
/// 计费车道版本。
pub const HEADER_LANE: &str = "X-DeepAI-Code-Theos";
/// 进程级实例号。
pub const HEADER_INSTANCE: &str = "X-DeepAI-Code-Instance";
/// 机器指纹。
pub const HEADER_MACHINE: &str = "X-DeepAI-Code-Machine";
/// 会话粘性。
pub const HEADER_AFFINITY: &str = "X-Session-Affinity";
/// 签到挑战回传（nonce）。
pub const HEADER_CHECKIN: &str = "X-DeepAI-Code-Check-In";
/// 客户端心跳（仅 `/theos` 使用）。
pub const HEADER_BEACON: &str = "X-DeepAI-Code-Beacon";

/// 插件自报的 User-Agent。
///
/// 上游 WAF 会拦 `user-agent: node`（Node 宿主默认值），必须显式带一个
/// 可辨识的 UA；不冒充桌面端。
pub const USER_AGENT: &str = "agent2api-deepaicode/0.1.0";
/// 桌面端客户端版本（心跳自报用，取自参考实现 0.3.41）。
pub const CLIENT_VERSION: &str = "0.3.41";

/// 默认车道版本（参考实现实测 `4` 受理）。
pub const DEFAULT_LANE: &str = "4";

/// 上游错误码。
pub const CODE_SESSION_BUSY: &str = "session_busy";
pub const CODE_LANE_UNSUPPORTED: &str = "lane_version_unsupported";
pub const CODE_SOCIAL_LOGIN: &str = "social_login_required";
pub const CODE_MACHINE_REQUIRED: &str = "machine_required";
pub const CODE_EMAIL_UNVERIFIED: &str = "email_unverified";
pub const CODE_CHECKIN_REFUSED: &str = "check_in_refused";
pub const CODE_THEOS_EXHAUSTED: &str = "theos_exhausted";
