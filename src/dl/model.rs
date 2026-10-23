//! 服务端下载：数据模型。
//!
//! 一个 Job = 一次批量下载（专辑 / 歌单 / 单曲），Job 下每个 Item = 一首曲目。
//! 全部状态都持久化到 `<root>/.jobs/<id>.json`，进程重启后能继续列出与重试。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Done,
    Failed,
    Skipped,
    Canceled,
}

impl Status {
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Status::Done | Status::Failed | Status::Skipped | Status::Canceled
        )
    }
}

/// 音质偏好。`Best` 按 ALAC → Atmos → AAC → HE-AAC 的顺序挑第一个可用的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    #[default]
    Best,
    Alac,
    Atmos,
    Aac,
    AacBinaural,
    AacDownmix,
    Heaac,
}

/// 一首曲目的元数据（来自 amp-api，随任务一起保存，重启后无需再查目录）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackSpec {
    pub adam_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_artist: String,
    #[serde(default)]
    pub composer: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub release_date: String,
    #[serde(default)]
    pub track_no: u32,
    #[serde(default)]
    pub track_total: u32,
    #[serde(default)]
    pub disc_no: u32,
    #[serde(default)]
    pub disc_total: u32,
    #[serde(default)]
    pub isrc: String,
    #[serde(default)]
    pub copyright: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub upc: String,
    #[serde(default)]
    pub cover_url: String,
    #[serde(default)]
    pub song_id: String,
    #[serde(default)]
    pub album_id: String,
    #[serde(default)]
    pub artist_id: String,
    #[serde(default)]
    pub playlist: String,
    /// 这条是 MV（走 PlayReady + 音视频合并）
    #[serde(default)]
    pub is_mv: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOptions {
    pub quality: Quality,
    /// 保存位置：相对路径（拼到根目录）或绝对路径（必须在允许的根目录内）
    #[serde(default)]
    pub dest: String,
    /// 0 = 不限制，否则选不超过该采样率的最高档（ALAC 用）
    #[serde(default)]
    pub max_sample_rate: u32,
    /// MV 的最高分辨率（0 = 不限制）
    #[serde(default)]
    pub max_height: u32,
    /// 专辑 / 歌单目录模板，`/` 表示子目录
    #[serde(default = "default_album_dir")]
    pub album_dir: String,
    /// 曲目文件名模板（不含扩展名）
    #[serde(default = "default_track_file")]
    pub track_file: String,
    #[serde(default = "default_true")]
    pub write_tags: bool,
    #[serde(default = "default_true")]
    pub embed_cover: bool,
    /// 歌词：`none` / `embed`（只写标签）/ `lrc`（只存 .lrc）/ `both`
    #[serde(default = "default_lyrics")]
    pub lyrics: String,
    /// 逐字（增强型 LRC）
    #[serde(default)]
    pub syllable: bool,
    /// 已存在的同名文件跳过（断点续传）
    #[serde(default = "default_true")]
    pub skip_existing: bool,
}

fn default_true() -> bool {
    true
}
fn default_album_dir() -> String {
    "{album_artist}/{album} ({year}) [{quality}]".into()
}
fn default_lyrics() -> String {
    "both".into()
}
fn default_track_file() -> String {
    "{tracknum:02d} {title}".into()
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            quality: Quality::Best,
            dest: String::new(),
            max_sample_rate: 0,
            max_height: 0,
            album_dir: default_album_dir(),
            track_file: default_track_file(),
            write_tags: true,
            embed_cover: true,
            lyrics: default_lyrics(),
            syllable: false,
            skip_existing: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub spec: TrackSpec,
    pub status: Status,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub bytes: u64,
    /// 实际下载到的音质（如 `ALAC 24bit/96kHz`）
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub attempts: u32,
}

impl Item {
    pub fn new(spec: TrackSpec) -> Self {
        Self {
            spec,
            status: Status::Queued,
            error: None,
            path: None,
            bytes: 0,
            quality: None,
            attempts: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub url: String,
    pub title: String,
    pub artist: String,
    pub status: Status,
    pub created_at: i64,
    pub updated_at: i64,
    pub options: JobOptions,
    pub items: Vec<Item>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Job {
    pub fn counts(&self) -> (usize, usize, usize) {
        let done = self
            .items
            .iter()
            .filter(|i| matches!(i.status, Status::Done | Status::Skipped))
            .count();
        let failed = self
            .items
            .iter()
            .filter(|i| i.status == Status::Failed)
            .count();
        (self.items.len(), done, failed)
    }

    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }

    /// 由各曲目状态推导整体状态
    pub fn refresh(&mut self) {
        self.updated_at = now();
        if self.items.is_empty() {
            self.status = Status::Failed;
            return;
        }
        if self.items.iter().any(|i| i.status == Status::Running) {
            self.status = Status::Running;
            return;
        }
        if self.items.iter().any(|i| i.status == Status::Queued) {
            self.status = Status::Running;
            return;
        }
        let failed = self.items.iter().any(|i| i.status == Status::Failed);
        let done = self
            .items
            .iter()
            .any(|i| matches!(i.status, Status::Done | Status::Skipped));
        self.status = match (failed, done) {
            (true, _) => Status::Failed,
            (false, true) => Status::Done,
            (false, false) => Status::Canceled,
        };
    }
}

/// 运行期句柄：状态 + 取消标志
pub struct Handle {
    pub job: Mutex<Job>,
    pub cancel: AtomicBool,
}

impl Handle {
    pub fn new(job: Job) -> Arc<Handle> {
        Arc::new(Handle {
            job: Mutex::new(job),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn snapshot(&self) -> Job {
        self.job.lock().unwrap().clone()
    }

    pub fn canceled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Job) -> R) -> R {
        let mut job = self.job.lock().unwrap();
        let r = f(&mut job);
        job.updated_at = now();
        r
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 任务 ID：时间戳 + 计数器，稳定可读且不重复
pub fn new_job_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{ms:x}-{:02x}",
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xff
    )
}
