use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};
use rusqlite::Connection;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauri::Emitter;

const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "ogg", "oga", "m4a", "aac", "opus", "aiff", "aif", "alac", "wma"];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
                if AUDIO_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                    out.push(p);
                }
            }
        }
    }
}

/// 标题前缀噪声剥离："01. " / "03 - " / "1_" 等（设计文档 4.2 清洗规则，白名单保护：带字母前缀不剥）
pub fn clean_title(t: &str) -> String {
    let mut s = t.trim();
    loop {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || i > 2 || i >= bytes.len() {
            break;
        }
        let rest = &s[i..];
        let trimmed = rest.trim_start_matches(['.', '．', '-', '–', '_', ' ', '\t']);
        if trimmed.len() < rest.len() && !trimmed.is_empty() {
            s = trimmed.trim_start();
        } else {
            break;
        }
    }
    s.to_string()
}

fn parse_frac(s: Option<&str>) -> (Option<i64>, Option<i64>) {
    // "3/12" → (3, 12)；"3" → (3, None)
    match s {
        None => (None, None),
        Some(v) => {
            let mut it = v.split('/');
            let a = it.next().and_then(|x| x.trim().parse::<i64>().ok());
            let b = it.next().and_then(|x| x.trim().parse::<i64>().ok());
            (a, b)
        }
    }
}

struct Meta {
    title: String,
    artist: String,
    album_artist: String,
    album: String,
    disc: i64,
    disc_total: Option<i64>,
    track_no: Option<i64>,
    track_total: Option<i64>,
    year: Option<i64>,
    genre: Option<String>,
    duration: f64,
    source: String, // tag | cleaned | guessed
    lyrics: Option<String>,
    cover: Option<String>, // 缓存文件绝对路径
    cover_src: Option<String>, // dir（目录图，优先）| embed（歌曲内嵌）
    rg_track: Option<f64>, // None=无标签（落库 0.0 表示已解析）
    rg_album: Option<f64>,
}

fn extract_cover(tag: &Tag, src_path: &Path, covers: &Path) -> Option<String> {
    let pic = tag.pictures().first()?;
    let mut h = DefaultHasher::new();
    pic.data().hash(&mut h);
    let ext = match pic.mime_type() {
        Some(lofty::picture::MimeType::Png) => "png",
        _ => "jpg",
    };
    let name = format!("{:016x}.{}", h.finish(), ext);
    let path = covers.join(&name);
    if !path.exists() {
        std::fs::write(&path, pic.data()).ok()?;
    }
    let _ = src_path;
    Some(path.to_string_lossy().into_owned())
}

/// 目录级封面探测（设计文档 4.2，优先级高于内嵌封面——「一张图管一张专辑」，
/// 保证同目录曲目封面天然一致）：探测曲目所在目录的 cover.jpg / folder.jpg 等常见命名
/// （大小写不敏感），并放宽到关键词匹配（如「周杰伦.八度空间封面.jpg」这类中文命名），
/// 排除封底/CD 面/内页等非封面图片。命中后按内容 hash 复制进 covers 缓存；
/// memo 保证同一目录只探测一次。
fn dir_cover(dir: &Path, covers: &Path, memo: &mut HashMap<PathBuf, Option<String>>) -> Option<String> {
    if let Some(v) = memo.get(dir) {
        return v.clone();
    }
    const NAMES: &[&str] = &[
        "cover.jpg", "cover.jpeg", "cover.png", "folder.jpg", "folder.png",
        "front.jpg", "front.png", "album.jpg", "album.png", "albumart.jpg", "artwork.jpg",
    ];
    // 关键词放宽：文件名包含封面语义词即候选（真实案例：周杰伦.八度空间封面.jpg）
    const KEYWORDS: &[&str] = &["cover", "front", "folder", "album", "artwork", "封面"];
    // 非封面图排除：封底 / CD 面 / 内页 / 歌手照 / 黑胶 / 书脊 / 光盘盒
    const EXCLUDES: &[&str] = &["back", "封底", "disc", "inlay", "booklet", "cd", "artist", "vinyl", "spine", "tray", "inside", "log"];
    // 先精确匹配小写命名（最常见，零 read_dir 成本），未命中再列目录：
    // 精确命中 > 关键词命中（组内按文件名排序取最稳定的一个）
    let found: Option<PathBuf> = NAMES
        .iter()
        .find_map(|n| {
            let p = dir.join(n);
            if p.is_file() { Some(p) } else { None }
        })
        .or_else(|| {
            std::fs::read_dir(dir).ok().and_then(|rd| {
                let mut exact: Option<PathBuf> = None;
                let mut fuzzy: Vec<PathBuf> = Vec::new();
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    if exact.is_none() && NAMES.contains(&name.as_str()) {
                        exact = Some(e.path());
                    } else if KEYWORDS.iter().any(|k| name.contains(k))
                        && !EXCLUDES.iter().any(|x| name.contains(x))
                        && matches!(name.rsplit('.').next(), Some("jpg" | "jpeg" | "png" | "webp"))
                    {
                        fuzzy.push(e.path());
                    }
                }
                // read_dir 顺序不稳定：候选排序后取字典序最小，保证探测结果可复现
                fuzzy.sort();
                exact.or(fuzzy.into_iter().next())
            })
        });
    let res = found.and_then(|src| {
        let data = std::fs::read(&src).ok()?;
        let mut h = DefaultHasher::new();
        data.hash(&mut h);
        let ext = src
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .filter(|x| x == "png" || x == "webp")
            .unwrap_or_else(|| "jpg".to_string());
        let name = format!("{:016x}.{}", h.finish(), ext);
        let dst = covers.join(&name);
        if !dst.exists() {
            std::fs::write(&dst, &data).ok()?;
        }
        Some(dst.to_string_lossy().into_owned())
    });
    memo.insert(dir.to_path_buf(), res.clone());
    res
}

/// 专辑名兜底：标签没有专辑名时用所在文件夹名；
/// 但文件直接躺在曲库根目录时，文件夹名（通常是 "music"/"下载" 之类）不是专辑名 → 归入「未命名专辑」
fn album_from_path(p: &Path, root: &Path) -> String {
    let Some(dir) = p.parent() else { return "未命名专辑".into() };
    if dir == root {
        return "未命名专辑".into();
    }
    dir.file_name()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名专辑".into())
}

fn probe_file(p: &Path, covers_dir: &Path, do_clean: bool, root: &Path, dir_cov: Option<String>) -> Meta {
    let stem = p
        .file_stem()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent_artist = p
        .parent()
        .and_then(|d| d.file_name())
        .map(|x| x.to_string_lossy().into_owned());
    let dummy = Meta {
        title: stem.clone(),
        artist: "未知艺术家".into(),
        album_artist: String::new(),
        album: parent_artist.map(|_| album_from_path(p, root)).unwrap_or_else(|| "未命名专辑".into()),
        disc: 1,
        disc_total: None,
        track_no: None,
        track_total: None,
        year: None,
        genre: None,
        duration: 0.0,
        source: "guessed".into(),
        lyrics: None,
        // 解析失败的曲目同样享受目录图（专辑封面与 tag 无关）
        cover_src: dir_cov.as_ref().map(|_| "dir".to_string()),
        cover: dir_cov.clone(),
        rg_track: None,
        rg_album: None,
    };
    let tagged = match Probe::open(p) {
        Ok(probe) => probe.read().ok(),
        Err(_) => None,
    };
    let tf = match tagged {
        Some(t) => t,
        None => return dummy,
    };
    let dur = tf.properties().duration().as_secs_f64();
    let tag = tf.primary_tag().or_else(|| tf.first_tag());
    let tag = match tag {
        Some(t) => t,
        None => return Meta { duration: dur, ..dummy },
    };
    let g = |k: &ItemKey| tag.get_string(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    // ReplayGain 标签（Vorbis REPLAYGAIN_* / ID3v2 TXXX，lofty 统一为 Unknown 键）
    let rg = |name: &str| -> Option<f64> {
        let v = tag.get_string(&ItemKey::Unknown(name.into()))?;
        v.trim_end_matches("dB").trim().parse::<f64>().ok()
    };
    let rg_track = rg("REPLAYGAIN_TRACK_GAIN");
    let rg_album = rg("REPLAYGAIN_ALBUM_GAIN");

    let raw_title = g(&ItemKey::TrackTitle).unwrap_or_else(|| stem.clone());
    let (title, source): (String, String) = if raw_title != stem {
        if do_clean {
            let c = clean_title(&raw_title);
            let src = if c != raw_title { "cleaned" } else { "tag" };
            (c, src.to_string())
        } else {
            (raw_title, "tag".to_string())
        }
    } else {
        (stem.clone(), "guessed".to_string())
    };
    let artist = g(&ItemKey::TrackArtist)
        .or_else(|| g(&ItemKey::AlbumArtist))
        .unwrap_or_else(|| "未知艺术家".into());
    let album_artist = g(&ItemKey::AlbumArtist).unwrap_or_else(|| artist.clone());
    let album = g(&ItemKey::AlbumTitle).unwrap_or_else(|| album_from_path(p, root));
    let (track_no, track_total) = parse_frac(tag.get_string(&ItemKey::TrackNumber));
    let (disc, disc_total) = parse_frac(tag.get_string(&ItemKey::DiscNumber));
    let year = g(&ItemKey::Year)
        .and_then(|s| s.split('-').next().unwrap_or("").trim().parse::<i64>().ok());
    let genre = g(&ItemKey::Genre);
    let lyrics = g(&ItemKey::Lyrics);
    // 封面优先级（专辑级一致性设计）：目录封面图 > 歌曲内嵌。
    // 目录图命中时跳过内嵌提取（也省一次图片落盘）
    let (cover, cover_src): (Option<String>, Option<String>) = match dir_cov {
        Some(c) => (Some(c), Some("dir".into())),
        None => extract_cover(tag, p, covers_dir)
            .map(|c| (Some(c), Some("embed".into())))
            .unwrap_or((None, None)),
    };
    Meta {
        title,
        artist,
        album_artist,
        album,
        disc: disc.unwrap_or(1),
        disc_total,
        track_no,
        track_total,
        year,
        genre,
        duration: dur,
        source: if source == "tag" && track_no.is_none() { "cleaned".to_string() } else { source },
        lyrics,
        cover,
        cover_src,
        rg_track,
        rg_album,
    }
}

/// 单遍流式扫描：发现与解析同时进行，100 条一批事务（设计文档 4.1）。
/// 真增量：mtime+size 未变且已有封面的文件直接跳过解析（notify 频繁触发场景的关键）。
/// 返回 (发现文件数, 实际解析数)
pub fn scan(
    conn: &mut Connection,
    root: &Path,
    covers_dir: &Path,
    app: &tauri::AppHandle,
    do_clean: bool,
    quiet: bool,
) -> (usize, usize) {
    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();
    // 现有库快照：未变动文件跳过（无封面来源的重解析以便后补；
    // 判据用 cover_src 而非 cover——升级到「目录图优先」策略时存量行 cover_src 为
    // NULL，自然触发一次全量重解析落标记，之后恢复增量）
    let mut existing: std::collections::HashMap<String, (i64, i64, bool, bool)> = conn
        .prepare("SELECT file_path, mtime, file_size, cover_src IS NOT NULL, rg_track IS NOT NULL FROM tracks")
        .map(|mut s| {
            s.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, i64>(4)? != 0,
                ))
            })
            .map(|rows| {
                rows.filter_map(|x| x.ok())
                    .map(|(p, m, sz, c, rg)| (p, (m, sz, c, rg)))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default()
        })
        .unwrap_or_default();
    let total = files.len();
    let mut done = 0usize;
    let mut changed = 0usize;
    let mut batch: Vec<(PathBuf, Meta)> = Vec::with_capacity(100);
    let mut dir_memo: HashMap<PathBuf, Option<String>> = HashMap::new();

    let flush = |conn: &mut Connection, batch: &mut Vec<(PathBuf, Meta)>| -> usize {
        if batch.is_empty() {
            return 0;
        }
        let tx = conn.transaction().unwrap();
        {
            let mut stmt = tx
                .prepare(
                    r#"INSERT INTO tracks(file_path,title,artist,album,album_artist,album_key,disc,disc_total,track_no,track_total,year,genre,duration,file_size,mtime,cover,cover_src,source,lyrics_embed,has_lrc_file,rg_track,rg_album,has_tr)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)
                    ON CONFLICT(file_path) DO UPDATE SET
                      title=excluded.title, artist=excluded.artist, album=excluded.album, album_artist=excluded.album_artist,
                      album_key=excluded.album_key, disc=excluded.disc, disc_total=excluded.disc_total, track_no=excluded.track_no,
                      track_total=excluded.track_total, year=excluded.year, genre=excluded.genre, duration=excluded.duration,
                      file_size=excluded.file_size, mtime=excluded.mtime, source=excluded.source,
                      cover=CASE WHEN tracks.cover_src='manual' THEN tracks.cover ELSE excluded.cover END,
                      cover_src=CASE WHEN tracks.cover_src='manual' THEN tracks.cover_src ELSE excluded.cover_src END,
                      lyrics_embed=excluded.lyrics_embed, has_lrc_file=excluded.has_lrc_file, rg_track=excluded.rg_track, rg_album=excluded.rg_album,
                      has_tr=CASE WHEN lyrics_tr IS NOT NULL AND lyrics_tr!='' THEN 1 ELSE excluded.has_tr END"#,
                )
                .unwrap();
            for (p, m) in batch.iter() {
                // 专辑键统一走 db::album_key_for（空标签回退艺人 / 隐形字符清洗 / 散装曲目归入未命名专辑），
                // 扫描结束后还会按「目录 + 专辑名」再做一次组内归并
                let album_key = crate::db::album_key_for(&m.album, &m.album_artist, &m.artist);
                let size = std::fs::metadata(p).map(|x| x.len() as i64).unwrap_or(0);
                let mtime = std::fs::metadata(p)
                    .and_then(|x| x.modified())
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let has_lrc = p.with_extension("lrc").exists() as i64;
                // 翻译检测（与预览"双语"口径对齐）：.zh.lrc 存在 / 内嵌 A2 双行 /
                // 外部 .lrc 本身双语（同时间戳双行）；lyrics_tr 缓存由 upsert CASE 兜住
                let a2 = |t: &str| crate::lyrics::parse_lrc(t).iter().any(|x| x.tr.is_some());
                let embed_tr = m.lyrics.as_deref().map(a2).unwrap_or(false);
                let lrc_tr = has_lrc == 1
                    && std::fs::read_to_string(p.with_extension("lrc"))
                        .map(|t| a2(&t))
                        .unwrap_or(false);
                let scan_tr = (p.with_extension("zh.lrc").exists() || embed_tr || lrc_tr) as i64;
                stmt.execute(rusqlite::params![
                    p.to_string_lossy(), m.title, m.artist, m.album, m.album_artist, album_key,
                    m.disc, m.disc_total, m.track_no, m.track_total, m.year, m.genre, m.duration,
                    size, mtime, m.cover, m.cover_src, m.source, m.lyrics, has_lrc,
                    m.rg_track.or(Some(0.0)), m.rg_album.or(Some(0.0)), scan_tr
                ])
                .unwrap();
            }
        }
        tx.commit().unwrap();
        let n = batch.len();
        batch.clear();
        n
    };

    for p in files {
        let key = p.to_string_lossy().into_owned();
        let (mtime, size) = std::fs::metadata(&p)
            .map(|md| {
                let mt = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (mt, md.len() as i64)
            })
            .unwrap_or((0, 0));
        // 增量跳过：未变动且已有封面来源（无来源的重解析以便后补）
        if let Some((m, s, has_cover, has_rg)) = existing.get(&key) {
            if *m == mtime && *s == size && *has_cover && *has_rg {
                done += 1;
                continue;
            }
        }
        // 封面优先级：目录封面图（含中文命名关键词匹配）> 歌曲内嵌；
        // 目录图 memo 每目录只探测一次
        let dir_cov = p.parent().and_then(|parent| dir_cover(parent, covers_dir, &mut dir_memo));
        let m = probe_file(&p, covers_dir, do_clean, root, dir_cov);
        batch.push((p, m));
        done += 1;
        if batch.len() >= 100 {
            changed += flush(conn, &mut batch);
            if !quiet {
                let _ = app.emit(
                    "scan-progress",
                    serde_json::json!({ "done": done, "total": total }),
                );
            }
        }
    }
    changed += flush(conn, &mut batch);
    if !quiet {
        let _ = app.emit("scan-progress", serde_json::json!({ "done": done, "total": total }));
    }
    (done, changed)
}

/// 增量：删除根目录外已不存在的记录（mtime+size 比对由 upsert 覆盖）
pub fn prune(conn: &Connection, root: &Path) -> usize {
    let rows: Vec<(i64, String)> = conn
        .prepare("SELECT id, file_path FROM tracks")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    let mut gone = 0;
    for (id, path) in rows {
        let p = PathBuf::from(&path);
        let missing = !p.exists();
        let outside_root = !p.starts_with(root);
        if missing || outside_root {
            conn.execute("DELETE FROM tracks WHERE id=?1", [id]).unwrap();
            gone += 1;
        }
    }
    gone
}

/// 标签写回：把 DB 中的元数据（补全成果）写回音频文件标签。
/// 仅写文本字段（标题/艺人/专辑/年份/track 排序）+ ReplayGain 四字段，不写封面。
pub fn writeback_tags(conn: &Connection, track_id: i64) -> Result<(), String> {
    let row: Option<(String, String, String, Option<String>, Option<i64>, Option<i64>, Option<f64>, Option<f64>, String)> = conn
        .query_row(
            "SELECT file_path, title, artist, album, year, track_no, rg_track, rg_peak, album_key FROM tracks WHERE id=?1",
            [track_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
        )
        .ok();
    let (path, title, artist, album, year, track_no, rg_track, rg_peak, album_key) = row.ok_or("曲目不存在")?;
    // 专辑增益/峰值聚合（组内 max peak、能量-时长加权 gain；组内任一缺 track 值则不写 album 字段）
    let (album_gain, album_peak): (Option<f64>, Option<f64>) = if rg_track.is_some() {
        let rows: Vec<(Option<f64>, f64, Option<f64>)> = conn
            .prepare("SELECT rg_track, duration, rg_peak FROM tracks WHERE album_key=?1")
            .and_then(|mut s| {
                Ok(s.query_map([&album_key], |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, f64>(1)?, r.get::<_, Option<f64>>(2)?)))?
                    .filter_map(|x| x.ok())
                    .collect())
            })
            .unwrap_or_default();
        let all = !rows.is_empty() && rows.iter().all(|(g, _, _)| g.is_some());
        if all {
            let tracks: Vec<(f64, f64)> = rows.iter().filter_map(|(g, d, _)| g.map(|g| (g, *d))).collect();
            let peak = rows.iter().filter_map(|(_, _, p)| *p).fold(0.0f64, f64::max);
            (Some(crate::replaygain::album_gain(&tracks)), Some(peak))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    let mut tf = Probe::open(&path)
        .and_then(|p| p.read())
        .map_err(|e| format!("读取失败: {e}"))?;
    {
        let tag = match tf.primary_tag_mut() {
            Some(t) => t,
            None => tf.first_tag_mut().ok_or("该格式不支持标签写入")?,
        };
        tag.insert_text(ItemKey::TrackTitle, title);
        tag.insert_text(ItemKey::TrackArtist, artist);
        if let Some(al) = album {
            if !al.is_empty() && al != "未命名专辑" {
                tag.insert_text(ItemKey::AlbumTitle, al);
            }
        }
        if let Some(y) = year {
            tag.insert_text(ItemKey::Year, y.to_string());
        }
        if let Some(tn) = track_no {
            if tn > 0 {
                tag.insert_text(ItemKey::TrackNumber, tn.to_string());
            }
        }
        // ReplayGain 四字段（RG 2.0 / -18 LUFS 口径，文本格式与 foobar2000 一致）
        if let Some(g) = rg_track {
            tag.insert_text(ItemKey::Unknown("REPLAYGAIN_TRACK_GAIN".into()), format!("{g:+.2} dB"));
            if let Some(p) = rg_peak {
                tag.insert_text(ItemKey::Unknown("REPLAYGAIN_TRACK_PEAK".into()), format!("{p:.6}"));
            }
        }
        if let Some(ag) = album_gain {
            tag.insert_text(ItemKey::Unknown("REPLAYGAIN_ALBUM_GAIN".into()), format!("{ag:+.2} dB"));
            if let Some(ap) = album_peak {
                tag.insert_text(ItemKey::Unknown("REPLAYGAIN_ALBUM_PEAK".into()), format!("{ap:.6}"));
            }
        }
    }
    let mut out = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("打开文件失败: {e}"))?;
    tf.save_to(&mut out, lofty::config::WriteOptions::default())
        .map_err(|e| format!("写回失败: {e}"))?;
    Ok(())
}
