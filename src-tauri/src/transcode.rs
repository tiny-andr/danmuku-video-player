// 后台媒体处理（ffmpeg 单任务模型）：
// 1) 外挂音轨——探测到内核解不了的音轨（DTS/TrueHD…）时，把指定音轨转成独立 AAC
//    音频文件（原基名 (音轨N).mka，落原视频同目录）；视频仍从原文件播放，由前端
//    audio-track 插件把 mka 与视频画面同步（原音轨本身无声，不会打架）。
// 2) 内嵌字幕抽取——把 MKV 里的文本字幕轨（S_TEXT/*）转出为 SRT 文件（原基名
//    (字幕N).srt），PGS 图形字幕不支持（内核无法渲染，不抽）。
// 两者完成前都写 *.tmp.*，成功后原子重命名——半成品永远不会被当成缓存命中。
//
// 单任务模型（代际号防串）：新任务启动前先同步杀掉并回收旧任务（最近的意图优先），
// 保证同一 tmp 文件不会有两个进程同时写；窗口销毁时杀掉子进程。

use serde::Serialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 音轨缓存标记（加在基名后、扩展名前）；scan_folder 跳过这类文件避免播放列表重复。
pub const CACHE_TAG: &str = " (AAC)";

pub struct TranscodeJob {
    child: Child,
    gen: u64,
}

impl TranscodeJob {
    /// 立即杀掉子进程并回收（窗口销毁/让位/取消共用）。
    pub fn kill_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub type JobState = Mutex<Option<TranscodeJob>>;

#[derive(Serialize, Clone)]
pub struct JobProgress {
    pub kind: String,  // "audio" | "subtitle"
    pub video: String,
    pub out: String,
    pub percent: f64, // 0~100；duration 未知时为 -1
    pub written: u64,
}

#[derive(Serialize, Clone)]
pub struct JobDone {
    pub kind: String,
    pub video: String,
    pub out: String,
}

#[derive(Serialize, Clone)]
pub struct JobError {
    pub kind: String,
    pub video: String,
    pub out: String,
    pub message: String,
}

/// 外挂音轨产物：原基名 (音轨N).mka（N 为 1-based 音轨序号），纯计算不发 IO。
pub fn mka_cache_path(video: &Path, audio_index: usize) -> PathBuf {
    let dir = video.parent().unwrap_or_else(|| Path::new("."));
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dir.join(format!("{stem} (音轨{}).mka", audio_index + 1))
}

/// 内嵌字幕抽取产物：原基名 (字幕N).srt（N 为 1-based 字幕轨序号）。
pub fn srt_cache_path(video: &Path, sub_index: usize) -> PathBuf {
    let dir = video.parent().unwrap_or_else(|| Path::new("."));
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dir.join(format!("{stem} (字幕{}).srt", sub_index + 1))
}

fn tmp_path(out: &Path) -> PathBuf {
    let dir = out.parent().unwrap_or_else(|| Path::new("."));
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = out.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mkv".into());
    dir.join(format!("{stem}.tmp.{ext}"))
}

/// 播放列表过滤：基名以 " (AAC)" 或 " (AAC).tmp" 结尾的是本应用的转码产物。
pub fn is_cache_name(name: &str) -> bool {
    match name.rfind('.') {
        Some(i) => {
            let stem = &name[..i];
            stem.ends_with(CACHE_TAG) || stem.ends_with(&format!("{CACHE_TAG}.tmp"))
        }
        None => false,
    }
}

/// ffmpeg 定位：exe 同目录优先，其次 PATH。
fn find_ffmpeg() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().map(|d| d.join("ffmpeg.exe")).unwrap_or_default();
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let probe = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match probe {
        Ok(s) if s.success() => Ok(PathBuf::from("ffmpeg")),
        _ => Err("未找到 ffmpeg（PATH 或程序目录），无法处理".into()),
    }
}

/// 全局参数段（进度流 + 不读 stdin + 覆盖输出）。
fn ffmpeg_header() -> Vec<String> {
    [
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
    ]
    .to_vec()
}

/// 音轨 → AAC 独立 mka。`limit_secs` 仅供测试截短时长。
/// -readrate 限速：转换期间视频还在同一个盘上播放，全速顺序读会饿死播放起播（HDD 尤甚）。
fn ffmpeg_audio_args(src: &Path, audio_index: usize, out: &Path, limit_secs: Option<u64>) -> Vec<String> {
    let mut a = ffmpeg_header();
    a.extend(["-readrate".into(), "8".into()]);
    a.extend(["-i".into(), src.to_string_lossy().into_owned(), "-map".into(), format!("0:a:{audio_index}"), "-c:a".into(), "aac".into(), "-b:a".into(), "640k".into()]);
    if let Some(s) = limit_secs {
        a.push("-t".into());
        a.push(s.to_string());
    }
    a.push(out.to_string_lossy().into_owned());
    a
}

/// 字幕轨 → SRT（文本轨道统一转 srt，ASS 样式丢弃，与前端 VTT 渲染一致）。字幕轨稀疏读取，无需限速。
fn ffmpeg_sub_args(src: &Path, sub_index: usize, out: &Path) -> Vec<String> {
    let mut a = ffmpeg_header();
    a.extend(["-i".into(), src.to_string_lossy().into_owned(), "-map".into(), format!("0:s:{sub_index}"), "-c:s".into(), "srt".into()]);
    a.push(out.to_string_lossy().into_owned());
    a
}

/// 拉起 ffmpeg；stdout 是 `-progress` 进度流，stderr 是错误输出。
/// 两个管道必须立刻有人读，否则缓冲写满 ffmpeg 会被卡死。
fn spawn_ffmpeg(ffmpeg: &Path, args: &[String]) -> Result<(Child, ChildStdout, ChildStderr), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    Ok((child, stdout, stderr))
}

/// 持续读 `-progress` 流直到 EOF（ffmpeg 退出/被杀）。
/// `on_progress` 以 ≥400ms 间隔回调（percent<0 表示时长未知）。
fn pump_progress(
    stdout: ChildStdout,
    duration_us: Option<u64>,
    mut on_progress: impl FnMut(f64, u64),
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut out_time_us: u64 = 0;
    let mut total_size: u64 = 0;
    let mut last_emit = Instant::now();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let kv = line.trim_end();
                if let Some(v) = kv.strip_prefix("out_time_us=") {
                    out_time_us = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = kv.strip_prefix("total_size=") {
                    total_size = v.trim().parse().unwrap_or(0);
                } else if kv.starts_with("progress=") && last_emit.elapsed() >= Duration::from_millis(400) {
                    last_emit = Instant::now();
                    let percent = match duration_us {
                        Some(d) if d > 0 => (out_time_us as f64 / d as f64 * 100.0).min(100.0),
                        _ => -1.0,
                    };
                    on_progress(percent, total_size);
                }
            }
            Err(_) => break,
        }
    }
}

/// 收集 stderr 全文（错误场景只取最后一行展示）。
fn drain_stderr(stderr: ChildStderr) -> String {
    let mut tail = String::new();
    for line in BufReader::new(stderr).lines().flatten() {
        if tail.len() > 4000 {
            tail.clear(); // 只留最近的错误
        }
        tail.push_str(&line);
        tail.push('\n');
    }
    tail
}

fn last_err_line(tail: &str) -> String {
    let msg = tail.lines().last().unwrap_or("").trim();
    if msg.is_empty() {
        "ffmpeg 异常退出".into()
    } else {
        msg.to_string()
    }
}

struct JobSpec {
    kind: &'static str, // "audio" | "subtitle"
    video: String,
    args: Vec<String>,
    tmp: PathBuf,
    out: PathBuf,
    duration_us: Option<u64>,
}

/// 单任务启动器：杀旧任务 → 拉起新 ffmpeg → 进度/收尾线程（改名 + 发事件）。
fn spawn_job(app: AppHandle, state: tauri::State<'_, JobState>, spec: JobSpec) -> Result<String, String> {
    let ffmpeg = find_ffmpeg()?;
    if spec.out.is_file() {
        return Ok(spec.out.to_string_lossy().into_owned()); // 缓存已存在
    }

    // 让位：同步杀旧任务并回收，确保 tmp 文件句柄已释放
    let gen = {
        let mut guard = state.lock().unwrap();
        match guard.take() {
            Some(mut old) => {
                let _ = old.child.kill();
                let _ = old.child.wait();
                old.gen.wrapping_add(1)
            }
            None => 1,
        }
    };

    let (mut child, stdout, stderr) = spawn_ffmpeg(&ffmpeg, &spec.args)?;
    *state.lock().unwrap() = Some(TranscodeJob { child, gen });

    let kind = spec.kind.to_string();
    let video_str = spec.video.clone();
    let out_str = spec.out.to_string_lossy().into_owned();
    let tmp2 = spec.tmp.clone();
    let out2 = spec.out.clone();
    let duration_us = spec.duration_us;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let err_thread = std::thread::spawn(move || drain_stderr(stderr));
        pump_progress(stdout, duration_us, |percent, written| {
            let _ = app2.emit(
                "transcode://progress",
                JobProgress { kind: kind.clone(), video: video_str.clone(), out: out_str.clone(), percent, written },
            );
        });
        let err_tail = err_thread.join().unwrap_or_default();
        let status = app2.state::<JobState>().lock().unwrap().take_if(|j| j.gen == gen);
        match status {
            // 我这单还挂在状态里 = 自然跑完（被杀/让位的单已被摘走）
            Some(mut job) => {
                let ok = job.child.wait().map(|s| s.success()).unwrap_or(false);
                if ok {
                    if fs::rename(&tmp2, &out2).is_ok() {
                        let _ = app2.emit("transcode://done", JobDone { kind, video: video_str, out: out_str });
                    } else {
                        let _ = fs::remove_file(&tmp2);
                        let _ = app2.emit(
                            "transcode://error",
                            JobError { kind, video: video_str, out: out_str, message: "产物改名失败".into() },
                        );
                    }
                } else {
                    let _ = fs::remove_file(&tmp2);
                    let _ = app2.emit(
                        "transcode://error",
                        JobError { kind, video: video_str, out: out_str, message: last_err_line(&err_tail) },
                    );
                }
            }
            // 被取消或让位：静默清理半成品
            None => {
                let _ = fs::remove_file(&tmp2);
            }
        }
    });

    Ok(spec.out.to_string_lossy().into_owned())
}

// ---- Tauri 命令 ----

/// 计算外挂音轨 mka 产物路径（供前端做缓存命中判断）。
#[tauri::command]
pub fn transcode_target(video_path: String, audio_index: usize) -> String {
    mka_cache_path(Path::new(&video_path), audio_index).to_string_lossy().into_owned()
}

/// 启动后台音轨转换（输出独立 .mka，视频仍从原文件播放）。
#[tauri::command]
pub fn transcode_audio_start(
    app: AppHandle,
    state: tauri::State<'_, JobState>,
    video_path: String,
    audio_index: usize,
    duration_us: Option<u64>,
) -> Result<String, String> {
    let src = PathBuf::from(&video_path);
    if !src.is_file() {
        return Err("源视频不存在".into());
    }
    let out = mka_cache_path(&src, audio_index);
    let tmp = tmp_path(&out);
    spawn_job(
        app,
        state,
        JobSpec {
            kind: "audio",
            video: video_path,
            args: ffmpeg_audio_args(&src, audio_index, &tmp, None),
            tmp,
            out,
            duration_us,
        },
    )
}

/// 启动后台内嵌字幕抽取（文本轨 → SRT）。
#[tauri::command]
pub fn extract_subtitle_start(
    app: AppHandle,
    state: tauri::State<'_, JobState>,
    video_path: String,
    sub_index: usize,
    duration_us: Option<u64>,
) -> Result<String, String> {
    let src = PathBuf::from(&video_path);
    if !src.is_file() {
        return Err("源视频不存在".into());
    }
    let out = srt_cache_path(&src, sub_index);
    let tmp = tmp_path(&out);
    spawn_job(
        app,
        state,
        JobSpec {
            kind: "subtitle",
            video: video_path,
            args: ffmpeg_sub_args(&src, sub_index, &tmp),
            tmp,
            out,
            duration_us,
        },
    )
}

/// 取消当前任务（若有）。半成品由工作线程清理。
#[tauri::command]
pub fn transcode_audio_cancel(state: tauri::State<'_, JobState>) -> bool {
    let mut guard = state.lock().unwrap();
    if let Some(mut job) = guard.take() {
        let _ = job.child.kill();
        let _ = job.child.wait();
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阻塞式跑一小段处理，供端到端冒烟测试。
    fn run_blocking(args: Vec<String>, out: &Path, limit_secs: u64) -> Result<u64, String> {
        let ffmpeg = find_ffmpeg()?;
        if let Some(dir) = out.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = tmp_path(out);
        let mut args2 = args.clone();
        // 测试统一截短时长：插在输出路径之前
        let pos = args2.len() - 1;
        args2.insert(pos, "-t".into());
        args2.insert(pos + 1, limit_secs.to_string());
        args2[pos + 2] = tmp.to_string_lossy().into_owned();
        let (mut child, stdout, stderr) = spawn_ffmpeg(&ffmpeg, &args2)?;
        let err_thread = std::thread::spawn(move || drain_stderr(stderr));
        pump_progress(stdout, None, |p, w| println!("  进度 {p:.0}% · {} bytes", w));
        let err_tail = err_thread.join().unwrap_or_default();
        if !child.wait().map_err(|e| e.to_string())?.success() {
            return Err(last_err_line(&err_tail));
        }
        let written = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        fs::rename(&tmp, out).map_err(|e| e.to_string())?;
        Ok(written)
    }

    /// 真机素材端到端测试：设 TRANSCODE_TEST_SRC=<mkv> TRANSCODE_TEST_OUT=<输出> 后才跑。
    #[test]
    fn ffmpeg_e2e() {
        let src = match std::env::var("TRANSCODE_TEST_SRC") {
            Ok(v) if !v.is_empty() => v,
            _ => return,
        };
        let out = std::env::var("TRANSCODE_TEST_OUT").expect("需要 TRANSCODE_TEST_OUT");
        let sub_index = std::env::var("TRANSCODE_TEST_SUB").ok().and_then(|v| v.parse::<usize>().ok());
        let src_path = Path::new(&src);
        let args = match sub_index {
            Some(i) => ffmpeg_sub_args(src_path, i, Path::new(&out)),
            None => ffmpeg_audio_args(src_path, 0, Path::new(&out), None),
        };
        // 字幕轨开头经常没有对白，窗口放大到 10 分钟
        let limit = if sub_index.is_some() { 600 } else { 30 };
        let written = run_blocking(args, Path::new(&out), limit).expect("处理失败");
        println!("  产物 {} bytes", written);
        assert!(written > 0);
    }

    #[test]
    fn cache_paths() {
        let v = Path::new(r"D:\影\深空失忆 (2009).mkv");
        assert_eq!(mka_cache_path(v, 0), Path::new(r"D:\影\深空失忆 (2009) (音轨1).mka"));
        assert_eq!(mka_cache_path(v, 2), Path::new(r"D:\影\深空失忆 (2009) (音轨3).mka"));
        assert_eq!(srt_cache_path(v, 1), Path::new(r"D:\影\深空失忆 (2009) (字幕2).srt"));
        assert!(is_cache_name("深空失忆 (2009) (AAC).mkv"));
        assert!(is_cache_name("深空失忆 (2009) (AAC).tmp.mkv"));
        assert!(!is_cache_name("深空失忆 (2009).mkv"));
        assert!(!is_cache_name(" AAC.mkv"));
    }
}
