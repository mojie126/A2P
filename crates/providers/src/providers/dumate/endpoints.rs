//! 百度搭子（DuMate）的地址与常量。
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考实现：`wearetheone777/dumate2api`（MIT，2026-11 实测记录）。
//!
//! ```text
//!   换票      GET  https://www.dumate.cn/api/dumate/app/token/get
//!             （Cookie 鉴权 → {result:{token, expired_at}}，令牌约 1 小时）
//!   对话      POST https://dumate-svc.baidu.com/gateway/apis/v1/chat/completions
//!             （OpenAI Chat 兼容 SSE，Bearer 令牌）
//!   千帆原生  POST https://dumate-svc.baidu.com/gateway/v2/chat/completions
//!             （模型名必须写千帆原生名，支持 response_format）
//!   余额      GET  https://www.dumate.cn/api/dumate/points/quota_overview
//!   签到信息  GET  https://www.dumate.cn/api/dumate/points/loginBonusInfo
//!   每日签到  POST https://www.dumate.cn/api/dumate/points/loginBonus
//! ```
//!
//! 上游没有模型目录接口（`/models` 404、`/model/list` 400，2026-10-21 与
//! 2026-10-31 两轮实测），模型清单只能静态登记实测过的路由名。

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = "dumate";

/// 网页端根（换票、余额、签到都在这里）。
pub const WEB_BASE: &str = "https://www.dumate.cn";

/// 模型网关根（对话在这里）。
pub const GATEWAY_BASE: &str = "https://dumate-svc.baidu.com";

/// 应用链路对话端点（OpenAI Chat 兼容）。
pub const CHAT_PATH: &str = "/gateway/apis/v1/chat/completions";

/// 千帆原生对话端点（模型名是千帆原生名，不认应用别名）。
pub const QIANFAN_CHAT_PATH: &str = "/gateway/v2/chat/completions";

/// Cookie 换 Bearer 令牌。
pub const TOKEN_GET_PATH: &str = "/api/dumate/app/token/get";

/// 积分余额总览（含订阅与加油包明细）。
pub const QUOTA_OVERVIEW_PATH: &str =
    "/api/dumate/points/quota_overview?clientType=desktop&timezone=Asia%2FShanghai";

/// 签到信息（当月已签日、是否已签）。
pub const LOGIN_BONUS_INFO_PATH: &str = "/api/dumate/points/loginBonusInfo";

/// 每日签到动作。
pub const LOGIN_BONUS_PATH: &str = "/api/dumate/points/loginBonus";

