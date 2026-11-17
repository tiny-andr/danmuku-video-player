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

// 子进程不允许新建控制台窗口：ffmpeg.exe 是控制台程序，Tauri 主程序是 GUI 子系统，
// 不加 CREATE_NO_WINDOW 时 Windows 会给每个 ffmpeg 弹一个黑框（转码、抽字幕时都会闪）。
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 统一的子进程外壳：Windows 上隐藏控制台窗口。
fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

// 子进程绑进带 KILL_ON_JOB_CLOSE 的 Job Object：父进程无论怎么死（点关闭、崩溃、
// 任务管理器强杀），内核都会连带杀掉 job 里的 ffmpeg。光靠 window 事件回调不够 ——
// 强杀时回调根本不执行，ffmpeg 会变孤儿继续占磁盘 IO 转两分钟。
#[cfg(windows)]
mod job {
    use std::process::Child;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobHandle(HANDLE);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

    fn job() -> Option<&'static JobHandle> {
        JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(h);
                return None;
            }
            Some(JobHandle(h))
        })
        .as_ref()
    }

    /// 把已启动的子进程收进 job；失败只是失去兜底，不影响转码本身
    pub fn assign(child: &Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        match job() {
            Some(h) => unsafe { AssignProcessToJobObject(h.0, child.as_raw_handle() as HANDLE) != 0 },
            None => false,
        }
    }
}
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// 任务代际号，只增不减。
///
/// 早先的写法是"槽里的 gen + 1，槽为空则从 1 开始"。问题在于取消/让位会把槽清空，
/// 于是下一个任务又从 1 开始编号，而上一单的 worker 收尾时正是靠
/// `take_if(|j| j.gen == gen)` 认领"自己那一单" —— 号一重合，旧 worker 就会把新任务
/// 摘走，然后拿着上一单的产物清单去改名/删 tmp。取消一次之后必然踩到。
static JOB_GEN: AtomicU64 = AtomicU64::new(0);

fn next_gen() -> u64 {
    JOB_GEN.fetch_add(1, Ordering::Relaxed) + 1
}

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

/// 任务归属的槽。音轨转码与字幕抽取是两件互不相干的事，各自占一个槽 ——
/// 早先共用一个单槽，字幕兜底一启动就把正在转的音轨杀掉，反过来也一样。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Audio,
    Subtitle,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Audio => "audio",
            Slot::Subtitle => "subtitle",
        }
    }
}

pub struct JobSlots {
    audio: Mutex<Option<TranscodeJob>>,
    subtitle: Mutex<Option<TranscodeJob>>,
}

impl Default for JobSlots {
    fn default() -> Self {
        Self { audio: Mutex::new(None), subtitle: Mutex::new(None) }
    }
}

pub type JobState = JobSlots;

impl JobSlots {
    fn slot(&self, slot: Slot) -> &Mutex<Option<TranscodeJob>> {
        match slot {
            Slot::Audio => &self.audio,
            Slot::Subtitle => &self.subtitle,
        }
    }

    /// 摘出槽里的任务但不在锁里 kill：等子进程退出可能要几百毫秒，
    /// 拿着锁等会把另一个槽的命令一起卡住。
    fn take(&self, slot: Slot) -> Option<TranscodeJob> {
        self.slot(slot).lock().unwrap().take()
    }

    /// 只有槽里仍然是"我这一代"的任务时才摘走。摘到 Some = 这单是自然跑完的；
    /// None = 已被取消或已被新任务让位。
    fn take_if_gen(&self, slot: Slot, gen: u64) -> Option<TranscodeJob> {
        self.slot(slot).lock().unwrap().take_if(|j| j.gen == gen)
    }

    /// 放进新任务，并把可能同时挤进来的旧任务交还给调用方去 kill
    /// （两个命令几乎同时启动时，只靠"进锁前先 take"仍可能漏掉一个）。
    fn install(&self, slot: Slot, job: TranscodeJob) -> Option<TranscodeJob> {
        self.slot(slot).lock().unwrap().replace(job)
    }

    /// 取消一个槽，返回是否真的杀掉了任务。
    /// 被杀的 worker 收尾时自己会广播 `transcode://cancelled`，这里不重复发。
    pub fn cancel(&self, slot: Slot) -> bool {
        match self.take(slot) {
            Some(mut job) => {
                job.kill_now();
                true
            }
            None => false,
        }
    }

    /// 换片/关窗时一把全取消，返回取消的数量。
    pub fn cancel_all(&self) -> u32 {
        [Slot::Audio, Slot::Subtitle].into_iter().filter(|s| self.cancel(*s)).count() as u32
    }
}

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
    /// 这一批的全部产物（一次 ffmpeg 调用可能抽出多条字幕轨）
    pub outs: Vec<String>,
    /// 纯内存字幕：抽取结果直接随事件带回，不落盘
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 对应哪条字幕轨（0 基序号）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

#[derive(Serialize, Clone)]
pub struct JobError {
    pub kind: String,
    pub video: String,
    pub out: String,
    pub message: String,
}

/// 任务被取消或被新任务让位。
///
/// 之前这条路径是完全静默的：前端只有 done/error 两个出口，一旦字幕任务在半路被
/// 顶掉，`pendingSub` 就永远等不到 done，那条轨的"加载中"状态会一直挂着。
/// 所以让位/取消也必须显式通知一次。
#[derive(Serialize, Clone)]
pub struct JobCancelled {
    pub kind: String, // "audio" | "subtitle"
    pub video: String,
    /// 字幕轨 0 基序号；音轨没有这个概念，序列化成 null
    pub index: Option<usize>,
}

/// 外挂音轨产物：原基名 (音轨N).mka（N 为 1-based 音轨序号），纯计算不发 IO。
pub fn mka_cache_path(video: &Path, audio_index: usize) -> PathBuf {
    let dir = video.parent().unwrap_or_else(|| Path::new("."));
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dir.join(format!("{stem} (音轨{}).mka", audio_index + 1))
}

/// 内嵌字幕抽取产物：原基名 (字幕N).srt（N 为 1-based 字幕轨序号）。
/// 现在内嵌字幕走纯内存，不再写这个文件；保留函数给旧缓存的兼容清理用。
#[allow(dead_code)]
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
    let probe = hidden_command("ffmpeg")
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
/// 不再限速（原来 -readrate 8）：2 小时的片子要等 15 分钟才出声，而全速只要
/// 约 2 分钟（实测 11.9GB/169MB 音频）。转码期间视频音频已被 WebAudio 隔离，
/// 慢的代价是用户干等，比重一点 IO 更难受。
/// -ac 2 是硬要求：TrueHD/DTS-HD 常见 8 声道，原样编成 8ch AAC 时 WebView2（Media
/// Foundation）解码上限只有 6 声道，7.1 会静音 —— 转了等于没转。立体声足够，192k 已富余。
///
/// `copy=true` 用于「内核本来就解得了、用户只是想换一条听」的轨道（多音轨片子里的
/// AC-3 / E-AC-3 / PCM）：直接 `-c:a copy` 抽进 mka，不重编码。理由有两条 ——
/// 一是重编码会把 5.1 压成立体声（上面 -ac 2 那条约束），用户切个语言反而降质；
/// 二是抽轨仍要顺序读完整个文件（音轨块遍布每个 cluster），IO 不可避免，但能省掉
/// 编码的 CPU 和时间。真解不了的轨（DTS/TrueHD…）必须走转码，`copy` 会给到它们吗？
/// 不会：调用方按 `isDeadCodec` 决定，见前端 ensureAudioTrack。
fn ffmpeg_audio_args(src: &Path, audio_index: usize, out: &Path, limit_secs: Option<u64>, copy: bool) -> Vec<String> {
    let mut a = ffmpeg_header();
    a.extend(["-i".into(), src.to_string_lossy().into_owned(), "-map".into(), format!("0:a:{audio_index}")]);
    if copy {
        a.extend(["-c:a".into(), "copy".into()]);
    } else {
        a.extend(["-c:a".into(), "aac".into(), "-ac".into(), "2".into(), "-b:a".into(), "192k".into()]);
    }
    if let Some(s) = limit_secs {
        a.push("-t".into());
        a.push(s.to_string());
    }
    a.push(out.to_string_lossy().into_owned());
    a
}

/// 字幕轨 → SRT（文本轨道统一转 srt，ASS 样式丢弃，与前端 VTT 渲染一致）。
/// 一次调用可以带多条轨：MKV 的音频块和字幕块遍布每个 cluster，读一遍盘就够，
/// 每条轨单独起进程会把整部片重读一遍（30 GB 片子 3 条字幕 = 3 遍）。
#[allow(dead_code)]
fn ffmpeg_sub_args(src: &Path, subs: &[(usize, PathBuf)]) -> Vec<String> {
    let mut a = ffmpeg_header();
    a.extend(["-i".into(), src.to_string_lossy().into_owned()]);
    for (index, out) in subs {
        a.extend([
            "-map".into(),
            format!("0:s:{index}"),
            "-c:s".into(),
            "srt".into(),
            out.to_string_lossy().into_owned(),
        ]);
    }
    a
}

/// 拉起 ffmpeg；stdout 是 `-progress` 进度流，stderr 是错误输出。
/// 两个管道必须立刻有人读，否则缓冲写满 ffmpeg 会被卡死。
fn spawn_ffmpeg(ffmpeg: &Path, args: &[String]) -> Result<(Child, ChildStdout, ChildStderr), String> {
    let mut cmd = hidden_command(ffmpeg);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    #[cfg(windows)]
    {
        job::assign(&child);
    }
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
    slot: Slot,
    video: String,
    args: Vec<String>,
    /// 每个产物：(临时文件, 最终文件)。多于一个表示一次 ffmpeg 调用产出多个文件 ——
    /// 音频块和字幕块遍布每个 cluster，读取次数才是成本，多路输出共用同一次 demux。
    outputs: Vec<(PathBuf, PathBuf)>,
    duration_us: Option<u64>,
}

/// 单任务启动器：杀旧任务 → 拉起新 ffmpeg → 进度/收尾线程（改名 + 发事件）。
/// 返回这一批的全部产物路径。
fn spawn_job(app: &AppHandle, spec: JobSpec) -> Result<Vec<String>, String> {
    let ffmpeg = find_ffmpeg()?;
    if spec.outputs.is_empty() {
        return Ok(Vec::new());
    }
    let slot = spec.slot;
    let gen = next_gen();

    // 让位：同步杀旧任务并回收，确保 tmp 文件句柄已释放
    if let Some(mut old) = app.state::<JobState>().take(slot) {
        old.kill_now();
    }

    let (child, stdout, stderr) = spawn_ffmpeg(&ffmpeg, &spec.args)?;
    if let Some(mut displaced) = app.state::<JobState>().install(slot, TranscodeJob { child, gen }) {
        displaced.kill_now();
    }

    let kind = slot.as_str().to_string();
    let video_str = spec.video.clone();
    let outputs = spec.outputs.clone();
    let first_out = outputs
        .first()
        .map(|(_, o)| o.to_string_lossy().into_owned())
        .unwrap_or_default();
    let all_outs: Vec<String> = outputs.iter().map(|(_, o)| o.to_string_lossy().into_owned()).collect();
    let duration_us = spec.duration_us;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let err_thread = std::thread::spawn(move || drain_stderr(stderr));
        let out_for_progress = first_out.clone();
        pump_progress(stdout, duration_us, |percent, written| {
            let _ = app2.emit(
                "transcode://progress",
                JobProgress { kind: kind.clone(), video: video_str.clone(), out: out_for_progress.clone(), percent, written },
            );
        });
        let err_tail = err_thread.join().unwrap_or_default();
        let status = app2.state::<JobState>().take_if_gen(slot, gen);
        match status {
            // 我这单还挂在状态里 = 自然跑完（被杀/让位的单已被摘走）
            Some(mut job) => {
                let ok = job.child.wait().map(|s| s.success()).unwrap_or(false);
                if ok {
                    let mut renamed = Vec::new();
                    let mut failed = None;
                    for (tmp, out) in &outputs {
                        if fs::rename(tmp, out).is_err() {
                            failed = Some((out.to_string_lossy().into_owned(), "产物改名失败".to_string()));
                            break;
                        }
                        // 0 字节 = 实际没抽出内容（例如字幕时间戳全在视频时长之外）。
                        // 当成失败处理，否则前端会挂载一条空字幕。
                        if fs::metadata(out).map(|m| m.len() == 0).unwrap_or(true) {
                            let _ = fs::remove_file(out);
                            failed = Some((out.to_string_lossy().into_owned(), "没有可抽取的内容（产物为空）".to_string()));
                            break;
                        }
                        renamed.push(out.to_string_lossy().into_owned());
                    }
                    if let Some((bad, why)) = failed {
                        for (tmp, _) in &outputs {
                            let _ = fs::remove_file(tmp);
                        }
                        let _ = app2.emit(
                            "transcode://error",
                            JobError { kind, video: video_str, out: bad, message: why },
                        );
                    } else {
                        let _ = app2.emit(
                            "transcode://done",
                            JobDone {
                                kind,
                                video: video_str,
                                out: first_out.clone(),
                                outs: renamed,
                                text: None,
                                index: None,
                            },
                        );
                    }
                } else {
                    for (tmp, _) in &outputs {
                        let _ = fs::remove_file(tmp);
                    }
                    let _ = app2.emit(
                        "transcode://error",
                        JobError { kind, video: video_str, out: first_out.clone(), message: last_err_line(&err_tail) },
                    );
                }
            }
            // 被取消或让位：清掉半成品，并且必须告诉前端 —— 它等的是 done，
            // 少这一条就永远等不到。
            None => {
                for (tmp, _) in &outputs {
                    let _ = fs::remove_file(tmp);
                }
                let _ = app2.emit(
                    "transcode://cancelled",
                    JobCancelled { kind, video: video_str, index: None },
                );
            }
        }
    });

    Ok(all_outs)
}

// ---- Tauri 命令 ----

/// 计算外挂音轨 mka 产物路径（供前端做缓存命中判断）。
#[tauri::command]
pub fn transcode_target(video_path: String, audio_index: usize) -> String {
    mka_cache_path(Path::new(&video_path), audio_index).to_string_lossy().into_owned()
}

/// 启动后台音轨转换（输出独立 .mka，视频仍从原文件播放）。
///
/// 声明成 async + spawn_blocking：里面有 PATH 探测、旧任务回收（等子进程退出）
/// 和文件系统探测，跑在同步命令里会卡住 webview 主线程。
#[tauri::command]
pub async fn transcode_audio_start(
    app: AppHandle,
    video_path: String,
    audio_index: usize,
    duration_us: Option<u64>,
    copy_audio: Option<bool>,
) -> Result<String, String> {
    let copy = copy_audio.unwrap_or(false);
    crate::blocking::blocking(move || {
        transcode_audio_start_impl(&app, video_path, audio_index, duration_us, copy)
    })
    .await?
}

/// 实现体：参数只有 AppHandle（发事件/取状态用），其余全是普通类型。
///
/// `copy_audio`：这条轨内核本来就解得开（多音轨片子里用户只是想换一条听），
/// 用 `-c copy` 抽进 mka 而不是转 AAC —— 避免把 5.1 压成立体声。
/// 传 false/None 就是原来的「转 AAC 2ch 192k」路径。
fn transcode_audio_start_impl(
    app: &AppHandle,
    video_path: String,
    audio_index: usize,
    duration_us: Option<u64>,
    copy_audio: bool,
) -> Result<String, String> {
    let src = PathBuf::from(&video_path);
    if !src.is_file() {
        return Err("源视频不存在".into());
    }
    let out = mka_cache_path(&src, audio_index);
    let tmp = tmp_path(&out);
    let paths = spawn_job(
        app,
        JobSpec {
            slot: Slot::Audio,
            video: video_path,
            args: ffmpeg_audio_args(&src, audio_index, &tmp, None, copy_audio),
            outputs: vec![(tmp, out)],
            duration_us,
        },
    )?;
    Ok(paths.into_iter().next().unwrap_or_default())
}

/// 一条待抽取的字幕轨：`index` 是字幕轨序号（0 基，决定缓存文件名），
/// `track` 是 Matroska TrackNumber（Cues 索引按它查）。
#[derive(serde::Deserialize)]
pub struct SubTrack {
    pub index: usize,
    pub track: u32,
    /// 这条轨是不是 ASS/SSA（Matroska 里的负载格式与 SRT 不同）
    #[serde(default)]
    pub ass: bool,
}

/// 把外挂字幕（SRT/VTT/ASS/SSA）转成 SRT 文本，结果只回给前端，不落盘。
///
/// 主路径是 `subtitle_text::to_srt`（纯 Rust，不碰外部程序）：srt/vtt/ass/ssa
/// 都是纯文本，转换只是重排时间轴 + 剥样式，不需要解码器。这一点很关键 ——
/// 安装包并没有把 ffmpeg 打进去，只有开发机 PATH 里恰好有才work，走 ffmpeg
/// 就等于"干净安装的机器上一个外挂字幕都挂不上"。
/// ffmpeg 只留给这套解析认不出来的格式兜底。
///
/// 为什么要转而不是直接挂：Artplayer 内置的 assToVtt 生成的 VTT 不合法
/// （WEBVTT 后无换行、序号和时间轴挤在一行），结果是「菜单显示已挂载、画面一个字
/// 都没有」；它又是 innerHTML + escape 渲染，`<font>` / `<i>` 会被显示成原文。
#[tauri::command]
pub async fn subtitle_to_srt_text(path: String) -> Result<String, String> {
    crate::blocking::blocking(move || subtitle_to_srt_text_impl(path)).await?
}

fn subtitle_to_srt_text_impl(path: String) -> Result<String, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err("字幕文件不存在".into());
    }
    match crate::subtitle_text::to_srt(&src) {
        Ok(srt) => Ok(srt),
        Err(pure_err) => subtitle_to_srt_with_ffmpeg(&src).map_err(|ff_err| format!("{pure_err}（ffmpeg 兜底：{ff_err}）")),
    }
}

/// ffmpeg 兜底：处理 `subtitle_text::to_srt` 认不出来的格式。
fn subtitle_to_srt_with_ffmpeg(src: &Path) -> Result<String, String> {
    let ffmpeg = find_ffmpeg()?;
    let mut cmd = hidden_command(&ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-i",
        src.to_string_lossy().as_ref(),
        "-f",
        "srt",
        "pipe:1",
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(last_err_line(&msg));
    }
    let srt = crate::mkv_sub::strip_markup(&String::from_utf8_lossy(&out.stdout));
    if srt.trim().is_empty() {
        return Err("没有可显示的内容".into());
    }
    Ok(srt)
}

/// 抽取好的字幕文本（纯内存交给前端，不落盘）
#[derive(Serialize, Clone)]
pub struct SubText {
    pub index: usize,
    pub text: String,
}

/// 抽取内嵌文本字幕，结果**全部走内存**，不产生任何缓存文件。
/// 有 Cues 索引的片子走稀疏 seek（28 GB 约 0.2 秒）；没有的回退 ffmpeg，
/// 用 `-f srt pipe:1` 把结果读到内存，同样不落盘（代价是要顺序读整片）。
#[tauri::command]
pub async fn extract_subtitle_text(
    app: AppHandle,
    video_path: String,
    subs: Vec<SubTrack>,
    duration_us: Option<u64>,
) -> Result<Vec<SubText>, String> {
    crate::blocking::blocking(move || extract_subtitle_text_impl(&app, video_path, subs, duration_us)).await?
}

fn extract_subtitle_text_impl(
    app: &AppHandle,
    video_path: String,
    subs: Vec<SubTrack>,
    duration_us: Option<u64>,
) -> Result<Vec<SubText>, String> {
    let src = PathBuf::from(&video_path);
    if !src.is_file() {
        return Err("源视频不存在".into());
    }
    let mut ready: Vec<SubText> = Vec::new();
    let mut pending: Vec<SubTrack> = Vec::new();

    for sub in subs {
        match crate::mkv_sub::sparse_extract_srt(&src, sub.track, sub.ass) {
            Ok(srt) if !srt.trim().is_empty() => ready.push(SubText { index: sub.index, text: srt }),
            _ => pending.push(sub),
        }
    }

    // 没被稀疏抽出来的那条交给 ffmpeg，结果从 stdout 读回，仍然不写文件。
    // 一次只跑一条：字幕走 pipe:1，一个进程只能有一个 stdout。
    if let Some(first) = pending.into_iter().next() {
        spawn_text_job(app, video_path, first, duration_us)?;
    }
    Ok(ready)
}

fn spawn_text_job(app: &AppHandle, video: String, sub: SubTrack, _duration_us: Option<u64>) -> Result<(), String> {
    let ffmpeg = find_ffmpeg()?;
    let args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-i".into(),
        video.clone(),
        "-map".into(),
        format!("0:s:{}", sub.index),
        "-c:s".into(),
        "srt".into(),
        "-f".into(),
        "srt".into(),
        "pipe:1".into(),
    ];

    let gen = next_gen();
    // 字幕只占字幕槽：让位时只杀上一条字幕抽取，音轨转码照旧在跑
    if let Some(mut old) = app.state::<JobState>().take(Slot::Subtitle) {
        old.kill_now();
    }
    let (child, stdout, stderr) = spawn_ffmpeg(&ffmpeg, &args)?;
    if let Some(mut displaced) = app.state::<JobState>().install(Slot::Subtitle, TranscodeJob { child, gen }) {
        displaced.kill_now();
    }

    let index = sub.index;
    let video_str = video;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let err_thread = std::thread::spawn(move || drain_stderr(stderr));
        let mut raw = Vec::new();
        let reader = BufReader::new(stdout);
        use std::io::Read as _;
        // 输出量不受输入文件大小约束：损坏轨或极端时间戳能让 ffmpeg 吐出远超片源的文本，
        // 无上限 read_to_end 会把内存吃穿。多读 1 字节用来判断"到底超没超"。
        let _ = reader.take(crate::MAX_READ_BYTES + 1).read_to_end(&mut raw);
        let oversize = raw.len() as u64 > crate::MAX_READ_BYTES;
        let err_tail = err_thread.join().unwrap_or_default();
        // 同样要剥标签：内嵌 ASS 走 ffmpeg 回退时会带上 <font face=...>
        let srt = crate::mkv_sub::strip_markup(&String::from_utf8_lossy(&raw));

        let Some(mut job) = app2.state::<JobState>().take_if_gen(Slot::Subtitle, gen) else {
            // 被取消/让位：前端还在等这条轨的 done，得告诉它别等了
            let _ = app2.emit(
                "transcode://cancelled",
                JobCancelled { kind: Slot::Subtitle.as_str().into(), video: video_str, index: Some(index) },
            );
            return;
        };
        let ok = job.child.wait().map(|s| s.success()).unwrap_or(false);
        if ok && !oversize && !srt.trim().is_empty() {
            let _ = app2.emit(
                "transcode://done",
                JobDone {
                    kind: "subtitle".into(),
                    video: video_str,
                    out: String::new(),
                    outs: Vec::new(),
                    text: Some(srt),
                    index: Some(index),
                },
            );
        } else {
            let msg = if oversize {
                format!("字幕文本超过 {} MB 上限，已放弃", crate::MAX_READ_BYTES / (1024 * 1024))
            } else if srt.trim().is_empty() {
                "没有可抽取的字幕内容".to_string()
            } else {
                last_err_line(&err_tail)
            };
            let _ = app2.emit(
                "transcode://error",
                JobError { kind: "subtitle".into(), video: video_str, out: String::new(), message: msg },
            );
        }
    });
    Ok(())
}

/// 取消音轨任务（若有）。半成品由工作线程清理，cancelled 事件也由它发。
///
/// 返回 `Result<bool, ()>` 而不是 `Result<bool, String>`：前端只拿它决定要不要提示，
/// 出错只可能是线程池崩了，再带一段字符串过去没有消费方。
#[tauri::command]
pub async fn transcode_audio_cancel(app: AppHandle) -> Result<bool, ()> {
    crate::blocking::blocking(move || app.state::<JobState>().cancel(Slot::Audio))
        .await
        .map_err(|_| ())
}

/// 取消两个槽里的所有后台任务（换片时用），返回取消的数量。
/// 换片时前端改调这个：只取消音轨会漏掉正在抽的那条字幕。
#[tauri::command]
pub async fn transcode_cancel_all(app: AppHandle) -> u32 {
    crate::blocking::blocking(move || app.state::<JobState>().cancel_all())
        .await
        .unwrap_or(0)
}

/// 判断某个文件名是不是本应用留下的半成品：`{stem} (音轨N).tmp.mka` /
/// `{stem} (字幕N).tmp.srt`（N 是十进制音轨/字幕序号，1 基）。
///
/// 早先的判据是「以 `{stem} (` 开头且含 `.tmp.`」，于是用户目录里名字碰巧套娃的
/// **普通文件**会被一起删掉（`反派 (上).tmp.txt`、`某片 (1).tmp.mkv` 实测都被删）。
/// 这是不可逆的删数据，所以按产物命名规则逐字符精确匹配：宁可漏删一个半成品，
/// 也绝不误删用户文件。
pub fn is_our_sidecar_tmp(stem: &str, name: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem) else { return false };
    let Some(rest) = rest.strip_prefix(" (") else { return false };
    let (expect, rest) = if let Some(r) = rest.strip_prefix("音轨") {
        ("mka", r)
    } else if let Some(r) = rest.strip_prefix("字幕") {
        ("srt", r)
    } else {
        return false;
    };
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    match rest[digits..].strip_prefix(").tmp.") {
        Some(ext) => ext == expect,
        None => false,
    }
}

/// 清掉同一部片遗留的半成品：上一次被强杀/崩溃时工作线程来不及删的
/// `基名 (音轨N).tmp.mka` / `基名 (字幕N).tmp.srt`。
///
/// 走 spawn_blocking：片源经常在 NAS/SMB 上，一次 read_dir 在断连时能卡几十秒。
/// 返回数量是给用户看的，原本就永不 reject，所以线程池异常时用 0 兜底。
#[tauri::command]
pub async fn clean_sidecar_tmp(video_path: String) -> u32 {
    crate::blocking::blocking(move || clean_sidecar_tmp_impl(video_path))
        .await
        .unwrap_or(0)
}

fn clean_sidecar_tmp_impl(video_path: String) -> u32 {
    let vp = Path::new(&video_path);
    let Some(dir) = vp.parent() else { return 0 };
    let stem = vp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.is_empty() {
        return 0;
    }
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !is_our_sidecar_tmp(&stem, &name) {
                continue;
            }
            let p = e.path();
            if p.is_file() && fs::remove_file(&p).is_ok() {
                n += 1;
            }
        }
    }
    n
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
            Some(i) => ffmpeg_sub_args(src_path, &[(i, PathBuf::from(&out))]),
            None => ffmpeg_audio_args(src_path, 0, Path::new(&out), None, false),
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

    /// 回归：`clean_sidecar_tmp` 只能删本应用自己命名的半成品。
    /// 之前的判据（基名前缀 + 含 ".tmp."）会连用户文件一起删。
    #[test]
    fn sidecar_tmp_pattern_is_exact() {
        let stem = "反派 (上)";
        for ok in [
            "反派 (上) (音轨1).tmp.mka",
            "反派 (上) (音轨12).tmp.mka",
            "反派 (上) (字幕2).tmp.srt",
        ] {
            assert!(is_our_sidecar_tmp(stem, ok), "应识别为本应用半成品：{ok}");
        }
        for bad in [
            "反派 (上).tmp.txt",              // 用户的普通文件
            "反派 (上) 笔记.tmp.txt",
            "反派 (上) (备注).tmp.mka",       // 括号里不是序号
            "反派 (上) (音轨).tmp.mka",       // 缺序号
            "反派 (上) (音轨1).tmp.mkv",      // 扩展名不对
            "反派 (上) (音轨1).mka",          // 正式产物，不能删
            "反派 (上) (AAC).tmp.mkv",        // 整片缓存半成品，另有归属
            "反派 (下) (音轨1).tmp.mka",      // 别的片
        ] {
            assert!(!is_our_sidecar_tmp(stem, bad), "误判为用户可删文件：{bad}");
        }
    }

    /// 端到端：真建目录真删文件，确认用户的同名普通文件原地不动。
    #[test]
    fn clean_sidecar_tmp_keeps_user_files() {
        let dir = std::env::temp_dir().join(format!("bplayer-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let video = dir.join("反派 (上).mkv");
        let ours = [
            "反派 (上) (音轨1).tmp.mka",
            "反派 (上) (字幕2).tmp.srt",
        ];
        let users = [
            "反派 (上).tmp.txt",
            "反派 (上) (备注).tmp.mka",
            "反派 (上) (音轨1).mka",
        ];
        fs::write(&video, b"x").unwrap();
        for n in ours.iter().chain(users.iter()) {
            fs::write(dir.join(n), b"x").unwrap();
        }
        let n = clean_sidecar_tmp_impl(video.to_string_lossy().into_owned());
        assert_eq!(n, ours.len() as u32, "删除数量不对：{n}");
        for name in ours {
            assert!(!dir.join(name).exists(), "半成品没删掉：{name}");
        }
        for name in users {
            assert!(dir.join(name).exists(), "用户文件被误删：{name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 任务调度（槽 + 代际号 + cancelled 事件）----

    /// 造一个"活着但立刻可杀"的子进程来占槽。
    /// 不拉真 ffmpeg：这里验证的是槽的归属，跑什么进程无关。
    fn dummy_job(gen: u64) -> TranscodeJob {
        let child = hidden_command("cmd")
            .args(["/c", "ping", "-n", "10", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("测试需要能启动 cmd ping");
        TranscodeJob { child, gen }
    }

    /// 代际号必须严格递增：只要还回退，旧 worker 就可能认领新任务。
    #[test]
    fn job_gen_never_goes_backwards() {
        let mut last = next_gen();
        for _ in 0..1000 {
            let g = next_gen();
            assert!(g > last, "代际号回退：{g} 不大于 {last}");
            last = g;
        }
    }

    /// 槽被清空（取消/让位）后旧 worker 收尾时，不能摘走新任务。
    #[test]
    fn stale_gen_cannot_claim_new_job() {
        let state = JobState::default();
        state.install(Slot::Audio, dummy_job(7));
        assert!(state.take_if_gen(Slot::Audio, 6).is_none(), "旧代际号不该认领新任务");
        let mut job = state.take_if_gen(Slot::Audio, 7).expect("当前代际号应该能摘走自己的任务");
        job.kill_now();
    }

    /// 音轨与字幕各自一个槽：取消/让位一边不能把另一边杀掉。
    #[test]
    fn audio_and_subtitle_slots_are_independent() {
        let state = JobState::default();
        state.install(Slot::Audio, dummy_job(1));
        state.install(Slot::Subtitle, dummy_job(2));

        assert!(state.cancel(Slot::Audio), "音轨槽里应该有任务");
        let mut sub = state.take_if_gen(Slot::Subtitle, 2).expect("取消音轨不该动到字幕槽");
        sub.kill_now();
        assert!(!state.cancel(Slot::Audio), "音轨槽已经空了");

        // 计数语义：只算真正被杀掉的任务
        state.install(Slot::Subtitle, dummy_job(3));
        assert_eq!(state.cancel_all(), 1);
        assert_eq!(state.cancel_all(), 0);
    }

    /// cancelled 的 payload 前端直接读字段：音轨的 index 必须是显式 null（不是缺字段），
    /// 字幕必须带 0 基序号，否则 pendingSub 认领不到。
    #[test]
    fn cancelled_payload_shape() {
        let audio = serde_json::to_value(JobCancelled { kind: "audio".into(), video: r"D:\影\a.mkv".into(), index: None }).unwrap();
        assert_eq!(
            audio,
            serde_json::json!({ "kind": "audio", "video": r"D:\影\a.mkv", "index": null })
        );

        let sub = serde_json::to_value(JobCancelled {
            kind: "subtitle".into(),
            video: "a.mkv".into(),
            index: Some(2),
        })
        .unwrap();
        assert_eq!(
            sub,
            serde_json::json!({ "kind": "subtitle", "video": "a.mkv", "index": 2 })
        );
    }

    /// 事件里 kind 的取值是前后端契约，改名会让前端认不出事件类型。
    #[test]
    fn slot_labels_match_frontend_contract() {
        assert_eq!(Slot::Audio.as_str(), "audio");
        assert_eq!(Slot::Subtitle.as_str(), "subtitle");
    }
}
