// PGS（蓝光位图字幕）本地渲染：用 libpgs 从 MKV 里按 Cues 索引抽取显示集，解出位图
// 合成为 PNG 事件表。不落盘、不 OCR —— 只是把"Chromium 内核不认的图形字幕"还原成图片。
//
// 关键实测（潘神 UHD 35.6 GB，简体轨）：抽取 1669 个显示集只读 13.4 MB（文件的
// 0.038%），冷启动 7.3 s / 缓存热 50 ms；合成 884 张裁边 PNG 约 2.5 s，合计约 30 MB。
// 之所以能只读千分之零点四，是 libpgs 读 Cues 索引直接 seek 到含字幕块的位置，
// 不像 mkvextract/ffmpeg 那样顺序扫全片（那个要读 33 GB、139 秒）。
//
// 事件切分：显示集里出现 PCS 对象且"构图签名"（对象 id/版本/坐标 + 调色板）变化，
// 就算一条新事件；事件在下一次清屏显示集处结束。纯调色板更新（palette_only）
// 不产生新事件，只是重绘已有事件。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use libpgs::pgs::payload::ods_rle_data;
use libpgs::pgs::rle::decode_rle;
use libpgs::pgs::{CompositionState, PaletteEntry};
use tauri::{AppHandle, Manager, State};

/// 一条字幕事件：时间区间 + 位图在 PGS 画布上的位置尺寸（已裁掉透明边）。
#[derive(Serialize, Clone)]
pub struct PgsEvent {
    pub start: f64, // ms
    pub end: f64,   // ms
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 交给前端的索引：画布尺寸 + 事件表（不含图像，图像按需取）。
#[derive(Serialize, Clone)]
pub struct PgsIndex {
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub events: Vec<PgsEvent>,
}

pub struct PgsDoc {
    video: String,
    track: u32,
    index: PgsIndex,
    images: Vec<Vec<u8>>, // 与 events 同序的 PNG 字节
}

pub type PgsState = Mutex<Option<PgsDoc>>;

// ---- 请求代际 ----

/// PGS 请求代际号，只增不减。前端连点两条 PGS 轨时，先发出的那次 build 可能更慢：
/// 如果它一完成就无条件写 state，会把后发者已经写好的文档盖回去，`pgs_image` 随后
/// 按事件序号取到的是另一条轨的图（或图数对不上而静默无图）。前端的 `pgsToken`
/// 只挡 promise 回调，挡不住 Rust 侧状态被旧文档覆盖，所以这里必须自己认号。
static PGS_GEN: AtomicU64 = AtomicU64::new(0);

fn next_pgs_gen() -> u64 {
    PGS_GEN.fetch_add(1, Ordering::SeqCst) + 1
}

/// 取 PGS 文档锁。**不要再写裸 `lock().unwrap()`**。
///
/// 为什么要包一层：持锁期间一旦有命令 panic，Mutex 就永久中毒，之后每次
/// `lock().unwrap()` 都返回 Err 并 panic —— 四个 PGS 命令（open/image/close/current）
/// 会全部失灵，位图字幕直到重启应用都彻底不可用，而用户在界面上只看到"没字幕"。
///
/// 为什么中毒后还敢继续用：我们从不修改文档的"半个状态"，锁内要么整体替换
/// `*guard = Some(doc)` / `*guard = None`，要么只读。持锁 panic 最多让那次操作
/// 作废（open 的 build 还没走到写入；close 的置空是单条赋值），文档本身要么是
/// 上一份完整文档、要么是 None，不存在撕裂。所以取内部值继续用比让整个功能瘫痪安全。
///
/// 写成泛型而不是 `&State<'_, PgsState>`：签名里带 `State` 就没法在单测里构造
/// （`State` 的 Inner 是私有字段，测试里也拿不到 AppHandle），中毒行为就测不到了。
/// 泛型对 `&Mutex<Option<PgsDoc>>` 的调用点零成本，命令侧传 `state.inner()`。
fn lock_doc<T>(doc: &Mutex<T>) -> MutexGuard<'_, T> {
    doc.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- 资源上限 ----

/// 画布边长硬顶。蓝光最大 1920×1080，UHD 蓝光 3840×2160，8192 留足余量。
/// 尺寸是文件里的外部数据，不设限时损坏/恶意文件声明 65535×65535 会让
/// `Canvas::new` 申请约 17 GB（65535²×4），分配失败直接 abort 整个播放器。
const PGS_MAX_DIM: u32 = 8192;

/// 单轨事件数上限。每条事件都常驻一张 PNG，条数失控同样会吃光内存。
const PGS_MAX_EVENTS: usize = 200_000;

/// 位图总量上限。只限条数是不够的 —— 200000 条 × 30 KB 也能到 6 GB。
/// 真片实测约 884 条、30 MB，128 MB 对正常片源绰绰有余。
const PGS_MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

/// 画布像素缓冲上限（RGBA 临时缓冲，每合成一条新事件都要重新分配一次）。
///
/// 为什么必须有：`Canvas::new` 直接 `vec![0u8; w*h*4]`，而 Rust 的 `Vec` 分配失败
/// 不是可捕获的 `Err` 而是 **abort** —— 一次 268 MB（8192²×4）申请失败，整个播放器
/// 直接消失，用户连报错都看不到。只限边长（PGS_MAX_DIM）挡不住它：8192×8192 两边
/// 都合法，照样放行。
///
/// 为什么是 64 MB：真实片源蓝光 1920×1080（8.3 MB RGBA）、UHD 3840×2160
/// （33.2 MB RGBA）；64 MB 连 4096×2160（35.4 MB）、4096×4096（整 64 MB）都装得下，
/// 留出 2× UHD 的余量，同时把 8192² 的 268 MB 挡在门外。桌面另两个预算
/// （PNG 总量、对象总量）都是 128 MB；画布缓冲是单次瞬时分配、裁完 PNG 立刻释放，
/// 64 MB 不会和它们叠加成常驻峰值。手机版取 48 MB 是因为手机上进程里还有 WebView
/// （上百 MB），桌面内存相对宽裕，取 64 MB 既覆盖 4096×2160 也留了余量。
const PGS_MAX_CANVAS_BYTES: usize = 64 * 1024 * 1024;

/// 单个 epoch 内解码后的对象总量上限。对象尺寸已限 8192²（单对象 64 MB），
/// 但 id 空间有 65536 个，坏文件能堆出天文数字；真片几百 KB 到几 MB。
const PGS_MAX_OBJECT_BYTES: usize = 128 * 1024 * 1024;

fn check_canvas_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 || w > PGS_MAX_DIM || h > PGS_MAX_DIM {
        return Err(format!("PGS 画布尺寸异常（{w}×{h}），拒绝加载"));
    }
    // 边长合法不等于分配得起：8192×8192 的 RGBA 缓冲是 268 MB，一次 abort。
    // saturating_mul 是给 32 位 target 兜底的：65535²×4 在 usize=u32 上会溢出回绕，
    // 回绕出个小数字就可能放行；饱和相乘只会往大里算，超预算判定不会漏。
    if (w as usize).saturating_mul(h as usize).saturating_mul(4) > PGS_MAX_CANVAS_BYTES {
        return Err(format!(
            "PGS 画布 {w}×{h} 的像素缓冲超过 {} MB 上限，拒绝加载",
            PGS_MAX_CANVAS_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// ODS 对象尺寸也是文件声明的，`decode_rle` 会按 w*h 预分配像素缓冲，超限同样是
/// 一次 abort。0 表示空对象，解码器自己会返回空切片，照旧放行。
///
/// 与 `PGS_MAX_CANVAS_BYTES` 不冲突：那条管的是 `Canvas::new` 的一次性画布缓冲，
/// 这条管的是单个 ODS 对象的解码缓冲，两处分配彼此独立。单个对象仍只受边长
/// （PGS_MAX_DIM）约束而不套 64 MB 预算 —— 真片里对象是从画布上裁出来的一小块
/// （几十×几十像素），宽高不会超过画布；坏文件即便声明 8192² 的对象，也在
/// `decode_rle` 之前就受边长硬顶，解码出的字节还会被 PGS_MAX_OBJECT_BYTES 累计封顶。
fn object_size_reasonable(w: u16, h: u16) -> bool {
    w <= PGS_MAX_DIM as u16 && h <= PGS_MAX_DIM as u16
}

// ---- 位图合成 ----

fn ycbcr_to_rgb(y: u8, cr: u8, cb: u8) -> (u8, u8, u8) {
    let yf = y as f32;
    let crf = cr as f32 - 128.0;
    let cbf = cb as f32 - 128.0;
    let clamp = |v: f32| v.clamp(0.0, 255.0) as u8;
    (
        clamp(yf + 1.402 * crf),
        clamp(yf - 0.344_136 * cbf - 0.714_136 * crf),
        clamp(yf + 1.772 * cbf),
    )
}

struct Canvas {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, rgba: vec![0; w * h * 4] }
    }

    fn draw(&mut self, x: usize, y: usize, bw: usize, bh: usize, idx: &[u8], pal: &HashMap<u8, PaletteEntry>) {
        for row in 0..bh {
            let cy = y + row;
            if cy >= self.h {
                break;
            }
            for col in 0..bw {
                let cx = x + col;
                if cx >= self.w {
                    break;
                }
                let Some(e) = pal.get(&idx[row * bw + col]) else { continue };
                if e.alpha == 0 {
                    continue;
                }
                let (r, g, b) = ycbcr_to_rgb(e.luminance, e.cr, e.cb);
                let o = (cy * self.w + cx) * 4;
                self.rgba[o] = r;
                self.rgba[o + 1] = g;
                self.rgba[o + 2] = b;
                self.rgba[o + 3] = e.alpha;
            }
        }
    }

    /// 透明像素的最小外接矩形（x, y, w, h）；全透明返回 None。
    fn ink_box(&self) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..self.h {
            let row = y * self.w;
            for x in 0..self.w {
                if self.rgba[(row + x) * 4 + 3] != 0 {
                    if x < x0 {
                        x0 = x;
                    }
                    if y < y0 {
                        y0 = y;
                    }
                    if x > x1 {
                        x1 = x;
                    }
                    if y > y1 {
                        y1 = y;
                    }
                }
            }
        }
        if x0 == usize::MAX {
            return None;
        }
        Some((x0 as u32, y0 as u32, (x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32))
    }

    fn crop_png(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for row in 0..h as usize {
            let src = ((y as usize + row) * self.w + x as usize) * 4;
            let dst = row * (w as usize) * 4;
            out[dst..dst + (w as usize) * 4].copy_from_slice(&self.rgba[src..src + (w as usize) * 4]);
        }
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(|e| e.to_string())?;
            writer.write_image_data(&out).map_err(|e| e.to_string())?;
        }
        Ok(buf)
    }
}

/// 构图签名：对象集合（id/版本/位置）+ 调色板版本决定这一帧画面是否变了。
fn signature(pcs: &libpgs::pgs::PcsData, palette_ver: u8) -> Vec<(u16, u8, u16, u16)> {
    let mut v: Vec<(u16, u8, u16, u16)> = pcs
        .objects
        .iter()
        .map(|o| (o.object_id, 0u8, o.x, o.y))
        .collect();
    v.push((u16::MAX, palette_ver, pcs.palette_id as u16, 0));
    v
}

// ---- 抽取 + 合成 ----

fn build(video: &str, track: u32) -> Result<PgsDoc, String> {
    let path = std::path::Path::new(video);
    if !path.is_file() {
        return Err("源视频不存在".into());
    }
    let sets = libpgs::extract_display_sets(path, Some(track)).map_err(|e| format!("抽取 PGS 失败: {e}"))?;
    if sets.is_empty() {
        return Err("这条字幕轨里没有可渲染的位图".into());
    }

    let mut palette: HashMap<u8, PaletteEntry> = HashMap::new();
    let mut palette_ver: u8 = 0;
    let mut objects: HashMap<u16, (usize, usize, Vec<u8>)> = HashMap::new();
    let mut objects_ver: HashMap<u16, u8> = HashMap::new();
    let mut events: Vec<PgsEvent> = Vec::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let (mut canvas_w, mut canvas_h) = (0u32, 0u32);
    let mut current: Option<Vec<(u16, u8, u16, u16)>> = None;
    // 累计已吃下的内存，超预算就整轨放弃（见上面两个常量）
    let mut object_bytes: usize = 0;
    let mut image_bytes: usize = 0;

    for ds in &sets {
        if ds.composition_state == CompositionState::EpochStart {
            objects.clear();
            objects_ver.clear();
            palette.clear();
            object_bytes = 0; // 新 epoch 重新开始计数，旧对象已释放
        }
        for seg in &ds.segments {
            if let Some(pds) = seg.parse_pds() {
                palette_ver = pds.version;
                for e in pds.entries {
                    palette.insert(e.id, e);
                }
            }
        }
        // 组装可能被切成多段的 ODS
        let mut ids: Vec<u16> = Vec::new();
        let mut groups: HashMap<u16, Vec<&libpgs::pgs::PgsSegment>> = HashMap::new();
        for seg in &ds.segments {
            if let Some(ods) = seg.parse_ods() {
                if ods.width.is_some() && ods.height.is_some() && !groups.contains_key(&ods.id) {
                    ids.push(ods.id);
                }
                groups.entry(ods.id).or_default().push(seg);
            }
        }
        for id in ids {
            let group = &groups[&id];
            let Some(first) = group[0].parse_ods() else { continue };
            let (Some(w), Some(h)) = (first.width, first.height) else { continue };
            if !object_size_reasonable(w, h) {
                return Err(format!("PGS 对象尺寸异常（{w}×{h}），拒绝加载"));
            }
            let mut rle: Vec<u8> = Vec::new();
            for seg in group {
                if let Some(ods) = seg.parse_ods() {
                    if let Some(d) = ods_rle_data(&seg.payload, ods.sequence) {
                        rle.extend_from_slice(d);
                    }
                }
            }
            if let Some(pixels) = decode_rle(&rle, w, h) {
                object_bytes = object_bytes.saturating_add(pixels.len());
                if object_bytes > PGS_MAX_OBJECT_BYTES {
                    return Err(format!(
                        "PGS 对象总量超过 {} MB 上限，拒绝加载",
                        PGS_MAX_OBJECT_BYTES / (1024 * 1024)
                    ));
                }
                objects_ver.insert(id, first.version);
                objects.insert(id, (w as usize, h as usize, pixels));
            }
        }

        let Some(pcs) = ds.segments.iter().find_map(|s| s.parse_pcs()) else { continue };
        // 先校验再分配：画布是照文件声明申请的，别让坏文件把进程撑爆
        check_canvas_size(pcs.video_width as u32, pcs.video_height as u32)?;
        canvas_w = pcs.video_width as u32;
        canvas_h = pcs.video_height as u32;

        if pcs.objects.is_empty() {
            // 清屏：收尾当前事件
            if let Some(mut sig) = current.take() {
                sig.clear();
                if let Some(last) = events.last_mut() {
                    last.end = ds.pts_ms;
                }
            }
            continue;
        }

        // 把对象版本合进签名，避免同一对象内容换了坐标没变时漏判
        let mut sig = signature(&pcs, palette_ver);
        for entry in sig.iter_mut() {
            if entry.0 != u16::MAX {
                if let Some(v) = objects_ver.get(&entry.0) {
                    entry.1 = *v;
                }
            }
        }
        if current.as_ref() == Some(&sig) {
            continue; // 同一帧，只是刷新
        }

        let mut canvas = Canvas::new(canvas_w as usize, canvas_h as usize);
        let mut any = false;
        for obj in &pcs.objects {
            if let Some((w, h, px)) = objects.get(&obj.object_id) {
                canvas.draw(obj.x as usize, obj.y as usize, *w, *h, px, &palette);
                any = true;
            }
        }
        let Some((bx, by, bw, bh)) = (if any { canvas.ink_box() } else { None }) else {
            continue;
        };

        let png = canvas.crop_png(bx, by, bw, bh)?;
        if let Some(last) = events.last_mut() {
            if last.end <= last.start {
                last.end = ds.pts_ms; // 上一帧结束在这里
            }
        }
        if events.len() >= PGS_MAX_EVENTS {
            return Err(format!("PGS 事件数超过上限（{PGS_MAX_EVENTS}），拒绝继续合成"));
        }
        image_bytes = image_bytes.saturating_add(png.len());
        if image_bytes > PGS_MAX_IMAGE_BYTES {
            return Err(format!(
                "PGS 位图总量超过 {} MB 上限，拒绝继续合成",
                PGS_MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        events.push(PgsEvent { start: ds.pts_ms, end: ds.pts_ms + 2000.0, x: bx, y: by, w: bw, h: bh });
        images.push(png);
        current = Some(sig);
    }

    // 最后一条事件没有明确的结束时间：按常见字幕时长兜底
    if let Some(last) = events.last_mut() {
        if last.end > last.start + 30_000.0 {
            last.end = last.start + 8000.0;
        }
    }
    if events.is_empty() {
        return Err("这条字幕轨里没有可渲染的位图".into());
    }

    Ok(PgsDoc { video: video.to_string(), track, index: PgsIndex { canvas_w, canvas_h, events }, images })
}

// ---- 命令 ----

/// 打开某条 PGS 字幕轨：后台抽取并合成，返回事件索引（图像按需再取）。
#[tauri::command]
pub async fn pgs_open(app: AppHandle, video_path: String, track: u32) -> Result<PgsIndex, String> {
    // 号在 build 之前就取好：之后只要再来一次 open 或 close，号就比这个大了
    let my_gen = next_pgs_gen();
    let app2 = app.clone();
    let doc = tauri::async_runtime::spawn_blocking(move || build(&video_path, track))
        .await
        .map_err(|e| format!("PGS 任务失败: {e}"))??;
    let index = doc.index.clone();
    // 比对和写入必须在同一把锁里：否则「比对通过 → 另一个 open 取号 → 本单写入」
    // 这个交错照样会把新文档盖掉
    {
        let state = app2.state::<PgsState>();
        let mut guard = lock_doc(state.inner());
        if PGS_GEN.load(Ordering::SeqCst) != my_gen {
            return Err("PGS 解析已被更新的请求取代".into());
        }
        *guard = Some(doc);
    }
    Ok(index)
}

/// 取某条事件的 PNG（base64）。前端自己做 LRU，只有当前时间附近几条会被请求。
#[tauri::command]
pub fn pgs_image(state: State<'_, PgsState>, index: usize) -> Result<String, String> {
    let guard = lock_doc(state.inner());
    let doc = guard.as_ref().ok_or("没有加载 PGS 字幕")?;
    let bytes = doc.images.get(index).ok_or("事件序号越界")?;
    Ok(base64_encode(bytes))
}

/// 释放当前 PGS 文档（切片/关闭字幕时调用，30 MB 内存不常驻）
#[tauri::command]
pub fn pgs_close(state: State<'_, PgsState>) {
    // 先推高代际号，让还在 build 的 open 完成后自己作废；再比对一次是为了防止
    // 「close 取号之后又来了一次新 open」时把新 open 刚写好的文档关掉
    let my_gen = next_pgs_gen();
    let mut guard = lock_doc(state.inner());
    if PGS_GEN.load(Ordering::SeqCst) != my_gen {
        return;
    }
    *guard = None;
}

/// 当前加载的是哪条轨（换片判断用）
#[tauri::command]
pub fn pgs_current(state: State<'_, PgsState>) -> Option<(String, u32)> {
    let guard = lock_doc(state.inner());
    guard.as_ref().map(|d| (d.video.clone(), d.track))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_size_accepts_real_and_boundary_sizes() {
        // 只收真实片源尺寸：1×1 最小合法值、蓝光 1080p、UHD 2160p、DCI 4K。
        // 8192×8192 不再算"边界"—— 它是 268 MB 的 RGBA 缓冲，必须被预算拒掉。
        for (w, h) in [(1u32, 1u32), (1920, 1080), (2560, 1440), (3840, 2160), (4096, 2160)] {
            assert!(check_canvas_size(w, h).is_ok(), "{w}×{h} 应被接受");
        }
    }

    #[test]
    fn canvas_size_rejects_zero_and_absurd_sizes() {
        for (w, h) in [
            (0u32, 0u32),
            (0, 1080),
            (1920, 0),
            (65535, 65535),
            (8193, 100),
            (100, 8193),
            // 边长都在 8192 硬顶以内，但像素缓冲 8192²×4 = 268 MB，必须被拒。
            (8192, 8192),
            (8192, 8191),
            (7680, 4320),
        ] {
            assert!(check_canvas_size(w, h).is_err(), "{w}×{h} 应被拒绝");
        }
    }

    /// 画布预算的边界语义：UHD 真实片源必须通过（否则误伤正片），刚好超过
    /// 预算的尺寸必须被拒（否则 8192² 那类 268 MB 的单次分配又能进来 abort 进程）。
    #[test]
    fn canvas_byte_budget_rejects_over_budget_and_keeps_uhd() {
        // 真实片源必须过：UHD 3840×2160 的 RGBA 缓冲 33.2 MB。
        assert!(check_canvas_size(3840, 2160).is_ok(), "UHD 3840×2160 必须通过");
        // 预算本身要能覆盖 UHD，又必须小于 8192² 的 268 MB。
        assert!(
            PGS_MAX_CANVAS_BYTES >= 3840 * 2160 * 4,
            "预算至少要装下 UHD 画布（{} MB）",
            PGS_MAX_CANVAS_BYTES / (1024 * 1024)
        );
        assert!(PGS_MAX_CANVAS_BYTES < 8192 * 8192 * 4, "预算必须挡住 8192² 的 268 MB");
        // 恰好整预算（64 MB = 4096×4096）：检查是"大于才拒"，边界要放行。
        assert_eq!(4096usize * 4096 * 4, PGS_MAX_CANVAS_BYTES);
        assert!(check_canvas_size(4096, 4096).is_ok(), "整预算画布应被接受");
        // 只超一行像素（4097×4096 = 67,125,248 > 64 MB）就要拒绝，
        // 证明拦截是逐字节预算，而不是又一个边长阈值。
        assert!(check_canvas_size(4097, 4096).is_err(), "超过预算 1 行像素应被拒绝");
        assert!(check_canvas_size(4096, 4097).is_err(), "超过预算 1 列像素应被拒绝");
        // 8192×2160 = 70.8 MB，边长全合法、只超预算。
        assert!(check_canvas_size(8192, 2160).is_err(), "8192×2160（70.8 MB）应被拒绝");
    }

    #[test]
    fn object_size_guard_allows_empty_but_rejects_oversized() {
        assert!(object_size_reasonable(0, 0));
        assert!(object_size_reasonable(1920, 1080));
        assert!(object_size_reasonable(8192, 8192));
        assert!(!object_size_reasonable(8193, 100));
        assert!(!object_size_reasonable(65535, 65535));
    }

    #[test]
    fn pgs_generation_only_increases() {
        let a = next_pgs_gen();
        let b = next_pgs_gen();
        assert!(b > a, "代际号必须单调递增：{a} -> {b}");
    }

    // ---- mutex 中毒 ----

    /// 造一个"中毒"的文档锁：在当前线程持锁后制造一次 panic（`catch_unwind` 接住，
    /// 锁因此永久中毒）。用 `resume_unwind` 而不是 `panic!`，因为前者不触发 panic
    /// hook —— 测试输出里不会多一行红字，也不去改全局 hook（并行的其它测试不受影响）。
    fn poisoned_doc_mutex() -> Mutex<Option<PgsDoc>> {
        let doc = PgsDoc {
            video: "毒化测试.mkv".into(),
            track: 3,
            index: PgsIndex {
                canvas_w: 1920,
                canvas_h: 1080,
                events: vec![PgsEvent { start: 0.0, end: 1000.0, x: 8, y: 900, w: 40, h: 20 }],
            },
            images: vec![vec![0x89, b'P', b'N', b'G']],
        };
        let m = Mutex::new(Some(doc));
        let guard = std::panic::catch_unwind(|| {
            let _held = m.lock().unwrap();
            std::panic::resume_unwind(Box::new("模拟持锁期间 panic"));
        });
        assert!(guard.is_err(), "持锁线程应该 panic 掉");
        assert!(m.is_poisoned(), "持锁 panic 后 Mutex 必须已中毒");
        m
    }

    /// 缺陷现场：裸 `lock().unwrap()` 在中毒后每次都返回 Err，于是四个 PGS 命令
    /// 每次调用都 panic，位图字幕直到重启应用都彻底不可用。
    #[test]
    fn poisoned_mutex_breaks_bare_unwrap() {
        let m = poisoned_doc_mutex();
        assert!(m.lock().is_err(), "中毒后裸 lock() 应返回 Err（裸 unwrap 必 panic）");
    }

    /// 修复后：`lock_doc` 必须在中毒状态下照常交出文档，而不是跟着 panic。
    /// 用局部 Mutex 构造，不碰 Tauri 的 managed state（单测里拿不到 AppHandle），
    /// 也不碰模块级全局状态，所以不会污染其它测试。
    #[test]
    fn poisoned_lock_still_yields_document() {
        let m = poisoned_doc_mutex();
        {
            let guard = lock_doc(&m);
            let doc = guard.as_ref().expect("中毒后仍应取到文档");
            assert_eq!(doc.track, 3);
            assert_eq!(doc.video, "毒化测试.mkv");
            assert_eq!(doc.index.events.len(), 1);
        }
        // 中毒后写入同样要生效（对应 pgs_open 覆盖 / pgs_close 置 None）
        {
            let mut guard = lock_doc(&m);
            *guard = None;
        }
        assert!(lock_doc(&m).is_none(), "中毒后写入也必须生效");
    }

    /// 端到端：用 libpgs 自己的编码器造一条最小 PGS 轨（1920×1080 画布、40×20 实心块
    /// 放在 (100,900)、PTS 2000 ms），真写成 `.sup` 落盘，再让 `build()` 走一遍
    /// 抽取 → RLE 解码 → 画布合成 → 裁边 → PNG 编码的完整链路。
    ///
    /// 以前这条链路只有靠真片源才跑得到（`pgs_open` 是 Tauri 命令 + AppHandle，
    /// 单测里根本调不动），而"编译通过但画面没图"恰恰是这里最容易出的问题。
    /// 这条测试不需要任何外部素材，所以在 CI / 日常 `cargo test` 里默认生效。
    ///
    /// 1920×1080 的 RGBA 画布缓冲是 8.3 MB，必须通过 `check_canvas_size` 的 64 MB
    /// 像素缓冲预算 —— 这条测试要是被预算拒了，说明预算算错了（误伤正片尺寸），
    /// 而不是测试该改小画布。
    #[test]
    fn synthetic_sup_decodes_to_png_event() {
        use libpgs::pgs::{
            CompositionObject, DisplaySetBuilder, ObjectBitmap, PcsData, PdsData, WindowDefinition,
            WdsData,
        };

        let pcs = PcsData {
            video_width: 1920,
            video_height: 1080,
            composition_number: 0,
            composition_state: CompositionState::EpochStart,
            palette_only: false,
            palette_id: 0,
            objects: vec![CompositionObject { object_id: 1, window_id: 0, x: 100, y: 900, crop: None }],
        };
        let wds = WdsData {
            windows: vec![WindowDefinition { id: 0, x: 100, y: 900, width: 40, height: 20 }],
        };
        // 调色板 1 号：BT.601 白 (Y=235, Cb=Cr=128)，完全不透明
        let pds = PdsData {
            id: 0,
            version: 0,
            entries: vec![PaletteEntry { id: 1, luminance: 235, cr: 128, cb: 128, alpha: 255 }],
        };
        let obj = ObjectBitmap { id: 1, version: 0, width: 40, height: 20, pixels: vec![1u8; 40 * 20] };
        // PGS 时间戳是 90 kHz 时钟：2000 ms -> 180000
        let ds = DisplaySetBuilder::new(2_000 * 90)
            .pcs(pcs)
            .wds(wds)
            .palette(pds)
            .object(obj)
            .build()
            .expect("构造显示集");

        // build() 会先查源文件存在，所以 .sup 必须真写盘
        let path = std::env::temp_dir()
            .join(format!("bplayer_desktop_pgs_synthetic_{}.sup", std::process::id()));
        libpgs::write_sup_file(&[ds], &path).expect("写 SUP");
        let path_str = path.to_str().unwrap().to_string();
        let doc = build(&path_str, 0).expect("合成 PGS 位图");
        let _ = std::fs::remove_file(&path);

        // build() 记录的是请求参数本身（换片判断要按它比）
        assert_eq!(doc.track, 0);
        assert_eq!(doc.video, path_str);

        assert_eq!((doc.index.canvas_w, doc.index.canvas_h), (1920, 1080));
        assert_eq!(doc.index.events.len(), 1);
        let e = &doc.index.events[0];
        // 位置尺寸是裁掉透明边后的对象外接矩形；未清屏时默认 2 s 时长
        assert_eq!((e.x, e.y, e.w, e.h), (100, 900, 40, 20));
        assert_eq!((e.start, e.end), (2000.0, 4000.0));

        assert_eq!(&doc.images[0][..8], b"\x89PNG\r\n\x1a\n", "应是 PNG 魔数");
        let mut reader = png::Decoder::new(&doc.images[0][..]).read_info().expect("读 PNG 头");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("解 PNG 数据");
        assert_eq!((info.width, info.height), (40, 20));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(buf[3], 255, "调色板 alpha=255 的像素应不透明");
        assert_eq!(&buf[..3], &[235, 235, 235], "Y=235 的中性色应还原成白");

        // 前端拿到的 base64 必须是裸串（前缀由前端自己拼），且能解回同一张 PNG
        let b64 = base64_encode(&doc.images[0]);
        assert!(!b64.starts_with("data:"), "pgs_image 不能带 data URL 前缀");
        assert!(b64.starts_with("iVBORw0KGgo"), "PNG 的 base64 应以 iVBORw0KGgo 开头");
    }
}
