# danmuku-video-player

本地视频弹幕播放器（Danmuku Video Player 桌面版）：打开本机视频，自动加载同名弹幕与字幕一起播放。

![播放器界面](docs/screenshot.png)

## 功能

- 视频与同目录同名 `.xml` / `.json` 弹幕自动配对，兼容 B 站导出的弹幕格式
- 字幕：自动挂载同目录外挂 `.srt` / `.ass` / `.vtt`（GBK 自动识别），MKV 内嵌文本字幕按需后台抽取；📝 菜单可切换来源，PGS 图形字幕不支持（菜单中标注）
- 拖入文件夹即生成剧集播放列表，点击换集、播完自动下一集
- 弹幕参数可调（按类型屏蔽、防重叠、透明度、显示区域、字号、速度），屏蔽词表改完立即生效

## 运行要求

Windows 10 / 11，需要 WebView2 运行时。

## 构建

需要 Node.js 18+ 和 Rust 工具链。

```powershell
npm install
npm run build
```

安装包输出到 `src-tauri/target/release/bundle/nsis/`。

## 许可证

[MIT](LICENSE)

## 致谢

- [ArtPlayer](https://github.com/zhw2590582/ArtPlayer) — 播放器内核与弹幕/字幕/音轨插件

