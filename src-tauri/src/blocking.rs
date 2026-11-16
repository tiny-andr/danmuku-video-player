// 把阻塞的 IO / 子进程等待挪出命令通道。
//
// Tauri 2 的同步命令**不会**冻住窗口消息循环（实测：一条 3 秒的 SMB `fs_stat` 期间，
// 窗口 Responding 一直为 true）。真正的代价是**所有 invoke 共用同一条串行通道** ——
// 同一时刻实测，慢命令耗时 2997ms，并发发起的本地快命令也是 2997ms 才返回。
// 也就是说一条 SMB 断连的 `fs_stat` 会把之后所有命令（挂字幕、探音轨、刷进度）堵在
// 队列里几十秒，用户看到的是"点了没反应、菜单不刷新"。
//
// 官方文档（v2.tauri.app/develop/calling-rust，2026-11 核对）要求这类命令声明成
// `async`，让出通道。但只写 `async` 还不够：async fn 的函数体跑在 tokio worker 上，
// 里面的阻塞调用仍会饿死异步运行时。真正的阻塞段必须交给 spawn_blocking 的专用线程池
// —— 这个模块就是那层薄壳，形态与 `pgs_render::pgs_open` 已有的写法一致。

/// 在阻塞线程池里执行 `f`。
///
/// 返回 `Result<T, String>` 而不是 `T`：`spawn_blocking` 在任务 panic 或运行时关闭时
/// 才会失败，所以调用方通常用 `unwrap_or` / `unwrap_or_else` 给一个安全的兜底值，
/// 保持命令原本"永不 reject"的语义。
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务异常退出: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真的跑到另一个线程上：只靠 async 的话，阻塞调用会占着 tokio worker。
    #[test]
    fn runs_off_thread_and_returns_value() {
        let caller = std::thread::current().id();
        let (worker, value) = tauri::async_runtime::block_on(blocking(move || {
            (std::thread::current().id(), 42u32)
        }))
        .unwrap();
        assert_eq!(value, 42);
        assert_ne!(worker, caller, "阻塞任务应该跑在 spawn_blocking 线程池里");
    }
}
