//! 运行配置与共享状态（am-dl 只做下载，没有播放器页面）。

use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub wrapper_url: String,
    /// wrapper-lite 的 Authorization（可选）
    pub wrapper_auth: Option<String>,
    /// 同时进行的 wrapper-lite 请求上限
    pub wrapper_concurrency: usize,
    /// 上游 am-hook 的地址；设置后本服务同时充当它的反向代理
    pub upstream: Option<String>,
}

pub struct AppState {
    #[allow(dead_code)]
    pub config: Config,
    pub http_client: reqwest::Client,
    /// 代理上游用的客户端：不设读超时（页面/资源可能慢），不解压（要就地改 HTML）
    pub proxy_client: reqwest::Client,
    pub upstream: Option<String>,
    pub wrapper: crate::wrapper::Wrapper,
    pub amp: Arc<crate::amp::Amp>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(32)
            .http1_only()
            .no_gzip()
            .no_brotli()
            .build()
            .expect("Failed to build HTTP client");

        let proxy_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .http1_only()
            .no_gzip()
            .no_brotli()
            .build()
            .expect("Failed to build proxy client");

        let wrapper = crate::wrapper::Wrapper::new(&config);
        let amp = Arc::new(crate::amp::Amp::new());
        Self {
            upstream: config.upstream.clone(),
            config,
            http_client,
            proxy_client,
            wrapper,
            amp,
        }
    }
}
