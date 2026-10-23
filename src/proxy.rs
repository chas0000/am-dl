//! 反向代理：把上游 am-hook（预编译二进制）的页面和接口转发出去，
//! 顺便在 HTML 里注入「下载到服务器」浮窗脚本。
//!
//! 这样就不用再给上游打补丁、也不用自己编译整个 workspace：
//! am-hook 原样跑在 127.0.0.1，对外只暴露 am-dl 这一个端口。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::config::AppState;

/// 注入到 `</body>` 之前的脚本
const INJECT: &str = r#"<script src="/api/dl/assets/inject.mjs" defer></script>"#;

/// 请求体上限：上游的接口都是小 JSON
const MAX_BODY: usize = 8 * 1024 * 1024;
/// 响应体上限：最大的是 wasm，几 MB
const MAX_RESP: usize = 64 * 1024 * 1024;

/// 逐跳头不能原样转发
fn skip_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "accept-encoding"
            | "content-length"
            | "etag"
            | "content-encoding"
    )
}

pub async fn handler(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let Some(upstream) = st.upstream.as_deref() else {
        return (StatusCode::NOT_FOUND, "未配置 AM_DL_UPSTREAM").into_response();
    };

    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/")
        .to_string();
    let url = format!("{upstream}{path}");

    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "请求体过大").into_response(),
    };

    let mut rb = st
        .proxy_client
        .request(parts.method.clone(), &url)
        .body(body.to_vec());
    for (k, v) in parts.headers.iter() {
        if skip_header(k.as_str()) {
            continue;
        }
        rb = rb.header(k.as_str(), v.as_bytes());
    }
    // 关掉压缩：HTML 要就地改，压缩流改不了
    rb = rb.header("accept-encoding", "identity");

    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%url, error = %e, "上游请求失败");
            return (StatusCode::BAD_GATEWAY, format!("上游不可达: {e}")).into_response();
        }
    };

    let status = resp.status();
    let headers = resp.headers().clone();
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_html = content_type.starts_with("text/html");

    let bytes = match resp.bytes().await {
        Ok(b) if b.len() <= MAX_RESP => b,
        Ok(_) => return (StatusCode::BAD_GATEWAY, "上游响应过大").into_response(),
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("读取上游响应失败: {e}")).into_response()
        }
    };

    // 304 之类没有响应体，原样返回（ETag 已经去掉，这里主要是保险）
    if status == StatusCode::NOT_MODIFIED || status == StatusCode::NO_CONTENT {
        let mut out = Response::builder().status(status);
        for (k, v) in headers.iter() {
            if skip_header(k.as_str()) {
                continue;
            }
            out = out.header(k.as_str(), v.as_bytes());
        }
        return out
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }

    let (payload, inject) = if is_html && status.is_success() {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let page = match text.rfind("</body>") {
            Some(i) => format!("{}{}{}", &text[..i], INJECT, &text[i..]),
            None => format!("{text}{INJECT}"),
        };
        (page.into_bytes(), true)
    } else {
        (bytes.to_vec(), false)
    };

    let mut out = Response::builder().status(status);
    for (k, v) in headers.iter() {
        if skip_header(k.as_str()) {
            continue;
        }
        out = out.header(k.as_str(), v.as_bytes());
    }
    if is_html {
        out = out.header(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
    }
    if inject {
        // 内容变了，ETag 不能再给，否则浏览器会拿着旧的 304 不刷新
        out = out.header("cache-control", HeaderValue::from_static("no-cache"));
    }
    out = out.header(CONTENT_LENGTH, payload.len());
    out = out.header(ETAG, HeaderValue::from_static("\"amdl\""));
    out.body(Body::from(payload))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}
