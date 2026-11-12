//! 音频文件磁盘缓存（AppData/audio_cache）：
//! - 播放时后台把音频文件拷贝进缓存目录，再次播放直接读本地（外置盘/网络盘曲库提速）
//! - 配额（默认 2GB）超出按 LRU（文件 mtime）淘汰最旧
//! - 设置页可调上限/开关/一键清理
//! 缓存键：hash(原路径|文件大小)，命中校验大小一致

use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

#[derive(Serialize, Clone, Debug)]
pub struct CacheInfo {
    pub enabled: bool,
    pub limit_mb: u64,
    pub used_mb: u64,
}

fn app_data(app: &AppHandle) -> PathBuf {
    use tauri::Manager;
    app.path().app_data_dir().expect("no app data dir")
}

fn config_path(app: &AppHandle) -> PathBuf {
    app_data(app).join("config.json")
}

fn cache_dir(app: &AppHandle) -> PathBuf {
    let d = app_data(app).join("audio_cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn read_config(app: &AppHandle) -> (bool, u64) {
    // (enabled, limit_mb) 默认开、2GB
    if let Ok(text) = std::fs::read_to_string(config_path(app)) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            return (
                v["cache_enabled"].as_bool().unwrap_or(true),
                v["cache_limit_mb"].as_u64().unwrap_or(2048),
            );
        }
    }
    (true, 2048)
}

pub fn get_info(app: &AppHandle) -> CacheInfo {
    let (enabled, limit_mb) = read_config(app);
    CacheInfo {
        enabled,
        limit_mb,
        used_mb: total_size(&cache_dir(app)) / 1024 / 1024,
    }
}

pub fn set_config(app: &AppHandle, enabled: bool, limit_mb: u64) -> CacheInfo {
    let cfg = serde_json::json!({ "cache_enabled": enabled, "cache_limit_mb": limit_mb });
    let _ = std::fs::write(config_path(app), cfg.to_string());
    if enabled {
        evict(app, limit_mb);
    }
    get_info(app)
}

fn cache_key(orig: &Path) -> String {
    let size = std::fs::metadata(orig).map(|m| m.len()).unwrap_or(0);
    let mut h = DefaultHasher::new();
    orig.to_string_lossy().hash(&mut h);
    size.hash(&mut h);
    let ext = orig
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    format!("{:016x}{}", h.finish(), ext)
}

/// 命中返回缓存路径（touch mtime 供 LRU），未命中返回 None
fn hit(app: &AppHandle, orig: &Path) -> Option<PathBuf> {
    let c = cache_dir(app).join(cache_key(orig));
    if !c.is_file() {
        return None;
    }
    let want = std::fs::metadata(orig).ok()?.len();
    if std::fs::metadata(&c).ok()?.len() != want {
        let _ = std::fs::remove_file(&c); // 源文件变了，缓存作废
        return None;
    }
    // touch mtime（LRU 依据）
    let now = filetime::FileTime::now();
    let _ = set_mtime(&c, now);
    Some(c)
}

#[cfg(unix)]
fn set_mtime(p: &Path, t: filetime::FileTime) -> std::io::Result<()> {
    filetime::set_file_mtime(p, t)
}
#[cfg(not(unix))]
fn set_mtime(p: &Path, t: filetime::FileTime) -> std::io::Result<()> {
    filetime::set_file_times(p, t, t)
}

/// 解析播放路径：命中用缓存，未命中返回原路径并（若开启）后台拷贝入缓存
pub fn resolve(app: &AppHandle, orig: &Path) -> PathBuf {
    let (enabled, _limit) = read_config(app);
    if !enabled {
        return orig.to_path_buf();
    }
    if let Some(c) = hit(app, orig) {
        return c;
    }
    // 后台拷贝（源与缓存同盘无意义：仅当缓存目录与源不同设备时拷贝）
    let src = orig.to_path_buf();
    let app2 = app.clone();
    std::thread::spawn(move || put(&app2, &src));
    orig.to_path_buf()
}

fn total_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// 拷贝入缓存（已在缓存/源与缓存同盘则跳过）+ 淘汰
fn put(app: &AppHandle, orig: &Path) {
    let (enabled, limit_mb) = read_config(app);
    if !enabled || limit_mb == 0 {
        return;
    }
    let dst = cache_dir(app).join(cache_key(orig));
    if dst.is_file() {
        return;
    }
    // 同盘检测：缓存已在源所在设备上（比如库就在内置盘）没有意义
    if same_device(orig, &dst) {
        return;
    }
    let size = match std::fs::metadata(orig) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size > limit_mb * 1024 * 1024 {
        return; // 单文件超过配额，不缓存
    }
    let tmp = dst.with_extension("part");
    if std::fs::copy(orig, &tmp).is_ok() {
        let _ = std::fs::rename(&tmp, &dst);
    }
    let _ = std::fs::remove_file(&tmp);
    evict(app, limit_mb);
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b.parent().unwrap_or(a))) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}
#[cfg(not(unix))]
fn same_device(_a: &Path, _b: &Path) -> bool {
    false
}

/// LRU 淘汰：按 mtime 升序删除最旧，直到总量 ≤ 配额
fn evict(app: &AppHandle, limit_mb: u64) {
    let dir = cache_dir(app);
    let limit = limit_mb * 1024 * 1024;
    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let m = e.metadata().ok()?;
                    if !m.is_file() || e.path().extension().map(|x| x == "part").unwrap_or(false) {
                        return None;
                    }
                    Some((e.path(), m.len(), m.modified().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut total: u64 = entries.iter().map(|x| x.1).sum();
    if total <= limit {
        return;
    }
    entries.sort_by_key(|x| x.2);
    for (p, sz, _) in entries {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total = total.saturating_sub(sz);
        }
    }
}

/// 一键清理
pub fn clear(app: &AppHandle) -> CacheInfo {
    let dir = cache_dir(app);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    get_info(app)
}
