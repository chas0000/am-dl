//! MV（音乐视频）下载：PlayReady 取密钥 → CBCS 解密 → 音视频合并 → progressive MP4。
//!
//! 与上游 `src/ui/mv-engine.mjs` + `crates/am-media` 的 wasm 导出等价，
//! 只是把 OPFS 换成本地文件、把 Worker 换成直接调用。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use am_media::defrag::{defragment, Options as DefragOptions};
use am_media::mv::{mux_init, Stream};
use am_media::playready::{parse_pssh, Cdm, Device, Entropy};
use am_media::readahead::{Sink, Source};

use crate::dl::m3u8::{absolute, attributes};
use crate::dl::model::{JobOptions, TrackSpec};
use crate::dl::tags::{self, Cover};
use crate::dl::download::Downloaded;
use crate::config::AppState;

/* ---------- HLS 解析 ---------- */

#[derive(Debug, Clone, Default)]
// 这些字段是解析 m3u8 时保留的完整信息，当前只用其中一部分
#[allow(dead_code)]
pub struct Variant {
    pub uri: String,
    pub codecs: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub audio_group: String,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AudioGroup {
    pub uri: String,
    pub name: String,
    pub channels: String,
}

#[derive(Debug, Clone, Default)]
pub struct Resource {
    pub url: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Segment {
    pub res: Resource,
    pub key: Option<String>,
    pub start: f64,
    pub duration: f64,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub init: Resource,
    pub segments: Vec<Segment>,
    pub duration: f64,
}

/// 解析 MV 的 master m3u8：视频变体 + 音频组
pub fn parse_master(text: &str) -> Result<(Vec<Variant>, HashMap<String, AudioGroup>), String> {
    let mut groups = HashMap::new();
    let mut variants = Vec::new();
    let mut pending: Option<String> = None;

    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attributes(rest);
            if a.get("TYPE") == Some(&"AUDIO") {
                if let Some(group) = a.get("GROUP-ID") {
                    groups.insert(
                        group.to_string(),
                        AudioGroup {
                            uri: a.get("URI").unwrap_or(&"").to_string(),
                            name: a.get("NAME").unwrap_or(&"").to_string(),
                            channels: a.get("CHANNELS").unwrap_or(&"").to_string(),
                        },
                    );
                }
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(rest.to_string());
        } else if !line.is_empty() && !line.starts_with('#') && (line.contains(".m3u8") || line.starts_with("http")) {
            let raw_attrs = pending.take().unwrap_or_default();
            let attrs = attributes(&raw_attrs);
            let (w, h) = attrs
                .get("RESOLUTION")
                .and_then(|r| r.split_once('x'))
                .map(|(w, h)| (w.parse().unwrap_or(0), h.parse().unwrap_or(0)))
                .unwrap_or((0, 0));
            variants.push(Variant {
                uri: line.to_string(),
                codecs: attrs.get("CODECS").unwrap_or(&"").to_string(),
                bandwidth: attrs
                    .get("AVERAGE-BANDWIDTH")
                    .or_else(|| attrs.get("BANDWIDTH"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                width: w,
                height: h,
                audio_group: attrs.get("AUDIO").unwrap_or(&"").to_string(),
            });
        }
    }
    if variants.is_empty() {
        return Err("MV master 里没有视频变体".into());
    }
    Ok((variants, groups))
}

/// 解析 MV 的 media m3u8（分片是独立地址，key 为 PlayReady 的 data: URI）
pub fn parse_media(text: &str, base_url: &str) -> Result<Playlist, String> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("不是合法的 HLS media playlist".into());
    }
    if !text.contains("#EXT-X-ENDLIST") {
        return Err("只支持已完结的 MV 播放列表".into());
    }
    let mut segments = Vec::new();
    let mut init: Option<Resource> = None;
    let mut key: Option<String> = None;
    let mut encrypted = false;
    let mut duration: Option<f64> = None;
    let mut range: Option<String> = None;
    let mut time = 0.0;
    let mut previous: Option<(String, u64)> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attributes(rest);
            encrypted = a.get("METHOD") != Some(&"NONE");
            if !encrypted {
                key = None;
            } else if a.get("KEYFORMAT") == Some(&"com.microsoft.playready") {
                let uri = a.get("URI").copied().unwrap_or("");
                if !uri.starts_with("data:") || !uri.contains(";base64,") {
                    return Err("不支持的 PlayReady key URI".into());
                }
                key = Some(uri.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attributes(rest);
            let uri = a.get("URI").copied().ok_or("EXT-X-MAP 没有 URI")?;
            let range = a.get("BYTERANGE").map(|r| parse_range(r, &uri, None)).transpose()?.flatten();
            init = Some(Resource { url: resolve_url(base_url, uri), range });
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest.split(',').next().and_then(|d| d.trim().parse::<f64>().ok());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(rest.to_string());
        } else if line == "#EXT-X-DISCONTINUITY" {
            return Err("不支持带 DISCONTINUITY 的 MV 播放列表".into());
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = resolve_url(base_url, line);
            let r = match range.take() {
                Some(spec) => parse_range(&spec, &url, previous.as_ref())?,
                None => None,
            };
            if let Some((off, len)) = r {
                previous = Some((url.clone(), off + len));
            } else {
                previous = None;
            }
            let d = duration.take().ok_or("分片缺少 EXTINF")?;
            if init.is_none() {
                return Err("分片出现在 EXT-X-MAP 之前".into());
            }
            segments.push(Segment { res: Resource { url, range: r }, key: key.clone(), start: time, duration: d });
            time += d;
        }
    }

    if segments.is_empty() {
        return Err("MV 播放列表里没有分片".into());
    }
    // 部分较早的 MV（如 1175186184）没有 EXT-X-KEY，整条流是未加密的：
    // 放行，解密环节遇到没有 senc 的分片会原样通过
    if encrypted && !segments.iter().any(|s| s.key.is_some()) {
        return Err("播放列表标了加密，但没有找到 PlayReady 的 key".into());
    }
    let init = init.ok_or("播放列表缺少 EXT-X-MAP")?;
    Ok(Playlist { init, segments, duration: time })
}

/// 相对地址 → 绝对地址，并继承 base 的查询串。
/// Apple 的 accessKey 挂在 media playlist 的 query 上，分片必须带着它，否则 403。
fn resolve_url(base: &str, uri: &str) -> String {
    let abs = absolute(base, uri);
    if abs.contains('?') {
        return abs;
    }
    match base.split_once('?') {
        Some((_, query)) => format!("{abs}?{query}"),
        None => abs,
    }
}

/// `LEN[@OFF]`；省略 OFF 时接在同一地址上一段的末尾
fn parse_range(spec: &str, url: &str, previous: Option<&(String, u64)>) -> Result<Option<(u64, u64)>, String> {
    let spec = spec.trim().trim_matches('"');
    let (len, off) = match spec.split_once('@') {
        Some((l, o)) => (l, Some(o)),
        None => (spec, None),
    };
    let length: u64 = len.parse().map_err(|_| format!("非法 BYTERANGE: {spec}"))?;
    let offset = match off {
        Some(o) => o.parse().map_err(|_| format!("非法 BYTERANGE 偏移: {spec}"))?,
        None => previous
            .filter(|(u, _)| u == url)
            .map(|(_, end)| *end)
            .ok_or_else(|| format!("BYTERANGE 缺少偏移且无法承接上一段: {spec}"))?,
    };
    Ok(Some((offset, length)))
}

/* ---------- 下载 ---------- */

async fn fetch_text(state: &AppState, url: &str, ua_am: bool) -> Result<String, String> {
    let mut req = state.http_client.get(url).timeout(Duration::from_secs(30));
    if ua_am {
        req = req.header(reqwest::header::USER_AGENT, "AM");
    }
    let resp = req.send().await.map_err(|e| format!("请求 {url} 失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("请求 {url} 返回 HTTP {}", resp.status()));
    }
    resp.text().await.map_err(|e| format!("读取 {url} 失败: {e}"))
}

async fn fetch_resource(state: &AppState, res: &Resource) -> Result<Vec<u8>, String> {
    let mut req = state.http_client.get(&res.url).timeout(Duration::from_secs(120));
    if let Some((off, len)) = res.range {
        req = req.header(reqwest::header::RANGE, format!("bytes={}-{}", off, off + len - 1));
    }
    let resp = req.send().await.map_err(|e| format!("拉取 {} 失败: {e}", res.url))?;
    if !resp.status().is_success() {
        return Err(format!("拉取 {} 返回 HTTP {}", res.url, resp.status()));
    }
    let body = resp.bytes().await.map_err(|e| format!("读取 {} 失败: {e}", res.url))?;
    if let Some((off, len)) = res.range {
        if body.len() as u64 > len {
            let start = off as usize;
            let end = (start + len as usize).min(body.len());
            return Ok(body[start.min(end)..end].to_vec());
        }
    }
    Ok(body.to_vec())
}

/// PlayReady：把 data: URI 里的 PSSH 变成 license 请求，拿回 16 字节内容密钥
async fn playready_key(state: &Arc<AppState>, adam_id: &str, uri: &str) -> Result<[u8; 16], String> {
    let mut fill = |buf: &mut [u8]| {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(buf);
        }
    };

    let mut cdm = Cdm::new(Device::default_device().map_err(|e| e.to_string())?);
    let session = cdm.open(&mut Entropy(&mut fill));

    let (_, payload) = uri.split_once(',').ok_or("PlayReady URI 缺少数据段")?;
    let header = match parse_pssh(payload) {
        Ok(h) => h,
        Err(_) => {
            // 回退：直接当 KID 用
            let decoded = am_media::crypto::base64_decode(payload).ok_or("PlayReady URI 无法解析")?;
            if decoded.len() != 16 {
                cdm.close(&session);
                return Err("PlayReady URI 既不是 PSSH 也不是 KID".into());
            }
            format!(
                r#"<WRMHEADER xmlns="http://schemas.microsoft.com/DRM/2007/03/PlayReadyHeader" version="4.0.0.0"><DATA><PROTECTINFO><KEYLEN>16</KEYLEN><ALGID>AESCTR</ALGID></PROTECTINFO><KID>{payload}</KID></DATA></WRMHEADER>"#
            )
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let challenge = match cdm.license_challenge(&session, &header, now, &mut Entropy(&mut fill)) {
        Ok(c) => c,
        Err(e) => {
            cdm.close(&session);
            return Err(format!("生成 PlayReady challenge 失败: {e}"));
        }
    };

    let body = serde_json::json!({
        "adamId": adam_id,
        "uri": uri,
        // wrapper-lite 期望 challenge 是 base64（与上游 wasm 的 challenge_json 一致）
        "challenge": am_media::crypto::base64_encode(challenge.as_bytes()),
        "drm-type": "pr",
    });
    let reply = state
        .wrapper
        .send(state.wrapper.post("/license").json(&body))
        .await
        .map_err(|e| format!("请求 wrapper-lite /license 失败: {e}"))?;
    if !reply.status.is_success() {
        cdm.close(&session);
        return Err(format!("wrapper-lite /license 返回 HTTP {}", reply.status));
    }
    let v: serde_json::Value = reply.json().map_err(|e| format!("解析 /license 响应失败: {e}"))?;
    if v.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        cdm.close(&session);
        let msg = v.get("msg").and_then(serde_json::Value::as_str).unwrap_or("未知错误");
        return Err(format!("wrapper-lite 返回错误: {msg}"));
    }
    let license = v
        .pointer("/data/license")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "wrapper-lite 没有返回 license".to_string())?;
    let xml = am_media::crypto::base64_decode(license.trim()).ok_or("license 不是合法 base64")?;
    let keys = cdm.parse_license(&session, &xml).map_err(|e| e.to_string())?;
    cdm.close(&session);
    if keys.len() != 1 {
        return Err(format!("期望 1 个内容密钥，实际 {}", keys.len()));
    }
    Ok(keys[0].1)
}

/// 一个已准备好的流（init + 分片 + 密钥表）
struct Prepared {
    stream: Stream,
    init: Vec<u8>,
    playlist: Playlist,
    keys: HashMap<String, [u8; 16]>,
}

async fn prepare(
    state: &Arc<AppState>,
    adam_id: &str,
    url: &str,
    name: &'static str,
    base: u32,
    cache: &mut HashMap<String, [u8; 16]>,
) -> Result<Prepared, String> {
    let text = fetch_text(state, url, false).await?;
    let playlist = parse_media(&text, url)?;

    let mut keys = HashMap::new();
    let uris: Vec<String> = playlist.segments.iter().filter_map(|s| s.key.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect();
    for uri in uris {
        let key = match cache.get(&uri) {
            Some(k) => *k,
            None => {
                let k = playready_key(state, adam_id, &uri).await?;
                cache.insert(uri.clone(), k);
                k
            }
        };
        keys.insert(uri, key);
    }

    let init_raw = fetch_resource(state, &playlist.init).await?;
    let (stream, init) = Stream::open(&init_raw, base).map_err(|e| e.to_string())?;
    let _ = name;
    Ok(Prepared { stream, init, playlist, keys })
}

fn moof_count(data: &[u8]) -> u32 {
    let mut n = 0u32;
    let mut at = 0usize;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if size < 8 || at + size > data.len() {
            break;
        }
        if &data[at + 4..at + 8] == b"moof" {
            n += 1;
        }
        at += size;
    }
    n
}

struct FileSource {
    file: std::fs::File,
    size: u64,
}

impl Source for FileSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, buf: &mut [u8], at: u64) -> usize {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, at).unwrap_or(0)
    }
}

struct FileSink {
    file: std::fs::File,
}

impl Sink for FileSink {
    fn write(&mut self, buf: &[u8]) -> am_media::Result<()> {
        use std::io::Write;
        self.file.write_all(buf).map_err(|e| am_media::Error::msg(format!("写入失败: {e}")))
    }
}

/// 下载一首 MV，输出 `out_path`（.mp4）
pub async fn download_mv(
    state: &Arc<AppState>,
    spec: &TrackSpec,
    opts: &JobOptions,
    out_path: &Path,
    temp_dir: &Path,
    cancel: &AtomicBool,
) -> Result<Downloaded, String> {
    // 1. master（必须带 UA: AM，否则拿不到高码率）
    let reply = state
        .wrapper
        .send(state.wrapper.get("/webplayback").query(&[("adamId", spec.adam_id.as_str())]))
        .await
        .map_err(|e| format!("请求 wrapper-lite /webplayback 失败: {e}"))?;
    if !reply.status.is_success() {
        return Err(format!("wrapper-lite /webplayback 返回 HTTP {}", reply.status));
    }
    let v: serde_json::Value = reply.json().map_err(|e| format!("解析 /webplayback 响应失败: {e}"))?;
    if v.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        let msg = v.get("msg").and_then(serde_json::Value::as_str).unwrap_or("未知错误");
        return Err(format!("wrapper-lite 返回错误: {msg}"));
    }
    let master_url = v
        .pointer("/data/m3u8")
        .and_then(serde_json::Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "wrapper-lite 没有返回 MV master 地址".to_string())?;
    let master_text = fetch_text(state, master_url, true).await?;
    let (variants, groups) = parse_master(&master_text)?;

    // 2. 选视频（不超过限制的最高分辨率）与音频
    let max_h = opts.max_height as u32;
    let mut videos: Vec<&Variant> = variants.iter().filter(|v| max_h == 0 || v.height <= max_h).collect();
    if videos.is_empty() {
        videos = variants.iter().collect();
    }
    videos.sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    let video = (*videos.first().ok_or("没有可用的视频变体")?).clone();
    let audio = groups.get(&video.audio_group).filter(|g| !g.uri.is_empty()).cloned();

    let mut cache = HashMap::new();
    let video_url = resolve_url(master_url, &video.uri);
    let v_stream = prepare(state, &spec.adam_id, &video_url, "video", 1, &mut cache).await?;
    let a_stream = match &audio {
        Some(group) => {
            let url = resolve_url(master_url, &group.uri);
            Some(prepare(state, &spec.adam_id, &url, "audio", 100, &mut cache).await?)
        }
        None => None,
    };

    let duration = v_stream
        .playlist
        .duration
        .max(a_stream.as_ref().map(|s| s.playlist.duration).unwrap_or(0.0));

    // 3. 合并 init，写入临时文件
    let tmp = temp_dir.join(format!("{}.mv.fmp4", sanitize(&spec.adam_id)));
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| format!("创建临时文件 {} 失败: {e}（下载根目录不可写？）", tmp.display()))?;
    {
        use std::io::Write;
        let init = match &a_stream {
            Some(a) => mux_init(&v_stream.stream, &a.stream, duration).map_err(|e| e.to_string())?,
            None => v_stream.init.clone(),
        };
        file.write_all(&init).map_err(|e| format!("写入 init 失败: {e}"))?;
    }

    // 4. 先写完整条视频轨，再写音频轨（与上游 / 参考实现的顺序一致）
    let mut sequence = 1u32;
    let mux = a_stream.is_some();
    let mut streams: Vec<(&str, Prepared)> = Vec::new();
    streams.push(("video", v_stream));
    if let Some(a) = a_stream {
        streams.push(("audio", a));
    }

    for (idx, (_, mut prepared)) in streams.into_iter().enumerate() {
        let _ = idx;
        for seg in prepared.playlist.segments.clone() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err("已取消".into());
            }
            let raw = fetch_resource(state, &seg.res).await?;
            let key = seg.key.as_ref().and_then(|u| prepared.keys.get(u)).copied().unwrap_or([0u8; 16]);
            let mut data = raw;
            let out = prepared.stream.fragment(&mut data, &key, mux, sequence).map_err(|e| e.to_string())?;
            if mux {
                sequence += moof_count(&out);
            }
            use std::io::Write;
            file.write_all(&out).map_err(|e| format!("写入分片失败: {e}"))?;
        }
    }
    drop(file);

    // 5. 标签 + 封面
    let tags_blob = if opts.write_tags {
        let cover_bytes = if opts.embed_cover && !spec.cover_url.is_empty() {
            fetch_cover(state, &spec.cover_url).await
        } else {
            None
        };
        let cover = cover_bytes.as_ref().map(|(b, png)| Cover { bytes: b, png: *png });
        tags::build_mv_ilst(spec, cover)
    } else {
        Vec::new()
    };

    // 6. defrag（MV 布局）
    let source = FileSource {
        file: std::fs::File::open(&tmp).map_err(|e| format!("打开临时文件失败: {e}"))?,
        size: std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0),
    };
    let final_tmp = temp_dir.join(format!("{}.mv.mp4", sanitize(&spec.adam_id)));
    let out_file = std::fs::File::create(&final_tmp).map_err(|e| format!("创建输出文件失败: {e}"))?;
    let mut sink = FileSink { file: out_file };
    let mut options = DefragOptions::mv();
    if !tags_blob.is_empty() {
        options = options.with_tags(tags_blob);
    }
    defragment(source, &mut sink, &options).map_err(|e| format!("封装 MP4 失败: {e}"))?;
    drop(sink);

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    std::fs::rename(&final_tmp, out_path).map_err(|e| format!("移动到目标路径失败: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    let bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);

    let quality = match &audio {
        Some(a) => format!("{}p {} · {}", video.height, video.codecs, if a.name.is_empty() { "audio" } else { &a.name }),
        None => format!("{}p {}", video.height, video.codecs),
    };
    Ok(Downloaded { path: out_path.to_path_buf(), bytes, quality })
}

async fn fetch_cover(state: &AppState, url: &str) -> Option<(Vec<u8>, bool)> {
    let resp = state.http_client.get(url).timeout(Duration::from_secs(20)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?.to_vec();
    let png = bytes.starts_with(&[0x89, b'P', b'N', b'G']);
    Some((bytes, png))
}

fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_parses_audio_groups() {
        let text = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Dolby Atmos\",CHANNELS=\"16/JOC\",URI=\"a1.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=9000000,AVERAGE-BANDWIDTH=8000000,CODECS=\"hvc1.2.4\",RESOLUTION=3840x2160,AUDIO=\"aud\"\n\
v1.m3u8\n";
        let (variants, groups) = parse_master(text).unwrap();
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].height, 2160);
        assert_eq!(variants[0].audio_group, "aud");
        assert_eq!(groups["aud"].name, "Dolby Atmos");
    }

    #[test]
    fn media_parses_playready() {
        let text = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT=\"com.microsoft.playready\",URI=\"data:text/plain;base64,AAA=\"\n\
#EXTINF:6.0,\nseg1.m4s\n\
#EXTINF:6.0,\nseg2.m4s\n\
#EXT-X-ENDLIST\n";
        let p = parse_media(text, "https://cdn/x/media.m3u8").unwrap();
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].res.url, "https://cdn/x/seg1.m4s");
        assert!(p.segments[0].key.is_some());
        assert!((p.duration - 12.0).abs() < 1e-9);
    }
}
