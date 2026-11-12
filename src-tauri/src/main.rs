#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod cache;
mod db;
mod lyrics;
mod provider;
mod replaygain;
mod scanner;
mod zhconv;

use audio::Player;
use db::Db;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Serialize, Clone)]
struct NowPlaying {
    id: i64,
    title: String,
    artist: String,
    album: String,
    cover: Option<String>,
    has_lyrics: bool,
}

#[derive(Serialize, Clone)]
struct PositionEvent {
    playing: bool,
    position: f64,
    duration: f64,
    volume: f32,
    shuffle: bool,
    repeat: u8,
    queue_index: i64,
    queue_len: usize,
    track: Option<NowPlaying>,
}

fn cur_track(conn: &rusqlite::Connection, id: i64) -> Option<NowPlaying> {
    conn.query_row(
        "SELECT id,title,artist,album,cover,(lyrics_embed IS NOT NULL AND lyrics_embed!='' OR lyrics_online IS NOT NULL AND lyrics_online!='' OR has_lrc_file=1) FROM tracks WHERE id=?1",
        [id],
        |r| {
            Ok(NowPlaying {
                id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                album: r.get(3)?,
                cover: r.get(4)?,
                has_lyrics: r.get::<_, i64>(5)? != 0,
            })
        },
    )
    .ok()
}

fn load_current(app: &AppHandle, player: &Player) -> Option<()> {
    send_current(app, player, false)
}

fn send_current(app: &AppHandle, player: &Player, paused: bool) -> Option<()> {
    let db = app.state::<Mutex<Db>>();
    let g = db.lock().unwrap();
    let idx = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    let q = player.queue.lock().unwrap();
    let tid = *q.get(idx as usize)?;
    let row: Option<(String, f64)> = g
        .0
        .prepare("SELECT file_path, duration FROM tracks WHERE id=?1")
        .unwrap()
        .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .next()
        .and_then(|x| x.ok());
    drop(g);
    drop(q);
    let (path, dur) = row?;
    apply_rg(app, player, tid);
    // 磁盘缓存解析：命中读本地缓存，未命中后台拷贝（外置盘曲库提速）
    let path = cache::resolve(app, std::path::Path::new(&path));
    if paused {
        player.preload(path, dur).ok()?;
    } else {
        // 实际播放（非预加载）→ 记入播放历史（智能歌单「最近/最常播放」数据源）
        {
            let g = app.state::<Mutex<Db>>();
            db::record_play(&g.lock().unwrap().0, tid);
        }
        player.load(path, dur).ok()?;
    }
    Some(())
}

/// 计算下一曲索引（顺序循环 / 智能随机），advance 与无缝衔接共用。
/// 智能随机：随机候选做艺人去重（避免连播同一艺人），重试 12 次后接受。
/// 生成洗牌序列：Fisher-Yates 全排列，当前曲置首；尽力做相邻同艺人交换（智能随机延续）
fn build_shuffled(app: &AppHandle, player: &Player, len: i64, cur: i64) -> Vec<i64> {
    use rand::seq::SliceRandom;
    let mut v: Vec<i64> = (0..len).collect();
    v.shuffle(&mut rand::thread_rng());
    if cur >= 0 {
        if let Some(p) = v.iter().position(|&x| x == cur) {
            v.remove(p);
            v.insert(0, cur);
        }
    }
    // 相邻去重（尽力而为）：批量取艺人映射后交换
    let artists: std::collections::HashMap<i64, String> = {
        let g = app.state::<Mutex<Db>>();
        let mut m = std::collections::HashMap::new();
        if let Ok(mut st) = g.lock().unwrap().0.prepare("SELECT id, artist FROM tracks") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                for row in rows.flatten() {
                    m.insert(row.0, row.1);
                }
            }
        }
        m
    };
    let same = |a: i64, b: i64| -> bool {
        match (artists.get(&a), artists.get(&b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    };
    let n = v.len();
    for i in 0..n.saturating_sub(1) {
        if same(v[i], v[i + 1]) {
            if let Some(j) = (i + 2..n).find(|&j| !same(v[i], v[j])) {
                v.swap(i + 1, j);
            }
        }
    }
    v
}

/// 沿洗牌轨迹导航：step=+1 下一曲 / -1 上一曲。
/// commit=false 为窥视（曲尾预排用，不动位置）；commit=true 实际步进（切歌时）。
/// 轨迹与队列长度不符时自动重洗（当前曲置首）；走到末尾列表循环语义下重洗一轮。
fn shuffle_nav(app: &AppHandle, player: &Player, cur: i64, step: i64, commit: bool) -> Option<i64> {
    let len = player.queue.lock().unwrap().len() as i64;
    if len <= 1 {
        return None;
    }
    {
        let mut seq = player.shuffle_seq.lock().unwrap();
        if seq.len() as i64 != len {
            *seq = build_shuffled(app, player, len, cur);
            player.shuffle_pos.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let mut np;
    {
        let mut seq = player.shuffle_seq.lock().unwrap();
        let pos = player.shuffle_pos.load(std::sync::atomic::Ordering::Relaxed);
        np = pos + step;
        if np >= len {
            // 列表循环：轨迹走完，重洗一轮（当前曲仍在首）
            *seq = build_shuffled(app, player, len, cur);
            player.shuffle_pos.store(0, std::sync::atomic::Ordering::Relaxed);
            np = 1;
        }
        if np < 0 {
            np = len - 1; // 上一曲越过起点：回卷到尾
        }
        let r = seq.get(np as usize).copied();
        if !commit {
            return r;
        }
    }
    player.shuffle_pos.store(np, std::sync::atomic::Ordering::Relaxed);
    player.shuffle_seq.lock().unwrap().get(np as usize).copied()
}

fn compute_next_index(app: &AppHandle, player: &Player, cur: i64) -> Option<i64> {
    let len = player.queue.lock().unwrap().len() as i64;
    if len == 0 {
        return None;
    }
    if player.shuffle_mode.load(std::sync::atomic::Ordering::Relaxed) == 1 {
        return shuffle_nav(app, player, cur, 1, false); // 随机开启：轨迹窥视
    }
    Some(if cur + 1 >= len { 0 } else { cur + 1 })
}

fn advance(app: &AppHandle, player: &Player, step: i64) -> bool {
    let q = player.queue.lock().unwrap();
    let len = q.len() as i64;
    drop(q);
    if len == 0 {
        return false;
    }
    let cur = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    let next_idx = if player.shuffle_mode.load(std::sync::atomic::Ordering::Relaxed) == 1 {
        shuffle_nav(app, player, cur, step, true).unwrap_or(0)
    } else if cur + step >= len {
        0
    } else if cur + step < 0 {
        len - 1
    } else {
        cur + step
    };
    player.cur_index.store(next_idx, std::sync::atomic::Ordering::Relaxed);
    load_current(app, player).is_some()
}

// ================= Commands =================

/// 同步执行一次增量扫描（手动/自动/watcher 共用；quiet=静默不发进度）
fn scan_sync(app: &AppHandle, root: &std::path::Path, quiet: bool) -> Result<(usize, usize, usize), String> {
    if !root.is_dir() {
        return Err("目录不存在".into());
    }
    let db_path = db::db_path(app);
    let covers = db::covers_dir(app);
    let app2 = app.clone();
    let root2 = root.to_path_buf();
    let mut conn = db::open(&db_path);
    let (done, changed) = scanner::scan(&mut conn, &root2, &covers, &app2, true, quiet);
    let gone = scanner::prune(&conn, &root2);
    // 新写入/更新过的曲目按「目录 + 专辑名」再归并一次，避免标签残缺导致同名专辑分裂
    db::normalize_album_keys(&conn);
    // 专辑封面归一（manual > dir > embed > artist）：保证同专辑播放时封面一致
    db::unify_album_covers(&conn);
    // 整组无封面的专辑兜底歌手目录图（artist.jpg 等）
    db::fill_artist_covers(&conn, &covers);
    Ok((done, changed, gone))
}

/// 全局扫描互斥（手动与 watcher 触发不并发）
static SCANNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn try_scan(app: &AppHandle, root: &std::path::Path, quiet: bool) -> Option<(usize, usize, usize)> {
    use std::sync::atomic::Ordering;
    if SCANNING.swap(true, Ordering::SeqCst) {
        return None; // 已有扫描进行中，跳过本次触发
    }
    let r = scan_sync(app, root, quiet);
    SCANNING.store(false, Ordering::SeqCst);
    r.ok()
}

/// notify watcher 重建：按当前 scan_roots 建监听 + debounce 线程（设计文档 4.4，2s 静默窗口）
fn rebuild_watcher(app: &AppHandle) {
    use notify::Watcher;
    let roots = {
        let g = app.state::<Mutex<Db>>();
        let gl = g.lock().unwrap();
        db::scan_roots(&gl.0)
    };
    let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
    let mut watcher = match notify::RecommendedWatcher::new(tx, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[watcher] 创建失败: {e}");
            return;
        }
    };
    let mut watched = 0;
    for r in &roots {
        if watcher
            .watch(std::path::Path::new(r), notify::RecursiveMode::Recursive)
            .is_ok()
        {
            watched += 1;
        }
    }
    eprintln!("[watcher] 监听 {watched} 个目录");
    let holder = app.state::<Mutex<Option<notify::RecommendedWatcher>>>();
    let old = holder.lock().unwrap().replace(watcher);
    drop(old); // 旧 watcher drop → 其 debounce 线程 channel 断开自动退出

    let handle = app.clone();
    std::thread::spawn(move || {
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut last_event = std::time::Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(Ok(ev)) => {
                    let roots = {
                        let g = handle.state::<Mutex<Db>>();
                        let gl = g.lock().unwrap();
                        db::scan_roots(&gl.0)
                    };
                    for p in &ev.paths {
                        for r in &roots {
                            if p.starts_with(r) {
                                pending.insert(r.clone());
                            }
                        }
                    }
                    last_event = std::time::Instant::now();
                }
                Ok(Err(_e)) => { /* notify 内部错误（如权限）忽略 */ }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 2s 静默窗口结束 → 对受影响目录增量扫描
                    if !pending.is_empty() && last_event.elapsed() >= std::time::Duration::from_secs(2) {
                        let roots: Vec<String> = pending.drain().collect();
                        for r in roots {
                            if let Some((found, parsed, gone)) = try_scan(&handle, std::path::Path::new(&r), false) {
                                if parsed > 0 || gone > 0 {
                                    eprintln!("[watcher] 自动扫描 {r}: 发现 {found} · 更新 {parsed} · 移除 {gone}");
                                    let _ = handle.emit(
                                        "scan-done",
                                        serde_json::json!({ "changed": parsed, "removed": gone, "auto": true }),
                                    );
                                }
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

#[tauri::command]
async fn pick_and_scan(app: AppHandle) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let folder = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .ok_or("未选择目录")?;
    let root = folder
        .as_path()
        .map(|p| p.to_path_buf())
        .ok_or("无效目录")?;
    scan_dir(app.clone(), root).await
}

#[tauri::command]
async fn scan_dir(app: AppHandle, root: std::path::PathBuf) -> Result<serde_json::Value, String> {
    let app2 = app.clone();
    let root2 = root.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        try_scan(&app2, &root2, false).ok_or_else(|| "已有扫描进行中".to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    // 记住根目录 + 重建 watcher
    {
        let g = app.state::<Mutex<Db>>();
        db::add_scan_root(&g.lock().unwrap().0, &root.to_string_lossy());
    }
    rebuild_watcher(&app);
    Ok(serde_json::json!({ "scanned": res.0, "changed": res.1, "removed": res.2 }))
}

#[tauri::command]
fn get_scan_roots(db: State<'_, Mutex<Db>>) -> Vec<String> {
    let g = db.lock().unwrap();
    db::scan_roots(&g.0)
}

#[tauri::command]
fn remove_scan_root(app: AppHandle, db: State<'_, Mutex<Db>>, path: String) -> Result<(), String> {
    {
        let g = db.lock().unwrap();
        db::remove_scan_root(&g.0, &path);
    }
    rebuild_watcher(&app);
    Ok(())
}

#[tauri::command]
fn albums(db: State<'_, Mutex<Db>>, t2s: Option<bool>) -> Vec<db::Album> {
    let conn = &db.lock().unwrap().0;
    let mut v = db::albums(conn);
    if t2s.unwrap_or(true) {
        for a in v.iter_mut() {
            a.album = zhconv::t2s(&a.album);
            a.artist = zhconv::t2s(&a.artist);
        }
    }
    v
}

#[tauri::command]
fn all_tracks(db: State<'_, Mutex<Db>>, t2s: Option<bool>) -> Vec<db::Track> {
    let conn = &db.lock().unwrap().0;
    let mut v = db::all_tracks(conn);
    if t2s.unwrap_or(true) {
        for t in v.iter_mut() {
            t.title = zhconv::t2s(&t.title);
            t.artist = zhconv::t2s(&t.artist);
            t.album = zhconv::t2s(&t.album);
        }
    }
    v
}

#[tauri::command]
fn list_playlists(db: State<'_, Mutex<Db>>) -> Vec<db::Playlist> {
    let conn = &db.lock().unwrap().0;
    db::playlists(conn)
}

#[tauri::command]
fn playlist_detail(db: State<'_, Mutex<Db>>, pid: i64) -> Vec<db::Track> {
    let conn = &db.lock().unwrap().0;
    db::playlist_tracks(conn, pid)
}

/// 导出歌单为 M3U8：保存对话框选位置后写文件（WebView 的 <a download> 保存位置不受控）
#[tauri::command]
async fn export_playlist_m3u(app: AppHandle, db: State<'_, Mutex<Db>>, pid: i64) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;
    let (name, ts) = {
        let g = db.lock().unwrap();
        let conn = &g.0;
        let name: String = conn
            .query_row("SELECT name FROM playlists WHERE id=?1", [pid], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        (name, db::playlist_tracks(conn, pid))
    };
    if ts.is_empty() {
        return Err("歌单为空，无可导出内容".into());
    }
    let target = app
        .dialog()
        .file()
        .add_filter("M3U8 播放列表", &["m3u8"])
        .set_file_name(format!("{}.m3u8", name))
        .blocking_save_file()
        .ok_or("未选择位置")?;
    let out = target.as_path().ok_or("无效路径")?.to_path_buf();
    let mut lines = String::from("#EXTM3U\n");
    for t in &ts {
        lines.push_str(&format!("#EXTINF:{},{:.3} - {}\n{}\n", t.duration.round() as i64, t.artist, t.title, t.file_path));
    }
    std::fs::write(&out, lines).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!("已导出 {}（{} 首）", out.display(), ts.len()))
}

#[tauri::command]
fn create_playlist(db: State<'_, Mutex<Db>>, name: String) -> i64 {
    let conn = &db.lock().unwrap().0;
    db::create_playlist(conn, &name)
}

#[tauri::command]
fn rename_playlist(db: State<'_, Mutex<Db>>, id: i64, name: String) {
    let conn = &db.lock().unwrap().0;
    db::rename_playlist(conn, id, &name);
}

#[tauri::command]
fn delete_playlist(db: State<'_, Mutex<Db>>, id: i64) {
    let conn = &db.lock().unwrap().0;
    db::delete_playlist(conn, id);
}

#[tauri::command]
fn add_to_playlist(db: State<'_, Mutex<Db>>, pid: i64, tid: i64) -> bool {
    let conn = &db.lock().unwrap().0;
    db::add_to_playlist(conn, pid, tid)
}

#[tauri::command]
fn remove_playlist_track(db: State<'_, Mutex<Db>>, pid: i64, idx: i64) {
    let conn = &db.lock().unwrap().0;
    db::remove_at(conn, pid, idx);
}

#[tauri::command]
fn move_playlist_track(db: State<'_, Mutex<Db>>, pid: i64, from: i64, to: i64) {
    let conn = &db.lock().unwrap().0;
    db::move_track(conn, pid, from, to);
}

// ================= 播放 Commands =================

#[tauri::command]
fn play_tracks(
    app: AppHandle,
    player: State<'_, Player>,
    ids: Vec<i64>,
    start: usize,
) -> Result<(), String> {
    if ids.is_empty() {
        return Err("队列为空".into());
    }
    *player.queue.lock().unwrap() = ids;
    player.cur_index.store(start as i64, std::sync::atomic::Ordering::Relaxed);
    *player.queue_snapshot.lock().unwrap() = None; // 显式换了队列：旧快照作废
    player.shuffle_seq.lock().unwrap().clear();
    player.prefetch_idx.store(-1, std::sync::atomic::Ordering::Relaxed);
    load_current(&app, &player).ok_or("加载音频失败")?;
    Ok(())
}

/// 队列视图（后端为唯一真源）：面板渲染/会话恢复共用。
/// 随机模式开启时队列可能是全库规模，批量 IN 查询（分块避开 SQLite 变量上限），
/// 避免逐首 prepare+query 造成面板打开时卡顿
#[tauri::command]
fn queue_list(player: State<'_, Player>, db: State<'_, Mutex<Db>>) -> serde_json::Value {
    let q = player.queue.lock().unwrap().clone();
    let idx = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    let g = db.lock().unwrap();
    let mut map: std::collections::HashMap<i64, NowPlaying> = std::collections::HashMap::new();
    for chunk in q.chunks(900) {
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id,title,artist,album,cover,(lyrics_embed IS NOT NULL AND lyrics_embed!='' OR lyrics_online IS NOT NULL AND lyrics_online!='' OR has_lrc_file=1) FROM tracks WHERE id IN ({ph})"
        );
        if let Ok(mut st) = g.0.prepare(&sql) {
            if let Ok(rows) = st.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok(NowPlaying {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    artist: r.get(2)?,
                    album: r.get(3)?,
                    cover: r.get(4)?,
                    has_lyrics: r.get::<_, i64>(5)? != 0,
                })
            }) {
                for np in rows.flatten() {
                    map.insert(np.id, np);
                }
            }
        }
    }
    let tracks: Vec<NowPlaying> = q.iter().filter_map(|id| map.get(id).cloned()).collect();
    serde_json::json!({ "tracks": tracks, "index": idx })
}

/// 从队列移除：当前曲之前的索引左移；移除当前曲=切下一首
#[tauri::command]
fn queue_remove(app: AppHandle, player: State<'_, Player>, index: i64) -> Result<(), String> {
    let mut q = player.queue.lock().unwrap();
    if index < 0 || index as usize >= q.len() {
        return Err("索引越界".into());
    }
    let cur = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    q.remove(index as usize);
    *player.queue_snapshot.lock().unwrap() = None; // 队列被手动编辑：旧快照作废
    player.shuffle_seq.lock().unwrap().clear();
    if index == cur {
        // 移除的是正在播的：队尾则停止，否则切下一首
        if q.is_empty() {
            player.cur_index.store(-1, std::sync::atomic::Ordering::Relaxed);
            player.pause();
        } else {
            if cur >= q.len() as i64 {
                player.cur_index.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            drop(q);
            let _ = load_current(&app, &player);
        }
    } else {
        if index < cur {
            player.cur_index.store(cur - 1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(())
}

/// 队列内跳转：改 index 并加载（不重建队列）
#[tauri::command]
fn queue_jump(app: AppHandle, player: State<'_, Player>, index: i64) -> Result<(), String> {
    let len = player.queue.lock().unwrap().len() as i64;
    if index < 0 || index >= len {
        return Err("索引越界".into());
    }
    player.cur_index.store(index, std::sync::atomic::Ordering::Relaxed);
    player.shuffle_seq.lock().unwrap().clear();
    load_current(&app, &player).ok_or("加载音频失败")?;
    Ok(())
}

#[tauri::command]
fn toggle_play(app: AppHandle, player: State<'_, Player>) {
    // 未加载过 或 已自然播完：重新加载当前曲（或从队列头开始）
    if !player.loaded() || player.finished() {
        if player.queue.lock().unwrap().is_empty() {
            return;
        }
        if player.cur_index.load(std::sync::atomic::Ordering::Relaxed) < 0 {
            player.cur_index.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = load_current(&app, &player);
        return;
    }
    if player.is_playing() {
        player.pause();
    } else {
        player.play();
    }
}

#[tauri::command]
fn next_track(app: AppHandle, player: State<'_, Player>) {
    advance(&app, &player, 1);
}

#[tauri::command]
fn prev_track(app: AppHandle, player: State<'_, Player>) {
    // 无论进度多少直接切上一曲（与媒体键/托盘菜单行为一致）
    advance(&app, &player, -1);
}

#[tauri::command]
fn seek(player: State<'_, Player>, sec: f64) {
    player.seek(sec);
}

#[tauri::command]
fn set_volume(player: State<'_, Player>, v: f32) {
    player.set_volume(v);
}

#[tauri::command]
fn set_shuffle(app: AppHandle, player: State<'_, Player>, on: bool) {
    // 兼容旧接口：on→歌曲随机，off→关闭
    set_shuffle_mode_impl(&app, &player, if on { 1 } else { 0 });
}

/// 本地歌手图约定：歌手任一曲目的上两级目录（歌手目录）内 artist.jpg/cover.jpg/folder.jpg
/// 返回 "local:" 前缀路径（前端转 asset 协议）
fn local_artist_imgs(conn: &rusqlite::Connection, name: &str) -> Vec<String> {
    let like = format!("%{}%", name);
    let Ok(mut st) = conn.prepare(
        "SELECT DISTINCT file_path FROM tracks WHERE artist LIKE ?1 OR album_artist LIKE ?1 LIMIT 40",
    ) else { return vec![] };
    let Ok(paths) = st.query_map([&like], |r| r.get::<_, String>(0)) else { return vec![] };
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    for p in paths.flatten() {
        let pb = std::path::PathBuf::from(&p);
        // 曲目目录（专辑目录）与其父目录（歌手目录）都查
        for base in [pb.parent().map(|x| x.to_path_buf()), pb.parent().and_then(|x| x.parent()).map(|x| x.to_path_buf())].into_iter().flatten() {
            if seen.iter().any(|s| *s == base) { continue; }
            seen.push(base.clone());
            for f in ["artist.jpg", "artist.png", "artist.jpeg", "cover.jpg", "cover.png", "folder.jpg"] {
                let c = base.join(f);
                if c.is_file() {
                    let s = format!("local:{}", c.display());
                    if !out.iter().any(|x| x == &s) { out.push(s); }
                }
            }
        }
    }
    out.into_iter().take(4).collect()
}

/// 已缓存歌手图批量返回（艺术家列表卡片用）
#[tauri::command]
fn artist_cached_imgs(db: State<'_, Mutex<Db>>) -> std::collections::HashMap<String, String> {
    let g = db.lock().unwrap();
    let mut m = std::collections::HashMap::new();
    let online: std::collections::HashMap<String, String> = {
        let mut m2 = std::collections::HashMap::new();
        if let Ok(mut st) = g.0.prepare("SELECT name, img FROM artist_cache WHERE img IS NOT NULL AND img!=''") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for x in rows.flatten() {
                    m2.insert(x.0, x.1);
                }
            }
        }
        m2
    };
    // 本地约定图（artist.jpg）优先，在线缓存兜底；每个已知名字都试本地
    let mut names: Vec<String> = online.keys().cloned().collect();
    if let Ok(mut st) = g.0.prepare("SELECT DISTINCT COALESCE(NULLIF(album_artist,''), artist) FROM tracks") {
        if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
            for n in rows.flatten() {
                if !names.iter().any(|x| x == &n) { names.push(n); }
            }
        }
    }
    for n in names {
        let local = local_artist_imgs(&g.0, &n);
        if let Some(first) = local.first() {
            m.insert(n, first.clone());
        } else if let Some(o) = online.get(&n) {
            m.insert(n, o.clone());
        }
    }
    m
}

/// 用系统默认浏览器打开外链
#[tauri::command]
fn open_external(url: String) {
    if !url.starts_with("https://") { return; }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", &url]).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
}

#[tauri::command]
fn get_keys(app: AppHandle) -> serde_json::Value {
    // 未配置过任何 key 时 config 里无 keys 节 → 返回空对象而非 Null（前端 null[id] 会炸）
    match read_cfg(&app)["keys"].clone() {
        v @ serde_json::Value::Object(_) => v,
        _ => serde_json::json!({}),
    }
}

#[tauri::command]
fn set_key(app: AppHandle, id: String, key: String) {
    let mut cfg = read_cfg(&app);
    if cfg["keys"].is_null() {
        cfg["keys"] = serde_json::json!({});
    }
    let k = key.trim().to_string();
    if k.is_empty() {
        cfg["keys"][&id].take();
        cfg["keys"].as_object_mut().unwrap().remove(&id);
    } else {
        cfg["keys"][&id] = serde_json::json!(k);
    }
    write_cfg(&app, &cfg);
}

#[tauri::command]
fn get_config(app: AppHandle) -> serde_json::Value {
    read_cfg(&app)
}

#[tauri::command]
fn set_shuffle_mode(app: AppHandle, player: State<'_, Player>, mode: i32) {
    set_shuffle_mode_impl(&app, &player, mode.clamp(0, 1));
}

fn set_shuffle_mode_impl(app: &AppHandle, player: &Player, mode: i32) {
    let mode = if mode > 0 { 1 } else { 0 };
    let prev = player.shuffle_mode.swap(mode, std::sync::atomic::Ordering::Relaxed);
    player.shuffle.store(mode > 0, std::sync::atomic::Ordering::Relaxed);
    if mode == 1 && prev == 0 {
        // 开启：队列整体换成全库随机（当前曲置首），原队列留快照
        enable_all_random(app, player);
    } else if mode == 0 && prev == 1 {
        // 关闭：还原开启前的队列，保住播放列表上下文
        restore_queue_snapshot(player);
    }
    if let Ok(mut seq) = player.shuffle_seq.lock() {
        seq.clear(); // 强制重洗（以当前曲为起点）
    }
    player.shuffle_pos.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut cfg = read_cfg(app);
    cfg["shuffle_mode"] = serde_json::json!(mode);
    cfg["shuffle"] = serde_json::json!(mode > 0);
    write_cfg(app, &cfg);
}

/// 开启「所有歌曲随机」：全库洗牌重建队列，当前曲放到队首（正在播的音频不中断）
fn enable_all_random(app: &AppHandle, player: &Player) {
    let ids: Vec<i64> = {
        let st = app.state::<Mutex<Db>>();
        let guard = st.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut pre) = guard.0.prepare("SELECT id FROM tracks WHERE hidden=0 ORDER BY RANDOM()") {
            if let Ok(rows) = pre.query_map([], |r| r.get::<_, i64>(0)) {
                out = rows.flatten().collect();
            }
        }
        out
    };
    if ids.is_empty() {
        return; // 曲库为空：只改开关状态，不动队列
    }
    let cur = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    let cur_tid = if cur >= 0 {
        player.queue.lock().unwrap().get(cur as usize).copied()
    } else {
        None
    };
    {
        let mut snap = player.queue_snapshot.lock().unwrap();
        if snap.is_none() {
            let old = player.queue.lock().unwrap().clone();
            if !old.is_empty() {
                *snap = Some(old);
            }
        }
    }
    let mut newq = ids;
    if let Some(t) = cur_tid {
        if let Some(p) = newq.iter().position(|&x| x == t) {
            newq.remove(p);
        }
        newq.insert(0, t); // 当前曲置首：队列变了但不打断正在播的音频
    }
    *player.queue.lock().unwrap() = newq;
    // 当前曲已在新队列首位；原本没在播则保持 -1（等用户点播）
    player.cur_index.store(if cur_tid.is_some() { 0 } else { -1 }, std::sync::atomic::Ordering::Relaxed);
    player.prefetch_idx.store(-1, std::sync::atomic::Ordering::Relaxed);
}

/// 关闭随机：队列还原为快照；当前曲若不在原队列内（随机跳过去了）则插到队首，保证索引有效
fn restore_queue_snapshot(player: &Player) {
    let snap = player.queue_snapshot.lock().unwrap().take();
    let Some(mut q) = snap else { return };
    let cur = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
    let cur_tid = if cur >= 0 {
        player.queue.lock().unwrap().get(cur as usize).copied()
    } else {
        None
    };
    let idx = match cur_tid {
        Some(t) => match q.iter().position(|&x| x == t) {
            Some(p) => p as i64,
            None => {
                q.insert(0, t);
                0
            }
        },
        None => -1,
    };
    *player.queue.lock().unwrap() = q;
    player.cur_index.store(idx, std::sync::atomic::Ordering::Relaxed);
    player.prefetch_idx.store(-1, std::sync::atomic::Ordering::Relaxed);
}

/// 循环模式：0=关 1=列表循环 2=单曲循环
#[tauri::command]
fn set_repeat(app: AppHandle, player: State<'_, Player>, mode: u8) {
    let mode = mode.min(2);
    player.repeat_mode.store(mode, std::sync::atomic::Ordering::Relaxed);
    let mut cfg = read_cfg(&app);
    cfg["repeat"] = serde_json::json!(mode);
    write_cfg(&app, &cfg);
}

// ================= 收藏 / 智能歌单 =================

#[tauri::command]
fn toggle_like(db: State<'_, Mutex<Db>>, track_id: i64) -> bool {
    let g = db.lock().unwrap();
    db::toggle_like(&g.0, track_id)
}

#[tauri::command]
fn smart_list(db: State<'_, Mutex<Db>>, kind: String) -> Vec<db::Track> {
    let g = db.lock().unwrap();
    db::smart_tracks(&g.0, &kind)
}

/// 隐藏曲目（cue 整轨等长音频）：所有列表不再展示，队列里也一并移除
#[tauri::command]
fn set_track_hidden(app: AppHandle, player: State<'_, Player>, db: State<'_, Mutex<Db>>, track_id: i64, hidden: bool) -> Result<(), String> {
    {
        let g = db.lock().unwrap();
        db::set_hidden(&g.0, track_id, hidden).map_err(|e| e.to_string())?;
    }
    if hidden {
        // 正在队列里（含正在播）的隐藏曲：直接移除，避免播完又跳回来
        let mut q = player.queue.lock().unwrap();
        let cur = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(pos) = q.iter().position(|&x| x == track_id) {
            q.remove(pos);
            player.shuffle_seq.lock().unwrap().clear();
            if q.is_empty() {
                player.cur_index.store(-1, std::sync::atomic::Ordering::Relaxed);
                player.pause();
            } else if pos as i64 == cur {
                // 移除的是正在播的：切到下一首（队尾回卷）
                let next = if cur >= q.len() as i64 { 0 } else { cur };
                player.cur_index.store(next, std::sync::atomic::Ordering::Relaxed);
                drop(q);
                let _ = load_current(&app, &player);
            } else if (pos as i64) < cur {
                player.cur_index.store(cur - 1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

/// 已隐藏曲目列表（设置 → 已隐藏）
#[tauri::command]
fn hidden_tracks(db: State<'_, Mutex<Db>>) -> Vec<db::Track> {
    let g = db.lock().unwrap();
    db::hidden_tracks(&g.0)
}

// ================= M3U8 导入 / 封面在线获取 =================

/// M3U8 导入（设计文档 6.4）：选文件 → 路径回链库内曲目 → 建歌单；缺失曲目计数报告
#[tauri::command]
async fn import_m3u8(app: AppHandle, db: State<'_, Mutex<Db>>) -> Result<serde_json::Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let file = app
        .dialog()
        .file()
        .add_filter("M3U/M3U8 歌单", &["m3u8", "m3u"])
        .blocking_pick_file()
        .ok_or("未选择文件")?;
    let path = file.as_path().ok_or("无效路径")?.to_path_buf();
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("读取失败: {e}"))?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "导入歌单".into());
    // 解析：去 #EXTINF/#EXTM3U 行，其余按路径
    let paths: Vec<String> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect();
    // 回链库内曲目
    let (ids, missing) = {
        let g = db.lock().unwrap();
        let mut ids = Vec::new();
        let mut missing = 0usize;
        for p in &paths {
            let hit: Option<i64> = g
                .0
                .query_row("SELECT id FROM tracks WHERE file_path=?1", [p], |r| r.get(0))
                .ok();
            match hit {
                Some(id) => ids.push(id),
                None => missing += 1,
            }
        }
        (ids, missing)
    };
    let total = paths.len();
    if ids.is_empty() {
        return Err(format!("歌单内 {total} 条路径均未匹配到曲库（M3U 仅支持 UTF-8，且需绝对路径与曲库一致）"));
    }
    let pid = {
        let g = db.lock().unwrap();
        db::create_playlist(&g.0, &name)
    };
    let matched = ids.len();
    for tid in ids {
        let g = db.lock().unwrap();
        let _ = db::add_to_playlist(&g.0, pid, tid);
    }
    Ok(serde_json::json!({ "name": name, "matched": matched, "missing": missing, "total": total }))
}

/// 专辑封面候选：自定义源优先 + iTunes 兜底
#[tauri::command]
async fn fetch_album_covers(
    app: AppHandle,
    db: State<'_, Mutex<Db>>,
    album_key: String,
    base_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let (artist, album) = {
        let g = db.lock().unwrap();
        g.0
            .query_row(
                "SELECT COALESCE(NULLIF(album_artist,''), artist), MIN(album) FROM tracks WHERE album_key=?1 GROUP BY album_key",
                [&album_key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(|e| e.to_string())?
    };
    // 各渠道并行收集（面板按源分组展示；每源返回状态，失败原因可见）
    let keys = read_cfg(&app)["keys"].clone();
    let a1 = artist.clone(); let al1 = album.clone();
    let base1 = base_url.clone().filter(|s| !s.trim().is_empty());
    let t_custom = tokio::task::spawn(async move {
        match base1 {
            Some(b) => lyrics::fetch_custom_cover(&a1, &al1, &b).await
                .map_err(|e| e.to_string()).unwrap_or_default(),
            None => vec![],
        }
    });
    let a2 = artist.clone(); let al2 = album.clone();
    let t_deezer = tokio::task::spawn(async move {
        provider::search_deezer_art(&a2, &al2).await.unwrap_or_default()
    });
    let a3 = artist.clone(); let al3 = album.clone();
    let lf = keys["lastfm"].as_str().unwrap_or("").trim().to_string();
    let t_lastfm = tokio::task::spawn(async move {
        if lf.is_empty() { return vec![]; }
        provider::search_lastfm_art(&a3, &al3, &lf).await.unwrap_or_default()
    });
    let a4 = artist.clone(); let al4 = album.clone();
    let t_itunes = tokio::task::spawn(async move {
        provider::search_album_art(&a4, &al4).await.unwrap_or_default()
    });
    // 每源状态（0 张=失败/无结果，前端展示失败提示）
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut status: Vec<serde_json::Value> = Vec::new();
    let custom = t_custom.await.unwrap_or_default();
    status.push(serde_json::json!({ "src": "自定义源", "n": custom.len(), "skipped": base_url.is_none() || base_url.as_deref().unwrap_or("").trim().is_empty() }));
    for u in custom { urls.push(("自定义源".into(), u)); }
    let dz = t_deezer.await.unwrap_or_default();
    status.push(serde_json::json!({ "src": "Deezer", "n": dz.len() }));
    for u in dz { urls.push(("Deezer".into(), u)); }
    let lfm = t_lastfm.await.unwrap_or_default();
    status.push(serde_json::json!({ "src": "Last.fm", "n": lfm.len(), "skipped": keys["lastfm"].as_str().unwrap_or("").trim().is_empty() }));
    for u in lfm { urls.push(("Last.fm".into(), u)); }
    let it = t_itunes.await.unwrap_or_default();
    status.push(serde_json::json!({ "src": "iTunes", "n": it.len() }));
    for u in it { urls.push(("iTunes".into(), u)); }
    urls.dedup();
    Ok(serde_json::json!({
        "covers": urls.into_iter().map(|(src, url)| serde_json::json!({ "src": src, "url": url })).collect::<Vec<_>>(),
        "status": status,
    }))
}

/// 应用专辑封面：下载（含 data:base64）入缓存 → 更新该专辑全部曲目
#[tauri::command]
async fn apply_album_cover(app: AppHandle, db: State<'_, Mutex<Db>>, album_key: String, url: String) -> Result<String, String> {
    let bytes: Vec<u8> = if let Some(b64) = url.strip_prefix("data:") {
        let payload = b64.split(",").nth(1).unwrap_or("");
        base64_decode(payload).ok_or("data URL 解码失败")?
    } else {
        reqwest::get(&url)
            .await
            .map_err(|e| format!("下载失败: {e}"))?
            .bytes()
            .await
            .map_err(|e| e.to_string())?
            .to_vec()
    };
    let ext = if url.contains(".png") { "png" } else { "jpg" };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hasher;
    h.write(&bytes);
    let name = format!("{:016x}.{}", h.finish(), ext);
    let dir = db::covers_dir(&app);
    let path = dir.join(&name);
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    let path_str = path.to_string_lossy().into_owned();
    let n = {
        let g = db.lock().unwrap();
        g.0
            .execute("UPDATE tracks SET cover=?1, cover_src='manual' WHERE album_key=?2", rusqlite::params![path_str, album_key])
            .map_err(|e| e.to_string())?
    };
    if n == 0 {
        return Err("该专辑无曲目".into());
    }
    let album_name = {
        let g = db.lock().unwrap();
        g.0.query_row(
            "SELECT MIN(album) FROM tracks WHERE album_key=?1",
            [&album_key],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| album_key.clone())
    };
    auto_backup(&db, &app, "cover", &album_name);
    Ok(path_str)
}

// ================= 自动备份（操作级快照，最多保留 10 份） =================

const AUTO_BK_KEEP: usize = 10;

fn auto_bk_dir(app: &AppHandle) -> std::path::PathBuf {
    use tauri::Manager;
    let d = app.path().app_data_dir().expect("no app data dir").join("backups");
    std::fs::create_dir_all(&d).ok();
    d
}

/// 操作级自动备份：封面 / 歌手 / 歌词 / 翻译获取成功后调用。
/// 打包 lyra.db + session.json + config.json（不含 covers：封面缓存只增不减、
/// hash 命名与任何历史库版本兼容，备份从几十 MB 降到 KB 级、秒级完成）。
/// 文件名 auto-<毫秒时间戳>-<trigger>.zip，最多保留最新 10 份。
/// detail 记录操作对象（歌名/专辑名/歌手名），存 index.json 供列表展示。
/// 失败静默（自动备份不应影响主操作的成功反馈）。
fn auto_backup(db: &State<'_, Mutex<Db>>, app: &AppHandle, trigger: &str, detail: &str) {
    use tauri::Manager;
    {
        let g = db.lock().unwrap();
        let _ = g.0.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }
    let data = app.path().app_data_dir().expect("no app data dir");
    let dir = auto_bk_dir(app);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let out = dir.join(format!("auto-{}-{}.zip", ms, trigger));
    if out.exists() {
        return; // 同毫秒重复触发，不重复备份
    }
    let Ok(z) = std::process::Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(&out)
        .arg("lyra.db")
        .arg("session.json")
        .arg("config.json")
        .current_dir(&data)
        .output()
    else {
        return;
    };
    if !z.status.success() {
        let _ = std::fs::remove_file(&out);
        return;
    }
    prune_auto_backups(&dir);
    update_bk_index(&dir, ms, detail);
}

/// 详情索引 backups/index.json：[{ts, detail}]。zip 文件名只放 trigger（白名单防穿越），
/// 操作对象（中文歌名等）放索引；列表时按 ts 合并，zip 被手动删掉的条目自动清理
fn read_bk_index(dir: &std::path::Path) -> std::collections::HashMap<u128, String> {
    std::fs::read_to_string(dir.join("index.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .map(|v| {
            v.into_iter()
                .filter_map(|e| {
                    let ts = e["ts"].as_u64()? as u128;
                    Some((ts, e["detail"].as_str().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn update_bk_index(dir: &std::path::Path, ts: u128, detail: &str) {
    let mut idx = read_bk_index(dir);
    idx.insert(ts, detail.to_string());
    // 只保留仍有 zip 的条目（prune 删除的同步清理）
    let alive: std::collections::HashSet<u128> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| parse_auto_bk_name(&e.file_name().to_string_lossy()).map(|(t, _)| t))
                .collect()
        })
        .unwrap_or_default();
    let arr: Vec<serde_json::Value> = idx
        .into_iter()
        .filter(|(t, _)| alive.contains(t))
        .map(|(t, d)| serde_json::json!({ "ts": t as u64, "detail": d }))
        .collect();
    let _ = std::fs::write(dir.join("index.json"), serde_json::to_string(&arr).unwrap_or_default());
}

/// 只保留最新 AUTO_BK_KEEP 份（按文件名时间戳降序）
fn prune_auto_backups(dir: &std::path::Path) {
    let mut list: Vec<(u128, std::path::PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    parse_auto_bk_name(&name).map(|(ts, _)| (ts, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    list.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in list.into_iter().skip(AUTO_BK_KEEP) {
        let _ = std::fs::remove_file(p);
    }
}

/// 解析 auto-<ms>-<trigger>.zip → (时间戳, trigger)
fn parse_auto_bk_name(name: &str) -> Option<(u128, String)> {
    let rest = name.strip_prefix("auto-")?.strip_suffix(".zip")?;
    let (ts, trigger) = rest.split_once('-')?;
    let ts: u128 = ts.parse().ok()?;
    if trigger.is_empty() || !trigger.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return None;
    }
    Some((ts, trigger.to_string()))
}

/// 自动备份列表（时间戳降序）：[{ ts, trigger, detail }]
#[tauri::command]
fn list_auto_backups(app: AppHandle) -> Vec<serde_json::Value> {
    let dir = auto_bk_dir(&app);
    let idx = read_bk_index(&dir);
    let mut list: Vec<(u128, String)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| parse_auto_bk_name(&e.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    list.sort_by(|a, b| b.0.cmp(&a.0));
    list.into_iter()
        .map(|(ts, trigger)| {
            serde_json::json!({
                "ts": ts as u64,
                "trigger": trigger,
                "detail": idx.get(&ts).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

/// 恢复自动备份：解包覆盖 lyra.db / config.json / session.json（不动 covers），需重启生效。
/// 只接受文件名（防路径穿越），且必须是 backups 目录内的 auto- 备份
#[tauri::command]
async fn restore_auto_backup(app: AppHandle, player: State<'_, Player>, file: String) -> Result<String, String> {
    use tauri::Manager;
    let fname = std::path::Path::new(&file)
        .file_name()
        .and_then(|x| x.to_str())
        .map(|x| x.to_string())
        .ok_or("无效文件名")?;
    if parse_auto_bk_name(&fname).is_none() {
        return Err("无效的自动备份文件".into());
    }
    let src = auto_bk_dir(&app).join(&fname);
    if !src.is_file() {
        return Err("备份不存在".into());
    }
    let data = app.path().app_data_dir().expect("no app data dir");
    let tmp = data.join("restore-tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let out = std::process::Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&src)
        .arg("-d")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("unzip 命令失败: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("解压失败: {}", String::from_utf8_lossy(&out.stderr)));
    }
    if !tmp.join("lyra.db").is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("备份包损坏（缺 lyra.db）".into());
    }
    // 停止播放后覆盖（自动备份不含 covers，封面缓存保持现状）
    player.stop();
    for name in ["lyra.db", "lyra.db-wal", "lyra.db-shm", "session.json", "config.json"] {
        let from = tmp.join(name);
        if from.is_file() {
            let _ = std::fs::copy(&from, data.join(name));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = app.emit("library-changed", ());
    Ok("恢复完成，请重启好好听歌生效".into())
}

/// 极简 base64 解码（标准字母表，容忍空白）——避免为一处 data URL 引入 base64 crate
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' | '\n' | '\r' | ' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

// ================= 备份 / 恢复 / 写回 / 统计 / 交叉淡入 =================

/// 备份：WAL checkpoint 后打包 lyra.db + session.json + config.json + covers/ 为 zip（系统 zip 命令）
#[tauri::command]
async fn backup_library(app: AppHandle, db: State<'_, Mutex<Db>>) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;
    // 先 checkpoint 把 WAL 并入主库文件
    {
        let g = db.lock().unwrap();
        let _ = g.0.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }
    let target = app
        .dialog()
        .file()
        .add_filter("备份包", &["zip"])
        .set_file_name(format!("lyra-backup-{}.zip", chrono_like_now()))
        .blocking_save_file()
        .ok_or("未选择位置")?;
    let out = target.as_path().ok_or("无效路径")?.to_path_buf();
    use tauri::Manager;
    let data = app.path().app_data_dir().expect("no app data dir");
    let zip = std::process::Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(&out)
        .arg("lyra.db")
        .arg("session.json")
        .arg("config.json")
        .arg("covers")
        .current_dir(&data)
        .output()
        .map_err(|e| format!("zip 命令失败（macOS/Linux 自带）: {e}"))?;
    if !zip.status.success() {
        return Err(format!("打包失败: {}", String::from_utf8_lossy(&zip.stderr)));
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(format!("已备份 {}（{:.1} MB）", out.display(), size as f64 / 1048576.0))
}
fn chrono_like_now() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 简易日期（本地时区 +8 近似）：不引 chrono，仅用于文件名
    let days = t / 86400;
    let (y, m, d) = epoch_to_ymd(days as i64);
    format!("{:04}{:02}{:02}-{:02}{:02}", y, m, d, (t % 86400) / 3600 + 8, (t % 3600) / 60)
}
fn epoch_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }), m, d)
}

/// 恢复：解压覆盖 AppData（zip -o），需重启应用生效
#[tauri::command]
async fn restore_library(app: AppHandle, player: State<'_, Player>) -> Result<String, String> {
    use tauri::Manager;
    use tauri_plugin_dialog::DialogExt;
    let file = app
        .dialog()
        .file()
        .add_filter("备份包", &["zip"])
        .blocking_pick_file()
        .ok_or("未选择文件")?;
    let src = file.as_path().ok_or("无效路径")?.to_path_buf();
    let data = app.path().app_data_dir().expect("no app data dir");
    let tmp = data.join("restore-tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let out = std::process::Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(&src)
        .arg("-d")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("unzip 命令失败: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("解压失败: {}", String::from_utf8_lossy(&out.stderr)));
    }
    // 校验包内容
    if !tmp.join("lyra.db").is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("这不是有效的好好听歌备份包（缺 lyra.db）".into());
    }
    // 停止播放后覆盖
    player.stop();
    for name in ["lyra.db", "lyra.db-wal", "lyra.db-shm", "session.json", "config.json"] {
        let from = tmp.join(name);
        if from.is_file() {
            let _ = std::fs::copy(&from, data.join(name));
        }
    }
    if tmp.join("covers").is_dir() {
        let _ = std::process::Command::new("cp").arg("-R").arg(tmp.join("covers")).arg(data).output();
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok("恢复完成，请重启好好听歌生效".into())
}

/// 单曲写回标签
#[tauri::command]
fn writeback_track(db: State<'_, Mutex<Db>>, track_id: i64) -> Result<(), String> {
    let g = db.lock().unwrap();
    scanner::writeback_tags(&g.0, track_id)
}

/// 批量写回：所有 online 来源曲目（补全成果永久化到源文件）
#[tauri::command]
async fn writeback_all(app: AppHandle, db: State<'_, Mutex<Db>>) -> Result<serde_json::Value, String> {
    let ids: Vec<i64> = {
        let g = db.lock().unwrap();
        g.0.prepare("SELECT id FROM tracks WHERE source='online' AND hidden=0 ORDER BY id")
            .and_then(|mut s| Ok(s.query_map([], |r| r.get(0))?.filter_map(|x| x.ok()).collect()))
            .map_err(|e| e.to_string())?
    };
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut ok = 0usize;
        let mut fail = Vec::new();
        for id in ids {
            let g = app2.state::<Mutex<Db>>();
            let r = { scanner::writeback_tags(&g.lock().unwrap().0, id) };
            match r {
                Ok(()) => ok += 1,
                Err(e) => fail.push(e),
            }
        }
        serde_json::json!({ "ok": ok, "fail": fail.len(), "first_err": fail.first().cloned() })
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_stats(db: State<'_, Mutex<Db>>) -> serde_json::Value {
    let g = db.lock().unwrap();
    db::stats(&g.0)
}

// —— 交叉淡入淡出 ——
#[tauri::command]
fn get_crossfade(player: State<'_, Player>) -> u32 {
    player.crossfade_ms()
}
#[tauri::command]
fn set_crossfade(app: AppHandle, player: State<'_, Player>, ms: u32) -> u32 {
    let ms = ms.min(10_000);
    player.set_crossfade(ms);
    let mut cfg = read_cfg(&app);
    cfg["crossfade_ms"] = serde_json::json!(ms);
    write_cfg(&app, &cfg);
    ms
}

/// 手动编辑曲目字段
#[tauri::command]
fn update_track_fields(db: State<'_, Mutex<Db>>, track_id: i64, fields: serde_json::Value) -> Result<(), String> {
    let g = db.lock().unwrap();
    db::update_fields(&g.0, track_id, &fields)
}

// —— ReplayGain ——
/// 播放/切曲时应用当前曲的 RG 增益（按模式取 track/album；无标签为 0dB）
fn apply_rg(app: &AppHandle, player: &Player, track_id: i64) {
    let mode = {
        let cfg = read_cfg(app);
        cfg["rg_mode"].as_str().unwrap_or("off").to_string()
    };
    maybe_spawn_rg_analysis(app, player, track_id);
    if mode == "off" {
        let _ = player.set_rg(0.0);
        return;
    }
    let col = if mode == "album" { "rg_album" } else { "rg_track" };
    let db = app.state::<Mutex<Db>>();
    let g = db.lock().unwrap();
    let db_val: Option<f64> = g
        .0
        .query_row(
            &format!("SELECT {} FROM tracks WHERE id=?1", col),
            [track_id],
            |r| r.get(0),
        )
        .ok();
    let v = db_val.unwrap_or(0.0);
    let _ = player.set_rg(v as f32);
}

/// 分析中曲目去重（快速切歌不重复解码同一首）
static RG_ANALYZING: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

/// 播放时顺带分析：rg_track 为 NULL（从未解析）的曲目，后台独立解码一遍
/// 喂 K 加权分析器（不进实时管道，不受 EQ/重采样/seek 污染），
/// 结果落库后：当前曲立即重应用增益；专辑曲目齐后聚合 rg_album。
fn maybe_spawn_rg_analysis(app: &AppHandle, player: &Player, track_id: i64) {
    let row: Option<(String,)> = {
        let db = app.state::<Mutex<Db>>();
        let g = db.lock().unwrap();
        g.0.query_row(
            "SELECT file_path FROM tracks WHERE id=?1 AND rg_track IS NULL",
            [track_id],
            |r| Ok((r.get(0)?,)),
        )
        .ok()
    };
    let Some((path,)) = row else { return };
    {
        let mut s = RG_ANALYZING.lock().unwrap();
        if s.contains(&track_id) {
            return; // 已在分析
        }
        s.push(track_id);
    }
    let app = app.clone();
    let _ = player;
    std::thread::Builder::new()
        .name("lyra-rg-analyze".into())
        .spawn(move || {
            use tauri::Manager;
            let player = app.state::<Player>();
            let result = (|| -> Option<(f64, f32)> {
                use rodio::Source;
                let p = std::path::PathBuf::from(&path);
                let mut dec = audio::open_decoder(&p)?;
                let (fs, ch) = (dec.sample_rate(), dec.channels().max(1) as usize);
                let mut an = replaygain::RgAnalyzer::new(fs, ch);
                let mut buf: Vec<f32> = Vec::with_capacity(8192);
                for s in dec.by_ref() {
                    buf.push(s);
                    if buf.len() >= 8192 {
                        an.feed(&buf);
                        buf.clear();
                    }
                }
                an.feed(&buf);
                Some(an.finish())
            })();
            RG_ANALYZING.lock().unwrap().retain(|&x| x != track_id);
            let Some((gain, peak)) = result else { return };
            // 落库（rg_track 由 NULL → 实际值，扫描增量与去重随之生效）
            let db = app.state::<Mutex<Db>>();
            {
                let g = db.lock().unwrap();
                let _ = g.0.execute(
                    "UPDATE tracks SET rg_track=?1, rg_peak=?2 WHERE id=?3 AND rg_track IS NULL",
                    rusqlite::params![gain, peak, track_id],
                );
            }
            // 专辑曲目全部有 track 值 → 聚合 album 增益（一次写全组）
            let album_key: Option<String> = {
                let g = db.lock().unwrap();
                g.0.query_row("SELECT album_key FROM tracks WHERE id=?1", [track_id], |r| r.get(0)).ok()
            };
            if let Some(k) = album_key {
                let g = db.lock().unwrap();
                let rows: Vec<(f64, f64)> = g
                    .0
                    .prepare("SELECT rg_track, duration FROM tracks WHERE album_key=?1")
                    .and_then(|mut s| {
                        Ok(s.query_map([&k], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)))?
                            .filter_map(|x| x.ok())
                            .collect())
                    })
                    .unwrap_or_default();
                drop(g);
                let total = {
                    let g = db.lock().unwrap();
                    g.0.query_row("SELECT COUNT(*) FROM tracks WHERE album_key=?1", [&k], |r| r.get::<_, i64>(0)).unwrap_or(0)
                };
                if rows.len() as i64 == total && total > 0 && rows.iter().all(|(g, _)| *g != 0.0 || true) {
                    let ag = replaygain::album_gain(&rows);
                    let g = db.lock().unwrap();
                    let _ = g.0.execute("UPDATE tracks SET rg_album=?1 WHERE album_key=?2", rusqlite::params![ag, k]);
                }
            }
            // 若仍是当前曲，立即用上刚算出的增益
            let cur = {
                let q = player.queue.lock().unwrap();
                q.get(player.cur_index.load(std::sync::atomic::Ordering::Relaxed).max(0) as usize).copied()
            };
            if cur == Some(track_id) {
                apply_rg(&app, &player, track_id);
            }
            let _ = player;
        })
        .ok();
}

#[tauri::command]
fn set_rg_mode(app: AppHandle, player: State<'_, Player>, mode: String) -> String {
    let mode = if mode == "album" { "album" } else if mode == "off" { "off" } else { "track" }.to_string();
    let mut cfg = read_cfg(&app);
    cfg["rg_mode"] = serde_json::json!(&mode);
    write_cfg(&app, &cfg);
    // 立即作用于当前曲
    if let Some(tid) = {
        let q = player.queue.lock().unwrap();
        q.get(player.cur_index.load(std::sync::atomic::Ordering::Relaxed).max(0) as usize).copied()
    } {
        apply_rg(&app, &player, tid);
    }
    mode
}

#[tauri::command]
fn get_rg_mode(app: AppHandle) -> String {
    read_cfg(&app)["rg_mode"].as_str().unwrap_or("off").to_string()
}

// —— 通用配置（AppData/config.json，缓存/渐变等共用）——
fn read_cfg(app: &AppHandle) -> serde_json::Value {
    use tauri::Manager;
    let p = app.path().app_data_dir().expect("no app data dir").join("config.json");
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::json!({}))
}
fn write_cfg(app: &AppHandle, v: &serde_json::Value) {
    use tauri::Manager;
    let p = app.path().app_data_dir().expect("no app data dir").join("config.json");
    let _ = std::fs::write(p, v.to_string());
}

#[tauri::command]
fn get_fade(player: State<'_, Player>) -> i32 {
    player.fade_ms.load(std::sync::atomic::Ordering::Relaxed)
}

// —— 播放速度 ——
#[tauri::command]
fn get_speed(player: State<'_, Player>) -> f32 {
    player.speed()
}

#[tauri::command]
fn set_speed(app: AppHandle, player: State<'_, Player>, v: f32) -> f32 {
    player.set_speed(v);
    let mut cfg = read_cfg(&app);
    cfg["speed"] = serde_json::json!(v.clamp(0.25, 4.0));
    write_cfg(&app, &cfg);
    player.speed()
}

// —— 均衡器 + 音调 ——
#[tauri::command]
fn get_eq(player: State<'_, Player>) -> serde_json::Value {
    let (on, gains, (tone_on, bass, treble)) = player.eq_state();
    serde_json::json!({ "enabled": on, "gains": gains, "toneOn": tone_on, "bass": bass, "treble": treble })
}

#[tauri::command]
fn set_eq(
    app: AppHandle,
    player: State<'_, Player>,
    enabled: bool,
    gains: Vec<f32>,
    tone_on: Option<bool>,
    bass: Option<f32>,
    treble: Option<f32>,
) -> serde_json::Value {
    // 音调参数缺省时保留现值（滑块拖动只带增益的场景）
    let (_, _, (cur_on, cur_bass, cur_treble)) = player.eq_state();
    let (t_on, t_bass, t_treble) = (
        tone_on.unwrap_or(cur_on),
        bass.unwrap_or(cur_bass),
        treble.unwrap_or(cur_treble),
    );
    player.set_eq(enabled, &gains, t_on, t_bass, t_treble);
    let mut cfg = read_cfg(&app);
    cfg["eq_on"] = serde_json::json!(enabled);
    cfg["eq"] = serde_json::json!(gains);
    cfg["tone_on"] = serde_json::json!(t_on);
    cfg["tone_bass"] = serde_json::json!(t_bass);
    cfg["tone_treble"] = serde_json::json!(t_treble);
    write_cfg(&app, &cfg);
    get_eq(player)
}

#[tauri::command]
fn set_fade(app: AppHandle, player: State<'_, Player>, ms: i32) -> i32 {
    let ms = ms.clamp(0, 1000);
    player.set_fade(ms);
    let mut cfg = read_cfg(&app);
    cfg["fade_ms"] = serde_json::json!(ms);
    write_cfg(&app, &cfg);
    ms
}

#[tauri::command]
fn get_cache_info(app: AppHandle) -> cache::CacheInfo {
    cache::get_info(&app)
}

#[tauri::command]
fn set_cache_config(app: AppHandle, enabled: bool, limit_mb: u64) -> cache::CacheInfo {
    cache::set_config(&app, enabled, limit_mb)
}

#[tauri::command]
fn clear_cache(app: AppHandle) -> cache::CacheInfo {
    cache::clear(&app)
}

#[tauri::command]
fn get_position(player: State<'_, Player>) -> serde_json::Value {
    serde_json::json!({
        "position": player.position_ms() as f64 / 1000.0,
        "duration": player.duration_ms() as f64 / 1000.0,
        "playing": player.is_playing(),
    })
}

// ================= 歌词 Commands =================

#[tauri::command]
fn get_lyrics(db: State<'_, Mutex<Db>>, track_id: i64, t2s: Option<bool>) -> lyrics::LyricDoc {
    let conn = &db.lock().unwrap().0;
    let mut doc = lyrics::get_lyrics(conn, track_id);
    // 懒更新列表"译"标记：预览时以真实判定纠正（覆盖 .zh.lrc 后加等漂移）
    let tr_flag = doc.source.contains("双语");
    // 繁体→简体（默认开启，前端设置页可关）：LRCLIB 等数据源的中文歌词常为繁体
    if t2s.unwrap_or(true) && !doc.lines.is_empty() {
        let converted: Vec<String> = doc.lines.iter().map(|l| zhconv::t2s(&l.l)).collect();
        let changed = doc.lines.iter().zip(converted.iter()).any(|(a, b)| a.l != *b);
        if changed {
            for (l, nl) in doc.lines.iter_mut().zip(converted.into_iter()) {
                l.l = nl;
            }
            doc.source.push_str(" · 已繁转简");
        }
    }
    let _ = conn.execute(
        "UPDATE tracks SET has_tr=?1 WHERE id=?2",
        rusqlite::params![tr_flag as i64, track_id],
    );
    doc
}

#[tauri::command]
async fn fetch_online_lyrics(
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    base_url: Option<String>,
) -> Result<String, String> {
    let info = {
        let g = db.lock().unwrap();
        lyrics::track_for_fetch(&g.0, track_id)
    };
    let info = info.ok_or("曲目不存在")?;
    // 智能顺序：自定义源（含官方中文翻译，一次拿全）→ LRCLIB 回退
    let base = base_url.filter(|s| !s.trim().is_empty());
    if let Some(base) = &base {
        match lyrics::fetch_custom_lyrics(&info, base).await {
            Ok(Some((lrc, tr))) => {
                let g = db.lock().unwrap();
                lyrics::save_online_lyrics(&g.0, track_id, &lrc)?;
                if let Some(t) = &tr {
                    let _ = g.0.execute(
                        "UPDATE tracks SET lyrics_tr=?1 WHERE id=?2",
                        rusqlite::params![t, track_id],
                    );
                }
                lyrics::recompute_has_tr(&g.0, track_id);
                return Ok(if tr.is_some() {
                    "已保存歌词 + 中文翻译（自定义源）".into()
                } else {
                    "已保存歌词（自定义源）".into()
                });
            }
            Ok(None) => { /* 源未收录，回退 LRCLIB */ }
            Err(e) => {
                eprintln!("[lyrics] 自定义源失败，回退 LRCLIB: {e}");
            }
        }
    }
    let text = lyrics::fetch_lrclib(&info).await?;
    {
        let g = db.lock().unwrap();
        lyrics::save_online_lyrics(&g.0, track_id, &text)?;
        lyrics::recompute_has_tr(&g.0, track_id);
    }
    Ok("已保存歌词（来源 LRCLIB）".into())
}

/// 用户自定义 HTTP 翻译源（不内置任何具体服务，合规责任由用户自建服务承担）
/// 歌词多源候选（选择面板）：自定义源搜索列表 + LRCLIB 搜索
#[tauri::command]
async fn lyrics_search(
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    base_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let info = {
        let g = db.lock().unwrap();
        lyrics::track_for_fetch(&g.0, track_id)
    }.ok_or("曲目不存在")?;
    let mut groups: Vec<serde_json::Value> = Vec::new();
    // 自定义源候选（占位艺人不传，避免污染搜索）
    let (_, q_artist) = search_terms_clean(&info.title, &info.artist);
    let base = base_url.filter(|s| !s.trim().is_empty());
    if let Some(b) = &base {
        let url = format!("{}/lyrics-search?artist={}&title={}&duration={}",
            b.trim_end_matches('/'),
            lyrics::urlencode(&q_artist), lyrics::urlencode(&info.title),
            info.duration as i64);
        match reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(8)).build()
            .map_err(|e| e.to_string())?.get(&url).send().await
        {
            Ok(r) if r.status().is_success() => {
                if let Ok(list) = r.json::<serde_json::Value>().await {
                    if let Some(mut arr) = list.as_array().cloned() {
                        // 列表期即标注翻译：并发拉正文检测 translation（签名搜索候选
                        // 通常 ≤10 条；上限 16 防爆，失败条目退回预览时回填）
                        let mut js = tokio::task::JoinSet::new();
                        const PROBE_MAX: usize = 16;
                        for (i, x) in arr.iter().enumerate().take(PROBE_MAX) {
                            let base2 = b.clone();
                            let nid = x["id"].as_i64().unwrap_or(0);
                            js.spawn(async move {
                                let u = format!("{}/lyrics-by-id?id={}", base2.trim_end_matches('/'), nid);
                                let has = async {
                                    let cl = reqwest::Client::builder()
                                        .user_agent("Lyra/0.1")
                                        .timeout(std::time::Duration::from_secs(6))
                                        .build()
                                        .ok()?;
                                    let v: serde_json::Value = cl.get(&u).send().await.ok()?.json().await.ok()?;
                                    Some(v["translation"].as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
                                }.await;
                                (i, has.unwrap_or(false))
                            });
                        }
                        let mut flags = vec![false; arr.len().min(PROBE_MAX)];
                        while let Some(r) = js.join_next().await {
                            if let Ok((i, f)) = r {
                                if i < flags.len() { flags[i] = f; }
                            }
                        }
                        for (i, x) in arr.iter_mut().enumerate() {
                            if i < flags.len() {
                                x["has_tr"] = serde_json::Value::Bool(flags[i]);
                            }
                        }
                        groups.push(serde_json::json!({ "src": "自定义源", "items": arr }));
                    }
                }
            }
            _ => {}
        }
    }
    // LRCLIB 候选
    match lyrics::lrclib_search(&info).await {
        Ok(list) => groups.push(serde_json::json!({ "src": "LRCLIB", "items": list })),
        Err(e) => eprintln!("[lyrics] LRCLIB 搜索失败: {e}"),
    }
    Ok(serde_json::json!({ "groups": groups, "track": { "title": info.title, "artist": info.artist } }))
}

/// 候选预览：拉正文（LRCLIB 候选自带；自定义源按 id 拉）
#[tauri::command]
async fn lyrics_fetch_item(
    item: serde_json::Value,
    src: String,
    base_url: Option<String>,
) -> Result<serde_json::Value, String> {
    if src == "lrclib" {
        let body = item["body"].as_str().unwrap_or("").to_string();
        if body.is_empty() { return Err("该条目无正文".into()); }
        Ok(serde_json::json!({ "lrc": body, "translation": null }))
    } else {
        let base = base_url.filter(|s| !s.trim().is_empty()).ok_or("自定义源未配置")?;
        let nid = item["id"].as_i64().unwrap_or(0);
        let url = format!("{}/lyrics-by-id?id={}", base.trim_end_matches('/'), nid);
        let resp = reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(10)).build()
            .map_err(|e| e.to_string())?.get(&url).send().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let lrc = v["lrc"].as_str().ok_or("无歌词正文")?.to_string();
        let tr = v["translation"].as_str().map(|s| s.to_string());
        Ok(serde_json::json!({ "lrc": lrc, "translation": tr }))
    }
}

/// 确认应用：直存正文（预览时已拉好）
#[tauri::command]
fn lyrics_apply(
    app: AppHandle,
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    lrc: String,
    translation: Option<String>,
    src_label: String,
) -> Result<String, String> {
    // 写库逻辑收进块内释放锁：auto_backup 内部会再拿 db 锁，
    // 同线程重入 std::sync::Mutex 会死锁（同步命令在主线程执行 → 整个 app 卡住）
    let detail = {
        let g = db.lock().unwrap();
        lyrics::save_online_lyrics(&g.0, track_id, &lrc)?;
        if let Some(t) = &translation {
            let _ = g.0.execute("UPDATE tracks SET lyrics_tr=?1 WHERE id=?2", rusqlite::params![t, track_id]);
        }
        lyrics::recompute_has_tr(&g.0, track_id);
        g.0.query_row(
            "SELECT title || ' - ' || artist FROM tracks WHERE id=?1",
            [track_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };
    auto_backup(&db, &app, "lyrics", &detail);
    Ok(format!("已应用歌词（{}）", src_label))
}

#[allow(dead_code)]
async fn lyrics_apply_old(
    app: AppHandle,
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    src: String,
    item: serde_json::Value,
    base_url: Option<String>,
) -> Result<String, String> {
    let (lrc, tr, src_label) = if src == "lrclib" {
        // LRCLIB：按 id 再查拿正文（get 端点）
        let id = item["id"].as_i64().unwrap_or(0);
        let url = format!("https://lrclib.net/api/get/{}", id);
        let resp = reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(10)).build()
            .map_err(|e| e.to_string())?.get(&url).send().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let body = v["syncedLyrics"].as_str().filter(|s| !s.is_empty())
            .or(v["plainLyrics"].as_str()).ok_or("该条目无歌词正文")?.to_string();
        (body, None, "LRCLIB".to_string())
    } else {
        // 自定义源：按源内 id 拉正文+翻译
        let base = base_url.filter(|s| !s.trim().is_empty()).ok_or("自定义源未配置")?;
        let nid = item["id"].as_i64().unwrap_or(0);
        let url = format!("{}/lyrics-by-id?id={}", base.trim_end_matches('/'), nid);
        let resp = reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(10)).build()
            .map_err(|e| e.to_string())?.get(&url).send().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let lrc = v["lrc"].as_str().ok_or("无歌词正文")?.to_string();
        let tr = v["translation"].as_str().map(|s| s.to_string());
        (lrc, tr, "自定义源".to_string())
    };
    {
        let g = db.lock().unwrap();
        lyrics::save_online_lyrics(&g.0, track_id, &lrc)?;
        if let Some(t) = &tr {
            let _ = g.0.execute("UPDATE tracks SET lyrics_tr=?1 WHERE id=?2", rusqlite::params![t, track_id]);
        }
    }
    let _ = &app;
    Ok(format!("已应用歌词（{}）", src_label))
}

/// 搜索词清洗：去括号后缀（Live/翻唱/品质标注）、feat. 段、合作艺人取首位；
/// 占位艺人（未知艺术家等）不传——拼进关键词会污染源端搜索结果。
/// 只用于搜索请求（脏关键词干扰源端分词），不影响展示与库内字段
fn search_terms_clean(title: &str, artist: &str) -> (String, String) {
    let strip_paren = |s: &str| -> String {
        let mut out = String::new();
        let mut depth = 0i32;
        for c in s.chars() {
            match c {
                '(' | '（' | '[' | '【' => depth += 1,
                ')' | '）' | ']' | '】' => depth = (depth - 1).max(0),
                _ if depth <= 0 => out.push(c),
                _ => {}
            }
        }
        out.trim().to_string()
    };
    let t = strip_paren(title);
    // feat. / feat / Feat- 截断（英文歌名常见）
    let t = t
        .split_whitespace()
        .take_while(|w| !w.to_lowercase().starts_with("feat"))
        .collect::<Vec<_>>()
        .join(" ");
    let a = strip_paren(artist);
    let a = a
        .split(['/', '、', ',', '&', ';'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    // 占位艺人不参与搜索
    let unknown = ["未知艺术家", "未知歌手", "未知艺人", "unknown artist", "unknown", "various artists"];
    let a = if unknown.iter().any(|u| a.eq_ignore_ascii_case(u)) { String::new() } else { a };
    (t, a)
}

/// 曲目元数据多源候选（iTunes / Deezer / 自定义源），状态透明。
/// 候选统一重排（时长匹配 > 标题相似度 > 艺人匹配）：源端相关性排序常把
/// 翻唱/Live/同艺人其他歌排前面（下架歌尤甚），重排让真货浮到首位
#[tauri::command]
async fn meta_search(
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    base_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let info = {
        let g = db.lock().unwrap();
        lyrics::track_for_fetch(&g.0, track_id)
    }.ok_or("曲目不存在")?;
    let mut groups: Vec<serde_json::Value> = Vec::new();
    let dur = info.duration as i64;
    let (q_title, q_artist) = search_terms_clean(&info.title, &info.artist);
    // 候选重排：dur_ok 优先 → 标题相似度 → 艺人包含
    let rank = |items: &mut Vec<serde_json::Value>| {
        items.sort_by(|x, y| {
            let dx = x["dur_ok"].as_bool().unwrap_or(false);
            let dy = y["dur_ok"].as_bool().unwrap_or(false);
            dy.cmp(&dx).then_with(|| {
                let sx = provider::similarity(x["title"].as_str().unwrap_or(""), &info.title);
                let sy = provider::similarity(y["title"].as_str().unwrap_or(""), &info.title);
                sy.partial_cmp(&sx).unwrap_or(std::cmp::Ordering::Equal)
            }).then_with(|| {
                let bx = x["artist"].as_str().unwrap_or("").contains(&info.artist);
                let by = y["artist"].as_str().unwrap_or("").contains(&info.artist);
                by.cmp(&bx)
            })
        });
    };
    // iTunes（entity=song）：失败也把原因带回面板，避免只显示"无结果"
    let (mut it_items, it_err): (Vec<serde_json::Value>, Option<String>) =
        match provider::itunes_songs(&q_artist, &q_title).await {
            Ok(it) => (it.iter().map(|s| serde_json::json!({
                "id": s.0, "title": s.1, "artist": s.2, "album": s.3,
                "year": s.4, "duration": s.5,
                "dur_ok": dur <= 0 || (s.5 > 0 && (s.5 - dur).abs() <= 5),
            })).collect(), None),
            Err(e) => (Vec::new(), Some(e)),
        };
    rank(&mut it_items);
    groups.push(serde_json::json!({ "src": "iTunes", "items": it_items, "err": it_err }));
    // Deezer
    let (mut dz_items, dz_err): (Vec<serde_json::Value>, Option<String>) =
        match provider::deezer_songs(&q_artist, &q_title).await {
            Ok(dz) => (dz.iter().map(|s| serde_json::json!({
                "id": s.0, "title": s.1, "artist": s.2, "album": s.3,
                "year": s.4, "duration": s.5,
                "dur_ok": dur <= 0 || (s.5 > 0 && (s.5 - dur).abs() <= 5),
            })).collect(), None),
            Err(e) => (Vec::new(), Some(e)),
        };
    rank(&mut dz_items);
    groups.push(serde_json::json!({ "src": "Deezer", "items": dz_items, "err": dz_err }));
    // 自定义源：未配置 / 连接失败 / 格式异常都把原因带回面板
    let custom_group = match base_url.filter(|s| !s.trim().is_empty()) {
        None => serde_json::json!({
            "src": "自定义源", "items": [],
            "err": "未配置（设置 → 歌词 → 自定义源）",
        }),
        Some(b) => {
            let url = format!("{}/meta-search?artist={}&title={}", b.trim_end_matches('/'),
                lyrics::urlencode(&q_artist), lyrics::urlencode(&q_title));
            let res: Result<serde_json::Value, String> =
                match reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(8)).build() {
                    Ok(c) => match c.get(&url).send().await {
                        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.map_err(|e| e.to_string()),
                        Ok(r) => Err(format!("适配器返回 HTTP {}", r.status().as_u16())),
                        Err(e) => Err(format!("连接失败：{e}")),
                    },
                    Err(e) => Err(e.to_string()),
                };
            match res {
                Ok(list) => match list.as_array() {
                    Some(arr) => {
                        let mut items: Vec<serde_json::Value> = arr.iter().map(|x| {
                            let d = x["duration"].as_i64().unwrap_or(0);
                            serde_json::json!({
                                "id": x["id"], "title": x["title"], "artist": x["artist"], "album": x["album"],
                                // 适配器双源聚合：QQ 项自带年份（pubtime），网易云项为 null
                                "year": x["year"], "duration": d,
                                "dur_ok": dur <= 0 || (d > 0 && (d - dur).abs() <= 5),
                            })
                        }).collect();
                        rank(&mut items);
                        serde_json::json!({ "src": "自定义源", "items": items, "err": serde_json::Value::Null })
                    }
                    None => serde_json::json!({ "src": "自定义源", "items": [], "err": "返回格式异常（应为数组）" }),
                },
                Err(e) => serde_json::json!({ "src": "自定义源", "items": [], "err": e }),
            }
        }
    };
    groups.insert(0, custom_group);
    Ok(serde_json::json!({ "groups": groups, "track": { "title": info.title, "artist": info.artist, "duration": dur } }))
}

/// 选中候选拉完整字段（自定义源按 id 补年份/track；iTunes/Deezer 候选已含）
#[tauri::command]
async fn meta_fetch_item(src: String, id: String, base_url: Option<String>) -> Result<serde_json::Value, String> {
    if src == "custom" {
        let b = base_url.filter(|s| !s.trim().is_empty()).ok_or("自定义源未配置")?;
        let url = format!("{}/meta-by-id?id={}", b.trim_end_matches('/'), id);
        let resp = reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(10)).build()
            .map_err(|e| e.to_string())?.get(&url).send().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "title": v["title"], "artist": v["artist"], "album": v["album"],
            "year": v["year"], "track_no": v["track_no"], "duration": v["duration"],
        }))
    } else {
        Err("该源候选已含完整字段，直接应用".into())
    }
}

#[tauri::command]
async fn fetch_translation(
    app: AppHandle,
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    base_url: String,
) -> Result<String, String> {
    if base_url.trim().is_empty() {
        return Err("未配置自定义源（设置 → 联网增强 → 自定义源）".into());
    }
    let info = {
        let g = db.lock().unwrap();
        lyrics::track_for_fetch(&g.0, track_id)
    };
    let info = info.ok_or("曲目不存在")?;
    let tr = lyrics::fetch_custom_translation(&base_url, &info).await?;
    match tr {
        Some(t) => {
            {
                let g = db.lock().unwrap();
                lyrics::save_translation(&g.0, track_id, &t)?;
                lyrics::recompute_has_tr(&g.0, track_id);
            }
            auto_backup(&db, &app, "tr", &format!("{} - {}", info.title, info.artist));
            Ok("已获取中文翻译（缓存在本地库）".into())
        }
        None => {
            // 区分提示：中文歌本就无翻译可测（翻译接口仅外文歌有）
            let has_cjk = info.title.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c));
            if has_cjk {
                Err("源无返回：当前曲目是中文歌，本就没有翻译（请换一首英文/日韩歌测试）".into())
            } else {
                Err("自定义源没有返回该曲目的翻译（源连通正常时多为源曲库未收录，可换个常见歌手试试）".into())
            }
        }
    }
}

/// 自定义源连通性测试：分层诊断（适配器 → 网易云上游），不依赖播放曲目。
/// 502 类问题（base 指向反代、后端脚本没起）能直接看出挂在哪一层
#[tauri::command]
async fn test_custom_source(base_url: String) -> Result<String, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("未配置地址".into());
    }
    let resp = reqwest::Client::builder()
        .user_agent("Lyra/0.1")
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{}/ping", base))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("Connection refused") {
                return Err(format!("连接被拒绝：{} 上没有服务在运行（适配脚本启动了吗？）", base));
            }
            return Err(format!("连接失败：{msg}"));
        }
    };
    if !resp.status().is_success() {
        return Err(format!("适配器返回 HTTP {}（地址指向了代理/网关？确认直连适配脚本端口）", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("响应不是 JSON: {e}"))?;
    let netease_ok = v["netease_ok"].as_bool().unwrap_or(false);
    let qq_ok = v["qq_ok"].as_bool().unwrap_or(false);
    let netease = v["netease"].as_str().unwrap_or("?");
    let detail = |k: &str| -> String {
        let e = v[k].as_str().unwrap_or("").trim().to_string();
        if e.is_empty() { String::new() } else { format!("：{e}") }
    };
    match (qq_ok, netease_ok) {
        (true, true) => Ok(format!("自定义源正常（QQ 直连 + 网易云上游 {} 均连通）", netease)),
        (true, false) => Ok(format!("QQ 直连正常；网易云上游 {} 不可达{}（歌词/翻译回退 QQ 与内置源）", netease, detail("err_n"))),
        (false, true) => Ok(format!("网易云上游正常；QQ 直连失败{}（元数据/图片回退网易云与内置源）", detail("err_q"))),
        (false, false) => Err(format!("适配器在运行，但 QQ 与网易云上游均不可达（QQ{}；网易云{}）——若是超时类错误，检查系统代理是否劫持 node fetch", detail("err_q"), detail("err_n"))),
    }
}

/// 重复曲目检测：清洗标题（去括号后缀）+ 主艺人（合作取首位）相同 且
/// 时长相邻差 ≤2s 聚类为一组——同歌多版本（专辑版/Live/不同音质）的整理入口。
/// 返回组列表（组内按时长升序），hidden 曲目不参与。
#[tauri::command]
fn dup_scan(db: State<'_, Mutex<Db>>) -> Vec<serde_json::Value> {
    let g = db.lock().unwrap();
    let rows: Vec<(i64, String, String, String, f64)> = g
        .0
        .prepare("SELECT id, title, artist, album, duration FROM tracks WHERE hidden=0")
        .and_then(|mut s| {
            Ok(s.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, f64>(4)?))
            })?
            .filter_map(|x| x.ok())
            .collect())
        })
        .unwrap_or_default();
    // 标题清洗：去括号段 + 小写（Live/伴奏等标注不参与判重）
    let clean_title = |s: &str| -> String {
        let mut out = String::new();
        let mut depth = 0i32;
        for c in s.chars() {
            match c {
                '(' | '（' | '[' | '【' => depth += 1,
                ')' | '）' | ']' | '】' => depth = (depth - 1).max(0),
                _ if depth <= 0 => out.push(c),
                _ => {}
            }
        }
        out.trim().to_lowercase()
    };
    // 主艺人：合作（/、feat 类连接）取首位，小写
    let main_artist = |s: &str| -> String {
        s.split(['/', '、', ',', '&', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase()
    };
    let mut groups: std::collections::HashMap<(String, String), Vec<(i64, String, String, String, f64)>> =
        std::collections::HashMap::new();
    for (id, title, artist, album, dur) in rows {
        let key = (clean_title(&title), main_artist(&artist));
        groups.entry(key).or_default().push((id, title, artist, album, dur));
    }
    let mut out = Vec::new();
    for ((ct, ca), mut items) in groups {
        if ct.is_empty() || items.len() < 2 {
            continue;
        }
        items.sort_by(|a, b| a.4.partial_cmp(&b.4).unwrap_or(std::cmp::Ordering::Equal));
        // 时长聚类：排序后相邻差 >2s 断开（album 版与 Live 版差大时不成组）
        let mut cluster: Vec<(i64, String, String, String, f64)> = vec![items[0].clone()];
        let mut clusters: Vec<Vec<_>> = Vec::new();
        for it in &items[1..] {
            if it.4 - cluster.last().unwrap().4 <= 2.0 {
                cluster.push(it.clone());
            } else {
                clusters.push(std::mem::take(&mut cluster));
                cluster.push(it.clone());
            }
        }
        clusters.push(cluster);
        for c in clusters {
            if c.len() < 2 {
                continue;
            }
            out.push(serde_json::json!({
                "title": c[0].1.clone(),
                "artist": c[0].2.clone(),
                "items": c.iter().map(|(id, title, artist, album, dur)| serde_json::json!({
                    "id": id, "title": title, "artist": artist, "album": album, "duration": dur,
                })).collect::<Vec<_>>(),
            }));
        }
    }
    out.sort_by(|a, b| b["items"].as_array().map(|x| x.len()).unwrap_or(0).cmp(&a["items"].as_array().map(|x| x.len()).unwrap_or(0)));
    out
}

/// 封面缓存清理：删除 covers 目录内 DB 不引用的孤儿文件（hash 命名，重扫不复活）。
/// dry_run=true 只统计不删。返回 {removed, freed_mb, dry}
#[tauri::command]
fn clean_covers(app: AppHandle, db: State<'_, Mutex<Db>>, dry_run: Option<bool>) -> serde_json::Value {
    use tauri::Manager;
    let dry = dry_run.unwrap_or(false);
    let dir = db::covers_dir(&app);
    let used: std::collections::HashSet<String> = {
        let g = db.lock().unwrap();
        g.0.prepare("SELECT DISTINCT cover FROM tracks WHERE cover IS NOT NULL")
            .and_then(|mut s| {
                Ok(s.query_map([], |r| r.get::<_, String>(0))?
                    .filter_map(|x| x.ok())
                    .collect())
            })
            .unwrap_or_default()
    };
    let mut removed = 0usize;
    let mut freed = 0u64;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !matches!(name.rsplit('.').next(), Some("jpg" | "jpeg" | "png" | "webp")) {
                continue;
            }
            let p = e.path();
            if used.contains(p.to_string_lossy().as_ref()) {
                continue;
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if dry {
                removed += 1;
                freed += size;
            } else if std::fs::remove_file(&p).is_ok() {
                removed += 1;
                freed += size;
            }
        }
    }
    serde_json::json!({ "removed": removed, "freed_mb": (freed as f64 / 1048576.0 * 10.0).round() / 10.0, "dry": dry })
}

/// 后台播放开关（设置 → 功能开关）：开启时 macOS 关窗隐藏继续播放（存 config.json，
/// 关窗拦截在 Rust 事件层，localStorage 够不着）
#[tauri::command]
fn get_bgplay(app: AppHandle) -> bool {
    read_cfg(&app)["bgplay"].as_bool().unwrap_or(false)
}
#[tauri::command]
fn set_bgplay(app: AppHandle, on: bool) -> bool {
    let mut cfg = read_cfg(&app);
    cfg["bgplay"] = serde_json::json!(on);
    write_cfg(&app, &cfg);
    on
}

// ================= MusicBrainz 元数据补全 =================

/// 待确认条目（前端渲染卡片）
#[derive(Serialize)]
struct EnrichItem {
    track_id: i64,
    title: String,
    artist: String,
    album: String,
    conf: f64,
    candidates: Vec<provider::Candidate>,
}

/// 自定义源候选（/meta-search，QQ+网易云聚合）→ 统一 Candidate。
/// conf 口径与 MusicBrainz 相同（时长 0.45 + 标题相似 0.35 + 艺人匹配 0.20），
/// 走同一套自动应用 / 待确认 / 跳过阈值
fn custom_cands(items: &[serde_json::Value], q_title: &str, q_artist: &str, dur: f64) -> Vec<provider::Candidate> {
    items
        .iter()
        .filter_map(|x| {
            let title = x["title"].as_str()?;
            if title.is_empty() {
                return None;
            }
            let artist = x["artist"].as_str().unwrap_or("");
            let d = x["duration"].as_f64().unwrap_or(0.0);
            let dur_score = if dur <= 0.0 {
                0.5
            } else if d <= 0.0 {
                0.3
            } else {
                let diff = (d - dur).abs();
                if diff <= 2.0 { 1.0 } else if diff <= 5.0 { 0.75 } else if diff <= 10.0 { 0.4 } else { 0.0 }
            };
            let title_sim = provider::similarity(title, q_title);
            let artist_sim = if q_artist.is_empty() {
                0.5
            } else if artist.split('/').any(|a| a.trim() == q_artist) {
                1.0
            } else {
                provider::similarity(artist, q_artist) * 0.7
            };
            let id = x["id"].as_i64().map(|i| i.to_string()).or_else(|| x["id"].as_str().map(|s| s.to_string()));
            Some(provider::Candidate {
                provider: "自定义源".into(),
                title: title.to_string(),
                artist: artist.to_string(),
                album: x["album"].as_str().map(|s| s.to_string()),
                year: x["year"].as_i64(),
                duration: d,
                mbid: id.unwrap_or_default(),
                conf: (0.45 * dur_score + 0.35 * title_sim + 0.20 * artist_sim).clamp(0.0, 1.0),
            })
        })
        .collect()
}

#[tauri::command]
async fn enrich_missing(db: State<'_, Mutex<Db>>, app: AppHandle, base_url: Option<String>) -> Result<serde_json::Value, String> {
    // 目标：关键字段缺失且「从未处理过」的曲目（enrichment_status='none'）。
    // needs_review / applied / dismissed / queried 的曲目不再重复选中与重复请求，
    // 防止反复点击按钮对 MusicBrainz 空耗 1 req/s 配额。
    let rows: Vec<(i64, String, String, String, f64, String)> = {
        let g = db.lock().unwrap();
        let mut stmt = g.0.prepare(
            "SELECT id, title, artist, album, duration, source FROM tracks
             WHERE (source='guessed' OR artist='未知艺术家' OR album='未命名专辑' OR album='' OR year IS NULL)
               AND enrichment_status='none' AND hidden=0
             ORDER BY (source='guessed'), id LIMIT 20",
        ).map_err(|e| e.to_string())?;
        let iter = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .map_err(|e| e.to_string())?;
        iter.filter_map(|x| x.ok()).collect()
    };
    if rows.is_empty() {
        return Ok(serde_json::json!({ "applied": 0, "review": 0, "skipped": 0, "needFingerprint": 0, "error": null, "message": "没有需要补全的曲目" }));
    }
    let (mut applied, mut review, mut skipped, mut need_fp) = (0usize, 0usize, 0usize, 0usize);
    let mut last_err: Option<String> = None;
    let total = rows.len();
    for (i, (id, title, artist, album, dur, source)) in rows.iter().enumerate() {
        let _ = app.emit(
            "enrich-progress",
            serde_json::json!({ "done": i, "total": total, "title": title }),
        );
        // 标题本身无效（Unknown/乱码文件名）→ 文本匹配无意义，属指纹识别（AcoustID，规划中）
        if *source == "guessed" || title.starts_with("Unknown") {
            need_fp += 1;
            continue;
        }
        // 多源补全：自定义源（QQ+网易云，中文覆盖最好）优先；无配置/无结果回退 MusicBrainz
        let (q_title, q_artist) = search_terms_clean(title, artist);
        let base = base_url.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
        let mut cands: Vec<provider::Candidate> = Vec::new();
        if let Some(b) = base {
            let url = format!(
                "{}/meta-search?artist={}&title={}",
                b.trim_end_matches('/'),
                lyrics::urlencode(&q_artist),
                lyrics::urlencode(&q_title)
            );
            if let Ok(cl) = reqwest::Client::builder()
                .user_agent("Lyra/0.1")
                .timeout(std::time::Duration::from_secs(8))
                .build()
            {
                if let Ok(r) = cl.get(&url).send().await {
                    if r.status().is_success() {
                        if let Ok(list) = r.json::<serde_json::Value>().await {
                            if let Some(arr) = list.as_array() {
                                cands = custom_cands(arr, &q_title, &q_artist, *dur);
                                cands.sort_by(|a, b| b.conf.partial_cmp(&a.conf).unwrap_or(std::cmp::Ordering::Equal));
                            }
                        }
                    }
                }
            }
            // 温和限速（QQ/网易云宽容，仅作礼貌间隔）
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let cands = if cands.is_empty() {
            match provider::search_candidates(title, artist, album, *dur).await {
                Ok(c) => c,
                Err(e) => { skipped += 1; last_err = Some(e); continue; }
            }
        } else {
            cands
        };
        match cands.first() {
            Some(c) if c.conf >= 0.9 => {
                // 高置信：自动应用（仅覆盖缺失字段，可撤销，设计文档 5.4）
                let g = db.lock().unwrap();
                provider::apply_candidate(&g.0, *id, c, "applied", None)?;
                applied += 1;
            }
            Some(_) if cands.iter().any(|c| c.conf >= 0.5) => {
                // 中置信：候选全部入库待确认
                let cands_json = serde_json::to_string(&cands).unwrap_or_default();
                let g = db.lock().unwrap();
                g.0.execute(
                    "UPDATE tracks SET enrichment_status='needs_review', online_meta=?2 WHERE id=?1",
                    rusqlite::params![id, cands_json],
                )
                .map_err(|e| e.to_string())?;
                review += 1;
            }
            _ => {
                // 低置信无匹配：标记 queried，避免重复查询同一曲目
                let g = db.lock().unwrap();
                let _ = g.0.execute(
                    "UPDATE tracks SET enrichment_status='queried' WHERE id=?1 AND enrichment_status='none'",
                    [id],
                );
                skipped += 1;
            }
        }
    }
    let _ = app.emit("enrich-progress", serde_json::json!({ "done": total, "total": total }));
    let mut msg = format!("自动应用 {applied} · 待确认 {review} · 未匹配 {skipped}");
    if need_fp > 0 {
        msg.push_str(&format!(" · {need_fp} 首无有效标题（需指纹识别，规划中）"));
    }
    Ok(serde_json::json!({
        "applied": applied, "review": review, "skipped": skipped,
        "needFingerprint": need_fp, "error": last_err, "message": msg
    }))
}

#[tauri::command]
fn list_enrich(db: State<'_, Mutex<Db>>) -> Vec<EnrichItem> {
    let g = db.lock().unwrap();
    let rows: Vec<(i64, String, String, String, Option<String>)> = g
        .0
        .prepare("SELECT id, title, artist, album, online_meta FROM tracks WHERE enrichment_status='needs_review' AND hidden=0 ORDER BY id")
        .and_then(|mut stmt| {
            let iter = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            Ok(iter.filter_map(|x| x.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(id, title, artist, album, meta)| {
            let cands: Vec<provider::Candidate> = serde_json::from_str(meta.as_deref()?).ok()?;
            let conf = cands.first().map(|c| c.conf).unwrap_or(0.0);
            Some(EnrichItem { track_id: id, title, artist, album, conf, candidates: cands })
        })
        .collect()
}

#[tauri::command]
fn apply_enrich(
    db: State<'_, Mutex<Db>>,
    track_id: i64,
    cand_idx: usize,
    fields: Option<Vec<String>>,
) -> Result<(), String> {
    let g = db.lock().unwrap();
    let meta: Option<String> = g
        .0
        .query_row("SELECT online_meta FROM tracks WHERE id=?1", [track_id], |r| r.get(0))
        .ok();
    let cands: Vec<provider::Candidate> =
        serde_json::from_str(meta.as_deref().ok_or("无候选")?).map_err(|e| e.to_string())?;
    let c = cands.get(cand_idx).ok_or("候选不存在")?;
    if let Some(list) = &fields {
        if list.is_empty() {
            return Err("未选择任何字段".into());
        }
    }
    provider::apply_candidate(&g.0, track_id, c, "applied", fields.as_deref())
}

#[tauri::command]
fn dismiss_enrich(db: State<'_, Mutex<Db>>, track_id: i64) -> Result<(), String> {
    let g = db.lock().unwrap();
    g.0.execute(
        "UPDATE tracks SET enrichment_status='dismissed' WHERE id=?1",
        [track_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 歌手信息：纯读缓存（在线获取由用户在面板手动触发后择要写入）
#[tauri::command]
fn artist_info(db: State<'_, Mutex<Db>>, name: String) -> serde_json::Value {
    let g = db.lock().unwrap();
    let (img, wiki, meta) = g.0.query_row(
        "SELECT img, wiki, meta FROM artist_cache WHERE name=?1",
        [&name],
        |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)),
    ).unwrap_or((None, None, None));
    // 历史缓存可能是繁体，读取时统一转简体（词表转换，只影响显示）
    serde_json::json!({ "img": img, "wiki": wiki.map(|w| zhconv::t2s(&w)), "meta": meta.map(|m| zhconv::t2s(&m)) })
}

/// 在线聚合包（手动触发面板用）：多渠道歌手图（带源）+ 维基简介 + MB 信息，不写缓存
#[tauri::command]
async fn fetch_artist_pack(app: AppHandle, name: String, base_url: Option<String>) -> Result<serde_json::Value, String> {
    // 自定义源（/artist）：图 + 中文简介
    let n0 = name.clone();
    let b0 = base_url.clone().filter(|s| !s.trim().is_empty());
    let t_custom = tokio::task::spawn(async move {
        let Some(b) = b0 else { return None; };
        let url = format!("{}/artist?artist={}", b.trim_end_matches('/'), crate::lyrics::urlencode(&n0));
        let Ok(resp) = reqwest::Client::builder().user_agent("Lyra/0.1").timeout(std::time::Duration::from_secs(8)).build()
            .ok()?.get(&url).send().await else { return None; };
        if !resp.status().is_success() { return None; }
        resp.json::<serde_json::Value>().await.ok()
    });
    let fk = read_cfg(&app)["keys"]["fanart"].as_str().unwrap_or("").trim().to_string();
    let n1 = name.clone();
    let n2 = name.clone();
    let n3 = name.clone();
    let fk2 = fk.clone();
    let t_fanart = tokio::task::spawn(async move {
        if fk2.is_empty() { return vec![]; }
        match provider::mb_artist_mbid(&n1).await {
            Some(mbid) => provider::fanart_artist_imgs(&mbid, &fk2).await,
            None => vec![],
        }
    });
    let t_deezer = tokio::task::spawn(async move {
        provider::deezer_artist_imgs(&n2).await
    });
    let t_wiki = tokio::task::spawn(async move { provider::wiki_intro(&n3).await });
    let n4 = name.clone();
    let t_mb = tokio::task::spawn(async move { provider::mb_artist_meta(&n4).await });
    // TheAudioDB（行业标准源，需赞助 key）：图 + 中文简介
    let adbk = read_cfg(&app)["keys"]["theaudiodb"].as_str().unwrap_or("").trim().to_string();
    let n7 = name.clone();
    let t_adb = tokio::task::spawn(async move {
        if adbk.is_empty() { return None; }
        provider::theaudiodb_artist(&adbk, &n7).await
    });
    // Wikipedia 肖像（词条官方照片，质量最高，排最前）
    let n6 = name.clone();
    let t_wimg = tokio::task::spawn(async move { provider::wiki_artist_img(&n6).await });
    // Last.fm（配置 key 后）：简介/标签（图片不再纳入候选——QQ/Deezer/fanart 覆盖更好且免 key）
    let lf = read_cfg(&app)["keys"]["lastfm"].as_str().unwrap_or("").trim().to_string();
    let n5 = name.clone();
    let t_lfm = tokio::task::spawn(async move {
        if lf.is_empty() { return None; }
        provider::lastfm_artist_pack(&n5, &lf).await
    });
    let fanart = t_fanart.await.unwrap_or_default();
    let deezer = t_deezer.await.unwrap_or_default();
    let wiki = t_wiki.await.unwrap_or(None);
    let mb = t_mb.await.unwrap_or(None);
    let lfm = t_lfm.await.unwrap_or(None);
    let wimg = t_wimg.await.unwrap_or(None);
    let adb = t_adb.await.unwrap_or(None);
    let custom = t_custom.await.unwrap_or(None);
    // 本地约定图（artist.jpg）最优先
    let local: Vec<String> = {
        let g = app.state::<Mutex<Db>>();
        let conn = &g.lock().unwrap().0;
        local_artist_imgs(conn, &name).into_iter().filter_map(|s| s.strip_prefix("local:").map(|x| x.to_string())).collect()
    };
    let mut imgs: Vec<serde_json::Value> = local.into_iter().take(1).map(|u| serde_json::json!({ "src": "本地", "url": format!("local:{}", u) })).collect();
    if let Some(c) = &custom {
        imgs.extend(c["imgs"].as_array().cloned().unwrap_or_default().into_iter()
            .filter_map(|u| u.as_str().map(|s| serde_json::json!({ "src": "自定义源", "url": s }))));
    }
    if let Some(w) = wimg { imgs.push(serde_json::json!({ "src": "Wikipedia", "url": w })); }
    if let Some(a) = &adb {
        imgs.extend(a["imgs"].as_array().cloned().unwrap_or_default().into_iter()
            .filter_map(|u| u.as_str().map(|s| serde_json::json!({ "src": "TheAudioDB", "url": s }))));
    }
    imgs.extend(fanart.into_iter().map(|u| serde_json::json!({ "src": "fanart.tv", "url": u })));
    imgs.extend(deezer.into_iter().map(|u| serde_json::json!({ "src": "Deezer", "url": u })));
    Ok(serde_json::json!({ "imgs": imgs, "wiki": wiki, "mb": mb, "lfm": lfm, "adb": adb, "custom": custom }))
}

/// 面板择要写入：field ∈ img/wiki/meta（value 空串=清除该字段）
#[tauri::command]
fn set_artist_field(app: AppHandle, db: State<'_, Mutex<Db>>, name: String, field: String, value: String) -> Result<(), String> {
    if !["img", "wiki", "meta"].contains(&field.as_str()) {
        return Err("非法字段".into());
    }
    let v = if value.trim().is_empty() { None } else { Some(value) };
    // 块内持锁写库后释放（auto_backup 会再拿锁，重入会死锁）
    {
        let g = db.lock().unwrap();
        let sql = format!("INSERT INTO artist_cache(name, img, wiki, meta) VALUES(?1,?2,?3,?4)
            ON CONFLICT(name) DO UPDATE SET {field}=excluded.{field}");
        let (img, wiki, meta) = match field.as_str() {
            "img" => (v, None, None),
            "wiki" => (None, v, None),
            _ => (None, None, v),
        };
        // 未指定的字段保留原值：先读旧值
        let (oimg, owiki, ometa) = g.0.query_row(
            "SELECT img, wiki, meta FROM artist_cache WHERE name=?1", [&name],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)),
        ).unwrap_or((None, None, None));
        let img = img.or(oimg);
        let wiki = wiki.or(owiki);
        let meta = meta.or(ometa);
        g.0.execute(&sql, rusqlite::params![name, img, wiki, meta]).map_err(|e| e.to_string())?;
    }
    auto_backup(&db, &app, "artist", &name);
    Ok(())
}

#[tauri::command]
async fn test_provider(app: AppHandle, id: String) -> Result<serde_json::Value, String> {
    let t0 = std::time::Instant::now();
    let r = match id.as_str() {
        "lrclib" => lyrics::test_lrclib().await,
        "itunes" => provider::search_album_art("Jay Chou", "范特西").await
            .map(|v| serde_json::json!({ "ok": true, "detail": format!("封面 {} 张", v.len()) })),
        "deezer" => provider::search_deezer_art("Jay Chou", "范特西").await
            .map(|v| serde_json::json!({ "ok": true, "detail": format!("封面 {} 张", v.len()) })),
        "mb" => Ok(provider::mb_artist_meta("周杰伦").await
            .map(|m| serde_json::json!({ "ok": true, "detail": m }))
            .unwrap_or(serde_json::json!({ "ok": false, "detail": "无结果" }))),
        "wiki" => Ok(provider::wiki_intro("周杰伦").await
            .map(|w| serde_json::json!({ "ok": true, "detail": w.chars().take(60).collect::<String>() + "…" }))
            .unwrap_or(serde_json::json!({ "ok": false, "detail": "无结果" }))),
        "lastfm" => {
            let key = read_cfg(&app)["keys"]["lastfm"].as_str().unwrap_or("").trim().to_string();
            if key.is_empty() { Err("请先配置 API Key（点击卡片上的「配置 Key」）".into()) }
            else { provider::test_lastfm(&key).await }
        }
        "fanart" => {
            let key = read_cfg(&app)["keys"]["fanart"].as_str().unwrap_or("").trim().to_string();
            if key.is_empty() { Err("请先配置 API Key（点击卡片上的「配置 Key」）".into()) }
            else { provider::test_fanart(&key).await }
        }
        "discogs" => {
            let key = read_cfg(&app)["keys"]["discogs"].as_str().unwrap_or("").trim().to_string();
            if key.is_empty() { Err("请先配置 Token（点击卡片上的「配置 Key」）".into()) }
            else { provider::test_discogs(&key).await }
        }
        "theaudiodb" => {
            let key = read_cfg(&app)["keys"]["theaudiodb"].as_str().unwrap_or("").trim().to_string();
            if key.is_empty() { Err("请先配置 API Key（ Patreon 赞护获取，见卡片链接）".into()) }
            else { Ok(provider::theaudiodb_artist(&key, "coldplay").await
                .map(|v| serde_json::json!({ "ok": true, "detail": format!("图 {} 张", v["imgs"].as_array().map(|a| a.len()).unwrap_or(0)) }))
                .unwrap_or(serde_json::json!({ "ok": false, "detail": "Key 无效或无结果" }))) }
        }
        "acoustid" => {
            let key = read_cfg(&app)["keys"]["acoustid"].as_str().unwrap_or("").trim().to_string();
            if key.is_empty() { Err("请先配置 API Key（点击卡片上的「配置 Key」）".into()) }
            else if key.len() == 32 { Ok(serde_json::json!({ "ok": true, "detail": "Key 格式校验通过" })) }
            else { Err("Key 格式不符（AcoustID key 为 32 位字符串）".into()) }
        }
        _ => Err("未知供应商".into()),
    };
    r.map(|mut v| { v["latency_ms"] = serde_json::json!(t0.elapsed().as_millis()); v })
}

#[tauri::command]
fn get_providers() -> serde_json::Value {
    serde_json::json!([
        {"id":"lrclib","nm":"LRCLIB","cat":["歌词"],"caps":["同步歌词"],"auth":"无 key（可注册提额）","rate":"宽松","on":true,"desc":"开放同步歌词库，同一响应含纯文本 + 时间戳 LRC；已接入 ✓"},


        {"id":"mb","nm":"MusicBrainz","cat":["元数据","歌手信息"],"caps":["元数据","排序"],"auth":"无 key（要求 UA）","rate":"1 req/s 严格","on":true,"desc":"已接入：歌手类型/国家/活跃年代（1 req/s 限流）"},
        {"id":"acoustid","nm":"AcoustID","cat":["元数据"],"caps":["指纹识别"],"auth":"免费 key","rate":"非商业免费","on":false,"needKey":true,"desc":"配置 Key 后可验证；指纹识别接入下个版本"},
        {"id":"itunes","nm":"iTunes Search","cat":["元数据","封面图片"],"caps":["元数据","封面"],"auth":"无 key","rate":"~20 req/min","on":true,"desc":"已接入：封面兜底链 + 目录检索（免 key）"},
        {"id":"deezer","nm":"Deezer","cat":["元数据","封面图片","歌手图片"],"caps":["元数据","封面"],"auth":"无 key","rate":"无公布硬配额","on":true,"desc":"已接入：封面兜底链 + 歌手图片（免 key）"},
        {"id":"discogs","nm":"Discogs","cat":["元数据","封面图片"],"caps":["发行版"],"auth":"免费 token","rate":"60 req/min","on":false,"needKey":true,"desc":"配置 Token 后可验证；发行版元数据接入下个版本"},
        {"id":"lastfm","nm":"Last.fm","cat":["元数据","封面图片"],"caps":["元数据","封面"],"auth":"免费 key","rate":"5 req/s","on":false,"needKey":true,"desc":"配置 Key 后即接入封面兜底链与歌手简介"},


        {"id":"theaudiodb","nm":"TheAudioDB","cat":["歌手图片","歌手信息"],"caps":["歌手图","中文简介"],"auth":"赞助 key（Patreon）","rate":"赞助期内宽松","on":false,"needKey":true,"desc":"行业标准源（Kodi 系）：多尺寸歌手图 + 中文简介；免费测试 key 已停用"},
        {"id":"fanart","nm":"fanart.tv","cat":["歌手图片"],"caps":["歌手图","艺术图"],"auth":"免费 key","rate":"宽松","on":false,"needKey":true,"desc":"配置 Key 后即接入歌手图片（经 MusicBrainz mbid）"},
        {"id":"wiki","nm":"Wikipedia / Wikidata","cat":["歌手信息"],"caps":["简介"],"auth":"无 key","rate":"宽松","on":true,"desc":"已接入：歌手简介（中文维基优先，英文兜底）"}
    ])
}

/// 会话快照保存（ticker 每 5s 与程序退出时调用）
fn save_session(app: &AppHandle) {
    let player = app.state::<Player>();
    let q = player.queue.lock().unwrap();
    let sess = serde_json::json!({
        "queue": &*q,
        "index": player.cur_index.load(std::sync::atomic::Ordering::Relaxed),
        "position": player.position_ms() as f64 / 1000.0,
        "volume": player.volume(),
    });
    let p = db::db_path(app).with_file_name("session.json");
    let _ = std::fs::write(p, sess.to_string());
}

/// Now Playing 命令（专用线程持有 MediaControls，通道通信规避 Send 限制）
enum NpCmd {
    Meta { title: String, artist: String, album: String, duration: f64, cover: Option<String> },
    State { playing: bool, position: f64 },
}
static NP_TX: std::sync::OnceLock<std::sync::mpsc::Sender<NpCmd>> = std::sync::OnceLock::new();

/// 媒体键/系统控制中心 → 播放器动作（souvlaki 回调）
fn media_action(app: &AppHandle, action: souvlaki::MediaControlEvent) {
    let player = app.state::<Player>();
    match action {
        souvlaki::MediaControlEvent::Play => {
            if player.finished() {
                let _ = load_current(app, &player);
            } else {
                player.play();
            }
        }
        souvlaki::MediaControlEvent::Pause => player.pause(),
        souvlaki::MediaControlEvent::Toggle => {
            if !player.loaded() || player.finished() {
                let _ = load_current(app, &player);
            } else if player.is_playing() {
                player.pause();
            } else {
                player.play();
            }
        }
        souvlaki::MediaControlEvent::Next => {
            let _ = advance(app, &player, 1);
        }
        souvlaki::MediaControlEvent::Previous => {
            let _ = advance(app, &player, -1);
        }
        souvlaki::MediaControlEvent::Stop => player.stop(),
        _ => {}
    }
}

/// ticker 每轮调用：曲目/播放状态变化时推送 Now Playing 更新
fn update_now_playing(_app: &AppHandle, p: &PositionEvent) {
    let Some(tx) = NP_TX.get() else { return };
    static LAST_META: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
    static LAST_PLAYING: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);
    let tid = p.track.as_ref().map(|t| t.id).unwrap_or(-1);
    if LAST_META.swap(tid, std::sync::atomic::Ordering::Relaxed) != tid {
        if let Some(t) = &p.track {
            let _ = tx.send(NpCmd::Meta {
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                duration: p.duration,
                cover: t.cover.as_deref().map(|c| format!("file://{c}")),
            });
        }
    }
    let now_playing = p.playing as i8;
    if LAST_PLAYING.swap(now_playing, std::sync::atomic::Ordering::Relaxed) != now_playing {
        let _ = tx.send(NpCmd::State { playing: p.playing, position: p.position });
    }
}

/// 启动 Now Playing 专用线程：创建 MediaControls + attach 媒体键回调 + 处理更新命令
fn start_now_playing(app: &AppHandle) {
    use souvlaki::{MediaControls, MediaMetadata, MediaPosition, PlatformConfig};
    let (tx, rx) = std::sync::mpsc::channel::<NpCmd>();
    let h = app.clone();
    std::thread::Builder::new()
        .name("lyra-nowplaying".into())
        .spawn(move || {
            let mut mc = match MediaControls::new(PlatformConfig {
                dbus_name: "lyra",
                display_name: "好好听歌",
                hwnd: None,
            }) {
                Ok(mc) => mc,
                Err(e) => {
                    eprintln!("[media] Now Playing 初始化失败（媒体键不可用）: {e:?}");
                    // 消费命令避免发送端报错
                    while rx.recv().is_ok() {}
                    return;
                }
            };
            {
                let h2 = h.clone();
                let _ = mc.attach(move |action| media_action(&h2, action));
            }
            eprintln!("[media] 媒体键 / Now Playing 已启用");
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    NpCmd::Meta { title, artist, album, duration, cover } => {
                        let _ = mc.set_metadata(MediaMetadata {
                            title: Some(&title),
                            artist: Some(&artist),
                            album: Some(&album),
                            duration: Some(std::time::Duration::from_secs_f64(duration.max(0.0))),
                            cover_url: cover.as_deref(),
                        });
                    }
                    NpCmd::State { playing, position } => {
                        let pos = MediaPosition(std::time::Duration::from_secs_f64(position.max(0.0)));
                        let _ = mc.set_playback(if playing {
                            souvlaki::MediaPlayback::Playing { progress: Some(pos) }
                        } else {
                            souvlaki::MediaPlayback::Paused { progress: Some(pos) }
                        });
                    }
                }
            }
        })
        .ok();
    let _ = NP_TX.set(tx);
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            // macOS 后台播放（设置 → 功能开关，默认关）：开启时点红色关闭钮 = 隐藏窗口继续播放
            // （Tauri v2 默认窗口全关即退出，与 AppKit 原生惯例不同）。拦截 CloseRequested 改 hide，
            // WebView 状态全保留，重开秒回。⌘Q / 托盘退出走 terminate 路径不受影响。
            #[cfg(target_os = "macos")]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if read_cfg(window.app_handle())["bgplay"].as_bool().unwrap_or(false) {
                    api.prevent_close();
                    let _ = window.hide();
                }
                return;
            }
            // 拖拽入库：拖文件夹到窗口 → 扫描入库并监听（设计文档拖拽项）
            if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                for p in paths {
                    if p.is_dir() {
                        let app = window.app_handle();
                        {
                            let g = app.state::<Mutex<Db>>();
                            db::add_scan_root(&g.lock().unwrap().0, &p.to_string_lossy());
                        }
                        if let Some((found, parsed, gone)) = try_scan(app, p, false) {
                            let _ = app.emit(
                                "scan-done",
                                serde_json::json!({ "changed": parsed, "removed": gone, "auto": true, "total": found }),
                            );
                        }
                        rebuild_watcher(app);
                    }
                }
            }
            // 第三重保险：窗口关闭/销毁时保存（点 ✕ 的第一现场）
            if matches!(event, tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed) {
                save_session(window.app_handle());
            }
        })
        .setup(|app| {
            let conn = db::open(&db::db_path(app.handle()));
            // 歌手图兜底（幂等）：启动归一后仍整组无封面的专辑，用歌手目录图填充
            db::fill_artist_covers(&conn, &db::covers_dir(app.handle()));
            // has_tr 后台回填（幂等：只处理未标记行）：完成 emit 通知前端刷新，
            // 否则列表徽标一直是启动时拉的 0 值——后端写库不会自动反映到前端内存
            {
                let bp = db::db_path(app.handle());
                let h = app.handle().clone();
                std::thread::Builder::new()
                    .name("lyra-has-tr-backfill".into())
                    .spawn(move || {
                        if db::backfill_has_tr(&bp) > 0 {
                            let _ = h.emit("library-changed", ());
                        }
                    })
                    .ok();
            }
            app.manage(Mutex::new(Db(conn)));
            let player = Player::new().expect("启动音频线程失败");
            player.set_volume(0.8);
            // 恢复上次播放上下文（队列/曲目/音量/位置；不自动开播，点播放键从上次位置继续）
            let sess_path = db::db_path(app.handle()).with_file_name("session.json");
            if let Ok(text) = std::fs::read_to_string(&sess_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let queue: Vec<i64> = v["queue"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
                    let index = v["index"].as_i64().unwrap_or(-1);
                    let vol = v["volume"].as_f64().unwrap_or(0.8) as f32;
                    let pos = v["position"].as_f64().unwrap_or(0.0);
                    if !queue.is_empty() && index >= 0 {
                        *player.queue.lock().unwrap() = queue;
                        player.cur_index.store(index, std::sync::atomic::Ordering::Relaxed);
                        player.set_volume(vol);
                        if pos > 1.0 {
                            player.set_resume_pos(pos);
                        }
                        // 启动即预加载：解码并 seek 到上次位置（暂停态不出声），
                        // 点播放键零延迟；底栏立即恢复时长/进度显示
                        let handle = app.handle().clone();
                        let _ = send_current(&handle, &player, true);
                    }
                }
            }
            // 恢复渐入渐出 / EQ 配置，须在 manage(move) 之前
            let cfgv = read_cfg(app.handle());
            if let Some(ms) = cfgv["fade_ms"].as_i64() {
                player.set_fade(ms as i32);
            }
            if let Some(sp) = cfgv["speed"].as_f64() {
                player.set_speed(sp as f32);
            }
            let sh_mode = cfgv["shuffle_mode"].as_i64()
                .map(|m| if m > 0 { 1 } else { 0 })
                .unwrap_or_else(|| if cfgv["shuffle"].as_bool().unwrap_or(false) { 1 } else { 0 });
            player.shuffle_mode.store(sh_mode, std::sync::atomic::Ordering::Relaxed);
            player.shuffle.store(sh_mode > 0, std::sync::atomic::Ordering::Relaxed);
            if let Some(rp) = cfgv["repeat"].as_i64() {
                player.repeat_mode.store(rp.clamp(0, 2) as u8, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(cf) = cfgv["crossfade_ms"].as_i64() {
                player.set_crossfade(cf.clamp(0, 10_000) as u32);
            }
            if let Some(eq_on) = cfgv["eq_on"].as_bool() {
                let gains = cfgv["eq"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect::<Vec<f32>>())
                    .unwrap_or_default();
                let t_on = cfgv["tone_on"].as_bool().unwrap_or(false);
                let t_bass = cfgv["tone_bass"].as_f64().unwrap_or(0.0) as f32;
                let t_treble = cfgv["tone_treble"].as_f64().unwrap_or(0.0) as f32;
                player.set_eq(eq_on, &gains, t_on, t_bass, t_treble);
            }
            app.manage(player);
            // —— souvlaki：媒体键 + Now Playing（设计文档 7.1，专用线程）——
            start_now_playing(app.handle());
            // —— 托盘：播放/暂停/上一首/下一首/退出（设计文档 7.2）——
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::TrayIconBuilder;
                let play = MenuItem::with_id(app, "play", "播放/暂停", true, None::<&str>)?;
                let prev = MenuItem::with_id(app, "prev", "上一首", true, None::<&str>)?;
                let next = MenuItem::with_id(app, "next", "下一首", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&prev, &play, &next, &quit])?;
                TrayIconBuilder::with_id("lyra-tray")
                    .icon(app.default_window_icon().cloned().ok_or("无图标")?)
                    .tooltip("好好听歌")
                    .menu(&menu)
                    .on_menu_event(|app, ev| match ev.id().as_ref() {
                        "play" => media_action(app, souvlaki::MediaControlEvent::Toggle),
                        "prev" => media_action(app, souvlaki::MediaControlEvent::Previous),
                        "next" => media_action(app, souvlaki::MediaControlEvent::Next),
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .build(app)?;
            }
            // watcher 持有者（Option 便于重建时替换）
            app.manage(Mutex::<Option<notify::RecommendedWatcher>>::new(None));
            // 启动 3 秒后：对已配置目录静默增量重扫 + 建立 notify 监听
            let boot = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let roots = {
                    let g = boot.state::<Mutex<Db>>();
                    let gl = g.lock().unwrap();
                    db::scan_roots(&gl.0)
                };
                let mut any_changed = false;
                for r in &roots {
                    if let Some((_, parsed, _)) = try_scan(&boot, std::path::Path::new(r), true) {
                        if parsed > 0 {
                            any_changed = true;
                        }
                    }
                }
                if any_changed {
                    let _ = boot.emit("scan-done", serde_json::json!({ "changed": 0, "removed": 0, "auto": true, "boot": true }));
                    let _ = boot.emit("library-changed", ());
                }
                rebuild_watcher(&boot);
            });

            // position ticker：500ms 事件推送（设计文档 8.2：事件驱动，前端不轮询）
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                let player = handle.state::<Player>();
                let db = handle.state::<Mutex<Db>>();
                let idx = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);
                let qlen = player.queue.lock().unwrap().len();

                // 衔接曲开播通知：更新 cur_index / cur_track_id / 时长 + RG
                if player.take_switch_notify() {
                    let pf = player.prefetch_idx.load(std::sync::atomic::Ordering::Relaxed);
                    eprintln!("[ticker] switch notify pf={}", pf);
                    if pf >= 0 {
                        player.cur_index.store(pf, std::sync::atomic::Ordering::Relaxed);
                        // 队列锁只取 tid 即放（后续 apply_rg/DB/shuffle_nav 各自拿锁，
                        // 避免持 queue 锁时再拿其它锁造成同线程重入死锁）
                        let tid = {
                            let q = player.queue.lock().unwrap();
                            q.get(pf as usize).copied()
                        };
                        if let Some(tid) = tid {
                            player.cur_track_id.store(tid, std::sync::atomic::Ordering::Relaxed);
                            apply_rg(&handle, &player, tid);
                            let g = db.lock().unwrap();
                            if let Ok(dur) = g.0.query_row(
                                "SELECT duration FROM tracks WHERE id=?1",
                                [tid],
                                |r| r.get::<_, f64>(0),
                            ) {
                                player.set_duration_ms((dur * 1000.0) as u64);
                            }
                        }
                        // pending 消费完毕：允许下一轮预排
                        player.prefetch_idx.store(-1, std::sync::atomic::Ordering::Relaxed);
                        // 随机轨迹步进：衔接曲就是轨迹上的下一首（预排时 peek 的）
                        if player.shuffle_mode.load(std::sync::atomic::Ordering::Relaxed) == 1 {
                            let _ = shuffle_nav(&handle, &player, idx, 1, true);
                        }
                    }
                }

                // 曲尾衔接：crossfade 开启时提前 N 秒交叉过渡；否则无缝预排（2.5s）
                // （单曲循环模式不预排，播完由 finished 分支重播）
                let cf_ms = player.crossfade_ms() as u64;
                if player.is_playing()
                    && !player.finished()
                    && idx >= 0
                    && qlen > 1
                    && player.duration_ms() > 4000
                    && player.repeat_mode.load(std::sync::atomic::Ordering::Relaxed) != 2
                {
                    let lead = if cf_ms > 0 { cf_ms + 500 } else { 2500 };
                    let remaining = player.duration_ms().saturating_sub(player.position_ms());
                    if remaining < lead {
                        if let Some(next_idx) = compute_next_index(&handle, &player, idx) {
                            // pending 状态机：-1=无预排。随机模式下抽签每轮不同，
                            // 不能靠比较 idx 防重——统一用 pending 标志防重复 append
                            if player.prefetch_idx.load(std::sync::atomic::Ordering::Relaxed) == -1 {
                                let q = player.queue.lock().unwrap();
                                if let Some(tid) = q.get(next_idx as usize).copied() {
                                    drop(q);
                                    let g = db.lock().unwrap();
                                    let row: Option<(String, f64)> = g
                                        .0
                                        .prepare("SELECT file_path, duration FROM tracks WHERE id=?1")
                                        .ok()
                                        .and_then(|mut s| {
                                            s.query_row([tid], |r| Ok((r.get(0)?, r.get(1)?))).ok()
                                        });
                                    drop(g);
                                    if let Some((path, dur)) = row {
                                        player.prefetch_idx.store(next_idx, std::sync::atomic::Ordering::Relaxed);
                                        if cf_ms > 0 {
                                            let _ = player.crossfade_next(path.into(), dur);
                                        } else {
                                            let _ = player.append_next(path.into(), dur);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // 自然播完 → 按循环模式处理：
                // 单曲循环：重播当前；列表循环：下一首（尾→头）；关：仅最后一曲后停止
                if player.finished() && qlen > 0 && idx >= 0 {
                    let mode = player.repeat_mode.load(std::sync::atomic::Ordering::Relaxed);
                    player.prefetch_idx.store(-1, std::sync::atomic::Ordering::Relaxed);
                    if mode == 2 {
                        let _ = load_current(&handle, &player); // 单曲循环：重播
                    } else if mode == 1 || idx < qlen as i64 - 1 {
                        advance(&handle, &player, 1);
                    } else {
                        player.pause(); // 循环关：队列播完保持停止
                    }
                }
                let idx = player.cur_index.load(std::sync::atomic::Ordering::Relaxed);

                let track = {
                    let conn = &db.lock().unwrap().0;
                    if idx >= 0 {
                        let q = player.queue.lock().unwrap();
                        q.get(idx as usize)
                            .and_then(|id| cur_track(conn, *id))
                    } else {
                        None
                    }
                };
                let payload = PositionEvent {
                    playing: player.is_playing(),
                    position: player.position_ms() as f64 / 1000.0,
                    duration: player.duration_ms() as f64 / 1000.0,
                    volume: player.volume(),
                    shuffle: player.shuffle.load(std::sync::atomic::Ordering::Relaxed),
                    repeat: player.repeat_mode.load(std::sync::atomic::Ordering::Relaxed),
                    queue_index: idx,
                    queue_len: qlen,
                    track,
                };
                // Now Playing 元数据（souvlaki，设计文档 7.1）：曲目/播放状态变化时同步
                update_now_playing(&handle, &payload);
                if let Err(e) = handle.emit("playback-position", &payload) {
                    eprintln!("[ticker] emit 失败: {e}");
                }
                // 低频诊断：每 5 秒打印一次状态，便于定位事件链路
                static TICK_COUNT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let c = TICK_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if c % 10 == 0 {
                    eprintln!(
                        "[ticker] idx={} qlen={} playing={} pos={:.1}s dur={:.1}s track={}",
                        idx,
                        qlen,
                        payload.playing,
                        payload.position,
                        payload.duration,
                        payload
                            .track
                            .as_ref()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| "None".into())
                    );
                    // 会话持久化（设计文档 6.1：重启恢复"上次播放上下文"快照）
                    save_session(&handle);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pick_and_scan,
            scan_dir,
            get_scan_roots,
            remove_scan_root,
            albums,
            all_tracks,
            list_playlists,
            playlist_detail,
            create_playlist,
            rename_playlist,
            delete_playlist,
            add_to_playlist,
            remove_playlist_track,
            move_playlist_track,
            play_tracks,
            toggle_play,
            next_track,
            prev_track,
            queue_list,
            artist_cached_imgs,
            open_external,
            get_keys,
            set_key,
            get_config,
            artist_info,
            fetch_artist_pack,
            set_artist_field,
            set_shuffle_mode,
            set_track_hidden,
            hidden_tracks,
            queue_remove,
            queue_jump,
            seek,
            set_volume,
            set_shuffle,
            set_repeat,
            toggle_like,
            smart_list,
            get_cache_info,
            set_cache_config,
            clear_cache,
            get_fade,
            set_fade,
            get_speed,
            set_speed,
            backup_library,
            restore_library,
            list_auto_backups,
            restore_auto_backup,
            test_custom_source,
            export_playlist_m3u,
            dup_scan,
            clean_covers,
            get_bgplay,
            set_bgplay,
            writeback_track,
            writeback_all,
            get_stats,
            get_crossfade,
            set_crossfade,
            update_track_fields,
            set_rg_mode,
            get_rg_mode,
            get_eq,
            set_eq,
            import_m3u8,
            fetch_album_covers,
            apply_album_cover,
            get_position,
            get_lyrics,
            fetch_online_lyrics,
            meta_search,
            meta_fetch_item,
            lyrics_search,
            lyrics_fetch_item,
            lyrics_apply,
            fetch_translation,
            enrich_missing,
            list_enrich,
            apply_enrich,
            dismiss_enrich,
            test_provider,
            get_providers,
        ])
        .build(tauri::generate_context!())
        .expect("error while building lyra")
        .run(|app, event| {
            // 退出时保存最终会话（消除 5s 快照误差）：ExitRequested 与 Exit 双保险
            match event {
                // macOS：点 Dock 图标重开主窗口（关窗后台播放期间唯一的唤起入口）
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { has_visible_windows, .. } => {
                    if !has_visible_windows {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                }
                tauri::RunEvent::ExitRequested { .. } => {
                    save_session(app);
                    eprintln!("[session] ExitRequested 保存完成");
                }
                tauri::RunEvent::Exit => {
                    save_session(app);
                    eprintln!("[session] Exit 保存完成");
                }
                _ => {}
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{parse_auto_bk_name, search_terms_clean};

    #[test]
    fn cleans_search_terms() {
        // 括号后缀（中英文）剔除
        assert_eq!(search_terms_clean("半兽人 (Live)", "周杰伦"), ("半兽人".into(), "周杰伦".into()));
        assert_eq!(search_terms_clean("半岛铁盒（电视剧版）", "周杰伦"), ("半岛铁盒".into(), "周杰伦".into()));
        // feat. 段截断
        assert_eq!(search_terms_clean("Song feat. Someone", "A"), ("Song".into(), "A".into()));
        // 合作艺人取首位
        assert_eq!(search_terms_clean("T", "张三/李四"), ("T".into(), "张三".into()));
        assert_eq!(search_terms_clean("T", "A、B & C"), ("T".into(), "A".into()));
        // 干净输入不受伤
        assert_eq!(search_terms_clean("如果你也听说", "张惠妹"), ("如果你也听说".into(), "张惠妹".into()));
        // 占位艺人不传（拼进关键词会污染源端搜索）
        assert_eq!(search_terms_clean("T", "未知艺术家"), ("T".into(), String::new()));
        assert_eq!(search_terms_clean("T", "Unknown Artist"), ("T".into(), String::new()));
    }

    #[test]
    fn parses_auto_backup_filename() {
        assert_eq!(
            parse_auto_bk_name("auto-1762934400123-cover.zip"),
            Some((1762934400123u128, "cover".into()))
        );
        assert_eq!(parse_auto_bk_name("auto-5-tr.zip"), Some((5u128, "tr".into())));
    }

    #[test]
    fn rejects_invalid_names() {
        // 路径穿越 / 非法 trigger / 缺组件 → 全部拒绝（restore_auto_backup 靠此防穿越）
        assert_eq!(parse_auto_bk_name("../lyra.db"), None);
        assert_eq!(parse_auto_bk_name("auto-123-..zip"), None);
        assert_eq!(parse_auto_bk_name("auto-123-Cover.zip"), None);
        assert_eq!(parse_auto_bk_name("auto-abc-cover.zip"), None);
        assert_eq!(parse_auto_bk_name("auto-123.zip"), None);
        assert_eq!(parse_auto_bk_name("cover.zip"), None);
    }
}
