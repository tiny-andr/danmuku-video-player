// 外挂字幕 → SRT 文本（纯 Rust，不依赖 ffmpeg）。
//
// 为什么需要这个模块：外挂字幕曾经一律走 `ffmpeg -i <file> -f srt pipe:1`，
// 但安装包并没有把 ffmpeg 打进去 —— `target/release` 里没有 ffmpeg.exe，
// `tauri.conf.json` 也没配 resources / externalBin。于是只有"本机 PATH 里恰好
// 装了 ffmpeg"的机器能挂外挂字幕，干净安装的用户连一个 .srt 都挂不上。
//
// srt / vtt / ass / ssa 全是纯文本容器，所谓"转换"只是重排时间轴、丢掉样式，
// 不需要任何解码器。这里自己实现，ffmpeg 只留给这套解析认不出来的格式兜底。

use std::path::Path;

use crate::mkv_sub::{ass_clean_text, fmt_ts, strip_markup};

/// 读字幕文件并转成规范化的 SRT 文本（已剥掉样式标签）。
pub fn to_srt(path: &Path) -> Result<String, String> {
    // 走带上限的读：外挂字幕也整读进内存，一个几十 GB 的假 .ass 能把进程读崩。
    // 上限与弹幕/文本共用 64 MB —— 30 万条的超大 ASS 实测 19 MB，够用。
    let bytes = crate::read_capped(&path.to_string_lossy(), crate::MAX_READ_BYTES)
        .map_err(|e| format!("读取字幕失败：{e}"))?;
    if bytes.is_empty() {
        return Err("字幕文件是空的".into());
    }
    let text = decode_text(&bytes);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let srt = match ext.as_str() {
        "srt" => normalize_srt(&text),
        "vtt" => vtt_to_srt(&text),
        "ass" | "ssa" => ass_to_srt(&text)?,
        _ => return Err(format!("不认识的字幕扩展名：{ext}")),
    };
    let srt = strip_markup(&srt);
    if srt.trim().is_empty() {
        return Err("这条字幕没有可显示的内容".into());
    }
    Ok(srt)
}

/// 字幕文件编码：严格 UTF-8 → GBK → 有损 UTF-8。
///
/// 中文外挂字幕有大量是 GBK 的（早年字幕组默认 ANSI）。这份文本要进 Rust 做
/// 格式转换，没法交给浏览器的 TextDecoder，所以在这里一次解干净。用系统 API
/// 而不是引第三方编码库，因为 windows-sys 已经是依赖了。
pub fn decode_text(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.trim_start_matches('\u{feff}').to_string();
    }
    #[cfg(windows)]
    if let Some(s) = cp936_decode(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn cp936_decode(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    const GBK_CODEPAGE: u32 = 936;
    if bytes.is_empty() {
        return Some(String::new());
    }
    if bytes.len() > i32::MAX as usize {
        return None;
    }
    let len = bytes.len() as i32;
    let need = unsafe { MultiByteToWideChar(GBK_CODEPAGE, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return None;
    }
    let mut buf = vec![0u16; need as usize];
    let got = unsafe { MultiByteToWideChar(GBK_CODEPAGE, 0, bytes.as_ptr(), len, buf.as_mut_ptr(), need) };
    if got <= 0 {
        return None;
    }
    buf.truncate(got as usize);
    String::from_utf16(&buf).ok()
}

/// 时间轴 → 毫秒。兼容 SRT（`,` 分隔毫秒）、WebVTT（`.` 分隔毫秒）
/// 与 ASS（`h:mm:ss.cc` 厘秒）。小数位数 1~3 位都按位数缩放。
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut halves = s.splitn(2, [',', '.']);
    let head = halves.next()?;
    let frac = halves.next().unwrap_or("").trim();
    let mut parts = head.split(':');
    let h: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next()?.trim().parse().ok()?;
    let sec: i64 = parts.next().unwrap_or("0").trim().parse().ok()?;
    if parts.next().is_some() || !(0..60).contains(&m) || !(0..60).contains(&sec) {
        return None;
    }
    let frac_ms = if frac.is_empty() {
        0
    } else {
        if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i64 = frac.parse().ok()?;
        n * 10i64.pow(3 - frac.len() as u32)
    };
    Some((h * 3600 + m * 60 + sec) * 1000 + frac_ms)
}

/// 一次 SRT 块的规范化结果：(开始, 结束, 正文)
struct Cue {
    start: i64,
    end: i64,
    body: String,
}

/// 按行扫描的 SRT 解析：以含 `-->` 的行为锚点，向下收到空行为正文。
/// 不用"按空行分块"是因为破损字幕常缺块间空行，那样会把两块并成一块。
fn scan_cues(text: &str, strip_seq: bool) -> Vec<Cue> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let Some(arrow) = lines[i].find("-->") else {
            i += 1;
            continue;
        };
        let time_line = lines[i].trim();
        i += 1;
        let body_start = i;
        while i < lines.len() && !lines[i].trim().is_empty() {
            i += 1;
        }
        let mut body = lines[body_start..i].join("\n");
        while i < lines.len() && lines[i].trim().is_empty() {
            i += 1;
        }
        if body.trim().is_empty() {
            continue;
        }
        // 正文里残留的上一块序号（缺空行时会出现）：首行若是纯数字就丢掉
        if strip_seq {
            if let Some((first, rest)) = body.split_once('\n') {
                if !first.trim().is_empty() && first.trim().bytes().all(|b| b.is_ascii_digit()) {
                    body = rest.to_string();
                }
            }
        }
        let tail = &time_line[arrow + 3..];
        let end_tok = tail.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_ts(&time_line[..arrow]), parse_ts(end_tok)) else {
            continue;
        };
        if end <= start {
            continue;
        }
        out.push(Cue { start, end, body: body.trim_end().to_string() });
    }
    out
}

fn render(cues: &[Cue]) -> String {
    let mut out = String::with_capacity(cues.len() * 64);
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, fmt_ts(c.start), fmt_ts(c.end), c.body));
    }
    out
}

/// SRT 规范化：CRLF→LF、去 BOM、统一时间轴格式、重新编号、块间空一行。
fn normalize_srt(text: &str) -> String {
    let t = text.replace("\r\n", "\n").replace('\r', "\n");
    let t = t.trim_start_matches('\u{feff}');
    render(&scan_cues(t, true))
}

/// WebVTT → SRT：去掉 `WEBVTT` 头与 NOTE/STYLE/REGION 块，
/// 剩下的 cue 结构与 SRT 同形（时间轴用 `.`，`parse_ts` 认）。
fn vtt_to_srt(text: &str) -> String {
    let t = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut body = String::with_capacity(t.len());
    let mut skipping = false;
    for line in t.split('\n') {
        let lt = line.trim();
        if lt.eq_ignore_ascii_case("webvtt") || lt.to_ascii_lowercase().starts_with("webvtt ") {
            continue;
        }
        // NOTE / STYLE / REGION 块以关键字开头，直到下一个空行
        if lt.starts_with("NOTE") || lt.starts_with("STYLE") || lt.starts_with("REGION") {
            skipping = true;
            continue;
        }
        if lt.is_empty() {
            skipping = false;
        }
        if skipping {
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    render(&scan_cues(&body, false))
}

/// ASS/SSA → SRT。只取 `[Events]` 段的 `Dialogue:` 行；样式、特效、定位全部
/// 丢弃 —— 播放器只渲染纯文本，保留它们反而会以 `{\i1}` 之类原样显示。
fn ass_to_srt(text: &str) -> Result<String, String> {
    let t = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut fields: Option<Vec<String>> = None;
    let mut in_events = false;
    let mut cues: Vec<Cue> = Vec::new();
    for line in t.split('\n') {
        let lt = line.trim();
        if lt.starts_with('[') {
            in_events = lt.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = strip_prefix_ci(lt, "Format:") {
            fields = Some(rest.split(',').map(|f| f.trim().to_ascii_lowercase()).collect());
            continue;
        }
        let Some(rest) = strip_prefix_ci(lt, "Dialogue:") else {
            continue;
        };
        // Format 声明缺失时按 ASS v4+ 的标准顺序兜底
        let names: Vec<String> = fields.clone().unwrap_or_else(|| {
            ["layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
        let parts: Vec<&str> = rest.splitn(names.len(), ',').collect();
        if parts.len() < names.len() {
            continue;
        }
        let pick = |key: &str| names.iter().position(|f| f == key).and_then(|i| parts.get(i).copied());
        let (Some(start), Some(end)) = (pick("start").and_then(parse_ts), pick("end").and_then(parse_ts)) else {
            continue;
        };
        let body = ass_clean_text(pick("text").unwrap_or(""));
        if body.is_empty() || end <= start {
            continue;
        }
        cues.push(Cue { start, end, body });
    }
    if cues.is_empty() {
        return Err("这个 ASS/SSA 里没有可用的 Dialogue 行".into());
    }
    cues.sort_by_key(|c| c.start);
    Ok(render(&cues))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// 每个用例一个独立临时目录，跑完删掉；不进仓库、不碰媒体目录。
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("bplayer-subtext-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            fs::write(&p, bytes).unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 回归：外挂 .srt 必须在没有 ffmpeg 的情况下也能转。
    /// 修复前这条路只有 `ffmpeg -f srt pipe:1`，而安装包不带 ffmpeg，
    /// 干净安装的机器上连 .srt 都挂不上。
    #[test]
    fn srt_needs_no_ffmpeg() {
        let s = Scratch::new("srt");
        let p = s.file(
            "movie.srt",
            "1\r\n00:00:01,000 --> 00:00:02,500\r\n<font face=\"Arial\">hello</font>\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nworld\r\n"
                .as_bytes(),
        );
        let out = to_srt(&p).expect("纯 Rust 路径应当能处理 .srt");
        assert!(out.contains("1\n00:00:01,000 --> 00:00:02,500\nhello"), "首条不规范：{out:?}");
        assert!(out.contains("2\n00:00:03,000 --> 00:00:04,000\nworld"), "次条丢失：{out:?}");
        assert!(!out.contains("<font"), "样式标签没剥干净：{out:?}");
        assert!(out.contains("\n\n"), "块之间应有空行：{out:?}");
    }

    /// 中文外挂字幕大量是 GBK 的，不能依赖系统 PATH 里有个能转编码的 ffmpeg。
    #[test]
    fn gbk_srt_needs_no_ffmpeg() {
        let s = Scratch::new("gbk");
        // "你好" 的 GBK 字节 = C4 E3 BA C3（故意不是合法 UTF-8）
        let mut raw = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
        raw.extend_from_slice(&[0xC4, 0xE3, 0xBA, 0xC3]);
        raw.extend_from_slice(b"\n");
        let p = s.file("movie.chs.srt", &raw);
        let out = to_srt(&p).expect("GBK 字幕应当能解");
        assert!(out.contains("你好"), "GBK 没解对：{out:?}");
    }

    /// 外挂 .ass 之前完全依赖 ffmpeg（Artplayer 自带的 assToVtt 产出非法 VTT）。
    #[test]
    fn ass_needs_no_ffmpeg() {
        let s = Scratch::new("ass");
        let p = s.file(
            "movie.ass",
            "\u{feff}[Script Info]\nTitle: t\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,{\\i1}\u{4f60}\u{597d}{\\i0}\\N\u{4e16}\u{754c}\nDialogue: 0,0:00:04.00,0:00:05.00,Default,,0,0,0,,third, with comma\n"
                .as_bytes(),
        );
        let out = to_srt(&p).expect("纯 Rust 路径应当能处理 .ass");
        assert!(out.contains("00:00:01,500 --> 00:00:03,000"), "ASS 厘秒换算不对：{out:?}");
        assert!(out.contains("你好\n世界"), "\\N 换行或覆盖标签处理不对：{out:?}");
        assert!(!out.contains('{'), "覆盖标签没剥掉：{out:?}");
        // 文本里自带的逗号不能被当成字段分隔符切掉
        assert!(out.contains("third, with comma"), "正文逗号被切：{out:?}");
    }

    #[test]
    fn vtt_needs_no_ffmpeg() {
        let s = Scratch::new("vtt");
        let p = s.file(
            "movie.vtt",
            "WEBVTT\n\nNOTE \u{6ce8}\u{91ca}\n\n1\n00:00:01.000 --> 00:00:02.000\n<v \u{67d0}\u{4eba}>hello</v>\n\n00:00:03.000 --> 00:00:04.000\ntwo\n"
                .as_bytes(),
        );
        let out = to_srt(&p).expect("纯 Rust 路径应当能处理 .vtt");
        assert!(out.contains("00:00:01,000 --> 00:00:02,000"), "VTT 时间轴没转好：{out:?}");
        assert!(out.contains("hello"), "正文丢了：{out:?}");
        assert!(!out.contains("WEBVTT") && !out.contains("NOTE"), "头部/注释没去掉：{out:?}");
        assert!(!out.contains("<v"), "VTT 标签没剥掉：{out:?}");
    }

    /// 认不出来的内容必须报错（交给 ffmpeg 兜底），不能静默返回空字幕。
    #[test]
    fn garbage_srt_is_rejected() {
        let s = Scratch::new("bad");
        let p = s.file("movie.srt", b"this is not a subtitle at all\n");
        assert!(to_srt(&p).is_err());
    }

    #[test]
    fn timestamp_parsing() {
        assert_eq!(parse_ts("00:00:01,000"), Some(1000));
        assert_eq!(parse_ts("00:00:01.500"), Some(1500));
        assert_eq!(parse_ts("0:00:01.05"), Some(1050));
        assert_eq!(parse_ts("01:02:03,004"), Some(3_723_004));
        assert_eq!(parse_ts("nonsense"), None);
    }
}
