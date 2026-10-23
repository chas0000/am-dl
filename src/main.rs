//! am-dl —— 轻量版 Apple Music 服务端下载器。
//!
//! 只做一件事：把 Apple Music 链接（专辑 / 歌单 / 单曲 / MV / 歌手）下载并解密到本机硬盘，
//! 用一个页面看进度。没有 am-hook 的播放器、资料库、电台，也不需要打补丁——
//! 解密与封装直接用上游的 `am-mp4` / `am-media` 两个纯 Rust crate。
//!
//! 环境变量：
//!   AM_DL_ROOT        下载根目录（默认 /root/music）
//!   AM_DL_ROOTS       多个根目录，`:` 分隔（优先于 AM_DL_ROOT）
//!   AM_DL_TOKEN       访问令牌，可选；不设 = 无鉴权
//!   AM_DL_CONCURRENCY 同时下载的曲目数（默认 2）
//!   AM_DL_WRAPPER     wrapper-lite 地址（默认 http://127.0.0.1:12340）
//!   AM_DL_UPSTREAM    上游 am-hook 地址（如 http://127.0.0.1:8899）；
//!                     设置后本服务同时做它的反向代理，并在页面里注入下载浮窗

mod amp;
mod config;
mod dl;
mod links;
mod proxy;
mod wrapper;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::config::{AppState, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "am_dl=info".into()),
        )
        .init();

    let config = Config {
        wrapper_url: std::env::var("AM_DL_WRAPPER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:12340".into()),
        wrapper_auth: std::env::var("AM_DL_WRAPPER_AUTH")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        wrapper_concurrency: std::env::var("AM_DL_WRAPPER_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24),
        upstream: std::env::var("AM_DL_UPSTREAM")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
    };
    let listen = std::env::var("AM_DL_LISTEN").unwrap_or_else(|_| "0.0.0.0:8888".into());
    let addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| format!("AM_DL_LISTEN 非法: {e}"))?;

    let state = Arc::new(AppState::new(config));
    dl::init(&state);

    let mut app = Router::new().merge(dl::router());
    match state.upstream.clone() {
        // 代理模式：除 /dl 和 /api/dl 之外全部转给上游（含首页），并注入浮窗
        Some(up) => {
            tracing::info!("代理模式：其余请求转发到 {up}");
            app = app.fallback(proxy::handler);
        }
        None => {
            app = app.route(
                "/",
                get(|| async { axum::response::Redirect::temporary("/dl") }),
            )
        }
    }
    let app = app.with_state(state);

    tracing::info!(
        "am-dl listening on http://{addr}  (wrapper {})",
        std::env::var("AM_DL_WRAPPER").unwrap_or_else(|_| "http://127.0.0.1:12340".into())
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
