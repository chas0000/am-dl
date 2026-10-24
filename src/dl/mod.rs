//! 服务端下载模块：任务队列 + HTTP API + 静态页面。
//!
//! 由 `scripts/patch.py` 在 `src/lib.rs` 里注册（`pub mod server_dl;` + `.merge(server_dl::router())`），
//! 不修改上游任何既有函数，上游更新时补丁冲突面最小。
//!
//! 配置（环境变量，均有默认值）：
//! - `AM_DL_ROOT`      下载根目录，默认 `/root/music`
//! - `AM_DL_TOKEN`     访问令牌；设置后 `/dl` 与 `/api/dl/*` 需要 `?token=` 或 `Authorization: Bearer`
//! - `AM_DL_CONCURRENCY` 同时下载的曲目数，默认 2

pub mod download;
pub mod limit;
mod lyrics;
pub mod m3u8;
pub mod model;
pub mod mv;
pub mod resolve;
pub mod tags;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, RwLock};

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::config::AppState;
use crate::dl::model::{Handle, Item, Job, JobOptions, Quality, Status, TrackSpec};

static DL: OnceLock<Arc<DlState>> = OnceLock::new();

pub struct DlState {
    /// 允许写入的根目录（第一个是默认值）
    pub roots: Vec<PathBuf>,
    pub root: PathBuf,
    pub temp: PathBuf,
    pub jobs_dir: PathBuf,
    pub token: Option<String>,
    /// 初始并发（来自 env / 部署脚本）；运行时以 limit 为准
    pub concurrency: usize,
    pub limit: Arc<limit::Limit>,
    pub handles: RwLock<HashMap<String, Arc<Handle>>>,
}

impl DlState {
    fn new() -> Self {
        // AM_DL_ROOTS 用 ':' 分隔多个根目录；未设置时回退到 AM_DL_ROOT
        let raw = std::env::var("AM_DL_ROOTS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("AM_DL_ROOT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "/root/music".into());
        let mut roots: Vec<PathBuf> = raw
            .split(':')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .collect();
        if roots.is_empty() {
            roots.push(PathBuf::from("/root/music"));
        }
        let root = roots[0].clone();
        let temp = root.join(".tmp");
        let jobs_dir = root.join(".jobs");
        let _ = std::fs::create_dir_all(&temp);
        let _ = std::fs::create_dir_all(&jobs_dir);
        let token = std::env::var("AM_DL_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut concurrency = std::env::var("AM_DL_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(2);
        // 页面上改过的并发存在 <root>/.settings.json，优先于环境变量
        let settings_file = root.join(".settings.json");
        if let Ok(text) = std::fs::read_to_string(&settings_file) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(n) = v.get("concurrency").and_then(|x| x.as_u64()) {
                    if n > 0 {
                        concurrency = n as usize;
                    }
                }
            }
        }
        Self {
            roots,
            root,
            temp,
            jobs_dir,
            token,
            concurrency,
            limit: Arc::new(limit::Limit::new(concurrency)),
            handles: RwLock::new(HashMap::new()),
        }
    }

    /// 把任务的 `dest` 解析成实际目录：
    /// - 空 → 默认根目录
    /// - 绝对路径 → 必须落在某个允许的根目录内（防路径穿越 / 写到 /etc）
    /// - 相对路径 → 拼到默认根目录
    pub fn resolve_base(&self, dest: &str) -> Result<PathBuf, String> {
        let dest = dest.trim();
        if dest.is_empty() {
            return Ok(self.root.clone());
        }
        if dest.starts_with('/') {
            let path = PathBuf::from(dest);
            let normalized = normalize(&path);
            if self
                .roots
                .iter()
                .any(|r| normalized.starts_with(normalize(r)))
            {
                return Ok(normalized);
            }
            return Err(format!(
                "保存目录 {dest} 不在允许的根目录内（{}）",
                self.roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let rel = normalize(&self.root.join(dest));
        if !rel.starts_with(normalize(&self.root)) {
            return Err(format!("保存目录 {dest} 越界"));
        }
        Ok(rel)
    }

    fn persist(&self, job: &Job) {
        let path = self.jobs_dir.join(format!("{}.json", job.id));
        if let Ok(text) = serde_json::to_string(job) {
            let _ = std::fs::write(path, text);
        }
    }

    fn insert(&self, handle: Arc<Handle>) {
        let id = handle.with(|j| j.id.clone());
        self.handles.write().unwrap().insert(id, handle);
    }

    fn get_handle(&self, id: &str) -> Option<Arc<Handle>> {
        self.handles.read().unwrap().get(id).cloned()
    }

    /// 启动时载入历史任务；上次运行中被中断的标为失败，可一键重试
    fn load_history(&self) {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut job) = serde_json::from_str::<Job>(&text) else {
                continue;
            };
            for item in job.items.iter_mut() {
                if matches!(item.status, Status::Running | Status::Queued) {
                    item.status = Status::Failed;
                    item.error = Some("服务重启，任务中断".into());
                }
            }
            job.refresh();
            self.insert(Handle::new(job));
        }
    }
}

/// 由 `router()` 调用；重复调用无副作用
pub fn init(_state: &Arc<AppState>) {
    DL.get_or_init(|| {
        let dl = Arc::new(DlState::new());
        dl.load_history();
        tracing::info!(root = %dl.root.display(), concurrency = dl.concurrency, "服务端下载已启用");
        dl
    });
}

fn dl() -> &'static Arc<DlState> {
    DL.get().expect("server_dl::init 未调用")
}

/* ---------- 路由 ---------- */

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dl", get(page))
        .route("/dl/", get(page))
        .route(
            "/api/dl/ping",
            get(|| async { format!("am-dl {}", crate::VERSION) }),
        )
        .route("/api/dl/config", get(config_handler))
        .route(
            "/api/dl/settings",
            get(settings_handler).post(settings_update),
        )
        .route("/api/dl/preview", post(preview_handler))
        .route("/api/dl/jobs", get(list_handler).post(create_handler))
        .route("/api/dl/jobs/:id", get(get_handler))
        .route("/api/dl/jobs/:id/cancel", post(cancel_handler))
        .route("/api/dl/jobs/:id/retry", post(retry_handler))
        .route("/api/dl/jobs/:id/remove", post(remove_handler))
        .route("/api/dl/jobs/batch", post(batch_handler))
        .route(
            "/api/dl/assets/dl.mjs",
            get(|| async { asset(JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/api/dl/assets/dl.css",
            get(|| async { asset(CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/api/dl/assets/inject.mjs",
            get(|| async { asset(INJECT_JS, "text/javascript; charset=utf-8") }),
        )
}

static HTML: &str = include_str!("../ui/dl.html");
static JS: &str = include_str!("../ui/dl.mjs");
static CSS: &str = include_str!("../ui/dl.css");
static INJECT_JS: &str = include_str!("../ui/inject.mjs");

fn asset(body: &'static str, mime: &'static str) -> Response {
    Response::builder()
        .header("content-type", mime)
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .unwrap()
}

async fn page() -> Html<&'static str> {
    Html(HTML)
}

/* ---------- 鉴权 ---------- */

#[derive(Deserialize, Default)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

fn authorized(state: &DlState, headers: &HeaderMap, query: &TokenQuery) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    if let Some(t) = &query.token {
        if t == expected {
            return true;
        }
    }
    if let Some(h) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        let h = h.trim();
        let token = h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
            .unwrap_or(h);
        if token.trim() == expected {
            return true;
        }
    }
    false
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"code": 1, "msg": "需要访问令牌（?token= 或 Authorization: Bearer）"})),
    )
        .into_response()
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"code": 1, "msg": msg.into()}))).into_response()
}

/* ---------- 接口 ---------- */

/// 并发上限：再高只会把 wrapper-lite 和带宽打满
const MAX_CONCURRENCY: usize = 16;

#[derive(serde::Deserialize)]
struct SettingsBody {
    concurrency: usize,
}

async fn settings_handler(
    State(_s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    Json(json!({
        "code": 0,
        "data": {
            "concurrency": dl.limit.get(),
            "active": dl.limit.active(),
            "max": MAX_CONCURRENCY,
        }
    }))
    .into_response()
}

/// 运行时调整并发：立即生效，并写入 `<root>/.settings.json` 供下次启动使用
async fn settings_update(
    State(_s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<SettingsBody>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let n = body.concurrency.clamp(1, MAX_CONCURRENCY);
    dl.limit.set(n);
    let path = dl.root.join(".settings.json");
    let payload = json!({ "concurrency": n });
    if let Err(e) = std::fs::write(&path, payload.to_string()) {
        tracing::warn!(error = %e, "写入 .settings.json 失败，本次调整仍然生效");
    }
    tracing::info!(concurrency = n, "并发已调整");
    Json(json!({
        "code": 0,
        "data": { "concurrency": dl.limit.get(), "active": dl.limit.active(), "max": MAX_CONCURRENCY }
    }))
    .into_response()
}

async fn config_handler(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let roots: Vec<_> = dl
        .roots
        .iter()
        .map(|r| json!({"path": r.display().to_string(), "free_bytes": free_bytes(r)}))
        .collect();
    Json(json!({
        "code": 0,
        "data": {
            "root": dl.root.display().to_string(),
            "roots": roots,
            "free_bytes": free_bytes(&dl.root),
            "concurrency": dl.limit.get(),
            "concurrency_max": MAX_CONCURRENCY,
            "concurrency_active": dl.limit.active(),
            "token_required": dl.token.is_some(),
            "qualities": ["best", "alac", "atmos", "aac", "aac-binaural", "aac-downmix", "heaac"],
        }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct PreviewBody {
    url: String,
}

async fn preview_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<PreviewBody>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    match resolve::resolve(&state, &body.url).await {
        Ok(r) => Json(json!({
            "code": 0,
            "data": {
                "kind": r.kind,
                "id": r.id,
                "storefront": r.storefront,
                "title": r.title,
                "artist": r.artist,
                "count": r.tracks.len(),
                "tracks": r.tracks.iter().map(|t| json!({
                    "adam_id": t.adam_id,
                    "title": t.title,
                    "artist": t.artist,
                    "album": t.album,
                    "track_no": t.track_no,
                    "disc_no": t.disc_no,
                    "isrc": t.isrc,
                })).collect::<Vec<_>>(),
            }
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct CreateBody {
    url: String,
    #[serde(default)]
    quality: Option<Quality>,
    #[serde(default)]
    max_sample_rate: Option<u32>,
    #[serde(default)]
    max_height: Option<u32>,
    #[serde(default)]
    dest: Option<String>,
    #[serde(default)]
    album_dir: Option<String>,
    #[serde(default)]
    track_file: Option<String>,
    #[serde(default)]
    write_tags: Option<bool>,
    #[serde(default)]
    embed_cover: Option<bool>,
    /// none / embed / lrc / both
    #[serde(default)]
    lyrics: Option<String>,
    #[serde(default)]
    syllable: Option<bool>,
    #[serde(default)]
    skip_existing: Option<bool>,
    /// 用国区中文元数据（默认开）
    #[serde(default)]
    cn_meta: Option<bool>,
    /// 只下载前 N 首（试跑用）
    #[serde(default)]
    limit: Option<usize>,
    /// 直接给定曲目（跳过解析），便于脚本调用
    #[serde(default)]
    tracks: Option<Vec<TrackSpec>>,
    #[serde(default)]
    title: Option<String>,
}

async fn create_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<CreateBody>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }

    let mut options = JobOptions::default();
    if let Some(v) = body.quality {
        options.quality = v;
    }
    if let Some(v) = body.max_sample_rate {
        options.max_sample_rate = v;
    }
    if let Some(v) = body.max_height {
        options.max_height = v;
    }
    if let Some(v) = body.dest {
        // 提前校验，避免任务跑起来才发现目录不可写
        if let Err(e) = dl.resolve_base(&v) {
            return err(StatusCode::BAD_REQUEST, e);
        }
        options.dest = v;
    }
    if let Some(v) = body.album_dir.filter(|v| !v.trim().is_empty()) {
        options.album_dir = v;
    }
    if let Some(v) = body.track_file.filter(|v| !v.trim().is_empty()) {
        options.track_file = v;
    }
    if let Some(v) = body.write_tags {
        options.write_tags = v;
    }
    if let Some(v) = body.embed_cover {
        options.embed_cover = v;
    }
    if let Some(v) = body.lyrics {
        options.lyrics = match v.as_str() {
            "none" | "embed" | "lrc" | "both" => v,
            other => {
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("lyrics 只能是 none/embed/lrc/both，收到 {other}"),
                )
            }
        };
    }
    if let Some(v) = body.syllable {
        options.syllable = v;
    }
    if let Some(v) = body.skip_existing {
        options.skip_existing = v;
    }
    if let Some(v) = body.cn_meta {
        options.cn_meta = v;
    }

    let (kind, mut title, artist, mut tracks) = if let Some(tracks) = body.tracks {
        let title = body.title.clone().unwrap_or_else(|| "手动任务".into());
        ("manual".to_string(), title, String::new(), tracks)
    } else {
        match resolve::resolve(&state, &body.url).await {
            Ok(r) => (r.kind, r.title, r.artist, r.tracks),
            Err(e) => return err(StatusCode::BAD_GATEWAY, e),
        }
    };
    // 用国区（cn）的中文元数据覆盖名称 / 专辑 / 歌手，并改用国区歌词
    if options.cn_meta && kind != "manual" {
        let storefront = resolve::storefront_of(&body.url);
        if let Some(album) = resolve::enrich_cn(&state, &mut tracks, &storefront).await {
            title = album;
        }
    }
    if let Some(limit) = body.limit {
        tracks.truncate(limit);
    }
    if tracks.is_empty() {
        return err(StatusCode::BAD_REQUEST, "没有可下载的曲目");
    }

    // 歌手链接：按专辑拆成多个顶层任务。
    // 一个任务里塞几千条子项时，/dl 每 3 秒重绘整份列表，根本没法看失败项。
    let groups: Vec<(String, String, Vec<model::TrackSpec>)> = if kind == "artist" {
        split_by_album(tracks)
    } else {
        vec![(title, artist, tracks)]
    };

    let mut created: Vec<Arc<Handle>> = Vec::new();
    for (title, artist, items) in groups {
        let job = Job {
            id: model::new_job_id(),
            kind: kind.clone(),
            url: body.url.clone(),
            title,
            artist,
            status: Status::Queued,
            created_at: model::now(),
            updated_at: model::now(),
            options: options.clone(),
            items: items.into_iter().map(Item::new).collect(),
            error: None,
        };
        let handle = Handle::new(job);
        dl.insert(handle.clone());
        dl.persist(&handle.snapshot());
        spawn_runner(state.clone(), handle.clone());
        created.push(handle);
    }

    let first = created.first().map(|h| h.snapshot()).expect("至少一个任务");
    let brief: Vec<_> = created
        .iter()
        .map(|h| {
            let j = h.snapshot();
            let (total, _, _) = j.counts();
            json!({"id": j.id, "title": j.title, "total": total})
        })
        .collect();
    Json(json!({
        "code": 0,
        "data": first,
        "created": created.len(),
        "jobs": brief,
    }))
    .into_response()
}

/// 按专辑分组，保持原有顺序；专辑 id 为空时退回「专辑名 + 年份」
fn split_by_album(tracks: Vec<model::TrackSpec>) -> Vec<(String, String, Vec<model::TrackSpec>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, (String, String, Vec<model::TrackSpec>)> =
        std::collections::HashMap::new();
    for t in tracks {
        let key = if !t.album_id.is_empty() {
            t.album_id.clone()
        } else {
            format!("{}|{}", t.album, t.release_date)
        };
        let entry = map.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            let artist = if t.album_artist.is_empty() {
                t.artist.clone()
            } else {
                t.album_artist.clone()
            };
            (t.album.clone(), artist, Vec::new())
        });
        entry.2.push(t);
    }
    order.into_iter().filter_map(|k| map.remove(&k)).collect()
}

fn spawn_runner(state: Arc<AppState>, handle: Arc<Handle>) {
    tokio::spawn(async move {
        run_job(state, handle).await;
    });
}

async fn run_job(state: Arc<AppState>, handle: Arc<Handle>) {
    let dl = dl();
    let (options, items) = {
        let job = handle.job.lock().unwrap();
        (job.options.clone(), job.items.clone())
    };

    let mut tasks = futures::stream::iter(items.into_iter().enumerate().map(|(idx, _item)| {
        let state = state.clone();
        let handle = handle.clone();
        let options = options.clone();
        async move {
            if handle.canceled() {
                return;
            }
            let skip = {
                let job = handle.job.lock().unwrap();
                job.items[idx].status != Status::Queued
            };
            if skip {
                return;
            }

            let _permit = dl.limit.acquire().await;

            let spec = {
                let job = handle.job.lock().unwrap();
                job.items[idx].spec.clone()
            };

            // 模板里用到 {quality} 时，先解析音质（一次 master 请求），路径才能在下载前定下来
            let quality = if needs_quality(&options) {
                match download::plan_label(&state, &spec, &options).await {
                    Ok(q) => q,
                    Err(e) => {
                        handle.with(|job| {
                            let item = &mut job.items[idx];
                            item.status = Status::Failed;
                            item.attempts += 1;
                            item.error = Some(e);
                            job.refresh();
                        });
                        persist(&handle);
                        return;
                    }
                }
            } else {
                String::new()
            };

            // 目标路径：先算出来，便于跳过已存在文件
            let target = target_path(&handle, idx, &options, &quality);
            if options.skip_existing {
                if let Some(path) = &target {
                    if path.is_file() {
                        handle.with(|job| {
                            job.items[idx].status = Status::Skipped;
                            job.items[idx].path = Some(path.display().to_string());
                            job.items[idx].bytes =
                                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        });
                        persist(&handle);
                        return;
                    }
                }
            }

            handle.with(|job| {
                job.items[idx].status = Status::Running;
                job.items[idx].error = None;
                job.refresh();
            });
            persist(&handle);
            let out_path = target.unwrap_or_else(|| dl.root.join(format!("{}.m4a", spec.adam_id)));
            let cancel_flag = &handle.cancel;

            let result = if spec.is_mv {
                mv::download_mv(&state, &spec, &options, &out_path, &dl.temp, cancel_flag).await
            } else {
                download::download_track(&state, &spec, &options, &out_path, &dl.temp, cancel_flag)
                    .await
            };

            handle.with(|job| {
                let item = &mut job.items[idx];
                item.attempts += 1;
                match result {
                    Ok(done) => {
                        item.status = Status::Done;
                        item.bytes = done.bytes;
                        item.quality = Some(done.quality);
                        item.path = Some(done.path.display().to_string());
                        item.error = None;
                    }
                    Err(e) => {
                        item.status = if handle.canceled() {
                            Status::Canceled
                        } else {
                            Status::Failed
                        };
                        item.error = Some(e);
                    }
                }
                job.refresh();
            });
            persist(&handle);
        }
    }))
    .buffer_unordered(16);

    while tasks.next().await.is_some() {}

    handle.with(|job| {
        job.refresh();
    });
    persist(&handle);
}

fn persist(handle: &Arc<Handle>) {
    let job = handle.snapshot();
    dl().persist(&job);
}

fn needs_quality(options: &JobOptions) -> bool {
    options.album_dir.contains("{quality}") || options.track_file.contains("{quality}")
}

fn target_path(
    handle: &Arc<Handle>,
    idx: usize,
    options: &JobOptions,
    quality: &str,
) -> Option<PathBuf> {
    let job = handle.job.lock().unwrap();
    let item = job.items.get(idx)?;
    let dir = tags::render_template(&options.album_dir, &item.spec, &item.spec.playlist, quality);
    let name = tags::render_template(
        &options.track_file,
        &item.spec,
        &item.spec.playlist,
        quality,
    );
    let mut path = match dl().resolve_base(&options.dest) {
        Ok(p) => p,
        Err(_) => dl().root.clone(),
    };
    if !dir.is_empty() && dir != "_" {
        path = path.join(dir);
    }
    path.push(format!(
        "{name}.{}",
        if item.spec.is_mv { "mp4" } else { "m4a" }
    ));
    Some(path)
}

async fn list_handler(
    State(_s): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let mut jobs: Vec<Job> = dl
        .handles
        .read()
        .unwrap()
        .values()
        .map(|h| h.snapshot())
        .collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.created_at));
    let list: Vec<_> = jobs
        .iter()
        .map(|j| {
            let (total, done, failed) = j.counts();
            json!({
                "id": j.id,
                "title": j.title,
                "artist": j.artist,
                "kind": j.kind,
                "status": j.status,
                "total": total,
                "done": done,
                "failed": failed,
                "bytes": j.bytes(),
                "created_at": j.created_at,
                "updated_at": j.updated_at,
            })
        })
        .collect();
    Json(json!({"code": 0, "data": list})).into_response()
}

async fn get_handler(
    State(_s): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    match dl.get_handle(&id) {
        Some(h) => Json(json!({"code": 0, "data": h.snapshot()})).into_response(),
        None => err(StatusCode::NOT_FOUND, "任务不存在"),
    }
}

async fn cancel_handler(
    State(_s): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let Some(h) = dl.get_handle(&id) else {
        return err(StatusCode::NOT_FOUND, "任务不存在");
    };
    h.cancel.store(true, Ordering::Relaxed);
    h.with(|job| {
        for item in job.items.iter_mut() {
            if item.status == Status::Queued {
                item.status = Status::Canceled;
            }
        }
        job.refresh();
    });
    persist(&h);
    Json(json!({"code": 0, "data": h.snapshot()})).into_response()
}

async fn retry_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let Some(h) = dl.get_handle(&id) else {
        return err(StatusCode::NOT_FOUND, "任务不存在");
    };
    retry_job(&state, &h);
    Json(json!({"code": 0, "data": h.snapshot()})).into_response()
}

/// 把失败 / 取消的曲目重新排队并启动执行器
fn retry_job(state: &Arc<AppState>, h: &Arc<Handle>) {
    h.cancel.store(false, Ordering::Relaxed);
    h.with(|job| {
        for item in job.items.iter_mut() {
            if matches!(item.status, Status::Failed | Status::Canceled) {
                item.status = Status::Queued;
                item.error = None;
            }
        }
        job.refresh();
    });
    persist(h);
    spawn_runner(state.clone(), h.clone());
}

async fn remove_handler(
    State(_s): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }
    let Some(h) = dl.get_handle(&id) else {
        return err(StatusCode::NOT_FOUND, "任务不存在");
    };
    h.cancel.store(true, Ordering::Relaxed);
    dl.handles.write().unwrap().remove(&id);
    let _ = std::fs::remove_file(dl.jobs_dir.join(format!("{id}.json")));
    Json(json!({"code": 0})).into_response()
}

/// 归一化路径：消掉 `.` 与 `..`（不访问文件系统，允许目标尚不存在）
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Deserialize)]
struct BatchBody {
    /// retry / remove / cancel
    action: String,
    #[serde(default)]
    ids: Vec<String>,
    /// 不给 ids 时按状态筛选：done / failed / canceled
    #[serde(default)]
    filter: Option<String>,
}

/// 批量操作：`{"action":"remove","ids":[...]}` 或 `{"action":"remove","filter":"done"}`
async fn batch_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(body): Json<BatchBody>,
) -> Response {
    let dl = dl();
    if !authorized(dl, &headers, &q) {
        return unauthorized();
    }

    // 目标集合：显式 ids 优先，否则按状态筛
    let targets: Vec<Arc<Handle>> = if !body.ids.is_empty() {
        body.ids.iter().filter_map(|id| dl.get_handle(id)).collect()
    } else {
        let want = body.filter.as_deref().unwrap_or("");
        let all: Vec<Arc<Handle>> = dl.handles.read().unwrap().values().cloned().collect();
        all.into_iter()
            .filter(|h| {
                let status = h.snapshot().status;
                match want {
                    "done" => status == Status::Done,
                    "failed" => status == Status::Failed,
                    "canceled" => status == Status::Canceled,
                    "finished" => status.is_final(),
                    _ => false,
                }
            })
            .collect()
    };

    let mut affected = 0usize;
    for h in targets {
        match body.action.as_str() {
            "retry" => {
                retry_job(&state, &h);
                affected += 1;
            }
            "cancel" => {
                h.cancel.store(true, Ordering::Relaxed);
                h.with(|job| {
                    for item in job.items.iter_mut() {
                        if item.status == Status::Queued {
                            item.status = Status::Canceled;
                        }
                    }
                    job.refresh();
                });
                persist(&h);
                affected += 1;
            }
            "remove" => {
                let id = h.with(|job| job.id.clone());
                h.cancel.store(true, Ordering::Relaxed);
                dl.handles.write().unwrap().remove(&id);
                let _ = std::fs::remove_file(dl.jobs_dir.join(format!("{id}.json")));
                affected += 1;
            }
            other => {
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("未知动作 {other}（retry / cancel / remove）"),
                )
            }
        }
    }

    Json(json!({"code": 0, "data": {"affected": affected}})).into_response()
}

/* ---------- 磁盘 ---------- */

fn free_bytes(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct StatVfs {
        f_bsize: u64,
        f_frsize: u64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_favail: u64,
        f_fsid: u64,
        f_flag: u64,
        f_namemax: u64,
        __f_spare: [i32; 6],
    }

    extern "C" {
        fn statvfs(path: *const std::os::raw::c_char, buf: *mut StatVfs) -> i32;
    }

    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut buf = std::mem::MaybeUninit::<StatVfs>::zeroed();
    let rc = unsafe { statvfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return 0;
    }
    let buf = unsafe { buf.assume_init() };
    buf.f_bavail.saturating_mul(buf.f_frsize)
}
