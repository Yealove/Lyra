//! MusicBrainz 在线元数据补全（设计文档 5.3/5.4 首发链的文本匹配部分）
//! 官方开放 API（CC0 数据），无 key，强制自定义 UA，严格 1 req/s。

use rusqlite::Connection;
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UA: &str = "LyraPlayer/0.1.0 (local-first music player; https://localhost/lyra)";
const MB_ENDPOINT: &str = "https://musicbrainz.org/ws/2/recording";

/// 全局限流（MusicBrainz 超 1 req/s 会封 IP）
static LAST_REQ: Mutex<Option<Instant>> = Mutex::new(None);

async fn rate_limit() {
    loop {
        let wait = {
            let mut g = LAST_REQ.lock().unwrap();
            match *g {
                Some(t) => {
                    let elapsed = t.elapsed();
                    if elapsed >= Duration::from_millis(1100) {
                        *g = Some(Instant::now());
                        return;
                    }
                    Duration::from_millis(1100) - elapsed
                }
                None => {
                    *g = Some(Instant::now());
                    return;
                }
            }
        };
        tokio::time::sleep(wait).await;
    }
}

#[derive(Serialize, serde::Deserialize, Clone, Debug)]
pub struct Candidate {
    pub provider: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub duration: f64,
    pub mbid: String,
    pub conf: f64,
}

/// iTunes Search 封面查询（免 key）：按 艺人+专辑 搜专辑 artwork，返回候选 URL（600px）
pub async fn search_album_art(artist: &str, album: &str) -> Result<Vec<String>, String> {
    let term = format!("{} {}", artist, album);
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=album&limit=6",
        urlencode(&term)
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("iTunes 请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("iTunes 返回 {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let urls = body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["artworkUrl100"].as_str())
                .map(|u| u.replace("100x100bb", "600x600bb"))
                .collect()
        })
        .unwrap_or_default();
    Ok(urls)
}

#[derive(serde::Deserialize)]
struct MbResp {
    #[serde(default)]
    recordings: Vec<MbRecording>,
}

#[derive(serde::Deserialize)]
struct MbRecording {
    id: String,
    title: String,
    #[serde(default)]
    length: Option<u64>, // 毫秒
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<MbArtistCredit>,
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(serde::Deserialize)]
struct MbArtistCredit {
    name: String,
}

#[derive(serde::Deserialize)]
struct MbRelease {
    title: String,
    #[serde(default)]
    date: Option<String>,
}

/// 归一化：小写、去标点空白（用于相似度）
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (norm(a), norm(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb || na.contains(&nb) || nb.contains(&na) {
        return 1.0;
    }
    let d = levenshtein(&na, &nb) as f64;
    1.0 - d / na.len().max(nb.len()) as f64
}

/// 候选打分（设计文档 5.4）：时长差 + 标题/艺人相似度；Live/现场版惩罚
fn score(local_title: &str, local_artist: &str, local_dur: f64, rec: &MbRecording) -> f64 {
    let title_sim = similarity(local_title, &rec.title);
    let artist = rec
        .artist_credit
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let artist_sim = similarity(local_artist, &artist);
    let dur_score = match rec.length {
        Some(ms) => {
            let d = (ms as f64 / 1000.0 - local_dur).abs();
            if local_dur <= 0.0 {
                0.5
            } else if d <= 2.0 {
                1.0
            } else if d <= 5.0 {
                0.7
            } else if d <= 10.0 {
                0.4
            } else {
                0.0
            }
        }
        None => 0.5,
    };
    let base = 0.45 * dur_score + 0.35 * title_sim + 0.20 * artist_sim;
    // 现场/演唱会版本惩罚：本地标题无 live 字样而候选是 Live/Concert/Tour 版 → 降权
    // （避免录音室版本被误匹配到现场录音）
    let nt = norm(local_title);
    let local_live = nt.contains("live") || nt.contains("演唱会") || nt.contains("现场");
    let ct = norm(&rec.title);
    let cand_live = ct.contains("live") || ct.contains("concert") || ct.contains("tour");
    let penalty = if cand_live && !local_live { 0.85 } else { 1.0 };
    (base * penalty).clamp(0.0, 1.0)
}

/// 查询 MusicBrainz 并返回按置信度排序的候选
pub async fn search_candidates(title: &str, artist: &str, album: &str, duration: f64) -> Result<Vec<Candidate>, String> {
    rate_limit().await;
    let q = if artist.is_empty() || artist == "未知艺术家" {
        format!("recording:\"{}\"", title.replace('"', ""))
    } else {
        format!(
            "recording:\"{}\" AND artist:\"{}\"",
            title.replace('"', ""),
            artist.replace('"', "")
        )
    };
    let _ = album;
    let url = format!("{}?query={}&fmt=json&limit=3", MB_ENDPOINT, urlencode(&q));
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("MusicBrainz 请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("MusicBrainz 返回 {}", resp.status()));
    }
    let body: MbResp = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let mut out: Vec<Candidate> = body
        .recordings
        .iter()
        .map(|r| {
            let artist = r
                .artist_credit
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let rel = r.releases.first();
            let conf = score(title, &artist, duration, r);
            Candidate {
                provider: "MusicBrainz".into(),
                title: r.title.clone(),
                artist,
                album: rel.map(|x| x.title.clone()),
                year: rel
                    .and_then(|x| x.date.as_deref())
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse::<i64>().ok()),
                duration: r.length.unwrap_or(0) as f64 / 1000.0,
                mbid: r.id.clone(),
                conf,
            }
        })
        .collect();
    out.sort_by(|a, b| b.conf.partial_cmp(&a.conf).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

/// 应用候选到曲目。
/// fields=None（自动应用）：仅覆盖无效/缺失字段，已有正确标签保留。
/// fields=Some（手动勾选）：勾选的字段强制用候选值（即使本地字段有效），未勾选保持本地。
/// 来源标记 online，MBID 存证。
pub fn apply_candidate(
    conn: &Connection,
    track_id: i64,
    c: &Candidate,
    status: &str,
    fields: Option<&[String]>,
) -> Result<(), String> {
    let meta = serde_json::json!({
        "provider": c.provider,
        "mbid": c.mbid,
        "conf": c.conf,
    })
    .to_string();

    if let Some(fields) = fields {
        // 手动模式：读现值 → 勾选字段替换 → 写回
        let (t0, a0, al0, y0): (String, String, String, Option<i64>) = conn
            .query_row(
                "SELECT title, artist, album, year FROM tracks WHERE id=?1",
                [track_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| e.to_string())?;
        let want = |k: &str| fields.iter().any(|x| x == k);
        let title = if want("title") { c.title.clone() } else { t0 };
        let artist = if want("artist") { c.artist.clone() } else { a0 };
        let album = match c.album.as_ref() {
            Some(v) if want("album") => v.clone(),
            _ => al0,
        };
        let year = match c.year {
            Some(v) if want("year") => Some(v),
            _ => y0,
        };
        conn.execute(
            "UPDATE tracks SET title=?2, artist=?3, album=?4, year=?5,
             duration=CASE WHEN duration<=0 THEN ?6 ELSE duration END,
             source='online', enrichment_status=?7, online_meta=?8 WHERE id=?1",
            rusqlite::params![track_id, title, artist, album, year, c.duration, status, meta],
        )
        .map_err(|e| e.to_string())?;
        // 改过专辑/艺人 → 同步重算专辑键，否则会留在旧专辑下
        crate::db::refresh_album_key(conn, track_id).ok();
        return Ok(());
    }

    // 自动模式：仅覆盖无效/缺失字段
    conn.execute(
        "UPDATE tracks SET
           title=CASE WHEN title='' OR title LIKE 'Unknown%' THEN ?2 ELSE title END,
           artist=CASE WHEN artist='' OR artist='未知艺术家' THEN ?3 ELSE artist END,
           album=CASE WHEN album='' OR album='未命名专辑' THEN COALESCE(?4, album) ELSE album END,
           year=COALESCE(?5, year),
           duration=CASE WHEN duration<=0 THEN ?6 ELSE duration END,
           source='online', enrichment_status=?7, online_meta=?8
         WHERE id=?1",
        rusqlite::params![
            track_id,
            c.title,
            c.artist,
            c.album,
            c.year,
            c.duration,
            status,
            meta
        ],
    )
    .map_err(|e| e.to_string())?;
    crate::db::refresh_album_key(conn, track_id).ok();
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sim() {
        assert!(similarity("Hello World", "hello world!") > 0.99);
        assert!(similarity("abc", "xyz") < 0.2);
    }
}


/// Deezer 专辑封面（免 key）：搜索接口拿 album.cover_big/xl
pub async fn search_deezer_art(artist: &str, album: &str) -> Result<Vec<String>, String> {
    let q = format!("artist:\"{}\" album:\"{}\"", artist, album);
    let url = format!("https://api.deezer.com/search?q={}&limit=6", urlencode(&q));
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Deezer 请求失败: {e}"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut urls: Vec<String> = Vec::new();
    if let Some(items) = v["data"].as_array() {
        for it in items {
            for k in ["cover_xl", "cover_big"] {
                if let Some(u) = it["album"][k].as_str() {
                    if !u.is_empty() && !urls.iter().any(|x| x == u) {
                        urls.push(u.to_string());
                    }
                }
            }
        }
    }
    Ok(urls)
}

/// Deezer 歌手图片（免 key）：picture_xl 640px
pub async fn deezer_artist_img(name: &str) -> Option<String> {
    let url = format!("https://api.deezer.com/search/artist?q={}&limit=1", urlencode(name));
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let u = v["data"][0]["picture_xl"].as_str()?.to_string();
    if u.is_empty() { None } else { Some(u) }
}

/// Wikipedia 歌手简介（免 key，中文优先，英文兜底）
pub async fn wiki_intro(name: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    for lang in ["zh", "en"] {
        // 第一步：搜索准确词条名
        let search_url = format!(
            "https://{}.wikipedia.org/w/api.php?action=query&format=json&list=search&srlimit=1&srsearch={}",
            lang,
            urlencode(name)
        );
        let Ok(v) = (client.get(&search_url).send().await.ok()?.json::<serde_json::Value>().await) else { continue };
        let title = v["query"]["search"][0]["title"].as_str()?.to_string();
        // 第二步：取 intro 纯文本（zh 站指定 zh-cn 变体，服务器端繁→简转换）
        let ext_url = format!(
            "https://{}.wikipedia.org/w/api.php?action=query&format=json&prop=extracts&exintro=1&explaintext=1&variant=zh-cn&titles={}",
            lang,
            urlencode(&title)
        );
        let Ok(v) = (client.get(&ext_url).send().await.ok()?.json::<serde_json::Value>().await) else { continue };
        if let Some(pages) = v["query"]["pages"].as_object() {
            for (_, pg) in pages {
                if let Some(ex) = pg["extract"].as_str() {
                    // 双保险：服务器转换 + 本地 OpenCC 词表再过一遍
                    let ex = crate::zhconv::t2s(ex.trim());
                    if ex.len() >= 40 {
                        return Some(ex.split("。").take(3).collect::<Vec<_>>().join("。") + "。");
                    }
                }
            }
        }
    }
    None
}

/// MusicBrainz 歌手信息（免 key，1 req/s 严格限流）：类型/国家/活跃年代
pub async fn mb_artist_meta(name: &str) -> Option<String> {
    let url = format!(
        "https://musicbrainz.org/ws/2/artist?fmt=json&limit=1&query=artist:%22{}%22",
        urlencode(name).replace("%20", "+")
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let a = v["artists"][0].as_object()?;
    // 中文名文化（type/country 常见值映射）
    let type_cn = |t: &str| match t {
        "Person" => "歌手", "Group" => "组合", "Orchestra" => "管弦乐团", "Choir" => "合唱团",
        "Character" => "虚拟角色", "Other" => "其他", _ => t,
    }.to_string();
    let country_cn = |c: &str| match c {
        "TW" => "台湾", "CN" => "中国大陆", "HK" => "香港", "MO" => "澳门", "JP" => "日本", "KR" => "韩国",
        "US" => "美国", "GB" => "英国", "CA" => "加拿大", "AU" => "澳大利亚", "FR" => "法国", "DE" => "德国",
        "IT" => "意大利", "ES" => "西班牙", "SE" => "瑞典", "NO" => "挪威", "DK" => "丹麦", "FI" => "芬兰",
        "NL" => "荷兰", "BE" => "比利时", "AT" => "奥地利", "CH" => "瑞士", "RU" => "俄罗斯", "BR" => "巴西",
        "IN" => "印度", "TH" => "泰国", "SG" => "新加坡", "MY" => "马来西亚", "VN" => "越南", "PH" => "菲律宾",
        _ => c,
    }.to_string();
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = a["type"].as_str() { parts.push(type_cn(t)); }
    if let Some(c) = a["country"].as_str() { parts.push(country_cn(c)); }
    if let Some(b) = a["life-span"]["begin"].as_str() {
        let b = &b[..b.len().min(4)];
        let end = a["life-span"]["end"].as_str().map(|e| &e[..e.len().min(4)]);
        parts.push(match end { Some(e) => format!("{} 年–{} 年", b, e), None => format!("{} 年–", b) });
    }
    if parts.is_empty() { None } else { Some(parts.join(" · ")) }
}


/// Last.fm 专辑封面（需免费 key）：album.search → extralarge 图
pub async fn search_lastfm_art(artist: &str, album: &str, key: &str) -> Result<Vec<String>, String> {
    let url = format!(
        "https://ws.audioscrobbler.com/2.0/?method=album.search&limit=6&format=json&api_key={}&album={}&artist={}",
        key, urlencode(album), urlencode(artist)
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Last.fm 请求失败: {e}"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if let Some(e) = v["error"].as_i64() {
        return Err(format!("Last.fm 错误 {}: {}", e, v["message"].as_str().unwrap_or("")));
    }
    let mut urls: Vec<String> = Vec::new();
    if let Some(albums) = v["results"]["albummatches"]["album"].as_array() {
        for a in albums {
            if let Some(imgs) = a["image"].as_array() {
                for img in imgs {
                    if img["size"].as_str() == Some("extralarge") {
                        if let Some(u) = img["#text"].as_str() {
                            if !u.is_empty() && !urls.iter().any(|x| x == u) {
                                urls.push(u.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(urls)
}

/// MusicBrainz 歌手 mbid（fanart 查询依赖）
pub async fn mb_artist_mbid(name: &str) -> Option<String> {
    let url = format!(
        "https://musicbrainz.org/ws/2/artist?fmt=json&limit=1&query=artist:%22{}%22",
        urlencode(name).replace("%20", "+")
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v["artists"][0]["id"].as_str().map(|s| s.to_string())
}

/// fanart.tv 歌手图集（需免费 key + mbid）：artistthumb 全部 + artistimage 首张
pub async fn fanart_artist_imgs(mbid: &str, key: &str) -> Vec<String> {
    let url = format!("https://webservice.fanart.tv/v3/music/{}?api_key={}", mbid, key);
    let Ok(resp) = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .and_then(|c| Ok(c))
        .unwrap_or_else(|_| unreachable!())
        .get(&url)
        .send()
        .await
    else { return vec![]; };
    let Ok(v) = resp.json::<serde_json::Value>().await else { return vec![]; };
    let mut out: Vec<String> = Vec::new();
    if let Some(arr) = v["artistthumb"].as_array() {
        for x in arr {
            if let Some(u) = x["url"].as_str() {
                if !u.is_empty() { out.push(u.to_string()); }
            }
        }
    }
    for k in ["artistimage", "hdmusiclogo"] {
        if let Some(u) = v[k][0]["url"].as_str() {
            if !u.is_empty() { out.push(u.to_string()); }
        }
    }
    out
}

/// Deezer 歌手图集：picture_xl + picture_big
pub async fn deezer_artist_imgs(name: &str) -> Vec<String> {
    let url = format!("https://api.deezer.com/search/artist?q={}&limit=1", urlencode(name));
    let Ok(resp) = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()
        .and_then(|c| Some(c))
        .unwrap_or_else(|| unreachable!())
        .get(&url)
        .send()
        .await
    else { return vec![]; };
    let Ok(v) = resp.json::<serde_json::Value>().await else { return vec![]; };
    let mut out: Vec<String> = Vec::new();
    for k in ["picture_xl", "picture_big"] {
        if let Some(u) = v["data"][0][k].as_str() {
            // Deezer 默认占位图特征：hash 为空（/images/artist//xxx.jpg）——过滤
            if !u.is_empty() && !u.contains("/images/artist//") && !out.iter().any(|x| x == u) {
                out.push(u.to_string());
            }
        }
    }
    out
}

/// Last.fm key 验证
pub async fn test_lastfm(key: &str) -> Result<serde_json::Value, String> {
    let url = format!(
        "https://ws.audioscrobbler.com/2.0/?method=album.search&limit=1&format=json&api_key={}&album=fantasy",
        key
    );
    let resp = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(8)).build()
        .map_err(|e| e.to_string())?.get(&url).send().await
        .map_err(|e| format!("请求失败: {e}"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if v["error"].is_number() {
        return Err(format!("Key 无效: {}", v["message"].as_str().unwrap_or("")));
    }
    Ok(serde_json::json!({ "ok": true, "detail": "Key 有效" }))
}

/// fanart.tv key 验证
pub async fn test_fanart(key: &str) -> Result<serde_json::Value, String> {
    // 周杰伦 mbid（固定测试，避免依赖 MB 查询限流）
    let url = format!("https://webservice.fanart.tv/v3/music/6a29409d-4b77-4c6d-9bb4-7f0b9d3f2d21?api_key={}", key);
    let resp = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(8)).build()
        .map_err(|e| e.to_string())?.get(&url).send().await
        .map_err(|e| format!("请求失败: {e}"))?;
    if resp.status().as_u16() == 403 {
        return Err("Key 无效（403）".into());
    }
    Ok(serde_json::json!({ "ok": true, "detail": "Key 有效" }))
}

/// Discogs token 验证
pub async fn test_discogs(key: &str) -> Result<serde_json::Value, String> {
    let url = format!("https://api.discogs.com/database/search?q=test&limit=1&token={}", key);
    let resp = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(8)).build()
        .map_err(|e| e.to_string())?.get(&url).send().await
        .map_err(|e| format!("请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Token 无效（{}）", resp.status()));
    }
    Ok(serde_json::json!({ "ok": true, "detail": "Token 有效" }))
}


/// Last.fm 歌手聚合（需 key）：多尺寸图 + 简介 + 标签 + 相似歌手 + 热度
pub async fn lastfm_artist_pack(name: &str, key: &str) -> Option<serde_json::Value> {
    let url = format!(
        "https://ws.audioscrobbler.com/2.0/?method=artist.getinfo&format=json&api_key={}&artist={}",
        key, urlencode(name)
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    if v["error"].is_number() { return None; }
    let a = &v["artist"];
    // 多尺寸图（去重，大的在前）
    let mut imgs: Vec<String> = Vec::new();
    // Last.fm 官方默认占位图 hash（灰色人影），过滤掉
    const LFM_PLACEHOLDER: &str = "2a96cbd8b46e442fc41c2b86b84656b9";
    for size in ["mega", "extralarge", "large", "medium"] {
        if let Some(arr) = a["image"].as_array() {
            for im in arr {
                if im["size"].as_str() == Some(size) {
                    if let Some(u) = im["#text"].as_str() {
                        if !u.is_empty() && !u.contains(LFM_PLACEHOLDER) && !imgs.iter().any(|x| x == u) {
                            imgs.push(u.to_string());
                        }
                    }
                }
            }
        }
    }
    // 简介（strip HTML 标签）
    let bio = a["bio"]["summary"].as_str().map(|s| {
        let mut out = String::new();
        let mut in_tag = false;
        for c in s.chars() {
            match c { '<' => in_tag = true, '>' => in_tag = false, _ if !in_tag => out.push(c), _ => {} }
        }
        out.trim().trim_end_matches("Read more on Last.fm.").trim().to_string()
    }).filter(|s| !s.is_empty());
    // 标签 / 相似歌手 / 热度
    let tags = a["tags"]["tag"].as_array().map(|arr| {
        arr.iter().filter_map(|t| t["name"].as_str()).take(6).collect::<Vec<_>>().join(" / ")
    }).filter(|s| !s.is_empty());
    let similar = a["similar"]["artist"].as_array().map(|arr| {
        arr.iter().filter_map(|t| t["name"].as_str()).take(6).collect::<Vec<_>>().join("、")
    }).filter(|s| !s.is_empty());
    let listeners = a["stats"]["listeners"].as_str().map(|s| {
        format!("{} 听众", s.chars().rev().collect::<Vec<_>>().chunks(3).map(|c| c.iter().rev().collect::<String>()).collect::<Vec<_>>().join(","))
    });
    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(t) = &tags { meta_parts.push(format!("标签：{}", t)); }
    if let Some(s) = &similar { meta_parts.push(format!("相似：{}", s)); }
    if let Some(l) = &listeners { meta_parts.push(l.clone()); }
    let meta = if meta_parts.is_empty() { None } else { Some(meta_parts.join(" · ")) };
    if imgs.is_empty() && bio.is_none() && meta.is_none() { return None; }
    Some(serde_json::json!({ "imgs": imgs, "bio": bio, "meta": meta }))
}


/// Wikipedia 歌手肖像（词条 lead image，免 key，中文优先）：真实官方照片
pub async fn wiki_artist_img(name: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    for lang in ["zh", "en"] {
        let search_url = format!(
            "https://{}.wikipedia.org/w/api.php?action=query&format=json&list=search&srlimit=1&srsearch={}",
            lang, urlencode(name)
        );
        let Ok(v) = (client.get(&search_url).send().await.ok()?.json::<serde_json::Value>().await) else { continue };
        let title = v["query"]["search"][0]["title"].as_str()?.to_string();
        let img_url = format!(
            "https://{}.wikipedia.org/w/api.php?action=query&format=json&prop=pageimages&piprop=original|thumbnail&pithumbnailsize=640&titles={}",
            lang, urlencode(&title)
        );
        let Ok(v) = (client.get(&img_url).send().await.ok()?.json::<serde_json::Value>().await) else { continue };
        if let Some(pages) = v["query"]["pages"].as_object() {
            for (_, pg) in pages {
                // original（原图）优先，thumbnail（640px）兜底
                for k in ["original", "thumbnail"] {
                    if let Some(u) = pg[k]["source"].as_str() {
                        if u.starts_with("http") && !u.contains(".svg") && !u.contains(".ogg") {
                            return Some(u.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}


/// TheAudioDB 歌手聚合（行业标准源，Kodi 系；key 需 Patreon 赞护，免费测试 key 已停）
/// 返回：多尺寸图 + 中文简介（strBiographyCN）+ EN 简介 + 流派/年代
pub async fn theaudiodb_artist(key: &str, name: &str) -> Option<serde_json::Value> {
    let url = format!(
        "https://www.theaudiodb.com/api/v1/json/{}/search.php?s={}",
        key, urlencode(name)
    );
    let resp = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() { return None; }
    let v: serde_json::Value = resp.json().await.ok()?;
    let a = v["artists"][0].as_object()?;
    let mut imgs: Vec<String> = Vec::new();
    for k in ["strArtistThumb", "strArtistFanart", "strArtistLogo"] {
        if let Some(u) = a.get(k).and_then(|x| x.as_str()) {
            if u.starts_with("http") { imgs.push(u.to_string()); }
        }
    }
    let bio_cn = a.get("strBiographyCN").and_then(|x| x.as_str()).filter(|s| !s.is_empty());
    let bio_en = a.get("strBiographyEN").and_then(|x| x.as_str()).filter(|s| !s.is_empty());
    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(g) = a.get("strGenre").and_then(|x| x.as_str()) { if !g.is_empty() { meta_parts.push(format!("流派：{}", g)); } }
    if let Some(y) = a.get("intFormedYear").and_then(|x| x.as_str()) { if y != "0" { meta_parts.push(format!("{} 年成立", y)); } }
    if let Some(c) = a.get("strCountry").and_then(|x| x.as_str()) { if !c.is_empty() { meta_parts.push(c.to_string()); } }
    if imgs.is_empty() && bio_cn.is_none() && bio_en.is_none() && meta_parts.is_empty() { return None; }
    Some(serde_json::json!({
        "imgs": imgs,
        "bio": bio_cn.or(bio_en),
        "bio_is_cn": bio_cn.is_some(),
        "meta": if meta_parts.is_empty() { serde_json::Value::Null } else { serde_json::json!(meta_parts.join(" · ")) },
    }))
}


/// iTunes 曲目搜索（实体=单曲）：(id, title, artist, album, year, dur_sec)
pub async fn itunes_songs(artist: &str, title: &str) -> Result<Vec<(String, String, String, String, i64, i64)>, String> {
    // artist 可能为空（占位艺人已清洗）：只用歌名搜
    let term = if artist.is_empty() { title.to_string() } else { format!("{} {}", artist, title) };
    let url = format!("https://itunes.apple.com/search?term={}&entity=song&limit=8", urlencode(&term));
    let resp = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(8)).build()
        .map_err(|e| e.to_string())?.get(&url).send().await
        .map_err(|e| format!("iTunes 请求失败: {e}"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(arr) = v["results"].as_array() {
        for r in arr {
            let year = r["releaseDate"].as_str().and_then(|s| s.get(0..4)).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            out.push((
                r["trackId"].as_i64().map(|x| x.to_string()).unwrap_or_default(),
                r["trackName"].as_str().unwrap_or("").to_string(),
                r["artistName"].as_str().unwrap_or("").to_string(),
                r["collectionName"].as_str().unwrap_or("").to_string(),
                year,
                (r["trackTimeMillis"].as_i64().unwrap_or(0)) / 1000,
            ));
        }
    }
    Ok(out)
}

/// Deezer 曲目搜索：(id, title, artist, album, year, dur_sec)
pub async fn deezer_songs(artist: &str, title: &str) -> Result<Vec<(String, String, String, String, i64, i64)>, String> {
    let q = if artist.is_empty() { title.to_string() } else { format!("{} {}", artist, title) };
    let url = format!("https://api.deezer.com/search?q={}&limit=8", urlencode(&q));
    let resp = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(8)).build()
        .map_err(|e| e.to_string())?.get(&url).send().await
        .map_err(|e| format!("Deezer 请求失败: {e}"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(arr) = v["data"].as_array() {
        for r in arr {
            let year = r["album"]["release_date"].as_str().and_then(|s| s.get(0..4)).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            out.push((
                r["id"].as_i64().map(|x| x.to_string()).unwrap_or_default(),
                r["title"].as_str().unwrap_or("").to_string(),
                r["artist"]["name"].as_str().unwrap_or("").to_string(),
                r["album"]["title"].as_str().unwrap_or("").to_string(),
                year,
                (r["duration"].as_i64().unwrap_or(0)),
            ));
        }
    }
    Ok(out)
}
