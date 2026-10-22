//! iTunes 风格标签（`moov.udta.meta.ilst`）构造 + 文件名模板渲染。
//!
//! 标签格式与上游 `src/ui/mp4-tags.mjs` 的 `buildIlst` 一致：一段连续的 box，
//! 交给 `am_media::defrag::Options::with_tags` 写进 progressive MP4 的 moov。

use crate::dl::model::TrackSpec;

const TYPE_BINARY: u32 = 0;
const TYPE_UTF8: u32 = 1;
const TYPE_JPEG: u32 = 13;
const TYPE_PNG: u32 = 14;
const TYPE_INT: u32 = 21;

fn fourcc(kind: &str) -> [u8; 4] {
    // 注意：`©` 在 iTunes 的 fourcc 里是单字节 0xA9，不能按 UTF-8 编码（否则会变成 5 字节）
    let mut out = [b' '; 4];
    for (i, ch) in kind.chars().take(4).enumerate() {
        out[i] = (ch as u32 & 0xff) as u8;
    }
    out
}

fn boxed(kind: &str, parts: &[&[u8]]) -> Vec<u8> {
    let size: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(&fourcc(kind));
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn u32_bytes(n: u32) -> [u8; 4] {
    n.to_be_bytes()
}

fn data_box(kind: u32, value: &[u8]) -> Vec<u8> {
    // 首字节是版本（0），后 3 字节是 flags —— 与上游 `u32(type)` 等价
    let t = u32_bytes(kind);
    boxed("data", &[&t, &[0, 0, 0, 0], value])
}

fn item(kind: &str, kind_type: u32, value: &[u8]) -> Vec<u8> {
    boxed(kind, &[&data_box(kind_type, value)])
}

fn text(kind: &str, value: &str) -> Vec<u8> {
    item(kind, TYPE_UTF8, value.as_bytes())
}

/// 自定义字段（----，mean 为 com.apple.iTunes）
fn freeform(name: &str, value: &str) -> Vec<u8> {
    let mean = boxed("mean", &[&u32_bytes(0), b"com.apple.iTunes"]);
    let name_box = boxed("name", &[&u32_bytes(0), name.as_bytes()]);
    let data = data_box(TYPE_UTF8, value.as_bytes());
    boxed("----", &[&mean, &name_box, &data])
}

fn int_item(kind: &str, value: i64, width: usize) -> Vec<u8> {
    let bytes: Vec<u8> = match width {
        1 => vec![(value as i8) as u8],
        2 => (value as i16).to_be_bytes().to_vec(),
        4 => (value as u32).to_be_bytes().to_vec(),
        _ => (value as u64).to_be_bytes().to_vec(),
    };
    item(kind, TYPE_INT, &bytes)
}

pub struct Cover<'a> {
    pub bytes: &'a [u8],
    pub png: bool,
}

/// 由曲目元数据构造 ilst；空字段自动跳过
pub fn build_ilst(spec: &TrackSpec, lyrics: Option<&str>, cover: Option<Cover<'_>>) -> Vec<u8> {
    build(spec, lyrics, cover, 1)
}

/// MV 的 ilst：与歌曲相同，但 `stik` 为 6（音乐视频）
pub fn build_mv_ilst(spec: &TrackSpec, cover: Option<Cover<'_>>) -> Vec<u8> {
    build(spec, None, cover, 6)
}

fn build(spec: &TrackSpec, lyrics: Option<&str>, cover: Option<Cover<'_>>, media_type: i64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut push = |b: Vec<u8>| out.extend_from_slice(&b);

    let has = |s: &str| !s.trim().is_empty();

    if has(&spec.title) {
        push(text("\u{a9}nam", &spec.title));
    }
    if has(&spec.artist) {
        push(text("\u{a9}ART", &spec.artist));
    }
    if has(&spec.album) {
        push(text("\u{a9}alb", &spec.album));
    }
    if has(&spec.album_artist) {
        push(text("aART", &spec.album_artist));
    }
    if has(&spec.composer) {
        push(text("\u{a9}wrt", &spec.composer));
    }
    if has(&spec.genre) {
        push(text("\u{a9}gen", &spec.genre));
    }
    if has(&spec.release_date) {
        push(text("\u{a9}day", &spec.release_date));
    }
    if spec.track_no > 0 {
        let mut b = [0u8; 8];
        b[2..4].copy_from_slice(&(spec.track_no.min(0xffff) as u16).to_be_bytes());
        b[4..6].copy_from_slice(&(spec.track_total.min(0xffff) as u16).to_be_bytes());
        push(item("trkn", TYPE_BINARY, &b));
    }
    if spec.disc_no > 0 {
        let mut b = [0u8; 6];
        b[2..4].copy_from_slice(&(spec.disc_no.min(0xffff) as u16).to_be_bytes());
        b[4..6].copy_from_slice(&(spec.disc_total.min(0xffff) as u16).to_be_bytes());
        push(item("disk", TYPE_BINARY, &b));
    }
    // stik：1 = 音乐，6 = 音乐视频
    push(int_item("stik", media_type, 1));
    if has(&spec.copyright) {
        push(text("cprt", &spec.copyright));
    }
    if has(&spec.label) {
        push(text("\u{a9}pub", &spec.label));
        push(freeform("LABEL", &spec.label));
    }
    if has(&spec.isrc) {
        push(freeform("ISRC", &spec.isrc));
    }
    if has(&spec.upc) {
        push(freeform("UPC", &spec.upc));
    }
    // cnID 是 32 位，plID 是 64 位（与 iTunes 相同）
    if let Ok(id) = spec.song_id.parse::<u64>() {
        if id <= 0xffff_ffff {
            push(int_item("cnID", id as i64, 4));
        }
    }
    if let Ok(id) = spec.album_id.parse::<u64>() {
        push(int_item("plID", id as i64, 8));
    }
    if let Ok(id) = spec.artist_id.parse::<u64>() {
        if id <= 0xffff_ffff {
            push(int_item("atID", id as i64, 4));
        }
    }
    if let Some(lyrics) = lyrics.filter(|l| !l.trim().is_empty()) {
        push(text("\u{a9}lyr", lyrics));
    }
    if let Some(cover) = cover.filter(|c| !c.bytes.is_empty()) {
        push(item("covr", if cover.png { TYPE_PNG } else { TYPE_JPEG }, cover.bytes));
    }
    out
}

/* ---------- 文件名模板 ---------- */

fn ctx_value(spec: &TrackSpec, playlist: &str, quality: &str, name: &str) -> Option<String> {
    let year = spec.release_date.get(0..4).unwrap_or("");
    let v = match name {
        "title" => spec.title.clone(),
        "artist" => spec.artist.clone(),
        "album" => spec.album.clone(),
        "album_artist" => spec.album_artist.clone(),
        "composer" => spec.composer.clone(),
        "genre" => spec.genre.clone(),
        "tracknum" => num(spec.track_no),
        "tracktotal" => num(spec.track_total),
        "disc" | "disk" => num(spec.disc_no),
        "disctotal" | "disktotal" => num(spec.disc_total),
        "year" => year.to_string(),
        "release_date" => spec.release_date.clone(),
        "quality" => quality.to_string(),
        "song_id" => spec.song_id.clone(),
        "album_id" => spec.album_id.clone(),
        "artist_id" => spec.artist_id.clone(),
        "isrc" => spec.isrc.clone(),
        "upc" => spec.upc.clone(),
        "record_label" => spec.label.clone(),
        "copyright" => spec.copyright.clone(),
        "playlist" => playlist.to_string(),
        "index" => String::new(),
        _ => return None,
    };
    Some(v)
}

fn num(n: u32) -> String {
    if n == 0 {
        String::new()
    } else {
        n.to_string()
    }
}

/// 渲染模板：支持 `{name}` 与 `{name:02d}`；未知变量渲染为空串。
/// 返回值已按 `/` 分段并逐段清洗，`..` 会被丢弃。
pub fn render_template(template: &str, spec: &TrackSpec, playlist: &str, quality: &str) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end_rel) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &rest[start + 1..start + end_rel];
        let (name, width) = match token.split_once(':') {
            Some((n, f)) => (n, f.trim_end_matches('d').parse::<usize>().unwrap_or(0)),
            None => (token, 0usize),
        };
        let value = ctx_value(spec, playlist, quality, name).unwrap_or_default();
        if width > 0 && !value.is_empty() {
            out.push_str(&format!("{value:0>width$}"));
        } else {
            out.push_str(&value);
        }
        rest = &rest[start + end_rel + 1..];
    }
    out.push_str(rest);
    sanitize_path(&out)
}

/// 清洗路径：去掉非法字符与空段，拒绝 `..`
pub fn sanitize_path(raw: &str) -> String {
    let cleaned = raw
        .split('/')
        .filter_map(|seg| {
            let seg: String = seg
                .chars()
                .map(|c| match c {
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                    c if (c as u32) < 0x20 => '_',
                    c => c,
                })
                .collect();
            let seg = seg.trim().trim_matches('.').trim().to_string();
            if seg.is_empty() || seg == ".." {
                None
            } else {
                Some(seg)
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TrackSpec {
        TrackSpec {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_artist: "AA".into(),
            track_no: 3,
            release_date: "2021-05-01".into(),
            ..Default::default()
        }
    }

    #[test]
    fn fourcc_is_latin1() {
        assert_eq!(fourcc("\u{a9}nam"), [0xA9, b'n', b'a', b'm']);
        assert_eq!(fourcc("aART"), [b'a', b'A', b'R', b'T']);
    }

    #[test]
    fn template_pads() {
        let s = spec();
        assert_eq!(render_template("{tracknum:02d} {title}", &s, "", "ALAC"), "03 Song");
        assert_eq!(render_template("{album_artist}/{album} ({year})", &s, "", "ALAC"), "AA/Album (2021)");
    }

    #[test]
    fn template_strips_traversal() {
        let mut s = spec();
        s.title = "../../etc/passwd".into();
        let out = render_template("{title}", &s, "", "ALAC");
        assert!(!out.contains(".."), "{out}");
    }
}
