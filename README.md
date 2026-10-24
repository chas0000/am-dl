# am-dl

Apple Music **服务端下载器** + **上游 am-hook 的反向代理**。

两个二进制，一个对外端口：

- `am-hook`（上游官方预编译二进制，跑在 `127.0.0.1:8899`）：播放器 / 资料库 / 电台 / 浏览器端解密
- `am-dl`（本项目，跑在 `0.0.0.0:8888`）：`/dl` 下载页 + `/api/dl/*`，其余请求全部反代给 am-hook，并往 HTML 里注入「下载到服务器」浮窗

好处是**不再给上游打补丁、也不用自己编译整个 workspace**：上游更新时只换一个二进制。

```
浏览器 ──► am-dl :8888 ──┬─ /dl、/api/dl/*      → 自己的下载器
                         └─ 其他（页面、资源）   → am-hook :8899（HTML 注入浮窗）
```

## 项目结构

```
src/
  main.rs        入口、环境变量、路由（有 AM_DL_UPSTREAM 时挂反代）
  proxy.rs       反向代理 + HTML 注入浮窗脚本
  config.rs      配置与共享状态
  amp.rs         amp-api 最小客户端（developer token 缓存）
  wrapper.rs     wrapper-lite 客户端（限并发）
  links.rs       链接解析（专辑/歌单/单曲/MV/歌手）
  dl/
    mod.rs       任务队列、HTTP API、鉴权、多根目录
    model.rs     任务/曲目/选项数据模型
    resolve.rs   链接 → 曲目列表（走 amp-api）
    m3u8.rs      master / media playlist 解析、音质选择
    download.rs  分片下载、原地解密、defrag 落盘、标签
    mv.rs        MV：PlayReady + CBCS、音视频合并
    tags.rs      iTunes 标签、文件名模板
    lyrics.rs    TTML → LRC / 纯文本
  ui/            /dl 页面、浮窗脚本（编译进二进制）
scripts/
  install-upstream.sh   下载上游预编译 am-hook 并装成服务
  deploy.sh             构建 + 装 am-dl 服务（幂等）
deploy/
  am-dl.service.in      systemd 模板
```

## 为什么不把下载模块编进 am-hook

上游用 `include_bytes!` 把 `src/ui/` 全部编译进二进制（`src/assets.rs` 的 `ass!` 宏），**没有任何运行时静态目录或插件入口**，所以预编译二进制里塞不进额外模块。想复用官方二进制，只能旁路 + 反代注入。

## 部署

前提：Linux + systemd、root、`cargo`（Rust 1.75+）、一个可用的 wrapper-lite。

```bash
cd /path/to/am-dl

# 1) 上游播放器（官方预编译，装成 am-hook.service，只监听 127.0.0.1:8899）
./scripts/install-upstream.sh            # 最新版
./scripts/install-upstream.sh v0.5.7     # 指定版本

# 2) 下载器 + 反代（装成 am-dl.service，对外 0.0.0.0:8888）
./scripts/deploy.sh --root /srv/music --upstream http://127.0.0.1:8899
```

打开 `http://<内网IP>:8888/`（播放器）或 `/dl`（下载中心）。

不带 `--upstream` 时 am-dl 退化成纯下载器：`/` 直接跳到 `/dl`，不代理、不注入。

### deploy.sh 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--root <路径>` | 沿用当前配置，否则 `/root/music` | 下载根目录 |
| `--roots <a>:<b>` | — | 多个根目录（页面上可切换） |
| `--port <端口>` | `8888` | 监听端口 |
| `--listen <地址>` | `0.0.0.0` | 监听地址 |
| `--upstream <URL>` | 沿用当前配置 | 上游 am-hook 地址；设置后开启代理 + 注入 |
| `--wrapper <URL>` | `http://127.0.0.1:12340` | wrapper-lite 地址 |
| `--concurrency <N>` | `2` | 同时下载的曲目数 |
| `--token <令牌>` | 不设 | 不设 = 无鉴权 |
| `--no-build` | — | 只重装服务，不编译 |
| `--uninstall` | — | 停止并移除服务（不动文件） |

**没显式传的参数会从已安装的 systemd 单元里继承**，所以重跑脚本不会把下载目录/令牌重置回默认值。

## 发版

版本号不用手工改 `Cargo.toml`：推 `v*` tag 时 CI 会把 tag 写进 `Cargo.toml` 再构建，二进制里的版本来自 `git describe`（`am-dl --version` 可查）。

```bash
./scripts/release.sh 0.2.0     # 改版本 + fmt/clippy/test + 提交 + 打 tag + 推送
```

推 tag 后 GitHub Actions 构建 `linux-x86_64` / `linux-aarch64` 两个包并发布到 Release，附 `SHA256SUMS.txt`。

## 更新

```bash
# 上游（秒级，不编译）
./scripts/install-upstream.sh              # 拉最新 release 并重启 am-hook

# 本项目（改了代码时）
./scripts/deploy.sh                        # 重新编译 + 装服务
```

上游只发布 4 个平台（Linux x86_64、macOS arm64/x86_64、Windows x86_64）的预编译二进制；没有对应架构时得自己 `cargo build --release` 编译 am-hook 本体，再改 `install-upstream.sh` 的下载步骤。

## 配置

环境变量（写进 systemd 单元，也可以直接跑二进制时设）：

| 变量 | 默认 | 说明 |
|---|---|---|
| `AM_DL_ROOT` | `/root/music` | 单个下载根目录 |
| `AM_DL_ROOTS` | — | 多根目录，`:` 分隔，第一个为默认 |
| `AM_DL_UPSTREAM` | 空 | 上游 am-hook 地址；空 = 不代理、不注入 |
| `AM_DL_TOKEN` | 空 | 访问令牌；不设 = 无鉴权 |
| `AM_DL_CONCURRENCY` | `2` | 同时下载的曲目数 |
| `AM_DL_WRAPPER` | `http://127.0.0.1:12340` | wrapper-lite 地址 |
| `AM_DL_WRAPPER_AUTH` | 空 | wrapper-lite 的 Authorization（可选） |
| `AM_DL_LISTEN` | `0.0.0.0:8888` | 监听地址（直接跑二进制时用） |

任务参数：`dest` `quality` `max_sample_rate` `max_height` `album_dir` `track_file` `write_tags` `embed_cover` `lyrics` `syllable` `skip_existing` `limit`。

## 使用

页面：`http://<IP>:8888/dl`（设了 `AM_DL_TOKEN` 就加 `?token=…`）。

```bash
BASE=http://127.0.0.1:8888

curl -s -X POST "$BASE/api/dl/preview" -H 'content-type: application/json' \
  -d '{"url":"https://music.apple.com/cn/album/xxx/123456"}'

curl -s -X POST "$BASE/api/dl/jobs" -H 'content-type: application/json' \
  -d '{"url":"https://music.apple.com/cn/playlist/xxx/pl.abc","quality":"alac"}'

curl -s "$BASE/api/dl/jobs"
curl -s -X POST "$BASE/api/dl/jobs/batch" -H 'content-type: application/json' \
  -d '{"action":"remove","filter":"done"}'
```

支持链接：专辑 / 歌单 / 单曲 / MV / 歌手。

**歌手链接会按专辑拆成多个任务**：每张专辑一个顶层任务，各自有独立的状态和进度。一个任务里塞几千条子项时，列表每 3 秒重绘一次根本没法看失败项，拆开之后每张专辑都是十来二十首，好定位。

`/dl` 的子项列表里，有失败项时会多一个「只看失败」开关；列表刷新会保留滚动位置，不再每 3 秒跳回顶部。

## 目录与命名

默认模板：`{album_artist}/{album} ({year}) [{quality}]/{tracknum:02d} {title}.m4a`

`{quality}` 是**实际解析到的音质**，不是请求里写的档位：

- ALAC 带位深与采样率：`[ALAC 24bit-96kHz]`、`[ALAC 16bit-44.1kHz]`
- 其他：`[AAC]`、`[Dolby Atmos]`、`[HE-AAC]`、`[AAC Binaural]`、`[AAC Downmix]`
- MV：`[MV 720p]`（按 `max_height` 上限）

同一张专辑的 ALAC 与 AAC 会落在两个目录里，互不覆盖；重跑同一音质仍然按"文件已存在"跳过。

代价：模板里用到 `{quality}` 时，每首歌会**多一次 master m3u8 请求**（几百字节，用于在下载前确定路径）。不想付这个代价就把 `[{quality}]` 从 `album_dir` 里去掉，此时按请求档位命名不了，也就回到同名覆盖的老行为。

## 常见问题

**改了下载目录后报 `No such file or directory`**

systemd 单元里 `ReadWritePaths=` 必须和 `AM_DL_ROOT` 指向同一个目录（`ProtectSystem=strict` 下它决定服务能写哪里）。用 `./scripts/deploy.sh --root <新路径>` 改，两行会一起更新。启动时也会自检：目录建不出来或不可写会直接报错并点名 `ReadWritePaths`。

**8888 起不来 / 健康检查却通过**

端口被别的服务占着时，`/api/dl/config` 也可能返回 200。部署脚本现在查 `/api/dl/ping` 并确认应答来自 am-dl。

**浮窗没出现**

确认 am-dl 带 `AM_DL_UPSTREAM` 启动（`systemctl cat am-dl | grep UPSTREAM`），并且访问的是 am-dl 的端口，不是 am-hook 的 8899。

## 注意

- 上游 crate 是 **AGPL-3.0**；本项目沿用同一许可。
- 服务端下载会把媒体流量全部转移到这台机器，注意磁盘与流量（ALAC 单曲 30–80 MB，MV 数百 MB）。
- wrapper-lite 的 `/key` 有速率限制，批量下载别把并发开太大。
- 公网暴露务必设 `--token`，最好再套 HTTPS 反代。
- 仅用于你有权访问的内容，遵守 Apple 的服务条款。
