use rusqlite::Connection;
use serde::Serialize;
use std::time::Duration;

#[derive(Serialize, Clone, Debug)]
pub struct LyricLine {
    pub t: f64, // 秒；-1 表示无时间戳（纯文本）
    pub l: String,
    /// 翻译行（双语对照）：同时间戳第二行或外部 .zh.lrc 对齐
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tr: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LyricDoc {
    pub source: String, // lrc 文件 / 内嵌歌词 / LRCLIB（在线）/ 无
    pub synced: bool,
    pub lines: Vec<LyricLine>,
}

/// LRC 解析（Rust 侧，设计文档 8.3）：[mm:ss.xx] 前缀，支持一行多时间标签。
/// 双语支持：相同时间戳连续出现的行视为「原文 + 翻译」（A2 双行扩展）。
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut raw: Vec<(f64, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut times: Vec<f64> = Vec::new();
        let mut rest = line;
        loop {
            let t = rest.trim_start();
            if !t.starts_with('[') {
                break;
            }
            let close = match t.find(']') {
                Some(c) => c,
                None => break,
            };
            let tag = &t[1..close];
            // 时间标签 mm:ss.xx（mm 可为 2+ 位）
            let mut parts = tag.splitn(2, ':');
            let mm = parts.next().unwrap_or("");
            let ss = parts.next();
            if !mm.is_empty() && mm.chars().all(|c| c.is_ascii_digit()) {
                if let Some(ss) = ss {
                    let sec: f64 = match ss.trim().parse::<f64>() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    if let Ok(m) = mm.parse::<f64>() {
                        times.push(m * 60.0 + sec);
                    }
                }
            }
            rest = &t[close + 1..];
        }
        let text_part = rest.trim().to_string();
        // 纯 meta 行（[ti:] 等无剩余文本）跳过
        if text_part.is_empty() && times.is_empty() {
            continue;
        }
        if times.is_empty() {
            if !text_part.is_empty() {
                raw.push((-1.0, text_part));
            }
        } else {
            for t in times {
                raw.push((t, text_part.clone()));
            }
        }
    }
    // stable 排序保持出现顺序；相同时间戳的行分组：第 1 行原文，第 2 行翻译
    // （仅对带时间戳的行生效；纯文本行 t 恒为 -1，不能合并）
    raw.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<LyricLine> = Vec::with_capacity(raw.len());
    for (t, l) in raw {
        match out.last_mut() {
            Some(prev) if t >= 0.0 && prev.t == t && prev.tr.is_none() => prev.tr = Some(l),
            _ => out.push(LyricLine { t, l, tr: None }),
        }
    }
    out
}

/// 合并外部翻译文件（xxx.zh.lrc）：
/// - 翻译带时间戳 → 按时间就近对齐（容差 0.6s）
/// - 翻译为纯文本 → 行号 1:1 对齐（行数一致才应用）
fn attach_translation(lines: &mut [LyricLine], zh_text: &str) -> bool {
    let zh = parse_lrc(zh_text);
    if zh.is_empty() || lines.is_empty() {
        return false;
    }
    let mut applied = false;
    if zh.iter().all(|x| x.t < 0.0) {
        // 纯文本翻译：行号对齐
        if zh.len() == lines.len() {
            for (dst, src) in lines.iter_mut().zip(zh.iter()) {
                dst.tr = Some(src.l.clone());
            }
            applied = true;
        }
    } else {
        // 带时间戳：对每个原文行找时间最近的翻译行
        let mut zi = 0usize;
        let mut hit = 0usize;
        for dst in lines.iter_mut() {
            while zi + 1 < zh.len()
                && (zh[zi + 1].t - dst.t).abs() <= (zh[zi].t - dst.t).abs()
            {
                zi += 1;
            }
            if (zh[zi].t - dst.t).abs() <= 0.6 {
                dst.tr = Some(zh[zi].l.clone());
                hit += 1;
            }
        }
        // 至少对齐一行才算有翻译（否则会标"双语"却没有任何翻译行）
        applied = hit > 0;
    }
    applied
}

fn plain_lines(text: &str) -> Vec<LyricLine> {
    text.lines()
        .map(|l| LyricLine { t: -1.0, l: l.to_string(), tr: None })
        .collect()
}

/// 统一收尾：同文件双语（A2）标注 + 外部 .zh.lrc + 库内自定义源翻译缓存
fn finish(mut doc: LyricDoc, audio_path: &str, cached_tr: Option<String>) -> LyricDoc {
    if doc.lines.iter().any(|x| x.tr.is_some()) {
        doc.source.push_str(" · 双语");
    }
    let zh = std::path::Path::new(audio_path).with_extension("zh.lrc");
    if zh.exists() {
        if let Ok(text) = std::fs::read_to_string(&zh) {
            if attach_translation(&mut doc.lines, &text)
                && !doc.source.contains("双语")
            {
                doc.source.push_str(" · 双语");
            }
        }
    }
    if doc.lines.iter().all(|x| x.tr.is_none()) {
        if let Some(t) = cached_tr {
            if !t.trim().is_empty() && attach_translation(&mut doc.lines, &t) {
                doc.source.push_str(" · 双语（自定义源）");
            }
        }
    }
    doc
}

/// 歌词优先级（设计文档 4.2）：外部 .lrc → 内嵌（SYLT/USLT）→ LRCLIB 在线缓存
pub fn get_lyrics(conn: &Connection, track_id: i64) -> LyricDoc {
    let row: Option<(String, Option<String>, Option<String>, Option<String>)> = conn
        .prepare("SELECT file_path, lyrics_embed, lyrics_online, lyrics_tr FROM tracks WHERE id=?1")
        .unwrap()
        .query_map([track_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .next()
        .and_then(|x| x.ok());
    let (path, embed, online, cached_tr) = match row {
        Some(x) => x,
        None => {
            return LyricDoc { source: "无".into(), synced: false, lines: vec![] };
        }
    };
    // 1. 外部 .lrc
    let lrc = std::path::Path::new(&path).with_extension("lrc");
    if lrc.exists() {
        if let Ok(text) = std::fs::read_to_string(&lrc) {
            let lines = parse_lrc(&text);
            if !lines.is_empty() {
                let synced = lines.iter().any(|x| x.t >= 0.0);
                return finish(LyricDoc { source: "外部 .lrc".into(), synced, lines }, &path, cached_tr);
            }
        }
    }
    // 2. 内嵌
    if let Some(e) = embed {
        if !e.trim().is_empty() {
            let has_ts = e.contains("[") && parse_lrc(&e).iter().any(|x| x.t >= 0.0);
            if has_ts {
                return finish(
                    LyricDoc { source: "内嵌（带时间戳）".into(), synced: true, lines: parse_lrc(&e) },
                    &path,
                    cached_tr,
                );
            }
            return finish(
                LyricDoc { source: "内嵌歌词".into(), synced: false, lines: plain_lines(&e) },
                &path,
                cached_tr,
            );
        }
    }
    // 3. LRCLIB 缓存
    if let Some(o) = online {
        if !o.trim().is_empty() {
            let lines = parse_lrc(&o);
            if !lines.is_empty() {
                return finish(
                    LyricDoc { source: "LRCLIB（在线补全）".into(), synced: true, lines },
                    &path,
                    cached_tr,
                );
            }
        }
    }
    finish(LyricDoc { source: "无".into(), synced: false, lines: vec![] }, &path, cached_tr)
}

const UA: &str = "LyraPlayer/0.1.0 (local-first music player)";

#[derive(serde::Deserialize)]
struct LrcLibResp {
    #[serde(default)]
    syncedLyrics: Option<String>,
    #[serde(default)]
    plainLyrics: Option<String>,
}

/// LRCLIB 查询所需曲目信息
pub struct FetchInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
}

pub fn track_for_fetch(conn: &Connection, track_id: i64) -> Option<FetchInfo> {
    conn.query_row(
        "SELECT title, artist, album, duration FROM tracks WHERE id=?1",
        [track_id],
        |r| {
            Ok(FetchInfo {
                title: r.get(0)?,
                artist: r.get(1)?,
                album: r.get(2)?,
                duration: r.get(3)?,
            })
        },
    )
    .ok()
}

pub fn save_online_lyrics(conn: &Connection, track_id: i64, text: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE tracks SET lyrics_online=?1 WHERE id=?2",
        rusqlite::params![text, track_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn save_translation(conn: &Connection, track_id: i64, text: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE tracks SET lyrics_tr=?1 WHERE id=?2",
        rusqlite::params![text, track_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 重算并落库 has_tr 标记：完整走一遍 get_lyrics 判定（.zh.lrc / 内嵌 A2 /
/// 外部 .lrc 双语 / lyrics_tr 缓存 attach），与预览区"· 双语"口径 100% 一致。
/// 用于：预览懒更新、歌词/翻译保存后、扫描回填。
pub fn recompute_has_tr(conn: &Connection, track_id: i64) {
    let has = get_lyrics(conn, track_id).source.contains("双语");
    let _ = conn.execute(
        "UPDATE tracks SET has_tr=?1 WHERE id=?2",
        rusqlite::params![has as i64, track_id],
    );
}

#[derive(serde::Deserialize)]
struct TrResp {
    #[serde(default)]
    translation: Option<String>,
}

/// 自定义 HTTP 翻译源（用户在设置页自行配置，应用不内置任何具体服务）：
/// GET {base}/lrc-translation?artist=&title=&album=&duration=
/// → 200 { "translation": "<LRC 或纯文本，null 表示无>" }
pub async fn fetch_custom_translation(
    base: &str,
    info: &FetchInfo,
) -> Result<Option<String>, String> {
    let base = base.trim_end_matches('/');
    let url = format!(
        "{}/lrc-translation?artist={}&title={}&album={}&duration={}",
        base,
        urlencode(&info.artist),
        urlencode(&info.title),
        urlencode(&info.album),
        info.duration as i64
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("连接自定义源失败: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("自定义源返回 {}（检查适配脚本是否在运行）", resp.status()));
    }
    let body: TrResp = resp.json().await.map_err(|e| format!("解析响应失败: {e}"))?;
    Ok(body.translation.filter(|t| !t.trim().is_empty()))
}

/// 自定义源歌词（三合一适配协议）：GET {base}/lyrics?artist=&title=&album=&duration=
/// 响应 {"lrc": "...|null", "translation": "...|null"}——一次拿到带时间轴歌词 + 官方中文翻译
pub async fn fetch_custom_lyrics(info: &FetchInfo, base: &str) -> Result<Option<(String, Option<String>)>, String> {
    let url = format!(
        "{}/lyrics?artist={}&title={}&album={}&duration={}",
        base.trim_end_matches('/'),
        urlencode(&info.artist),
        urlencode(&info.title),
        urlencode(&info.album),
        info.duration as i64
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("自定义源请求失败: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None); // 源未收录（正常情况，非错误）
    }
    if !resp.status().is_success() {
        return Err(format!("自定义源返回 {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let lrc = body["lrc"].as_str().map(|s| s.to_string()).filter(|s| !s.trim().is_empty());
    let tr = body["translation"]
        .as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());
    Ok(lrc.map(|l| (l, tr)))
}

/// 自定义源封面（三合一适配协议）：GET {base}/cover?artist=&album=
/// 响应 {"url": "https://..."} 或 {"base64": "..."} 或 404
pub async fn fetch_custom_cover(artist: &str, album: &str, base: &str) -> Result<Vec<String>, String> {
    let url = format!(
        "{}/cover?artist={}&album={}",
        base.trim_end_matches('/'),
        urlencode(artist),
        urlencode(album)
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("自定义源请求失败: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        return Err(format!("自定义源返回 {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(u) = body["url"].as_str() {
        out.push(u.to_string());
    }
    if let Some(b) = body["base64"].as_str() {
        out.push(format!("data:image/jpeg;base64,{}", b));
    }
    Ok(out)
}

/// LRCLIB 精确签名查询：/api/get（免 key，要求 UA）
pub async fn fetch_lrclib(info: &FetchInfo) -> Result<String, String> {
    let url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}&album_name={}&duration={}",
        urlencode(&info.artist),
        urlencode(&info.title),
        urlencode(&info.album),
        info.duration as i64
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Err("LRCLIB 未收录该曲目（可尝试手动修正标题 / 艺人后再试）".into());
    }
    if !resp.status().is_success() {
        return Err(format!("LRCLIB 返回 {}", resp.status()));
    }
    let body: LrcLibResp = resp.json().await.map_err(|e| e.to_string())?;
    let text = body
        .syncedLyrics
        .filter(|s| !s.trim().is_empty())
        .or(body.plainLyrics.filter(|s| !s.trim().is_empty()))
        .ok_or("LRCLIB 响应中无歌词")?;
    Ok(text)
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 供应商连通性实测（设置页 5.5）：对 LRCLIB 发一次真实搜索请求
pub async fn test_lrclib() -> Result<serde_json::Value, String> {
    let url = "https://lrclib.net/api/search?q=hello&limit=1";
    let start = std::time::Instant::now();
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status().as_u16();
    let hits: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
    Ok(serde_json::json!({
        "status": status,
        "latencyMs": start.elapsed().as_millis() as u64,
        "hits": hits.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_line_lrc() {
        // 同时间戳双行（A2 双语扩展）：第二行为翻译
        let text = "[00:10.00]Hello world\n[00:10.00]你好世界\n[00:20.00]Second line\n[00:20.00]第二行\n";
        let lines = parse_lrc(text);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].l, "Hello world");
        assert_eq!(lines[0].tr.as_deref(), Some("你好世界"));
        assert_eq!(lines[1].tr.as_deref(), Some("第二行"));
    }

    #[test]
    fn translation_by_time() {
        let mut base = vec![
            LyricLine { t: 1.0, l: "one".into(), tr: None },
            LyricLine { t: 5.0, l: "two".into(), tr: None },
        ];
        let zh = "[00:01.02]一\n[00:05.00]二\n";
        assert!(attach_translation(&mut base, zh));
        assert_eq!(base[0].tr.as_deref(), Some("一"));
        assert_eq!(base[1].tr.as_deref(), Some("二"));
    }

    #[test]
    fn translation_by_lineno() {
        let mut base = vec![
            LyricLine { t: -1.0, l: "a".into(), tr: None },
            LyricLine { t: -1.0, l: "b".into(), tr: None },
        ];
        assert!(attach_translation(&mut base, "甲\n乙\n"));
        assert_eq!(base[0].tr.as_deref(), Some("甲"));
        assert_eq!(base[1].tr.as_deref(), Some("乙"));
    }
}


/// LRCLIB 多候选搜索（歌词选择面板用）：返回前 8 条（含正文，选中即用）
pub async fn lrclib_search(info: &FetchInfo) -> Result<Vec<serde_json::Value>, String> {
    use std::time::Duration;
    let url = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        urlencode(&info.title), urlencode(&info.artist)
    );
    let resp = reqwest::Client::builder()
        .user_agent("Lyra/0.1 (local-first music player)")
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("LRCLIB 请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("LRCLIB 返回 {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for x in arr.iter().take(8) {
            let dur = x["duration"].as_f64().unwrap_or(0.0);
            let dur_ok = info.duration <= 0.0 || (dur > 0.0 && (dur - info.duration).abs() <= 5.0);
            let body = x["syncedLyrics"].as_str().filter(|s| !s.is_empty())
                .or(x["plainLyrics"].as_str()).unwrap_or("");
            // 正文是否自带双语（A2 同时间戳双行）：候选列表"译"徽标
            let has_tr = parse_lrc(body).iter().any(|l| l.tr.is_some());
            out.push(serde_json::json!({
                "id": x["id"].as_i64().unwrap_or(0),
                "title": x["trackName"].as_str().unwrap_or(""),
                "artist": x["artistName"].as_str().unwrap_or(""),
                "album": x["albumName"].as_str().unwrap_or(""),
                "duration": dur as i64,
                "synced": x["syncedLyrics"].as_str().is_some(),
                "dur_ok": dur_ok,
                "has_tr": has_tr,
                "body": body,
            }));
        }
    }
    Ok(out)
}
