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
        // 头本身就越过父范围时直接收尾。以前照样 push，会产出 start > end 的元素，
        // 下游 read_uint 切片立刻 panic；而这个 panic 发生在 spawn_blocking 里，
        // 前端只会看到一个永不 settle 的进度胶囊。
        if p3 > end {
            break;
        }
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
    // 不用 `&buf[start..end]`：损坏的字节流能让 start 跑到 end 之后，切片直接 panic。
    // 区间非法时按 0 处理（跟"元素缺失"同义），交给上层已有的 fail-closed 校验去拒绝。
    let Some(bytes) = buf.get(start..end.min(buf.len())) else { return 0 };
    for b in bytes {
        v = (v << 8) | *b as u64;
    }
    v
}

/// 从已读进内存的文件头里取 TimestampScale。
/// 返回 None 表示这段窗口里没有 Info，或 Info 里没有该元素——调用方必须 fail-closed。
fn read_timestamp_scale(head: &[u8], seg_data: usize) -> Option<u64> {
    let (info_start, info_end) = find_child(head, seg_data, head.len(), ID_INFO)?;
    let (s, e) = find_child(head, info_start, info_end, ID_TIMESTAMP_SCALE)?;
    Some(read_uint(head, s, e))
}

/// 通过 SeekHead 找到 Cues 的绝对位置，同时返回 Segment 数据区起点和 TimestampScale。
/// TimestampScale 必须读到：所有时间戳换算成毫秒都靠它。只读前 2 MB 就够不到 Info 的片子
/// （4K 片首簇常超 2 MB）以前静默按 1e6 猜，实际值非 1e6 时整条时间轴按比例错位，
/// 产出却是一份看起来正常的 SRT —— 所以读不到就返回 Err 让调用方回退 ffmpeg。
fn locate_cues(f: &mut File) -> Result<(u64, u64, u64), String> {
    let mut head = vec![0u8; 2 * 1024 * 1024];
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let n = f.read(&mut head).map_err(|e| e.to_string())?;
    head.truncate(n);

    let seg = find_child(&head, 0, head.len(), ID_SEGMENT).ok_or("不是有效的 MKV：找不到 Segment")?;
    let seg_data = seg.0 as u64;
    let timestamp_scale = read_timestamp_scale(&head, seg_data as usize)
        .ok_or("头部 2 MB 内没读到 TimestampScale，时间轴无法确定，回退整片抽取")?;

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
                    // 用 get 而不是直接切片：长度是文件里读来的，损坏时可能越过缓冲区
                    ID_SEEK_ID => seek_id = head.get(cds..e),
                    ID_SEEK_POS => seek_pos = Some(read_uint(&head, cds, e)),
                    _ => {}
                }
            }
            if seek_id == Some(CUE_BYTES) {
                if let Some(p) = seek_pos {
                    let abs = seg_data.checked_add(p).ok_or("Cues 位置溢出，回退整片抽取")?;
                    return Ok((abs, seg_data, timestamp_scale));
                }
            }
        }
    }
    Err("没有 Cues 索引".into())
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
    // Block 与 SimpleBlock 的 flags 布局相同（bit1-2 = lacing）。以前只查了 SimpleBlock，
    // BlockGroup 里的 Block 带 lacing 时正文前面是 lace 头，会被当成字幕字节 →
    // 乱码却不报错，用户只觉得"这段字怎么不对"。两种都拒绝，交给 ffmpeg 回退。
    if flags & 0x06 != 0 {
        return None;
    }
    buf.get(p..de).map(|b| (track as u32, rel_ts, duration, b.to_vec()))
}

pub(crate) fn fmt_ts(ms: i64) -> String {
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
    ass_clean_text(it.next().unwrap_or(""))
}

/// ASS 的「文本字段」→ 可直接显示的纯文本：去掉 `{\...}` 覆盖标签，
/// 处理 `\N`（硬换行）/ `\n`（软换行）/ `\h`（不换行空格）三个转义。
/// 内嵌块（`ass_line_to_text`）与外挂 .ass 文件（`subtitle_text`）共用。
pub(crate) fn ass_clean_text(text: &str) -> String {
    let mut text = text.to_string();
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
/// 播放器把 cue 文本经 escape 后写进 innerHTML，标签会原样显示在画面上。
///
/// 只能剥"看起来确实是标签"的片段：`<` 必须在**同一行内**找到 `>`，且内容以 `/` 或
/// ASCII 字母开头、内部不含 `<`。否则就把 `<` 原样输出。
/// 早先的写法是跨行保持一个 `inside` 状态，结果字幕正文里出现 `<3`、`a < b`、`<--`
/// 这类字符时，会一直吞到下一个 `>`（通常是下一行时间轴的 `-->`），把后半条字幕的
/// 序号和时间轴一起吃掉，产出一份结构已坏的 SRT。
pub fn strip_markup(srt: &str) -> String {
    let mut out = String::with_capacity(srt.len());
    for line in srt.split_inclusive('\n') {
        let mut rest = line;
        loop {
            let Some(lt) = rest.find('<') else { break };
            let Some(gt_rel) = rest[lt..].find('>') else { break };
            let inner = &rest[lt + 1..lt + gt_rel];
            let looks_like_tag = !inner.contains('<')
                && (inner.starts_with('/')
                    || inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic()));
            out.push_str(&rest[..lt]);
            if looks_like_tag {
                rest = &rest[lt + gt_rel + 1..];
            } else {
                out.push('<');
                rest = &rest[lt + 1..];
            }
        }
        out.push_str(rest);
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
    let (cues_pos, seg_data, timestamp_scale) = locate_cues(&mut f)?;
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
        // 下面这些加法的两个操作数都是文件里读出来的 u64，溢出在 debug 下会 panic、
        // release 下会回绕到一个随便的位置——两种都是错数据，统一按"这条 cue 解析失败"处理。
        let Some(cluster_abs) = seg_data.checked_add(cue.cluster) else {
            dropped += 1;
            continue;
        };
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

        let Some(block_off) = usize::try_from(relative).ok().and_then(|r| cdata.checked_add(r)) else {
            dropped += 1;
            continue;
        };
        let mut got = None;
        if block_off < hn.saturating_sub(16) {
            got = parse_block(&head[..hn], block_off, track_number);
        }
        if got.is_none() {
            let Some(abs) = cluster_abs.checked_add(block_off as u64) else {
                dropped += 1;
                continue;
            };
            f.seek(SeekFrom::Start(abs)).map_err(|e| e.to_string())?;
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
        let Some(start_ms) = base_ms.checked_add(rel_ts) else {
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
            start_ms,
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
        // BlockDuration 是文件里读来的 u64，超出 i64 的畸形值以前会 as 成负数，
        // 产出一条 end < start 的坏 SRT；这种值按"没有时长"处理，退回下一条 cue 推算。
        let dur = e.duration.and_then(|d| i64::try_from(d).ok()).filter(|d| *d > 0);
        let end = match dur {
            Some(d) => e.start_ms.saturating_add(d),
            None => entries
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
    /// strip_markup 不能因为一个不成对的 < 而吞掉后面的内容。
    /// 字幕正文里 <3、 < b、<-- 都很常见。
    #[test]
    fn strip_markup_survives_unpaired_lt() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n<3\n\n2\n00:00:03,000 --> 00:00:04,000\nhello\n";
        let out = strip_markup(srt);
        assert!(
            out.contains("hello"),
            "不成对的 '<' 把后续字幕全吞了：{out:?}"
        );
        assert!(out.contains("<3"), "普通文本里的 '<3' 不应被当标签删掉：{out:?}");
    }

    /// 真正的样式标签仍然要被剥掉。
    #[test]
    fn strip_markup_removes_real_tags() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n<font face=\"Arial\"><i>hi</i></font>\n";
        let out = strip_markup(srt);
        assert!(out.contains("hi"));
        assert!(!out.contains("<font"), "标签没剥干净：{out:?}");
        assert!(!out.contains("<i>"), "标签没剥干净：{out:?}");
    }

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

    fn temp_mkv_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // 带进程号：同一进程里并行跑的测试不会撞名，也避开本机其它进程的临时文件
        p.push(format!("bplayer_mkv_sub_{}_{tag}.mkv", std::process::id()));
        p
    }

    // ---- 合成 MKV 样本（不依赖外部素材，CI / 日常开发机都没有真片源） ----
    //
    // 下面这套构造器与安卓版同源（平台无关，只用标准库），让 Cues → seek →
    // BlockGroup 解析这整条链路在没有真片源时也能真跑一遍。

    /// 最少字节的大端编码（元素 ID 用）。
    fn id_bytes(id: u64) -> Vec<u8> {
        let be = id.to_be_bytes();
        let start = be.iter().position(|&b| b != 0).unwrap_or(7);
        be[start..].to_vec()
    }

    /// 固定宽度的 EBML 尺寸 vint（含长度标记位）。
    fn size_vint(width: usize, n: u64) -> Vec<u8> {
        let mut v = n | (1u64 << (7 * width));
        let mut out = vec![0u8; width];
        for i in (0..width).rev() {
            out[i] = (v & 0xFF) as u8;
            v >>= 8;
        }
        out
    }

    /// 最短形式的尺寸 vint。
    fn vint_size(n: u64) -> Vec<u8> {
        let mut width = 1usize;
        while n >= (1u64 << (7 * width)) {
            width += 1;
        }
        size_vint(width, n)
    }

    fn el(id: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = id_bytes(id);
        out.extend(vint_size(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    /// 位置类字段固定 8 字节：两遍构建（占位/真实位置）之间元素长度必须一致，
    /// 否则 SeekHead 里写死的 Cues 偏移会跟着长度变化而错位。
    fn uint8(value: u64) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    /// EBML 的 uint 类元素（CueTrack / ClusterTimestamp / BlockDuration……）是不带长度
    /// 标记的裸大端整数，跟 vint 完全不同 —— 用 vint 编码会让 read_uint 读出 0x81 这种错值。
    fn uint_min(value: u64) -> Vec<u8> {
        let be = value.to_be_bytes();
        let start = be.iter().position(|&b| b != 0).unwrap_or(7);
        be[start..].to_vec()
    }

    const TRACK_SRT: u32 = 1;
    const TRACK_ASS: u32 = 2;

    /// Block/SimpleBlock 负载：track vint + 相对时间戳(i16) + flags(无 lacing) + 文本
    fn block_payload(track: u32, rel_ms: i16, text: &[u8]) -> Vec<u8> {
        let mut p = vint_size(track as u64);
        p.extend_from_slice(&rel_ms.to_be_bytes());
        p.push(0);
        p.extend_from_slice(text);
        p
    }

    fn block_group(track: u32, rel_ms: i16, text: &[u8], duration_ms: Option<u64>) -> Vec<u8> {
        let mut inner = Vec::new();
        if let Some(d) = duration_ms {
            inner.extend(el(ID_BLOCK_DURATION, &uint_min(d)));
        }
        inner.extend(el(ID_BLOCK, &block_payload(track, rel_ms, text)));
        el(ID_BLOCK_GROUP, &inner)
    }

    /// 返回 (Cluster 元素字节, 每个 block 相对 Cluster 数据区的偏移)
    fn cluster_element(groups: &[(u32, i16, &[u8], Option<u64>)]) -> (Vec<u8>, Vec<u64>) {
        let mut data = el(ID_CLUSTER_TS, &uint_min(1000));
        let mut offsets = Vec::new();
        for (track, rel, text, dur) in groups {
            offsets.push(data.len() as u64);
            data.extend(block_group(*track, *rel, text, *dur));
        }
        let cluster = el(ID_CLUSTER, &data);
        // CueRelativePosition 是相对 Cluster **数据区**（元素头之后）的偏移
        (cluster, offsets)
    }

    fn cue_point(track: u32, time: u64, cluster_pos: u64, relative: Option<u64>) -> Vec<u8> {
        let mut ctps = Vec::new();
        ctps.extend(el(ID_CUE_TRACK, &uint_min(track as u64)));
        ctps.extend(el(ID_CUE_CLUSTER, &uint8(cluster_pos)));
        if let Some(r) = relative {
            ctps.extend(el(ID_CUE_REL, &uint8(r)));
        }
        let mut point = el(ID_CUE_TIME, &uint_min(time));
        point.extend(el(ID_CUE_TRACK_POS, &ctps));
        el(ID_CUE_POINT, &point)
    }

    fn seek_head(cues_pos: u64) -> Vec<u8> {
        let mut seek = el(ID_SEEK_ID, CUE_BYTES);
        seek.extend(el(ID_SEEK_POS, &uint8(cues_pos)));
        el(ID_SEEK_HEAD, &el(ID_SEEK, &seek))
    }

    fn cues_element(cluster_pos: u64, rels: &[u64], with_relative: bool) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(cue_point(TRACK_SRT, 1000, cluster_pos, with_relative.then_some(rels[0])));
        body.extend(cue_point(TRACK_ASS, 2000, cluster_pos, with_relative.then_some(rels[1])));
        el(ID_CUES, &body)
    }

    /// 最小可解析 MKV：EBML 头 + Segment{ Info(TimestampScale=1e6), SeekHead(Cues),
    /// Cues(两条轨各一个 CuePoint), Cluster(字幕块) }。
    /// Cluster 放在文件末尾，Cues 的 CueClusterPosition 指向它；
    /// `with_relative=false` 时故意不写 CueRelativePosition（fail-closed 用例）。
    fn build_mkv(with_relative: bool) -> Vec<u8> {
        let info = el(ID_INFO, &el(ID_TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40]));
        // 同一个 Cluster 里两条轨各一条字幕：SRT 轨带 BlockDuration，ASS 轨不带（走下一条推算）
        let (cluster, offsets) = cluster_element(&[
            (TRACK_SRT, 0, b"hello", Some(4000)),
            (TRACK_ASS, 1000, b"0,0,Default,,0,0,0,,{\\an8}hi, there", None),
        ]);

        // 两遍构建：Cues 里要写 Cluster 相对位置，SeekHead 里要写 Cues 相对位置。
        // 位置字段固定 8 字节，所以第二遍的元素长度与第一遍完全相同。
        // 这条断言钉的就是「位置字段固定 8 字节」这个前提：一旦有人把 uint8 改成
        // 最短编码，两遍长度会不同，SeekHead 里写死的 Cues 偏移就会错位。
        let seek_head_len = seek_head(0).len();
        let cues_pos = info.len() + seek_head_len;
        let cues_len = cues_element(0, &offsets, with_relative).len();
        let cluster_pos = (cues_pos + cues_len) as u64;
        let cues = cues_element(cluster_pos, &offsets, with_relative);
        let seek = seek_head(cues_pos as u64);
        assert_eq!(cues.len(), cues_len, "Cues 两遍构建长度必须一致");
        assert_eq!(seek.len(), seek_head_len, "SeekHead 两遍构建长度必须一致");

        let mut seg_data = Vec::new();
        seg_data.extend(info);
        seg_data.extend(seek);
        seg_data.extend(cues);
        seg_data.extend(cluster);
        let mut out = el(0x1A45_DFA3, &[0x42, 0x86, 0x81, 0x01]); // EBML
        out.extend(el(ID_SEGMENT, &seg_data));
        out
    }

    /// 端到端：真解析 Info/SeekHead/Cues → 按 CueClusterPosition + CueRelativePosition
    /// seek 到 Cluster → 解 BlockGroup → 拼 SRT（SRT 轨 + ASS 轨各一条）。
    /// 桌面版最核心的性能路径（稀疏抽取）以前只有靠环境变量喂真片源才跑得到，
    /// CI 和日常 `cargo test` 永远覆盖不到；这条测试把它变成默认回归。
    #[test]
    fn synthetic_mkv_sparse_extract_srt_and_ass() {
        let path = temp_mkv_path("synthetic");
        std::fs::write(&path, build_mkv(true)).unwrap();

        let srt = sparse_extract_srt(&path, TRACK_SRT, false)
            .unwrap_or_else(|e| panic!("SRT 轨稀疏抽取失败：{e}"));
        // 起点 = ClusterTimestamp(1000) + Block 相对时间(0)；
        // 结束 = BlockDuration(4000) → 5000。这是桌面实现的精确输出。
        assert_eq!(srt, "1\n00:00:01,000 --> 00:00:05,000\nhello\n\n");

        let ass = sparse_extract_srt(&path, TRACK_ASS, true)
            .unwrap_or_else(|e| panic!("ASS 轨稀疏抽取失败：{e}"));
        // 无 BlockDuration → 结束时间按桌面 sparse_extract_srt 里的兜底规则
        // （没有下一条 cue 时用「起点 + 2000」）→ 4000。
        // ASS 的 `{\an8}` 覆盖标签已剥掉，正文里的逗号保留。
        assert_eq!(ass, "1\n00:00:02,000 --> 00:00:04,000\nhi, there\n\n");

        // Cues 里没有的轨必须 Err（不能返回空 SRT 让前端以为成功）
        let other = sparse_extract_srt(&path, 9, false);
        assert!(other.is_err(), "不存在的轨不该抽取成功：{other:?}");

        let _ = std::fs::remove_file(&path);
    }

    /// 缺 CueRelativePosition 时整轨 Err，不能退化成顺序扫（退化的代价是整片读取，
    /// 而且读失败还会产出半份字幕）。
    #[test]
    fn synthetic_mkv_without_relative_position_is_fail_closed() {
        let path = temp_mkv_path("no_rel");
        std::fs::write(&path, build_mkv(false)).unwrap();
        let err = sparse_extract_srt(&path, TRACK_SRT, false).expect_err("缺 CueRelativePosition 必须整轨 Err");
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("CueRelativePosition"), "错误要指明原因，实际：{err}");
    }

    /// BlockGroup 里的 Block 与 SimpleBlock 的 flags 布局相同，带 lacing 时正文前面是
    /// lace 头。以前只拒绝了 SimpleBlock 的 lacing，BlockGroup 这条会把 lace 头当字幕字节。
    #[test]
    fn block_group_with_lacing_is_rejected() {
        // BlockGroup { Block [track=1, rel_ts=0, flags=0x02(Xiph lacing), "hi"] }
        let mut buf = vec![0xA0, 0x88, 0xA1, 0x86, 0x81, 0x00, 0x00, 0x02, b'h', b'i'];
        assert!(
            parse_block(&buf, 0, 1).is_none(),
            "BlockGroup 里带 lacing 的 Block 必须被拒绝"
        );
        // 同一条 Block 去掉 lacing 位后要能正常解析，证明判的是 flags 而不是整类元素
        buf[7] = 0x00;
        let (track, rel_ts, duration, payload) =
            parse_block(&buf, 0, 1).expect("无 lacing 的 Block 应能解析");
        assert_eq!(track, 1);
        assert_eq!(rel_ts, 0);
        assert!(duration.is_none());
        assert_eq!(payload, b"hi".to_vec());
    }

    /// 截断元素会让 payload 起点越过父范围。以前 children 照样把它 push 出去，
    /// read_uint 拿 start > end 去切片就 panic；panic 落在 spawn_blocking 里会变成
    /// JoinError / 永不 settle 的 Promise，前端进度胶囊卡死。
    #[test]
    fn children_never_emit_start_beyond_end() {
        // 只有 ID + 尺寸头、没有内容：p3(=2) 已经越过 end(=1)
        let out = children(&[0xA0, 0x81], 0, 1);
        assert!(out.is_empty(), "越过父范围的元素不应进入结果：{out:?}");
        for raw_end in 0..=4usize {
            for buf in [
                &b"\xA0"[..],
                &b"\xA0\x81"[..],
                &b"\xA0\x88\x81"[..],
                &b"\xA1\xFF\xFF"[..],
            ] {
                let end = raw_end.min(buf.len());
                for (_id, ds, de) in children(buf, 0, end) {
                    assert!(ds <= end, "元素起点 {ds} 越过父范围 {end}");
                    if let Some(de) = de {
                        assert!(de >= ds && de <= end, "非法区间 {ds}..{de}（父范围到 {end}）");
                    }
                    // 以前这一行会 panic
                    let _ = read_uint(buf, ds, de.unwrap_or(end));
                }
            }
        }
        // read_uint 自己收到非法区间也只能返回 0，不能切片 panic
        assert_eq!(read_uint(b"abc", 3, 1), 0);
        assert_eq!(read_uint(b"abc", 99, 100), 0);
    }

    /// TimestampScale 只从文件头 2 MB 里找。Info 落在首个 Cluster 之后时（4K 片首簇常
    /// 超 2 MB）读不到，以前静默按 1e6 猜 —— 实际值非 1e6 就整条时间轴错位。
    /// 现在必须走 Err，让调用方回退 ffmpeg。
    #[test]
    fn timestamp_scale_missing_is_fail_closed() {
        // 最小 MKV：EBML 头 + 未知长度的 Segment，Segment 里故意没有 Info
        let bytes = vec![
            0x1A, 0x45, 0xDF, 0xA3, 0x84, 0x42, 0x86, 0x81, 0x01, // EBML
            0x18, 0x53, 0x80, 0x67, 0xFF, // Segment（未知长度）
        ];
        assert_eq!(read_timestamp_scale(&bytes, 14), None, "没有 Info 时不能编出一个 scale");
        let path = temp_mkv_path("no_scale");
        std::fs::write(&path, &bytes).unwrap();
        let err = sparse_extract_srt(&path, 1, false).expect_err("缺 TimestampScale 必须整体 Err");
        let _ = std::fs::remove_file(&path);
        assert!(
            err.contains("TimestampScale"),
            "必须是 TimestampScale 缺失导致的 Err（而不是猜 1e6 后继续解析），实际：{err}"
        );
    }

    /// 有 Info 时要正常读到 1e6，别把上面那条 fail-closed 修成对正常片源也拒绝。
    #[test]
    fn timestamp_scale_is_read_from_info() {
        let bytes = vec![
            0x1A, 0x45, 0xDF, 0xA3, 0x84, 0x42, 0x86, 0x81, 0x01, // EBML
            0x18, 0x53, 0x80, 0x67, 0xFF, // Segment（未知长度）
            // Info { TimestampScale = 1000000 }
            0x15, 0x49, 0xA9, 0x66, 0x87, 0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40,
        ];
        assert_eq!(read_timestamp_scale(&bytes, 14), Some(1_000_000));
    }
}
