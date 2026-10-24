//! Apple Music TTML 歌词 → LRC（逐行 / 逐字）与纯文本。
//!
//! 上游在 `src/ui/lrc.mjs` 里做同样的事；这里只保留下载需要的部分：
//! `[mm:ss.xx]歌词`，可选的逐字 `<mm:ss.xx>`，翻译 / 发音作为同时间的附加行。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

static P_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<p\b([^>]*)>(.*?)</p>").unwrap());
static SPAN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<span\b([^>]*)>(.*?)</span>").unwrap());
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
static TRANS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<translation\b([^>]*)>(.*?)</translation>").unwrap());
static TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<text\b([^>]*)>(.*?)</text>").unwrap());
static ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"([a-zA-Z:_-]+)\s*=\s*"([^"]*)""#).unwrap());

fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    ATTR_RE.captures_iter(attrs).find_map(|c| {
        if c.get(1).map(|m| m.as_str()) == Some(name) {
            c.get(2).map(|m| m.as_str())
        } else {
            None
        }
    })
}

/// 时间：`12.345`、`12.345s`、`1:03.349`、`00:00:12.345`，单位秒
fn parse_time(value: &str) -> Option<f64> {
    let v = value.trim().trim_end_matches('s');
    if v.is_empty() {
        return None;
    }
    let parts: Vec<&str> = v.split(':').collect();
    match parts.len() {
        1 => parts[0].parse().ok(),
        2 => {
            let m: f64 = parts[0].parse().ok()?;
            let s: f64 = parts[1].parse().ok()?;
            Some(m * 60.0 + s)
        }
        3 => {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let s: f64 = parts[2].parse().ok()?;
            Some(h * 3600.0 + m * 60.0 + s)
        }
        _ => None,
    }
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn plain(inner: &str) -> String {
    unescape(&TAG_RE.replace_all(inner, "")).trim().to_string()
}

/// 与 [`plain`] 相同但保留首尾空白：逐字歌词里词间的空格就在 span 的尾部
fn plain_raw(inner: &str) -> String {
    unescape(&TAG_RE.replace_all(inner, ""))
}

fn stamp(secs: f64) -> String {
    let cs = (secs.max(0.0) * 100.0).floor() as u64;
    format!("{:02}:{:02}.{:02}", cs / 6000, (cs / 100) % 60, cs % 100)
}

struct Line {
    begin: f64,
    text: String,
    words: Vec<(f64, f64, String)>,
    extra: Vec<String>,
}

/// 翻译块里的一行：`for="L1"` → (整行文本, 逐字时间)
type Replacements = HashMap<String, (String, Vec<(f64, f64, String)>)>;

/// 取出 `type="replacement"` 且语言为简体（zh-Hans）的翻译。
/// Apple 的 TTML 正文是原文（常常是繁体），简体在翻译块里。
fn parse_replacements(ttml: &str) -> Replacements {
    let mut out = HashMap::new();
    for cap in TRANS_RE.captures_iter(ttml) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let lang = attr(attrs, "xml:lang").unwrap_or("");
        let kind = attr(attrs, "type").unwrap_or("");
        if kind != "replacement" || !lang.starts_with("zh-Hans") {
            continue;
        }
        let body = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        for text in TEXT_RE.captures_iter(body) {
            let tattrs = text.get(1).map(|m| m.as_str()).unwrap_or("");
            let key = attr(tattrs, "for").unwrap_or("");
            if key.is_empty() {
                continue;
            }
            let inner = text.get(2).map(|m| m.as_str()).unwrap_or("");
            let mut line = String::new();
            let mut words: Vec<(f64, f64, String)> = Vec::new();
            let mut last = 0usize;
            for span in SPAN_RE.captures_iter(inner) {
                let whole = span.get(0).unwrap();
                if whole.start() > last {
                    let gap = plain_raw(&inner[last..whole.start()]);
                    line.push_str(&gap);
                    if let Some(w) = words.last_mut() {
                        w.2.push_str(&gap);
                    }
                }
                last = whole.end();
                let sattrs = span.get(1).map(|m| m.as_str()).unwrap_or("");
                let sinner = span.get(2).map(|m| m.as_str()).unwrap_or("");
                let t = plain_raw(sinner);
                if t.trim().is_empty() {
                    continue;
                }
                let b = attr(sattrs, "begin").and_then(parse_time);
                let e = attr(sattrs, "end").and_then(parse_time);
                line.push_str(&t);
                words.push((b.unwrap_or(0.0), e.unwrap_or(b.unwrap_or(0.0)), t));
            }
            if last < inner.len() {
                let tail = plain_raw(&inner[last..]);
                line.push_str(&tail);
                if let Some(w) = words.last_mut() {
                    w.2.push_str(&tail);
                }
            }
            out.insert(key.to_string(), (line.trim().to_string(), words));
        }
    }
    out
}

fn parse_lines(ttml: &str) -> Vec<Line> {
    let replacements = parse_replacements(ttml);
    let mut out = Vec::new();
    for cap in P_RE.captures_iter(ttml) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let begin = attr(attrs, "begin").and_then(parse_time);
        let end = attr(attrs, "end").and_then(parse_time);

        // 有简体翻译就用翻译那一行（正文可能是繁体）
        let key = attr(attrs, "itunes:key").or_else(|| attr(attrs, "xml:id"));
        if let Some((text, words)) = key.and_then(|k| replacements.get(k)) {
            if !text.is_empty() {
                let b = begin.or_else(|| words.first().map(|w| w.0)).unwrap_or(0.0);
                out.push(Line {
                    begin: b,
                    text: text.clone(),
                    words: words.clone(),
                    extra: Vec::new(),
                });
                continue;
            }
        }

        let mut words: Vec<(f64, f64, String)> = Vec::new();
        let mut extra = Vec::new();
        let mut main = String::new();
        let mut last = 0usize;

        for span in SPAN_RE.captures_iter(inner) {
            let whole = span.get(0).unwrap();
            // span 之间的文本节点（Apple 把词间空格放在这里）
            if whole.start() > last {
                let gap = plain_raw(&inner[last..whole.start()]);
                main.push_str(&gap);
                if let Some(w) = words.last_mut() {
                    w.2.push_str(&gap);
                }
            }
            last = whole.end();

            let sattrs = span.get(1).map(|m| m.as_str()).unwrap_or("");
            let sinner = span.get(2).map(|m| m.as_str()).unwrap_or("");
            let text = plain_raw(sinner);
            if text.trim().is_empty() {
                continue;
            }
            let role = attr(sattrs, "ttm:role")
                .or_else(|| attr(sattrs, "role"))
                .unwrap_or("");
            let sb = attr(sattrs, "begin").and_then(parse_time).or(begin);
            let se = attr(sattrs, "end").and_then(parse_time).or(end);
            if role.contains("translation")
                || role.contains("romanization")
                || role.contains("pronunciation")
            {
                extra.push(text.trim().to_string());
            } else {
                main.push_str(&text);
                words.push((sb.unwrap_or(0.0), se.unwrap_or(sb.unwrap_or(0.0)), text));
            }
        }
        if last < inner.len() {
            let tail = plain_raw(&inner[last..]);
            main.push_str(&tail);
            if let Some(w) = words.last_mut() {
                w.2.push_str(&tail);
            }
        }

        let text = if words.is_empty() {
            plain(inner)
        } else {
            main.trim().to_string()
        };
        if text.is_empty() && extra.is_empty() {
            continue;
        }
        let b = begin.or_else(|| words.first().map(|w| w.0)).unwrap_or(0.0);
        out.push(Line {
            begin: b,
            text,
            words,
            extra,
        });
    }
    out.sort_by(|a, b| {
        a.begin
            .partial_cmp(&b.begin)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// TTML → LRC。`syllable` 为真且歌词带逐字时间时输出增强型 LRC。
/// 没有时间轴的歌词只输出纯文本行。
pub fn ttml_to_lrc(ttml: &str, syllable: bool) -> String {
    let lines = parse_lines(ttml);
    if lines.is_empty() {
        return String::new();
    }
    let timed = lines.iter().any(|l| l.begin > 0.0);
    if !timed {
        return lines
            .into_iter()
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut out = String::new();
    for line in &lines {
        let words: Vec<_> = line
            .words
            .iter()
            .filter(|w| !w.2.trim().is_empty())
            .collect();
        let body = if syllable && words.len() > 1 {
            let mut s = String::new();
            for (b, _, t) in &words {
                s.push_str(&format!("<{}>{}", stamp(*b), t));
            }
            if let Some((_, e, _)) = words.last() {
                s.push_str(&format!("<{}>", stamp(*e)));
            }
            s
        } else {
            line.text.clone()
        };
        out.push_str(&format!("[{}]{}\n", stamp(line.begin), body));
        for e in &line.extra {
            out.push_str(&format!("[{}]{}\n", stamp(line.begin), e));
        }
    }
    out
}

/// TTML → 纯文本（嵌入标签用；保留时间戳时用 [`ttml_to_lrc`]）
pub fn ttml_to_plain(ttml: &str) -> String {
    parse_lines(ttml)
        .into_iter()
        .map(|l| l.text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<tt><body><div>
<p begin="12.345" end="15.5"><span begin="12.345" end="13.0">Hello </span><span begin="13.0" end="15.5">world</span></p>
<p begin="16.0" end="18.0">Second line</p>
</div></body></tt>"#;

    #[test]
    fn line_lrc() {
        let lrc = ttml_to_lrc(SAMPLE, false);
        assert!(lrc.contains("[00:12.34]Hello world"), "{lrc}");
        assert!(lrc.contains("[00:16.00]Second line"), "{lrc}");
    }

    #[test]
    fn syllable_lrc() {
        let lrc = ttml_to_lrc(SAMPLE, true);
        assert!(
            lrc.contains("[00:12.34]<00:12.34>Hello <00:13.00>world<00:15.50>"),
            "{lrc}"
        );
    }

    #[test]
    fn colon_minutes_and_spaces() {
        let ttml = r#"<tt><body><div><p begin="1:03.349" end="1:05.0"><span begin="1:03.349" end="1:04.0">You</span> <span begin="1:04.0" end="1:05.0">know</span></p></div></body></tt>"#;
        let lrc = ttml_to_lrc(ttml, false);
        assert!(lrc.contains("[01:03.34]You know"), "{lrc}");
        let syl = ttml_to_lrc(ttml, true);
        assert!(
            syl.contains("[01:03.34]<01:03.34>You <01:04.00>know<01:05.00>"),
            "{syl}"
        );
    }

    #[test]
    fn plain_text() {
        assert_eq!(ttml_to_plain(SAMPLE), "Hello world\nSecond line");
    }
}
