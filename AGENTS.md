# AGENTS.md

给在这个仓库里改代码的 AI / 人类协作者。只写容易踩错、又不好从代码里一眼看出来的东西。

## 1. 这是什么

Tauri 2 桌面播放器：打开本机视频，自动配同目录的弹幕 XML 与字幕一起播。

- 前端**没有构建步骤**：`src/index.html` 是唯一入口，CSS 与 JS 全部内联，`src/vendor/` 下的 Artplayer 与两个插件是本地化副本，不走 CDN。
- 后端是 Rust，8 个模块，多数命令是"调外部 ffmpeg / 读二进制 / 开系统对话框"的薄封装。

## 2. 代码地图

| 文件 | 职责 |
|---|---|
| `src/index.html` (~2100 行) | 整个前端。播放器装配、弹幕、字幕菜单、音轨菜单、设置页、拖放、进度胶囊 |
| `src/vendor/artplayer.js` | 播放器内核，本地副本，**不要改** |
| `src/vendor/artplayer-plugin-danmuku.js` | 弹幕插件，本地副本，只在必要时打补丁（`.orig.js` 保留原版） |
| `src-tauri/src/lib.rs` | 命令注册、选文件/选文件夹、目录扫描、`reveal_path`、`sidecar_cache_files` |
| `src-tauri/src/mkv_probe.rs` | 解析 MKV 轨道表。产出 `number`（Matroska TrackNumber，**不是** 0 基索引） |
| `src-tauri/src/mkv_sub.rs` | 用 Cues 索引稀疏抽取文本字幕（核心性能点） |
| `src-tauri/src/pgs_render.rs` | libpgs 解码 PGS 位图 → base64 PNG |
| `src-tauri/src/subtitle_text.rs` | 外挂字幕（srt/vtt/ass/ssa）纯 Rust → SRT，**不依赖 ffmpeg** |
| `src-tauri/src/transcode.rs` | 音轨转码、字幕抽取兜底、后台任务状态机、Job Object |
| `src-tauri/src/autostart.rs` | 注册表 `Run` 项自启动 |

## 3. 构建与验证

```powershell
npm run build -- --no-bundle        # 只编 exe，不出安装包（迭代时用这个）
npm run build                       # 出 NSIS 安装包，落在 src-tauri/target/release/bundle/nsis/
```

**验证界面改动必须把程序真跑起来看截图**，别只断言 DOM 状态。

调试用 CDP：

```powershell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9333'
Start-Process 'src-tauri\target\release\danmuku-video-player.exe'
# 然后 http://127.0.0.1:9333/json 拿 webSocketDebuggerUrl，用 Runtime.evaluate 驱动
```

注意：`document.getElementById('someRange').value` 在面板**没打开过**时读到的是 HTML 初始值，不是运行时配置。验证默认值必须先把面板打开。

```powershell
cargo test --lib                     # Rust 单测（外挂字幕解析、选轨清理、字幕标签剥离）
npm run test:audio                   # 前端选轨决策单测（从 index.html 抠真函数跑，不依赖程序）
```

`cargo test --release sparse_probe -- --nocapture` 是 `mkv_sub` 的真机单测入口，靠环境变量喂片源：

```powershell
$env:SPARSE_TEST_SRC='<mkv>'; $env:SPARSE_TEST_TRACK='9'; $env:SPARSE_TEST_ASS='0'
```

## 4. 硬性约束（都是踩过的坑）

**作用域**：`index.html` 是一个大 IIFE，但内部有二级块作用域——`createPlayer` 在 2 空格缩进，字幕/音频/PGS 那一大坨在 4 空格缩进的块里。**块内的函数声明不会外泄**，从外层调用会 `ReferenceError: pgsPump is not defined`。要跨层用，就显式挪到外层或挂到对象上。

**WebAudio 隔离**：`art.video.crossOrigin = 'anonymous'` 必须在**第一次设 `src` 之前**。页面源是 `tauri.localhost`，素材源是 `asset.localhost`，跨源被污染的媒体抓出来是纯静音，而且一个元素只能抓一次。

**音频规则（用户明确要求过两次）**：不管能不能转，**只能读默认音轨**；默认轨解不了就静音。Chromium 的真实行为是"挑第一条能解的"，不是播默认轨——所以 `classifyAudio()` 算出的 `pin`（`firstPlayable !== audioIndex`）为真时必须强制隔离并静音原轨，否则会放出一条没人要的评论轨。

**`pin` 优先于"编码都能解"**：双 AAC 文件（默认标在第二条）没有任何死编码轨，但内核照样会播错。所以 `handleAudioCodecs()` 的提前返回必须写在 `pin` 之后，音轨菜单入口用 `needsAudioEntry()`（`dead > 0 || pin`）而不是"有没有死编码轨"。改动这块逻辑跑 `npm run test:audio` 回归。

**外挂字幕不依赖 ffmpeg**：`subtitle_text.rs` 自己解析 srt/vtt/ass/ssa（编码探测走 `MultiByteToWideChar` 的 936 代码页）。**安装包不带 ffmpeg** —— `target/release` 里没有 `ffmpeg.exe`，`tauri.conf.json` 也没配 `resources`/`externalBin`，只有开发机 PATH 恰好有才work。任何"外挂字幕走 ffmpeg"的写法等于干净安装的机器上一个外挂字幕都挂不上。ffmpeg 只留给 `subtitle_text` 认不出来的格式兜底。

**删文件前先精确匹配**：`clean_sidecar_tmp()` 只认 `` {stem} (音轨N).tmp.mka `` / `` {stem} (字幕N).tmp.srt ``（`is_our_sidecar_tmp`）。**不要退回「基名前缀 + 含 `.tmp.`」**——实测那样会把用户目录里 `反派 (上).tmp.txt`、`某片 (备注).tmp.mka` 这类普通文件一起删掉，不可逆。

**字幕只进内存**：内嵌文本字幕抽出来直接交给前端做 blob URL，**不写 `(字幕N).srt`**。只有音轨转码会落盘 `(音轨N).mka`。别再往媒体目录里写派生文件。

**稀疏抽取 fail-closed**：`mkv_sub.rs` 只要有一条 cue 解析不出（缺 `CueRelativePosition`、block 带 lacing、`TimestampScale != 1e6`、Cues 被截断、ASS 行格式不对），就整轨返回 `Err` 让调用方回退 ffmpeg。**不要改回"失败就 continue"**——那样会产出一份缺行却看起来正常的 SRT，用户只会觉得"这段怎么没字幕"，而且无从排查。

**ffmpeg 调用**：必须走 `hidden_command()`（`CREATE_NO_WINDOW`），否则会弹黑框；必须 `job::assign()` 进 Job Object，否则宿主被强杀后留下孤儿进程；同时把 `.tmp.*` 半成品清掉。

**后台任务可中断**：换片或关窗要杀当前任务（`transcode_audio_cancel`）。换片时杀在跑的任务，用户换片后新片才不会因为磁盘 IO 抢不到而卡缓冲。

**floating 面板互斥**：音轨、字幕、弹幕密度、弹幕屏蔽四个面板都用 `.block-panel`，固定在同一个 `right:16px; bottom:64px`。新加这类面板必须接进 `closeOtherPanels()`。

**设置项改默认值**：`localStorage` 里已经存过的旧值会盖住新默认。凡是改默认值，必须同时升 `SETTINGS_VER` / `CONFIG_VER` / `DENSITY_VER` 并让旧存档（无版本号）回落到新默认，否则老用户看不到变化。

**发版**：GitHub Release / tag / 上传资产要**每轮单独拿同意**，跟 git push 是两个独立的门。版本号要同时改 `package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml` 三处。

## 5. 代码风格

- 注释用中文，解释**为什么**这么做（尤其是绕过某个引擎缺陷的地方），不要复述代码在做什么。
- 前端是 ES5 风格的 IIFE，别引入构建链或框架；新逻辑跟着现有的 `$()` / `toast()` / `invoke()` 走。
- Rust 侧错误一律 `Result<_, String>` 给前端直接显示，不要吞成 `Option`。
- 仓库只放代码。片源和抽出的 `(音轨N).mka` 都在各自视频旁边，别复制进仓库；临时脚本与测试素材放仓库外（`H:\tmp\`），仓库里不留 `tmp/`、`test*` 这类痕迹。
