//! master / media m3u8 解析（移植自上游 v0.2.6 的 `src/m3u8.rs`，去掉播放器相关部分）。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use am_mp4::FIXED_KEY_URI;

use crate::dl::model::Quality;

static ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([A-Z0-9-]+)=("[^"]*"|[^,\r\n]+)"#).unwrap());
static ADAM_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_A(\d+)_").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub length: u64,
}

impl Segment {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// 一首歌的解密所需信息：key uri（skd://…）+ 分段字节范围
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Track {
    pub adam_id: String,
    pub key_uri: String,
    pub file_uri: String,
    pub segments: Vec<Segment>,
}

impl Track {
    #[allow(dead_code)]
    pub fn total_size(&self) -> u64 {
        self.segments.last().map_or(0, Segment::end)
    }
}

/// master m3u8 的一个变体
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Variant {
    pub uri: String,
    pub group_id: String,
    pub audio_name: String,
    pub codecs: String,
    pub channels: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub bandwidth: Option<u64>,
}

impl Variant {
    /// 这个变体属于哪种音质
    pub fn quality(&self) -> Quality {
        let codecs = self.codecs.to_ascii_lowercase();
        let name = self.audio_name.to_ascii_lowercase();
        if codecs.contains("alac") {
            return Quality::Alac;
        }
        if codecs.contains("ec-3") || codecs.contains("eac3") || name.contains("atmos") {
            return Quality::Atmos;
        }
        if codecs.contains("mp4a.40.5") || name.contains("he-aac") {
            return Quality::Heaac;
        }
        if name.contains("binaural") {
            return Quality::AacBinaural;
        }
        if name.contains("downmix") {
            return Quality::AacDownmix;
        }
        Quality::Aac
    }

    /// 展示用音质名，如 `ALAC 24bit/96kHz`
    /// 目录/文件名用的短标签：`ALAC 24bit-96kHz`、`AAC`、`Dolby Atmos`。
    /// 不含 `/`，可以直接进路径。
    pub fn short_label(&self) -> String {
        let q = self.quality();
        let mut out = match q {
            Quality::Alac => "ALAC".to_string(),
            Quality::Atmos => "Dolby Atmos".to_string(),
            Quality::Aac => "AAC".to_string(),
            Quality::AacBinaural => "AAC Binaural".to_string(),
            Quality::AacDownmix => "AAC Downmix".to_string(),
            Quality::Heaac => "HE-AAC".to_string(),
            Quality::Best => "Best".to_string(),
        };
        // 只有 ALAC 值得在目录名里标出位深/采样率（同一张专辑常有 24/48 与 24/96 两种）
        if q == Quality::Alac {
            if let Some(depth) = self.bit_depth {
                out.push_str(&format!(" {depth}bit"));
            }
            if let Some(rate) = self.sample_rate {
                out.push_str(&format!("-{}kHz", rate as f64 / 1000.0));
            }
        }
        out
    }

    pub fn label(&self) -> String {
        let q = match self.quality() {
            Quality::Alac => "ALAC",
            Quality::Atmos => "Dolby Atmos",
            Quality::Aac => "AAC",
            Quality::AacBinaural => "AAC Binaural",
            Quality::AacDownmix => "AAC Downmix",
            Quality::Heaac => "HE-AAC",
            Quality::Best => "Best",
        };
        let mut out = q.to_string();
        if let Some(depth) = self.bit_depth {
            out.push_str(&format!(" {depth}bit"));
        }
        if let Some(rate) = self.sample_rate {
            out.push_str(&format!("/{}kHz", rate as f64 / 1000.0));
        }
        if let Some(ch) = &self.channels {
            out.push_str(&format!(" {ch}ch"));
        }
        out
    }
}

/// 解析 HLS 属性列表（`A="x",B=y`）
pub fn attributes(input: &str) -> HashMap<&str, &str> {
    ATTR_RE
        .captures_iter(input)
        .map(|caps| {
            let (k, v) = (caps.get(1).unwrap().as_str(), caps.get(2).unwrap().as_str());
            (k, v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v))
        })
        .collect()
}

/// 解析 master m3u8 的全部变体
pub fn parse_master_variants(content: &str) -> Result<Vec<Variant>, String> {
    #[derive(Default, Clone)]
    struct Group {
        name: String,
        channels: Option<String>,
        sample_rate: Option<u32>,
        bit_depth: Option<u32>,
    }

    let mut groups = HashMap::<String, Group>::new();
    let mut pending: Option<HashMap<&str, &str>> = None;
    let mut variants = Vec::new();

    for line in content.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(attrs);
            if attrs.get("TYPE") == Some(&"AUDIO") {
                if let Some(group_id) = attrs.get("GROUP-ID") {
                    groups.insert(
                        group_id.to_string(),
                        Group {
                            name: attrs.get("NAME").unwrap_or(&"").to_string(),
                            channels: attrs.get("CHANNELS").map(|s| s.to_string()),
                            sample_rate: attrs.get("SAMPLE-RATE").and_then(|s| s.parse().ok()),
                            bit_depth: attrs.get("BIT-DEPTH").and_then(|s| s.parse().ok()),
                        },
                    );
                }
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(attrs));
        } else if !line.is_empty() && !line.starts_with('#') && line.ends_with(".m3u8") {
            let attrs = pending.take().unwrap_or_default();
            let group_id = attrs.get("AUDIO").unwrap_or(&"").to_string();
            let group = groups.get(&group_id).cloned().unwrap_or_default();
            variants.push(Variant {
                uri: line.to_string(),
                group_id,
                audio_name: group.name,
                codecs: attrs.get("CODECS").unwrap_or(&"").to_string(),
                channels: group.channels,
                sample_rate: group.sample_rate,
                bit_depth: group.bit_depth,
                bandwidth: attrs
                    .get("AVERAGE-BANDWIDTH")
                    .or_else(|| attrs.get("BANDWIDTH"))
                    .and_then(|s| s.parse().ok()),
            });
        }
    }

    if variants.is_empty() {
        return Err("master m3u8 里没有找到任何变体".into());
    }
    Ok(variants)
}

/// 按偏好挑一个变体：`quality` 精确匹配，`Best` 按 ALAC → Atmos → AAC → HE-AAC 顺序；
/// `max_sample_rate` 限制 ALAC 采样率（0 表示不限），超限时退到不超过它的最高档。
pub fn pick_variant(variants: &[Variant], quality: Quality, max_sample_rate: u32) -> Result<Variant, String> {
    let mut candidates: Vec<&Variant> = match quality {
        Quality::Best => variants.iter().collect(),
        other => variants.iter().filter(|v| v.quality() == other).collect(),
    };
    if candidates.is_empty() && quality != Quality::Best {
        // 该音质不存在时退到最高可用档（与上游「没有首选音质时降级」一致）
        candidates = variants.iter().collect();
    }
    if candidates.is_empty() {
        return Err("没有可用的音质变体".into());
    }

    let rank = |q: Quality| match q {
        Quality::Alac => 0,
        Quality::Atmos => 1,
        Quality::Aac => 2,
        Quality::AacBinaural => 3,
        Quality::AacDownmix => 4,
        Quality::Heaac => 5,
        Quality::Best => 9,
    };

    if max_sample_rate > 0 {
        let capped: Vec<&Variant> = candidates
            .iter()
            .copied()
            .filter(|v| v.sample_rate.map_or(true, |r| r <= max_sample_rate))
            .collect();
        if !capped.is_empty() {
            candidates = capped;
        }
    }

    candidates.sort_by(|a, b| {
        rank(a.quality())
            .cmp(&rank(b.quality()))
            .then(b.sample_rate.unwrap_or(0).cmp(&a.sample_rate.unwrap_or(0)))
            .then(b.bit_depth.unwrap_or(0).cmp(&a.bit_depth.unwrap_or(0)))
            .then(b.bandwidth.unwrap_or(0).cmp(&a.bandwidth.unwrap_or(0)))
    });
    Ok(candidates[0].clone())
}

/// 解析 `LEN[@OFF]`，缺省 OFF 时接在 `next` 之后
fn parse_byterange(value: &str, next: u64) -> Result<Segment, String> {
    let value = value.trim().trim_matches('"');
    let (len, off) = match value.split_once('@') {
        Some((l, o)) => (l, Some(o)),
        None => (value, None),
    };
    let length = len.parse().map_err(|e| format!("BYTERANGE 长度非法 '{value}': {e}"))?;
    let offset = match off {
        Some(o) => o.parse().map_err(|e| format!("BYTERANGE 偏移非法 '{value}': {e}"))?,
        None => next,
    };
    Ok(Segment { offset, length })
}

/// 解析 media m3u8：取出 adamId / key uri / 文件 URI 与全部分段范围。
/// 第 0 段是 init（EXT-X-MAP），第 1 段起是媒体分片。
pub fn parse_media_m3u8(source_url: &str, content: &str) -> Result<Track, String> {
    let path = source_url.split('?').next().unwrap_or(source_url);
    let filename = path.rsplit('/').next().unwrap_or(path);
    let adam_id = ADAM_ID_RE
        .captures(filename)
        .map(|c| c[1].to_string())
        .ok_or_else(|| format!("无法从地址中解析出 _A<数字>_: {source_url}"))?;

    let mut key_uri: Option<String> = None;
    let mut file_uri: Option<String> = None;
    let mut segments: Vec<Segment> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("#EXT-X-KEY:") || trimmed.starts_with("#EXT-X-SESSION-KEY:") {
            let (_, attrs) = trimmed.split_once(':').unwrap();
            let attrs = attributes(attrs);
            if attrs.get("METHOD") == Some(&"SAMPLE-AES") {
                if let Some(u) = attrs.get("URI").filter(|u| **u != FIXED_KEY_URI) {
                    key_uri.get_or_insert_with(|| u.to_string());
                }
            }
            continue;
        }

        if let Some(attrs) = trimmed.strip_prefix("#EXT-X-MAP:") {
            if !segments.is_empty() {
                return Err("不支持多个或位置靠后的 EXT-X-MAP".into());
            }
            let attrs = attributes(attrs);
            file_uri = attrs.get("URI").map(|s| s.to_string());
            let range = attrs.get("BYTERANGE").ok_or("EXT-X-MAP 没有 BYTERANGE")?;
            segments.push(parse_byterange(range, 0)?);
        } else if let Some(value) = trimmed.strip_prefix("#EXT-X-BYTERANGE:") {
            if segments.is_empty() {
                return Err("EXT-X-BYTERANGE 出现在 EXT-X-MAP 之前".into());
            }
            let next = segments.last().map_or(0, Segment::end);
            segments.push(parse_byterange(value, next)?);
        }
    }

    let key_uri = key_uri.ok_or("m3u8 里没有找到 SAMPLE-AES 的 key URI")?;
    let file_uri = file_uri.ok_or("m3u8 里没有找到 EXT-X-MAP URI")?;
    if segments.len() < 2 {
        return Err("m3u8 里没有媒体分片".into());
    }

    let mut expected = 0;
    for s in &segments {
        if s.offset != expected || s.length == 0 {
            return Err(format!("分片在偏移 {} 处不连续（期望 {expected}）", s.offset));
        }
        expected = s.end();
    }

    Ok(Track { adam_id, key_uri, file_uri, segments })
}

/// 相对地址转绝对地址
pub fn absolute(base: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_string();
    }
    // 根相对地址：拼到 scheme://host 上
    if let Some(rest) = uri.strip_prefix('/') {
        if let Some(scheme_end) = base.find("://") {
            let after = scheme_end + 3;
            let host_end = base[after..].find('/').map_or(base.len(), |i| after + i);
            return format!("{}/{}", &base[..host_end], rest);
        }
        return uri.to_string();
    }
    match base.rfind('/') {
        Some(idx) => format!("{}/{}", &base[..idx], uri),
        None => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byterange_continues() {
        let s = parse_byterange("100@0", 0).unwrap();
        assert_eq!((s.offset, s.length), (0, 100));
        let s = parse_byterange("50", 100).unwrap();
        assert_eq!((s.offset, s.length), (100, 50));
    }

    #[test]
    fn absolute_join() {
        assert_eq!(absolute("https://a/b/c.m3u8", "d.m3u8"), "https://a/b/d.m3u8");
        assert_eq!(absolute("https://a/b/c.m3u8", "/d.m3u8"), "https://a/d.m3u8");
    }
}
