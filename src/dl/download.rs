//! 单曲下载流水线：wrapper-lite 取地址 → 拉分片 → 原地解密 → 落盘 → defrag 成 progressive MP4。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use am_media::defrag::{defragment, Options as DefragOptions};
use am_media::readahead::{Sink, Source};
use am_mp4::{
    alac_track, decrypt_fragment_in_place, fixed_template, patch_init_in_place,
    repair_alac_fragment, template_from_json, Template,
};

use crate::config::AppState;
use crate::dl::m3u8::{self, Segment};
use crate::dl::model::{JobOptions, TrackSpec};
use crate::dl::tags::{self, Cover};

/// 每个分片的下载+解密重试次数
const SEGMENT_RETRIES: u32 = 3;
/// 同时进行的分片数
const SEGMENT_CONCURRENCY: usize = 4;

pub struct Downloaded {
    pub path: PathBuf,
    pub bytes: u64,
    pub quality: String,
}

fn client(state: &AppState) -> &reqwest::Client {
    &state.http_client
}

async fn fetch_text(state: &AppState, url: &str) -> Result<String, String> {
    let resp = client(state)
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("请求 {url} 失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("请求 {url} 返回 HTTP {}", resp.status()));
    }
    resp.text()
        .await
        .map_err(|e| format!("读取 {url} 失败: {e}"))
}

async fn fetch_range(state: &AppState, url: &str, seg: Segment) -> Result<Vec<u8>, String> {
    let range = format!("bytes={}-{}", seg.offset, seg.end() - 1);
    let resp = client(state)
        .get(url)
        .header(reqwest::header::RANGE, range.clone())
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("拉取分片 {range} 失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("拉取分片 {range} 返回 HTTP {}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("读取分片 {range} 失败: {e}"))?;
    // 上游忽略 Range 时自行截取
    if body.len() as u64 > seg.length {
        let start = seg.offset as usize;
        let end = (start + seg.length as usize).min(body.len());
        Ok(body[start.min(end)..end].to_vec())
    } else {
        Ok(body.to_vec())
    }
}

async fn fetch_range_retry(state: &AppState, url: &str, seg: Segment) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 1..=SEGMENT_RETRIES {
        match fetch_range(state, url, seg).await {
            Ok(data) if data.len() as u64 == seg.length => return Ok(data),
            Ok(data) => last = format!("分片长度不符：期望 {} 实际 {}", seg.length, data.len()),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(300 * attempt as u64)).await;
    }
    Err(last)
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
        self.file
            .write_all(buf)
            .map_err(|e| am_media::Error::msg(format!("写入失败: {e}")))
    }
}

/// 取 master m3u8 地址（wrapper-lite `/m3u8`）
async fn master_url(state: &AppState, adam_id: &str) -> Result<String, String> {
    let reply = state
        .wrapper
        .send(state.wrapper.get("/m3u8").query(&[("adamId", adam_id)]))
        .await
        .map_err(|e| format!("请求 wrapper-lite /m3u8 失败: {e}"))?;
    if !reply.status.is_success() {
        return Err(format!("wrapper-lite /m3u8 返回 HTTP {}", reply.status));
    }
    let v: serde_json::Value = reply
        .json()
        .map_err(|e| format!("解析 /m3u8 响应失败: {e}"))?;
    if v.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        let msg = v
            .get("msg")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("wrapper-lite 返回错误: {msg}"));
    }
    v.pointer("/data/m3u8")
        .and_then(serde_json::Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "wrapper-lite 没有返回 master m3u8 地址".to_string())
}

/// 只解析音质标签、不下载：模板里用到 `{quality}` 时，路径必须在下载前定下来。
/// MV 直接按分辨率上限给标签，不额外请求。
pub async fn plan_label(
    state: &Arc<AppState>,
    spec: &TrackSpec,
    opts: &JobOptions,
) -> Result<String, String> {
    if spec.is_mv {
        return Ok(if opts.max_height > 0 {
            format!("MV {}p", opts.max_height)
        } else {
            "MV".to_string()
        });
    }
    let master = master_url(state, &spec.adam_id).await?;
    let master_text = fetch_text(state, &master).await?;
    let variants = m3u8::parse_master_variants(&master_text)?;
    let variant = m3u8::pick_variant(&variants, opts.quality, opts.max_sample_rate)?;
    Ok(variant.short_label())
}

/// 下载并解密一首歌，输出到 `out_path`（.m4a）
pub async fn download_track(
    state: &Arc<AppState>,
    spec: &TrackSpec,
    opts: &JobOptions,
    out_path: &Path,
    temp_dir: &Path,
    cancel: &AtomicBool,
) -> Result<Downloaded, String> {
    // 1. master → 选变体
    let master = master_url(state, &spec.adam_id).await?;
    let master_text = fetch_text(state, &master).await?;
    let variants = m3u8::parse_master_variants(&master_text)?;
    let variant = m3u8::pick_variant(&variants, opts.quality, opts.max_sample_rate)?;
    let quality = variant.label();

    // 2. media m3u8 → 分片表
    let media_url = m3u8::absolute(&master, &variant.uri);
    let media_text = fetch_text(state, &media_url).await?;
    let track = m3u8::parse_media_m3u8(&media_url, &media_text)?;
    let file_url = m3u8::absolute(&media_url, &track.file_uri);

    // 3. init 段（改写 enca → 原始编码，FairPlay box → free）
    let mut init = fetch_range_retry(state, &file_url, track.segments[0]).await?;
    patch_init_in_place(&mut init);

    // 4. 轨道解密模板
    let template_json = state
        .wrapper
        .fetch_key_json(&spec.adam_id, &track.key_uri)
        .await?;
    let template: Arc<Template> = Arc::new(template_from_json(&template_json)?);

    // 5. 分片：并发拉取 + 解密，按序写入临时 fMP4
    let tmp_mp4 = temp_dir.join(format!("{}.frag.mp4", sanitize_id(&spec.adam_id)));
    let mut file = std::fs::File::create(&tmp_mp4).map_err(|e| format!("创建临时文件失败: {e}"))?;
    {
        use std::io::Write;
        file.write_all(&init)
            .map_err(|e| format!("写入 init 失败: {e}"))?;
    }

    let segments = track.segments.clone();
    let total_segments = segments.len();
    let alac = Arc::new(alac_track(&init));
    let state_for_fetch = state.clone();
    let file_url_for_fetch = file_url.clone();

    let mut stream = futures::stream::iter(1..total_segments)
        .map(|idx| {
            let state = state_for_fetch.clone();
            let url = file_url_for_fetch.clone();
            let template = template.clone();
            let alac = alac.clone();
            let seg = segments[idx];
            async move {
                let mut data = fetch_range_retry(&state, &url, seg).await?;
                tokio::task::spawn_blocking(move || {
                    let tmpl: &Template = if idx == 1 {
                        fixed_template()
                    } else {
                        &template
                    };
                    decrypt_fragment_in_place(&mut data, tmpl)?;
                    if let Some(track) = alac.as_ref() {
                        repair_alac_fragment(&mut data, track)?;
                    }
                    Ok::<Vec<u8>, String>(data)
                })
                .await
                .map_err(|e| format!("解密任务失败: {e}"))?
            }
        })
        .buffered(SEGMENT_CONCURRENCY);

    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_mp4);
            return Err("已取消".into());
        }
        let data = item?;
        use std::io::Write;
        file.write_all(&data)
            .map_err(|e| format!("写入分片失败: {e}"))?;
    }
    drop(file);

    // 6. 歌词（可选）：TTML → LRC / 纯文本
    let want_lyrics = opts.lyrics != "none";
    let lyrics = if want_lyrics {
        match state.wrapper.fetch_lyrics(&spec.adam_id, None).await {
            Ok(crate::wrapper::Lyrics::Found(ttml)) => {
                Some((crate::dl::lyrics::ttml_to_lrc(&ttml, opts.syllable), ttml))
            }
            Ok(crate::wrapper::Lyrics::NotFound) => None,
            Err(e) => {
                tracing::debug!(adam_id = %spec.adam_id, error = %e, "歌词获取失败，跳过");
                None
            }
        }
    } else {
        None
    };

    // 7. defrag：fragmented → progressive（moov 在前），顺带写入标签与封面
    let tags_blob = if opts.write_tags {
        let cover_bytes = if opts.embed_cover && !spec.cover_url.is_empty() {
            fetch_cover(state, &spec.cover_url).await
        } else {
            None
        };
        let cover = cover_bytes
            .as_ref()
            .map(|(bytes, png)| Cover { bytes, png: *png });
        let embedded = lyrics
            .as_ref()
            .filter(|_| opts.lyrics == "embed" || opts.lyrics == "both")
            .map(|(_, ttml)| crate::dl::lyrics::ttml_to_plain(ttml));
        tags::build_ilst(spec, embedded.as_deref(), cover)
    } else {
        Vec::new()
    };

    let source = FileSource {
        file: std::fs::File::open(&tmp_mp4).map_err(|e| format!("打开临时文件失败: {e}"))?,
        size: std::fs::metadata(&tmp_mp4).map(|m| m.len()).unwrap_or(0),
    };
    let final_tmp = temp_dir.join(format!("{}.final.m4a", sanitize_id(&spec.adam_id)));
    let out_file =
        std::fs::File::create(&final_tmp).map_err(|e| format!("创建输出文件失败: {e}"))?;
    let mut sink = FileSink { file: out_file };
    let mut options = DefragOptions::song();
    if !tags_blob.is_empty() {
        options = options.with_tags(tags_blob);
    }
    defragment(source, &mut sink, &options).map_err(|e| format!("封装 MP4 失败: {e}"))?;
    drop(sink);

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }
    std::fs::rename(&final_tmp, out_path).map_err(|e| format!("移动到目标路径失败: {e}"))?;
    let _ = std::fs::remove_file(&tmp_mp4);
    let bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);

    // 8. 歌词文件（与音频同目录同名）
    if let Some((lrc, ttml)) = &lyrics {
        let stem = out_path.with_extension("");
        if opts.lyrics == "lrc" || opts.lyrics == "both" {
            let _ = std::fs::write(stem.with_extension("lrc"), lrc);
        }
        if opts.lyrics == "both" {
            let _ = std::fs::write(stem.with_extension("ttml"), ttml);
        }
    }

    Ok(Downloaded {
        path: out_path.to_path_buf(),
        bytes,
        quality,
    })
}

async fn fetch_cover(state: &AppState, url: &str) -> Option<(Vec<u8>, bool)> {
    let resp = client(state)
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?.to_vec();
    let png = bytes.starts_with(&[0x89, b'P', b'N', b'G']);
    Some((bytes, png))
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
