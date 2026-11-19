//! 科搭（Codar）上游端点与常量。
//!
//! 依据官方独立插件 `wasm/codar`（MIT）：业务接口在
//! `https://api.claw.speedai.com`，账号/积分/签到接口在
//! `https://api.kuaipaper.com`，鉴权统一 `Authorization: Bearer <JWT>`。
//! 管理接口沿 Node 标识、对话沿 Go 标识，是参考客户端核对过的取值。

/// provider id。
pub const PROVIDER_ID: &str = "codar";
/// 展示名。
pub const DISPLAY_NAME: &str = "科搭";
/// 业务接口（对话、模型目录、活动）。
pub const API_BASE: &str = "https://api.claw.speedai.com";
/// 账号接口（资料、积分、签到、续期）。
pub const ACCOUNT_BASE: &str = "https://api.kuaipaper.com";

/// 账号资料。
pub const PROFILE: &str = "/api/user/self";
/// 精选模型目录（带真实协议与请求 schema）。
pub const CATALOG: &str = "/v1/codar/models";
/// 令牌续期。
pub const REFRESH: &str = "/v1/token/refresh";
/// 通用点数。
pub const POINTS: &str = "/v1/gettoken";
/// Agent 点数。
pub const AGENT_POINTS: &str = "/v1/agent/balance";
/// 每日签到。
pub const SIGN: &str = "/v1/daily-sign";
/// 签到状态。
pub const SIGN_STATUS: &str = "/v1/daily-sign/status";

/// 微信扫码登录：申请二维码。
pub const QR: &str = "/v1/wechat/login/qrcode";
/// 微信扫码登录：查询扫码状态。
pub const QR_STATUS: &str = "/v1/wechat/login/status";
/// 登录短信验证码。
pub const SMS: &str = "/v1/verification-code";

/// 活动：免费模型活动状态。
pub const FREE: &str = "/api/codar/activities/free-model";
/// 活动：领取免费模型。
pub const FREE_CLAIM: &str = "/api/codar/activities/free-model/claim";
/// 活动：我的活动清单。
pub const ACTIVITIES: &str = "/api/codar/activities/mine";

/// 对话端点。
pub const CHAT: &str = "/v1/chat/completions";
/// Anthropic Messages 端点。
pub const ANTHROPIC: &str = "/v1/messages";
/// OpenAI Responses 端点。
pub const RESPONSES: &str = "/v1/responses";

/// 管理接口的 User-Agent（参考客户端为 Node 默认标识）。
pub const USER_AGENT: &str = "node";
/// 对话链路的 User-Agent（参考客户端为 Go 默认标识）。
pub const CHAT_USER_AGENT: &str = "Go-http-client/1.1";

/// 令牌提前续期的窗口（48 小时）。
pub const REFRESH_BEFORE_MS: i64 = 48 * 60 * 60 * 1000;
/// 续期去抖（5 秒）。
pub const REFRESH_DEBOUNCE_MS: i64 = 5_000;
