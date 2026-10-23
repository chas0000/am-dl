//! wrapper-lite 客户端：限并发 + 可选鉴权，只保留下载需要的接口。

use std::time::Duration;

use bytes::Bytes;
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::config::Config;

pub struct Wrapper {
    client: reqwest::Client,
    base: String,
    auth: Option<String>,
    sem: Semaphore,
}

pub struct Reply {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Reply {
    pub fn json<'a, T: Deserialize<'a>>(&'a self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.body)
    }
}

impl Wrapper {
    pub fn new(config: &Config) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .http1_only()
            .no_gzip()
            .no_brotli()
            .build()
            .expect("wrapper client");
        Self {
            client,
            base: config.wrapper_url.trim_end_matches('/').to_string(),
            auth: config.wrapper_auth.clone(),
            sem: Semaphore::new(config.wrapper_concurrency.max(1)),
        }
    }

    fn builder(&self, method: Method, path: &str) -> RequestBuilder {
        let req = self
            .client
            .request(method, format!("{}{}", self.base, path));
        match &self.auth {
            Some(auth) => req.header(reqwest::header::AUTHORIZATION, auth),
            None => req,
        }
    }

    pub fn get(&self, path: &str) -> RequestBuilder {
        self.builder(Method::GET, path)
    }

    pub fn post(&self, path: &str) -> RequestBuilder {
        self.builder(Method::POST, path)
    }

    pub async fn send(&self, request: RequestBuilder) -> Result<Reply, reqwest::Error> {
        let _permit = self.sem.acquire().await;
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        Ok(Reply { status, body })
    }

    /// `/key` 返回的 `data` JSON（轨道解密模板）
    pub async fn fetch_key_json(&self, adam_id: &str, uri: &str) -> Result<String, String> {
        let request = self
            .get("/key")
            .query(&[("adamId", adam_id), ("uri", uri)])
            .timeout(Duration::from_secs(15));
        let reply = self
            .send(request)
            .await
            .map_err(|e| format!("请求 wrapper-lite /key 失败: {e}"))?;
        if !reply.status.is_success() {
            return Err(format!("wrapper-lite /key 返回 HTTP {}", reply.status));
        }
        let v: KeyResponse = reply
            .json()
            .map_err(|e| format!("解析 /key 响应失败: {e}"))?;
        if v.code != 0 {
            return Err(format!(
                "wrapper-lite 返回错误码 {}: {}",
                v.code,
                v.msg.unwrap_or_default()
            ));
        }
        let data = v.data.ok_or("wrapper-lite 响应缺少 data")?;
        serde_json::to_string(&data).map_err(|e| format!("序列化 key 模板失败: {e}"))
    }

    /// `/lyrics` 返回的 TTML
    pub async fn fetch_lyrics(
        &self,
        adam_id: &str,
        language: Option<&str>,
    ) -> Result<Lyrics, String> {
        let mut request = self.get("/lyrics").query(&[("adamId", adam_id)]);
        if let Some(language) = language {
            request = request.query(&[("language", language)]);
        }
        let reply = self
            .send(request.timeout(Duration::from_secs(15)))
            .await
            .map_err(|e| format!("请求 /lyrics 失败: {e}"))?;
        if reply.status == StatusCode::NOT_FOUND {
            return Ok(Lyrics::NotFound);
        }
        if !reply.status.is_success() {
            return Err(format!("wrapper-lite /lyrics 返回 HTTP {}", reply.status));
        }
        let v: LyricsResponse = reply
            .json()
            .map_err(|e| format!("解析 /lyrics 响应失败: {e}"))?;
        if v.code == 404 {
            return Ok(Lyrics::NotFound);
        }
        if v.code != 0 {
            return Err(format!("wrapper-lite 返回错误码 {}", v.code));
        }
        match v
            .data
            .and_then(|d| d.lyrics)
            .filter(|t| !t.trim().is_empty())
        {
            Some(ttml) => Ok(Lyrics::Found(ttml)),
            None => Ok(Lyrics::NotFound),
        }
    }
}

pub enum Lyrics {
    Found(String),
    NotFound,
}

#[derive(Deserialize)]
struct KeyResponse {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct LyricsResponse {
    code: i64,
    #[serde(default)]
    data: Option<LyricsData>,
}

#[derive(Deserialize)]
struct LyricsData {
    #[serde(default)]
    lyrics: Option<String>,
}
