// 文本字幕的稀疏抽取：MKV 的 Cues 索引记着每条字幕 block 的精确字节位置
// （CueClusterPosition + CueRelativePosition），直接 seek 过去只读那几 KB，
// 不必像 ffmpeg 那样顺序扫完整片。
//
// 实测 Free Guy（27.95 GB，1902 条 subrip）：ffmpeg 195.6 s → 0.19 s，
// 产物与原流程逐字节相同（SHA256 一致）。
//
// 回退：Cues 里没有该轨的索引（只索引了视频关键帧）时返回 Err，调用方走 ffmpeg。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const ID_SEGMENT: u64 = 0x1853_8067;
const ID_SEEK_HEAD: u64 = 0x114D_9B74;
const ID_SEEK: u64 = 0x4DBB;
const ID_SEEK_ID: u64 = 0x53AB;
const ID_SEEK_POS: u64 = 0x53AC;
const ID_CUES: u64 = 0x1C53_BB6B;
const ID_CUE_POINT: u64 = 0xBB;
const ID_CUE_TIME: u64 = 0xB3;
const ID_CUE_TRACK_POS: u64 = 0xB7;
const ID_CUE_TRACK: u64 = 0xF7;
const ID_CUE_CLUSTER: u64 = 0xF1;
const ID_CUE_REL: u64 = 0xF0;
const ID_CLUSTER: u64 = 0x1F43_B675;
const ID_CLUSTER_TS: u64 = 0xE7;
const ID_BLOCK_GROUP: u64 = 0xA0;
const ID_BLOCK: u64 = 0xA1;
const ID_SIMPLE_BLOCK: u64 = 0xA3;
const ID_BLOCK_DURATION: u64 = 0x9B;
const ID_INFO: u64 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u64 = 0x2AD7_B1;

/// Cues 表超过这个大小就不解析了，宁可整片回退 ffmpeg 也不产出半份字幕
const MAX_CUES_BYTES: usize = 64 * 1024 * 1024;

const CUE_BYTES: &[u8] = &[0x1C, 0x53, 0xBB, 0x6B];

/// EBML 变长整数。`keep` 为真时保留长度标记位（用于元素 ID）。
fn vint(buf: &[u8], pos: usize, keep: bool) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let mut len = 1usize;
    let mut mask = 0x80u8;
    while first & mask == 0 {
        mask >>= 1;
        len += 1;
        if len > 8 {
            return None;
        }
    }
    let mut val = if keep { first as u64 } else { (first & (mask - 1)) as u64 };
    for k in 1..len {
        val = (val << 8) | *buf.get(pos + k)? as u64;
    }
    Some((val, pos + len))
}

/// 迭代 EBML 子元素：(id, payload 起点, payload 结束点或 None=长度未知/越界)
fn children(buf: &[u8], mut pos: usize, end: usize) -> Vec<(u64, usize, Option<usize>)> {
    let mut out = Vec::new();
    while pos < end {
        let Some((id, p2)) = vint(buf, pos, true) else { break };
        let Some((size, p3)) = vint(buf, p2, false) else { break };
        // 未知长度（尺寸位全 1）会算出超大值，超界即视为不可解析，直接收尾
        let stop = p3.checked_add(size as usize).filter(|e| *e <= end);
        out.push((id, p3, stop));
        match stop {
            Some(e) => pos = e,
            None => break,
        }
    }
    out
}

/// 找子元素；长度未知（Segment 这种流式写法）时按父范围收尾
fn find_child(buf: &[u8], start: usize, end: usize, want: u64) -> Option<(usize, usize)> {
    for (id, ds, de) in children(buf, start, end) {
        if id == want {
            return Some((ds, de.unwrap_or(end).min(buf.len())));
        }
    }
    None
}

fn read_uint(buf: &[u8], start: usize, end: usize) -> u64 {
    let mut v = 0u64;
    for b in &buf[start..end.min(buf.len())] {
        v = (v << 8) | *b as u64;
    }
    v
}

/// 通过 SeekHead 找到 Cues 的绝对位置，同时返回 Segment 数据区起点和 TimestampScale。
/// TimestampScale 必须读到：所有时间戳换算成毫秒都靠它，猜错了整条字幕的时间轴会整体偏移。
fn locate_cues(f: &mut File) -> Option<(u64, u64, u64)> {
    let mut head = vec![0u8; 2 * 1024 * 1024];
    f.seek(SeekFrom::Start(0)).ok()?;
    let n = f.read(&mut head).ok()?;
    head.truncate(n);

    let seg = find_child(&head, 0, head.len(), ID_SEGMENT)?;
    let seg_data = seg.0 as u64;

    // Info → TimestampScale（缺省按规范是 1000000 ns = 1 ms）
    let mut timestamp_scale = 1_000_000u64;
    if let Some((ids, ide)) = find_child(&head, seg_data as usize, head.len(), ID_INFO) {
        if let Some((s, e)) = find_child(&head, ids, ide, ID_TIMESTAMP_SCALE) {
            timestamp_scale = read_uint(&head, s, e);
        }
    }

    for (id, ds, de) in children(&head, seg_data as usize, head.len()) {
        if id != ID_SEEK_HEAD {
            continue;
        }
        let sh_end = de.unwrap_or(head.len());
        for (sid, sds, sde) in children(&head, ds, sh_end) {
            if sid != ID_SEEK {
                continue;
            }
            let seek_end = sde.unwrap_or(sh_end);
            let mut seek_id: Option<&[u8]> = None;
            let mut seek_pos: Option<u64> = None;
            for (cid, cds, cde) in children(&head, sds, seek_end) {
                let e = cde.unwrap_or(seek_end);
                match cid {
                    ID_SEEK_ID => seek_id = Some(&head[cds..e]),
                    ID_SEEK_POS => seek_pos = Some(read_uint(&head, cds, e)),
                    _ => {}
                }
            }
            if seek_id == Some(CUE_BYTES) {
                if let Some(p) = seek_pos {
                    return Some((seg_data + p, seg_data, timestamp_scale));
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
struct CuePoint {
    cluster: u64,
    relative: Option<u64>,
    #[allow(dead_code)]
    time: u64,
}

fn read_cues(f: &mut File, cues_pos: u64, track: u32) -> Result<Vec<CuePoint>, String> {
    // 先读元素头确认长度，再一次读进来（Cues 常在几 MB 内）
    let mut head = [0u8; 64];
    f.seek(SeekFrom::Start(cues_pos)).map_err(|e| e.to_string())?;
    let hn = f.read(&mut head).map_err(|e| e.to_string())?;
    let (id, p2) = vint(&head[..hn], 0, true).ok_or("Cues 头部损坏")?;
    if id != ID_CUES {
        return Err("SeekHead 指向的不是 Cues".into());
    }
    let (size, p3) = vint(&head[..hn], p2, false).ok_or("Cues 尺寸损坏")?;
    // 元素总长 = payload 起点偏移（相对 cues_pos）+ payload 大小，比"size + 64"精确
    let total = p3.checked_add(size as usize).ok_or("Cues 尺寸溢出")?;
    // 超限或读不满都直接放弃，宁可整片回退也不产出半份字幕
    if total > MAX_CUES_BYTES {
        return Err(format!("Cues 超过 {} MB，回退整片抽取", MAX_CUES_BYTES / 1024 / 1024));
    }
    let mut buf = vec![0u8; total];
    f.seek(SeekFrom::Start(cues_pos)).map_err(|e| e.to_string())?;
    let n = f.read(&mut buf).map_err(|e| e.to_string())?;
    if n < total {
        return Err("Cues 读取不完整".into());
    }
    buf.truncate(n);

    let top = children(&buf, 0, buf.len());
    let Some((_, cds, cde)) = top.into_iter().next() else { return Err("Cues 为空".into()) };
    let end = cde.unwrap_or(buf.len());

    let mut out = Vec::new();
    for (id, ds, de) in children(&buf, cds, end) {
        if id != ID_CUE_POINT {
            continue;
        }
        let point_end = de.unwrap_or(end);
        let mut cue_time = 0u64;
        for (sid, sds, sde) in children(&buf, ds, point_end) {
            if sid == ID_CUE_TIME {
                cue_time = read_uint(&buf, sds, sde.unwrap_or(point_end));
            }
        }
        for (sid, sds, sde) in children(&buf, ds, point_end) {
            if sid != ID_CUE_TRACK_POS {
                continue;
            }
            let pos_end = sde.unwrap_or(point_end);
            let (mut t, mut cluster, mut rel) = (None, None, None);
            for (cid, cds2, cde2) in children(&buf, sds, pos_end) {
                let e2 = cde2.unwrap_or(pos_end);
                match cid {
                    ID_CUE_TRACK => t = Some(read_uint(&buf, cds2, e2)),
                    ID_CUE_CLUSTER => cluster = Some(read_uint(&buf, cds2, e2)),
                    ID_CUE_REL => rel = Some(read_uint(&buf, cds2, e2)),
                    _ => {}
                }
            }
            if t == Some(track as u64) {
                if let Some(c) = cluster {
                    out.push(CuePoint { cluster: c, relative: rel, time: cue_time });
                }
            }
        }
    }
    Ok(out)
}

/// 解析一个 block：返回 (track, 相对时间, 时长, 文本负载)
fn parse_block(buf: &[u8], start: usize, _track_hint: u32) -> Option<(u32, i64, Option<u64>, Vec<u8>)> {
    let mut eid = 0u64;
    let mut ds = 0usize;
    let mut de = 0usize;
    let mut duration: Option<u64> = None;
    for (id, s, e) in children(buf, start, buf.len()) {
        eid = id;
        ds = s;
        de = e?;
        break;
    }
    if eid == ID_BLOCK_GROUP {
        if let Some((s, e)) = find_child(buf, ds, de, ID_BLOCK_DURATION) {
            duration = Some(read_uint(buf, s, e));
        }
        let (s, e) = find_child(buf, ds, de, ID_BLOCK)?;
        ds = s;
        de = e;
        eid = ID_BLOCK;
    }
    if eid != ID_BLOCK && eid != ID_SIMPLE_BLOCK {
        return None;
    }
    let (track, mut p) = vint(buf, ds, false)?;
    if p + 3 > de {
        return None;
    }
    let rel_ts = i16::from_be_bytes([buf[p], buf[p + 1]]) as i64;
    p += 2;
    let flags = buf[p];
    p += 1;
    if eid == ID_SIMPLE_BLOCK && flags & 0x06 != 0 {
        return None; // 带 lacing 的字幕极罕见，交给 ffmpeg 回退
    }
    Some((track as u32, rel_ts, duration, buf[p..de].to_vec()))
}

fn fmt_ts(ms: i64) -> String {
    let ms = ms.max(0);
    let (h, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, frac) = (rest / 1000, rest % 1000);
    format!("{h:02}:{m:02}:{s:02},{frac:03}")
}

/// Matroska 里的 ASS 每条不是 `Dialogue:` 行，而是
/// `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`（8 段前置 + 文本，
/// 时间戳来自 block 本身）。某些 muxer 会保留 `Dialogue:` 前缀，那种是 9 段 + 文本。
/// 这里按前缀判断跳几段，文本里自带的逗号不受影响。
fn ass_line_to_text(line: &str) -> String {
    let (rest, skip) = match line.strip_prefix("Dialogue:") {
        Some(r) => (r, 9usize),
        None => (line, 8usize),
    };
    let mut it = rest.splitn(skip + 1, ',');
    for _ in 0..skip {
        if it.next().is_none() {
            return String::new();
        }
    }
    let mut text = it.next().unwrap_or("").to_string();
    // 去掉 {\...} 覆盖标签（{\i1} / {\pos(..)} 之类）
    loop {
        let Some(a) = text.find('{') else { break };
        let Some(b) = text[a..].find('}') else { break };
        text.replace_range(a..a + b + 1, "");
    }
    text.replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
        .trim()
        .to_string()
}

struct SubEntry {
    start_ms: i64,
    duration: Option<u64>,
    text: String,
}

/// 去掉字幕文本里的 HTML 风格标签（`<font face=...>`、`<i>`、`</font>` 等）。
///
/// 播放器把 cue 文本用 textContent 写进 DOM，不解析标签，所以这些会原样显示在画面上。
/// SRT 文本里 `<` 极罕见，时间轴用的 `-->` 不含 `<`，整段扫一遍是安全的。
pub fn strip_markup(srt: &str) -> String {
    let mut out = String::with_capacity(srt.len());
    let mut inside = false;
    for ch in srt.chars() {
        match ch {
            '<' => inside = true,
            '>' if inside => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

/// 稀疏抽出某条文本字幕轨，返回 SRT 内容。
///
/// 全程 fail-closed：任何一条 cue 解析不出（缺 CueRelativePosition、block 带 lacing、
/// cluster 头异常、时间戳溢出、ASS 行格式不对……）就整轨返回 Err，让调用方回退 ffmpeg。
/// 宁可慢一次，也不能产出一份缺行却看起来正常的 SRT —— 它会被写进缓存，之后永远命中，
/// 用户只会觉得"这段怎么没字幕"，而且没有任何自动修复途径。
pub fn sparse_extract_srt(path: &Path, track_number: u32, is_ass: bool) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let (cues_pos, seg_data, timestamp_scale) = locate_cues(&mut f).ok_or("没有 Cues 索引")?;
    if timestamp_scale != 1_000_000 {
        return Err(format!("TimestampScale={timestamp_scale}，非 1ms 时间基，回退整片抽取"));
    }
    let cues = read_cues(&mut f, cues_pos, track_number)?;
    if cues.is_empty() {
        return Err("Cues 里没有这条字幕轨".into());
    }

    let mut buf = vec![0u8; 8192];
    let mut head = vec![0u8; 96];
    let mut entries: Vec<SubEntry> = Vec::new();
    let mut dropped = 0usize;

    for cue in &cues {
        let cluster_abs = seg_data + cue.cluster;
        // 没有 CueRelativePosition 就没法定位到 block，只能整轨回退
        let Some(relative) = cue.relative else {
            return Err("Cues 缺少 CueRelativePosition，无法精确定位字幕块".into());
        };
        f.seek(SeekFrom::Start(cluster_abs)).map_err(|e| e.to_string())?;
        let hn = f.read(&mut head).map_err(|e| e.to_string())?;
        let Some((cid, cp2)) = vint(&head[..hn], 0, true) else {
            dropped += 1;
            continue;
        };
        if cid != ID_CLUSTER {
            dropped += 1;
            continue;
        }
        let Some((_csize, cdata)) = vint(&head[..hn], cp2, false) else {
            dropped += 1;
            continue;
        };
        let Some((ts_s, ts_e)) = find_child(&head[..hn], cdata, hn, ID_CLUSTER_TS) else {
            dropped += 1;
            continue;
        };
        let cluster_ts = read_uint(&head[..hn], ts_s, ts_e);

        let block_off = cdata + relative as usize;
        let mut got = None;
        if block_off < hn.saturating_sub(16) {
            got = parse_block(&head[..hn], block_off, track_number);
        }
        if got.is_none() {
            f.seek(SeekFrom::Start(cluster_abs + block_off as u64)).map_err(|e| e.to_string())?;
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            got = parse_block(&buf[..n], 0, track_number);
        }
        let Some((track, rel_ts, duration, payload)) = got else {
            dropped += 1;
            continue;
        };
        if track != track_number {
            dropped += 1;
            continue;
        }
        let Ok(base_ms) = i64::try_from(cluster_ts) else {
            dropped += 1;
            continue;
        };
        let raw = String::from_utf8_lossy(&payload).replace("\r\n", "\n");
        let mut lines: Vec<String> = Vec::new();
        let mut bad_line = false;
        for l in raw.lines() {
            let l = l.trim_end();
            if l.trim().is_empty() {
                continue;
            }
            if is_ass {
                let t = ass_line_to_text(l);
                if t.is_empty() {
                    bad_line = true;
                    break;
                }
                lines.push(t);
            } else {
                lines.push(l.to_string());
            }
        }
        if bad_line {
            dropped += 1;
            continue;
        }
        let text = lines.join("\n");
        if text.trim().is_empty() {
            continue; // 空字幕块是合法的，不算失败
        }
        entries.push(SubEntry {
            start_ms: base_ms + rel_ts,
            duration,
            text,
        });
    }

    if dropped > 0 {
        return Err(format!("稀疏解析失败 {dropped}/{} 条，回退整片抽取", cues.len()));
    }

    if entries.is_empty() {
        return Err("这条字幕轨没有可导出的文本".into());
    }
    entries.sort_by_key(|e| e.start_ms);

    let mut out = String::with_capacity(entries.len() * 64);
    for (i, e) in entries.iter().enumerate() {
        let end = match e.duration {
            Some(d) if d > 0 => e.start_ms + d as i64,
            _ => entries
                .get(i + 1)
                .map(|n| n.start_ms)
                .filter(|n| *n > e.start_ms)
                .unwrap_or(e.start_ms + 2000),
        };
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, fmt_ts(e.start_ms), fmt_ts(end), e.text));
    }
    Ok(strip_markup(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机调试入口：
    ///   $env:SPARSE_TEST_SRC="<mkv>"; $env:SPARSE_TEST_TRACK="3"; $env:SPARSE_TEST_ASS="1"
    ///   cargo test --release sparse_probe -- --nocapture
    #[test]
    fn sparse_probe() {
        let Ok(src) = std::env::var("SPARSE_TEST_SRC") else { return };
        let track: u32 = std::env::var("SPARSE_TEST_TRACK").unwrap_or_else(|_| "1".into()).parse().unwrap();
        let is_ass = std::env::var("SPARSE_TEST_ASS").map(|v| v == "1").unwrap_or(false);
        match sparse_extract_srt(Path::new(&src), track, is_ass) {
            Ok(srt) => println!("OK {} bytes\n{}", srt.len(), srt.chars().take(400).collect::<String>()),
            Err(e) => println!("ERR: {e}"),
        }
    }
}
