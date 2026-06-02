# Tune 入门指南

**Tune** 是一个开源的多房间音乐服务器，将您的本地音乐库与流媒体服务（Tidal、Qobuz、Spotify、Deezer）统一在 Web 和 iPad 界面中。无需依赖云服务即可流式传输到您的 DLNA、AirPlay、Chromecast、BluOS 和 Squeezebox 设备。

> 注：此翻译为初版，欢迎在论坛提出改进建议。

---

## 1. 安装

### Docker（推荐）

```bash
docker run -d \
  --name tune \
  --network host \
  -v /音乐路径:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **重要**：`--network host` 是本地网络上 DLNA/mDNS 发现所必需的。

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

从 [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) 下载 `.exe` 安装程序并运行。

### iPad (TestFlight)

在 [mozaiklabs 论坛](https://mozaiklabs.fr/forum) 请求 TestFlight 邀请。

---

## 2. 首次启动

在浏览器中打开 `http://localhost:8888`（或您的服务器地址）。

引导向导将带您完成以下步骤：

1. 指定包含您音乐的文件夹
2. 启动首次扫描
3. 选择输出区域（DAC、DLNA 扬声器等）

---

## 3. 添加您的音乐库

**设置 → 音乐文件夹 → 添加**

Tune 支持所有常见的音频格式：

- **无损**：FLAC、WAV、AIFF、ALAC、APE、WavPack
- **DSD**：DSF、DFF、DST
- **有损**：MP3、AAC、OGG、Opus、WMA

扫描是渐进式的：歌曲会在索引时陆续出现在库中。对于 10 万首歌曲的库，约需 30 分钟。

---

## 4. 第一个区域

**区域**代表一个音频输出。Tune 自动检测：

- **DLNA/UPnP**：Hi-Fi 流媒体设备（Eversolo、Lindemann、Cocktail Audio、Hifi Rose、Sonos）
- **AirPlay**：Apple 扬声器，兼容 AVR
- **Chromecast**：Google 扬声器，部分电视
- **BluOS**：Bluesound、NAD
- **OAAT**：开源比特完美协议（RPi + USB DAC）
- **本地输出**：连接到服务器的 USB DAC

**设置 → 设备** 列出所有已检测到的设备。

创建区域：**设置 → 区域 → 新建**，选择名称并关联设备。

---

## 5. 首次播放

在顶部栏选择目标区域。然后在库中：

- 点击一首歌 → 立即播放
- 点击 **播放专辑** → 整张专辑加入队列
- 点击播放列表的箭头 → 播放该列表

播放控件（播放/暂停/下一首/音量）位于 Web 客户端底部。

---

## 6. 流媒体服务

**设置 → 流媒体服务** → 连接

| 服务 | 认证方式 | 最高质量 |
|------|---------|---------|
| Tidal | OAuth (HiFi 账户) | FLAC 24/192 |
| Qobuz | 登录/密码 (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | ARL token | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

连接后，服务将出现在 **流媒体** 菜单中。

---

## 7. 多房间

要在**多个区域同时播放同一首歌**：

**设置 → 区域组 → 创建组**

服务器通过 NTP 同步输出。每个区域可调节延迟（**设置 → 区域 → 同步延迟**）。

---

## 8. 进一步学习

- **测试计划**：[docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 项测试
- **API 文档**：`GET /api/v1/system/api-docs` 或通过浏览器访问您的服务器
- **社区论坛**：https://mozaiklabs.fr/forum
- **GitHub**：https://github.com/renesenses/tune-server-rust
- **CLI**：`cargo install tune-cli` 从终端控制

祝您聆听愉快！
