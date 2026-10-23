//! 把真实的 git 版本写进二进制：`/api/dl/ping` 和启动日志都会带上它。
//!
//! 打 tag 构建时版本来自 `git describe`（如 `v0.1.0`），没有 git 环境（源码包解压）
//! 时退回 `Cargo.toml` 里的版本。

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=AM_DL_VERSION_OVERRIDE");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    // CI 打 tag 时会直接给版本（此时工作区因为改了 Cargo.toml 而是 dirty 的）
    let from_env = std::env::var("AM_DL_VERSION_OVERRIDE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let from_git = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let version = from_env.or(from_git).unwrap_or_else(|| {
        format!(
            "v{}",
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into())
        )
    });
    println!("cargo:rustc-env=AM_DL_VERSION={version}");
}
