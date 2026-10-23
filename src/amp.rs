//! amp-api（Apple Music 目录）最小客户端：抓网页版 developer token + 带缓存地请求目录接口。
//!
//! 与 am-hook 的完整代理相比只保留下载需要的部分：没有 HTTP/2 预热、没有 LRU、
//! 没有电台接口；token 按 JWT 的 `exp` 提前一小时刷新。

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::Value;

const WEB_ORIGIN: &str = "https://music.apple.com";
const API_ORIGIN: &str = "https://amp-api-edge.music.apple.com";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);

pub struct Amp {
    client: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
    script_re: Regex,
    jwt_re: Regex,
}

impl Amp {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent(UA)
            .build()
            .expect("amp client");
        Self {
            client,
            token: Mutex::new(None),
            script_re: Regex::new(r#"src="(/assets/index[~-][0-9A-Za-z_-]+\.js)""#).unwrap(),
            jwt_re: Regex::new(r"eyJ[0-9A-Za-z_-]{10,}\.eyJ[0-9A-Za-z_-]{10,}\.[0-9A-Za-z_-]{10,}")
                .unwrap(),
        }
    }

    fn cached(&self) -> Option<String> {
        let guard = self.token.lock().unwrap();
        guard
            .as_ref()
            .filter(|(_, exp)| Instant::now() < *exp)
            .map(|(t, _)| t.clone())
    }

    async fn token(&self) -> Result<String, String> {
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let page = self
            .client
            .get(format!("{WEB_ORIGIN}/us/browse"))
            .send()
            .await
            .map_err(|e| format!("获取 music.apple.com 失败: {e}"))?
            .text()
            .await
            .map_err(|e| format!("读取 music.apple.com 失败: {e}"))?;

        let script = self
            .script_re
            .captures(&page)
            .map(|c| c[1].to_string())
            .ok_or("music.apple.com 页面里没找到主脚本")?;

        let js = self
            .client
            .get(format!("{WEB_ORIGIN}{script}"))
            .send()
            .await
            .map_err(|e| format!("获取脚本失败: {e}"))?
            .text()
            .await
            .map_err(|e| format!("读取脚本失败: {e}"))?;

        let token = self
            .jwt_re
            .find(&js)
            .map(|m| m.as_str().to_string())
            .ok_or("脚本里没找到 developer token")?;

        // 按 JWT 的 exp 提前一小时过期；解析不出来就用固定 12 小时
        let ttl = jwt_ttl(&token).unwrap_or(TOKEN_TTL);
        *self.token.lock().unwrap() = Some((token.clone(), Instant::now() + ttl));
        tracing::info!(ttl_min = ttl.as_secs() / 60, "已获取 developer token");
        Ok(token)
    }

    /// `path` 形如 `catalog/us/albums/123`，`query` 不含 `?`
    pub async fn catalog_json(&self, path: &str, query: &str) -> Result<Value, String> {
        let mut url = format!("{API_ORIGIN}/v1/{path}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        let token = self.token().await?;
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .header(reqwest::header::ORIGIN, WEB_ORIGIN)
            .header(reqwest::header::REFERER, format!("{WEB_ORIGIN}/"))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("amp-api 请求失败: {e}"))?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            // token 失效：清掉再试一次
            *self.token.lock().unwrap() = None;
            let token = self.token().await?;
            let resp = self
                .client
                .get(&url)
                .bearer_auth(token)
                .header(reqwest::header::ORIGIN, WEB_ORIGIN)
                .header(reqwest::header::REFERER, format!("{WEB_ORIGIN}/"))
                .send()
                .await
                .map_err(|e| format!("amp-api 重试失败: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("amp-api 返回 HTTP {}", resp.status()));
            }
            return resp
                .json()
                .await
                .map_err(|e| format!("解析 amp-api JSON 失败: {e}"));
        }
        if !status.is_success() {
            return Err(format!("amp-api 返回 HTTP {status}"));
        }
        resp.json()
            .await
            .map_err(|e| format!("解析 amp-api JSON 失败: {e}"))
    }
}

/// JWT 的 exp 减现在，再留一小时余量
fn jwt_ttl(token: &str) -> Option<Duration> {
    use base64_decode::decode_payload;
    let payload = decode_payload(token)?;
    let v: Value = serde_json::from_slice(&payload).ok()?;
    let exp = v.get("exp")?.as_u64()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let secs = exp.saturating_sub(now).saturating_sub(3600);
    (secs > 60).then(|| Duration::from_secs(secs))
}

/// 只为了解 JWT 载荷：标准 base64url，去掉 padding
mod base64_decode {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    pub fn decode_payload(token: &str) -> Option<Vec<u8>> {
        let payload = token.split('.').nth(1)?;
        let mut out = Vec::with_capacity(payload.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for ch in payload.bytes() {
            let val = ALPHABET.iter().position(|&c| c == ch)? as u32;
            buf = (buf << 6) | val;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        Some(out)
    }
}
