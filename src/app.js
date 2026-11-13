/* 好好听歌 前端：原型 UI × Tauri 真实数据 */
const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/* ================= 状态 ================= */
const L = { albums: [], tracks: [], playlists: [] };
let SMART_TRACKS = [];
const state = {
  hist: [],
  shuffleMode: 0,
  keys: {},
  artistImgs: {},
  view: 'albums', albumKey: null, artist: null, playlistId: null, tab: 'providers', folder: null, folderTab: 'tracks',
  tracksSort: { key: null, dir: 1 },   // 曲目视图排序：key=null 专辑序 | title 拼音 | dur 时长 | plays 播放次数 | added 添加时间；dir 1 升 | -1 降
  trackFilter: { genre: '', decade: '' },  // 曲目视图筛选：流派/年代（与搜索词 AND）
  version: '0.1.0',
  search: '',
  playing: false, curId: null, t: 0, dur: 0, volume: .8, shuffle: false,
  curTrack: null,      // 后端推送的当前曲（NowPlaying，队列变换后仍准确）
  queue: [],           // [{id,title,artist,album,cover,duration}]（队列面板用）
  queueIndex: -1,
  lyric: null,         // {source,synced,lines}
};
const trackById = id => L.tracks.find(t => t.id === id);
// 本地歌手图路径（"local:" 前缀）转 asset 协议
const artistImgSrc = u => u && u.startsWith('local:') ? convertFileSrc(u.slice(6)) : u;
const fmt = s => { s = Math.max(0, Math.round(s || 0)); return (s / 60 | 0) + ':' + String(s % 60).padStart(2, '0'); };
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const hue = s => { let h = 0; for (const c of s) h = (h * 31 + c.charCodeAt(0)) % 360; return h; };
const cstyle = t => `background:linear-gradient(135deg,hsl(${hue(t)} 46% 34%),hsl(${(hue(t) + 55) % 360} 52% 17%))`;
const SRC = { tag: ['本地标签', 'b-tag'], cleaned: ['已清洗', 'b-clean'], guessed: ['推断', 'b-guess'], online: ['在线', 'b-online'], manual: ['手动', 'b-clean'] };
const sbadge = s => { const [n, c] = SRC[s] || SRC.tag; return `<span class="badge ${c}">${n}</span>`; };

function cover(coverPath, title, size, radius) {
  const style = `width:${size}px;height:${size}px;${radius != null ? `border-radius:${radius}px;` : ''}`;
  if (coverPath) return `<div class="cover" style="${style}"><img src="${convertFileSrc(coverPath)}" loading="lazy"></div>`;
  return `<div class="cover flat" style="${style}${cstyle(title || 'Lyra')}"><span class="init" style="font-size:${Math.max(11, size * .3)}px">${(title || '?').slice(0, 2)}</span></div>`;
}

/* ================= 数据刷新 ================= */
async function refresh() {
  rebuildArtistSet();
  const [albums, tracks, pls] = await Promise.all([
    invoke('albums', { t2s: t2sEnabled() }), invoke('all_tracks', { t2s: t2sEnabled() }), invoke('list_playlists'),
  ]);
  L.albums = albums; L.tracks = tracks; L.playlists = pls;
  render();
}

/* ================= 侧栏 ================= */
function renderSide() {
  cntA.textContent = L.albums.length;
  cntR.textContent = new Set(L.albums.map(a => a.artist)).size;
  cntT.textContent = L.tracks.length;
  const noLyric = L.tracks.filter(t => !t.has_lyrics).length;
  cntLF.textContent = noLyric || '';
  cntFav.textContent = L.tracks.filter(t => t.liked).length || '';
  plList.innerHTML = L.playlists.map(p => `
    <div class="nav-item pl-item ${state.view === 'playlist' && state.playlistId === p.id ? 'on' : ''}" onclick="go('playlist',${p.id})">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 5h12v11H4zM16 8h4v8h-4"/></svg>
      <span style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(p.name)}</span><span class="cnt">${p.count}</span>
      <span class="pl-acts">
        <button title="重命名" onclick="event.stopPropagation();uiRename(${p.id})">✎</button>
        <button title="删除" onclick="event.stopPropagation();uiDelete(${p.id})">🗑</button></span></div>`).join('');
  const totalSec = L.tracks.reduce((n, t) => n + t.duration, 0);
}
function renderCrumb() {
  const m = { albums: '音乐库 / 专辑', artists: '音乐库 / 艺术家', tracks: '音乐库 / 曲目', folders: '音乐库 / 文件夹', stats: '音乐库 / 统计', dups: '工具 / 重复检测', lyricsfetch: '工具 / 歌词补全', settings: '工具 / 设置' };
  if (state.view === 'smart') m.smart = `智能歌单 / <b>${(SMART_META[state.smartKind] || {}).title || ''}</b>`;
  let s = m[state.view] || '';
  if (state.view === 'album') { const a = L.albums.find(x => x.album_key === state.albumKey); s = `音乐库 / <b>${esc(a ? a.album : '')}</b>`; }
  if (state.view === 'artist') s = `音乐库 / 艺术家 / <b>${esc(state.artist)}</b>`;
  if (state.view === 'playlist') { const p = L.playlists.find(x => x.id === state.playlistId); s = `歌单 / <b>${esc(p ? p.name : '')}</b>`; }
  crumb.innerHTML = s;
}
function go(v, id) {
  if (typeof v === 'string' && v !== state.view || (v === state.view && id !== undefined)) {
    // 历史栈（goBack 用）
    state.hist.push({ view: state.view, albumKey: state.albumKey, artist: state.artist, playlistId: state.playlistId, smartKind: state.smartKind });
    if (state.hist.length > 30) state.hist.shift();
  }
  state.view = v;
  if (v === 'album') state.albumKey = decodeURIComponent(id);
  if (v === 'artist') state.artist = decodeURIComponent(id);
  if (v === 'playlist') state.playlistId = id;
  if (v === 'smart') state.smartKind = id;
  render();
  if (v === 'playlist') loadPlaylistView();
  if (v === 'enrich') loadEnrich();
  if (v === 'stats') loadStats();
  if (v === 'smart') loadSmart();
}
function goBack() {
  const prev = state.hist && state.hist.pop();
  if (!prev) { go('albums'); return; }
  Object.assign(state, { view: prev.view, albumKey: prev.albumKey, artist: prev.artist, playlistId: prev.playlistId, smartKind: prev.smartKind });
  render();
  if (prev.view === 'playlist') loadPlaylistView();
  if (prev.view === 'smart') loadSmart();
}
const enc = encodeURIComponent;
// 顶栏按钮：有历史=返回；首页=home 图标（禁用态，纯位置指示）
function topBackGo() { if (state.hist.length) goBack(); }

/* ================= 重复曲目检测（同名+主艺人+时长≤2s 聚类） ================= */
let DUPS = null;
async function loadDups() {
  try { DUPS = await invoke('dup_scan'); } catch { DUPS = []; }
  cntDup.textContent = DUPS.length || '';
  if (state.view === 'dups') render();
}
function vDups() {
  if (DUPS == null) { requestAnimationFrame(loadDups); return '<div class="empty">扫描中…</div>'; }
  if (!DUPS.length) return `<div class="h-row"><h1>重复检测</h1></div>
    <div class="empty"><b>没有检测到重复曲目</b><br><span class="muted">判重规则：同名（忽略括号标注）+ 同主艺人 + 时长差 ≤2 秒</span></div>`;
  return `<div class="h-row"><h1>重复检测</h1><span class="sub">${DUPS.length} 组</span></div>
  <p class="muted" style="font-size:12px;margin:0 0 14px">同名（忽略括号标注）+ 同主艺人 + 时长差 ≤2 秒视为重复；点行播放，行尾「藏」可隐藏不需要的版本</p>
  ${DUPS.map((g, gi) => `
  <div class="set-card" style="padding:14px 18px;margin-bottom:12px">
    <div style="display:flex;align-items:center;gap:10px;margin-bottom:8px">
      <b style="font-size:14px">${esc(g.title)}</b>
      <span class="muted" style="font-size:12px">${esc(g.artist)}</span>
      <span class="cnt">${g.items.length} 个版本</span>
    </div>
    <div class="tlist">${g.items.map(t => trow(trackById(t.id) || t, { path: true })).join('')}</div>
  </div>`).join('')}`;
}

/* ================= 智能歌单（设计文档 6.2：运行时视图，非 playlists 实体） ================= */
const SMART_META = {
  favorites: { title: '收藏', sub: '心形标记的曲目（运行时视图，不占歌单实体）' },
  recent: { title: '最近播放', sub: '按最后播放时间排序 · 来自 play_history' },
  top: { title: '最常播放', sub: '按播放次数排序 · 来自 play_history' },
};
async function loadSmart() {
  SMART_TRACKS = await invoke('smart_list', { kind: state.smartKind });
  if (state.view === 'smart') render();
  renderSide();
}
function vSmart() {
  const m = SMART_META[state.smartKind] || SMART_META.favorites;
  const ts = SMART_TRACKS;
  return `<div class="h-row"><h1>${m.title}</h1><span class="sub">${ts.length} 首 · ${m.sub}</span>
    <span class="grow" style="flex:1"></span>
    ${ts.length ? '<button class="btn pri" onclick="playSmart()">▶ 播放全部</button>' : ''}</div>
  <div class="tlist"><div class="tlist-h"><span>#</span><span>标题</span><span>专辑</span><span>时长</span></div>
  ${ts.length ? ts.map((t, k) => trow(t, { no: k + 1 })).join('')
   : `<div class="empty">还没有内容<br><span class="muted">${state.smartKind === 'favorites' ? '曲目行尾 ♥ 收藏' : '播放过的曲目会出现在这里'}</span></div>`}</div>`;
}
function playSmart() {
  const ids = SMART_TRACKS.map(t => t.id);
  if (ids.length) playList(ids, 0);
}
function render() {
  ['albums', 'artists', 'tracks', 'folders', 'stats', 'dups', 'lyricsfetch', 'enrich', 'settings'].forEach(v =>
    document.getElementById('nav-' + v).classList.toggle('on', state.view === v));
  ['favorites', 'recent', 'top'].forEach(k =>
    document.getElementById('nav-' + k).classList.toggle('on', state.view === 'smart' && state.smartKind === k));
  renderSide(); renderCrumb();
  const map = { albums: vAlbums, album: vAlbum, artists: vArtists, artist: vArtist, tracks: vTracks, folders: vFolders, stats: vStats, dups: vDups, playlist: vPlaylist, lyricsfetch: vLyricsFetch, enrich: vEnrich, smart: vSmart, settings: vSettings };
  // 设置页单页模式：记下内容区滚动位置（render 会重建 DOM），渲染后恢复——
  // 否则改任何设置项（缓存开关等触发 render）都会跳回顶部
  const keepScroll = state.view === 'settings' ? (document.getElementById('setBody')?.scrollTop ?? null) : null;
  view.innerHTML = map[state.view]();
  // 设置页分栏布局：main-inner 撑满高度供左菜单固定 / 右内容区独立滚动
  document.querySelector('.main-inner').classList.toggle('settings-mode', state.view === 'settings');
  if (state.view === 'settings') {
    if (keepScroll != null) { const b = document.getElementById('setBody'); if (b) b.scrollTop = keepScroll; }
    bindSetSpy();
    // 已隐藏列表默认折叠：仅展开过才加载（首次展开在 toggleHiddenList 里触发）
    requestAnimationFrame(() => { loadAutoBk(); if (hiddenOpen) loadHidden(); });
  }
  // 视图切换回到顶部（保留滚动位置会"落"在中部）
  const mainEl = document.getElementById('main');
  if (mainEl) mainEl.scrollTop = 0;
  // 顶栏返回按钮：有历史=返回箭头；首页=home 图标（禁用态占位，非交互）
  const tb = document.getElementById('topBack');
  if (tb) {
    const home = !state.hist.length;
    tb.classList.toggle('at-home', home);
    tb.title = home ? '首页' : '返回';
  }
  if (state.view === 'artist') loadArtistHero();
}
function onSearch(q) {
  state.search = q.trim().toLowerCase();
  const clear = document.getElementById('searchClear');
  if (clear) clear.hidden = !state.search;
  if (!['albums', 'tracks', 'artists'].includes(state.view)) go('albums'); else render();
}
function clearSearch() {
  const box = document.getElementById('searchBox');
  if (box) box.value = '';
  onSearch('');
}
// 合作艺人拆分（数据驱动）：正则硬拆（/ feat. ft.）+ 曲库已知名最长匹配（. 、 & , 等任意连接符）
let ARTIST_SET = new Set();
function rebuildArtistSet() {
  ARTIST_SET = new Set();
  const add = s => { (s || '').split(/\s*\/\s*|\s*(?:feat\.?|ft\.?)\s+/i).forEach(x => {
    const t = x.trim();
    if (!t || t.length > 40) return;
    // 点连接组合名（周杰伦.梁心颐.杨瑞代）不能整串入集合——否则 artistsOf 的 has() 捷径
    // 命中、跳过拆分逻辑，组合名原样成卡。各段入集合，逼整串走贪心拆分。
    // 段各 ≥2 字才拆：B.o.B / J.Cole 这类英文缩写艺人保留整串。
    const dotted = t.split('.').map(y => y.trim()).filter(Boolean);
    if (dotted.length > 1 && dotted.every(y => y.length >= 2)) { dotted.forEach(y => ARTIST_SET.add(y)); return; }
    ARTIST_SET.add(t);
  }); };
  L.albums.forEach(a => { add(a.artist); });
  L.tracks.forEach(t => { add(t.artist); });
}
const artistsOf = s => {
  if (!s) return [];
  const hard = s.split(/\s*\/\s*|\s*(?:feat\.?|ft\.?)\s+/i).map(x => x.trim()).filter(Boolean);
  const out = [];
  for (const p of hard) {
    if (ARTIST_SET.has(p)) { out.push(p); continue; }
    // 段中贪心找已知名（最长优先），处理「周杰伦.梁心颐.杨瑞代」这类任意连接
    let rest = p;
    let guard = 0;
    while (rest && guard++ < 8) {
      let best = '';
      for (const n of ARTIST_SET) {
        if (n.length > best.length && rest.includes(n)) best = n;
      }
      if (best) {
        out.push(best);
        rest = rest.replace(best, ' ').replace(/^[.\s,&、;；+×·与]+|[.\s,&、;；+×·与]+$/g, '').trim();
      } else break;
    }
    if (!rest) continue;
    // 剩余无已知名：清连接符；若点分段各 ≥2 字则按点拆（中文人名惯例），否则整段保留
    const cleaned = rest.replace(/^[.\s,&、;；+×·与]+|[.\s,&、;；+×·与]+$/g, '').trim();
    if (!cleaned) continue;
    const dotted = cleaned.split('.').map(x => x.trim()).filter(Boolean);
    if (dotted.length > 1 && dotted.every(x => x.length >= 2)) out.push(...dotted);
    else out.push(cleaned);
  }
  return [...new Set(out.filter(Boolean))];
};
// 合作艺人显示：拆分后 ≤2 位全显示，>2 位「前两位 + 等N位」
const fmtArtists = s => {
  const a = artistsOf(s);
  if (a.length <= 2) return a.join('、');
  return a.slice(0, 2).join('、') + ' 等' + a.length + ' 位';
};
const hit = t => !state.search || t.title.toLowerCase().includes(state.search) || t.artist.toLowerCase().includes(state.search) || t.album.toLowerCase().includes(state.search);

/* ================= 视图 ================= */
function vAlbums() {
  // 拼音序（localeCompare zh）：中文按拼音、英文按字母，混合自然
  const list = L.albums
    .filter(a => !state.search || a.album.toLowerCase().includes(state.search) || a.artist.toLowerCase().includes(state.search))
    .sort((a, b) => a.artist.localeCompare(b.artist, 'zh-CN') || a.album.localeCompare(b.album, 'zh-CN'));
  if (!L.albums.length) return `<div class="empty"><b>曲库还是空的</b><br>点击侧栏「添加音乐目录」选择你的音乐文件夹<br><span class="muted">支持 MP3 / FLAC / WAV / OGG / M4A / AAC / Opus</span><br><button class="btn pri" style="margin-top:14px" onclick="doScan()">添加音乐目录</button></div>`;
  // 搜索时：歌曲命中置顶（可点播，上下文=过滤集），其后才是专辑卡片
  let songs = '';
  if (state.search) {
    const ts = L.tracks.filter(hit);
    if (ts.length) {
      songs = `<div class="nav-h" style="padding-left:0">歌曲（${ts.length}）</div>
      <div class="tlist">${ts.slice(0, 20).map((t, k) => trow(t, { no: k + 1 })).join('')}
      ${ts.length > 20 ? `<div class="empty" style="padding:12px">仅显示前 20 首 · <span style="color:var(--accent);cursor:pointer" onclick="go('tracks')">查看全部</span></div>` : ''}</div>
      <div class="nav-h" style="padding-left:0;margin-top:20px">专辑</div>`;
    } else {
      songs = `<div class="nav-h" style="padding-left:0">专辑</div>`;
    }
  }
  return `<div class="h-row"><h1>${state.search ? '搜索' : '专辑'}</h1><span class="sub">${state.search ? `${list.length} 张专辑` : list.length + ' 张专辑'}</span></div>
  ${songs}
  <div class="grid">${list.map(a => `<div class="album-card" data-key="${enc(a.album_key)}" onclick="go('album','${enc(a.album_key)}')"><div class="cv-wrap">${cover(a.cover, a.album, 150)}
    <div class="img-ovl" onclick="event.stopPropagation();coverSourceDialog('album','${enc(a.album_key)}')">获取</div></div>
    <div class="album-meta"><div class="t">${esc(a.album)}</div>
    <div class="a">${esc(fmtArtists(a.artist))}${a.year ? ' · ' + a.year : ''}</div>
    <div class="a">${sbadge(a.source)} · ${a.track_count} 首</div></div></div>`).join('')}</div>`;
}
function albumTracks(key) {
  return L.tracks.filter(t => t.album_key === key)
    .sort((a, b) => a.disc - b.disc || (a.track_no || 999) - (b.track_no || 999) || a.id - b.id);
}
function vAlbum() {
  const a = L.albums.find(x => x.album_key === state.albumKey);
  if (!a) return '<div class="empty">专辑不存在</div>';
  const ts = albumTracks(a.album_key);
  const discs = [...new Set(ts.map(t => t.disc))];
  return `<div class="detail-head" data-key="${enc(a.album_key)}"><div class="cv-wrap">${cover(a.cover, a.album, 184)}<div class="img-ovl" onclick="coverSourceDialog('album','${enc(a.album_key)}')">设置封面</div></div>
   <div style="flex:1;min-width:0;padding-bottom:4px">
    <div class="kicker">专辑${discs.length > 1 ? ` · ${discs.length} CD` : ''}</div>
    <h1 style="display:flex;align-items:center;gap:10px">${esc(a.album)}</h1>
    <div class="meta"><b>${esc(fmtArtists(a.artist))}</b>${a.year ? ' · ' + a.year : ''} · ${ts.length} 首 · ${fmt(a.duration)} ${sbadge(a.source)}</div>
    <div class="acts">
      <button class="btn pri" onclick="playAlbumNow('${enc(a.album_key)}')">▶ 播放全部</button>
      <button class="btn" onclick="fetchAlbumLyrics('${enc(a.album_key)}')">获取本专辑歌词（LRCLIB）</button>
    </div></div></div>
  <div class="tlist"><div class="tlist-h"><span>#</span><span>标题</span><span>排序依据</span><span>时长</span></div>
  ${ts.map((t, i) => {
    const pd = i ? ts[i - 1].disc : null;
    // 单碟专辑不显示 DISC 分隔头；多碟时曲目号也不再带碟前缀
    const single = discs.length <= 1;
    return (t.disc !== pd && !single ? `<div class="disc-h">── DISC ${t.disc} ──</div>` : '') +
      trow(t, { no: single ? (t.track_no || i + 1) : `${t.disc}-${String(t.track_no || i + 1).padStart(2, '0')}`, sort: true });
  }).join('')}</div>`;
}
function trow(t, opt = {}) {
  const now = state.curId === t.id;
  return `<div class="trk ${now ? 'playing' : ''}" data-tid="${t.id}" onclick="trkClick(event,${t.id})">
   <div class="no" style="cursor:pointer" title="播放">
     <span class="ply"><svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7z"/></svg></span>
     <span class="num">${opt.no ?? '·'}</span><span class="eq"><i></i><i></i><i></i></span>
   </div>
   <div class="ti"><div class="t">${esc(t.title)} ${t.source !== 'tag' ? sbadge(t.source) : ''}${t.has_lyrics ? ' <span class="badge b-online b-lyr">词</span>' : ''}${t.has_tr ? '<span class="badge b-tr">译</span>' : ''}</div></div>
   <div class="ar">${opt.path && t.file_path
      ? `<span title="${esc(t.file_path)}" style="font-family:ui-monospace,monospace;font-size:11px;color:var(--tx2)">${esc(t.file_path.split('/').slice(-2).join('/'))}</span>`
      : opt.sort ? `<span class="muted">disc ${t.disc} · track ${t.track_no || '—'}</span>` : esc(t.album)}</div>
   <div class="du"><span class="dt">${fmt(t.duration)}</span><span class="ops">
     <button class="icobtn ${t.liked ? 'liked' : ''}" title="收藏" onclick="event.stopPropagation();doLike(${t.id})">♥</button>
     <button class="icobtn" title="加入歌单" onclick="addMenu(${t.id},event)">＋</button>
     <button class="icobtn" title="${t.has_lyrics ? '查看歌词' : '在线获取歌词（LRCLIB）'}" onclick="event.stopPropagation();lyricsAction(${t.id},this)">词</button>
     <button class="icobtn" title="隐藏（cue 整轨等长音频，设置 → 已隐藏 可恢复）" onclick="event.stopPropagation();doHide(${t.id})">藏</button>
   </span></div></div>`;
}
function vArtists() {
  // 合作艺人（A/B、A feat. B）拆分聚合：每位独立艺人一张卡，曲目计入各位名下
  const stat = new Map(); // name -> {albums:Set, cnt}
  const bump = (n, albumKey, tracks) => {
    let s = stat.get(n);
    if (!s) { s = { albums: new Set(), cnt: 0 }; stat.set(n, s); }
    if (albumKey) s.albums.add(albumKey);
    s.cnt += tracks || 0;
  };
  L.tracks.forEach(t => artistsOf(t.artist).forEach(n => bump(n, t.album_key, 1)));
  L.albums.forEach(a => artistsOf(a.artist).forEach(n => bump(n, a.album_key, 0)));
  const names = [...stat.keys()].filter(n => !state.search || n.toLowerCase().includes(state.search))
    .sort((a, b) => (stat.get(b)?.cnt || 0) - (stat.get(a)?.cnt || 0));
  return `<div class="h-row"><h1>艺术家</h1><span class="sub">${names.length} 位</span></div>
  <div class="grid artist-grid">${names.map(n => {
    const st = stat.get(n) || { albums: new Set(), cnt: 0 };
    const img = state.artistImgs[n];
    const av = img
      ? `<img class="avatar big" src="${artistImgSrc(img)}" loading="lazy" alt="" onerror="this.style.display='none';this.nextElementSibling.style.display='grid'"><div class="avatar big" style="${cstyle(n)};display:none">${n.slice(0, 2)}</div>`
      : `<div class="avatar big" style="${cstyle(n)}">${n.slice(0, 2)}</div>`;
    return `<div class="artist-card" onclick="go('artist','${enc(n)}')">
      <div class="cv-wrap round">${av}<div class="img-ovl" onclick="event.stopPropagation();coverSourceDialog('artist','${enc(n)}')">获取</div></div>
      <div class="nm">${esc(n)}</div><div class="ct">${st.albums.size} 张专辑 · ${st.cnt} 首</div></div>`;
  }).join('')}</div>`;
}
function vArtist() {
  const n = state.artist;
  // 合作曲目匹配：艺人名出现在拆分列表中即归属（The Weeknd feat. 曲目计入 The Weeknd 页）
  const als = L.albums.filter(a => a.artist === n || artistsOf(a.artist).includes(n));
  const top = L.tracks.filter(t => artistsOf(t.artist).includes(n)).slice(0, 6);
  return `<div id="artistHero" data-artist="${esc(n)}" class="artist-hero">
    <div class="cv-wrap round"><div class="avatar big" style="${cstyle(n)}">${n.slice(0, 2)}</div><div class="img-ovl" onclick="coverSourceDialog('artist','${enc(n)}')">设置头像</div></div>
    <div class="ah-body"><h1>${esc(n)}</h1>
    <div class="ah-meta">${als.length} 张专辑 · ${top.length} 首热门</div>
    <div class="ah-info" id="ahInfo"></div></div></div>
  ${top.length ? `<div class="nav-h" style="padding-left:0;margin-top:20px">曲目</div>
  <div class="tlist"><div class="tlist-h"><span>#</span><span>标题</span><span>专辑</span><span>时长</span></div>
  ${top.map((t, k) => trow(t, { no: k + 1 })).join('')}</div>` : ''}
  <div class="nav-h" style="padding-left:0;margin-top:22px">专辑</div>
  <div class="grid">${als.map(a => `<div class="album-card" data-key="${enc(a.album_key)}" onclick="go('album','${enc(a.album_key)}')"><div class="cv-wrap">${cover(a.cover, a.album, 150)}
    <div class="img-ovl" onclick="event.stopPropagation();coverSourceDialog('album','${enc(a.album_key)}')">获取</div></div>
    <div class="album-meta"><div class="t">${esc(a.album)}</div><div class="a">${a.year || '—'} · ${a.track_count} 首</div></div></div>`).join('')}</div>`;
}
// 在线获取面板：各渠道结果分区展示，用户自由选择采用
async function fetchArtistPack(nEnc, btn) {
  const n = decodeURIComponent(nEnc);
  if (btn) { btn.disabled = true; const o = btn.textContent; btn.textContent = '获取中…'; btn.dataset.old = o; }
  // 立即弹窗 + loading（多源并发搜索需数秒）
  modal('在线获取 · ' + n, `<div class="lyr-loading"><span class="spin"></span>正在并行搜索：本地 / 自定义源 / Wikipedia / MusicBrainz / Deezer…</div>`, '', true);
  let v;
  try { v = await invoke('fetch_artist_pack', { name: n, baseUrl: trSrcUrl() || null }); }
  catch (e) { closeModal(); toast(String(e), 'err'); if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '在线获取'; } return; }
  if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '在线获取'; }
  window.__artistPack = { name: n, data: v };
  window.__artistPack = { name: n, data: v, selImg: -1 };
  const h = [];
  if (v.imgs && v.imgs.length) {
    h.push(`<div id="avPreview" class="cv-preview" style="border-radius:50%;width:170px;height:170px"><span class="muted" style="font-size:12px">点击图片选中并预览</span></div>
    <div class="pack-src">歌手图片 <span class="pg-cnt">${v.imgs.length} 张</span></div>
    <div class="cover-grid av-grid">${v.imgs.map((x, i) => `
      <div class="av-pick"><img src="${artistImgSrc(x.url)}" loading="lazy" onload="this.dataset.dim=this.naturalWidth+'×'+this.naturalHeight;this.closest('.av-pick').querySelector('.src-tag').textContent='${esc(x.src)} · '+this.dataset.dim" onclick="pickArtistImg(${i},this)"><span class="src-tag">${esc(x.src)}</span></div>`).join('')}</div>`);
  } else {
    h.push('<div class="pack-src">歌手图片</div><p class="muted">各渠道均无结果（fanart 需配置 Key）</p>');
  }
  if (v.wiki) {
    h.push(`<div class="pack-src">简介 <span class="src-tag">Wikipedia</span></div>
    <div class="pack-text">${esc(v.wiki)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('wiki')">采用此简介</button>`);
  }
  if (v.mb) {
    h.push(`<div class="pack-src">信息 <span class="src-tag">MusicBrainz</span></div>
    <div class="pack-text">${esc(v.mb)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('meta')">采用此信息</button>`);
  }
  if (v.custom && v.custom.bio) {
    h.push(`<div class="pack-src">简介 <span class="src-tag">自定义源</span></div>
    <div class="pack-text">${esc(v.custom.bio)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('wiki_custom')">采用此简介</button>`);
  }
  if (v.adb && v.adb.bio) {
    h.push(`<div class="pack-src">简介 <span class="src-tag">TheAudioDB${v.adb.bio_is_cn ? ' · 中文' : ''}</span></div>
    <div class="pack-text">${esc(v.adb.bio)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('wiki_adb')">采用此简介</button>`);
  }
  if (v.adb && v.adb.meta) {
    h.push(`<div class="pack-src">信息 <span class="src-tag">TheAudioDB</span></div>
    <div class="pack-text">${esc(v.adb.meta)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('meta_adb')">采用此信息</button>`);
  }
  if (v.lfm && v.lfm.bio) {
    h.push(`<div class="pack-src">简介 <span class="src-tag">Last.fm</span></div>
    <div class="pack-text">${esc(v.lfm.bio)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('wiki_lfm')">采用此简介</button>`);
  }
  if (v.lfm && v.lfm.meta) {
    h.push(`<div class="pack-src">信息 <span class="src-tag">Last.fm</span></div>
    <div class="pack-text">${esc(v.lfm.meta)}</div>
    <button class="btn sm" style="margin-top:8px" onclick="applyPackField('meta_lfm')">采用此信息</button>`);
  }
  const hasImgs = v.imgs && v.imgs.length;
  modal('在线获取 · ' + n, h.join('') || '<p class="muted">所有渠道均无结果</p>',
    `${hasImgs ? `<button class="btn pri" id="avApply" disabled onclick="applyArtistImgConfirm()">确认应用头像</button>` : ''}<button class="btn" onclick="closeModal()">关闭</button>`, true);
}
function pickArtistImg(i, el) {
  const pk = window.__artistPack;
  if (!pk) return;
  pk.selImg = i;
  document.querySelectorAll('.av-pick img').forEach(x => x.style.outline = '');
  if (el) el.style.outline = '2px solid var(--accent)';
  const pv = document.getElementById('avPreview');
  if (pv) pv.innerHTML = `<img src="${artistImgSrc(pk.data.imgs[i].url)}" style="width:100%;height:100%;object-fit:cover;border-radius:50%">`;
  const btn = document.getElementById('avApply');
  if (btn) btn.disabled = false;
}
async function applyArtistImgConfirm() {
  const pk = window.__artistPack;
  if (!pk || pk.selImg < 0) return;
  const btn = document.getElementById('avApply');
  if (btn) { btn.disabled = true; btn.textContent = '应用中…'; }
  const url = pk.data.imgs[pk.selImg].url;
  try {
    await invoke('set_artist_field', { name: pk.name, field: 'img', value: url });
    state.artistImgs[pk.name] = url;
    closeModal();
    // 局部刷新：直接替换 hero 头像（硬更新，不依赖缓存回读；回读链路若有静默失败也不影响视觉）
    const heroAv = document.querySelector('#artistHero .avatar');
    if (heroAv) heroAv.outerHTML = `<img class="avatar big" src="${artistImgSrc(url)}" alt="">`;
    // 艺术家列表卡片（若当前视图有同名卡片）：头像原地换
    document.querySelectorAll('.artist-card').forEach(card => {
      if (card.querySelector('.nm')?.textContent === pk.name) {
        const old = card.querySelector('.avatar');
        if (old && old.tagName === 'DIV') old.outerHTML = `<img class="avatar big" src="${artistImgSrc(url)}" loading="lazy">`;
        else if (old) old.src = artistImgSrc(url);
      }
    });
    loadArtistHero(); // 简介等其余区域仍走原逻辑
    toast('头像已应用', 'ok');
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = '确认应用头像'; }
  }
}
async function applyPackField(field, i, img) {
  const pk = window.__artistPack;
  if (!pk) return;
  let value = '';
  if (field === 'img') value = pk.data.imgs[i].url;
  else if (field === 'wiki') value = pk.data.wiki || '';
  else if (field === 'wiki_lfm') value = (pk.data.lfm && pk.data.lfm.bio) || '';
  else if (field === 'meta_lfm') value = (pk.data.lfm && pk.data.lfm.meta) || '';
  else if (field === 'wiki_adb') value = (pk.data.adb && pk.data.adb.bio) || '';
  else if (field === 'wiki_custom') value = (pk.data.custom && pk.data.custom.bio) || '';
  else if (field === 'meta_adb') value = (pk.data.adb && pk.data.adb.meta) || '';
  else value = pk.data.mb || '';
  const f = field.startsWith('wiki') ? 'wiki' : field.startsWith('meta') ? 'meta' : field;
  const btn = img ? null : event && event.target && event.target.closest('button');
  if (img) img.style.outline = '2px solid var(--accent)';
  if (btn) { btn.disabled = true; const o = btn.textContent; btn.textContent = '应用中…'; btn.dataset.old = o; }
  try {
    await invoke('set_artist_field', { name: pk.name, field: f, value });
    if (field === 'img') { state.artistImgs[pk.name] = value; render(); loadArtistHero(); }
    else { loadArtistHero(); }
    toast('已应用', 'ok');
  } catch (e) {
    if (img) img.style.outline = '';
    if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '采用'; }
    toast(String(e), 'err');
  }
}
// 歌手卡异步加载：读缓存填充头像与简介
async function loadArtistHero() {
  const hero = document.getElementById('artistHero');
  if (!hero) return;
  const n = hero.dataset.artist;
  try {
    const v = await invoke('artist_info', { name: n });
    if (v.img) state.artistImgs[n] = v.img;
    const avatar = hero.querySelector('.avatar');
    const heroImg = state.artistImgs[n] || v.img; // 本地约定图（artist.jpg）优先
    if (heroImg) { avatar.outerHTML = `<img class="avatar big" src="${artistImgSrc(heroImg)}" alt="" onerror="this.outerHTML='<div class=\\'avatar big\\' style=\\'${cstyle(n)}\\'>${esc(n.slice(0, 2))}</div>'">`; }
    const info = document.getElementById('ahInfo');
    if (info) {
      info.innerHTML = `${v.meta ? `<div class="ah-mb">${esc(v.meta)}</div>` : ''}${v.wiki ? `<div class="ah-wiki" id="ahWiki" onclick="toggleWiki()">${esc(v.wiki)}</div><button class="wiki-toggle" id="wikiTg" style="display:none" onclick="toggleWiki()">展开 ▾</button>` : ''}`;
      if (!v.meta && !v.wiki) info.innerHTML = '<div class="ah-wiki muted">暂无歌手信息（Deezer / 维基 / MusicBrainz 均无结果）</div>';
      // 简介超 3 行折叠：显示展开按钮
      requestAnimationFrame(() => {
        const w = document.getElementById('ahWiki');
        const tg = document.getElementById('wikiTg');
        if (w && tg && w.scrollHeight > w.clientHeight + 4) tg.style.display = '';
      });
    }
  } catch { /* 静默：缓存/下次再试 */ }
}
function vTracks() {
  const { key, dir } = state.tracksSort;
  const fg = state.trackFilter || {};
  // 过滤：搜索词 + 流派 + 年代（AND）
  const rows = L.tracks.filter(t => hit(t)
    && (!fg.genre || (t.genre || '') === fg.genre)
    && (!fg.decade || (t.year && Math.floor(t.year / 10) * 10 === +fg.decade))).slice();
  let hint = '';
  if (key === 'dur') {
    rows.sort((a, b) => (a.duration - b.duration) * dir);
    hint = ` · 按时长${dir > 0 ? '↑' : '↓'}`;
  } else if (key === 'title') {
    rows.sort((a, b) => a.title.localeCompare(b.title, 'zh-CN') * dir);
    hint = ` · 按标题${dir > 0 ? '↑' : '↓'}`;
  } else if (key === 'plays') {
    rows.sort((a, b) => ((a.plays || 0) - (b.plays || 0)) * dir || b.duration - a.duration);
    hint = ` · 按播放次数${dir > 0 ? '↑' : '↓'}`;
  } else if (key === 'added') {
    rows.sort((a, b) => (a.id - b.id) * dir);
    hint = ` · 按添加时间${dir > 0 ? '↑' : '↓'}`;
  }
  const th = (k, label) => key === k
    ? `<span class="th-sort on" onclick="toggleTracksSort('${k}')" title="切换${label}排序">${label} ${dir > 0 ? '↑' : '↓'}</span>`
    : `<span class="th-sort" onclick="toggleTracksSort('${k}')" title="切换${label}排序">${label} ⇅</span>`;
  // 筛选条：流派 / 年代（选项从曲库实时聚合）+ 排序下拉（与表头三态共用状态机）
  const genres = [...new Set(L.tracks.map(t => t.genre).filter(Boolean))].sort((a, b) => a.localeCompare(b, 'zh-CN'));
  const decades = [...new Set(L.tracks.map(t => t.year && t.year > 1900 ? Math.floor(t.year / 10) * 10 : null).filter(Boolean))].sort();
  const sel = (opts, val, fn, lab) => `<select onchange="${fn}" style="padding:6px 10px;border-radius:8px;border:1px solid var(--line2);background:var(--bg2);color:var(--tx0);font-size:12px;outline:none">
    <option value="">${lab}</option>${opts.map(o => `<option value="${o}" ${String(val) === String(o) ? 'selected' : ''}>${typeof o === 'number' ? o + 's' : esc(o)}</option>`).join('')}</select>`;
  const sortSel = `<select onchange="setTracksSortKey(this.value)" style="padding:6px 10px;border-radius:8px;border:1px solid var(--line2);background:var(--bg2);color:var(--tx0);font-size:12px;outline:none">
    ${[['', '默认排序'], ['title', '标题'], ['dur', '时长'], ['plays', '播放次数'], ['added', '添加时间']].map(([v, l]) => `<option value="${v}" ${key === v || (!v && !key) ? 'selected' : ''}>${l}</option>`).join('')}</select>
    <button class="btn sm" onclick="toggleTracksDir()" title="切换方向" ${key ? '' : 'disabled'}>${dir > 0 ? '↑' : '↓'}</button>`;
  return `<div class="h-row"><h1>曲目</h1><span class="sub">${rows.length} 首${state.search ? ' · 过滤中' : ''}${hint}</span></div>
  <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin-bottom:14px">
    ${sel(genres, fg.genre, 'setTrackFilter("genre",this.value)', '全部流派')}
    ${sel(decades, fg.decade, 'setTrackFilter("decade",this.value)', '全部年代')}
    <span style="flex:1"></span>${sortSel}
  </div>
  <div class="tlist"><div class="tlist-h"><span>#</span>${th('title', '标题')}<span>专辑</span>${th('dur', '时长')}</div>
  ${rows.map((t, k) => trow(t, { no: k + 1 })).join('') || '<div class="empty">无匹配</div>'}</div>`;
}
function setTrackFilter(k, v) { state.trackFilter = { ...(state.trackFilter || {}), [k]: v }; render(); }
function setTracksSortKey(k) { state.tracksSort = { key: k || null, dir: 1 }; render(); }
function toggleTracksDir() { state.tracksSort.dir *= -1; render(); }
// 三态循环：默认 → 升序 → 降序 → 默认；点另一列直接从该列升序开始
function toggleTracksSort(key) {
  const s = state.tracksSort;
  if (s.key !== key) { s.key = key; s.dir = 1; }
  else if (s.dir > 0) s.dir = -1;
  else { s.key = null; s.dir = 1; }
  render();
}
async function doHide(tid) {
  try {
    await invoke('set_track_hidden', { trackId: tid, hidden: true });
    toast('已隐藏（设置 → 已隐藏 可恢复）', 'info');
    await refresh();
    if (state.view === 'dups') loadDups(); // 隐藏版本后重新分组（组可能随之消失）
  } catch (e) { toast(String(e), 'err'); }
}
async function doUnhide(tid) {
  try {
    await invoke('set_track_hidden', { trackId: tid, hidden: false });
    toast('已取消隐藏', 'ok');
    await refresh();
    if (state.view === 'settings') loadHidden();
  } catch (e) { toast(String(e), 'err'); }
}
function playHidden(tid) { playList([tid], 0); }
async function loadHidden() {
  const el = document.getElementById('hiddenList');
  if (!el) return;
  let list = [];
  try { list = await invoke('hidden_tracks'); } catch { }
  HID_N = list.length;
  const cnt = document.getElementById('hidCount');
  if (cnt) cnt.textContent = HID_N;
  if (!list.length) { el.innerHTML = '<p class="desc">没有隐藏的曲目</p>'; return; }
  el.innerHTML = `<div class="tlist"><div class="tlist-h"><span>#</span><span>标题</span><span>专辑</span><span>时长</span></div>
    ${list.map((t, i) => `<div class="trk" title="点击试听" onclick="playHidden(${t.id})">
      <div class="no"><span class="num">${i + 1}</span></div>
      <div class="ti"><div class="t">${esc(t.title)}</div></div>
      <div class="ar">${esc(t.album || '—')}</div>
      <div class="du"><span class="dt">${fmt(t.duration)}</span><span class="ops">
        <button class="icobtn" title="取消隐藏，重新出现在所有列表" onclick="event.stopPropagation();doUnhide(${t.id})">恢复</button>
      </span></div></div>`).join('')}</div>`;
}
/* 已隐藏列表折叠展开（默认收起，展开时加载） */
let hiddenOpen = false, HID_N = null;
function toggleHiddenList() {
  hiddenOpen = !hiddenOpen;
  const list = document.getElementById('hiddenList');
  if (list) list.style.display = hiddenOpen ? '' : 'none';
  const tg = document.querySelector('#hidHead .abk-tg');
  if (tg) tg.textContent = hiddenOpen ? '收起 ▴' : '展开 ▾';
  if (hiddenOpen && list && !list.dataset.loaded) { list.dataset.loaded = '1'; loadHidden(); }
}
function vPlaylist() {
  const p = L.playlists.find(x => x.id === state.playlistId);
  if (!p) return '<div class="empty">歌单不存在</div>';
  return `<div class="detail-head">
   <div class="cv-wrap"><div class="collage" id="plCollage"></div><div class="img-ovl" onclick="coverSourceDialog('playlist','${p.id}')">设置封面</div></div>
   <div style="flex:1;min-width:0;padding-bottom:4px">
    <div class="kicker">歌单</div><h1 style="display:flex;align-items:center;gap:10px">${esc(p.name)}</h1>
    <div class="meta" id="plMeta">加载中…</div>
    <div class="acts">
      <button class="btn pri" onclick="playPl(${p.id})">▶ 播放全部</button>
      <button class="btn" onclick="exportM3U(${p.id})"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="vertical-align:-2px;margin-right:4px"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/></svg>导出 M3U8</button>
      <button class="btn" id="btnPlEdit" onclick="togglePlEdit()" ${p.count ? '' : 'disabled'}>${plEditBtnHtml(false)}</button>
    </div></div></div>
  <div class="tlist" id="plTracks"><div class="empty">加载中…</div></div>`;
}
async function loadPlaylistView() {
  const p = L.playlists.find(x => x.id === state.playlistId);
  if (!p || state.view !== 'playlist') return;
  const ts = await invoke('playlist_detail', { pid: p.id });
  p._tracks = ts;
  if (p.cover) {
    // 自定义封面：单图铺满整个拼贴框
    plCollage.innerHTML = `<div class="cell" style="grid-column:1/3;grid-row:1/3"><img src="${convertFileSrc(p.cover)}"></div>`;
  } else {
    const covers = ts.slice(0, 4);
    plCollage.innerHTML = Array.from({ length: 4 }, (_, i) => {
      const t = covers[i];
      return `<div class="cell">${t && t.cover ? `<img src="${convertFileSrc(t.cover)}">` : (t ? esc(t.album.slice(0, 2)) : '')}</div>`;
    }).join('');
  }
  plMeta.textContent = `${ts.length} 首 · ${fmt(ts.reduce((n, t) => n + t.duration, 0))}${p.cover ? ' · 自定义封面' : ' · 前 4 首封面拼贴'}${plEditing ? ' · 编辑模式（拖动排序 · ✕ 移出）' : ''}`;
  // 编辑模式：拖拽排序 + 移出；普通模式与专辑页一致（trow：点击播放 + 行尾操作按钮）
  const grip = '<svg width="14" height="14" viewBox="0 0 24 24" fill="var(--tx2)"><circle cx="9" cy="6" r="1.6"/><circle cx="15" cy="6" r="1.6"/><circle cx="9" cy="12" r="1.6"/><circle cx="15" cy="12" r="1.6"/><circle cx="9" cy="18" r="1.6"/><circle cx="15" cy="18" r="1.6"/></svg>';
  plTracks.innerHTML = `<div class="tlist-h"><span>#</span><span>标题${plEditing ? '（拖动排序）' : ''}</span><span>艺人</span><span>时长</span></div>` +
    (ts.length ? ts.map((t, k) => plEditing
      ? `<div class="trk pl-sort-row" draggable="true" data-idx="${k}"
          ondragstart="plDragStart(event,${k})" ondragover="plDragOver(event,${k})" ondrop="plDrop(event,${k})" ondragend="plDragEnd()">
          <div class="no">${grip}</div>
          <div class="ti"><div class="t">${esc(t.title)}</div></div><div class="ar">${esc(t.artist)}</div>
          <div class="du"><span class="dt">${fmt(t.duration)}</span><span class="ops">
           <button class="icobtn" title="移出歌单" onclick="removePlTrack(${k})">✕</button></span></div></div>`
      : trow(t, { no: k + 1 })).join('')
    : '<div class="empty">空歌单 · 去曲目列表行尾 ＋ 加入</div>');
}
/* ---- 歌单编辑模式：拖拽排序（HTML5 DnD）+ 移出歌曲 ---- */
let plEditing = false, plDragFrom = -1;
// 编辑/完成按钮（lucide 线条图标，与播放器其他 SVG 按钮同风格）
const plEditBtnHtml = done => done
  ? `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="vertical-align:-2px;margin-right:4px"><polyline points="20 6 9 17 4 12"/></svg>完成`
  : `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="vertical-align:-2px;margin-right:4px"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/></svg>编辑歌单`;
function togglePlEdit() {
  plEditing = !plEditing;
  const btn = document.getElementById('btnPlEdit');
  if (btn) btn.innerHTML = plEditBtnHtml(plEditing);
  loadPlaylistView();
}
function plDragStart(ev, k) {
  plDragFrom = k;
  ev.dataTransfer.effectAllowed = 'move';
  try { ev.dataTransfer.setData('text/plain', String(k)); } catch {}
  ev.target.closest('.pl-sort-row')?.classList.add('dragging');
}
function plDragOver(ev, k) {
  ev.preventDefault();
  ev.dataTransfer.dropEffect = 'move';
  document.querySelectorAll('.pl-sort-row.drag-over').forEach(x => x.classList.remove('drag-over'));
  const row = ev.target.closest('.pl-sort-row');
  if (row && k !== plDragFrom) row.classList.add('drag-over');
}
function plDrop(ev, k) {
  ev.preventDefault();
  const from = plDragFrom;
  plDragFrom = -1;
  if (from < 0 || from === k) return plDragEnd();
  invoke('move_playlist_track', { pid: state.playlistId, from, to: k }).then(loadPlaylistView);
}
function plDragEnd() {
  document.querySelectorAll('.pl-sort-row.dragging, .pl-sort-row.drag-over').forEach(x => x.classList.remove('dragging', 'drag-over'));
}
function removePlTrack(k) {
  invoke('remove_playlist_track', { pid: state.playlistId, idx: k }).then(() => { loadPlaylistView(); renderSide(); });
}
async function playPl(pid) {
  const ts = await invoke('playlist_detail', { pid });
  if (!ts.length) return toast('歌单为空', 'info');
  playList(ts.map(t => t.id), 0);
}
async function exportM3U(pid) {
  try {
    const msg = await invoke('export_playlist_m3u', { pid });
    toast(msg, 'ok');
  } catch (e) { if (String(e) !== '未选择位置') toast(String(e), 'err'); }
}

async function doLike(tid) {
  const liked = await invoke('toggle_like', { trackId: tid });
  const t = trackById(tid);
  if (t) t.liked = liked;
  render();
  if (state.view === 'smart' && state.smartKind === 'favorites') loadSmart();
  toast(liked ? '已收藏 ♥' : '已取消收藏', liked ? 'ok' : 'info');
}

/* ================= 文件夹视图（树状导航：根 → 子目录 → 直属曲目） ================= */
const dirOf = p => p.slice(0, p.lastIndexOf('/'));
function vFolders() {
  const cur = state.folder; // null=顶层（根列表）
  if (cur == null) {
    const rows = SCAN_ROOTS.map(r => ({
      dir: r,
      count: L.tracks.filter(t => t.file_path.startsWith(r + '/')).length,
    })).filter(r => r.count > 0);
    return `<div class="h-row"><h1>文件夹</h1><span class="sub">${SCAN_ROOTS.length} 个音乐根目录</span></div>
    <div class="tlist" style="padding:8px">${rows.length ? rows.map(r => folderRow(r.dir, r.count)).join('')
      : '<div class="empty">未配置音乐目录 · 侧栏「添加音乐目录」</div>'}</div>`;
  }
  const prefix = cur + '/';
  const subdirs = new Map();
  const own = [];
  for (const t of L.tracks) {
    const d = dirOf(t.file_path);
    if (d === cur) own.push(t);
    else if (d.startsWith(prefix)) {
      const name = d.slice(prefix.length).split('/')[0];
      subdirs.set(name, (subdirs.get(name) || 0) + 1);
    }
  }
  own.sort((a, b) => (a.track_no || 999) - (b.track_no || 999) || a.title.localeCompare(b.title));
  const subs = [...subdirs.entries()].sort((a, b) => a[0].localeCompare(b[0], 'zh-Hans-CN'));
  // 面包屑
  const crumbs = [];
  if (SCAN_ROOTS.includes(cur)) {
    crumbs.push({ name: cur.split('/').pop(), dir: cur });
  } else {
    const root = SCAN_ROOTS.find(r => cur.startsWith(r + '/'));
    if (root) {
      crumbs.push({ name: root.split('/').pop(), dir: root });
      let rest = cur.slice(root.length + 1).split('/');
      rest.forEach((seg, i) => {
        if (seg) crumbs.push({ name: seg, dir: root + '/' + rest.slice(0, i + 1).join('/') });
      });
    } else {
      crumbs.push({ name: cur.split('/').pop(), dir: cur });
    }
  }
  const total = own.length + [...subdirs.values()].reduce((a, b) => a + b, 0);
  // 歌曲 / 子文件夹双 tab（两者都有时；默认歌曲）。tab 状态跨目录保持——目录树浏览时不必反复切
  const both = own.length > 0 && subs.length > 0;
  const tab = state.folderTab || 'tracks';
  // 头部工具行：返回 + 面包屑导航（父级可点、当前段高亮）+ 右侧操作
  return `<div class="fd-head">
    <button class="btn sm" onclick="upFolder()" title="上一级">←</button>
    <div class="fd-crumbs">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--tx2)" stroke-width="2" style="flex:none"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>
      ${crumbs.map((c, i) => `<span class="seg ${i === crumbs.length - 1 ? 'cur' : ''}" onclick="enterFolder('${enc(c.dir)}')">${esc(c.name)}</span>${i < crumbs.length - 1 ? '<span class="sep">/</span>' : ''}`).join('')}
      <span class="sub" style="margin-left:8px">${total} 首${subs.length ? ` · ${subs.length} 个子文件夹` : ''}</span>
    </div>
    <span style="flex:1"></span>
    ${own.length ? '<button class="btn sm pri" onclick="playFolderOwn()">▶ 播放</button>' : ''}
  </div>
  ${both ? `<div class="q-tabs" style="margin-bottom:14px">
    <div class="q-tab ${tab === 'tracks' ? 'on' : ''}" onclick="setFolderTab('tracks')">歌曲（${own.length}）</div>
    <div class="q-tab ${tab === 'dirs' ? 'on' : ''}" onclick="setFolderTab('dirs')">子文件夹（${subs.length}）</div>
  </div>` : ''}
  ${(!both || tab === 'tracks') && own.length ? `<div class="tlist"><div class="tlist-h"><span>#</span><span>标题</span><span>专辑</span><span>时长</span></div>
    ${own.map((t, k) => trow(t, { no: k + 1 })).join('')}</div>` : ''}
  ${(!both || tab === 'dirs') && subs.length ? `<div class="tlist" style="padding:8px">
    ${subs.map(([name, n]) => folderRow(cur + '/' + name, n, name)).join('')}
  </div>` : ''}`;
}
function setFolderTab(t) { state.folderTab = t; render(); }
function folderRow(dir, count, displayName) {
  const name = displayName ?? dir.split('/').pop();
  return `<div class="artist-row" onclick="enterFolder('${enc(dir)}')">
    <div class="avatar" style="background:var(--bg3);border-radius:10px"><svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="var(--tx1)" stroke-width="2"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg></div>
    <div><div class="nm">${esc(name)}</div><div class="ct">${count} 首</div></div></div>`;
}
function enterFolder(dir) { state.folder = decodeURIComponent(dir); render(); }
function upFolder() {
  const cur = state.folder;
  if (cur == null) return;
  if (SCAN_ROOTS.includes(cur)) { state.folder = null; }
  else {
    const parent = dirOf(cur);
    state.folder = SCAN_ROOTS.some(r => parent.startsWith(r) ) ? parent : null;
  }
  render();
}
function playFolderOwn() {
  const cur = state.folder;
  if (cur == null) return;
  const own = L.tracks.filter(t => dirOf(t.file_path) === cur);
  if (own.length) playList(own.map(t => t.id), 0);
}

/* ================= A-B 段落循环（练歌/学外语；不持久化，切歌清除） ================= */
const abLoop = { a: null, b: null };
function uiAbLoop() {
  if (abLoop.a == null) {
    abLoop.a = state.t;
    btnAb.classList.add('half');
    btnAb.title = 'A-B 循环：设 B 点';
    toast(`A 点 ${fmt(state.t)}（再点设 B）`, 'info');
  } else if (abLoop.b == null) {
    if (state.t <= abLoop.a + 0.5) return toast('B 点需在 A 点之后', 'err');
    abLoop.b = state.t;
    btnAb.classList.remove('half'); btnAb.classList.add('on');
    btnAb.title = 'A-B 循环中（点击清除）';
    toast(`A-B 循环 ${fmt(abLoop.a)} – ${fmt(abLoop.b)}`, 'ok');
  } else {
    resetAbLoop();
    toast('A-B 循环已清除', 'info');
  }
  renderAbMarks();
}
function resetAbLoop() {
  abLoop.a = abLoop.b = null;
  btnAb.classList.remove('half', 'on');
  btnAb.title = 'A-B 循环：设 A 点';
  renderAbMarks();
}
// 进度条上的 A/B 竖线 + 区间高亮（pointer-events:none，不挡点击 seek）
function renderAbMarks() {
  const el = document.getElementById('abMarks');
  if (!el) return;
  if (abLoop.a == null || !state.dur) { el.innerHTML = ''; return; }
  const pct = s => Math.min(100, Math.max(0, s / state.dur * 100));
  el.innerHTML = abLoop.b == null
    ? `<div style="position:absolute;left:${pct(abLoop.a)}%;top:-2px;bottom:-2px;width:2px;background:var(--warn);border-radius:1px;pointer-events:none"></div>`
    : `<div style="position:absolute;left:${pct(abLoop.a)}%;width:${pct(abLoop.b) - pct(abLoop.a)}%;top:0;bottom:0;background:rgba(251,188,36,.18);pointer-events:none"></div>
       <div style="position:absolute;left:${pct(abLoop.a)}%;top:-2px;bottom:-2px;width:2px;background:var(--warn);border-radius:1px;pointer-events:none"></div>
       <div style="position:absolute;left:${pct(abLoop.b)}%;top:-2px;bottom:-2px;width:2px;background:var(--warn);border-radius:1px;pointer-events:none"></div>`;
}

/* ================= 播放速度 / 睡眠定时 ================= */
/// 从按钮正上方向上弹出气泡菜单
function popAbove(btn, inner) {
  menuRoot.innerHTML = `<div class="menu">${inner}</div>`;
  menuRoot.style.display = 'block';
  const m = menuRoot.firstChild;
  const r = btn.getBoundingClientRect();
  menuRoot.style.top = (r.top - m.offsetHeight - 8) + 'px';
  menuRoot.style.left = Math.min(Math.max(8, r.left + r.width / 2 - m.offsetWidth / 2), innerWidth - m.offsetWidth - 8) + 'px';
}

/* ================= 播放条功能开关（设置 → 功能，默认关闭） ================= */
const FEAT_BTN = { speed: 'btnSpeed', sleep: 'btnSleep', ab: 'btnAb' };
const featOn = k => localStorage.getItem('lyra.feat.' + k) === '1';
// 按开关显示/隐藏播放条按钮（boot 与开关切换时调用）
function applyFeatBtns() {
  for (const [k, id] of Object.entries(FEAT_BTN)) {
    const el = document.getElementById(id);
    if (el) el.style.display = featOn(k) ? '' : 'none';
  }
}
function toggleFeat(k, on) {
  localStorage.setItem('lyra.feat.' + k, on ? '1' : '0');
  applyFeatBtns();
  // 关闭时复位运行态，防功能隐身但逻辑还在跑
  if (!on) {
    if (k === 'speed' && SPEED !== 1.0) { SPEED = 1.0; btnSpeed.textContent = '1.0x'; invoke('set_speed', { v: 1.0 }); toast('倍速已重置为 1.0x', 'info'); }
    if (k === 'sleep' && sleepAt) { sleepAt = 0; btnSleep.classList.remove('on'); toast('已取消睡眠定时', 'info'); }
    if (k === 'ab' && (abLoop.a != null || abLoop.b != null)) { resetAbLoop(); toast('A-B 循环已清除', 'info'); }
  }
}

let BGPLAY = false;
async function loadBgPlay() { try { BGPLAY = await invoke('get_bgplay'); } catch { } }
async function toggleBgPlay(on) {
  try { BGPLAY = await invoke('set_bgplay', { on }); toast(on ? '后台播放已开启：关窗继续播放，点 Dock 图标恢复' : '后台播放已关闭：关窗即退出', 'info'); }
  catch (e) { toast(String(e), 'err'); }
}

let SPEED = 1.0;
let spdTimer = null;
const SPD_MARKS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3];
async function loadSpeed() { SPEED = await invoke('get_speed'); btnSpeed.textContent = fmtSpeed(SPEED); }
const fmtSpeed = v => parseFloat((+v).toFixed(2)) + 'x';
function uiSpeed() {
  popAbove(btnSpeed, `
    <div class="pop-h">播放速度<span class="sub">音调随速度变化 · 双击滑块归位</span></div>
    <div class="spd-pop">
      <div class="spd-val" id="spdVal">${fmtSpeed(SPEED)}</div>
      <input type="range" min="0.5" max="3" step="0.05" value="${SPEED}" id="spdSlider"
        oninput="spdSlide(this.value)" ondblclick="this.value=1;spdSlide(1)">
      <div class="spd-marks">
        ${SPD_MARKS.map(v => `<button class="spd-mark ${Math.abs(v - SPEED) < 0.001 ? 'on' : ''}" onclick="spdJump(${v})">${v}</button>`).join('')}
      </div>
    </div>`);
}
function spdSlide(v) {
  SPEED = parseFloat(v);
  const lab = document.getElementById('spdVal');
  if (lab) lab.textContent = fmtSpeed(SPEED);
  btnSpeed.textContent = fmtSpeed(SPEED);
  document.querySelectorAll('.spd-mark').forEach(el =>
    el.classList.toggle('on', Math.abs(parseFloat(el.textContent) - SPEED) < 0.001));
  clearTimeout(spdTimer);
  spdTimer = setTimeout(() => invoke('set_speed', { v: SPEED }), 120);
}
function spdJump(v) {
  const s = document.getElementById('spdSlider');
  if (s) s.value = v;
  spdSlide(v);
}

let sleepAt = 0;
function uiSleep() {
  const leftMs = sleepAt ? sleepAt - Date.now() : 0;
  const left = leftMs > 0 ? Math.ceil(leftMs / 60000) : 0;
  const opts = [15, 30, 45, 60, 90, 120];
  popAbove(btnSleep, `
    ${left ? `<div class="sleep-live"><span class="sl-moon">🌙</span><div><b>${left}</b> 分钟后暂停<button class="btn sm" onclick="setSleep(0)">取消</button></div></div><div class="pop-h">重新设定</div>` : '<div class="pop-h">睡眠定时<span class="sub">到点自动暂停</span></div>'}
    <div class="sleep-grid">
      ${opts.map(m => `<button class="sleep-item" onclick="setSleep(${m})"><b>${m}</b><i>分钟</i></button>`).join('')}
    </div>`);
}
function setSleep(min) {
  closeMenu();
  if (!min) { sleepAt = 0; btnSleep.classList.remove('on'); return toast('已取消睡眠定时', 'info'); }
  sleepAt = Date.now() + min * 60000;
  btnSleep.classList.add('on');
  toast(`将在 ${min} 分钟后暂停播放`, 'ok');
}

/* ================= 歌词偏移（每曲独立，localStorage） ================= */
const lyOffKey = () => 'lyra.lyOff.' + state.curId;
const lyOff = () => parseFloat(localStorage.getItem(lyOffKey()) || '0');
function renderLyOffBox() {
  const svg = p => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${p}</svg>`;
  lyOffBox.innerHTML = `
    <button onclick="nudgeLyOff(0.25)" title="歌词放慢 0.25 秒">${svg('<path d="m18 17-5-5 5-5"/><path d="m11 17-5-5 5-5"/>')}</button>
    <button onclick="nudgeLyOff(-0.25)" title="歌词加快 0.25 秒">${svg('<path d="m6 17 5-5-5-5"/><path d="m13 17 5-5-5-5"/>')}</button>
    <button onclick="fetchOneLyrics(state.curId,this)" title="获取歌词">${svg('<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>')}</button>`;
}
function nudgeLyOff(d) {
  // 0.25s 步进：按 1/4 秒取整，避免浮点累加误差（0.25/0.5/0.75…）
  const v = Math.round((lyOff() + d) * 4) / 4;
  localStorage.setItem(lyOffKey(), String(v));
  updateLyricHighlight(true);
  updateLyOffUI();
  toast(`歌词偏移 ${v > 0 ? '+' : ''}${v}s`, 'info');
}
function resetLyOff() { localStorage.removeItem(lyOffKey()); updateLyricHighlight(true); updateLyOffUI(); toast('歌词偏移已重置', 'info'); }
function updateLyOffUI() { /* 按钮无状态显示，无需重渲染 */ }

/* ================= 多选批量 ================= */
const sel = { ids: new Set(), anchor: -1 };
function currentViewIds(id) {
  let ctx;
  if (state.view === 'album') ctx = albumTracks(state.albumKey);
  else if (state.view === 'smart') ctx = SMART_TRACKS;
  else if (state.view === 'playlist') ctx = (L.playlists.find(p => p.id === state.playlistId) || {})._tracks || [];
  else if (state.view === 'folders') ctx = L.tracks; // 文件夹视图范围选择退化为全库顺序
  else if (state.view === 'dups') ctx = (DUPS || []).flatMap(g => g.items.map(x => trackById(x.id))).filter(Boolean);
  else ctx = L.tracks.filter(hit);
  if (!ctx.some(t => t.id === id)) ctx = [trackById(id)].filter(Boolean);
  return ctx.map(t => t.id);
}
function trkClick(ev, id) {
  if (ev.metaKey || ev.ctrlKey) {
    ev.preventDefault(); ev.stopPropagation();
    if (sel.ids.has(id)) sel.ids.delete(id); else sel.ids.add(id);
    sel.anchor = id;
    renderSelState();
    return;
  }
  if (ev.shiftKey) {
    ev.preventDefault(); ev.stopPropagation();
    const ids = currentViewIds(id);
    const a = ids.indexOf(sel.anchor >= 0 ? sel.anchor : id);
    const b = ids.indexOf(id);
    if (a >= 0 && b >= 0) {
      ids.slice(Math.min(a, b), Math.max(a, b) + 1).forEach(x => sel.ids.add(x));
      renderSelState();
    }
    return;
  }
  if (sel.ids.size) { sel.ids.clear(); renderSelState(); }
  playFromView(id);
}
function renderSelState() {
  // 行高亮
  document.querySelectorAll('.trk').forEach(el => {
    const id = parseInt(el.dataset.tid);
    el.classList.toggle('sel', sel.ids.has(id));
  });
  // 浮条
  if (sel.ids.size) {
    selBar.style.display = 'flex';
    selBar.innerHTML = `<span>已选 <b>${sel.ids.size}</b> 首</span>
      <button class="btn sm" onclick="selLike()">♥ 收藏</button>
      <button class="btn sm" onclick="selAddMenu(event)">＋ 加入歌单</button>
      <button class="btn sm" onclick="selClear()">✕ 取消</button>`;
  } else {
    selBar.style.display = 'none';
  }
}
function selClear() { sel.ids.clear(); sel.anchor = -1; renderSelState(); }
async function selLike() {
  let n = 0;
  for (const id of sel.ids) {
    const t = trackById(id);
    if (t && !t.liked) { await invoke('toggle_like', { trackId: id }); t.liked = true; n++; }
  }
  await refresh(); selClear();
  toast(`已收藏 ${n} 首`, 'ok');
}
function selAddMenu(ev) {
  if (!L.playlists.length) return toast('还没有歌单，先侧栏新建一个', 'info');
  menuRoot.innerHTML = `<div class="menu"><div class="menu-h">加入歌单（${sel.ids.size} 首）</div>
    ${L.playlists.map(p => `<div class="menu-i" onclick="selDoAdd(${p.id})">${esc(p.name)}</div>`).join('')}</div>`;
  menuRoot.style.top = ev.clientY + 'px';
  menuRoot.style.left = Math.min(ev.clientX, innerWidth - 240) + 'px';
  menuRoot.style.display = 'block';
}
async function selDoAdd(pid) {
  closeMenu();
  let n = 0;
  for (const tid of sel.ids) {
    if (await invoke('add_to_playlist', { pid, tid })) n++;
  }
  selClear(); await refresh();
  if (state.view === 'playlist') loadPlaylistView();
  toast(`已加入 ${n} 首`, 'ok');
}

/* ================= 备份 / 写回 / 交叉淡入淡出 ================= */
async function doBackup(btn) {
  btn.disabled = true; btn.textContent = '打包中…';
  try { const msg = await invoke('backup_library'); toast(msg, 'ok'); }
  catch (e) { if (String(e) !== '未选择位置') toast(String(e), 'err'); }
  btn.disabled = false; btn.textContent = '备份';
}
async function doRestore(btn) {
  btn.disabled = true;
  try {
    const msg = await invoke('restore_library');
    toast(msg, 'ok');
    setTimeout(() => toast('建议现在重启好好听歌（⌘Q 后重新打开）', 'info'), 800);
  } catch (e) { if (String(e) !== '未选择文件') toast(String(e), 'err'); }
  btn.disabled = false;
}
/* ---- 自动备份：操作级快照（最多 10 份），设置页折叠列表 ---- */
let AUTO_BK = [], autoBkOpen = false;
const AUTO_BK_TRG = { cover: '封面获取', artist: '歌手获取', lyrics: '歌词获取', tr: '翻译获取' };
const fmtAutoBkTime = ts => {
  const d = new Date(ts), p = n => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
};
function autoBkRows() {
  return AUTO_BK.length
    ? AUTO_BK.map(b => `<div class="abk-row"><span class="t">${fmtAutoBkTime(b.ts)}</span><span class="tg">${AUTO_BK_TRG[b.trigger] || b.trigger}</span>
       <span class="dt" title="${esc(b.detail || '')}">${esc(b.detail || '')}</span>
       <button class="btn sm" onclick="restoreAutoBk(${b.ts},'${b.trigger}',this)">恢复</button></div>`).join('')
    : '<div class="abk-none">暂无自动备份——封面 / 歌手 / 歌词 / 翻译获取成功后自动创建</div>';
}
async function loadAutoBk() {
  try { AUTO_BK = await invoke('list_auto_backups'); } catch { return; }
  // 离开设置页后 DOM 不在，静默跳过；数据已在内存，下次渲染带上
  const cnt = document.getElementById('abkCount');
  if (!cnt || state.view !== 'settings') return;
  cnt.textContent = `${AUTO_BK.length}/10`;
  const list = document.getElementById('autoBkList');
  if (list) list.innerHTML = autoBkRows();
}
function toggleAutoBk() {
  autoBkOpen = !autoBkOpen;
  const list = document.getElementById('autoBkList');
  if (list) list.style.display = autoBkOpen ? '' : 'none';
  const tg = list?.closest('.abk')?.querySelector('.abk-head .abk-tg');
  if (tg) tg.textContent = autoBkOpen ? '收起 ▴' : '展开 ▾';
}
async function restoreAutoBk(ts, trigger, btn) {
  btn.disabled = true;
  try {
    const msg = await invoke('restore_auto_backup', { file: `auto-${ts}-${trigger}.zip` });
    toast(msg, 'ok');
    setTimeout(() => toast('建议现在重启好好听歌（⌘Q 后重新打开）', 'info'), 800);
  } catch (e) { toast(String(e), 'err'); btn.disabled = false; }
}
/* 待写回曲目（source=online：在线补全过元数据、尚未固化到文件标签） */
const vSettingsWB = () => L.tracks.filter(t => t.source === 'online' && !t.hidden);
let wbOpen = false;
function vSettingsWBRows() {
  const list = vSettingsWB();
  return list.length
    ? list.map(t => `<div class="abk-row"><span class="dt" title="${esc(t.title)} - ${esc(t.artist)}">${esc(t.title)} · ${esc(t.artist)}</span><span class="tg">${esc(t.album || '')}</span></div>`).join('')
    : '<div class="abk-none">没有待写回的曲目</div>';
}
function toggleWbList() {
  wbOpen = !wbOpen;
  const list = document.getElementById('wbList');
  if (list) list.style.display = wbOpen ? '' : 'none';
  const tg = list?.closest('.abk')?.querySelector('.abk-head .abk-tg');
  if (tg) tg.textContent = wbOpen ? '收起 ▴' : '展开 ▾';
}
// 封面清理：dry-run 预览 → modal 确认 → 执行
async function doCleanCovers(btn) {
  btn.disabled = true; const old = btn.textContent; btn.textContent = '扫描中…';
  let dry;
  try { dry = await invoke('clean_covers', { dryRun: true }); }
  catch (e) { toast(String(e), 'err'); btn.disabled = false; btn.textContent = old; return; }
  btn.disabled = false; btn.textContent = old;
  if (!dry.removed) return toast('没有未引用的封面，无需清理', 'ok');
  modal('清理封面缓存', `<p style="color:var(--tx1);line-height:1.9">检测到 <b>${dry.removed}</b> 个未引用封面文件，共 <b>${dry.freed_mb} MB</b>。<br><span class="muted">删除后不影响现有封面显示；被删封面若再次需要会自动重新获取。</span></p>`,
    `<button class="btn" onclick="closeModal()">取消</button>
     <button class="btn pri" onclick="confirmCleanCovers()">清理</button>`, true);
}
async function confirmCleanCovers() {
  try {
    const r = await invoke('clean_covers', { dryRun: false });
    closeModal();
    toast(`已清理 ${r.removed} 个文件，释放 ${r.freed_mb} MB`, 'ok');
  } catch (e) { toast(String(e), 'err'); }
}
async function doWritebackAll(btn) {
  btn.disabled = true; btn.textContent = '写回中…';
  try {
    const r = await invoke('writeback_all');
    toast(`写回完成：成功 ${r.ok}${r.fail ? ` · 失败 ${r.fail}` : ''}${r.first_err ? '（如 ' + r.first_err + '）' : ''}`, r.fail ? 'info' : 'ok');
    await refresh(); // 刷新待写回数量与列表
  } catch (e) { toast(String(e), 'err'); }
  btn.disabled = false; btn.textContent = '写回全部补全结果';
}
async function writebackOne(tid, btn) {
  btn.disabled = true; btn.textContent = '写回中…';
  try { await invoke('writeback_track', { trackId: tid }); toast('已写回到音频文件', 'ok'); }
  catch (e) { toast(String(e), 'err'); btn.disabled = false; btn.textContent = '写回文件'; }
  closeModal();
}

let CROSSFADE = 0;
let RG_MODE = 'off';
const CF_OPTS = [[0, '关闭（无缝衔接）'], [1000, '1 秒'], [2000, '2 秒'], [3000, '3 秒'], [4000, '4 秒'], [6000, '6 秒'], [8000, '8 秒']];
async function loadCrossfade() { CROSSFADE = await invoke('get_crossfade'); }
async function saveRgMode(mode) {
  RG_MODE = await invoke('set_rg_mode', { mode });
  toast(`ReplayGain：${mode === 'off' ? '关' : mode === 'track' ? 'Track 模式' : 'Album 模式'}`, 'info');
  render();
}
async function saveCrossfade(ms) {
  CROSSFADE = await invoke('set_crossfade', { ms });
  toast(`交叉淡入淡出：${ms === 0 ? '关（无缝衔接）' : ms / 1000 + ' 秒'}`, 'info');
  render();
}

/* ================= 曲库统计 ================= */
let STATS = null;
async function loadStats() {
  STATS = await invoke('get_stats');
  if (state.view === 'stats') render();
}
const fmtDur = s => s >= 3600 ? Math.floor(s / 3600) + ' 小时 ' + Math.round(s % 3600 / 60) + ' 分' : Math.round(s / 60) + ' 分钟';
function vStats() {
  if (!STATS) return '<div class="empty">加载中…</div>';
  const o = STATS.overview;
  const bar = (rows, color) => {
    const max = Math.max(...rows.map(r => r.n), 1);
    return rows.map(r => `<div class="stat-row">
      <span class="sr-name">${esc(r.name)}</span>
      <span class="sr-bar"><i style="width:${(r.n / max * 100).toFixed(1)}%;background:${color || 'var(--accent)'}"></i></span>
      <span class="sr-val">${r.n}</span></div>`).join('');
  };
  return `<div class="h-row"><h1>曲库统计</h1><span class="sub">本地计算 · 来自 play_history 与 tracks</span></div>
  <div class="stat-chips">
    <div class="chip"><div class="v">${o.tracks}</div><div class="k">曲目</div></div>
    <div class="chip"><div class="v">${o.albums}</div><div class="k">专辑</div></div>
    <div class="chip"><div class="v">${o.artists}</div><div class="k">艺人</div></div>
    <div class="chip"><div class="v" style="font-size:15px;line-height:1.3;margin-top:3px">${fmtDur(o.total_dur)}</div><div class="k">总时长</div></div>
    <div class="chip"><div class="v">${o.liked}</div><div class="k">收藏</div></div>
    <div class="chip"><div class="v">${o.enriched}</div><div class="k">在线补全</div></div>
    <div class="chip"><div class="v">${o.plays}</div><div class="k">累计播放</div></div>
    <div class="chip"><div class="v">${o.days7}</div><div class="k">近 7 天</div></div>
  </div>
  ${STATS.month_top.length ? `<div class="set-card"><h3>本月最常听 · Top 10</h3>${bar(STATS.month_top)}</div>` : ''}
  ${STATS.genres.length ? `<div class="set-card"><h3>流派分布</h3>${bar(STATS.genres, 'var(--ok)')}</div>` : ''}
  ${STATS.decades.length ? `<div class="set-card"><h3>年代分布</h3>${bar(STATS.decades, 'var(--warn)')}</div>` : ''}`;
}

/* ================= 主题（暗/亮/跟随系统 + 强调色） ================= */
const ACCENTS = [
  ['#5b8cff', '#8b5cf6', '晨蓝'],
  ['#8b5cf6', '#d946ef', '罗兰紫'],
  ['#10b981', '#14b8a6', '薄荷绿'],
  ['#f59e0b', '#f97316', '琥珀橙'],
  ['#ec4899', '#f43f5e', '玫瑰粉'],
  ['#06b6d4', '#3b82f6', '青碧'],
];
const themeMode = () => localStorage.getItem('lyra.theme') || 'auto';
const themeAccent = () => parseInt(localStorage.getItem('lyra.accent') || '0');
const sysDark = () => matchMedia('(prefers-color-scheme: dark)').matches;
function applyTheme() {
  const mode = themeMode();
  const light = mode === 'light' || (mode === 'auto' && !sysDark());
  document.body.classList.toggle('light', light);
  const [a, a2] = ACCENTS[themeAccent()] || ACCENTS[0];
  document.body.style.setProperty('--accent', a);
  document.body.style.setProperty('--accent2', a2);
  // 侧栏外观区同步
  const tb = document.getElementById('sideThemeBtn');
  if (tb) tb.textContent = light ? '🌙 切换暗色' : '☀ 切换亮色';
  const sa = document.getElementById('sideAccents');
  if (sa && !sa.childElementCount) {
    sa.innerHTML = ACCENTS.map(([c], i) =>
      `<span class="side-accent ${themeAccent() === i ? 'on' : ''}" style="background:${c}" onclick="setAccent(${i})"></span>`).join('');
  } else if (sa) {
    sa.querySelectorAll('.side-accent').forEach((el, i) => el.classList.toggle('on', themeAccent() === i));
  }
}
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => { if (themeMode() === 'auto') applyTheme(); });
function setTheme(mode) {
  localStorage.setItem('lyra.theme', mode);
  applyTheme();
  toast(`主题：${mode === 'auto' ? '跟随系统' : mode === 'light' ? '亮色' : '暗色'}`, 'info');
  render();
}
function setAccent(i) {
  localStorage.setItem('lyra.accent', String(i));
  applyTheme();
  render();
}
function toggleThemeQuick(btn) {
  const light = document.body.classList.contains('light');
  setTheme(light ? 'dark' : 'light');
}
applyTheme();

/* ================= 歌单操作 ================= */
function uiNewPlaylist() {
  modal('新建歌单', `<div class="field"><label>歌单名称</label><input id="mName" placeholder="例如：深夜驾驶" onkeydown="if(event.key==='Enter')okNewPl()"></div>`,
    `<button class="btn" onclick="closeModal();importM3U()">↧ 导入 M3U/M3U8</button><span style="flex:1"></span><button class="btn" onclick="closeModal()">取消</button><button class="btn pri" onclick="okNewPl()">创建</button>`);
  setTimeout(() => mName.focus(), 40);
}
async function okNewPl() {
  const n = mName.value.trim(); if (!n) return;
  await invoke('create_playlist', { name: n });
  closeModal(); await refresh();
  const p = L.playlists[L.playlists.length - 1];
  go('playlist', p.id); toast('已创建「' + esc(n) + '」', 'ok');
}
function uiRename(id) {
  const p = L.playlists.find(x => x.id === id);
  modal('重命名歌单', `<div class="field"><label>歌单名称</label><input id="mName" value="${esc(p.name)}"></div>`,
    `<button class="btn" onclick="closeModal()">取消</button><button class="btn pri" onclick="confirmRename(${id})">保存</button>`);
  setTimeout(() => { mName.focus(); mName.select(); }, 40);
}
async function confirmRename(id) {
  const n = mName.value.trim(); if (!n) return;
  await invoke('rename_playlist', { id, name: n });
  closeModal(); await refresh();
}
function uiDelete(id) {
  const p = L.playlists.find(x => x.id === id);
  modal('删除歌单', `<p style="color:var(--tx1);line-height:1.9">删除歌单 <b>「${esc(p.name)}」</b>？<br><span class="muted">仅删除歌单与关联（级联），不删除任何曲目文件。</span></p>`,
    `<button class="btn" onclick="closeModal()">取消</button><button class="btn danger" onclick="doDeletePl(${id})">删除</button>`);
}
async function doDeletePl(id) {
  await invoke('delete_playlist', { id });
  closeModal(); go('albums'); await refresh(); toast('歌单已删除', 'ok');
}
function addMenu(tid, ev) {
  ev.stopPropagation();
  menuRoot.innerHTML = `<div class="menu">${L.playlists.map(p => `<div class="mi" onclick="doAdd(${p.id},${tid})">${esc(p.name)}<span class="n">${p.count} 首</span></div>`).join('')}
   <div class="msep"></div><div class="mi" onclick="closeMenu();uiNewPlaylist()">＋ 新建歌单…</div></div>`;
  menuRoot.style.display = 'block';
  menuRoot.style.left = Math.min(ev.clientX, innerWidth - 220) + 'px';
  menuRoot.style.top = Math.min(ev.clientY, innerHeight - 260) + 'px';
}
async function doAdd(pid, tid) {
  const added = await invoke('add_to_playlist', { pid, tid });
  closeMenu();
  toast(added ? '已加入歌单（position 尾部追加）' : '已在歌单中（不重复加入）', added ? 'ok' : 'info');
  await refresh();
  if (state.view === 'playlist') loadPlaylistView();
}

/* ================= 歌词补全视图 ================= */
function vLyricsFetch() {
  const noLyric = L.tracks.filter(t => !t.has_lyrics);
  const hasLyric = L.tracks.length - noLyric.length;
  return `<div class="h-row"><h1>歌词补全</h1><span class="sub">LRCLIB（免 key · 同步 LRC）· 已接入</span></div>
  <div class="set-card"><h3>统计</h3>
   <p class="desc">有歌词 ${hasLyric} 首（外部 .lrc / 内嵌 / LRCLIB 缓存）· 缺歌词 ${noLyric.length} 首。获取结果写入本地库（lyrics_online 列），断网后依然可用。</p>
   <button class="btn pri" onclick="batchFetchLyrics()" ${noLyric.length ? '' : 'disabled'}>为缺歌词的 ${noLyric.length} 首批量获取（逐首请求，尊重服务端）</button></div>
  <div class="set-card"><h3>缺歌词曲目</h3>
  ${noLyric.length ? noLyric.slice(0, 100).map(t => `<div class="lf-row">${cover(t.cover, t.album, 34, 6)}
    <span class="t">${esc(t.title)} <span class="muted">· ${esc(t.artist)}</span></span>
    <button class="btn sm" onclick="fetchOneLyrics(${t.id},this)">获取</button></div>`).join('')
   : '<div class="empty">全部曲目都有歌词了 ✓</div>'}</div>`;
}
async function fetchOneLyrics(tid, btn) {
  if (btn) { btn.disabled = true; btn.dataset.old = btn.textContent; btn.textContent = '搜索中…'; }
  // 立即弹出面板 + loading（网络搜索需数秒，不能让用户干等无反馈）
  modal('选择歌词来源', `<div class="lyr-loading"><span class="spin"></span>正在搜索各渠道候选…（自定义源 + LRCLIB，约需数秒）</div>`, '<button class="btn" onclick="closeModal()">取消</button>', true);
  let r;
  try { r = await invoke('lyrics_search', { trackId: tid, baseUrl: trSrcUrl() || null }); }
  catch (e) {
    closeModal();
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '获取'; }
    return null;
  }
  if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '获取'; }
  window.__lyrPick = { tid, sel: null, body: null };
  const groups = r.groups || [];
  let h = '';
  if (!groups.length) {
    h = '<p class="muted">所有渠道均无候选结果（检查自定义源是否运行 / 网络连通）</p>';
  } else {
    h = `<div id="lyrPreview" class="pack-text" style="min-height:44px;display:flex;align-items:center;justify-content:center;color:var(--tx2);font-size:12px;margin-bottom:6px">点击候选预览歌词</div>` +
    groups.map(g => {
      const items = g.items || [];
      return `<div class="pack-src">${esc(g.src)} <span class="pg-cnt">${items.length} 条</span></div>` +
        (items.length ? `<div class="cand-list">${items.map((x, i) => `
          <div class="cand-item" id="cand-${g.src === 'LRCLIB' ? 'lrclib' : 'custom'}-${i}" onclick="pickLyrics(${i},'${g.src === 'LRCLIB' ? 'lrclib' : 'custom'}')">
            <div class="ci-main"><b>${esc(x.title)}</b> — ${esc(x.artist)}${x.album && x.album !== '未命名专辑' ? ' · ' + esc(x.album) : ''}</div>
            <div class="ci-sub">${x.duration ? fmt(x.duration) : ''}${x.dur_ok === true ? ' · <span class="src-st ok">时长匹配</span>' : (x.dur_ok === false ? ' · <span class="src-st bad">时长不符</span>' : '')}${x.synced === true ? ' · 时间轴' : (x.synced === false ? ' · 纯文本' : '')}${x.has_tr ? ' · <span class="badge b-tr">译</span>' : ''}</div>
          </div>`).join('')}</div>` : '<p class="muted" style="margin:0 0 6px">无候选</p>');
    }).join('');
  }
  h += `<p class="muted" style="font-size:11px;margin-top:8px">当前曲目：${esc((r.track || {}).title || '')} — ${esc((r.track || {}).artist || '')} · 点击预览，确认后应用</p>`;
  window.__lyrGroups = groups;
  modal('选择歌词来源', h,
    `<button class="btn" onclick="closeModal()">取消</button>
     <button class="btn pri" id="lyrConfirm" disabled onclick="confirmLyrics()">确认应用</button>`, true);
  return null;
}
function pickLyrics(i, src) {
  const g = (window.__lyrGroups || []).find(x => (src === 'lrclib' ? x.src === 'LRCLIB' : x.src !== 'LRCLIB'));
  const item = g && g.items[i];
  if (!item) return;
  // 选中态
  document.querySelectorAll('.cand-item').forEach(x => x.style.outline = '');
  const el = document.getElementById(`cand-${src}-${i}`);
  if (el) el.style.outline = '2px solid var(--accent)';
  const pv = document.getElementById('lyrPreview');
  const btn = document.getElementById('lyrConfirm');
  if (btn) btn.disabled = true;
  if (pv) pv.innerHTML = '<span class="spin"></span> 正在拉取歌词正文…';
  window.__lyrPick.sel = { src, item };
  invoke('lyrics_fetch_item', { item, src, baseUrl: trSrcUrl() || null })
    .then(v => {
      window.__lyrPick.body = v;
      // 预览确认有翻译 → 回填候选行"译"徽标（自定义源列表期未知正文）
      if (v.translation && !item.has_tr) {
        item.has_tr = true;
        const row = document.getElementById(`cand-${src}-${i}`);
        if (row && !row.querySelector('.b-tr')) {
          const sub = row.querySelector('.ci-sub');
          if (sub) sub.insertAdjacentHTML('beforeend', ' · <span class="badge b-tr">译</span>');
        }
      }
      // 预览：strip 时间戳，前 12 行
      const lines = (v.lrc || '').split('\n').map(l => l.replace(/\[\d{2}:\d{2}(?:\.\d{2,3})?\]/g, '').trim()).filter(Boolean).slice(0, 12);
      if (pv) pv.innerHTML = lines.map(l => esc(l)).join('<br>') + ((v.translation) ? '<br><span style="color:var(--tx2)">（含中文翻译）</span>' : '') || '（无正文）';
      if (btn) btn.disabled = false;
    })
    .catch(e => {
      if (pv) pv.innerHTML = `<span style="color:var(--err)">${esc(String(e))}</span>`;
    });
}
async function confirmLyrics() {
  const pk = window.__lyrPick;
  if (!pk || !pk.sel || !pk.body) return;
  const btn = document.getElementById('lyrConfirm');
  if (btn) { btn.disabled = true; btn.textContent = '应用中…'; }
  try {
    const label = pk.sel.src === 'lrclib' ? 'LRCLIB' : '自定义源';
    const msg = await invoke('lyrics_apply', { trackId: pk.tid, lrc: pk.body.lrc, translation: pk.body.translation || null, srcLabel: label });
    closeModal();
    toast(msg, 'ok');
    // 应用了翻译 → 立即同步列表徽标（后端已 recompute 写库；非当前曲 refreshLyricsView 管不到）
    if (pk.body.translation) syncTrBadge(pk.tid, true);
    // 「词」徽标局部刷新（不整页 render，滚动位置不动）
    syncLyricsBadge(pk.tid, true);
    // 歌词页开着且正在播这首 → 原地刷新；从列表按钮获取（页未开）不动歌词页
    if (lyrics.classList.contains('open') && state.curId === pk.tid) await refreshLyricsView();
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = '确认应用'; }
  }
}
async function batchFetchLyrics() {
  const noLyric = L.tracks.filter(t => !t.has_lyrics);
  toast(`开始批量获取 ${noLyric.length} 首（逐首请求）`, 'info');
  let ok = 0, fail = 0;
  for (const t of noLyric) {
    try { await invoke('fetch_online_lyrics', { trackId: t.id }); ok++; } catch { fail++; }
    await new Promise(r => setTimeout(r, 350)); // 温和限速
  }
  await refresh();
  toast(`批量完成：成功 ${ok} · 未收录 ${fail}`, ok ? 'ok' : 'info');
}
function playAlbumNow(keyEnc) {
  const key = decodeURIComponent(keyEnc);
  const ids = albumTracks(key).map(t => t.id);
  if (ids.length) playList(ids, 0);
}
async function fetchAlbumLyrics(keyEnc) {
  const key = decodeURIComponent(keyEnc) || state.albumKey;
  const ts = albumTracks(key).filter(t => !t.has_lyrics);
  if (!ts.length) return toast('本专辑全部曲目已有歌词', 'info');
  toast(`为本专辑 ${ts.length} 首获取歌词…`, 'info');
  let ok = 0;
  for (const t of ts) { try { await invoke('fetch_online_lyrics', { trackId: t.id }); ok++; } catch { } await new Promise(r => setTimeout(r, 350)); }
  await refresh(); toast(`完成：成功 ${ok} / ${ts.length}`, ok ? 'ok' : 'info');
}
async function lyricsAction(id, btn) {
  // 直接弹多源选择，应用后局部刷新行徽标——不跳转歌词页、不改动当前播放曲目
  await fetchOneLyrics(id, btn);
}

/* ================= 元数据补全（MusicBrainz，设计文档 5.4） ================= */
let ENRICH = [];
async function loadEnrich() {
  ENRICH = await invoke('list_enrich');
  cntEn.textContent = ENRICH.length || '';
  if (state.view === 'enrich') render();
}
function vEnrich() {
  const guessed = L.tracks.filter(t => t.source === 'guessed' || t.artist === '未知艺术家' || t.album === '未命名专辑').length;
  const applied = L.tracks.filter(t => t.source === 'online').length;
  return `<div class="h-row"><h1>元数据补全</h1><span class="sub">MusicBrainz 文本匹配 · 高置信自动应用 / 中置信人工确认（5.4）</span></div>
  <div class="stat-chips">
    <div class="chip"><div class="v" style="color:var(--warn)">${ENRICH.length}</div><div class="k">待确认</div></div>
    <div class="chip"><div class="v" style="color:var(--ok)">${applied}</div><div class="k">已应用（online）</div></div>
    <div class="chip"><div class="v">${guessed}</div><div class="k">缺失/推断候选曲目</div></div></div>
  <div class="set-card" style="margin-bottom:20px">
    <p class="desc">按 标题+艺人+时长 匹配 MusicBrainz（严格 1 req/s）：<b>有真实标题但缺专辑/年份/艺人</b>的曲目走文本匹配，
    置信度 ≥0.9 自动应用、0.5–0.9 进入待确认；<b>标题本身无效</b>（Unknown 01 / 乱码文件名）的曲目文本匹配无意义，
    需要<b>音频指纹识别（AcoustID，规划中）</b>，扫描时会单独计数跳过。结果写入本地库，仅覆盖缺失字段，来源标记 online（可撤销）。</p>
    <button class="btn pri" id="enrichBtn" onclick="doEnrichMissing()" ${guessed ? '' : 'disabled'}>扫描缺失曲目并在线补全（最多 20 首）</button>
    <span class="muted" id="enrichProg" style="margin-left:12px"></span></div>
  ${ENRICH.length ? ENRICH.map((e, ei) => {
    const c = e.candidates[e.ci ?? 0] || e.candidates[0];
    if (!c) return '';
    if (!e.sel) e.sel = { title: true, artist: true, album: true, year: true };
    const pct = Math.round(e.conf * 100);
    const col = e.conf >= .9 ? 'var(--ok)' : e.conf >= .6 ? 'var(--warn)' : 'var(--err)';
    const C = 2 * Math.PI * 23;
    const KEY = { '标题': 'title', '艺人': 'artist', '专辑': 'album', '年份': 'year' };
    const localFlds = ['标题', '艺人', '专辑', '年份'].map(k => {
      const v = k === '标题' ? e.title : k === '艺人' ? e.artist : k === '专辑' ? (e.album ?? '—') : (e.year ?? '—');
      return `<div class="fld"><span class="fk">${k}</span><span class="fv">${esc(v)}</span></div>`;
    }).join('');
    const candFlds = ['标题', '艺人', '专辑', '年份'].map(k => {
      const key = KEY[k];
      const v = key === 'title' ? c.title : key === 'artist' ? c.artist : key === 'album' ? (c.album ?? '—') : (c.year ?? '—');
      const noVal = v === '—';
      return `<div class="fld" style="${noVal ? 'opacity:.4' : ''}">${noVal ? '' : `<input type="checkbox" class="fchk" ${e.sel[key] ? 'checked' : ''} onchange="toggleField(${ei},'${key}',this.checked)">`}<span class="fk">${k}</span><span class="fv">${esc(v)}</span></div>`;
    }).join('');
    const listening = state.curId === e.track_id;
    const selCount = Object.values(e.sel).filter(Boolean).length;
    return `<div class="ecard" ${listening ? 'style="box-shadow:inset 0 0 0 1px var(--accent)"' : ''}>
      <div class="top">
       <button class="btn sm ${listening ? 'pri' : ''}" style="flex:none" onclick="playEnrichTrack(${e.track_id})" title="播放该曲目，听完再决定">${listening ? '♪ 试听中' : '▶ 试听'}</button>
       <span class="fn">${esc(e.title)} · ${esc(e.artist)}</span><span class="sp"></span>
       <span class="muted">${pct >= 90 ? '高置信' : pct >= 60 ? '中置信 · 需确认' : '低置信'}</span></div>
      <div class="body">
        <div class="col"><div class="ch">本地（现状）</div>${localFlds}</div>
        <div class="mid"><div class="conf"><svg width="54" height="54"><circle cx="27" cy="27" r="23" fill="none" stroke="var(--bg4)" stroke-width="4"/>
          <circle cx="27" cy="27" r="23" fill="none" stroke="${col}" stroke-width="4" stroke-linecap="round" stroke-dasharray="${C * e.conf} ${C}"/></svg>
          <span class="val" style="color:${col}">${pct}%</span></div>
         <span class="muted">${(e.ci ?? 0) + 1}/${e.candidates.length}</span>
         <button class="btn sm" ${e.candidates.length < 2 ? 'disabled' : ''} onclick="cycleEC(${ei})">⇄ 换一个</button></div>
        <div class="col cand"><div class="ch" style="display:flex;align-items:center;gap:6px">
          <input type="checkbox" class="fchk-all" ${Object.values(e.sel).every(Boolean) ? 'checked' : ''} onchange="toggleAllFields(${ei},this.checked)">
          <span style="width:46px;text-align:left;flex:none">全选</span>
          <span class="badge b-online">${esc(c.provider)}</span>
        </div>${candFlds}</div>
      </div>
      <div class="foot"><span class="muted">应用所选 ${selCount} 项（勾选=用候选值，未勾=保持本地）· 来源标记 online</span><span class="sp"></span>
       <button class="btn sm" onclick="dismissE(${e.track_id})">✗ 忽略</button>
       <button class="btn sm pri" onclick="applyE(${e.track_id},${ei})">✓ 应用所选</button></div></div>`;
  }).join('')
  : `<div class="empty">没有待确认的补全<br><span class="muted">已处理过的曲目（应用/忽略/无匹配）不会重复查询 MusicBrainz；<br>网络失败的曲目下次点击会自动重试。Unknown/乱码标题需指纹识别（规划中）</span></div>`}`;
}
function cycleEC(i) { ENRICH[i].ci = ((ENRICH[i].ci ?? 0) + 1) % ENRICH[i].candidates.length; ENRICH[i].sel = null; render(); }
function toggleField(ei, key, on) { ENRICH[ei].sel[key] = on; syncCardSel(ei); }
function toggleAllFields(ei, on) {
  Object.keys(ENRICH[ei].sel).forEach(k => ENRICH[ei].sel[k] = on);
  syncCardSel(ei);
}
/// 局部同步卡片勾选 UI（全选框状态 + 底部计数）
function syncCardSel(ei) {
  const e = ENRICH[ei];
  const card = document.querySelectorAll('.ecard')[ei];
  if (!card) return;
  const allOn = Object.values(e.sel).every(Boolean);
  const all = card.querySelector('.fchk-all');
  if (all) all.checked = allOn;
  const muted = card.querySelector('.foot .muted');
  if (muted) {
    const n = Object.values(e.sel).filter(Boolean).length;
    muted.textContent = `应用所选 ${n} 项（勾选=用候选值，未勾=保持本地）· 来源标记 online`;
  }
}
function playEnrichTrack(tid) { playList([tid], 0); }
async function applyE(tid, ei) {
  const sel = ENRICH[ei].sel || {};
  const fields = Object.keys(sel).filter(k => sel[k]);
  if (!fields.length) return toast('至少勾选一个要应用的字段', 'err');
  try {
    await invoke('apply_enrich', { trackId: tid, candIdx: ENRICH[ei].ci ?? 0, fields });
    toast(`已应用 ${fields.length} 个字段（${fields.join('/')}），来源 online（可撤销）`, 'ok');
  } catch (e) { toast(String(e), 'err'); return; }
  await refresh(); await loadEnrich();
}
async function dismissE(tid) {
  await invoke('dismiss_enrich', { trackId: tid });
  await loadEnrich(); toast('已忽略该候选', 'info');
}
async function doEnrichMissing() {
  const btn = enrichBtn; btn.disabled = true;
  try {
    const r = await invoke('enrich_missing', { baseUrl: trSrcUrl() || null });
    toast(r.message || '补全完成', (r.applied || r.review) ? 'ok' : 'info');
    if (r.error) toast('部分查询失败（如持续出现请反馈）: ' + r.error, 'err', { copy: true });
  } catch (e) { toast(String(e), 'err', { copy: true }); }
  btn.disabled = false; enrichProg.textContent = '';
  await refresh(); await loadEnrich();
}

/* ================= 设置 ================= */
let PROVIDERS = [];
const t2sEnabled = () => localStorage.getItem('lyra.t2s') !== '0';
const trSrcUrl = () => (localStorage.getItem('lyra.trSrc') || '').trim();
async function fetchTranslation(id, btn) {
  const url = trSrcUrl();
  if (!url) return toast('请先在 设置 → 联网增强 → 自定义源 配置地址', 'err');
  if (btn) { btn.disabled = true; btn.dataset.old = btn.textContent; btn.textContent = '获取中…'; btn.classList.add('loading'); }
  try {
    const msg = await invoke('fetch_translation', { trackId: id, baseUrl: url });
    toast(msg, 'ok');
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = btn.dataset.old || '获取翻译'; btn.classList.remove('loading'); }
    return;
  }
  await refreshLyricsView();
}
function setT2s(on) {
  localStorage.setItem('lyra.t2s', on ? '1' : '0');
  if (lyrics.classList.contains('open')) refreshLyricsView();
  toast('歌词繁体转简体：' + (on ? '开' : '关'), 'info');
}
function saveTrSrc() {
  localStorage.setItem('lyra.trSrc', trSrcInput.value.trim());
  toast('自定义源地址已保存', 'ok');
}
// 连通性测试：分层诊断（适配器 → 网易云上游），不依赖播放曲目
async function testTrSrc() {
  const url = trSrcUrl();
  if (!url) return toast('请先填写地址', 'err');
  toast('测试中…', 'info');
  try { toast(await invoke('test_custom_source', { baseUrl: url }), 'ok'); }
  catch (e) { toast(String(e), 'err'); }
}
let SCAN_ROOTS = [], CACHE_INFO = { enabled: true, limit_mb: 2048, used_mb: 0 };
async function loadCacheInfo() {
  CACHE_INFO = await invoke('get_cache_info');
  if (state.view === 'settings') render();
}
async function saveCacheCfg(enabled, mb) {
  CACHE_INFO = await invoke('set_cache_config', { enabled, limitMb: mb });
  toast(`缓存：${enabled ? '开 · ' + fmtMB(mb) : '关'}`, 'ok');
  if (state.view === 'settings') render();
}
async function clearCache(btn) {
  btn.disabled = true;
  CACHE_INFO = await invoke('clear_cache');
  toast(`已清理，释放 ${CACHE_INFO.used_mb} MB`, 'ok');
  render();
}
const fmtMB = mb => mb >= 1024 ? (mb / 1024).toFixed(1) + ' GB' : mb + ' MB';
const CACHE_OPTS = [512, 1024, 2048, 4096, 8192, 0];
let FADE_MS = 10;
const FADE_OPTS = [5, 10, 20, 50, 100, 200, 500];
async function loadFade() { FADE_MS = await invoke('get_fade'); }
async function saveFade(on, ms) {
  FADE_MS = on ? ms : 0;
  await invoke('set_fade', { ms: FADE_MS });
  toast(`音量渐入渐出：${on ? ms + 'ms' : '关'}`, 'info');
  render();
}

/* ================= 均衡器（10 段，M5） ================= */
const EQ_FREQS = ['31', '62', '125', '250', '500', '1k', '2k', '4k', '8k', '16k'];
const EQ_PRESETS = {
  '平坦': [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
  '流行': [-1, 1, 3, 4, 3, 0, -1, -1, 1, 2],
  '摇滚': [4, 3, 1, 0, -1, 0, 1, 3, 4, 4],
  '爵士': [2, 2, 1, 2, -1, -1, 0, 1, 3, 3],
  '古典': [3, 2, 0, 0, 0, 0, 0, 1, 2, 3],
  '人声': [-2, -1, 1, 3, 4, 4, 3, 1, 0, -1],
  '低音增强': [6, 5, 3, 1, 0, 0, 0, 0, 0, 0],
};
let EQ = { enabled: false, gains: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], toneOn: false, bass: 0, treble: 0 };
const eqCommit = () => invoke('set_eq', { enabled: EQ.enabled, gains: EQ.gains, toneOn: EQ.toneOn, bass: EQ.bass, treble: EQ.treble });

/* —— 频响曲线（前端复刻 biquad 系数，合成 10 段 peak + 音调搁架）—— */
const EQ_FREQS_HZ = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
function eqBiquad(type, f0, gdb, fs) {
  const A = Math.pow(10, gdb / 40), w0 = 2 * Math.PI * f0 / fs;
  const sin = Math.sin(w0), cos = Math.cos(w0);
  if (type === 'peak') {
    const al = sin / (2 * 1.4), a0 = 1 + al / A;
    return { b: [(1 + al * A) / a0, -2 * cos / a0, (1 - al * A) / a0], a: [1, (-2 * cos) / a0, (1 - al / A) / a0] };
  }
  const al = sin / (2 * 0.7), taa = 2 * Math.sqrt(A) * al;
  if (type === 'low') {
    const a0 = (A + 1) + (A - 1) * cos + taa;
    return { b: [A * ((A + 1) - (A - 1) * cos + taa) / a0, 2 * A * ((A - 1) - (A + 1) * cos) / a0, A * ((A + 1) - (A - 1) * cos - taa) / a0],
             a: [1, -2 * ((A - 1) + (A + 1) * cos) / a0, ((A + 1) + (A - 1) * cos - taa) / a0] };
  }
  // RBJ Audio EQ Cookbook 原文公式（DC=0dB、Nyquist=+g 双约束数值验证通过）
  const a0 = (A + 1) - (A - 1) * cos + taa;
  return { b: [A * ((A + 1) + (A - 1) * cos + taa) / a0, -2 * A * ((A - 1) + (A + 1) * cos) / a0, A * ((A + 1) + (A - 1) * cos - taa) / a0],
           a: [1, 2 * ((A - 1) - (A + 1) * cos) / a0, ((A + 1) - (A - 1) * cos - taa) / a0] };
}
function eqDbAt(f, scope) {
  // scope: 'eq'（仅 10 段 peak）/ 'tone'（仅音调搁架）——两条曲线独立展示；
  // EQ 段随 EQ 开关、音调随音调开关，两者相互独立
  const fs = 48000;
  let segs = [];
  if (scope !== 'tone' && EQ.enabled) {
    EQ_FREQS_HZ.forEach((fr, i) => { if (Math.abs(EQ.gains[i] ?? 0) > 0.05) segs.push(eqBiquad('peak', fr, EQ.gains[i], fs)); });
  }
  if (scope !== 'eq' && EQ.toneOn) {
    if (Math.abs(EQ.bass) > 0.05) segs.push(eqBiquad('low', 200, EQ.bass, fs));
    if (Math.abs(EQ.treble) > 0.05) segs.push(eqBiquad('high', 3500, EQ.treble, fs));
  }
  let db = 0;
  for (const bq of segs) {
    const w = 2 * Math.PI * f / fs, c = Math.cos(w), s = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w);
    const nR = bq.b[0] + bq.b[1] * c + bq.b[2] * c2, nI = -(bq.b[1] * s + bq.b[2] * s2);
    const dR = 1 + bq.a[1] * c + bq.a[2] * c2, dI = -(bq.a[1] * s + bq.a[2] * s2);
    db += 10 * Math.log10(Math.max((nR * nR + nI * nI) / (dR * dR + dI * dI), 1e-12));
  }
  return db;
}
function eqSmoothPath(pts) {
  if (pts.length < 3) return 'M' + pts.map(p => p.join(',')).join('L');
  let d = `M${pts[0][0].toFixed(1)},${pts[0][1].toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[Math.max(0, i - 1)], p1 = pts[i], p2 = pts[i + 1], p3 = pts[Math.min(pts.length - 1, i + 2)];
    d += `C${(p1[0] + (p2[0] - p0[0]) / 6).toFixed(1)},${(p1[1] + (p2[1] - p0[1]) / 6).toFixed(1)} ${(p2[0] - (p3[0] - p1[0]) / 6).toFixed(1)},${(p2[1] - (p3[1] - p1[1]) / 6).toFixed(1)} ${p2[0].toFixed(1)},${p2[1].toFixed(1)}`;
  }
  return d;
}
function drawEqCurve() {
  const el = document.getElementById('eqCurve');
  if (!el) return;
  const W = 700, H = 130, mid = H / 2, span = mid - 8, N = 160;
  const traceEq = () => {
    const pts = [];
    for (let i = 0; i <= N; i++) {
      const f = 20 * Math.pow(1000, i / N); // 20Hz → 20kHz 对数轴
      const y = mid - (eqDbAt(f, 'eq') / 15) * span;
      pts.push([(i / N) * W, Math.max(5, Math.min(H - 5, y))]);
    }
    return eqSmoothPath(pts);
  };
  // 音调曲线独立刻度：0%（0dB）贴底部基线，100%（+12dB）到顶部（Poweramp 风格，仅提升）
  const traceTone = () => {
    const pts = [];
    for (let i = 0; i <= N; i++) {
      const f = 20 * Math.pow(1000, i / N);
      const db = Math.max(0, eqDbAt(f, 'tone'));
      const y = (H - 6) - (db / 12) * (H - 12);
      pts.push([(i / N) * W, Math.max(5, Math.min(H - 5, y))]);
    }
    return eqSmoothPath(pts);
  };
  // EQ 曲线（主题色 + 渐变填充）
  const dEq = traceEq();
  el.setAttribute('d', dEq);
  const fill = document.getElementById('eqCurveFill');
  if (fill) fill.setAttribute('d', dEq + `L${W},${H} L0,${H} Z`);
  // 音调曲线（浅蓝实线 + 底部面积填充，独立底部基线；关闭时不画）
  const toneEl = document.getElementById('eqToneCurve');
  if (toneEl) {
    const dTone = EQ.toneOn ? traceTone() : '';
    toneEl.setAttribute('d', dTone);
    const toneFill = document.getElementById('eqToneFill');
    if (toneFill) toneFill.setAttribute('d', dTone ? dTone + `L${W},${H - 4} L0,${H - 4} Z` : '');
  }
}
let eqTimer = null;
/// 当前增益匹配的预设名（无匹配 = 自定义），localStorage 持久化供重启后下拉选中
const eqMatchPreset = () => {
  const found = Object.keys(EQ_PRESETS).find(k =>
    EQ_PRESETS[k].length === EQ.gains.length && EQ_PRESETS[k].every((g, i) => Math.abs(g - (EQ.gains[i] ?? 0)) < 0.01));
  return found || '自定义';
};
let eqPresetName = '自定义';
async function loadEq() {
  EQ = await invoke('get_eq');
  eqPresetName = eqMatchPreset();
  localStorage.setItem('lyra.eqPreset', eqPresetName);
}
function openEq() {
  const sliders = EQ_FREQS.map((f, i) => `
    <div class="eq-band">
      <input type="range" min="-12" max="12" step="0.5" value="${EQ.gains[i] ?? 0}"
        oninput="eqSlide(${i},this.value)">
      <span class="eq-v" id="eqv${i}">${(EQ.gains[i] ?? 0) > 0 ? '+' : ''}${(EQ.gains[i] ?? 0).toFixed(1)}</span>
      <span class="eq-f">${f}</span>
    </div>`).join('');
  modal('均衡器 EQ · 10 段', `
    <div style="display:flex;gap:10px;align-items:center;margin-bottom:14px;flex-wrap:wrap">
      <label class="switch"><input type="checkbox" ${EQ.enabled ? 'checked' : ''} onchange="eqToggle(this.checked)"><i></i></label>
      <span>启用</span>
      <select id="eqSel" onchange="eqPreset(this.value)">
        ${Object.keys(EQ_PRESETS).map(k => `<option ${k === eqPresetName ? 'selected' : ''}>${k}</option>`).join('')}
        ${eqPresetName === '自定义' ? '<option selected>自定义</option>' : ''}
      </select>
      <span class="muted" style="font-size:12px">双击滑块归零 · 实时生效（采样级滤波）</span>
    </div>
    <div class="eq-grid" ${EQ.enabled ? '' : 'style="opacity:.45"'}>${sliders}</div>
    <svg class="eq-chart" viewBox="0 0 700 130" preserveAspectRatio="none">
      <defs><linearGradient id="eqFillGrad" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="var(--accent)" stop-opacity=".28"/>
        <stop offset="1" stop-color="var(--accent)" stop-opacity="0"/>
      </linearGradient></defs>
      ${EQ_FREQS_HZ.map((f, i) => {
        const x = ((Math.log10(f / 20) / 3) * 700).toFixed(1);
        return `<line x1="${x}" x2="${x}" y1="8" y2="122" class="eq-grid-line"/>
          <text x="${x}" y="127" text-anchor="middle" class="eq-grid-lab">${EQ_FREQS[i]}</text>`;
      }).join('')}
      <line x1="0" x2="700" y1="65" y2="65" class="eq-zero-line"/>
      <path id="eqCurveFill" fill="url(#eqFillGrad)"/>
      <path id="eqCurve" fill="none" stroke="var(--accent)" stroke-width="2.5" stroke-linejoin="round"/>
      <path id="eqToneFill" fill="var(--accent)" fill-opacity=".15"/>
      <path id="eqToneCurve" fill="none" stroke="var(--accent)" stroke-opacity=".5" stroke-width="2" stroke-linejoin="round"/>
    </svg>
    <div class="tone-row">
      <label class="switch" style="flex:none"><input type="checkbox" ${EQ.toneOn ? 'checked' : ''} onchange="eqToneToggle(this.checked)"><i></i></label>
      <span style="flex:none;font-size:13px">音调</span>
      <span class="tone-lab">低音</span>
      <input type="range" min="0" max="100" step="1" value="${eqDbToPct(EQ.bass)}" oninput="eqTone('bass',this.value)" ondblclick="this.value=0;eqTone('bass',0)">
      <span class="tone-v" id="tvBass">${eqDbToPct(EQ.bass)}%</span>
      <span class="tone-lab">高音</span>
      <input type="range" min="0" max="100" step="1" value="${eqDbToPct(EQ.treble)}" oninput="eqTone('treble',this.value)" ondblclick="this.value=0;eqTone('treble',0)">
      <span class="tone-v" id="tvTreble">${eqDbToPct(EQ.treble)}%</span>
    </div>`,
    `<button class="btn" onclick="closeModal()">关闭</button>`, true);
  setTimeout(() => document.querySelectorAll('.eq-band input').forEach((el, i) => {
    el.ondblclick = () => { el.value = 0; eqSlide(i, 0); };
  }), 0);
  drawEqCurve();
}
/// 下拉同步：匹配当前增益的预设（必要时动态补「自定义」选项并选中）
function eqUpdateSel() {
  eqPresetName = eqMatchPreset();
  localStorage.setItem('lyra.eqPreset', eqPresetName);
  const sel = document.getElementById('eqSel');
  if (!sel) return;
  if (eqPresetName === '自定义' && ![...sel.options].some(o => o.value === '自定义' || o.text === '自定义')) {
    sel.add(new Option('自定义', '自定义'));
  }
  sel.value = eqPresetName;
}
function eqSlide(i, v) {
  EQ.gains[i] = parseFloat(v);
  document.getElementById('eqv' + i).textContent = (v > 0 ? '+' : '') + parseFloat(v).toFixed(1);
  eqUpdateSel(); drawEqCurve();
  clearTimeout(eqTimer);
  eqTimer = setTimeout(() => { eqTimer = null; eqCommit(); }, 120);
}
/// 局部同步滑块/数值 UI（不重建弹窗，避免闪烁与下拉选中丢失）
function eqSyncSliders() {
  document.querySelectorAll('.eq-band input').forEach((inp, i) => {
    inp.value = EQ.gains[i] ?? 0;
  });
  EQ_FREQS.forEach((_, i) => {
    const v = document.getElementById('eqv' + i);
    if (v) {
      const g = EQ.gains[i] ?? 0;
      v.textContent = (g > 0 ? '+' : '') + g.toFixed(1);
    }
  });
  const grid = document.querySelector('.eq-grid');
  if (grid) grid.style.opacity = EQ.enabled ? '' : '0.45';
}
function eqToggle(on) { EQ.enabled = on; eqCommit(); eqSyncSliders(); drawEqCurve(); }
function eqToneToggle(on) {
  EQ.toneOn = on;
  const row = document.querySelector('.tone-row');
  if (row) row.style.opacity = on ? '' : '0.45';
  eqCommit(); drawEqCurve();
}
/// Poweramp 风格音调映射：0–100%，0=无效果（平直），100=最大增强 +12dB（仅提升无衰减）
const eqPctToDb = pct => parseFloat(pct) / 100 * 12;
const eqDbToPct = db => Math.round(db / 12 * 100);
function eqTone(kind, pct) {
  EQ[kind] = eqPctToDb(pct);
  const el = document.getElementById(kind === 'bass' ? 'tvBass' : 'tvTreble');
  if (el) el.textContent = Math.round(pct) + '%';
  drawEqCurve();
  clearTimeout(eqTimer);
  eqTimer = setTimeout(() => { eqTimer = null; eqCommit(); }, 120);
}
function eqPreset(name) {
  EQ.gains = [...EQ_PRESETS[name]];
  if (!EQ.enabled) EQ.enabled = true;
  eqPresetName = name;
  localStorage.setItem('lyra.eqPreset', name);
  eqCommit();
  eqSyncSliders();
}

/* ================= 专辑封面在线获取与切换（M4） ================= */
let selCover = null;
function pickCover(u, el) {
  selCover = u;
  document.querySelectorAll('.cover-grid img').forEach(x => x.classList.remove('sel'));
  el.classList.add('sel');
  const pv = document.getElementById('cvPreview');
  if (pv) pv.innerHTML = `<img src="${u}">`;
  const btn = document.getElementById('cvApply');
  if (btn) btn.disabled = false;
}
async function fetchAlbumCovers(keyEnc) {
  const key = decodeURIComponent(keyEnc);
  // 立即弹窗 + loading（规范 §1.1：绝不无反馈等待）
  modal('选择专辑封面', `<div class="lyr-loading"><span class="spin"></span>正在并行查询：自定义源 / Deezer / Last.fm / iTunes…（约需数秒）</div>`,
    '<button class="btn" onclick="closeModal()">取消</button>');
  let r;
  try { r = await invoke('fetch_album_covers', { albumKey: key, baseUrl: trSrcUrl() || null }); }
  catch (e) { closeModal(); toast(String(e), 'err'); return; }
  renderCoverPicker(r.covers || [], r.status || [], '选择专辑封面',
    `applyAlbumCoverConfirm('${enc(key)}')`, '下载入库，专辑内全部曲目同步');
}
// 封面候选 → 选择网格弹窗（专辑/歌单共用；选中写 selCover，#cvApply 调 applyCall 片段）
function renderCoverPicker(urls, status, title, applyCall, applyHint) {
  const statLine = (status || []).map(s => `<span class="src-st ${s.n ? 'ok' : 'bad'}">${esc(s.src)}：${s.skipped ? '未配置' : (s.n ? s.n + ' 张' : '无结果/失败')}</span>`).join('　');
  if (!urls.length) {
    modal(title, `<p class="muted">各渠道均无结果</p><p style="margin-top:8px">${statLine}</p>`,
      '<button class="btn" onclick="closeModal()">关闭</button>');
    return;
  }
  const groups = {};
  urls.forEach(x => { (groups[x.src] = groups[x.src] || []).push(x.url); });
  modal(title,
    `<div id="cvPreview" class="cv-preview"><span class="muted" style="font-size:12px">点击下方封面选中并预览</span></div>
     <p class="muted" style="margin:6px 0 4px">${statLine}</p>` +
    Object.entries(groups).map(([src, list]) => `
    <div class="pack-src">${esc(src)} <span class="pg-cnt">${list.length} 张</span></div>
    <div class="cover-grid">${list.map(u => `
      <div class="cv-item"><img src="${u}" loading="lazy" onload="this.closest('.cv-item').querySelector('.cv-dim').textContent=this.naturalWidth+'×'+this.naturalHeight" onclick="pickCover('${u}',this)"><div class="cv-dim"></div></div>`).join('')}</div>`).join('')
    + `<p class="muted" style="margin-top:10px">点击选中并预览，确认后应用（${applyHint}）</p>`,
    `<button class="btn" onclick="closeModal()">取消</button>
     <button class="btn pri" id="cvApply" disabled onclick="${applyCall}">确认应用</button>`);
}

/* ================= 封面来源选择：本地图片 / 网络搜索（专辑·歌手·歌单） ================= */
function coverSourceDialog(kind, arg) {
  const titles = { album: '设置专辑封面', artist: '设置歌手头像', playlist: '设置歌单封面' };
  const pl = kind === 'playlist' ? L.playlists.find(x => x.id === Number(arg)) : null;
  modal(titles[kind] || '设置封面',
    `<p class="muted" style="padding:4px 0 10px">选择封面来源：</p>
     <div style="display:flex;gap:12px;justify-content:center;padding:6px 0 12px">
       <button class="btn pri" style="min-width:140px" onclick="coverLocal('${kind}','${arg}')">本地图片</button>
       <button class="btn" style="min-width:140px" onclick="coverSearch('${kind}','${arg}')">网络搜索</button>
     </div>` +
    (pl && pl.cover ? `<div style="text-align:center"><button class="btn sm" onclick="applyPlaylistCover('${arg}','')">清除封面（恢复拼贴）</button></div>` : ''),
    '<button class="btn" onclick="closeModal()">取消</button>');
}
function coverSearch(kind, arg) {
  if (kind === 'album') fetchAlbumCovers(arg);
  else if (kind === 'artist') fetchArtistPack(arg);
  else fetchPlaylistCovers(arg);
}
async function coverLocal(kind, arg) {
  let p;
  try { p = await invoke('pick_image_file'); } catch (e) { toast(String(e), 'err'); return; }
  if (!p) return; // 用户取消选择
  if (kind === 'album') { selCover = p; closeModal(); applyAlbumCoverConfirm(arg); }
  else if (kind === 'artist') artistImgLocal(arg, p);
  else applyPlaylistCover(arg, p);
}
async function artistImgLocal(nEnc, path) {
  const n = decodeURIComponent(nEnc);
  const localVal = 'local:' + path;
  try {
    await invoke('set_artist_field', { name: n, field: 'img', value: localVal });
    state.artistImgs[n] = localVal;
    closeModal();
    // 与 applyArtistImgConfirm 相同的局部刷新（hero + 列表卡片头像原地替换）
    const heroAv = document.querySelector('#artistHero .avatar');
    if (heroAv) heroAv.outerHTML = `<img class="avatar big" src="${artistImgSrc(localVal)}" alt="">`;
    document.querySelectorAll('.artist-card').forEach(card => {
      if (card.querySelector('.nm')?.textContent === n) {
        const old = card.querySelector('.avatar');
        if (old && old.tagName === 'DIV') old.outerHTML = `<img class="avatar big" src="${artistImgSrc(localVal)}" loading="lazy">`;
        else if (old) old.src = artistImgSrc(localVal);
      }
    });
    loadArtistHero();
    toast('头像已应用', 'ok');
  } catch (e) { toast(String(e), 'err'); }
}
async function fetchPlaylistCovers(pidEnc) {
  const pid = Number(pidEnc);
  const kw = (L.playlists.find(x => x.id === pid) || {}).name || '';
  modal('网络搜索歌单封面', `<div class="lyr-loading"><span class="spin"></span>正在并行查询：自定义源 / Deezer / Last.fm / iTunes…（关键词：${esc(kw)}）</div>`,
    '<button class="btn" onclick="closeModal()">取消</button>');
  let r;
  try { r = await invoke('fetch_keyword_covers', { keyword: kw, baseUrl: trSrcUrl() || null }); }
  catch (e) { closeModal(); toast(String(e), 'err'); return; }
  renderCoverPicker(r.covers || [], r.status || [], '选择歌单封面',
    `applyPlaylistCoverConfirm('${pid}')`, '下载入库，仅作为歌单封面');
}
async function applyPlaylistCoverConfirm(pidEnc) {
  if (!selCover) return;
  applyPlaylistCover(pidEnc, selCover);
}
async function applyPlaylistCover(pidEnc, url) {
  const btn = document.getElementById('cvApply');
  if (btn) { btn.disabled = true; btn.textContent = '应用中…'; }
  try {
    const newPath = await invoke('set_playlist_cover', { pid: Number(pidEnc), url: url || null });
    selCover = null;
    closeModal();
    toast(newPath ? '歌单封面已应用' : '自定义封面已清除', 'ok');
    const p = L.playlists.find(x => x.id === Number(pidEnc));
    if (p) p.cover = newPath || null;
    if (state.view === 'playlist' && state.playlistId === Number(pidEnc)) loadPlaylistView();
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = '确认应用'; }
  }
}
async function applyAlbumCoverConfirm(keyEnc) {
  if (!selCover) return;
  const key = decodeURIComponent(keyEnc);
  const btn = document.getElementById('cvApply');
  if (btn) { btn.disabled = true; btn.textContent = '应用中…'; }
  try {
    const newPath = await invoke('apply_album_cover', { albumKey: key, url: selCover });
    closeModal();
    toast('封面已应用', 'ok');
    // 局部刷新：只换封面图，不动列表 DOM（保留滚动位置继续浏览）
    const a = L.albums.find(x => x.album_key === key);
    if (a) a.cover = newPath;
    // 属性值存的是 enc(key)，选择器必须用同款编码后的值匹配
    document.querySelectorAll(`[data-key="${enc(key)}"] .cover`).forEach(cv => {
      cv.classList.remove('flat');
      cv.innerHTML = `<img src="${convertFileSrc(newPath)}">`;
    });
    // 详情页遮罩/其它视图下次渲染自然更新
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = '确认应用'; }
  }
}

/* ================= M3U8 导入 ================= */
async function importM3U() {
  try {
    const r = await invoke('import_m3u8');
    toast(`已导入「${r.name}」：匹配 ${r.matched}/${r.total}${r.missing ? ` · 缺失 ${r.missing}（路径未命中曲库）` : ''}`, r.missing ? 'info' : 'ok');
    await refresh();
  } catch (e) { if (String(e) !== '未选择文件') toast(String(e), 'err'); }
}
async function loadScanRoots() {
  SCAN_ROOTS = await invoke('get_scan_roots');
}
function vSettings() {
  const TI = {
    providers: '<path d="M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z"/>',
    lyrics: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>',
    appearance: '<circle cx="13.5" cy="6.5" r=".5"/><circle cx="17.5" cy="10.5" r=".5"/><circle cx="8.5" cy="7.5" r=".5"/><circle cx="6.5" cy="12.5" r=".5"/><path d="M12 2C6.5 2 2 6.5 2 12s4.5 10 10 10c.926 0 1.648-.746 1.648-1.688 0-.437-.18-.835-.437-1.125-.29-.289-.438-.652-.438-1.125a1.64 1.64 0 0 1 1.668-1.668h1.996c3.051 0 5.555-2.503 5.555-5.554C21.965 6.012 17.461 2 12 2z"/>',
    playback: '<path d="M8 5v14l11-7z" fill="currentColor" stroke="none"/>',
    library: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
    hidden: '<path d="M9.88 4.24A9.12 9.12 0 0 1 12 4c7 0 10 8 10 8a13.16 13.16 0 0 1-1.67 2.68"/><path d="M6.61 6.61A13.526 13.526 0 0 0 2 12s3 8 10 8a9.74 9.74 0 0 0 5.39-1.61"/><line x1="2" x2="22" y1="2" y2="22"/>',
    about: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>',
    feature: '<path d="m12 2 3 7h7l-5.5 4.5L18.5 21 12 16.5 5.5 21l2-7.5L2 9h7z"/>',
  };
  // 单页设置：所有分区纵向平铺，左侧菜单仅做锚点跳转 + 滚动高亮（scrollspy）
  const SECS = [['library', '曲库'], ['playback', '播放'], ['feature', '功能开关'], ['appearance', '外观'], ['lyrics', '歌词'], ['providers', '联网增强'], ['hidden', '已隐藏'], ['about', '关于']];

  /* ---- 播放 ---- */
  const on = FADE_MS > 0;
  const cur = on ? (FADE_OPTS.includes(FADE_MS) ? FADE_MS : 10) : 10;
  const secPlayback = `<div class="set-card"><h3>音量渐入渐出</h3>
     <p class="desc">切歌时音量平滑过渡，消除爆音；关闭则硬切。</p>
     <div class="tgl"><div><div class="tt">启用渐入渐出</div>
      <div class="td">${on ? '当前 ' + FADE_MS + 'ms' : '已关闭'}</div></div>
      <label class="switch"><input type="checkbox" ${on ? 'checked' : ''} onchange="saveFade(this.checked,${cur})"><i></i></label></div>
     <div class="frow" style="${on ? '' : 'opacity:.45;pointer-events:none'}"><label>渐变时长</label>
      <select onchange="saveFade(true,parseInt(this.value))">
        ${FADE_OPTS.map(ms => `<option value="${ms}" ${on && FADE_MS === ms ? 'selected' : ''}>${ms} ms</option>`).join('')}
      </select></div></div>
    <div class="set-card"><h3>交叉淡入淡出（Crossfade）</h3>
     <p class="desc">曲尾前若干秒下一首渐入、当前曲渐出，重叠过渡；关闭则无缝衔接。与单曲循环互斥。</p>
     <div class="frow"><label>过渡时长</label>
      <select onchange="saveCrossfade(parseInt(this.value))">
        ${CF_OPTS.map(([ms, lab]) => `<option value="${ms}" ${CROSSFADE === ms ? 'selected' : ''}>${lab}</option>`).join('')}
      </select></div></div>
    <div class="set-card"><h3>ReplayGain 响度归一</h3>
     <p class="desc">按响度补偿音量，切歌不再忽大忽小。<b>播放时自动分析</b>；「标签写回」可把结果固化到音频文件。
     Track 适合歌单乱序，Album 适合整专辑聆听。</p>
     <div class="frow"><label>模式</label>
      <select onchange="saveRgMode(this.value)">
        ${[['off', '关闭'], ['track', 'Track（按单曲）'], ['album', 'Album（按专辑）']].map(([v, l]) =>
          `<option value="${v}" ${RG_MODE === v ? 'selected' : ''}>${l}</option>`).join('')}
      </select></div></div>`;

  /* ---- 外观 ---- */
  const mode = themeMode();
  const secAppearance = `<div class="set-card"><h3>主题</h3>
     <p class="desc">亮色 / 暗色 / 跟随系统。</p>
     <div class="frow"><label>模式</label>
      <select onchange="setTheme(this.value)">
        ${[['auto', '跟随系统'], ['light', '亮色'], ['dark', '暗色']].map(([v, l]) =>
          `<option value="${v}" ${mode === v ? 'selected' : ''}>${l}</option>`).join('')}
      </select>
      <button class="btn sm" onclick="toggleThemeQuick(this)">${mode === 'light' || (mode === 'auto' && !sysDark()) ? '🌙 切暗色' : '☀️ 切亮色'}</button></div></div>
    <div class="set-card"><h3>强调色</h3>
     <p class="desc">按钮、进度条、选中态等主题色。</p>
     <div class="accent-dots">
      ${ACCENTS.map(([c1, , name], i) =>
        `<div class="accent-dot ${themeAccent() === i ? 'on' : ''}" style="background:${c1}" title="${name}" onclick="setAccent(${i})"></div>`).join('')}
     </div></div>`;

  /* ---- 歌词 ---- */
  const secLyrics = `<div class="set-card"><h3>歌词显示</h3>
     <div class="tgl"><div><div class="tt">繁体转简体</div>
      <div class="td">标题、艺人、专辑名与歌词显示为简体，只影响显示不改动文件。</div></div>
      <label class="switch"><input type="checkbox" ${t2sEnabled() ? 'checked' : ''} onchange="setT2s(this.checked)"><i></i></label></div>
     <div class="tgl"><div><div class="tt">歌词优先级</div>
      <div class="td">外部同名 .lrc → 内嵌标签 → 在线缓存</div></div></div></div>`;

  /* ---- 在线服务（自定义源 + 各数据源） ---- */
  const CATS = ['歌词', '元数据', '封面图片', '歌手图片', '歌手信息'];
  const CAT_ICON = {
    '歌词': '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>',
    '元数据': '<path d="M4 6h16M4 12h16M4 18h10"/>',
    '封面图片': '<rect width="18" height="18" x="3" y="3" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/>',
    '歌手图片': '<circle cx="12" cy="8" r="4"/><path d="M6 21v-1a6 6 0 0 1 12 0v1"/>',
    '歌手信息': '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>',
  };
  const ready = p => !!p.on || (p.needKey && state.keys[p.id]);
  const card = p => {
    const k = p.needKey ? state.keys[p.id] : null;
    const badge = p.on ? '<span class="order-badge">已接入</span>'
      : k ? '<span class="order-badge">Key ✓</span>'
      : p.needKey ? '<span class="key-badge">需要 Key</span>'
      : '<span class="plan-badge">规划中</span>';
    return `<div class="prov ${ready(p) ? '' : 'off'}">
      <div class="r1"><span class="nm"><span class="dot"></span>${esc(p.nm)}</span>${badge}</div>
      <div class="caps">${(p.caps || []).map(c => `<span class="cap">${c}</span>`).join('')}</div>
      <div class="desc2">${esc(p.desc)}</div>
      <div class="meta"><span title="认证">${esc(p.auth)}</span><span class="sep">·</span><span title="限流">${esc(p.rate)}</span></div>
      <div class="foot"><span class="res" id="pr-${p.id}"></span>
       ${p.needKey ? `<button class="btn sm" onclick="openKeyModal('${p.id}','${esc(p.nm)}')">${k ? '管理 Key' : '配置 Key'}</button>` : ''}
       <button class="btn sm" onclick="testProv('${p.id}',this)" ${ready(p) ? '' : 'disabled'}>测试</button></div></div>`;
  };
  const secProviders = `<div class="set-card"><h3>自定义源（歌词 / 翻译 / 封面 三合一）</h3>
     <p class="desc">配置自建服务地址后<b>优先于</b>内置源使用；无结果自动回退，结果缓存本地断网可用。</p>
     <div style="display:flex;gap:8px;flex-wrap:wrap">
      <input id="trSrcInput" placeholder="http://127.0.0.1:3989" value="${esc(trSrcUrl())}" style="flex:1;min-width:240px;padding:8px 12px;border-radius:8px;border:1px solid var(--line2);background:var(--bg2);color:var(--tx0);font-size:13px;outline:none">
      <button class="btn sm" onclick="saveTrSrc()">保存</button>
      <button class="btn sm" onclick="testTrSrc()">测试连通</button>
     </div></div>
    <div class="set-card"><h3>数据源</h3>
     <p class="desc">按能力分类的在线数据源。</p>
     ${CATS.map(cat => {
      const ps = PROVIDERS.filter(p => (p.cat || []).includes(cat));
      if (!ps.length) return '';
      const onn = ps.filter(p => p.on).length;
      return `<div class="prov-group">
       <div class="pg-head"><svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${CAT_ICON[cat]}</svg>
        <b>${cat}</b><span class="pg-cnt">${onn ? onn + ' 已接入 · ' : ''}${ps.length} 个源</span>
        <span class="grow" style="flex:1"></span></div>
       ${ps.map(card).join('')}</div>`;
    }).join('')}</div>`;

  /* ---- 曲库 ---- */
  const secLibrary = `<div class="set-card"><h3>音乐目录</h3>
     <p class="desc">目录内的文件变动自动扫描入库（约 2 秒延迟），未变动的文件增量跳过。</p>
     ${SCAN_ROOTS.length ? SCAN_ROOTS.map(r => `<div class="lf-row" style="border-color:var(--line)">
       <span class="t" style="font-family:ui-monospace,monospace;font-size:12px">${esc(r)}</span>
       <button class="btn sm" onclick="doRemoveRoot('${esc(r).replace(/'/g, '&#39;')}',this)">移除</button></div>`).join('')
      : '<div class="empty">尚未配置目录 · 用侧栏「添加音乐目录」</div>'}
     <button class="btn pri" style="margin-top:12px" onclick="doScan()">＋ 添加音乐目录</button></div>
    <div class="set-card"><h3>音频缓存</h3>
     <p class="desc">首次播放自动缓存到内置盘，再次播放明显提速（外置盘 / 网络盘曲库尤其有效）；超出上限按最久未播清理。</p>

     <div class="tgl"><div><div class="tt">启用缓存</div>
      <div class="td">当前占用 <b>${fmtMB(CACHE_INFO.used_mb)}</b> · 与曲库同盘时自动跳过</div></div>
      <label class="switch"><input type="checkbox" ${CACHE_INFO.enabled ? 'checked' : ''} onchange="saveCacheCfg(this.checked,${CACHE_INFO.limit_mb})"><i></i></label></div>
     <div class="frow"><label>缓存上限</label>
      <select onchange="saveCacheCfg(${CACHE_INFO.enabled ? 'true' : 'false'},parseInt(this.value))">
        ${CACHE_OPTS.map(mb => `<option value="${mb}" ${CACHE_INFO.limit_mb === mb ? 'selected' : ''}>${mb === 0 ? '不限制' : fmtMB(mb)}</option>`).join('')}
      </select>
      <button class="btn sm" onclick="clearCache(this)">清理缓存</button></div></div>
    <div class="set-card"><h3>备份与恢复</h3>
     <p class="desc">备份包含曲库、播放会话、设置与封面；恢复会覆盖当前数据，完成后需重启应用。</p>
     <div class="frow">
      <button class="btn" onclick="doBackup(this)">备份</button>
      <button class="btn" onclick="doRestore(this)">恢复…</button>
     </div>
     <div class="abk">
      <div class="abk-head" onclick="toggleAutoBk()">自动备份（<span id="abkCount">${AUTO_BK.length}/10</span>）<span class="abk-tg">${autoBkOpen ? '收起 ▴' : '展开 ▾'}</span></div>
      <div class="abk-desc">封面 / 歌手 / 歌词 / 翻译获取成功后自动快照，恢复即回到该时刻</div>
      <div id="autoBkList" style="display:${autoBkOpen ? '' : 'none'}">${autoBkRows()}</div>
     </div></div>
    <div class="set-card"><h3>标签写回</h3>
     <p class="desc">把在线补全的元数据写回音频文件，换播放器或重装后仍保留；只写文本字段，不动封面与音频。当前 <b>${vSettingsWB().length}</b> 首待写回。</p>
     <div class="frow">
      <button class="btn" onclick="doWritebackAll(this)">写回全部补全结果</button>
     </div>
     <div class="abk">
      <div class="abk-head" onclick="toggleWbList()">待写回列表（${vSettingsWB().length}）<span class="abk-tg">${wbOpen ? '收起 ▴' : '展开 ▾'}</span></div>
      <div id="wbList" style="display:${wbOpen ? '' : 'none'};max-height:300px;overflow-y:auto">${vSettingsWBRows()}</div>
     </div></div>
    <div class="set-card"><h3>封面缓存清理</h3>
     <p class="desc">删除封面缓存目录中已不被曲库引用的孤儿文件（换封面/删曲目后残留）。</p>
     <div class="frow">
      <button class="btn" onclick="doCleanCovers(this)">清理未引用封面</button>
     </div></div>`;

  /* ---- 已隐藏（默认折叠，展开时加载） ---- */
  const secHidden = `<div class="set-card"><h3>已隐藏歌曲</h3>
     <div class="abk" style="margin-top:0;border-top:none;padding-top:0">
      <div class="abk-head" id="hidHead" onclick="toggleHiddenList()">隐藏列表（<span id="hidCount">${HID_N ?? '…'}</span>）<span class="abk-tg">${hiddenOpen ? '收起 ▴' : '展开 ▾'}</span></div>
      <div class="abk-desc">隐藏的曲目不出现在曲目、专辑、搜索与播放中；点击行试听，可恢复</div>
      <div id="hiddenList" style="display:${hiddenOpen ? '' : 'none'}"><p class="desc">加载中…</p></div>
     </div></div>`;

  /* ---- 关于 ---- */
  const secAbout = `<div class="about-hero">
      <img src="icon.png" alt="好好听歌">
      <h2>好好听歌</h2>
      <div class="ver">版本 ${esc(state.version)}</div>
      <p>本地优先的桌面音乐播放器<br>播放与曲库管理完全离线可用，联网只用于可选的歌词、封面与元数据补全</p>
     </div>`;

  /* ---- 功能（播放条可选功能开关，默认关闭） ---- */
  const featRow = (k, name, desc, usage) => `<div class="tgl"><div><div class="tt">${name}</div><div class="td">${desc}</div></div>
    <label class="switch"><input type="checkbox" ${featOn(k) ? 'checked' : ''} onchange="toggleFeat('${k}',this.checked)"><i></i></label></div>
    ${usage ? `<div class="feat-usage">${usage}</div>` : ''}`;
  const secFeature = `<div class="set-card">
     ${featRow('speed', '倍速播放', '0.5–3.0x 气泡调节（音调随速度变化），适合听播客/课程。')}
     ${featRow('sleep', '睡眠定时', '15–120 分钟到点自动暂停，适合睡前听。')}
     ${featRow('ab', 'A-B 循环', '进度条设 A/B 两点区间循环，适合练歌/学外语。',
       '用法：播放条点「A‑B」第一次设起点（黄色竖线），第二次设终点并开始循环（区间高亮），第三次清除；切歌自动清除。')}
     <div class="tgl"><div><div class="tt">后台播放</div>
      <div class="td">开启后关闭窗口最小化到后台继续播放（托盘菜单 / 媒体键可控，点 Dock 图标恢复窗口）；关闭则关窗即退出。</div></div>
      <label class="switch"><input type="checkbox" ${BGPLAY ? 'checked' : ''} onchange="toggleBgPlay(this.checked)"><i></i></label></div>
     </div>`;

  const secs = { playback: secPlayback, appearance: secAppearance, lyrics: secLyrics, providers: secProviders, library: secLibrary, feature: secFeature, hidden: secHidden, about: secAbout };
  return `<div class="set-page">
   <aside class="set-nav">${SECS.map(([k, n]) => `<div class="set-nav-item" data-sec="${k}" onclick="setNavTo('${k}')"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${TI[k]}</svg><span>${n}</span></div>`).join('')}</aside>
   <div class="set-body" id="setBody">${SECS.map(([k, n], i) => `<div class="set-sec" data-sec="${k}"><div class="set-sec-title">${n}</div>${secs[k]}</div>`).join('')}</div></div>`;
}
// 锚点跳转：滚动到分区（smooth），左菜单高亮同步
function setNavTo(k) {
  const body = document.getElementById('setBody');
  const sec = body?.querySelector(`.set-sec[data-sec="${k}"]`);
  if (body && sec) body.scrollTo({ top: Math.max(0, sec.offsetTop - 12), behavior: 'smooth' });
  document.querySelectorAll('.set-nav-item').forEach(x => x.classList.toggle('on', x.dataset.sec === k));
}
// scrollspy：内容区滚动时高亮左侧菜单当前分区（rAF 节流）
function bindSetSpy() {
  const body = document.getElementById('setBody');
  if (!body) return;
  document.querySelector('.set-nav-item')?.classList.add('on');
  let raf = 0;
  body.addEventListener('scroll', () => {
    if (raf) return;
    raf = requestAnimationFrame(() => {
      raf = 0;
      let cur = null;
      body.querySelectorAll('.set-sec').forEach(s => { if (s.offsetTop - 70 <= body.scrollTop) cur = s.dataset.sec; });
      if (cur) document.querySelectorAll('.set-nav-item').forEach(x => x.classList.toggle('on', x.dataset.sec === cur));
    });
  }, { passive: true });
}
function openKeyModal(id, nm) {
  const cur = state.keys[id] || '';
  const link = (u, t) => `<a href="#" onclick="event.preventDefault();invoke('open_external',{url:'${u}'})" style="color:var(--accent)">${t} ↗</a>`;
  const hint = {
    lastfm: link('https://www.last.fm/api/account/create', '在 last.fm 免费申请'),
    fanart: link('https://fanart.tv/register', '在 fanart.tv 免费申请 personal API key'),
    discogs: link('https://www.discogs.com/settings/developers', '在 Discogs 免费生成 personal access token'),
    acoustid: link('https://acoustid.org/api-key', '在 AcoustID 免费申请'),
    theaudiodb: link('https://www.patreon.com/theaudiodb', 'Patreon 赞护获取 API Key（$6/月，行业标准源）'),
  }[id] || '';
  modal('配置 ' + nm + ' 凭证',
    `<div class="field" style="margin-bottom:8px"><label style="font-size:12px;color:var(--tx2)">API Key / Token</label>
     <input id="keyInput" value="${esc(cur)}" placeholder="粘贴 key" style="width:100%;padding:8px 12px;border-radius:8px;border:1px solid var(--line2);background:var(--bg2);color:var(--tx0);outline:none"></div>
     <p class="muted" style="font-size:11.5px">${hint}</p>`,
    `<button class="btn pri" onclick="saveKey('${id}')">保存</button>
     ${cur ? `<button class="btn" onclick="saveKey('${id}',true)">清除</button>` : ''}
     <button class="btn" onclick="closeModal()">取消</button>`, true);
}
async function saveKey(id, clear) {
  const v = clear ? '' : (document.getElementById('keyInput').value || '').trim();
  try {
    await invoke('set_key', { id, key: v });
    state.keys = (await invoke('get_keys')) || {};
    closeModal();
    toast(v ? 'Key 已保存' : 'Key 已清除', 'ok');
    render();
  } catch (e) { toast(String(e), 'err'); }
}
async function testProv(id, btn) {
  btn.disabled = true; const old = btn.textContent;
  btn.innerHTML = '<span class="spin"></span> 测试中';
  const el = document.getElementById('pr-' + id);
  try {
    const r = await invoke('test_provider', { id });
    el.innerHTML = `<b class="ok">HTTP ${r.status}</b> · <b>${r.latencyMs}ms</b> · 命中 <b>${r.hits}</b> 条`;
  } catch (e) {
    el.innerHTML = `<b style="color:var(--err)">失败</b> · ${esc(String(e))}`;
  }
  btn.disabled = false; btn.textContent = old;
}

/* ================= 播放 ================= */
async function playList(ids, start) {
  state.queue = ids.map(trackById).filter(Boolean); // 队列快照（前端持有，事件驱动 index）
  try { await invoke('play_tracks', { ids, start }); }
  catch (e) { toast(String(e), 'err'); }
  lastQSig = ''; renderQueueList();
}
function playFromView(id) {
  const ts = L.tracks.filter(hit);
  // 上下文：当前视图顺序（专辑视图按专辑内排序）
  let ctx;
  if (state.view === 'album') ctx = albumTracks(state.albumKey);
  else if (state.view === 'playlist') ctx = (L.playlists.find(p => p.id === state.playlistId) || {})._tracks || [trackById(id)];
  else if (state.view === 'dups') ctx = (DUPS || []).flatMap(g => g.items.map(x => trackById(x.id))).filter(Boolean);
  else ctx = ts;
  if (!ctx.some(t => t.id === id)) ctx = [trackById(id)];
  const ids = ctx.map(t => t.id);
  playList(ids, Math.max(0, ids.indexOf(id)));
}
function clearQueue() { invoke('set_shuffle', { on: false }); toast('提示：右键曲目可从队列上下文播放（演示）', 'info'); }
async function uiShuffle() {
  // 只有两档：关 = 顺序播放队列；开 = 所有歌曲随机（全库洗牌重建队列）
  await setShuffleMode(state.shuffleMode > 0 ? 0 : 1);
}
async function setShuffleMode(m) {
  m = m > 0 ? 1 : 0;
  state.shuffleMode = m;
  state.shuffle = m > 0;
  syncShuffleBtn(m);
  try { await invoke('set_shuffle_mode', { mode: m }); } catch (e) { toast(String(e), 'err'); }
  toast(m ? '随机播放：开 · 所有歌曲随机' : '随机播放：关 · 按队列顺序', 'info');
  renderQueueList(); // 队列被重建/还原，立刻刷新面板
}
function syncShuffleBtn(m) {
  btnShuffle.classList.toggle('on', m > 0);
  btnShuffle.title = m > 0 ? '随机播放：开（点击关闭）' : '随机播放：关（点击开启）';
}
function closeModalPop() { menuRoot.style.display = 'none'; }
const REPEAT_TXT = ['循环：关（播完停止）', '循环：列表', '循环：单曲'];
function uiRepeat() {
  state.repeat = ((state.repeat ?? 1) + 1) % 3;
  invoke('set_repeat', { mode: state.repeat });
  syncRepeatBtn();
  toast(REPEAT_TXT[state.repeat], 'info');
}
function syncRepeatBtn() {
  const r = state.repeat ?? 1;
  btnRepeat.classList.toggle('on', r !== 0);
  btnRepeat.classList.toggle('one', r === 2);
  btnRepeat.style.color = r === 2 ? 'var(--accent)' : '';
  btnRepeat.title = REPEAT_TXT[r];
}
let volTimer = null;
let lastVol = 0.8; // 静音前音量（恢复用）
const volSvg = v =>
  v <= 0.001
    ? '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="m23 9-6 6"/><path d="m17 9 6 6"/>'
    : v < 0.5
      ? '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/>'
      : '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a9 9 0 0 1 0 14"/>';
function updateVolIcon(v) {
  const el = document.getElementById('volIcon');
  if (el) el.innerHTML = volSvg(v);
  btnVol.classList.toggle('off', v <= 0.001);
}
/// 音量气泡：悬停喇叭图标向上弹出（竖向滑条 + 数值 + 静音切换）；点击图标=静音切换
let volPopOpen = false, volPopTimer = null;
function uiVolPop() {
  const v = Math.round((state.volume ?? 0.8) * 100);
  popAbove(btnVol, `
    <div class="menu-h" style="justify-content:center;text-align:center;padding:10px 10px 0"><b id="volVal" style="display:inline-block;min-width:46px;font-variant-numeric:tabular-nums;text-align:center">${v}</b></div>
    <div style="display:flex;flex-direction:column;align-items:center;padding:8px 10px 10px">
      <input type="range" min="0" max="100" step="1" value="${v}" id="volSlider"
        style="-webkit-appearance:slider-vertical;writing-mode:vertical-lr;direction:rtl;width:22px;height:130px;accent-color:var(--accent)"
        oninput="setVolVal(this.value)">
    </div>`);
  volPopOpen = true;
}
// 悬停弹出 / 移开延时关闭（移入气泡取消关闭）
btnVol.addEventListener('mouseenter', () => { clearTimeout(volPopTimer); uiVolPop(); });
btnVol.addEventListener('mouseleave', () => {
  volPopTimer = setTimeout(() => { if (volPopOpen) closeMenu(); }, 250);
});
menuRoot.addEventListener('mouseenter', () => clearTimeout(volPopTimer));
menuRoot.addEventListener('mouseleave', () => {
  if (volPopOpen) volPopTimer = setTimeout(() => closeMenu(), 200);
});
{ // closeMenu 时清除气泡标记
  const _origClose = closeMenu;
  closeMenu = function () { volPopOpen = false; _origClose(); };
}
function setVolVal(pct) {
  state.volume = pct / 100;
  const lab = document.getElementById('volVal');
  if (lab) lab.textContent = pct;
  updateVolIcon(state.volume);
  clearTimeout(volTimer);
  volTimer = setTimeout(() => invoke('set_volume', { v: state.volume }), 80);
}
function toggleMute() {
  if ((state.volume ?? 0.8) > 0.001) {
    lastVol = state.volume;
    state.volume = 0;
  } else {
    state.volume = lastVol > 0.001 ? lastVol : 0.8;
  }
  invoke('set_volume', { v: state.volume });
  updateVolIcon(state.volume);
  uiVolPop(); // 刷新气泡内容
}
function seekClk(ev) {
  if (!state.dur) return;
  const r = ev.currentTarget.getBoundingClientRect();
  const sec = state.dur * Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width));
  invoke('seek', { sec });
}

/* ================= 底栏 / 队列 / 歌词 ================= */
let lastTrackId = null;
let lastNpId = null, lastPlayBtn = '', lastQSig = '¬', lastQPullAt = 0;
/* 轻量同步曲目行"译"徽标：预览/应用确认翻译后更新前端内存与 DOM，
   不触发整库 refresh（后端写库不会自动反映到前端 L.tracks 缓存） */
function syncTrBadge(tid, has) {
  if (tid == null) return;
  const t = trackById(tid);
  if (t) t.has_tr = has;
  document.querySelectorAll(`.trk[data-tid="${tid}"] .ti .t`).forEach(el => {
    const b = el.querySelector('.b-tr');
    if (has && !b) el.insertAdjacentHTML('beforeend', ' <span class="badge b-tr">译</span>');
    else if (!has && b) b.remove();
  });
}
/* 轻量同步曲目行"词"徽标：应用歌词后局部更新（同 syncTrBadge 思路，不动滚动位置） */
function syncLyricsBadge(tid, has) {
  if (tid == null) return;
  const t = trackById(tid);
  if (t) t.has_lyrics = has;
  document.querySelectorAll(`.trk[data-tid="${tid}"] .ti .t`).forEach(el => {
    const b = el.querySelector('.b-lyr');
    if (has && !b) el.insertAdjacentHTML('beforeend', ' <span class="badge b-online b-lyr">词</span>');
    else if (!has && b) b.remove();
  });
}

/* 切歌时迁移曲目行“正在播放”高亮：只动 class，不整页重绘——
   全量 render 会销毁重建主视图（滚动回顶、封面图重载闪烁） */
function updateNowRows() {
  view.querySelectorAll('.trk.playing').forEach(r => r.classList.remove('playing'));
  if (state.curId != null) {
    view.querySelectorAll(`.trk[data-tid="${state.curId}"]`).forEach(r => r.classList.add('playing'));
  }
}
function syncBar() {
  // 队列可能被随机重建，state.queue 只在队列面板打开时同步 → 当前曲以后端推送为准
  const t = trackById(state.curId) || state.curTrack;
  // 封面/曲名只在曲目变化时重绘：500ms 重写 <img> 会不断销毁重建导致闪烁
  const nid = t ? t.id : null;
  if (nid !== lastNpId) {
    lastNpId = nid;
    npCover.innerHTML = t ? cover(t.cover, t.album, 56, 8) : cover(null, '好好听歌', 56, 8);
    npTitle.textContent = t ? t.title : '—';
    npArtist.textContent = t ? `${fmtArtists(t.artist)} · ${t.album}` : '';
  }
  tCur.textContent = fmt(state.t); tDur.textContent = fmt(state.dur);
  const pct = state.dur ? state.t / state.dur * 100 : 0;
  progFill.style.width = pct + '%'; progDot.style.left = pct + '%';
  const btnSvg = state.playing ? 'pause' : 'play';
  if (btnSvg !== lastPlayBtn) {
    lastPlayBtn = btnSvg;
    btnPlay.innerHTML = state.playing
      ? '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M7 5h4v14H7zM13 5h4v14h-4z"/></svg>'
      : '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7z"/></svg>';
  }
  btnShuffle.classList.toggle('on', state.shuffleMode > 0);
  if (lyrics.classList.contains('open')) updateVinylArm();
  renderQueueList();
  if (state.curId !== lastTrackId) {
    const changed = lastTrackId !== null;
    lastTrackId = state.curId;
    if (changed) {
      // enrich 卡片的试听状态内嵌样式复杂，保持整绘；其余视图仅迁移行高亮
      if (state.view === 'enrich') render();
      else updateNowRows();
    }
    if (lyrics.classList.contains('open')) refreshLyricsView();
  }
}
async function renderQueueList(force = false) {
  // 队列以后端为唯一真源；面板收起时不拉取（全库随机队列可达数千首，拉取有代价）
  if (!drawer.classList.contains('open')) return;
  // 专辑 tab：不拉队列，直接刷专辑列表（纯前端数据，切歌高亮跟随）
  if (drawerTab === 'album') { renderAlbumList(); return; }
  const now = Date.now();
  if (!force && now - lastQPullAt < 1200) return; // 拉取节流：500ms 心跳不必每次都问后端
  lastQPullAt = now;
  let v;
  try { v = await invoke('queue_list'); } catch { return; }
  const q = v.tracks || [], idx = v.index ?? -1;
  // 签名不含播放状态：暂停/继续不重绘（封面闪烁），动画随下次队列/切歌重绘纠正
  const sig = q.map(x => x.id).join(',') + '|' + idx;
  if (sig === lastQSig) return;
  lastQSig = sig;
  state.queue = q; state.queueIndex = idx; // 本地镜像（当前曲显示等处复用）
  qCount.textContent = q.length ? `· ${q.length} 首` : '';
  // 全库随机队列可达数千首，DOM 只渲染前 600 行（索引仍与后端一致）
  const MAX_ROWS = 600;
  const shown = q.length > MAX_ROWS ? q.slice(0, MAX_ROWS) : q;
  qList.innerHTML = q.length ? shown.map((t, k) => `
    <div class="q-item ${k === idx ? 'now' : ''}" onclick="playQueueAt(${k})">
      ${cover(t.cover, t.album, 40, 6)}<div><div class="t">${esc(t.title)}${k === idx && state.playing ? ' <span class="eq" style="display:inline-flex"><i></i><i></i></span>' : ''}</div>
      <div class="a">${esc(t.artist)}</div></div>
      ${k === idx ? '' : `<button class="rm" title="从队列移除" onclick="event.stopPropagation();removeQueueAt(${k})">✕</button>`}</div>`).join('')
      + (q.length > shown.length ? `<div class="empty" style="padding:14px 10px">仅显示前 ${shown.length} 首 · 共 ${q.length} 首</div>` : '')
    : '<div class="empty" style="padding:40px 10px">队列为空<br><span class="muted">播放专辑或歌单生成队列</span></div>';
  // 自动定位：当前曲滚到列表中部（渲染截断 600 行内才可见）
  const nowEl = qList.querySelector('.q-item.now');
  if (nowEl) qList.scrollTop = Math.max(0, nowEl.offsetTop - qList.clientHeight / 2 + nowEl.offsetHeight / 2);
}
async function playQueueAt(k) { try { await invoke('queue_jump', { index: k }); } catch (e) { toast(String(e), 'err'); } }
async function removeQueueAt(k) { try { await invoke('queue_remove', { index: k }); renderQueueList(true); } catch (e) { toast(String(e), 'err'); } }
function toggleWiki() {
  const w = document.getElementById('ahWiki');
  const tg = document.getElementById('wikiTg');
  if (!w) return;
  const open = w.classList.toggle('full');
  if (tg) { tg.textContent = open ? '收起 ▴' : '展开 ▾'; tg.style.display = ''; }
}
function toggleDrawer() {
  const open = drawer.classList.contains('open');
  if (open) { drawer.classList.remove('open'); btnQueue.classList.remove('on'); }
  else { drawer.classList.add('open'); btnQueue.classList.add('on'); renderQueueList(true).then(scrollDrawerToNow); }
}
// 抽屉外点击收起：不挡任何交互（无实体遮罩层），弹窗/菜单/toast 内的点击不触发
document.addEventListener('click', ev => {
  if (!drawer.classList.contains('open')) return;
  for (const id of ['drawer', 'btnQueue', 'modalRoot', 'menuRoot', 'toastRoot']) {
    const el = document.getElementById(id);
    if (el && el.contains(ev.target)) return;
  }
  drawer.classList.remove('open');
  btnQueue.classList.remove('on');
});
/* ---- 抽屉双 tab：播放队列 / 当前专辑 ---- */
let drawerTab = 'queue';
// 当前 tab 列表滚到当前曲中部（打开抽屉 / 切 tab / 重绘后调用）
function scrollDrawerToNow() {
  const el = drawerTab === 'album' ? document.getElementById('aList') : document.getElementById('qList');
  const nowEl = el?.querySelector('.q-item.now');
  if (el && nowEl) el.scrollTop = Math.max(0, nowEl.offsetTop - el.clientHeight / 2 + nowEl.offsetHeight / 2);
}
function switchDrawerTab(t) {
  drawerTab = t;
  document.getElementById('qTabQueue')?.classList.toggle('on', t === 'queue');
  document.getElementById('qTabAlbum')?.classList.toggle('on', t === 'album');
  document.getElementById('qList').style.display = t === 'queue' ? '' : 'none';
  document.getElementById('aList').style.display = t === 'album' ? '' : 'none';
  document.getElementById('qClearBtn').style.display = t === 'queue' ? '' : 'none';
  if (t === 'queue') renderQueueList(true).then(scrollDrawerToNow);
  else { renderAlbumList(); scrollDrawerToNow(); }
}
// 当前曲所在专辑曲目（前端数据，disc/track 序）；点击以专辑为上下文播放。
// 签名变化检测（不含播放状态：暂停/继续不重绘，避免封面闪烁）；重绘后当前曲定位到列表中部
let lastAlbumSig = '¬';
function renderAlbumList() {
  const el = document.getElementById('aList');
  if (!el || !drawer.classList.contains('open')) return;
  const cur = trackById(state.curId) || state.curTrack;
  const sig = cur ? `${cur.album_key}|${cur.id}` : '';
  if (sig === lastAlbumSig) return;
  lastAlbumSig = sig;
  if (!cur) { qCount.textContent = ''; el.innerHTML = '<div class="empty" style="padding:40px 10px">未在播放</div>'; return; }
  const ts = albumTracks(cur.album_key);
  if (!ts.length) { qCount.textContent = ''; el.innerHTML = '<div class="empty" style="padding:40px 10px">当前曲目不在专辑中</div>'; return; }
  const key = cur.album_key;
  qCount.textContent = `· ${ts.length} 首`;
  el.innerHTML = ts.map(t => `
    <div class="q-item a-item ${t.id === cur.id ? 'now' : ''}" onclick="playAlbumFrom('${enc(key)}',${t.id})" title="${esc(t.title)}">
      ${cover(t.cover, t.album, 40, 6)}
      <div><div class="t">${t.track_no ? t.track_no + '. ' : ''}${esc(t.title)}${t.id === cur.id && state.playing ? ' <span class="eq" style="display:inline-flex"><i></i><i></i></span>' : ''}</div>
      <div class="a">${esc(t.artist)}</div></div>
      <span class="dur muted">${fmt(t.duration)}</span></div>`).join('');
  // 自动定位：当前曲滚到列表中部（offsetTop 需要 .list 为 offsetParent）
  const nowEl = el.querySelector('.q-item.now');
  if (nowEl) el.scrollTop = Math.max(0, nowEl.offsetTop - el.clientHeight / 2 + nowEl.offsetHeight / 2);
}
async function playAlbumFrom(albumKeyEnc, tid) {
  const ids = albumTracks(decodeURIComponent(albumKeyEnc)).map(t => t.id);
  if (ids.length) playList(ids, Math.max(0, ids.indexOf(tid)));
}

// 唱针角度：暂停抬起 35°；播放中随进度从外圈(-25°)滑向内圈(15°)
function updateVinylArm() {
  const arm = document.getElementById('vinylArm');
  if (!arm) return;
  if (!state.playing) { arm.style.transform = ''; return; }
  const p = state.dur ? Math.min(1, state.t / state.dur) : 0;
  arm.style.transform = `rotate(${-25 + 40 * p}deg)`;
}
async function toggleLyrics() {
  lyrics.classList.toggle('open');
  btnLyrics.classList.toggle('on', lyrics.classList.contains('open'));
  if (lyrics.classList.contains('open')) await refreshLyricsView();
}
async function refreshLyricsView() {
  if (!state.curId) { lyLines.classList.add('empty-mode'); lyLines.innerHTML = '<div class="ly-empty"><div class="t">未在播放</div></div>'; return; }
  const doc = await invoke('get_lyrics', { trackId: state.curId, t2s: t2sEnabled() });
  state.lyric = doc;
  // 预览即同步列表"译"徽标（get_lyrics 命令已按真实判定写库，这里同步前端内存+DOM）
  syncTrBadge(state.curId, (doc.source || '').includes('双语'));
  const t = trackById(state.curId) || state.curTrack || {};
  lyTitle.textContent = t.title || '';
  lyArtist.textContent = [t.album && t.album !== '未命名专辑' ? t.album : null, t.artist].filter(Boolean).join(' · ');
  vinylCover.innerHTML = t.cover
    ? `<img src="${convertFileSrc(t.cover)}">`
    : `<div class="init" style="${cstyle(t.album || t.title)}">${esc((t.album || t.title || '?')[0])}</div>`;
  // 旋转/唱针随播放状态
  lyrics.classList.toggle('playing', !!state.playing);
  lyBg.style.cssText = cstyle(t.album || 'Lyra') + ';position:absolute;right:-140px;top:-140px;width:520px;height:520px;border-radius:50%;filter:blur(130px);opacity:.35';
  // 有歌词、无翻译、已配置自定义翻译源 → 末尾提供获取入口
  if (doc.lines.length && doc.lines.every(x => !x.tr) && trSrcUrl()) {
    lyLines.insertAdjacentHTML('beforeend',
      `<div class="ly-line" style="margin-top:30px"><button class="btn" onclick="fetchTranslation(${state.curId},this)">✦ 获取中文翻译（自定义源）</button></div>`);
  }
  // 偏移校准：默认折叠，点击 chip 展开
  if (doc.synced && doc.lines.length) {
    renderLyOffBox();
  } else {
    lyOffBox.innerHTML = '';
  }
  lyLines.classList.toggle('empty-mode', !doc.lines.length);
  lyLastActive = -1;
  lyLines.innerHTML = doc.lines.length
    ? doc.lines.map((x, i) => `<div class="ly-line${doc.synced ? '' : ' active'}" id="ll${i}">${esc(x.l) || '&nbsp;'}${x.tr ? `<span class="tr">${esc(x.tr)}</span>` : ''}${doc.synced ? `<span class="ly-seek"><i>${fmt(x.t)}</i><button onclick="event.stopPropagation();seekLyLine(${i})" title="跳到此处播放"><svg viewBox="0 0 24 24" width="11" height="11" fill="currentColor"><polygon points="6 3 20 12 6 21 6 3"/></svg></button></span>` : ''}</div>`).join('')
    : `<div class="ly-empty"><div class="t">暂无歌词</div>
       <button class="btn" onclick="fetchOneLyrics(${state.curId},this).then(refreshLyricsView)"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" style="margin-right:6px;flex:none"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>获取歌词</button></div>`;
  updateLyricHighlight(true);
}
/* 手动滚动状态：滚动后 3s 内不自动跟随，期间高亮行右侧显示时间+跳播按钮 */
let lyUserUntil = 0, lyUserTimer = null, lyAutoScrolling = false, lyLastActive = -1, lyPreviewRaf = 0;
function lyUserScroll() {
  if (!state.lyric || !state.lyric.synced || !state.lyric.lines.length) return;
  lyUserUntil = Date.now() + 3000;
  lyLines.classList.add('user-scrolling');
  // rAF 节流：滚动事件高频触发，合并到下一帧，避免行边界来回切换闪烁
  cancelAnimationFrame(lyPreviewRaf);
  lyPreviewRaf = requestAnimationFrame(lyPreview);
  clearTimeout(lyUserTimer);
  lyUserTimer = setTimeout(() => {
    lyLines.classList.remove('user-scrolling');
    lyUserUntil = 0;
    lyLastActive = -2;          // 强制视为变化：恢复播放行高亮并滚回
    updateLyricHighlight();
  }, 3000);
}
// 视口第 4 行的歌词作为预览高亮（右侧显示该行时间+跳播）
function lyPreview() {
  const first = lyLines.querySelector('.ly-line');
  if (!first) return;
  const y = lyLines.scrollTop + first.offsetHeight * 3.5;
  let c = 0;
  document.querySelectorAll('.ly-line').forEach((el, i) => { if (el.offsetTop + el.offsetHeight / 2 <= y) c = i; });
  document.querySelectorAll('.ly-line').forEach((el, i) => el.classList.toggle('active', i === c));
}
lyLines.addEventListener('wheel', lyUserScroll, {passive:true});
lyLines.addEventListener('scroll', () => { if (!lyAutoScrolling) lyUserScroll(); });
function seekLyLine(i) {
  const ln = state.lyric && state.lyric.lines[i];
  if (!ln) return;
  invoke('seek', { sec: ln.t });
  lyUserUntil = 0;
  lyLines.classList.remove('user-scrolling');
  clearTimeout(lyUserTimer);
  lyLastActive = -2;
  updateLyricHighlight();
}
function updateLyricHighlight(init) {
  if (!state.lyric || !state.lyric.synced) return;
  // 手动浏览态：高亮归预览逻辑管，播放高亮不抢（3s 超时后强制恢复）
  if (lyLines.classList.contains('user-scrolling')) return;
  const off = lyOff();
  let c = 0;
  for (let i = 0; i < state.lyric.lines.length; i++) if (state.t - off >= state.lyric.lines[i].t) c = i;
  document.querySelectorAll('.ly-line').forEach((el, i) => el.classList.toggle('active', i === c));
  if (c !== lyLastActive) {
    lyLastActive = c;
    const act = document.getElementById('ll' + c);
    // 手动滚动 3s 内不跟随；且仅高亮行变化时滚动（3s 后等下一句才回去）
    if (act && (init || Date.now() >= lyUserUntil)) {
      lyAutoScrolling = true;
      // 高亮行固定在可视区第 4 行（约 1/4 处）：下方可见更多后续歌词
      const lineH = act.offsetHeight || 49;
      lyLines.scrollTop = act.offsetTop - lineH * 3;
      setTimeout(() => lyAutoScrolling = false, 700);
    }
  }
}

/* ================= 曲目详情 ================= */
function openTrackDetail() {
  const t = trackById(state.curId) || state.curTrack;
  if (!t) return;
  const row = (k, v, src) => `<tr><td>${k}</td><td>${esc(v)}</td><td>${sbadge(src)}</td></tr>`;
  const f = (label, id, val, ph) => `<div class="field" style="margin-bottom:8px"><label style="font-size:12px;color:var(--tx2)">${label}</label>
    <input id="${id}" value="${esc(String(val ?? ''))}" placeholder="${ph}" style="width:100%;padding:7px 10px;border-radius:8px;border:1px solid var(--line2);background:var(--bg2);color:var(--tx0);outline:none"></div>`;
  modal('曲目详情 · 编辑',
    `${f('标题', 'edTitle', t.title, '标题')}
      ${f('艺人', 'edArtist', t.artist, '艺人')}
      ${f('专辑', 'edAlbum', t.album === '未命名专辑' ? '' : t.album, '专辑名')}
      <div style="display:flex;gap:10px">
        ${f('年份', 'edYear', t.year ?? '', '2001').replace('width:100%', 'flex:1')}
        ${f('Track', 'edTrack', t.track_no || '', '3').replace('width:100%', 'flex:1')}
      </div>
      <p class="muted" style="margin-top:6px;font-size:12px">来源：${sbadge(t.source)} · 留空/不改的字段保持原值</p>
      <p class="muted" style="margin-top:4px;font-size:11px">文件：${esc(t.file_path || '')}</p>`,
    `<button class="btn" onclick="metaSearch(${t.id})">元数据</button>
     <span style="flex:1"></span>
     <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--tx1);cursor:pointer;user-select:none" title="保存同时把修改写回音频文件标签">
       <input type="checkbox" id="edWriteback" ${localStorage.getItem('lyra.edWb') === '1' ? 'checked' : ''}
         onchange="localStorage.setItem('lyra.edWb', this.checked ? '1' : '0')" style="accent-color:var(--accent)">写回文件
     </label>
     <button class="btn pri" onclick="saveTrackFields(${t.id})">保存修改</button>
     <button class="btn" onclick="closeModal()">关闭</button>`, true);
}
// 在线元数据查询（规范 §1 三段式）
async function metaSearch(tid) {
  const t = trackById(tid) || {};
  modal('元数据查询 · ' + (t.title || ''), `<div class="lyr-loading"><span class="spin"></span>正在搜索：自定义源 / iTunes / Deezer…（约需数秒）</div>`,
    '<button class="btn" onclick="openTrackDetail()">返回</button>', true);
  let r;
  try { r = await invoke('meta_search', { trackId: tid, baseUrl: trSrcUrl() || null }); }
  catch (e) { toast(String(e), 'err'); openTrackDetail(); return; }
  window.__metaPick = { tid, sel: null, body: null };
  const groups = r.groups || [];
  const dur = (r.track || {}).duration || 0;
  let h = groups.map(g => {
    const items = g.items || [];
    const st = items.length ? `${items.length} 条` : (g.err ? '失败' : '无结果');
    // 失败原因直接展示（未配置 / 连接失败 / HTTP 码），避免用户猜"为什么没数据"
    const why = (!items.length && g.err) ? `<div class="muted" style="font-size:11px;margin:-2px 0 8px 12px">${esc(g.err)}</div>` : '';
    // 整组时长全不符：多为翻唱/Live 或本源未收录（下架），明确提示看其他源
    const allBad = items.length > 0 && items.every(x => x.dur_ok === false);
    const warn = allBad ? ' <span class="src-st bad">⚠ 全部时长不符（多为翻唱 / 本源未收录）</span>' : '';
    return `<div class="pack-src">${esc(g.src)} <span class="pg-cnt">${st}</span>${warn}</div>` + why +
      (items.length ? `<div class="cand-list">${items.map((x, i) => `
        <div class="cand-item cand-apply" id="mc-${i}" data-src="${esc(g.src)}" onclick="pickMeta(${i},'${esc(g.src)}',this)">
          <div class="ci-body">
            <div class="ci-main"><b>${esc(x.title)}</b> — ${esc(x.artist)}${x.album ? ' · ' + esc(x.album) : ''}</div>
            <div class="ci-sub">${x.duration ? fmt(x.duration) : ''}${x.dur_ok ? ' · <span class="src-st ok">时长匹配</span>' : ' · <span class="src-st bad">时长不符</span>'}${x.year ? ' · ' + x.year : ''}</div>
          </div>
          <button class="btn sm" onclick="applyMetaDirect(${i},'${esc(g.src)}',this,event)">应用</button>
        </div>`).join('')}</div>` : '');
  }).join('');
  h += `<p class="muted" style="font-size:11px;margin-top:8px">当前：${esc(t.title || '')} — ${esc(t.artist || '')}${dur ? ' · ' + fmt(dur) : ''} · 点击候选预览字段</p>`;
  h = `<div id="metaPreview" class="pack-text" style="min-height:40px;display:flex;align-items:center;justify-content:center;color:var(--tx2);font-size:12px">点击候选预览将应用的字段</div>` + h;
  window.__metaGroups = groups;
  modal('元数据查询 · ' + (t.title || ''), h,
    `<button class="btn" onclick="openTrackDetail()">返回</button>
     <button class="btn pri" id="metaApply" disabled onclick="applyMetaConfirm()">应用元数据</button>`, true);
}
function pickMeta(i, src, el) {
  const g = (window.__metaGroups || []).find(x => x.src === src);
  const item = g && g.items[i];
  if (!item) return;
  document.querySelectorAll('.cand-item').forEach(x => x.style.outline = '');
  if (el) el.style.outline = '2px solid var(--accent)';
  const pv = document.getElementById('metaPreview');
  const btn = document.getElementById('metaApply');
  if (btn) { btn.disabled = true; btn.textContent = '应用元数据'; }
  if (pv) pv.innerHTML = '<span class="spin"></span> 正在拉取字段…';
  const srcKey = src === 'iTunes' ? 'itunes' : src === 'Deezer' ? 'deezer' : 'custom';
  window.__metaPick.sel = { src: srcKey, item, srcLabel: src };
  // custom 需补拉年份/track；itunes/deezer 候选已含
  const p = srcKey === 'custom'
    ? invoke('meta_fetch_item', { src: srcKey, id: String(item.id), baseUrl: trSrcUrl() || null }).catch(() => item)
    : Promise.resolve({ ...item, track_no: null });
  p.then(v => {
    window.__metaPick.body = v;
    const t = trackById(window.__metaPick.tid) || {};
    const row = (k, nv, cur) => `<div class="mr-row ${nv && String(nv) !== String(cur ?? '') ? 'diff' : ''}"><span class="k">${k}</span><span class="v">${esc(String(nv ?? ''))}</span><span class="old">${nv && String(nv) !== String(cur ?? '') ? '原：' + esc(String(cur ?? '')) : ''}</span></div>`;
    if (pv) pv.innerHTML = row('标题', v.title, t.title) + row('艺人', v.artist, t.artist) + row('专辑', v.album, t.album) + row('年份', v.year, t.year) + (v.track_no ? row('Track', v.track_no, t.track_no) : '');
    if (btn) btn.disabled = false;
  }).catch(e => { if (pv) pv.innerHTML = `<span style="color:var(--err)">${esc(String(e))}</span>`; });
}
// 候选行一键应用：跳过预览直接拉字段并应用（与 applyMetaConfirm 同一应用逻辑）
async function applyMetaDirect(i, src, btn, ev) {
  ev.stopPropagation();
  const g = (window.__metaGroups || []).find(x => x.src === src);
  const item = g && g.items[i];
  const tid = window.__metaPick?.tid;
  if (!item || tid == null || btn.disabled) return;
  btn.disabled = true; const old = btn.textContent; btn.textContent = '应用中…';
  const srcKey = src === 'iTunes' ? 'itunes' : src === 'Deezer' ? 'deezer' : 'custom';
  const v = srcKey === 'custom'
    ? await invoke('meta_fetch_item', { src: srcKey, id: String(item.id), baseUrl: trSrcUrl() || null }).catch(() => item)
    : { ...item, track_no: null };
  const t = trackById(tid) || {};
  const fields = {};
  for (const k of ['title', 'artist', 'album']) {
    if (v[k] && v[k] !== t[k]) fields[k] = v[k];
  }
  if (v.year && parseInt(v.year) !== t.year) fields.year = parseInt(v.year);
  if (v.track_no && parseInt(v.track_no) !== t.track_no) fields.track_no = parseInt(v.track_no);
  if (!Object.keys(fields).length) { toast('字段与当前一致，无需应用', 'info'); btn.disabled = false; btn.textContent = old; return; }
  try {
    await invoke('update_track_fields', { trackId: tid, fields });
    closeModal();
    toast('元数据已应用（' + Object.keys(fields).join('、') + '）', 'ok');
    await refresh();
    lastNpId = null;
    syncBar();
    if (lyrics.classList.contains('open')) refreshLyricsView();
  } catch (e) {
    toast(String(e), 'err');
    btn.disabled = false; btn.textContent = old;
  }
}
async function applyMetaConfirm() {
  const pk = window.__metaPick;
  if (!pk || !pk.sel || !pk.body) return;
  const btn = document.getElementById('metaApply');
  if (btn) { btn.disabled = true; btn.textContent = '应用中…'; }
  const v = pk.body;
  const t = trackById(pk.tid) || {};
  const fields = {};
  for (const k of ['title', 'artist', 'album']) {
    if (v[k] && v[k] !== t[k]) fields[k] = v[k];
  }
  if (v.year && parseInt(v.year) !== t.year) fields.year = parseInt(v.year);
  if (v.track_no && parseInt(v.track_no) !== t.track_no) fields.track_no = parseInt(v.track_no);
  if (!Object.keys(fields).length) { closeModal(); toast('字段与当前一致，无需应用', 'info'); return; }
  try {
    await invoke('update_track_fields', { trackId: pk.tid, fields });
    closeModal();
    toast('元数据已应用（' + Object.keys(fields).join('、') + '）', 'ok');
    await refresh();
    // 播放器信息刷新：底栏只按曲目 id 变化重绘，同曲改字段需强制重绘；歌词页开着同步刷新
    lastNpId = null;
    syncBar();
    if (lyrics.classList.contains('open')) refreshLyricsView();
  } catch (e) {
    toast(String(e), 'err');
    if (btn) { btn.disabled = false; btn.textContent = '应用元数据'; }
  }
}
async function saveTrackFields(tid) {
  const fields = {};
  const t = trackById(tid) || {};
  const cur = { title: t.title, artist: t.artist, album: t.album, year: t.year, track_no: t.track_no };
  const get = id => document.getElementById(id)?.value.trim();
  const v = { title: get('edTitle'), artist: get('edArtist'), album: get('edAlbum'), year: get('edYear'), track_no: get('edTrack') };
  for (const k of ['title', 'artist', 'album']) {
    if (v[k] && v[k] !== cur[k]) fields[k] = v[k];
  }
  if (v.year && parseInt(v.year) !== cur.year) fields.year = parseInt(v.year);
  if (v.track_no && parseInt(v.track_no) !== cur.track_no) fields.track_no = parseInt(v.track_no);
  if (!Object.keys(fields).length) { closeModal(); return toast('没有修改', 'info'); }
  try { await invoke('update_track_fields', { trackId: tid, fields }); }
  catch (e) { return toast(String(e), 'err'); }
  closeModal();
  // 勾选「写回文件」→ 保存同时固化到音频文件标签
  let wbOk = null;
  if (document.getElementById('edWriteback')?.checked) {
    try { await invoke('writeback_track', { trackId: tid }); wbOk = true; }
    catch { wbOk = false; }
  }
  toast('已保存（来源标记：手动）' + (wbOk === true ? '，已写回文件' : wbOk === false ? '（写回文件失败，检查文件权限）' : ''), wbOk === false ? 'info' : 'ok');
  await refresh();
  // 播放器信息刷新：底栏只按曲目 id 变化重绘，同曲改标题需强制重绘；歌词页开着同步刷新
  lastNpId = null;
  syncBar();
  if (lyrics.classList.contains('open')) refreshLyricsView();
}

/* ================= 通用组件 ================= */
// 弹窗双模式：foot 为空 → A 式（右上角圆形 × 关闭，无底栏）；foot 非空 → B 式（底部固定按钮行，不随内容滚动）
// title 可为空（空则不渲染标题行）
function modal(title, body, foot, wide) {
  const hasFoot = !!foot && foot.trim() !== '';
  const closeBtn = hasFoot ? '' :
    `<button class="m-close" onclick="closeModal()" title="关闭"><svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg></button>`;
  const head = (title || closeBtn) ? `<div class="mh">${title ? `<h3>${title}</h3>` : '<span style="flex:1"></span>'}${closeBtn}</div>` : '';
  modalRoot.innerHTML = `<div class="modal${wide ? ' wide' : ''}${hasFoot ? ' has-foot' : ''}">${head}<div class="mb">${body}</div>${hasFoot ? `<div class="mf">${foot}</div>` : ''}</div>`;
  modalRoot.classList.add('open');
}
function closeModal() {
  // 冲刷 EQ/音调防抖：弹窗关闭前确保最后一次调整已提交（否则丢失）
  if (eqTimer) {
    clearTimeout(eqTimer); eqTimer = null;
    eqCommit().catch?.(e => toast(String(e), 'err'));
  }
  modalRoot.classList.remove('open');
}
function closeMenu() { menuRoot.style.display = 'none'; }
document.addEventListener('click', e => { if (menuRoot.style.display === 'block' && !menuRoot.contains(e.target)) closeMenu(); }, true);
async function copyText(s) {
  try { await navigator.clipboard.writeText(s); return true; }
  catch {
    try {
      const ta = document.createElement('textarea');
      ta.value = s; ta.style.position = 'fixed'; ta.style.opacity = 0;
      document.body.appendChild(ta); ta.select();
      const ok = document.execCommand('copy'); ta.remove(); return ok;
    } catch { return false; }
  }
}
function toast(msg, type, opts = {}) {
  const t = document.createElement('div');
  t.className = 'toast ' + (type || '');
  const txt = document.createElement('span');
  txt.textContent = (type === 'ok' ? '✓ ' : type === 'err' ? '✕ ' : 'ℹ ') + msg;
  t.appendChild(txt);
  if (opts.copy) {
    const cp = document.createElement('button');
    cp.className = 'toast-btn';
    cp.textContent = '复制';
    cp.onclick = async () => {
      const ok = await copyText(msg);
      cp.textContent = ok ? '已复制' : '复制失败';
      setTimeout(() => (cp.textContent = '复制'), 1500);
    };
    t.appendChild(cp);
  }
  if (opts.sticky) {
    const x = document.createElement('button');
    x.className = 'toast-btn';
    x.textContent = '✕';
    x.title = '关闭';
    x.onclick = () => t.remove();
    t.appendChild(x);
  }
  toastRoot.appendChild(t);
  if (!opts.sticky) {
    setTimeout(() => { t.style.transition = '.3s'; t.style.opacity = 0; t.style.transform = 'translateX(30px)'; setTimeout(() => t.remove(), 320); }, 3400);
  }
}
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && sel.ids.size) { selClear(); return; }
  if (e.code === 'Space' && !/INPUT|TEXTAREA/.test(document.activeElement.tagName)) { e.preventDefault(); invoke('toggle_play'); }
});

/* ================= 扫描 ================= */
async function doScan(btn) {
  if (btn) btn.disabled = true;
  try {
    const r = await invoke('pick_and_scan');
    toast(`扫描完成：处理 ${r.scanned} · 更新 ${r.changed} · 移除 ${r.removed}`, 'ok');
    await refresh();
  } catch (e) {
    if (String(e) !== '未选择目录') toast(String(e), 'err');
  }
  if (btn) btn.disabled = false;
  loadScanRoots();
}
async function doRemoveRoot(path, btn) {
  btn.disabled = true;
  try { await invoke('remove_scan_root', { path }); } catch (e) { toast(String(e), 'err'); btn.disabled = false; return; }
  toast('已移除（目录内曲目保留，不再自动监听）', 'ok');
  loadScanRoots();
  if (state.view === 'settings') render();
}

/* ================= 启动 ================= */
(async function boot() {
  // JS 运行时错误可见化：任何异常弹 toast，避免静默失败
  window.addEventListener('error', e => toast('JS 错误: ' + e.message, 'err'));
  window.addEventListener('unhandledrejection', e => toast('异步错误: ' + (e.reason && e.reason.message || e.reason), 'err'));

  try {
    await listen('scan-progress', e => {
      const { done, total } = e.payload;
      // 侧栏进度条已移除，扫描进度静默（完成时 scan-done 事件有 toast）
    });
    await listen('playback-position', e => {
      const p = e.payload;
      state.playing = p.playing; state.t = p.position; state.dur = p.duration;
      state.shuffle = p.shuffle; state.queueIndex = p.queue_index;
      // A-B 循环：越出 B 点即回 A（500ms 事件粒度内）
      if (abLoop.b != null && state.playing && state.t >= abLoop.b) {
        invoke('seek', { sec: abLoop.a });
      }
      if (typeof p.repeat === 'number' && p.repeat !== state.repeat) { state.repeat = p.repeat; syncRepeatBtn(); }
      if (p.track) {
        state.curTrack = p.track;
        if (p.track.id !== state.curId) { state.curId = p.track.id; resetAbLoop(); } // 切歌清除
      }
      // 音量图标/气泡与真实音量同步（会话恢复时 UI 默认值的问题；差异判断避免打断拖动）
      if (typeof p.volume === 'number' && Math.abs(p.volume - (state.volShown ?? -1)) > 0.01) {
        state.volShown = p.volume; state.volume = p.volume;
        updateVolIcon(p.volume);
        const s = document.getElementById('volSlider');
        if (s) { s.value = Math.round(p.volume * 100); }
        const lab = document.getElementById('volVal');
        if (lab) lab.textContent = Math.round(p.volume * 100);
      }
      syncBar();
      if (lyrics.classList.contains('open')) {
        lyrics.classList.toggle('playing', !!p.playing);
        updateLyricHighlight();
      }
      // 睡眠定时到点 → 暂停
      if (sleepAt && Date.now() >= sleepAt) {
        sleepAt = 0; btnSleep.classList.remove('on');
        invoke('toggle_play');
        toast('睡眠定时到了，已暂停播放 🌙', 'info');
      }
    });
  } catch (e) {
    // 失败：常驻不消失 + 可复制（便于报障），手动 ✕ 关闭
    toast('事件监听注册失败（时间轴/歌词将不可用）: ' + e, 'err', { sticky: true, copy: true });
  }
  PROVIDERS = await invoke('get_providers');
  await refresh();
  await loadEnrich();
  await loadScanRoots();
  loadFade();
  loadEq();
  loadSpeed();
  loadBgPlay();
  applyFeatBtns(); // 播放条功能开关（默认关闭：倍速/睡眠/A-B 不占位）
  loadCrossfade();
  loadRg();
  async function loadRg() { try { RG_MODE = await invoke('get_rg_mode'); } catch (e) { } }
  listen('scan-done', async e => {
    const { changed, removed, auto } = e.payload;
    if (auto) {
      toast(`检测到文件变动：更新 ${changed} · 移除 ${removed}，曲库已自动刷新`, 'ok');
      await refresh();
    }
  });
  // 曲库数据静默刷新（has_tr 回填完成等场景；不弹 toast）
  listen('library-changed', () => { refresh(); });
  listen('enrich-progress', e => {
    const { done, total, title } = e.payload;
    enrichProg.textContent = total ? `匹配中 ${done}/${total}${title ? ' · ' + title : ''}（1 req/s 限流）` : '';
  });
  try { state.shuffleMode = ((await invoke('get_config')).shuffle_mode ?? 0) > 0 ? 1 : 0; } catch {}
  try { state.keys = (await invoke('get_keys')) || {}; } catch {}
  try { const v = await window.__TAURI__.app.getVersion(); if (v) state.version = v; } catch {}
  try { state.artistImgs = await invoke('artist_cached_imgs'); } catch {}
  syncShuffleBtn(state.shuffleMode);
  if (state.view === 'playlist') loadPlaylistView();
  syncBar();
  renderQueueList();
})();
