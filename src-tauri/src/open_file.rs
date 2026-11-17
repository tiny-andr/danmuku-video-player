// 从命令行参数打开视频。
//
// Windows 上没有 macOS 的 `RunEvent::Opened`：用户右键「打开方式」选本程序、或者
// 双击注册过关联的视频文件时，系统只是把文件路径塞进进程的第 1 个参数 —— 不自己
// 从 argv 里捞出来，程序就只会干开一个空窗口（这正是 1.2.8 之前的真实表现）。
//
// 两个入口必须分开处理，因为前端就绪状态不同：
//   - 首次启动：参数在我自己的 argv 里，但 webview 还没加载完 → 先存进 state，
//     前端初始化完之后 `invoke('take_open_file')` 来取。
//   - 已有实例在跑：`tauri-plugin-single-instance` 会把第二个进程的 argv 用
//     WM_COPYDATA 转发过来 → 这时前端早就在跑，所以除了存 state 再 emit 一条事件，
//     让界面立刻换片。
//
// 只认视频扩展名并且要求文件真实存在：用户可能把任意文件关联过来（字幕、弹幕、
// 甚至 txt），那些不该被当成视频丢给播放器。

use std::path::Path;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, Runtime, State};

/// 前端监听的事件名。带自定义 scheme，避免和 Tauri 内置事件撞名。
pub const OPEN_FILE_EVENT: &str = "bplayer://open-file";

/// 待打开的文件。首次启动时由 setup 写入，前端取一次就清空；
/// 第二实例转发时由回调写入（前端取不到也不会残留，下一次 take 会拿到它）。
#[derive(Default)]
pub struct PendingOpen(pub Mutex<Option<String>>);

/// 取一次待打开的文件（取完即清空）。
///
/// 锁中毒不能跟着 panic：持锁方只做「整体替换 Option」，不存在半个状态，
/// 所以取内部值继续用（与 pgs_render 的 lock_doc 同一套理由）。
#[tauri::command]
pub fn take_open_file(state: State<'_, PendingOpen>) -> Option<String> {
    state.0.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// 存下待打开的文件并通知前端。
///
/// emit 失败（前端还没监听）不算错误：state 里的值还在，前端启动后会
/// `take_open_file` 取走，两条路互为兜底。
pub fn stash<R: Runtime>(app: &AppHandle<R>, path: String) {
    let state = app.state::<PendingOpen>();
    *state.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
    let _ = app.emit(OPEN_FILE_EVENT, path);
}

/// 这个参数看起来是要打开的视频吗：扩展名在白名单里，且文件确实存在。
fn is_openable(arg: &str) -> bool {
    let path = Path::new(arg);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // 扩展名白名单与「打开视频」对话框、目录扫描共用一份，不另立第二套
    crate::VIDEO_EXTS.contains(&ext.as_str()) && path.is_file()
}

/// 从完整 argv 里挑出要打开的视频（argv[0] 是程序自身，跳过）。
///
/// 约定：
/// - 第一个满足「视频扩展名 + 文件存在」的参数胜出，后面的一律忽略。
/// - `--open <path>` 是显式写法，方便命令行调试；`<path>` 前面挂别的开关也不影响。
/// - `-` / `--` 开头的参数当成开关跳过（Tauri / WebView2 会塞一些自己的命令行参数）。
pub fn video_from_args<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut iter = args.into_iter();
    let _exe = iter.next(); // argv[0] 永远是程序路径
    let mut expect_path = false;
    for arg in iter {
        if expect_path {
            expect_path = false;
            if is_openable(&arg) {
                return Some(arg);
            }
            continue;
        }
        if arg == "--open" {
            expect_path = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if is_openable(&arg) {
            return Some(arg);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立目录，避免并行跑互相踩（与 lib.rs 的 scratch 同一套路）。
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bplayer-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p.to_string_lossy().to_string()
    }

    /// Windows 用「打开方式」选本程序时，传进来的就是 [exe, 文件路径]。
    #[test]
    fn picks_the_only_video_argument() {
        let dir = scratch("only");
        let video = touch(&dir, "movie.mkv");
        let got = video_from_args(vec!["C:\\app\\player.exe".to_string(), video.clone()]);
        assert_eq!(got, Some(video));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 大小写不敏感：Windows 上 .MKV / .Mkv 都常见。
    #[test]
    fn extension_match_is_case_insensitive() {
        let dir = scratch("case");
        let video = touch(&dir, "MOVIE.MP4");
        let got = video_from_args(vec!["player.exe".to_string(), video.clone()]);
        assert_eq!(got, Some(video));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 程序自己那一个参数不能被当成待打开文件（否则有同名的 exe 就会误开）。
    #[test]
    fn exe_itself_is_ignored() {
        assert_eq!(video_from_args(vec!["C:\\app\\player.mp4".to_string()]), None);
    }

    /// 没有参数（正常双击图标）什么都不打开。
    #[test]
    fn no_arguments_opens_nothing() {
        assert_eq!(video_from_args(Vec::<String>::new()), None);
        assert_eq!(video_from_args(vec!["player.exe".to_string()]), None);
    }

    /// 非视频扩展名一律忽略：用户可能把字幕/弹幕/任意文件关联过来。
    #[test]
    fn non_video_extensions_are_ignored() {
        let dir = scratch("nonvideo");
        let srt = touch(&dir, "movie.srt");
        let txt = touch(&dir, "notes.txt");
        assert_eq!(video_from_args(vec!["p.exe".to_string(), srt]), None);
        assert_eq!(video_from_args(vec!["p.exe".to_string(), txt]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在就不打开：Windows 有时会把快捷方式的 argv 传成陈旧路径。
    #[test]
    fn missing_file_is_ignored() {
        let ghost = std::env::temp_dir().join("bplayer-open-ghost-does-not-exist.mkv");
        let _ = std::fs::remove_file(&ghost);
        assert_eq!(
            video_from_args(vec!["p.exe".to_string(), ghost.to_string_lossy().to_string()]),
            None
        );
    }

    /// 开关参数要跳过，不能被当成路径（WebView2 / Tauri 会塞自己的开关）。
    #[test]
    fn switches_are_skipped() {
        let dir = scratch("switch");
        let video = touch(&dir, "movie.mp4");
        let got = video_from_args(vec![
            "p.exe".to_string(),
            "--disable-gpu".to_string(),
            "-foo".to_string(),
            video.clone(),
        ]);
        assert_eq!(got, Some(video));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--open <path>` 显式写法（给命令行调试用）。
    #[test]
    fn explicit_open_flag_is_honoured() {
        let dir = scratch("openflag");
        let video = touch(&dir, "movie.webm");
        let got = video_from_args(vec!["p.exe".to_string(), "--open".to_string(), video.clone()]);
        assert_eq!(got, Some(video));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 多个视频参数时取第一个 —— 「打开方式」只会传一个，多出来的算用户误操作。
    #[test]
    fn first_video_wins() {
        let dir = scratch("first");
        let a = touch(&dir, "a.mp4");
        let b = touch(&dir, "b.mp4");
        let got = video_from_args(vec!["p.exe".to_string(), a.clone(), b]);
        assert_eq!(got, Some(a));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
