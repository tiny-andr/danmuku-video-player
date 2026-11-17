// 流式音频：把指定音轨从 start_secs 开始编成 fMP4 写到管道，按 ISO-BMFF fragment
// 切块后由前端主动拉取（pull），前端 append 进 MSE 的 SourceBuffer 边收边播。
//
// 为什么不沿用"转完整个文件再挂 mka"：DTS / TrueHD / DTS-HD 这类真解不了的音轨只能
// 转码，单进程实测 38~76 倍实时 —— 2 小时的片子要 2~3 分钟才出成品。用户点开片子时
// 不能干等，所以从第一块就开始喂 MSE：init + 首个 fragment 实测 60~70 ms 就绪，
// 之后转码一直远快于播放，缓冲只会越攒越多。
//
// 为什么用 pull 而不是 push（事件 / Channel）：转码比播放快 40~80 倍，push 没有背压，
// 2 小时片子两分钟内会把几百 MB 推进前端内存。pull 模式下"前端不来取 = 读线程不读
// stdout = ffmpeg 写满管道自己阻塞"，缓冲上限天然卡死，顺带省 CPU。
//
// 为什么是单进程连续转、seek 时杀掉重起：每段单独起一个 ffmpeg 会在接缝处掉约 21 ms
// 声音（编码器 flush 尾部填充 + 新编码器 pre-roll，empty_moov 没有 edit list 可裁）。
// 单进程从 seek 点连续编码是零空隙的；seek 的代价只是重起一次进程（~70 ms）。
//
// 前端怎么用：
//   const info = await invoke('audio_stream_start', { videoPath, audioIndex, startSecs });
//   const ms = new MediaSource(); audio.src = URL.createObjectURL(ms);
//   sourceopen: sb = ms.addSourceBuffer(info.mime);
//               sb.timestampOffset = info.startSecs;   // 让媒体时间轴对上视频
//               ms.duration = 视频总时长;               // empty_moov 下不设就是 Infinity
//   循环：await invoke('audio_stream_read', { id: info.id, maxBytes: 262144 })
//        -> ArrayBuffer（长度为 0 表示整条流已结束）
//   await invoke('audio_stream_stop', { id: info.id });
//
// 切块规则：第一块是 init（ftyp + moov），之后每块是 moof + mdat（一个 1 秒 fragment）。
// MSE 要求 init 只 append 一次且必须最先；fragment 按顺序追加，时间戳由 ffmpeg 连续生成。

use serde::Serialize;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// 读线程允许积压的上限：转码比播放快 40~80 倍，不设上限会把整条音轨推进内存。
/// 8 MB ≈ 5.5 分钟 192k 音频，够 MSE 攒一大截又能让 ffmpeg 及时阻塞。
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;

/// 单个 box 的上限（防御损坏数据把内存吃穿）。1 秒 AAC fragment 实测约 24 KB。
const MAX_BOX_BYTES: u64 = 64 * 1024 * 1024;

/// 单个 1 秒 fragment 正常只有几十 KB；一次最多交 4 MB 给前端，避免单次 IPC 过大。
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

/// 前端没来取数据时读线程最多等这么久，超过按"暂时没有输出"报错让前端重试。
/// NAS 断连时 ffmpeg 可能长时间不吐数据，所以放得比较宽。
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

const MIME_AAC: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

/// 会话号只增不减：前端 stop / read 都带它，旧请求打不到新流上。
static STREAM_GEN: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Clone)]
pub struct StreamInfo {
    /// 会话号，前端后续 read / stop 都带着
    pub id: u64,
    /// addSourceBuffer 用的 MIME
    pub mime: String,
    /// 这一路流的媒体时间 0 对应视频的哪个位置（秒），前端拿去设 timestampOffset
    pub start_secs: f64,
}

#[derive(Default)]
struct Shared {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    eof: bool,
    error: Option<String>,
    cancelled: bool,
}

struct StreamHandle {
    id: u64,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
}

/// 全局最多一路流：一次只播一条音轨，换轨 / 换片 / seek 都走"先杀旧再起新"。
#[derive(Default)]
pub struct StreamState(Mutex<Option<StreamHandle>>);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 某个线程 panic 不该让整条音频链路永久瘫痪：毒化后继续用里面的数据
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- 命令 ----

/// 启动一路流式转码。同一次只留一路，新的一路会先杀掉旧的。
#[tauri::command]
pub async fn audio_stream_start(
    app: AppHandle,
    video_path: String,
    audio_index: usize,
    start_secs: f64,
) -> Result<StreamInfo, String> {
    crate::blocking::blocking(move || start_impl(&app, video_path, audio_index, start_secs)).await?
}

/// 前端主动拉下一块。返回空数组 = 整条流已经结束；出错表示这次流不可用。
#[tauri::command]
pub async fn audio_stream_read(
    app: AppHandle,
    id: u64,
    max_bytes: usize,
) -> Result<tauri::ipc::Response, String> {
    let want = max_bytes.clamp(4096, MAX_READ_BYTES);
    let data = crate::blocking::blocking(move || read_impl(&app, id, want)).await??;
    Ok(tauri::ipc::Response::new(data))
}

/// 停止一路流（seek / 换轨 / 换片 / 关窗）。`id` 为 None 时无条件停当前流。
#[tauri::command]
pub async fn audio_stream_stop(app: AppHandle, id: Option<u64>) -> Result<bool, String> {
    crate::blocking::blocking(move || stop_by_id(&app, id)).await
}

/// 当前是否还有流在跑（前端 seek 打断后确认用）。
#[tauri::command]
pub async fn audio_stream_state(app: AppHandle, id: u64) -> Result<String, String> {
    crate::blocking::blocking(move || stream_state_now(&app, id)).await
}

fn stream_state_now(app: &AppHandle, id: u64) -> String {
    let state = app.state::<StreamState>();
    let shared = {
        let slot = lock(&state.0);
        match slot.as_ref() {
            Some(h) if h.id == id => h.shared.clone(),
            _ => return "gone".to_string(),
        }
    };
    let (l, _) = &*shared;
    let g = lock(l);
    if g.error.is_some() {
        "error".to_string()
    } else if g.eof {
        "eof".to_string()
    } else {
        "live".to_string()
    }
}
/// 关窗 / 退出时统一收尾（Job Object 另有兜底，这里负责主动 kill 和释放管道）。
pub fn shutdown(app: &AppHandle) {
    stop_by_id(app, None);
}

// ---- 实现 ----

fn start_impl(
    app: &AppHandle,
    video_path: String,
    audio_index: usize,
    start_secs: f64,
) -> Result<StreamInfo, String> {
    let src = PathBuf::from(&video_path);
    if !src.is_file() {
        return Err("源视频不存在".into());
    }
    if !start_secs.is_finite() || start_secs < 0.0 {
        return Err("起始时间不合法".into());
    }
    // 单流模型：新的一路先杀掉旧的（换轨 / 换片 / seek 重建都走这里）
    stop_by_id(app, None);

    let ffmpeg = crate::transcode::find_ffmpeg()?;
    let args = stream_args(&src, audio_index, start_secs);
    let mut cmd = crate::transcode::hidden_command(&ffmpeg);
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    #[cfg(windows)]
    crate::transcode::job::assign(&child);

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Err("ffmpeg 没有 stdout 管道".into());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        return Err("ffmpeg 没有 stderr 管道".into());
    };

    let id = STREAM_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let cancel = Arc::new(AtomicBool::new(false));
    let child_slot = Arc::new(Mutex::new(Some(child)));

    let s2 = shared.clone();
    let c2 = cancel.clone();
    let ch2 = child_slot.clone();
    std::thread::spawn(move || worker(stdout, stderr, s2, c2, ch2));

    let handle = StreamHandle { id, shared, child: child_slot, cancel };
    *lock(&app.state::<StreamState>().0) = Some(handle);

    Ok(StreamInfo { id, mime: MIME_AAC.into(), start_secs })
}

fn read_impl(app: &AppHandle, id: u64, max_bytes: usize) -> Result<Vec<u8>, String> {
    let shared = {
        let state = app.state::<StreamState>();
        let slot = lock(&state.0);
        match slot.as_ref() {
            Some(h) if h.id == id => h.shared.clone(),
            // 已经 stop 掉了：当成流结束，前端不会误以为是错误
            _ => return Ok(Vec::new()),
        }
    };
    let (l, cv) = &*shared;
    let mut g = lock(l);
    loop {
        if let Some(err) = g.error.clone() {
            return Err(err);
        }
        if !g.chunks.is_empty() {
            let mut out: Vec<u8> = Vec::new();
            while let Some(chunk) = g.chunks.pop_front() {
                // 第一块无论多大都要取：单个 fragment 不能拆给 MSE
                if !out.is_empty() && out.len() + chunk.len() > max_bytes {
                    g.chunks.push_front(chunk);
                    break;
                }
                g.bytes = g.bytes.saturating_sub(chunk.len());
                out.extend_from_slice(&chunk);
                if out.len() >= max_bytes {
                    break;
                }
            }
            // 唤醒可能卡在背压里的读线程
            cv.notify_all();
            return Ok(out);
        }
        if g.eof {
            return Ok(Vec::new());
        }
        let (g2, res) = cv.wait_timeout(g, WAIT_TIMEOUT).unwrap_or_else(|e| e.into_inner());
        g = g2;
        if res.timed_out() && g.chunks.is_empty() && !g.eof && g.error.is_none() {
            return Err("音频转码长时间没有输出".into());
        }
    }
}

fn stop_by_id(app: &AppHandle, id: Option<u64>) -> bool {
    let taken = {
        let state = app.state::<StreamState>();
        let mut slot = lock(&state.0);
        match slot.as_ref() {
            // id 不匹配：这是旧流迟到的 stop，别把新流误杀
            Some(h) if id.is_some_and(|want| want != h.id) => return false,
            None => return false,
            Some(_) => slot.take(),
        }
    };
    let Some(h) = taken else { return false };
    h.cancel.store(true, Ordering::Relaxed);
    {
        let (l, cv) = &*h.shared;
        let mut g = lock(l);
        g.cancelled = true;
        cv.notify_all();
    }
    // kill 在锁外做：等子进程退出可能要几百毫秒，拿锁会把前端其它命令一起卡住
    let child = lock(&h.child).take();
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    true
}

/// 读线程：ffmpeg stdout → 按 fragment 切块 → 队列；stderr 另起一线程收集错误。
fn worker(
    stdout: ChildStdout,
    stderr: ChildStderr,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    cancel: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<Child>>>,
) {
    // stderr 必须立刻有人读，否则 ffmpeg 写满错误管道会卡死
    let err_thread = std::thread::spawn(move || crate::transcode::drain_stderr(stderr));
    let read_result = read_fragments(stdout, &shared, &cancel);
    let err_tail = err_thread.join().unwrap_or_default();
    // stdout 关了才说明 ffmpeg 真的结束；wait 顺便回收子进程
    let status = lock(&child_slot).take().and_then(|mut c| c.wait().ok());

    let (l, cv) = &*shared;
    let mut g = lock(l);
    if g.cancelled || cancel.load(Ordering::Relaxed) {
        cv.notify_all();
        return;
    }
    match read_result {
        Err(e) => g.error = Some(e),
        Ok(()) => {
            if let Some(st) = status {
                if !st.success() {
                    g.error = Some(crate::transcode::last_err_line(&err_tail));
                }
            }
            g.eof = true;
        }
    }
    cv.notify_all();
}

fn read_fragments(
    mut stdout: ChildStdout,
    shared: &Arc<(Mutex<Shared>, Condvar)>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut splitter = FragmentSplitter::new();
    let mut ready: Vec<Vec<u8>> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        // 背压：队列积压到上限就停读 stdout，ffmpeg 写满管道自己阻塞
        if !wait_for_room(shared, cancel) {
            return Ok(());
        }
        let Some((typ, bytes)) = read_box(&mut stdout).map_err(|e| format!("读取 ffmpeg 输出失败: {e}"))? else {
            break;
        };
        splitter.push(&typ, &bytes, &mut ready);
        for chunk in ready.drain(..) {
            if !push_chunk(shared, chunk, cancel) {
                return Ok(());
            }
        }
    }
    splitter.flush(&mut ready);
    for chunk in ready.drain(..) {
        if !push_chunk(shared, chunk, cancel) {
            return Ok(());
        }
    }
    Ok(())
}

/// 等队列腾出空间。返回 false 表示已被取消。
fn wait_for_room(shared: &Arc<(Mutex<Shared>, Condvar)>, cancel: &AtomicBool) -> bool {
    let (l, cv) = &**shared;
    let mut g = lock(l);
    while g.bytes >= MAX_BUFFERED_BYTES {
        if g.cancelled || cancel.load(Ordering::Relaxed) {
            return false;
        }
        let (g2, _) = cv.wait_timeout(g, Duration::from_millis(100)).unwrap_or_else(|e| e.into_inner());
        g = g2;
    }
    !(g.cancelled || cancel.load(Ordering::Relaxed))
}

fn push_chunk(shared: &Arc<(Mutex<Shared>, Condvar)>, data: Vec<u8>, cancel: &AtomicBool) -> bool {
    let (l, cv) = &**shared;
    let mut g = lock(l);
    if g.cancelled || cancel.load(Ordering::Relaxed) {
        return false;
    }
    g.bytes += data.len();
    g.chunks.push_back(data);
    cv.notify_all();
    true
}

// ---- fMP4 切块 ----

/// 把连续的 box 切成「init 一次 + 每 fragment 一块」。
///
/// ffmpeg 用 `+empty_moov+delay_moov` 时，moov 要等第一帧解析出 `dac3/dac3` 之类的
/// codec 配置盒才写得出来，所以 init（ftyp + moov）会和第一个 fragment 一起到达。
/// 切块规则就一条：遇到 moof 时，它前面累积的全是 init；mdat 收完就是一个完整 fragment。
struct FragmentSplitter {
    pending: Vec<u8>,
    init_sent: bool,
}

impl FragmentSplitter {
    fn new() -> Self {
        Self { pending: Vec::new(), init_sent: false }
    }

    fn push(&mut self, typ: &[u8], bytes: &[u8], out: &mut Vec<Vec<u8>>) {
        if typ == b"moof" && !self.init_sent {
            if !self.pending.is_empty() {
                out.push(std::mem::take(&mut self.pending));
            }
            self.init_sent = true;
        }
        // 播放用不到顶层 mfra（fMP4 的文件索引，ffmpeg 会追加在流末尾）。
        // 塞给 MSE 没有意义，还可能被 source buffer 判成非法 box。
        if self.init_sent && typ != b"moof" && typ != b"mdat" {
            return;
        }
        self.pending.extend_from_slice(bytes);
        // mdat 收完就是一个完整 fragment，MSE 可以立刻吃，不必等下一段
        if self.init_sent && typ == b"mdat" {
            out.push(std::mem::take(&mut self.pending));
        }
    }

    /// 管道结束时把残块交出去（正常情况不会有：fragment 都以 mdat 收尾）。
    fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.pending.is_empty() {
            out.push(std::mem::take(&mut self.pending));
        }
    }
}

/// 从管道读一个完整的 ISO-BMFF box，返回 (类型, 整个 box 字节)。
/// box 会跨 read 边界，必须按 size 精确读满，不能假设一次 read 就是一个 box。
fn read_box(r: &mut impl Read) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut hdr = [0u8; 8];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let typ = hdr[4..8].to_vec();
    let mut out = hdr.to_vec();
    let size = if size32 == 1 {
        let mut ext = [0u8; 8];
        if !read_full(r, &mut ext)? {
            return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "box largesize 被截断"));
        }
        out.extend_from_slice(&ext);
        u64::from_be_bytes(ext)
    } else if size32 == 0 {
        // size 0 = 延伸到 EOF，fMP4 管道里不该出现；出现就是数据损坏
        return Err(std::io::Error::new(ErrorKind::InvalidData, "box size 为 0（不支持延伸到 EOF）"));
    } else {
        size32 as u64
    };
    let head_len = out.len() as u64;
    if size < head_len {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("box size {size} 小于头部 {head_len}"),
        ));
    }
    let body_len = size - head_len;
    if body_len > MAX_BOX_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("box 体积异常：{body_len} 字节"),
        ));
    }
    let body_start = out.len();
    out.resize(body_start + body_len as usize, 0);
    if !read_full(r, &mut out[body_start..])? {
        return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "box 体被截断"));
    }
    Ok(Some((typ, out)))
}

/// 读满整个 buf；返回 false 表示读之前就 EOF（一个字节都没有）。
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => return Ok(false),
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

// ---- ffmpeg 参数 ----

/// 流式转 AAC 的 fMP4 参数。
///
/// `-ss` 放在 `-i` 之前（输入端快速 seek）：MKV 有 Cues 索引，能直接跳到目标附近，
/// 不必从片头解一遍；ffmpeg 会在输出侧把时间戳归零，前端再用 timestampOffset 摆位置。
///
/// `-ac 2` 是硬要求：WebView2（Media Foundation）解码上限 6 声道，7.1 编出来是静音。
/// `-frag_duration` 单位是微秒（1 秒一块），配合 `-flush_packets 1` 才能从管道即时吐出来。
/// `-map_chapters -1` 必须加：否则 mov muxer 会把 MKV 章节写成一条 QuickTime text 轨。
fn stream_args(src: &Path, audio_index: usize, start_secs: f64) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-ss".into(),
        format!("{start_secs:.3}"),
        "-i".into(),
        src.to_string_lossy().into_owned(),
        "-map".into(),
        format!("0:a:{audio_index}"),
        "-map_chapters".into(),
        "-1".into(),
        "-c:a".into(),
        "aac".into(),
        "-ac".into(),
        "2".into(),
        "-b:a".into(),
        "192k".into(),
        "-ar".into(),
        "48000".into(),
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        "+frag_keyframe+empty_moov+default_base_moof".into(),
        "-frag_duration".into(),
        "1000000".into(),
        "-flush_packets".into(),
        "1".into(),
        "pipe:1".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_bytes(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (8 + body.len()) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn reads_one_box_at_a_time() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&box_bytes(b"ftyp", b"isom"));
        stream.extend_from_slice(&box_bytes(b"moov", b"1234567890"));
        let mut cur = std::io::Cursor::new(stream);

        let (t1, b1) = read_box(&mut cur).unwrap().unwrap();
        assert_eq!(t1, b"ftyp");
        assert_eq!(b1, box_bytes(b"ftyp", b"isom"));

        let (t2, b2) = read_box(&mut cur).unwrap().unwrap();
        assert_eq!(t2, b"moov");
        assert_eq!(b2, box_bytes(b"moov", b"1234567890"));

        assert!(read_box(&mut cur).unwrap().is_none(), "读完后应返回 None");
    }

    /// 64 位 largesize 头：size 字段为 1 时后面跟 8 字节真实长度
    #[test]
    fn reads_largesize_box() {
        let body = b"abcdefgh";
        let mut v = Vec::new();
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&(16u64 + body.len() as u64).to_be_bytes());
        v.extend_from_slice(body);
        let mut cur = std::io::Cursor::new(v);
        let (t, b) = read_box(&mut cur).unwrap().unwrap();
        assert_eq!(t, b"mdat");
        assert_eq!(b.len(), 16 + body.len());
    }

    /// 被截断的 box 必须报错，不能把半个 body 当成完整块交给 MSE
    #[test]
    fn truncated_box_is_an_error() {
        let mut v = Vec::new();
        v.extend_from_slice(&100u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(b"short");
        let mut cur = std::io::Cursor::new(v);
        assert!(read_box(&mut cur).is_err());
    }

    #[test]
    fn empty_stream_is_clean_eof() {
        let mut cur = std::io::Cursor::new(Vec::new());
        assert!(read_box(&mut cur).unwrap().is_none());
    }

    /// init 必须单独成块且排在所有 fragment 之前（MSE 只接受一次 appendSourceBuffer）
    #[test]
    fn splitter_emits_init_before_fragments() {
        let mut sp = FragmentSplitter::new();
        let mut out = Vec::new();
        sp.push(b"ftyp", &box_bytes(b"ftyp", b""), &mut out);
        assert!(out.is_empty(), "ftyp 之后还不知道 moov 有没有来，不能发");
        sp.push(b"moov", &box_bytes(b"moov", b"cfg"), &mut out);
        assert!(out.is_empty(), "moov 之后要到第一个 moof 才能确定 init 完整");

        sp.push(b"moof", &box_bytes(b"moof", b"m1"), &mut out);
        assert_eq!(out.len(), 1, "遇到第一个 moof 要把 init 单独交出去");
        let init = &out[0];
        assert_eq!(&init[4..8], b"ftyp");
        assert!(init.windows(4).any(|w| w == b"moov"));
        assert!(!init.windows(4).any(|w| w == b"moof"), "init 里不能混进 moof");

        out.clear();
        sp.push(b"mdat", &box_bytes(b"mdat", b"data1"), &mut out);
        assert_eq!(out.len(), 1, "mdat 收完就是一个完整 fragment");
        assert_eq!(&out[0][4..8], b"moof", "fragment 必须以 moof 开头");
        assert!(out[0].windows(4).any(|w| w == b"mdat"));
    }

    /// 多个 fragment 必须一块一块地交，不能合并（合并会让前端缓冲粒度失控）
    #[test]
    fn splitter_keeps_fragments_separate() {
        let mut sp = FragmentSplitter::new();
        let mut out = Vec::new();
        sp.push(b"ftyp", &box_bytes(b"ftyp", b""), &mut out);
        sp.push(b"moov", &box_bytes(b"moov", b"cfg"), &mut out);
        sp.push(b"moof", &box_bytes(b"moof", b"m1"), &mut out);
        for i in 0..3 {
            out.clear();
            sp.push(b"mdat", &box_bytes(b"mdat", format!("d{i}").as_bytes()), &mut out);
            assert_eq!(out.len(), 1);
            assert!(out[0].windows(4).any(|w| w == b"mdat"));
            out.clear();
            sp.push(b"moof", &box_bytes(b"moof", format!("m{i}").as_bytes()), &mut out);
            assert!(out.is_empty(), "moof 本身不构成可交付块，要等它的 mdat");
        }
    }

    /// mfra 是 fMP4 的文件索引，只在文件末尾出现；播放时用不到，不能交给 MSE
    #[test]
    fn splitter_drops_mfra() {
        let mut sp = FragmentSplitter::new();
        let mut out = Vec::new();
        sp.push(b"ftyp", &box_bytes(b"ftyp", b""), &mut out);
        sp.push(b"moov", &box_bytes(b"moov", b"cfg"), &mut out);
        sp.push(b"moof", &box_bytes(b"moof", b"m1"), &mut out);
        sp.push(b"mdat", &box_bytes(b"mdat", b"d1"), &mut out);
        out.clear();
        sp.push(b"mfra", &box_bytes(b"mfra", b"idx"), &mut out);
        assert!(out.is_empty(), "mfra 不该单独成块");
        sp.flush(&mut out);
        assert!(out.is_empty(), "mfra 也不该混进最后的残块");
    }

    /// flush 只在管道异常结束时兜底，不能在前面的正常切块里多发
    #[test]
    fn splitter_flush_emits_leftover() {
        let mut sp = FragmentSplitter::new();
        let mut out = Vec::new();
        sp.push(b"ftyp", &box_bytes(b"ftyp", b""), &mut out);
        sp.push(b"moov", &box_bytes(b"moov", b"cfg"), &mut out);
        sp.push(b"moof", &box_bytes(b"moof", b"m1"), &mut out);
        out.clear();
        sp.flush(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][4..8], b"moof");
    }

    /// 参数是 ffmpeg 契约：少一个 flag 就是静音 / 无输出 / 弹章节轨，用测试钉住
    #[test]
    fn stream_args_are_complete() {
        let a = stream_args(Path::new(r"D:\影\a.mkv"), 2, 12.5);
        let joined = a.join(" ");
        assert!(joined.contains("-nostdin"), "必须不读 stdin");
        assert!(joined.contains("-map 0:a:2"), "必须按指定音轨号 map");
        assert!(joined.contains("-ac 2"), "超过 2 声道 WebView2 会静音");
        assert!(joined.contains("-b:a 192k"));
        assert!(joined.contains("-f mp4"));
        assert!(joined.contains("+frag_keyframe+empty_moov+default_base_moof"));
        assert!(joined.contains("-frag_duration 1000000"), "1 秒一块");
        assert!(joined.contains("-flush_packets 1"), "管道不加这个会攒着不出");
        assert!(joined.contains("-map_chapters -1"), "不加会把章节写成一条 text 轨");
        assert_eq!(a.last().unwrap(), "pipe:1");
        // -ss 必须在 -i 之前（输入端快速 seek）
        let ss = a.iter().position(|x| x == "-ss").unwrap();
        let ii = a.iter().position(|x| x == "-i").unwrap();
        assert!(ss < ii, "-ss 放 -i 后面就不是快速 seek 了");
    }
}
