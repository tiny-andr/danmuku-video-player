// 单实例闸门。
//
// 不加这层时，两个实例可以同时给同一部片转码：各自起一个 ffmpeg 往同一个
// `(音轨N).tmp.mka` 里写，谁后完成谁把半成品改名为成品，另一个实例的前端却以为
// 拿到的是自己那单的结果；关窗时 A 的 cancel_all 也杀不到 B 的 ffmpeg（各进程
// 各有自己的 Job Object）。用 Windows 命名互斥量在启动最早期拦住第二个实例，
// 比事后再做文件锁冲突检测便宜得多。

/// `Local\` 前缀：只在当前登录会话内唯一。换成 `Global\` 会让「快速切换用户」和
/// 远程会话里同一台机器上的另一个人也打不开播放器，而且那个命名空间通常还要
/// 额外的创建权限。
#[cfg(windows)]
const MUTEX_NAME: &str = "Local\\danmuku-video-player-single-instance";

/// 互斥量句柄必须活到进程结束。故意不 CloseHandle：句柄一关互斥量就没了，
/// 第二个实例就能重新进来；进程退出时内核会统一回收，所以这里只存不用。
#[cfg(windows)]
static MUTEX_HANDLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 已有实例在跑就返回 true。
///
/// 判断依据只能是 `GetLastError() == ERROR_ALREADY_EXISTS`：对已存在的同名互斥量，
/// `CreateMutexW` 照样返回一个**有效句柄**（不是 null），只看句柄会永远判成"没在跑"。
#[cfg(windows)]
fn already_running(name: &str) -> bool {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // 先清 LastError：它是线程级的，可能带着别的 Win32 调用留下的旧值；
    // 不清的话"碰巧旧值就是 ERROR_ALREADY_EXISTS"会让首次启动被误判成第二个实例。
    unsafe { SetLastError(0) };
    // bInitialOwner = 0：不抢归属权，只要"是否存在"这一个信号。
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        // 建不出来（权限/句柄耗尽）时放行：宁可退化成两个实例，也不能让播放器打不开。
        return false;
    }
    let mutex_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // 句柄存进 static 只是为了让它活到进程结束，不参与后续判断。
    MUTEX_HANDLE.store(handle as usize, Ordering::SeqCst);
    mutex_exists
}

/// 恰好在 `run()` 的第一行调用。
///
/// 第二个实例静默 `exit(0)`：不弹框、不写日志 —— 用户双击两次图标不该看到任何
/// 报错。把已有窗口提到前台需要跨进程 IPC，这里不做，保持最小实现。
pub fn acquire_or_exit() {
    #[cfg(windows)]
    if already_running(MUTEX_NAME) {
        std::process::exit(0);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 只有第二次能读到 ERROR_ALREADY_EXISTS。名字带 pid：测试进程自己跑两遍
    /// （或机器上真开着播放器）都不会互相干扰。
    #[test]
    fn second_acquire_reports_already_running() {
        let name = format!("Local\\bplayer-single-instance-test-{}", std::process::id());
        assert!(!already_running(&name), "首次创建不该被判成已运行");
        assert!(already_running(&name), "同名互斥量已存在时必须被判成已运行");
    }

    /// 不同名字之间不能互相影响（否则名字写错一个字符就变成"永远起不来第二个程序"）。
    #[test]
    fn different_names_are_independent() {
        let a = format!("Local\\bplayer-single-test-a-{}", std::process::id());
        let b = format!("Local\\bplayer-single-test-b-{}", std::process::id());
        assert!(!already_running(&a));
        assert!(!already_running(&b));
    }
}
