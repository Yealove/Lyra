use rusqlite::Connection;
use serde::Serialize;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri::Manager;

pub struct Db(pub Connection);

#[derive(Serialize, Clone, Debug)]
pub struct Track {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_key: String,
    pub album_artist: String,
    pub disc: i64,
    pub track_no: i64,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub duration: f64,
    pub cover: Option<String>,
    pub source: String,
    pub has_lyrics: bool,
    /// 歌词含中文翻译（双语）：列表"译"徽标（与预览"· 双语"同口径）
    pub has_tr: bool,
    pub liked: bool,
    pub file_path: String,
    pub hidden: i64,
    /// 播放次数（play_history 计数；曲目页排序/筛选用）
    pub plays: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Album {
    pub album_key: String,
    pub album: String,
    pub artist: String,
    pub year: Option<i64>,
    pub track_count: i64,
    pub duration: f64,
    pub cover: Option<String>,
    pub source: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub cover: Option<String>,
    pub count: i64,
}

pub fn db_path(app: &AppHandle) -> PathBuf {
    let dir = app.path().app_data_dir().expect("no app data dir");
    std::fs::create_dir_all(&dir).ok();
    dir.join("lyra.db")
}

pub fn covers_dir(app: &AppHandle) -> PathBuf {
    let dir = app.path().app_data_dir().expect("no app data dir");
    let d = dir.join("covers");
    std::fs::create_dir_all(&d).ok();
    d
}

pub fn open(path: &PathBuf) -> Result<Connection, String> {
    // 全新库（文件不存在）无需迁移前快照——没有可丢的数据，SCHEMA 直接建最新结构
    let is_new = !path.exists();
    let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "foreign_keys", "ON").ok();
    conn.execute_batch(SCHEMA).map_err(|e| format!("初始化表结构失败: {e}"))?;
    // 存量补列（历史平铺迁移，幂等）：错误窄化，不再 .ok() 全吞
    legacy_baseline(&conn)?;
    // 版本化增量迁移（v2 封面策略收编于此，新迁移从 v3 起登记）
    run_migrations(&conn, if is_new { None } else { Some(path) })?;
    // 专辑键归并（幂等）：修复历史数据里因标签残缺/多艺人变体造成的同名专辑分裂
    normalize_album_keys(&conn);
    // 专辑封面归一（幂等）：封面是专辑级资产，保证同专辑播放时封面一致
    unify_album_covers(&conn);
    Ok(conn)
}

/// 幂等补列：仅忽略 "duplicate column name"（列已存在的正常情况），
/// 其余错误（磁盘满/库损坏/语法错误）上抛——旧实现 .ok() 全吞导致
/// 迁移失败后带病运行，缺列问题延迟到运行深处才爆炸
fn add_column(conn: &Connection, table: &str, col: &str, def: &str) -> Result<bool, String> {
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {def}"), []) {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
            if msg.contains("duplicate column name") => Ok(false),
        Err(e) => Err(format!("补列 {table}.{col} 失败: {e}")),
    }
}

/// v3 之前的存量迁移：把任意历史形态的老库（user_version 0/2，列集合不确定）
/// 补齐到"v3 前夜"。幂等：列已存在自动跳过。加列语义注释从旧 open() 原样收编
fn legacy_baseline(conn: &Connection) -> Result<(), String> {
    add_column(conn, "tracks", "lyrics_tr", "TEXT")?;
    add_column(conn, "tracks", "enrichment_status", "TEXT NOT NULL DEFAULT 'none'")?;
    add_column(conn, "tracks", "online_meta", "TEXT")?;
    add_column(conn, "tracks", "liked", "INTEGER NOT NULL DEFAULT 0")?;
    // ReplayGain：NULL=未解析，0.0=已解析无标签
    add_column(conn, "tracks", "rg_track", "REAL")?;
    add_column(conn, "tracks", "rg_album", "REAL")?;
    // 样本峰值（RG 分析产出，写回标签时用）
    add_column(conn, "tracks", "rg_peak", "REAL")?;
    // 封面来源标记：manual（在线应用）> dir（目录图）> embed（内嵌）> artist（歌手图兜底）。
    // 归一选代表与扫描覆盖保护（manual 不被重扫覆盖）都依赖它
    add_column(conn, "tracks", "cover_src", "TEXT")?;
    // 隐藏曲目（cue 整轨等长音频）：所有列表不展示，设置里可查看/恢复
    add_column(conn, "tracks", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
    // 歌单自定义封面（本地图片或网络图入库后的路径）；NULL=用前 4 首曲目拼贴
    add_column(conn, "playlists", "cover", "TEXT")?;
    // 歌词含翻译标记（列表"译"徽标）：首次加列时回填全库
    if add_column(conn, "tracks", "has_tr", "INTEGER NOT NULL DEFAULT 0")? {
        // 立即覆盖最常见来源：自定义源翻译缓存（纯 SQL，瞬时）
        let _ = conn.execute(
            "UPDATE tracks SET has_tr=1 WHERE lyrics_tr IS NOT NULL AND lyrics_tr!=''",
            [],
        );
        // 其余来源（.zh.lrc / 内嵌 A2 / 外部 .lrc 双语）由 setup 的
        // backfill_has_tr 幂等回填（含通知前端刷新）
    }
    Ok(())
}

/// 版本化迁移条目 (版本号, 描述, SQL)。编写规范见 docs/数据库迁移改造.md：
/// 只增不改（已发布条目永不修改）、版本只前进、每条在单事务内原子执行
type Migration = (i64, &'static str, &'static str);

/// 增量迁移登记处。v2 已被"封面策略"占用（历史孤例，收编在 run_migrations_with 内），
/// 新迁移从 v3 起编号；建新表永远 CREATE TABLE IF NOT EXISTS（可同时进 SCHEMA，双写不冲突）
const MIGRATIONS: &[Migration] = &[
    // (3, "在线歌单（B站）基础表", "CREATE TABLE IF NOT EXISTS bili_playlists(...); ..."),
];

/// 当前程序支持的库结构版本（有迁移条目时 = 末条版本号）
const DB_VERSION: i64 = 2;

fn run_migrations(conn: &Connection, db_path: Option<&PathBuf>) -> Result<(), String> {
    run_migrations_with(conn, db_path.map(|p| p as &std::path::Path), MIGRATIONS, DB_VERSION)
}

/// 版本化迁移执行器：
/// - 降级保护：库版本高于程序支持 → 拒绝打开（防旧版程序写坏新库）
/// - v < 目标版本时先做迁移前快照（VACUUM INTO，保留最近一份）
/// - v2 封面策略 + MIGRATIONS 依序执行，每条单事务原子（DDL 与 user_version 同事务，
///   中断不会出现"结构改了版本没记"或反之；条目间独立提交，重试只补剩余）
/// - 测试通过注入 extra 迁移与目标版本验证执行器行为（migrate_tests）
fn run_migrations_with(
    conn: &Connection,
    db_path: Option<&std::path::Path>,
    mig: &[Migration],
    db_ver: i64,
) -> Result<(), String> {
    let mut v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("读取 user_version 失败: {e}"))?;
    if v > db_ver {
        return Err(format!(
            "数据库版本（v{v}）高于当前程序支持的版本（v{db_ver}），\n\
             可能正在用旧版程序打开新版数据。\n\
             请先升级好好听歌后再启动，以免数据损坏。"
        ));
    }
    if v < db_ver {
        if let Some(p) = db_path {
            snapshot_before_migration(conn, p, v)?;
        }
    }
    // v2 封面策略（历史孤例收编）：存量 embed 标记可能来自旧版探测逻辑
    // （未认出中文命名目录图），清空触发一次全量重扫升级；manual 是用户手动选择，保持不动
    if v < 2 {
        let tx = conn.unchecked_transaction().map_err(mig_tx_err)?;
        tx.execute(
            "UPDATE tracks SET cover_src=NULL WHERE cover_src IS NOT NULL AND cover_src != 'manual'",
            [],
        )
        .map_err(|e| format!("迁移 v2 失败: {e}"))?;
        tx.pragma_update(None, "user_version", 2)
            .map_err(|e| format!("迁移 v2 写版本失败: {e}"))?;
        tx.commit().map_err(|e| format!("迁移 v2 提交失败: {e}"))?;
        v = 2;
        eprintln!("[db] 迁移 v2：封面策略（清空非 manual 的 cover_src）");
    }
    for (ver, desc, sql) in mig {
        if *ver <= v {
            continue;
        }
        let tx = conn.unchecked_transaction().map_err(mig_tx_err)?;
        tx.execute_batch(sql)
            .map_err(|e| format!("迁移 v{ver}（{desc}）失败: {e}"))?;
        tx.pragma_update(None, "user_version", *ver)
            .map_err(|e| format!("迁移 v{ver} 写版本失败: {e}"))?;
        tx.commit().map_err(|e| format!("迁移 v{ver} 提交失败: {e}"))?;
        v = *ver;
        eprintln!("[db] 迁移升级至 v{ver}：{desc}");
    }
    debug_assert_eq!(v, db_ver, "DB_VERSION 应与 MIGRATIONS 末条版本一致");
    Ok(())
}

fn mig_tx_err(e: rusqlite::Error) -> String {
    format!("开启迁移事务失败: {e}")
}

/// 迁移前快照：VACUUM INTO 产出一致性副本（WAL 安全，区别于直接 copy 文件）。
/// 文件名带毫秒时间戳（并发 open 不互踩），保留最近一份（清理更早的 pre-migration-*）；
/// 恢复方式：退出应用后用它替换 lyra.db（不入自动备份 zip 索引，防误恢复旧结构）
fn snapshot_before_migration(conn: &Connection, db_path: &std::path::Path, from: i64) -> Result<(), String> {
    let dir = db_path.with_file_name("backups");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dst = dir.join(format!("pre-migration-from-v{from}-{ms}.db"));
    conn.execute("VACUUM INTO ?1", [dst.to_string_lossy().into_owned()])
        .map_err(|e| format!("迁移前快照失败: {e}"))?;
    // 只保留最新一份（并发极端下最坏丢快照，不丢数据，可接受）
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("pre-migration-") && name.ends_with(".db") && e.path() != dst {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    eprintln!("[db] 迁移前快照已保存: {}", dst.display());
    Ok(())
}

/// has_tr 全库回填：对有歌词但未标记的曲目逐曲完整判定（.zh.lrc / A2 双行等）。
/// 独立连接（WAL），由 main setup 在后台线程调用，完成 emit library-changed
/// 通知前端刷新列表——否则前端启动时拉到的全是回填前的 0 值。
/// 幂等：已标记的行自动跳过（返回本次处理的数量，0 则无需通知）。
pub fn backfill_has_tr(db_path: &std::path::Path) -> usize {
    let Ok(bc) = Connection::open(db_path) else { return 0 };
    let ids: Vec<i64> = match bc
        .prepare(
            "SELECT id FROM tracks WHERE has_tr=0 AND (has_lrc_file=1 OR (lyrics_embed IS NOT NULL AND lyrics_embed!='') OR (lyrics_online IS NOT NULL AND lyrics_online!=''))",
        )
        .and_then(|mut s| {
            s.query_map([], |r| r.get(0))
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
        }) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    for id in &ids {
        crate::lyrics::recompute_has_tr(&bc, *id);
    }
    ids.len()
}

const UNKNOWN_ARTIST: &str = "未知艺术家";
const UNNAMED_ALBUM: &str = "未命名专辑";

/// 专辑键：正常专辑 = 「专辑艺人|专辑名」；无专辑标签的散装曲目统一归入「未命名专辑」，
/// 不再按艺人前缀分裂成多张同名专辑。
pub fn album_key_for(album: &str, album_artist: &str, artist: &str) -> String {
    let al = clean_text(album);
    if al.is_empty() || al == UNNAMED_ALBUM {
        return UNNAMED_ALBUM.to_string();
    }
    let rep = trusted_artist(album_artist)
        .or_else(|| trusted_artist(artist))
        .unwrap_or_else(|| UNKNOWN_ARTIST.to_string());
    format!("{}|{}", rep, al)
}

fn trusted_artist(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t == UNKNOWN_ARTIST { None } else { Some(t.to_string()) }
}

/// 重算单曲 album_key（改过 album / artist 之后必须调用，否则会挂在旧专辑下）
pub fn refresh_album_key(conn: &Connection, track_id: i64) -> rusqlite::Result<()> {
    let (album, aa, ar): (String, String, String) = conn.query_row(
        "SELECT album, album_artist, artist FROM tracks WHERE id=?1",
        [track_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let key = album_key_for(&album, &aa, &ar);
    conn.execute(
        "UPDATE tracks SET album_key=?1 WHERE id=?2 AND album_key<>?1",
        rusqlite::params![key, track_id],
    )?;
    Ok(())
}

/// 文本归一：去掉标签里肉眼不可见的字符（Unicode 私有区 PUA、零宽符、变体选择符、
/// 软连字符、控制符），并把连续空白折叠成一个空格。
/// 专辑名带一个 PUA 字符就足以让「看起来一模一样」的两张专辑分成两张，必须清掉。
pub fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        let keep = match ch {
            '\u{E000}'..='\u{F8FF}'
            | '\u{F0000}'..='\u{FFFFD}'
            | '\u{100000}'..='\u{10FFFD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FEFF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{00AD}' => continue,
            c if c.is_control() => continue,
            c if c.is_whitespace() => {
                if last_space {
                    continue;
                }
                last_space = true;
                ' '
            }
            c => {
                last_space = false;
                c
            }
        };
        out.push(keep);
    }
    out.trim().to_string()
}

/// 专辑归并：album_key 逐文件按「专辑艺人|专辑名」生成，一旦部分曲目缺 album_artist
/// 标签（或写成多艺人变体），同一张专辑就会分裂成多张同名专辑。
/// 这里按「所在目录 + 专辑名」分组，取组内出现最多的可信专辑艺人重算 key（缺失回退 artist），
/// 幂等：只更新与目标值不同的行。
pub fn normalize_album_keys(conn: &Connection) {
    use std::collections::HashMap;
    let mut rows: Vec<(i64, String, String, String, String)> = Vec::new();
    if let Ok(mut st) = conn.prepare("SELECT id, file_path, album, album_artist, artist FROM tracks") {
        if let Ok(it) = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        }) {
            rows = it.flatten().collect();
        }
    }
    if rows.is_empty() {
        return;
    }
    // 曲库根目录：直接躺在根目录里的文件是散装单曲（文件夹名不可能是专辑名）
    let roots: Vec<String> = {
        let mut v = Vec::new();
        if let Ok(mut st) = conn.prepare("SELECT path FROM scan_roots") {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                v = rows.flatten().map(|s| s.trim_end_matches('/').to_string()).collect();
            }
        }
        v
    };
    let mut groups: HashMap<(String, String), Vec<(i64, String)>> = HashMap::new();
    for (id, path, album, aa, artist) in rows {
        let p = std::path::Path::new(&path);
        let dir = p.parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let name = trusted_artist(&aa).or_else(|| trusted_artist(&artist)).unwrap_or_default();
        let at_root = roots.iter().any(|r| r == &dir.trim_end_matches('/'));
        // 根目录散装单曲 / 无专辑标签：全库统一归入「未命名专辑」，不再按艺人和文件夹分裂
        let group = if at_root || clean_text(&album) == UNNAMED_ALBUM {
            (String::new(), UNNAMED_ALBUM.to_string())
        } else {
            (dir, clean_text(&album))
        };
        groups.entry(group).or_default().push((id, name));
    }
    let Ok(tx) = conn.unchecked_transaction() else { return };
    for ((_, album), items) in groups {
        let mut votes: HashMap<String, usize> = HashMap::new();
        for (_, name) in &items {
            if !name.is_empty() {
                *votes.entry(name.clone()).or_insert(0) += 1;
            }
        }
        // 票数最多者胜出；平票时取字典序较小者，保证结果稳定
        let rep = votes
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| UNKNOWN_ARTIST.to_string());
        let key = album_key_for(&album, &rep, "");
        for (id, _) in items {
            // 顺带把 album 里的不可见字符清掉（列表显示 / 再次分组都需要干净值）
            let _ = tx.execute(
                "UPDATE tracks SET album_key=?1, album=?2 WHERE id=?3 AND (album_key<>?1 OR album<>?2)",
                rusqlite::params![key, album, id],
            );
        }
    }
    let _ = tx.commit();
}

/// 专辑封面归一（幂等）：封面是专辑级资产，保证同专辑所有曲目播放时显示同一张封面。
/// 组内按来源 rank 选代表并全组统一：manual（在线获取应用）> dir（目录封面图）> embed（歌曲内嵌）
/// > artist（歌手图兜底）；同级取字典序最小保证结果稳定。
/// 「未命名专辑」组是散装单曲大杂烩，互相统一封面会张冠李戴，跳过。
pub fn unify_album_covers(conn: &Connection) -> usize {
    use std::collections::HashMap;
    fn rank(src: Option<&str>) -> i32 {
        match src {
            Some("manual") => 0,
            Some("dir") => 1,
            Some("embed") => 2,
            Some("artist") => 3,
            _ => 4,
        }
    }
    let rows: Vec<(String, Option<String>, Option<String>)> = match conn
        .prepare("SELECT album_key, cover, cover_src FROM tracks WHERE cover IS NOT NULL AND album_key != ?1")
        .and_then(|mut s| {
            s.query_map([UNNAMED_ALBUM], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
            })
            .map(|it| it.filter_map(|x| x.ok()).collect())
        }) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    // 组代表：(rank, cover) 双键取最小
    let mut best: HashMap<String, (i32, String, String)> = HashMap::new();
    for (key, cover, src) in rows {
        let Some(cover) = cover else { continue };
        let src = src.unwrap_or_else(|| "embed".into());
        let r = rank(Some(&src));
        let e = best.entry(key).or_insert((r, cover.clone(), src.clone()));
        if (r, cover.clone()) < (e.0, e.1.clone()) {
            *e = (r, cover, src);
        }
    }
    let mut n = 0;
    let Ok(tx) = conn.unchecked_transaction() else { return 0 };
    for (key, (_, cover, src)) in best {
        n += tx
            .execute(
                "UPDATE tracks SET cover=?1, cover_src=?2 WHERE album_key=?3 AND (cover IS NULL OR cover<>?1 OR cover_src IS NULL OR cover_src<>?2)",
                rusqlite::params![cover, src, key],
            )
            .unwrap_or(0);
    }
    let _ = tx.commit();
    n
}

/// 歌手图兜底（幂等）：归一后仍整组无封面的专辑，用曲目所在歌手目录
/// （专辑目录及其上一级）的 artist.jpg / cover.jpg / folder.jpg 等作封面，
/// 按 hash 复制进 covers 缓存（asset 协议 scope 只覆盖 $APPDATA），
/// 全组写 cover_src='artist'。「未命名专辑」散装单曲不参与。
pub fn fill_artist_covers(conn: &Connection, covers_dir: &std::path::Path) -> usize {
    const NAMES: &[&str] = &["artist.jpg", "artist.png", "artist.jpeg", "cover.jpg", "cover.png", "folder.jpg"];
    // 无封面组的任意一条 file_path，作为歌手目录定位样本
    let groups: Vec<(String, String)> = match conn
        .prepare(
            "SELECT album_key, MIN(file_path) FROM tracks
             WHERE cover IS NULL AND album_key != ?1
             GROUP BY album_key",
        )
        .and_then(|mut s| {
            s.query_map([UNNAMED_ALBUM], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.filter_map(|x| x.ok()).collect())
        }) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let mut n = 0;
    let Ok(tx) = conn.unchecked_transaction() else { return 0 };
    for (key, sample) in groups {
        let p = std::path::PathBuf::from(&sample);
        // 专辑目录与其上一级（歌手目录）都查，与 local_artist_imgs 同一套命名约定
        let bases = [p.parent().and_then(|x| x.parent()).map(|x| x.to_path_buf()), p.parent().map(|x| x.to_path_buf())];
        let mut img: Option<std::path::PathBuf> = None;
        for base in bases.into_iter().flatten() {
            img = NAMES.iter().find_map(|nm| {
                let c = base.join(nm);
                if c.is_file() { Some(c) } else { None }
            });
            if img.is_some() { break; }
        }
        let Some(src_path) = img else { continue };
        let Ok(data) = std::fs::read(&src_path) else { continue };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hasher;
        h.write(&data);
        let ext = src_path
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .filter(|x| x == "png" || x == "webp")
            .unwrap_or_else(|| "jpg".to_string());
        let dst = covers_dir.join(format!("{:016x}.{}", h.finish(), ext));
        if !dst.exists() && std::fs::write(&dst, &data).is_err() {
            continue;
        }
        n += tx
            .execute(
                "UPDATE tracks SET cover=?1, cover_src='artist' WHERE album_key=?2 AND cover IS NULL",
                rusqlite::params![dst.to_string_lossy(), key],
            )
            .unwrap_or(0);
    }
    let _ = tx.commit();
    n
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tracks(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  file_path TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  album TEXT NOT NULL DEFAULT '',
  album_artist TEXT NOT NULL DEFAULT '',
  album_key TEXT NOT NULL DEFAULT '',
  disc INTEGER NOT NULL DEFAULT 1,
  disc_total INTEGER,
  track_no INTEGER,
  track_total INTEGER,
  year INTEGER,
  genre TEXT,
  duration REAL NOT NULL DEFAULT 0,
  file_size INTEGER NOT NULL DEFAULT 0,
  mtime INTEGER NOT NULL DEFAULT 0,
  cover TEXT,
  cover_src TEXT,
  source TEXT NOT NULL DEFAULT 'tag',
  lyrics_embed TEXT,
  lyrics_online TEXT,
  lyrics_tr TEXT,
  has_lrc_file INTEGER NOT NULL DEFAULT 0,
  hidden INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_key, disc, track_no, id);
CREATE TABLE IF NOT EXISTS playlists(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  cover TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlist_tracks(
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  added_at INTEGER NOT NULL,
  PRIMARY KEY (playlist_id, track_id)
);
CREATE TABLE IF NOT EXISTS scan_roots(
  path TEXT PRIMARY KEY,
  added_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS artist_cache(
            name TEXT PRIMARY KEY,
            img TEXT, wiki TEXT, meta TEXT);
        CREATE TABLE IF NOT EXISTS play_history(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  played_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_history_time ON play_history(played_at DESC);
"#;

const TRACK_COLS: &str =
    "t.id,t.title,t.artist,t.album,t.album_key,t.album_artist,t.disc,t.track_no,t.year,t.genre,t.duration,t.cover,t.source,(t.lyrics_embed IS NOT NULL AND t.lyrics_embed!='' OR t.lyrics_online IS NOT NULL AND t.lyrics_online!='' OR t.has_lrc_file=1) AS has_lyrics,t.has_tr,t.liked,t.file_path,t.hidden,(SELECT COUNT(*) FROM play_history h WHERE h.track_id=t.id) AS plays";

fn row_to_track(r: &rusqlite::Row) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        title: r.get(1)?,
        artist: r.get(2)?,
        album: r.get(3)?,
        album_key: r.get(4)?,
        album_artist: r.get(5)?,
        disc: r.get(6)?,
        track_no: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
        year: r.get(8)?,
        genre: r.get(9)?,
        duration: r.get(10)?,
        cover: r.get(11)?,
        source: r.get(12)?,
        has_lyrics: r.get::<_, i64>(13)? != 0,
        has_tr: r.get::<_, i64>(14)? != 0,
        liked: r.get::<_, i64>(15)? != 0,
        file_path: r.get(16)?,
        hidden: r.get::<_, i64>(17)?,
        plays: r.get(18)?,
    })
}

pub fn all_tracks(conn: &Connection) -> Vec<Track> {
    let sql = format!("SELECT {} FROM tracks t WHERE t.hidden=0 ORDER BY t.artist,t.album,t.disc,t.track_no,t.id", TRACK_COLS);
    let res: Result<Vec<Track>, rusqlite::Error> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |r| row_to_track(r))
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
        });
    if let Err(e) = &res {
        eprintln!("[db] all_tracks: {e}");
    }
    res.unwrap_or_default()
}

pub fn tracks_by_ids(conn: &Connection, ids: &[i64]) -> Vec<Track> {
    if ids.is_empty() {
        return vec![];
    }
    let list = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {} FROM tracks t WHERE t.id IN ({})",
        TRACK_COLS, list
    );
    conn.prepare(&sql)
        .unwrap()
        .query_map([], |r| row_to_track(r))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect()
}

pub fn albums(conn: &Connection) -> Vec<Album> {
    let sql = r#"
    SELECT album_key,
      COALESCE(NULLIF(MIN(CASE WHEN album!='未命名专辑' AND album!=title THEN album END),''), MIN(album)) AS album,
      CASE
        WHEN MIN(album_artist)=MAX(album_artist) AND COALESCE(MIN(album_artist),'')!='' THEN MIN(album_artist)
        WHEN COUNT(DISTINCT NULLIF(artist,''))=1 THEN MAX(NULLIF(artist,''))
        ELSE '群星'
      END AS artist,
      MAX(year) AS year, COUNT(*) AS n, SUM(duration) AS dur, MAX(cover) AS cover,
      CASE WHEN SUM(source='guessed')>0 THEN 'guessed'
           WHEN SUM(source='online')>0 THEN 'online'
           WHEN SUM(source='cleaned')>0 THEN 'cleaned'
           ELSE 'tag' END AS source
    FROM tracks
    WHERE hidden=0
    GROUP BY album_key
    HAVING NOT (COUNT(*)=1 AND (MIN(album)='未命名专辑' OR MIN(album)=MIN(title)))
    ORDER BY artist, album
    "#;
    let res: Result<Vec<Album>, rusqlite::Error> = conn
        .prepare(sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |r| {
                Ok(Album {
                    album_key: r.get(0)?,
                    album: r.get(1)?,
                    artist: r
                        .get::<_, Option<String>>(2)?
                        .unwrap_or_else(|| "未知艺术家".into()),
                    year: r.get(3)?,
                    track_count: r.get(4)?,
                    duration: r.get(5)?,
                    cover: r.get(6)?,
                    source: r.get(7)?,
                })
            })
            .map(|rows| rows.filter_map(|x| x.ok()).collect())
        });
    if let Err(e) = &res {
        eprintln!("[db] albums: {e}");
    }
    res.unwrap_or_default()
}

pub fn playlists(conn: &Connection) -> Vec<Playlist> {
    conn.prepare(
        "SELECT p.id, p.name, p.cover, COUNT(pt.track_id) FROM playlists p
         LEFT JOIN playlist_tracks pt ON pt.playlist_id=p.id
         GROUP BY p.id ORDER BY p.created_at",
    )
    .unwrap()
    .query_map([], |r| {
        Ok(Playlist {
            id: r.get(0)?,
            name: r.get(1)?,
            cover: r.get(2)?,
            count: r.get(3)?,
        })
    })
    .unwrap()
    .filter_map(|x| x.ok())
    .collect()
}

pub fn playlist_tracks(conn: &Connection, pid: i64) -> Vec<Track> {
    let sql = format!(
        "SELECT {} FROM tracks t JOIN playlist_tracks pt ON pt.track_id=t.id
         WHERE pt.playlist_id=?1 AND t.hidden=0 ORDER BY pt.position",
        TRACK_COLS
    );
    conn.prepare(&sql)
        .unwrap()
        .query_map([pid], |r| row_to_track(r))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect()
}

// —— 歌单写操作 ——
/// 设置歌单封面（covers 缓存内的绝对路径；空=恢复拼贴）
pub fn set_playlist_cover(conn: &Connection, pid: i64, cover: Option<&str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "UPDATE playlists SET cover=?1, updated_at=?2 WHERE id=?3",
        rusqlite::params![cover, now, pid],
    )
    .unwrap();
}

pub fn create_playlist(conn: &Connection, name: &str) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO playlists(name, created_at, updated_at) VALUES(?1,?2,?2)",
        [name, &now.to_string()],
    )
    .unwrap();
    conn.last_insert_rowid()
}

pub fn rename_playlist(conn: &Connection, id: i64, name: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "UPDATE playlists SET name=?1, updated_at=?2 WHERE id=?3",
        rusqlite::params![name, now, id],
    )
    .unwrap();
}

pub fn delete_playlist(conn: &Connection, id: i64) {
    conn.execute("DELETE FROM playlists WHERE id=?1", [id]).unwrap();
}

pub fn add_to_playlist(conn: &Connection, pid: i64, tid: i64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let next: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position),0)+65536 FROM playlist_tracks WHERE playlist_id=?1",
            [pid],
            |r| r.get(0),
        )
        .unwrap_or(65536);
    let n = conn
        .execute(
            "INSERT OR IGNORE INTO playlist_tracks(playlist_id,track_id,position,added_at) VALUES(?1,?2,?3,?4)",
            rusqlite::params![pid, tid, next, now],
        )
        .unwrap();
    n > 0
}

pub fn remove_at(conn: &Connection, pid: i64, idx: i64) {
    let row: Option<(i64,)> = conn
        .prepare("SELECT track_id FROM playlist_tracks WHERE playlist_id=?1 ORDER BY position LIMIT 1 OFFSET ?2")
        .unwrap()
        .query_map(rusqlite::params![pid, idx], |r| Ok(r.get(0)?))
        .unwrap()
        .next()
        .and_then(|x| x.ok())
        .map(|tid| (tid,));
    if let Some((tid,)) = row {
        conn.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id=?1 AND track_id=?2",
            rusqlite::params![pid, tid],
        )
        .unwrap();
    }
}

/// 拖拽排序：移动到目标位置（取中点 position，间隔耗尽时全量重排——设计文档 6.1）
pub fn move_track(conn: &Connection, pid: i64, from: i64, to: i64) {
    let rows: Vec<(i64, i64)> = conn
        .prepare("SELECT track_id, position FROM playlist_tracks WHERE playlist_id=?1 ORDER BY position")
        .unwrap()
        .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    if from < 0 || to < 0 || from as usize >= rows.len() || to as usize >= rows.len() {
        return;
    }
    let mut ids: Vec<i64> = rows.iter().map(|x| x.0).collect();
    let it = ids.remove(from as usize);
    ids.insert(to as usize, it);
    let positions: Vec<i64> = rows.iter().map(|x| x.1).collect();
    let new_pos = if to as usize == 0 {
        positions[0] - 65536
    } else if to as usize == ids.len() - 1 {
        *positions.last().unwrap() + 65536
    } else {
        let a = positions[to as usize - 1];
        let b = positions[to as usize];
        if b - a <= 1 {
            // 间隔耗尽：全量重排
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            for (i, id) in ids.iter().enumerate() {
                conn.execute(
                    "UPDATE playlist_tracks SET position=?3 WHERE playlist_id=?1 AND track_id=?2",
                    rusqlite::params![pid, id, ((i + 1) * 65536) as i64 + now % 7],
                )
                .unwrap();
            }
            return;
        }
        a + (b - a) / 2
    };
    conn.execute(
        "UPDATE playlist_tracks SET position=?3 WHERE playlist_id=?1 AND track_id=?2",
        rusqlite::params![pid, it, new_pos],
    )
    .unwrap();
}

// —— 收藏与播放历史（设计文档 6.2 智能歌单）——
pub fn toggle_like(conn: &Connection, track_id: i64) -> bool {
    conn.execute("UPDATE tracks SET liked = 1-liked WHERE id=?1", [track_id]).ok();
    conn.query_row("SELECT liked FROM tracks WHERE id=?1", [track_id], |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v != 0)
    .unwrap_or(false)
}

pub fn record_play(conn: &Connection, track_id: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let _ = conn.execute(
        "INSERT INTO play_history(track_id, played_at) VALUES(?1, ?2)",
        rusqlite::params![track_id, now],
    );
}

/// 已隐藏曲目（cue 整轨等长音频）：只在「设置 → 已隐藏」里展示，按时长降序
pub fn hidden_tracks(conn: &Connection) -> Vec<Track> {
    let sql = format!("SELECT {} FROM tracks t WHERE t.hidden=1 ORDER BY t.duration DESC", TRACK_COLS);
    let res: Result<Vec<Track>, rusqlite::Error> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |r| row_to_track(r))
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
        });
    res.unwrap_or_default()
}

/// 设置/取消隐藏
pub fn set_hidden(conn: &Connection, track_id: i64, hidden: bool) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE tracks SET hidden=?1 WHERE id=?2",
        rusqlite::params![hidden as i64, track_id],
    )
}

/// 智能歌单：favorites=收藏 / recent=最近播放 / top=最常播放（运行时计算，非 playlists 表实体）
pub fn smart_tracks(conn: &Connection, kind: &str) -> Vec<Track> {
    let sql = match kind {
        "favorites" => format!(
            "SELECT {} FROM tracks t WHERE t.liked=1 AND t.hidden=0 ORDER BY t.id DESC LIMIT 500",
            TRACK_COLS
        ),
        "recent" => format!(
            "SELECT {} FROM (SELECT track_id, MAX(played_at) AS m FROM play_history GROUP BY track_id ORDER BY m DESC LIMIT 100) h JOIN tracks t ON t.id=h.track_id AND t.hidden=0 ORDER BY h.m DESC",
            TRACK_COLS
        ),
        _ => format!(
            "SELECT {} FROM (SELECT track_id, COUNT(*) AS n, MAX(played_at) AS m FROM play_history GROUP BY track_id ORDER BY n DESC, m DESC LIMIT 100) h JOIN tracks t ON t.id=h.track_id AND t.hidden=0 ORDER BY h.n DESC, h.m DESC",
            TRACK_COLS
        ),
    };
    let res: Result<Vec<Track>, rusqlite::Error> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            let iter = stmt.query_map([], |r| row_to_track(r))?;
            Ok(iter.filter_map(|x| x.ok()).collect())
        });
    res.unwrap_or_default()
}
pub fn scan_roots(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT path FROM scan_roots ORDER BY added_at")
        .and_then(|mut s| {
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .filter_map(|x| x.ok())
                .collect::<Vec<_>>())
        })
        .unwrap_or_default()
}

pub fn add_scan_root(conn: &Connection, path: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let _ = conn.execute(
        "INSERT OR IGNORE INTO scan_roots(path, added_at) VALUES(?1, ?2)",
        rusqlite::params![path, now],
    );
}

pub fn remove_scan_root(conn: &Connection, path: &str) {
    let _ = conn.execute("DELETE FROM scan_roots WHERE path=?1", [path]);
}

/// 曲库统计（统计页）：概览/月度 top/流派/年代/近 7 天
pub fn stats(conn: &Connection) -> serde_json::Value {
    use serde_json::json;
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
    };
    let q = |sql: &str| -> Vec<serde_json::Value> {
        conn.prepare(sql)
            .and_then(|mut s| {
                let it = s.query_map([], |r| {
                    Ok(json!({ "name": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)?, "dur": r.get::<_, Option<f64>>(2)?.unwrap_or(0.0) }))
                })?;
                Ok(it.filter_map(|x| x.ok()).collect())
            })
            .unwrap_or_default()
    };
    let overview = json!({
        "tracks": one("SELECT COUNT(*) FROM tracks WHERE hidden=0"),
        "albums": one("SELECT COUNT(DISTINCT album_key) FROM tracks WHERE hidden=0"),
        "artists": one("SELECT COUNT(DISTINCT COALESCE(NULLIF(album_artist,''), artist)) FROM tracks WHERE hidden=0"),
        "total_dur": one("SELECT CAST(SUM(duration) AS INTEGER) FROM tracks WHERE hidden=0"),
        "liked": one("SELECT COUNT(*) FROM tracks WHERE liked=1 AND hidden=0"),
        "enriched": one("SELECT COUNT(*) FROM tracks WHERE source='online' AND hidden=0"),
        "plays": one("SELECT COUNT(*) FROM play_history"),
        "days7": one("SELECT COUNT(*) FROM play_history WHERE played_at > strftime('%s','now') - 604800"),
    });
    json!({
        "overview": overview,
        "month_top": q("SELECT COALESCE(NULLIF(t.album_artist,''), t.artist) || ' - ' || t.title AS name, COUNT(*) AS n, MAX(t.duration) AS dur FROM play_history h JOIN tracks t ON t.id=h.track_id AND t.hidden=0 WHERE h.played_at > strftime('%s','now') - 2592000 GROUP BY h.track_id ORDER BY n DESC, MAX(h.played_at) DESC LIMIT 10"),
        "genres": q("SELECT COALESCE(NULLIF(genre,''),'未知') AS name, COUNT(*) AS n, SUM(duration) AS dur FROM tracks WHERE hidden=0 GROUP BY 1 ORDER BY n DESC LIMIT 8"),
        "decades": q("SELECT (year/10*10) || 's' AS name, COUNT(*) AS n, SUM(duration) AS dur FROM tracks WHERE hidden=0 AND year IS NOT NULL AND year > 1900 GROUP BY year/10 ORDER BY name"),
    })
}

/// 手动编辑曲目字段：仅更新传入的非空字段，来源标记 manual
pub fn update_fields(conn: &Connection, track_id: i64, f: &serde_json::Value) -> Result<(), String> {
    let title = f["title"].as_str().map(|s| s.trim()).filter(|s| !s.is_empty());
    let artist = f["artist"].as_str().map(clean_text).filter(|s| !s.is_empty());
    let album = f["album"].as_str().map(clean_text).filter(|s| !s.is_empty());
    let year = f["year"].as_i64().filter(|y| *y > 0);
    let track_no = f["track_no"].as_i64().filter(|t| *t > 0);
    if title.is_none() && artist.is_none() && album.is_none() && year.is_none() && track_no.is_none() {
        return Err("没有要修改的字段".into());
    }
    // 改专辑/艺人后必须同步重算 album_key，否则这首会留在旧专辑下（列表里多出一张同名专辑）
    conn.execute(
        "UPDATE tracks SET
           title=COALESCE(?2, title), artist=COALESCE(?3, artist),
           album=CASE WHEN ?4 IS NOT NULL AND ?4 != '' THEN ?4 ELSE album END,
           year=COALESCE(?5, year), track_no=COALESCE(?6, track_no),
           source='manual'
         WHERE id=?1",
        rusqlite::params![track_id, title, artist, album, year, track_no],
    )
    .map_err(|e| e.to_string())?;
    refresh_album_key(conn, track_id).map_err(|e| e.to_string())?;
    // 换组后新专辑内可能有封面，为这首继承组代表
    unify_album_covers(conn);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(conn: &Connection, path: &str, album: &str, album_artist: &str, artist: &str) {
        conn.execute(
            "INSERT INTO tracks(file_path,title,artist,album,album_artist,album_key) VALUES(?1,'t',?2,?3,?4,'stale')",
            rusqlite::params![path, artist, album, album_artist],
        )
        .unwrap();
    }

    fn keys(conn: &Connection) -> Vec<String> {
        let mut st = conn.prepare("SELECT album_key FROM tracks ORDER BY id").unwrap();
        let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.flatten().collect()
    }

    #[test]
    fn merges_album_split_by_missing_album_artist() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let dir = "/music/许嵩/寻物启事";
        seed(&conn, &format!("{dir}/01.flac"), "寻物启事", "许嵩", "许嵩");
        seed(&conn, &format!("{dir}/02.flac"), "寻物启事", "", "未知艺术家");
        seed(&conn, &format!("{dir}/03.flac"), "寻物启事", "未知艺术家", "未知艺术家");
        normalize_album_keys(&conn);
        assert_eq!(keys(&conn), vec!["许嵩|寻物启事"; 3]);
    }

    #[test]
    fn keeps_same_name_albums_in_different_dirs_apart() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        seed(&conn, "/music/A/Greatest Hits/01.flac", "Greatest Hits", "A", "A");
        seed(&conn, "/music/B/Greatest Hits/01.flac", "Greatest Hits", "B", "B");
        normalize_album_keys(&conn);
        assert_eq!(keys(&conn), vec!["A|Greatest Hits", "B|Greatest Hits"]);
    }

    #[test]
    fn idempotent_on_second_run() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        seed(&conn, "/m/x/01.flac", "X", "", "歌手");
        normalize_album_keys(&conn);
        let first = keys(&conn);
        normalize_album_keys(&conn);
        assert_eq!(first, keys(&conn));
        assert_eq!(first, vec!["歌手|X"]);
    }

    #[test]
    fn cleans_invisible_pua_chars_in_album() {
        // 真实案例：专辑名尾部混入 Unicode 私有区字符 U+EE64，肉眼看着同名却分成两张
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        seed(&conn, "/m/朴树/01.flac", "我去2000年\u{EE64}", "朴树", "朴树");
        seed(&conn, "/m/朴树/02.flac", "我去2000年", "朴树", "朴树");
        normalize_album_keys(&conn);
        assert_eq!(keys(&conn), vec!["朴树|我去2000年"; 2]);
        let albums: Vec<String> = {
            let mut st = conn.prepare("SELECT album FROM tracks ORDER BY id").unwrap();
            let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.flatten().collect()
        };
        assert_eq!(albums, vec!["我去2000年"; 2], "album 字段也要清洗");
    }

    #[test]
    fn root_level_loose_files_go_to_unnamed_album() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO scan_roots(path,added_at) VALUES('/Volumes/ssd/music',0)", []).unwrap();
        // 4 首散装单曲，album 标签都被写成了文件夹名 "music"
        seed(&conn, "/Volumes/ssd/music/a.flac", "music", "", "未知艺术家");
        seed(&conn, "/Volumes/ssd/music/b.flac", "music", "段兴华", "段兴华");
        seed(&conn, "/Volumes/ssd/music/c.flac", "music", "未知艺术家", "未知艺术家");
        seed(&conn, "/Volumes/ssd/music/d.flac", "music", "周杰伦", "周杰伦");
        // 子目录里的正常专辑不受影响
        seed(&conn, "/Volumes/ssd/music/朴树-[我去2000年]/01.flac", "我去2000年", "朴树", "朴树");
        normalize_album_keys(&conn);
        let k = keys(&conn);
        assert_eq!(&k[..4], vec!["未命名专辑"; 4]);
        assert_eq!(k[4], "朴树|我去2000年");
    }

    #[test]
    fn unify_album_cover_rank_and_inherit() {
        // 封面归一：组内按 manual > dir > embed 选代表并全组统一，NULL 曲目继承
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let dir = "/Volumes/ssd/music/2002.周杰伦.八度空间";
        seed(&conn, &format!("{dir}/01.半兽人.flac"), "八度空间", "周杰伦", "周杰伦");
        seed(&conn, &format!("{dir}/02.半岛铁盒.flac"), "八度空间", "周杰伦", "周杰伦");
        seed(&conn, &format!("{dir}/03.暗号.flac"), "八度空间", "周杰伦", "周杰伦");
        let k = "周杰伦|八度空间";
        // 半兽人：内嵌；半岛铁盒：目录图；暗号：无（等待继承）
        conn.execute("UPDATE tracks SET album_key=?1, cover='/covers/embed.jpg', cover_src='embed' WHERE id=1", [k]).unwrap();
        conn.execute("UPDATE tracks SET album_key=?1, cover='/covers/dir.jpg', cover_src='dir' WHERE id=2", [k]).unwrap();
        conn.execute("UPDATE tracks SET album_key=?1, cover=NULL WHERE id=3", [k]).unwrap();
        let n = unify_album_covers(&conn);
        // 半兽人被 dir 覆盖 + 暗号继承 = 2 行
        assert_eq!(n, 2);
        let rows: Vec<(Option<String>, Option<String>)> = {
            let mut st = conn.prepare("SELECT cover, cover_src FROM tracks ORDER BY id").unwrap();
            let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            r.flatten().collect()
        };
        for (c, s) in &rows {
            assert_eq!(c.as_deref(), Some("/covers/dir.jpg"), "dir 应赢 embed");
            assert_eq!(s.as_deref(), Some("dir"));
        }
        // manual 最高：在线应用的封面不被归一降级
        conn.execute("UPDATE tracks SET cover='/covers/manual.jpg', cover_src='manual' WHERE id=1", []).unwrap();
        assert_eq!(unify_album_covers(&conn), 2);
        let c1: (String, String) = conn.query_row("SELECT cover, cover_src FROM tracks WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(c1, ("/covers/manual.jpg".into(), "manual".into()));
        // 组内全统一后再跑幂等
        assert_eq!(unify_album_covers(&conn), 0);
    }

    #[test]
    fn unnamed_album_tracks_do_not_inherit_each_others_covers() {
        // 散装单曲都归「未命名专辑」，互相统一封面会张冠李戴
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        seed(&conn, "/music/a.flac", "music", "", "未知艺术家");
        seed(&conn, "/music/b.flac", "music", "段兴华", "段兴华");
        conn.execute("UPDATE tracks SET album_key='未命名专辑', cover='/covers/x.jpg', cover_src='embed' WHERE id=1", []).unwrap();
        conn.execute("UPDATE tracks SET album_key='未命名专辑' WHERE id=2", []).unwrap();
        assert_eq!(unify_album_covers(&conn), 0);
        let c2: Option<String> = conn.query_row("SELECT cover FROM tracks WHERE id=2", [], |r| r.get(0)).unwrap();
        assert_eq!(c2, None);
    }

    #[test]
    fn fill_artist_covers_uses_artist_dir_image() {
        // 整组无封面 → 歌手目录 artist.jpg 复制进缓存并全组填充
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let tmp = std::env::temp_dir().join(format!("lyra-test-artist-{}", std::process::id()));
        let singer_dir = tmp.join("朴树");
        let album_dir = singer_dir.join("生如夏花");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(singer_dir.join("artist.jpg"), b"fake-jpg-bytes").unwrap();
        let covers = tmp.join("covers");
        std::fs::create_dir_all(&covers).unwrap();
        seed(&conn, &album_dir.join("01.flac").to_string_lossy(), "生如夏花", "朴树", "朴树");
        seed(&conn, &album_dir.join("02.flac").to_string_lossy(), "生如夏花", "朴树", "朴树");
        conn.execute("UPDATE tracks SET album_key='朴树|生如夏花'", []).unwrap();
        let n = fill_artist_covers(&conn, &covers);
        assert_eq!(n, 2);
        let rows: Vec<(Option<String>, Option<String>)> = {
            let mut st = conn.prepare("SELECT cover, cover_src FROM tracks ORDER BY id").unwrap();
            let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            r.flatten().collect()
        };
        for (c, s) in &rows {
            let p = c.as_ref().unwrap();
            assert!(p.starts_with(covers.to_string_lossy().as_ref()), "应复制进 covers 缓存");
            assert!(std::path::Path::new(p).is_file());
            assert_eq!(s.as_deref(), Some("artist"));
        }
        // 幂等：有封面后不再处理
        assert_eq!(fill_artist_covers(&conn, &covers), 0);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn editing_album_updates_album_key() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        seed(&conn, "/m/x/01.flac", "旧专辑", "歌手", "歌手");
        update_fields(&conn, 1, &serde_json::json!({ "album": "新专辑" })).unwrap();
        assert_eq!(keys(&conn), vec!["歌手|新专辑"]);
        refresh_album_key(&conn, 1).unwrap();
        assert_eq!(keys(&conn), vec!["歌手|新专辑"], "refresh 应与 update_fields 一致");
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    /// 独立临时目录（按测试标签隔离，backups/ 快照随之隔离，消除并发竞争）
    fn tmp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lyra-mig-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lyra.db")
    }

    fn cleanup(p: &PathBuf) {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// backups/ 下是否存在指定起始版本的快照（文件名带时间戳，前缀匹配）
    fn snapshot_exists(p: &PathBuf, from: &str) -> bool {
        let dir = p.with_file_name("backups");
        std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("pre-migration-from-{from}-"))
                })
            })
            .unwrap_or(false)
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    fn has_col(conn: &Connection, table: &str, col: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .any(|c| c.unwrap() == col)
    }

    fn table_exists(conn: &Connection, t: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [t],
            |_| Ok(()),
        )
        .is_ok()
    }

    #[test]
    fn fresh_db_reaches_current_version() {
        let p = tmp_db("fresh");
        let conn = open(&p).unwrap();
        assert_eq!(user_version(&conn), DB_VERSION);
        assert!(has_col(&conn, "tracks", "has_tr"));
        // 全新库不做快照（无数据可丢）
        assert!(!snapshot_exists(&p, "v0"), "新库不应产生迁移前快照");
        cleanup(&p);
    }

    /// v0 老库（跳级升级）：baseline 补齐历史列 + v2 封面策略执行 + 迁移前快照落盘
    #[test]
    fn v0_legacy_db_upgrades_with_baseline_and_v2() {
        let p = tmp_db("v0");
        {
            let conn = open(&p).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
            conn.execute(
                "INSERT INTO tracks(file_path,title,artist,album,album_key,cover_src)
                 VALUES('/x.mp3','t','a','al','k','manual')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO tracks(file_path,title,artist,album,album_key,cover_src)
                 VALUES('/y.mp3','t','a','al','k','embed')",
                [],
            ).unwrap();
        }
        let conn = open(&p).unwrap();
        assert_eq!(user_version(&conn), DB_VERSION);
        // v2 策略生效：manual 保留、embed 清空
        let (m, e): (i64, Option<String>) = conn
            .query_row(
                "SELECT SUM(cover_src='manual'),
                        (SELECT cover_src FROM tracks WHERE file_path='/y.mp3') FROM tracks",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(m, 1, "manual 应保留");
        assert!(e.is_none(), "embed 应被清空");
        assert!(has_col(&conn, "tracks", "hidden"), "baseline 应补齐历史列");
        // 迁移前快照落盘
        assert!(snapshot_exists(&p, "v0"), "应有 v0 迁移前快照");
        cleanup(&p);
    }

    /// v2 库（当前线上形态）：不再重跑 v2 清空
    #[test]
    fn v2_db_skips_cover_reset() {
        let p = tmp_db("v2");
        {
            let conn = open(&p).unwrap();
            conn.execute_batch("PRAGMA user_version=2").unwrap();
            conn.execute(
                "INSERT INTO tracks(file_path,title,artist,album,album_key,cover_src)
                 VALUES('/z.mp3','t','a','al','k','embed')",
                [],
            ).unwrap();
        }
        let conn = open(&p).unwrap();
        let c: Option<String> = conn
            .query_row("SELECT cover_src FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(c.as_deref(), Some("embed"), "v>=2 不应重跑封面清空");
        assert_eq!(user_version(&conn), DB_VERSION);
        cleanup(&p);
    }

    /// 降级保护：库版本高于程序支持 → 拒绝打开
    #[test]
    fn downgrade_rejected() {
        let p = tmp_db("down");
        {
            let conn = open(&p).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version={}", DB_VERSION + 1)).unwrap();
        }
        let err = open(&p).unwrap_err();
        assert!(err.contains("升级"), "文案应引导升级：{err}");
        cleanup(&p);
    }

    /// 注入迁移（模拟未来版本）：坏条目回滚自身且版本停在上一条；好条目已提交可续跑
    #[test]
    fn injected_migration_atomic_and_versioned() {
        let p = tmp_db("inject");
        let conn = open(&p).unwrap();
        conn.execute_batch("PRAGMA user_version=2").unwrap();
        let bad: &[Migration] = &[
            (3, "好迁移", "CREATE TABLE IF NOT EXISTS mig_ok(a INTEGER)"),
            (4, "坏迁移", "THIS IS NOT SQL"),
            (5, "不该执行", "CREATE TABLE IF NOT EXISTS mig_never(a INTEGER)"),
        ];
        let err = run_migrations_with(&conn, Some(&p), bad, 5).unwrap_err();
        assert!(err.contains("v4"), "错误应定位到坏条目：{err}");
        // v3 已独立提交（表存在、版本推进）；v4 回滚自身、v5 未执行
        assert_eq!(user_version(&conn), 3);
        assert!(table_exists(&conn, "mig_ok"));
        assert!(!table_exists(&conn, "mig_never"));
        // 重试：v3 跳过，v4 仍失败，行为确定
        assert!(run_migrations_with(&conn, Some(&p), bad, 5).is_err());
        assert_eq!(user_version(&conn), 3);
        cleanup(&p);
    }

    /// duplicate column 窄化：重复加列幂等返回 false，不再吞其他错误
    #[test]
    fn duplicate_column_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        assert!(add_column(&conn, "tracks", "extra_test_col", "INTEGER").unwrap());
        assert!(!add_column(&conn, "tracks", "extra_test_col", "INTEGER").unwrap());
        // 不存在的表 → 真错误上抛（不再被吞）
        assert!(add_column(&conn, "no_such_table", "c", "INTEGER").is_err());
    }

    /// 快照覆盖：重复迁移场景只保留最近一份快照，二次迁移不报错
    #[test]
    fn snapshot_keeps_latest_only() {
        let p = tmp_db("snap");
        {
            let conn = open(&p).unwrap();
            conn.execute_batch("PRAGMA user_version=0").unwrap();
        }
        let conn = open(&p).unwrap(); // 二次 open 触发快照（时间戳新文件，清理旧的）
        assert_eq!(user_version(&conn), DB_VERSION);
        let dir = p.with_file_name("backups");
        let n = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().filter(|e| {
                e.file_name().to_string_lossy().starts_with("pre-migration-")
            }).count())
            .unwrap_or(0);
        assert_eq!(n, 1, "应只保留一份迁移前快照");
        assert!(snapshot_exists(&p, "v0"));
        cleanup(&p);
    }
}
