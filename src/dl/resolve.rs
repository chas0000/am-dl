//! 把 Apple Music 链接解析成曲目列表（专辑 / 歌单 / 单曲），走服务端已有的 amp-api 代理。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::config::AppState;
use crate::dl::model::TrackSpec;

static STOREFRONT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"music\.apple\.com/([a-z]{2})/").unwrap());

#[derive(Debug, Clone)]
pub struct Resolved {
    pub kind: String,
    pub id: String,
    pub storefront: String,
    pub title: String,
    pub artist: String,
    pub tracks: Vec<TrackSpec>,
}

pub fn storefront_of(url: &str) -> String {
    STOREFRONT_RE
        .captures(url)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "us".into())
}

fn str_at<'a>(v: &'a Value, path: &str) -> &'a str {
    v.pointer(path).and_then(Value::as_str).unwrap_or("")
}

fn u32_at(v: &Value, path: &str) -> u32 {
    v.pointer(path)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(0)
}

fn artwork(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    url.replace("{w}", "1200")
        .replace("{h}", "1200")
        .replace("{c}", "bb")
        .replace("{f}", "jpg")
}

/// 把 `relationships.tracks.data[]` 里的一条转成 TrackSpec
fn track_from(value: &Value, album: &AlbumCtx) -> Option<TrackSpec> {
    let id = value.get("id").and_then(Value::as_str)?.to_string();
    let a = value.get("attributes").cloned().unwrap_or(Value::Null);
    let genre = a
        .get("genreNames")
        .and_then(Value::as_array)
        .and_then(|g| g.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut spec = TrackSpec {
        adam_id: id.clone(),
        song_id: id,
        title: str_at(&a, "/name").to_string(),
        artist: str_at(&a, "/artistName").to_string(),
        composer: str_at(&a, "/composerName").to_string(),
        isrc: str_at(&a, "/isrc").to_string(),
        genre,
        track_no: u32_at(&a, "/trackNumber"),
        disc_no: u32_at(&a, "/discNumber"),
        // 专辑内曲目统一用专辑的发行日期，避免同名专辑因年份不同被拆成两个目录
        release_date: if album.release.is_empty() {
            str_at(&a, "/releaseDate").to_string()
        } else {
            album.release.clone()
        },
        album: album.name.clone(),
        album_artist: album.artist.clone(),
        album_id: album.id.clone(),
        copyright: album.copyright.clone(),
        upc: album.upc.clone(),
        label: album.label.clone(),
        cover_url: album.cover.clone(),
        track_total: album.track_total,
        disc_total: album.disc_total,
        artist_id: str_at(&a, "/artistId").to_string(),
        ..Default::default()
    };
    // 歌单里的曲目自带专辑信息时优先用它
    if let Some(name) = a.get("albumName").and_then(Value::as_str) {
        if !name.is_empty() {
            spec.album = name.to_string();
        }
    }
    if spec.title.is_empty() {
        return None;
    }
    Some(spec)
}

#[derive(Default, Clone)]
struct AlbumCtx {
    id: String,
    name: String,
    artist: String,
    copyright: String,
    upc: String,
    label: String,
    cover: String,
    release: String,
    track_total: u32,
    disc_total: u32,
}

fn album_ctx(v: &Value) -> AlbumCtx {
    let a = v.get("attributes").cloned().unwrap_or(Value::Null);
    let disc_total = v
        .pointer("/relationships/tracks/data")
        .and_then(Value::as_array)
        .map(|tracks| {
            tracks
                .iter()
                .map(|t| u32_at(t, "/attributes/discNumber"))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    AlbumCtx {
        id: v
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        name: str_at(&a, "/name").to_string(),
        artist: str_at(&a, "/artistName").to_string(),
        copyright: str_at(&a, "/copyright").to_string(),
        upc: str_at(&a, "/upc").to_string(),
        label: str_at(&a, "/recordLabel").to_string(),
        cover: artwork(str_at(&a, "/artwork/url")),
        release: str_at(&a, "/releaseDate").to_string(),
        track_total: u32_at(&a, "/trackCount"),
        disc_total,
    }
}

/// 拉取分页：`next` 是形如 `/v1/catalog/us/playlists/pl.x/tracks?offset=100` 的相对地址
async fn collect_tracks(
    state: &Arc<AppState>,
    first: &Value,
    ctx: &AlbumCtx,
) -> Result<Vec<TrackSpec>, String> {
    let mut out = Vec::new();
    let push_all = |v: &Value, out: &mut Vec<TrackSpec>| {
        if let Some(list) = v
            .pointer("/relationships/tracks/data")
            .and_then(Value::as_array)
        {
            for t in list {
                if let Some(spec) = track_from(t, ctx) {
                    out.push(spec);
                }
            }
        }
    };
    push_all(first, &mut out);

    let mut next = first
        .pointer("/relationships/tracks/next")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut pages = 0;
    while let Some(path) = next.take() {
        if pages >= 40 {
            break;
        }
        pages += 1;
        let (p, q) = path.split_once('?').unwrap_or((path.as_str(), ""));
        let p = p.trim_start_matches("/v1/");
        let value = state.amp.catalog_json(p, q).await?;
        push_all(&value, &mut out);
        next = value
            .pointer("/relationships/tracks/next")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    Ok(out)
}

/// 解析链接；返回曲目列表与展示用信息
pub async fn resolve(state: &Arc<AppState>, url: &str) -> Result<Resolved, String> {
    let url = url.trim();
    let storefront = storefront_of(url);

    if let Ok(id) = crate::links::parse_album_link(url) {
        let value = state
            .amp
            .catalog_json(
                &format!("catalog/{storefront}/albums/{id}"),
                "include=tracks&limit[tracks]=300",
            )
            .await?;
        let item = value
            .pointer("/data/0")
            .ok_or_else(|| "amp-api 没有返回专辑数据".to_string())?;
        let ctx = album_ctx(item);
        let tracks = collect_tracks(state, item, &ctx).await?;
        if tracks.is_empty() {
            return Err("这张专辑没有可下载的曲目".into());
        }
        return Ok(Resolved {
            kind: "album".into(),
            id,
            storefront,
            title: ctx.name.clone(),
            artist: ctx.artist.clone(),
            tracks,
        });
    }

    if let Ok(id) = crate::links::parse_playlist_link(url) {
        let value = state
            .amp
            .catalog_json(
                &format!("catalog/{storefront}/playlists/{id}"),
                "include=tracks&limit[tracks]=300",
            )
            .await?;
        let item = value
            .pointer("/data/0")
            .ok_or_else(|| "amp-api 没有返回歌单数据".to_string())?;
        let a = item.get("attributes").cloned().unwrap_or(Value::Null);
        let ctx = AlbumCtx {
            id: String::new(),
            name: str_at(&a, "/name").to_string(),
            artist: str_at(&a, "/curatorName").to_string(),
            cover: artwork(str_at(&a, "/artwork/url")),
            ..Default::default()
        };
        let mut tracks = collect_tracks(state, item, &ctx).await?;
        for (idx, t) in tracks.iter_mut().enumerate() {
            t.playlist = ctx.name.clone();
            t.track_no = idx as u32 + 1;
            if t.album.is_empty() {
                t.album = ctx.name.clone();
            }
            if t.album_artist.is_empty() {
                t.album_artist = ctx.artist.clone();
            }
            if t.cover_url.is_empty() {
                t.cover_url = ctx.cover.clone();
            }
        }
        if tracks.is_empty() {
            return Err("这个歌单没有可下载的曲目".into());
        }
        return Ok(Resolved {
            kind: "playlist".into(),
            id,
            storefront,
            title: ctx.name.clone(),
            artist: ctx.artist.clone(),
            tracks,
        });
    }

    if let Ok(id) = crate::links::parse_artist_link(url) {
        return resolve_artist(state, &storefront, &id).await;
    }

    if let Ok(id) = crate::links::parse_mv_link(url) {
        let value = state
            .amp
            .catalog_json(
                &format!("catalog/{storefront}/music-videos/{id}"),
                "include=albums,artists",
            )
            .await?;
        let item = value
            .pointer("/data/0")
            .ok_or_else(|| "amp-api 没有返回 MV 数据".to_string())?;
        let a = item.get("attributes").cloned().unwrap_or(Value::Null);
        let album = item
            .pointer("/relationships/albums/data/0")
            .cloned()
            .unwrap_or(Value::Null);
        let ctx = album_ctx(&album);
        let spec = TrackSpec {
            adam_id: id.clone(),
            song_id: id.clone(),
            title: str_at(&a, "/name").to_string(),
            artist: str_at(&a, "/artistName").to_string(),
            album: if ctx.name.is_empty() {
                str_at(&a, "/albumName").to_string()
            } else {
                ctx.name.clone()
            },
            album_artist: if ctx.artist.is_empty() {
                str_at(&a, "/artistName").to_string()
            } else {
                ctx.artist.clone()
            },
            genre: a
                .get("genreNames")
                .and_then(Value::as_array)
                .and_then(|g| g.first())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            release_date: str_at(&a, "/releaseDate").to_string(),
            cover_url: {
                let art = artwork(str_at(&a, "/artwork/url"));
                if art.is_empty() {
                    ctx.cover.clone()
                } else {
                    art
                }
            },
            album_id: ctx.id.clone(),
            is_mv: true,
            ..Default::default()
        };
        if spec.title.is_empty() {
            return Err("无法解析 MV 信息".into());
        }
        let title = spec.title.clone();
        let artist = spec.artist.clone();
        return Ok(Resolved {
            kind: "mv".into(),
            id,
            storefront,
            title,
            artist,
            tracks: vec![spec],
        });
    }

    if let Ok(id) = crate::links::parse_song_link(url) {
        let value = state
            .amp
            .catalog_json(
                &format!("catalog/{storefront}/songs/{id}"),
                "include=albums,artists",
            )
            .await?;
        let item = value
            .pointer("/data/0")
            .ok_or_else(|| "amp-api 没有返回歌曲数据".to_string())?;
        let album = item
            .pointer("/relationships/albums/data/0")
            .cloned()
            .unwrap_or(Value::Null);
        let ctx = album_ctx(&album);
        let mut spec = track_from(item, &ctx).ok_or_else(|| "无法解析歌曲信息".to_string())?;
        if spec.album.is_empty() {
            spec.album = str_at(item, "/attributes/albumName").to_string();
        }
        let title = spec.title.clone();
        let artist = spec.artist.clone();
        return Ok(Resolved {
            kind: "song".into(),
            id,
            storefront,
            title,
            artist,
            tracks: vec![spec],
        });
    }

    Err("只支持 Apple Music 的专辑 / 歌单 / 歌曲链接".into())
}

/// 歌手页：把这个歌手名下的全部专辑（含单曲 / EP）展开成一张曲目表。
/// 一张专辑一次请求，最多 100 张，避免误点一个大牌歌手时把服务器打爆。
async fn resolve_artist(
    state: &Arc<AppState>,
    storefront: &str,
    artist_id: &str,
) -> Result<Resolved, String> {
    const MAX_ALBUMS: usize = 100;

    let artist = state
        .amp
        .catalog_json(&format!("catalog/{storefront}/artists/{artist_id}"), "")
        .await?;
    let name = artist
        .pointer("/data/0/attributes/name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return Err("amp-api 没有返回歌手信息".into());
    }

    // 1) 该歌手的专辑列表（分页）
    let mut album_ids: Vec<String> = Vec::new();
    let first = state
        .amp
        .catalog_json(
            &format!("catalog/{storefront}/artists/{artist_id}/albums"),
            "limit=100",
        )
        .await?;
    let push_albums = |v: &Value, out: &mut Vec<String>| {
        if let Some(list) = v.pointer("/data").and_then(Value::as_array) {
            for a in list {
                if let Some(id) = a.get("id").and_then(Value::as_str) {
                    out.push(id.to_string());
                }
            }
        }
    };
    push_albums(&first, &mut album_ids);
    let mut next = first
        .pointer("/next")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut pages = 0;
    while let Some(path) = next.take() {
        if pages >= 5 || album_ids.len() >= MAX_ALBUMS {
            break;
        }
        pages += 1;
        let (p, q) = path.split_once('?').unwrap_or((path.as_str(), ""));
        let p = p.trim_start_matches("/v1/");
        let value = state.amp.catalog_json(p, q).await?;
        push_albums(&value, &mut album_ids);
        next = value
            .pointer("/next")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    album_ids.truncate(MAX_ALBUMS);
    if album_ids.is_empty() {
        return Err("这个歌手没有可下载的专辑".into());
    }

    // 2) 逐张专辑取曲目（每批 4 张并发，大牌歌手的几十张专辑才不至于等太久）
    let mut tracks: Vec<TrackSpec> = Vec::new();
    let mut failed = 0usize;
    for chunk in album_ids.chunks(4) {
        let mut futs = Vec::with_capacity(chunk.len());
        for album_id in chunk {
            let st = Arc::clone(state);
            let sf = storefront.to_string();
            let id = album_id.clone();
            futs.push(async move {
                st.amp
                    .catalog_json(
                        &format!("catalog/{sf}/albums/{id}"),
                        "include=tracks&limit[tracks]=300",
                    )
                    .await
            });
        }
        for result in futures::future::join_all(futs).await {
            match result {
                Ok(value) => {
                    if let Some(item) = value.pointer("/data/0") {
                        let ctx = album_ctx(item);
                        if let Ok(mut list) = collect_tracks(state, item, &ctx).await {
                            tracks.append(&mut list);
                        }
                    }
                }
                Err(_) => failed += 1,
            }
        }
    }

    if tracks.is_empty() {
        return Err(format!("没有取到任何曲目（{} 张专辑失败）", failed));
    }

    // 歌手任务统一用「当前下载的歌手」当上层目录：
    // 合作专辑（如「陈淑桦 & 罗大佑」）不再单独拉出一个顶层歌手文件夹
    for t in tracks.iter_mut() {
        t.album_artist = name.clone();
    }

    Ok(Resolved {
        kind: "artist".into(),
        id: artist_id.to_string(),
        storefront: storefront.to_string(),
        title: name.clone(),
        artist: name,
        tracks,
    })
}

/// 有没有汉字（用来判断国区条目是不是真给了中文名）
fn has_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'))
}

/// 用国区（cn）的元数据覆盖曲目名称 / 专辑 / 歌手，并把歌词 id 换成国区的。
///
/// 只按 ISRC 匹配，且只接受带汉字的条目——国区没有这首歌时保持原样。
/// 返回被覆盖的专辑名（全部一致时），便于同步任务标题。
pub async fn enrich_cn(
    state: &Arc<AppState>,
    tracks: &mut [TrackSpec],
    storefront: &str,
) -> Option<String> {
    if storefront == "cn" {
        return None;
    }

    // isrc → 曲目下标（同一首歌可能在专辑里出现多次）
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        if !t.isrc.is_empty() && !t.is_mv {
            index.entry(t.isrc.clone()).or_default().push(i);
        }
    }
    if index.is_empty() {
        return None;
    }

    let mut isrcs: Vec<String> = index.keys().cloned().collect();
    isrcs.sort();
    let mut matched = 0usize;
    let mut albums: Vec<String> = Vec::new();

    for chunk in isrcs.chunks(25) {
        let query = format!("filter[isrc]={}&limit=25", chunk.join(","));
        let value = match state.amp.catalog_json("catalog/cn/songs", &query).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "国区元数据查询失败，保留原样");
                continue;
            }
        };
        let Some(items) = value.get("data").and_then(|d| d.as_array()) else {
            continue;
        };
        for item in items {
            let a = &item["attributes"];
            let isrc = a.get("isrc").and_then(|v| v.as_str()).unwrap_or("");
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let album = a.get("albumName").and_then(|v| v.as_str()).unwrap_or("");
            let artist = a.get("artistName").and_then(|v| v.as_str()).unwrap_or("");
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() || (!has_cjk(name) && !has_cjk(album) && !has_cjk(artist)) {
                continue;
            }
            let Some(list) = index.get(isrc) else {
                continue;
            };
            for &i in list {
                let t = &mut tracks[i];
                t.title = name.to_string();
                if !album.is_empty() {
                    t.album = album.to_string();
                }
                if !artist.is_empty() {
                    t.artist = artist.to_string();
                    t.album_artist = artist.to_string();
                }
                if !id.is_empty() {
                    t.lyrics_id = id.to_string();
                }
                albums.push(t.album.clone());
                matched += 1;
            }
        }
    }

    if matched == 0 {
        tracing::debug!("国区没有匹配到中文元数据");
        return None;
    }
    tracing::info!(matched, "已用国区中文元数据覆盖曲目");
    let first = albums.first().cloned();
    match first {
        Some(a) if albums.iter().all(|x| *x == a) => Some(a),
        _ => None,
    }
}
