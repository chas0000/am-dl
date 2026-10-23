//! Apple Music 链接解析（专辑 / 歌单 / 单曲 / MV / 歌手）。

use std::sync::LazyLock;

use regex::Regex;

static SONG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://music\.apple\.com/[a-z]{2}/song/[^/?#]+/([0-9]+)(?:[/?#]|$)").unwrap()
});
static MV_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://music\.apple\.com/[a-z]{2}/music-video/[^/?#]+/([0-9]+)(?:[/?#]|$)")
        .unwrap()
});
static ALBUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://music\.apple\.com/[a-z]{2}/album/(?:[^/?#]+/)?([0-9]+)(?:[/?#]|$)")
        .unwrap()
});
static PLAYLIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://music\.apple\.com/[a-z]{2}/playlist/(?:[^/?#]+/)?(pl\.[0-9A-Za-z_-]+)(?:[/?#]|$)").unwrap()
});
static ARTIST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https://music\.apple\.com/[a-z]{2}/artist/(?:[^/?#]+/)?([0-9]+)(?:[/?#]|$)")
        .unwrap()
});

fn capture(re: &Regex, url: &str, what: &str) -> Result<String, String> {
    re.captures(url.trim())
        .map(|c| c[1].to_string())
        .ok_or_else(|| format!("不是有效的 Apple Music {what}链接: {url}"))
}

pub fn parse_song_link(url: &str) -> Result<String, String> {
    capture(&SONG_RE, url, "歌曲")
}
pub fn parse_mv_link(url: &str) -> Result<String, String> {
    capture(&MV_RE, url, "MV")
}
pub fn parse_album_link(url: &str) -> Result<String, String> {
    capture(&ALBUM_RE, url, "专辑")
}
pub fn parse_playlist_link(url: &str) -> Result<String, String> {
    capture(&PLAYLIST_RE, url, "歌单")
}
pub fn parse_artist_link(url: &str) -> Result<String, String> {
    capture(&ARTIST_RE, url, "歌手")
}
