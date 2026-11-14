use agent2api_provider_protocol as proto;

use crate::provider::BoxProvider;

#[derive(Clone, Copy, Default)]
pub struct ServeOptions {
    /// 在握手应答前初始化机器级资源；失败时拒绝握手。
    pub on_init: Option<fn(&proto::HostEnv) -> std::io::Result<()>>,
    /// 仅申报已在所有出站请求中应用账号 UA 的提供商。
    pub account_user_agent: Option<fn(&str) -> bool>,
}

/// 安装插件的 panic 钩子：把 panic 位置与载荷落到 stderr。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// release 是 `panic = "abort"`，默认 panic 输出虽然也走 stderr，但网关
/// 只保留退出诊断的 stderr 摘要，缺省栈信息并不带任务上下文。钩子在
/// 第一时间打印一条单行的位置与载荷摘要，第二重 panic（载荷 drop 再
/// panic）同样会经过钩子，两行都能被网关的 stderr 摘要捕获，为崩溃
/// 排查保留现场。
pub fn install_panic_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}:{}", location.file(), location.line(), location.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        eprintln!("provider host: panic at {location}: {payload}");
        default_hook(info);
    }));
}

pub fn serve_blocking(providers: Vec<BoxProvider>) {
    serve_blocking_with(providers, ServeOptions::default());
}

pub fn serve_blocking_with(providers: Vec<BoxProvider>, options: ServeOptions) {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("provider host: 建 tokio 运行时失败: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = runtime.block_on(crate::serve::serve_with(providers, options)) {
        eprintln!("provider host: 主循环异常退出: {e}");
        std::process::exit(1);
    }
}
