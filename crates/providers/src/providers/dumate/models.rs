//! 百度搭子的模型目录：静态表（上游没有模型目录接口）。
//!
//! ── 名字从哪来 ──────────────────────────────────────────────
//! 上游 `/models` 404、`/model/list` 400（2026-10-21 实测排除），这里全部
//! 登记参考实现逐个真机探测过的路由名（2026-11-06，`max_tokens=1` 探针 +
//! `X-Dumate-Gw-Realmodel` 响应头确认）。注意这些名字是**应用路由名**，
//! 不是真实后端模型名。
//!
//! ── 限额口径 ────────────────────────────────────────────────
//! 上下文与输出上限只填有实测依据的值（上游 400 错误体明示或实测通过），
//! 没实测的留空由网关走默认，不编造。

use serde_json::{json, Value};

/// 静态清单（参考实现 2026-11-06 实测快照）。
pub fn fallback_models() -> Vec<Value> {
    let rows: [(&str, &str, Option<i64>, Option<i64>, bool); 13] = [
        // 应用路由名, 实跑后端, 输入上限, 输出上限, 视觉
        (
            "model-text",
            "deepseek-v4.1-flash-low-latency（上游默认档）",
            Some(1_048_576),
            Some(393_216),
            false,
        ),
        ("kimi-k31", "kimi-k3.1-pro", None, Some(2_000_000), false),
        ("glm-53", "glm-5.3", None, None, false),
        ("glm-53-flash", "glm-5.3-flash", None, None, false),
        ("deepseek-v4-pro", "deepseek-v4-pro-0813", None, None, false),
        (
            "deepseek-v41-flash",
            "deepseek-v4.1-flash-low-latency",
            Some(1_048_576),
            Some(393_216),
            false,
        ),
        ("qwen38-flash", "qwen3.8-flash", None, None, false),
        (
            "dm-auto-model/text.L0",
            "deepseek-v4.1-flash-low-latency（自动档）",
            Some(1_048_576),
            Some(393_216),
            false,
        ),
        (
            "dm-auto-model/text.L1",
            "kimi-k3（自动档）",
            None,
            Some(2_000_000),
            false,
        ),
        (
            "dm-auto-model/text.L2",
            "qwen3.8-max（自动档）",
            Some(983_616),
            Some(131_072),
            false,
        ),
        (
            "model-mm",
            "deepseek-v4.1-flash-low-latency（视觉）",
            Some(1_048_576),
            Some(393_216),
            true,
        ),
        (
            "dm-auto-model/mm.L0",
            "deepseek-v4.1-flash-low-latency（视觉自动档）",
            Some(1_048_576),
            Some(393_216),
            true,
        ),
        (
            "dm-auto-model/mm.L2",
            "qwen3.8-max（视觉自动档）",
            Some(983_616),
            Some(131_072),
            true,
        ),
    ];
    let mut models: Vec<Value> = rows
        .iter()
        .map(|(id, name, input, output, images)| {
            let mut record = json!({
                "id": id,
                "name": name,
                "supportsToolCall": true,
                "isDefault": *id == "model-text",
            });
            if let Some(value) = input {
                record["maxInputTokens"] = json!(value);
            }
            if let Some(value) = output {
                record["maxOutputTokens"] = json!(value);
            }
            if *images {
                record["supportsImages"] = json!(true);
            }
            record
        })
        .collect();
    // 千帆原生命名空间：另一个端点（/gateway/v2），模型名必须写千帆原生名。
    // 上下文与输出上限来自上游 400 错误体的实测声明。
    models.push(json!({
        "id": "qianfan/qwen3.5-35b-a3b",
        "name": "qwen3.5-35b-a3b（千帆原生）",
        "maxInputTokens": 258_048,
        "maxOutputTokens": 65_536,
        "supportsToolCall": true,
    }));
    models
}

