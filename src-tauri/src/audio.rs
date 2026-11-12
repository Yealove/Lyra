use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 音频线程命令（设计文档 3.1：播放状态机与 UI 线程完全隔离，
/// OutputStream/Sink/Decoder 全部收敛在专用音频线程内，避免 Send/Sync 问题）
pub enum AudioCmd {
    Load { path: PathBuf, duration: f64, start_ms: u64, paused: bool },
    /// 无缝衔接：把下一首排进 Sink 队列（当前曲播完瞬间自动开始，零间隙）
    AppendNext { path: PathBuf, duration: f64 },
    /// 交叉淡入淡出：旧 sink 移交渐出线程，新 sink 立即开始渐入（重叠播放）
    CrossfadeNext { path: PathBuf, duration: f64 },
    Play,
    Pause,
    Seek(u64),
    Volume(f32),
    /// 渐入渐出时长（毫秒，0=关闭）
    SetFade(i32),
    /// 播放速度（音频线程按当前进度重建生效）
    SetSpeed(f32),
    /// ReplayGain 增益（dB，线性折算后与用户音量相乘）
    SetRg(f32),
    Stop,
}

/// 衔接曲"已开播"标记：音频回调首次拉取样本时置位并记录时刻（精确换曲计时）
#[derive(Default)]
pub struct TrackStart {
    started: AtomicBool,
    at: Mutex<Option<Instant>>,
}

impl TrackStart {
    fn mark(&self) {
        if !self.started.swap(true, Ordering::Relaxed) {
            *self.at.lock().unwrap() = Some(Instant::now());
        }
    }
    pub fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }
    pub fn started_at(&self) -> Instant {
        self.at.lock().unwrap().unwrap_or_else(Instant::now)
    }
}

/// 跨线程共享的播放快照（全部为原子/锁保护的简单值，天然 Send+Sync）
struct SharedState {
    playing: AtomicBool,
    /// seek/load 时的基准位置（毫秒）
    pos_base_ms: AtomicU64,
    /// 播放段起点；position = pos_base + elapsed（播放中）
    last_resume: Mutex<Option<Instant>>,
    /// 自然播完标记（仅当音频线程检测到 sink 耗尽时置位；初始必须为 false，
    /// 否则 ticker 会在首次 Load 完成前误判"已播完"而疯狂跳歌）
    ended: AtomicBool,
    /// 已成功加载过音频（区分"未加载"与"自然播完"，toggle_play 依赖）
    loaded: AtomicBool,
    /// 衔接曲已开播通知（音频线程 → ticker 更新 cur_index/cur_track_id）
    notify_switch: AtomicBool,
    /// 播放速度（f32 bits，默认 1.0；position 累计按速度折算）
    speed: AtomicU32,
    /// 交叉淡入淡出时长毫秒（0=关，用无缝衔接；>0 时曲间重叠过渡）
    pub crossfade_ms: AtomicU32,
    /// 当前曲时长（毫秒）——音频线程加载时更新，UI 事件读取
    pub cur_duration: AtomicU64,
}

pub struct Player {
    tx: Sender<AudioCmd>,
    st: Arc<SharedState>,
    vol_bits: AtomicU32,
    pub queue: Mutex<Vec<i64>>,
    pub cur_index: AtomicI64,
    pub shuffle: AtomicBool,
    /// 随机模式：0=关（顺序播放队列） 1=开启（所有歌曲随机：队列换成全库洗牌）
    pub shuffle_mode: AtomicI32,
    /// 随机轨迹（队列索引洗牌序列）+ 当前位置：开启随机时队列已是全库乱序，
    /// 轨迹保证一轮内不重复，走完自动重洗
    pub shuffle_seq: Mutex<Vec<i64>>,
    pub shuffle_pos: AtomicI64,
    /// 开启随机前的原队列快照（关闭随机时还原，避免丢失播放列表上下文）
    pub queue_snapshot: Mutex<Option<Vec<i64>>>,
    pub cur_track_id: AtomicI64,
    /// 已排队衔接的曲目索引（防重复 append；手动切歌时重置）
    pub prefetch_idx: AtomicI64,
    /// 会话恢复：下次加载从此位置开始（一次性）
    pending_start_ms: AtomicU64,
    /// 循环模式：0=关（播完停） 1=列表循环（默认） 2=单曲循环
    pub repeat_mode: AtomicU8,
    /// 渐入渐出时长毫秒（0=关；默认 10ms，可由设置页调整）
    pub fade_ms: AtomicI32,
    /// 均衡器共享参数（解码线程读取滤波）
    pub eq: Arc<EqShared>,
}

type SharedDec = Arc<Mutex<Option<Decoder<BufReader<File>>>>>;

/// 共享 Decoder 的 Source 包装：**解码、IO 与重采样全部在独立预解码线程**，
/// 音频回调只做无阻塞 try_recv——彻底消除回调内解码/锁/磁盘 IO（断续的根因）。
/// 样本全程 f32：symphonia 解出的 24-bit 高位深不再降为 16-bit；
/// 采样率转换由预解码线程的 sinc 重采样器完成（变速也折算进比率），
/// rodio 内部的线性插值转换层因 from==to 而直通。
/// 有界通道（512K 样本 ≈ 0.7s@192kHz×2ch）天然背压并吸收 IO 抖动。
/// start：衔接曲首次真实发声（回调首次成功取到样本）时置位（精确换曲计时）
/// fade_in_ms：起播音量斜坡时长（毫秒，0=关），消除新 Sink 起播的爆音（click）；
///          无缝衔接曲必须传 false（样本直连，不能有渐入）。
struct SharedSource {
    rx: std::sync::mpsc::Receiver<f32>,
    channels: u16,
    /// 输出采样率（= 设备采样率；重采样已在预解码线程完成）
    out_rate: u32,
    total_duration: Option<Duration>,
    start: Option<Arc<TrackStart>>,
    fade_total: usize,
    ramp_left: usize,
    eq: Arc<EqShared>,
    eqp: EqProcessor,
    ch_i: usize,
}

const PIPE_CAP: usize = 262_144;

/// 预解码线程内的高质量重采样：symphonia f32 → sinc 多相滤波 → 设备采样率。
/// 变速播放折算为虚拟输入率（in×speed），同一滤波器同时完成变速，
/// 质量远高于 rodio 的线性插值（无高频滚降/混叠）。
/// EOF 时 process_partial 冲出滤波器尾巴，不丢结尾。
fn resample_push(
    d: &mut Decoder<BufReader<File>>,
    tx: &std::sync::mpsc::SyncSender<f32>,
    virtual_in: f64,
    out_rate: u32,
) {
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
        WindowFunction,
    };
    let ch = d.channels().max(1) as usize;
    const CHUNK: usize = 1024; // 输入帧/批（≈23ms@44.1k，延迟与效率的折中）
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler = match SincFixedIn::<f32>::new(
        out_rate as f64 / virtual_in,
        1.0, // 不做运行时变比，固定比率
        params,
        CHUNK,
        ch,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[lyra-audio] 重采样器创建失败({e})，退回直通");
            while let Some(s) = d.next() {
                if tx.send(s).is_err() {
                    return;
                }
            }
            return;
        }
    };
    let mut interleaved: Vec<f32> = Vec::with_capacity(CHUNK * ch);
    loop {
        interleaved.clear();
        for _ in 0..CHUNK * ch {
            match d.next() {
                Some(s) => interleaved.push(s),
                None => break,
            }
        }
        let frames = interleaved.len() / ch;
        if frames == 0 {
            break;
        }
        // 交织 → 每通道独立（rubato 的输入布局）
        let waves: Vec<Vec<f32>> = (0..ch)
            .map(|c| (0..frames).map(|f| interleaved[f * ch + c]).collect())
            .collect();
        let outs = if frames == CHUNK {
            resampler.process(&waves, None)
        } else {
            resampler.process_partial(Some(&waves), None)
        };
        match outs {
            Ok(out) => {
                let out_frames = out.first().map(|v| v.len()).unwrap_or(0);
                for f in 0..out_frames {
                    for c in 0..ch {
                        if tx.send(out[c][f]).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[lyra-audio] 重采样失败: {e}");
                return;
            }
        }
        if frames < CHUNK {
            break; // EOF 已 flush，结束
        }
    }
}

// ================= 10 段图形均衡器（M5）=================
// RBJ Audio EQ Cookbook 的 peak EQ biquad，级联 10 段；滤波在预解码线程执行
// （回调零负担），参数经原子共享即时生效，系数仅在参数变化时重算。

const EQ_BANDS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

/// EQ 共享参数（Player 持有，消费端读取）
pub struct EqShared {
    pub enabled: AtomicBool,
    /// 各段增益 dB 的 bits（AtomicU32 存 f32）
    gains: [AtomicU32; 10],
    /// 音调（独立开关 + 低/高音搁架 dB）
    tone_on: AtomicBool,
    bass: AtomicU32,
    treble: AtomicU32,
}

impl EqShared {
    fn new() -> Self {
        EqShared {
            enabled: AtomicBool::new(false),
            gains: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())),
            tone_on: AtomicBool::new(false),
            bass: AtomicU32::new(0f32.to_bits()),
            treble: AtomicU32::new(0f32.to_bits()),
        }
    }
    pub fn set_gains(&self, gains: &[f32]) {
        for (i, g) in gains.iter().take(10).enumerate() {
            self.gains[i].store(g.clamp(-12.0, 12.0).to_bits(), Ordering::Relaxed);
        }
    }
    pub fn gains(&self) -> [f32; 10] {
        std::array::from_fn(|i| f32::from_bits(self.gains[i].load(Ordering::Relaxed)))
    }
    pub fn set_tone(&self, on: bool, bass: f32, treble: f32) {
        self.tone_on.store(on, Ordering::Relaxed);
        self.bass.store(bass.clamp(-12.0, 12.0).to_bits(), Ordering::Relaxed);
        self.treble.store(treble.clamp(-12.0, 12.0).to_bits(), Ordering::Relaxed);
    }
    pub fn tone(&self) -> (bool, f32, f32) {
        (
            self.tone_on.load(Ordering::Relaxed),
            f32::from_bits(self.bass.load(Ordering::Relaxed)),
            f32::from_bits(self.treble.load(Ordering::Relaxed)),
        )
    }
}

/// 单段 biquad（含双声道状态）。系数与状态用 f64：f32 DF1 在低频段
/// （31/62Hz 极点近单位圆）级联 10 段的舍入噪声可到 -67dB（高于 16-bit 底噪），
/// f64 内部将该噪声压到 -150dB 以下（不可闻、不可测）。
#[derive(Clone, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    // 每声道 x1/x2/y1/y2
    xs: Vec<[f64; 2]>,
    ys: Vec<[f64; 2]>,
}

impl Biquad {
    fn peak(f0: f32, gain_db: f32, sr: f32, channels: usize) -> Self {
        let mut bq = Biquad {
            xs: vec![[0.0; 2]; channels],
            ys: vec![[0.0; 2]; channels],
            ..Default::default()
        };
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f0 as f64 / sr.max(1.0) as f64;
        let alpha = w0.sin() / (2.0 * 1.4); // Q = 1.4
        let cosw = w0.cos();
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cosw;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cosw;
        let a2 = 1.0 - alpha / a;
        bq.b = [b0 / a0, b1 / a0, b2 / a0];
        bq.a = [1.0, a1 / a0, a2 / a0];
        bq
    }
    /// 低频搁架（RBJ cookbook，Q=0.7）：200Hz 以下整片提升/衰减
    fn low_shelf(f0: f32, gain_db: f32, sr: f32, channels: usize) -> Self {
        let mut bq = Biquad {
            xs: vec![[0.0; 2]; channels],
            ys: vec![[0.0; 2]; channels],
            ..Default::default()
        };
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f0 as f64 / sr.max(1.0) as f64;
        let alpha = w0.sin() / (2.0 * 0.7);
        let cosw = w0.cos();
        let two_aa = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cosw + two_aa);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cosw);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cosw - two_aa);
        let a0 = (a + 1.0) + (a - 1.0) * cosw + two_aa;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cosw);
        let a2 = (a + 1.0) + (a - 1.0) * cosw - two_aa;
        bq.b = [b0 / a0, b1 / a0, b2 / a0];
        bq.a = [1.0, a1 / a0, a2 / a0];
        bq
    }
    /// 高频搁架：3.5kHz 以上整片提升/衰减
    fn high_shelf(f0: f32, gain_db: f32, sr: f32, channels: usize) -> Self {
        let mut bq = Biquad {
            xs: vec![[0.0; 2]; channels],
            ys: vec![[0.0; 2]; channels],
            ..Default::default()
        };
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f0 as f64 / sr.max(1.0) as f64;
        let alpha = w0.sin() / (2.0 * 0.7);
        let cosw = w0.cos();
        let two_aa = 2.0 * a.sqrt() * alpha;
        // RBJ Audio EQ Cookbook 原文公式（DC=0dB、Nyquist=+g 双约束数值验证通过）
        let b0 = a * ((a + 1.0) + (a - 1.0) * cosw + two_aa);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cosw);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cosw - two_aa);
        let a0 = (a + 1.0) - (a - 1.0) * cosw + two_aa;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cosw);
        let a2 = (a + 1.0) - (a - 1.0) * cosw - two_aa;
        bq.b = [b0 / a0, b1 / a0, b2 / a0];
        bq.a = [1.0, a1 / a0, a2 / a0];
        bq
    }
}

/// EQ 处理器：消费端（音频回调内）逐样本滤波——参数变化即时生效。
/// 10 段 peak + 音调双搁架（低架 200Hz / 高架 3.5kHz，独立开关）级联。
/// 实时安全：无锁（原子读）、无 IO、无内存分配（容量预分配，truncate/push 不触分配器）。
#[derive(Default)]
pub struct EqProcessor {
    bands: Vec<Biquad>,
    last_key: ([f32; 10], bool, bool, f32, f32),
    channels: usize,
    sample_rate: f32,
    /// 防削波预衰减 = 1/级联频响峰值（仅提升时 <1）。
    /// 数字音源普遍满幅（~0dBFS），EQ 提升会样本越界，此前靠 clamp 硬削波
    /// （可闻破音）。改为按频响峰值整体下移电平：boost 相对音色保持、峰值不越界。
    headroom: f32,
}

impl EqProcessor {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let mut p = EqProcessor {
            bands: Vec::with_capacity(12),
            last_key: ([f32::NAN; 10], false, false, 0.0, 0.0), // 强制首帧重建
            channels: channels.max(1),
            sample_rate: sample_rate as f32,
            headroom: 1.0,
        };
        p.rebuild(&[0.0; 10], false, false, 0.0, 0.0);
        p
    }
    fn rebuild(&mut self, gains: &[f32; 10], eq_on: bool, tone_on: bool, bass: f32, treble: f32) {
        self.bands.clear();
        if eq_on {
            for (i, f) in EQ_BANDS.iter().enumerate() {
                // 平坦段跳过：零增益段传函恒等，但 DF1 数值噪声会累积；
                // 跳过后只滤波用户动过的段（少噪声、少 CPU）
                if gains[i].abs() >= 0.05 {
                    self.bands.push(Biquad::peak(*f, gains[i], self.sample_rate, self.channels));
                }
            }
        }
        if tone_on {
            if bass.abs() > 0.05 {
                self.bands.push(Biquad::low_shelf(200.0, bass, self.sample_rate, self.channels));
            }
            if treble.abs() > 0.05 {
                self.bands.push(Biquad::high_shelf(3500.0, treble, self.sample_rate, self.channels));
            }
        }
        // 级联频响峰值扫描（含相邻段叠加）：headroom 只补偿提升，衰减不放大
        self.headroom = if self.bands.is_empty() {
            1.0
        } else {
            let peak = self.cascade_peak_gain();
            if peak > 1.0 { 1.0 / peak } else { 1.0 }
        };
        self.last_key = (*gains, eq_on, tone_on, bass, treble);
    }
    /// 级联频响峰值：20Hz~Nyquist 对数网格逐点求 |H(e^jw)| 乘积（f64 中间精度）。
    /// 覆盖相邻段交叠（Q=1.4 时两段相邻同时 +12dB 中心叠加可到 ~18dB）。
    fn cascade_peak_gain(&self) -> f32 {
        let nyq = ((self.sample_rate / 2.0).min(20_000.0).max(21.0)) as f64;
        let n = 240; // ~1/24 倍频程网格
        let mut peak = 1.0f64;
        for i in 0..n {
            let f = 20.0 * (nyq / 20.0).powf(i as f64 / (n - 1) as f64);
            let w = 2.0 * std::f64::consts::PI * f / self.sample_rate as f64;
            let (cw, sw) = (w.cos(), -w.sin()); // e^{-jw}
            let (c2, s2) = (cw * cw - sw * sw, 2.0 * cw * sw); // e^{-2jw}
            let mut mag = 1.0f64;
            for bq in &self.bands {
                let (b0, b1, b2) = (bq.b[0], bq.b[1], bq.b[2]);
                let (a1, a2) = (bq.a[1], bq.a[2]);
                let nr = b0 + b1 * cw + b2 * c2;
                let ni = b1 * sw + b2 * s2;
                let dr = 1.0 + a1 * cw + a2 * c2;
                let di = a1 * sw + a2 * s2;
                mag *= ((nr * nr + ni * ni) / (dr * dr + di * di)).sqrt();
            }
            if mag > peak {
                peak = mag;
            }
        }
        peak as f32
    }
    /// 滤波：EQ 段随 EQ 开关，音调搁架随音调开关——两者相互独立；
    /// 无任何生效段时直通（零开销、零损失）。f32 全程无量化。
    /// 有生效段时：滤波 → ×headroom（防提升削波）→ clamp（极端安全网）。
    pub fn process(&mut self, s: f32, ch: usize, gains: &[f32; 10], eq_on: bool, tone: (bool, f32, f32)) -> f32 {
        let key = (*gains, eq_on, tone.0, tone.1, tone.2);
        if key != self.last_key {
            self.rebuild(gains, eq_on, tone.0, tone.1, tone.2);
        }
        if self.bands.is_empty() {
            return s;
        }
        let mut y = s as f64;
        for bq in self.bands.iter_mut() {
            let (x1, x2) = (bq.xs[ch][0], bq.xs[ch][1]);
            let (y1, y2) = (bq.ys[ch][0], bq.ys[ch][1]);
            let out = bq.b[0] * y + bq.b[1] * x1 + bq.b[2] * x2 - bq.a[1] * y1 - bq.a[2] * y2;
            bq.xs[ch] = [y, x1];
            bq.ys[ch] = [out, y1];
            y = out;
        }
        ((y as f32) * self.headroom).clamp(-1.0, 1.0)
    }
}
#[cfg(target_os = "macos")]
fn set_thread_qos_interactive() {
    extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
    }
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe {
        pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    };
}
#[cfg(not(target_os = "macos"))]
fn set_thread_qos_interactive() {}

impl SharedSource {
    fn new(
        dec: SharedDec,
        start: Option<Arc<TrackStart>>,
        fade_in_ms: i32,
        eq: Arc<EqShared>,
        speed: f32,
        out_rate: u32,
    ) -> Self {
        // 元数据只在创建时读一次
        let g = dec.lock().unwrap();
        let (ch, sr, dur) = match g.as_ref() {
            Some(d) => (d.channels(), d.sample_rate(), d.total_duration()),
            None => (2, 44100, None),
        };
        drop(g);
        let (tx, rx) = std::sync::mpsc::sync_channel::<f32>(PIPE_CAP);
        std::thread::Builder::new()
            .name("lyra-predecode".into())
            .spawn(move || {
                set_thread_qos_interactive();
                // 解码 + （必要时）高质量重采样；EQ 在消费端做，参数变化即时生效。
                // 变速 = 虚拟输入率 sr×speed，重采样器一并完成变速。
                let mut g = dec.lock().unwrap();
                if let Some(d) = g.as_mut() {
                    let virtual_in = sr as f64 * speed as f64;
                    if (virtual_in - out_rate as f64).abs() < 0.5 {
                        // 采样率一致（且原速）：样本直通，bit-perfect
                        while let Some(s) = d.next() {
                            if tx.send(s).is_err() {
                                break;
                            }
                        }
                    } else {
                        resample_push(d, &tx, virtual_in, out_rate);
                    }
                }
            })
            .ok();
        // 渐入样本数（fade_in_ms 毫秒；0=关闭）——按输出率计算
        let fade_total = if fade_in_ms > 0 {
            (out_rate as usize * ch as usize) * fade_in_ms as usize / 1000
        } else {
            0
        };
        SharedSource {
            rx,
            channels: ch,
            out_rate,
            total_duration: dur,
            start,
            fade_total,
            ramp_left: fade_total,
            eq,
            eqp: EqProcessor::new(ch as usize, sr),
            ch_i: 0,
        }
    }
}

impl Iterator for SharedSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if let Some(f) = self.start.as_ref() {
            f.mark(); // 幂等：首次发声时刻
        }
        // 关键修复：缓冲空 ≠ 曲目结束。衔接曲刚 append 时解码线程可能尚未产出
        // 首批样本——此前 try_recv 的 Empty 会被 `?` 当成 EOF，衔接曲瞬间"结束"，
        // next_flag 永远等待 mark → 卡死（无声、pos 持续涨、不切歌）。
        // 现在：Empty 短暂等待（解码线程全速产出）；Disconnected（tx drop=EOF）才结束；
        // 累计等待 ~2s 仍无样本则返回静音保活，防音频回调被 IO 异常卡死。
        let s = {
            let mut waited: u32 = 0;
            loop {
                match self.rx.try_recv() {
                    Ok(s) => break s,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        if waited >= 5000 {
                            break 0.0f32; // 静音保活，下一轮继续等
                        }
                        std::thread::sleep(Duration::from_micros(400));
                        waited += 1;
                    }
                }
            }
        };
        // EQ 消费端滤波：参数变化即时生效（下一个样本即用新系数）
        let out = {
            let enabled = self.eq.enabled.load(Ordering::Relaxed);
            let gains = self.eq.gains();
            let tone = self.eq.tone();
            let y = self.eqp.process(s, self.ch_i, &gains, enabled, tone);
            self.ch_i = (self.ch_i + 1) % self.channels.max(1) as usize;
            y
        };
        if self.ramp_left > 0 {
            self.ramp_left -= 1;
            let g = 1.0 - self.ramp_left as f32 / self.fade_total.max(1) as f32;
            return Some(out * g);
        }
        Some(out)
    }
}

impl Source for SharedSource {
    fn current_frame_len(&self) -> Option<usize> {
        None // 未知（流水线缓冲），rodio 允许
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        // 重采样已在预解码线程完成（含变速），这里报设备率：
        // rodio 转换层 from==to 直通，杜绝其线性插值
        self.out_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }
}

/// 防御性构建 Decoder：部分容器（如 afconvert 产出的 m4a）会让
/// rodio 内部 panic（unreachable!），catch_unwind 防止击杀音频线程。
pub(crate) fn open_decoder(path: &std::path::Path) -> Option<Decoder<BufReader<File>>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[lyra-audio] 打开失败 {}: {e}", path.display());
            return None;
        }
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Decoder::new(BufReader::new(f))
    })) {
        Ok(Ok(d)) => Some(d),
        Ok(Err(e)) => {
            eprintln!("[lyra-audio] 解码失败 {}: {e}", path.display());
            None
        }
        Err(_) => {
            eprintln!("[lyra-audio] 解码器初始化 panic（不支持的容器）: {}", path.display());
            None
        }
    }
}

impl Player {
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<AudioCmd>();
        let st = Arc::new(SharedState {
            playing: AtomicBool::new(false),
            pos_base_ms: AtomicU64::new(0),
            last_resume: Mutex::new(None),
            ended: AtomicBool::new(false),
            loaded: AtomicBool::new(false),
            notify_switch: AtomicBool::new(false),
            speed: AtomicU32::new(1.0f32.to_bits()),
            crossfade_ms: AtomicU32::new(0),
            cur_duration: AtomicU64::new(0),
        });
        let st2 = st.clone();
        let eq = Arc::new(EqShared::new());
        let eq2 = eq.clone();
        std::thread::Builder::new()
            .name("lyra-audio".into())
            .spawn(move || audio_thread(rx, st2, eq2))?;
        Ok(Player {
            tx,
            st,
            vol_bits: AtomicU32::new(0.8f32.to_bits()),
            queue: Mutex::new(Vec::new()),
            cur_index: AtomicI64::new(-1),
            shuffle: AtomicBool::new(false),
            shuffle_mode: AtomicI32::new(0),
            shuffle_seq: Mutex::new(Vec::new()),
            shuffle_pos: AtomicI64::new(-1),
            queue_snapshot: Mutex::new(None),
            cur_track_id: AtomicI64::new(-1),
            prefetch_idx: AtomicI64::new(-1),
            pending_start_ms: AtomicU64::new(0),
            repeat_mode: AtomicU8::new(1),
            fade_ms: AtomicI32::new(10),
            eq,
        })
    }

    pub fn load(&self, path: PathBuf, duration: f64) -> Result<(), String> {
        self.send_load(path, duration, false)
    }

    /// 预加载：解码并 seek 到恢复位置，但保持暂停（启动时恢复上下文用，点播放零延迟）
    pub fn preload(&self, path: PathBuf, duration: f64) -> Result<(), String> {
        self.send_load(path, duration, true)
    }

    fn send_load(&self, path: PathBuf, duration: f64, paused: bool) -> Result<(), String> {
        self.st
            .cur_duration
            .store((duration * 1000.0) as u64, Ordering::Relaxed);
        self.prefetch_idx.store(-1, Ordering::Relaxed);
        // 恢复播放位置（会话恢复场景，一次性消费）
        let start_ms = self.pending_start_ms.swap(0, Ordering::Relaxed);
        self.tx
            .send(AudioCmd::Load { path, duration, start_ms, paused })
            .map_err(|_| "音频线程已退出".to_string())
    }

    /// 记录下次加载时的起始位置（会话恢复）
    pub fn set_resume_pos(&self, sec: f64) {
        self.pending_start_ms
            .store((sec.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
    }

    /// 设置渐入渐出时长（毫秒，0=关），立即生效并同步音频线程
    pub fn set_fade(&self, ms: i32) {
        let ms = ms.clamp(0, 1000);
        self.fade_ms.store(ms, Ordering::Relaxed);
        let _ = self.tx.send(AudioCmd::SetFade(ms));
    }

    /// 播放速度（0.25–4.0），音频线程按当前进度重建生效
    pub fn set_speed(&self, v: f32) {
        let _ = self.tx.send(AudioCmd::SetSpeed(v.clamp(0.25, 4.0)));
    }

    pub fn speed(&self) -> f32 {
        f32::from_bits(self.st.speed.load(Ordering::Relaxed))
    }

    /// ReplayGain 增益（dB）；实际输出音量 = 用户音量 × 10^(dB/20)
    pub fn set_rg(&self, db: f32) {
        let _ = self.tx.send(AudioCmd::SetRg(db));
    }

    pub fn set_crossfade(&self, ms: u32) {
        self.st.crossfade_ms.store(ms.min(10_000), Ordering::Relaxed);
    }
    pub fn crossfade_ms(&self) -> u32 {
        self.st.crossfade_ms.load(Ordering::Relaxed)
    }

    // —— 均衡器 + 音调 ——
    pub fn set_eq(&self, enabled: bool, gains: &[f32], tone_on: bool, bass: f32, treble: f32) {
        self.eq.enabled.store(enabled, Ordering::Relaxed);
        self.eq.set_gains(gains);
        self.eq.set_tone(tone_on, bass, treble);
    }
    pub fn eq_state(&self) -> (bool, [f32; 10], (bool, f32, f32)) {
        (
            self.eq.enabled.load(Ordering::Relaxed),
            self.eq.gains(),
            self.eq.tone(),
        )
    }

    /// 无缝衔接：把下一首排进 Sink 队列（当前曲剩余 2.5s 内调用）
    pub fn append_next(&self, path: PathBuf, duration: f64) -> Result<(), String> {
        self.tx
            .send(AudioCmd::AppendNext { path, duration })
            .map_err(|_| "音频线程已退出".to_string())
    }

    /// 交叉淡入淡出衔接（crossfade_ms > 0 时由 ticker 调用）
    pub fn crossfade_next(&self, path: PathBuf, duration: f64) -> Result<(), String> {
        self.tx
            .send(AudioCmd::CrossfadeNext { path, duration })
            .map_err(|_| "音频线程已退出".to_string())
    }

    /// 衔接曲已开播（ticker 消费后更新 cur_index/cur_track_id）
    pub fn take_switch_notify(&self) -> bool {
        self.st.notify_switch.swap(false, Ordering::Relaxed)
    }

    pub fn play(&self) {
        let _ = self.tx.send(AudioCmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(AudioCmd::Pause);
    }

    pub fn seek(&self, sec: f64) {
        let _ = self.tx.send(AudioCmd::Seek((sec * 1000.0).max(0.0) as u64));
    }

    pub fn set_volume(&self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        self.vol_bits.store(v.to_bits(), Ordering::Relaxed);
        let _ = self.tx.send(AudioCmd::Volume(v));
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.vol_bits.load(Ordering::Relaxed))
    }

    pub fn stop(&self) {
        let _ = self.tx.send(AudioCmd::Stop);
        self.cur_index.store(-1, Ordering::Relaxed);
        self.cur_track_id.store(-1, Ordering::Relaxed);
    }

    // —— 共享状态读取 ——
    pub fn is_playing(&self) -> bool {
        self.st.playing.load(Ordering::Relaxed)
    }

    /// 自然播完（仅音频线程检测到 sink 耗尽时为 true，供 ticker 触发自动下一首）
    pub fn finished(&self) -> bool {
        self.st.ended.load(Ordering::Relaxed) && !self.st.playing.load(Ordering::Relaxed)
    }

    /// 已成功加载音频（未加载时 toggle_play 应触发加载而非 play）
    pub fn loaded(&self) -> bool {
        self.st.loaded.load(Ordering::Relaxed)
    }

    pub fn position_ms(&self) -> u64 {
        let base = self.st.pos_base_ms.load(Ordering::Relaxed);
        let speed = f32::from_bits(self.st.speed.load(Ordering::Relaxed)) as f64;
        let g = self.st.last_resume.lock().unwrap();
        match *g {
            Some(t) if self.st.playing.load(Ordering::Relaxed) => {
                base + (t.elapsed().as_millis() as f64 * speed) as u64
            }
            _ => base,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.st.cur_duration.load(Ordering::Relaxed)
    }

    pub fn set_duration_ms(&self, ms: u64) {
        self.st.cur_duration.store(ms, Ordering::Relaxed);
    }
}

/// 简化 seek：try_seek 失败时同步丢弃样本到目标位置（速度重建场景，一次性可接受）
fn decoder_seek_to_dumb(d: &mut Decoder<BufReader<File>>, ms: u64) {
    if d.try_seek(Duration::from_millis(ms)).is_err() {
        let sr = d.sample_rate().max(1) as u64;
        let ch = d.channels().max(1) as u64;
        let need = ms.saturating_mul(sr).saturating_mul(ch) / 1000;
        let mut n = 0u64;
        while n < need {
            if d.next().is_none() {
                break;
            }
            n += 1;
        }
    }
}

/// 停止旧 Sink 前的音量渐出，消除硬停的截断爆音；fade_ms=0 直接停（用户关闭渐变）
fn fade_out_and_stop(sink: &mut Option<Sink>, volume: f32, fade_ms: i32) {
    if let Some(s) = sink.take() {
        if fade_ms <= 0 {
            s.stop();
            return;
        }
        const STEPS: i32 = 8;
        let per = (fade_ms as u64 / STEPS.max(1) as u64).max(1);
        for k in 1..=STEPS {
            s.set_volume(volume * (1.0 - k as f32 / STEPS as f32));
            std::thread::sleep(Duration::from_millis(per));
        }
        s.stop();
    }
}

/// 专用音频线程：唯一持有 OutputStream / Sink / Decoder
fn audio_thread(rx: Receiver<AudioCmd>, st: Arc<SharedState>, eq: Arc<EqShared>) {
    set_thread_qos_interactive();
    let (_stream, handle): (OutputStream, OutputStreamHandle) =
        match OutputStream::try_default() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[lyra-audio] 打开输出设备失败: {e}");
                // 仍需消费命令避免发送端报错
                while let Ok(_) = rx.recv() {}
                return;
            }
        };
    // 设备实际运行采样率：预解码线程重采样到此率（rodio 转换层随之直通）
    let out_rate = _stream.sample_rate();
    eprintln!("[lyra-audio] 输出流采样率: {out_rate} Hz");
    let _ = &handle; // handle 在下方使用
    let mut sink: Option<Sink> = None;
    let mut dec: Option<Arc<Mutex<Option<Decoder<BufReader<File>>>>>> = None;
    let mut volume = 0.8f32;
    let mut cur_path: Option<PathBuf> = None;
    let mut fade_ms: i32 = 10; // 渐入渐出时长（SetFade 命令更新）
    let mut speed: f32 = 1.0;  // 播放速度（SetSpeed 命令更新）
    let mut rg_db: f32 = 0.0;  // ReplayGain 增益 dB（SetRg 命令更新）
    let eff_vol = |vol: f32, rg_db: f32| vol * 10f32.powf(rg_db / 20.0);

    let settle = |st: &SharedState| {
        let mut g = st.last_resume.lock().unwrap();
        if let Some(t) = *g {
            let add = t.elapsed().as_millis() as u64;
            st.pos_base_ms.fetch_add(add, Ordering::Relaxed);
            *g = None;
        }
    };

    let mut next_flag: Option<Arc<TrackStart>> = None;
    let mut next_path: Option<PathBuf> = None;
    let mut empty_streak: u32 = 0; // sink 连续耗尽轮数（衔接切换瞬间防误判）

    /// seek 丢弃任务（Unseekable 格式）：分片推进，期间旧曲继续播、命令可插队
    struct SkipJob {
        dec: SharedDec,
        path: PathBuf,
        target_ms: u64,
        skipped: u64, // 已丢样本数
        rate: u64,    // 样本/秒（sr*ch）
        was_playing: bool,
    }
    let mut skip: Option<SkipJob> = None;
    let mut pending_cmds: std::collections::VecDeque<AudioCmd> = std::collections::VecDeque::new();

    loop {
        let cmd = if let Some(c) = pending_cmds.pop_front() {
            Ok(c)
        } else {
            rx.recv_timeout(Duration::from_millis(200))
        };
        match cmd {
            Ok(AudioCmd::CrossfadeNext { path, duration }) => {
                // 交叉淡入淡出：旧 sink 移交专用渐出线程（分步降音量后 stop），
                // 新 sink 立即开播并以同样时长渐入（FadeSource 复用渐入机制）
                let cf = st.crossfade_ms.load(Ordering::Relaxed) as i64;
                if cf <= 0 || sink.is_none() {
                    continue;
                }
                let d = match open_decoder(&path) {
                    Some(d) => d,
                    None => continue,
                };
                let flag = Arc::new(TrackStart::default());
                let shared: SharedDec = Arc::new(Mutex::new(Some(d)));
                if let Ok(new_sink) = Sink::try_new(&handle) {
                    // 旧 sink 渐出（后台线程持有至 stop）
                    if let Some(old) = sink.take() {
                        let vol = eff_vol(volume, rg_db);
                        let _ = std::thread::Builder::new().name("lyra-crossfade".into()).spawn(move || {
                            let steps = 16i32;
                            let per = (cf as u64 / steps.max(1) as u64).max(1);
                            for k in 1..=steps {
                                old.set_volume(vol * (1.0 - k as f32 / steps as f32));
                                std::thread::sleep(Duration::from_millis(per));
                            }
                            old.stop();
                        });
                    }
                    next_flag = None;
                    next_path = None;
                    let cf32 = cf as i32;
                    new_sink.append(SharedSource::new(shared.clone(), Some(flag.clone()), cf32, eq.clone(), speed, out_rate));
                    new_sink.set_volume(eff_vol(volume, rg_db));
                    new_sink.play();
                    sink = Some(new_sink);
                    dec = Some(shared);
                    cur_path = Some(path.clone());
                    st.pos_base_ms.store(0, Ordering::Relaxed);
                    *st.last_resume.lock().unwrap() = Some(Instant::now());
                    st.playing.store(true, Ordering::Relaxed);
                    st.ended.store(false, Ordering::Relaxed);
                    st.loaded.store(true, Ordering::Relaxed);
                    st.cur_duration.store((duration * 1000.0) as u64, Ordering::Relaxed);
                    st.notify_switch.store(true, Ordering::Relaxed);
                }
            }
            Ok(AudioCmd::Load { path, duration, start_ms, paused }) => {
                skip = None; // 手动换曲：作废进行中的 seek 丢弃
                settle(&st);
                cur_path = Some(path.clone());
                let mut d = match open_decoder(&path) {
                    Some(d) => d,
                    None => {
                        st.playing.store(false, Ordering::Relaxed);
                        st.loaded.store(false, Ordering::Relaxed);
                        // 不置 ended：解码失败不触发自动跳歌，避免整库被刷
                        continue;
                    }
                };
                // 会话恢复：从上次位置开始（append 前 seek 最可靠；失败则同步丢弃）
                if start_ms > 0 {
                    if let Err(e) = d.try_seek(Duration::from_millis(start_ms)) {
                        let sr = d.sample_rate().max(1) as u64;
                        let ch = d.channels().max(1) as u64;
                        let need = start_ms.saturating_mul(sr).saturating_mul(ch) / 1000;
                        let mut n = 0u64;
                        while n < need {
                            if d.next().is_none() {
                                break;
                            }
                            n += 1;
                        }
                        eprintln!("[lyra-audio] 恢复位置 try_seek 失败（{e}），同步丢弃至 {:.1}s", start_ms as f64 / 1000.0);
                    }
                    st.pos_base_ms.store(start_ms, Ordering::Relaxed);
                }
                let shared: SharedDec = Arc::new(Mutex::new(Some(d)));
                match Sink::try_new(&handle) {
                    Ok(new_sink) => {
                        // 先备好新链路，再停旧：静音间隙压缩到毫秒级
                        fade_out_and_stop(&mut sink, eff_vol(volume, rg_db), fade_ms);
                        next_flag = None; // 手动切歌：丢弃可能已排队的衔接曲（随旧 sink 一并作废）
                        next_path = None;
                        // 时长随加载更新（否则 UI 显示上一首时长，position 会"超窗"）
                        st.cur_duration.store((duration * 1000.0) as u64, Ordering::Relaxed);
                        new_sink.append(SharedSource::new(shared.clone(), None, fade_ms, eq.clone(), speed, out_rate));
                        new_sink.set_volume(eff_vol(volume, rg_db));
                        if paused {
                            // 预加载：解码就绪但挂起，不出声（点播放键零延迟响应）
                            new_sink.pause();
                            sink = Some(new_sink);
                            dec = Some(shared);
                            if start_ms == 0 {
                                st.pos_base_ms.store(0, Ordering::Relaxed);
                            }
                            *st.last_resume.lock().unwrap() = None;
                            st.playing.store(false, Ordering::Relaxed);
                            st.ended.store(false, Ordering::Relaxed);
                            st.loaded.store(true, Ordering::Relaxed);
                        } else {
                            new_sink.play();
                            sink = Some(new_sink);
                            dec = Some(shared);
                            if start_ms == 0 {
                                st.pos_base_ms.store(0, Ordering::Relaxed);
                            }
                            *st.last_resume.lock().unwrap() = Some(Instant::now());
                            st.playing.store(true, Ordering::Relaxed);
                            st.ended.store(false, Ordering::Relaxed);
                            st.loaded.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(e) => eprintln!("[lyra-audio] Sink 创建失败: {e}"),
                }
                let _ = duration;
            }
            Ok(AudioCmd::AppendNext { path, duration }) => {
                // 无缝衔接：下一首排进当前 Sink 队列，当前曲播完瞬间自动开始（零间隙）
                if sink.is_none() || next_flag.is_some() {
                    continue; // 已有未开播的衔接：拒绝（防随机模式重复 append 堆积）
                }
                let d = match open_decoder(&path) {
                    Some(d) => d,
                    None => {
                        eprintln!("[xfade] append 解码失败: {}", path.display());
                        continue;
                    }
                };
                let flag = Arc::new(TrackStart::default());
                let shared: SharedDec = Arc::new(Mutex::new(Some(d)));
                if let Some(s) = sink.as_ref() {
                    s.append(SharedSource::new(shared, Some(flag.clone()), 0, eq.clone(), speed, out_rate));
                    next_flag = Some(flag);
                    next_path = Some(path.clone());
                    eprintln!("[xfade] append OK: {}", path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default());
                }
            }
            Ok(AudioCmd::Play) => {
                if let Some(s) = sink.as_ref() {
                    s.play();
                    *st.last_resume.lock().unwrap() = Some(Instant::now());
                    st.playing.store(true, Ordering::Relaxed);
                }
            }
            Ok(AudioCmd::Pause) => {
                if let Some(s) = sink.as_ref() {
                    s.pause();
                }
                settle(&st);
                st.playing.store(false, Ordering::Relaxed);
            }
            Ok(AudioCmd::Seek(ms)) => {
                // 重建式 seek：全新 decoder 上 seek（无缓冲竞争）。
                // try_seek 成功（WAV/MP3/带 seek table 的 FLAC）→ 立即换轨。
                // 失败（Unseekable）→ 建分片丢弃 job：旧曲继续出声、命令可插队、
                // 新目标 ≥ 已丢位置时增量续丢（避免连续拖动重复劳动）。
                let was_playing = st.playing.load(Ordering::Relaxed);
                // 增量续丢：丢弃中的 job，同曲且目标在已丢位置之后 → 只补差量
                if let Some(job) = skip.as_mut() {
                    if Some(&job.path) == cur_path.as_ref() {
                        let done_ms = job.skipped.saturating_mul(1000) / job.rate.max(1);
                        if ms >= done_ms {
                            job.target_ms = ms;
                            job.was_playing = was_playing;
                            continue;
                        }
                    }
                }
                let path = match cur_path.clone() {
                    Some(p) => p,
                    None => continue,
                };
                settle(&st);
                let mut d = match open_decoder(&path) {
                    Some(d) => d,
                    None => {
                        st.playing.store(false, Ordering::Relaxed);
                        st.loaded.store(false, Ordering::Relaxed);
                        continue;
                    }
                };
                match d.try_seek(Duration::from_millis(ms)) {
                    Ok(()) => {
                        let shared: SharedDec = Arc::new(Mutex::new(Some(d)));
                        match Sink::try_new(&handle) {
                            Ok(new_sink) => {
                                fade_out_and_stop(&mut sink, eff_vol(volume, rg_db), fade_ms);
                                next_flag = None;
                                next_path = None;
                                new_sink.append(SharedSource::new(shared.clone(), None, fade_ms, eq.clone(), speed, out_rate));
                                new_sink.set_volume(eff_vol(volume, rg_db));
                                if was_playing {
                                    new_sink.play();
                                } else {
                                    new_sink.pause();
                                }
                                sink = Some(new_sink);
                                dec = Some(shared);
                                st.pos_base_ms.store(ms, Ordering::Relaxed);
                                *st.last_resume.lock().unwrap() =
                                    if was_playing { Some(Instant::now()) } else { None };
                                st.ended.store(false, Ordering::Relaxed);
                                st.loaded.store(true, Ordering::Relaxed);
                            }
                            Err(e) => eprintln!("[lyra-audio] seek 重建 Sink 失败: {e}"),
                        }
                    }
                    Err(e) => {
                        let sr = d.sample_rate().max(1) as u64;
                        let ch = d.channels().max(1) as u64;
                        eprintln!(
                            "[lyra-audio] try_seek 失败（{e}），分片丢弃到 {:.1}s（旧曲继续播）",
                            ms as f64 / 1000.0
                        );
                        skip = Some(SkipJob {
                            dec: Arc::new(Mutex::new(Some(d))),
                            path,
                            target_ms: ms,
                            skipped: 0,
                            rate: sr.saturating_mul(ch),
                            was_playing,
                        });
                    }
                }
            }
            Ok(AudioCmd::SetFade(ms)) => {
                fade_ms = ms.clamp(0, 1000);
            }
            Ok(AudioCmd::SetRg(db)) => {
                rg_db = db.clamp(-24.0, 12.0);
                if let Some(s) = sink.as_ref() {
                    s.set_volume(eff_vol(volume, rg_db));
                }
            }
            Ok(AudioCmd::SetSpeed(v)) => {
                // settle 按旧速度把 elapsed 并入 pos_base，再切新速度
                let was_playing = st.playing.load(Ordering::Relaxed);
                let pos = st.pos_base_ms.load(Ordering::Relaxed)
                    + st.last_resume.lock().unwrap().map(|t| {
                        (t.elapsed().as_millis() as f64 * speed as f64) as u64
                    }).unwrap_or(0);
                settle(&st);
                st.pos_base_ms.store(pos, Ordering::Relaxed);
                speed = v.clamp(0.25, 4.0);
                st.speed.store(speed.to_bits(), Ordering::Relaxed);
                // 重建当前曲（保位置、保播放态）；无加载则只更新状态
                if let Some(path) = cur_path.clone() {
                    if let Some(mut d) = open_decoder(&path) {
                        decoder_seek_to_dumb(&mut d, pos);
                        let shared: SharedDec = Arc::new(Mutex::new(Some(d)));
                        if let Ok(new_sink) = Sink::try_new(&handle) {
                            fade_out_and_stop(&mut sink, eff_vol(volume, rg_db), 0); // 变速重建瞬时切换（不渐出，保位置优先）
                            next_flag = None;
                            next_path = None;
                            new_sink.append(SharedSource::new(shared.clone(), None, 0, eq.clone(), speed, out_rate));
                            new_sink.set_volume(eff_vol(volume, rg_db));
                            if was_playing { new_sink.play(); } else { new_sink.pause(); }
                            sink = Some(new_sink);
                            dec = Some(shared);
                            *st.last_resume.lock().unwrap() = if was_playing { Some(Instant::now()) } else { None };
                            st.playing.store(was_playing, Ordering::Relaxed);
                            st.ended.store(false, Ordering::Relaxed);
                            st.loaded.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            Ok(AudioCmd::Volume(v)) => {
                volume = v.clamp(0.0, 1.0);
                if let Some(s) = sink.as_ref() {
                    s.set_volume(eff_vol(volume, rg_db));
                }
            }
            Ok(AudioCmd::Stop) => {
                fade_out_and_stop(&mut sink, eff_vol(volume, rg_db), fade_ms);
                dec = None;
                cur_path = None;
                next_flag = None;
                next_path = None;
                skip = None;
                settle(&st);
                st.pos_base_ms.store(0, Ordering::Relaxed);
                st.playing.store(false, Ordering::Relaxed);
                st.ended.store(false, Ordering::Relaxed);
                st.loaded.store(false, Ordering::Relaxed);
            }
            Err(_) => {
                // 推进 seek 丢弃 job：每轮最多丢 10s 音频，期间命令可插队（进 pending）
                if let Some(job) = skip.as_mut() {
                    let need = job.target_ms.saturating_mul(job.rate) / 1000;
                    let goal = need.min(job.skipped + job.rate.saturating_mul(10));
                    let t0 = Instant::now();
                    while job.skipped < goal {
                        {
                            let mut g = job.dec.lock().unwrap();
                            match g.as_mut() {
                                Some(d) => {
                                    if d.next().is_none() {
                                        job.skipped = need; // EOF：直接视为到位
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        job.skipped += 1;
                    }
                    let _ = t0;
                    if job.skipped >= need {
                        // 丢弃完成：换轨（旧曲停止，新位置开始）
                        let job = skip.take().unwrap();
                        settle(&st);
                        match Sink::try_new(&handle) {
                            Ok(new_sink) => {
                                fade_out_and_stop(&mut sink, eff_vol(volume, rg_db), fade_ms);
                                next_flag = None;
                                next_path = None;
                                new_sink.append(SharedSource::new(job.dec.clone(), None, fade_ms, eq.clone(), speed, out_rate));
                                new_sink.set_volume(eff_vol(volume, rg_db));
                                if job.was_playing {
                                    new_sink.play();
                                } else {
                                    new_sink.pause();
                                }
                                sink = Some(new_sink);
                                dec = Some(job.dec);
                                st.pos_base_ms.store(job.target_ms, Ordering::Relaxed);
                                *st.last_resume.lock().unwrap() =
                                    if job.was_playing { Some(Instant::now()) } else { None };
                                st.ended.store(false, Ordering::Relaxed);
                                st.loaded.store(true, Ordering::Relaxed);
                                st.playing.store(job.was_playing, Ordering::Relaxed);
                            }
                            Err(e) => eprintln!("[lyra-audio] seek 完成后重建 Sink 失败: {e}"),
                        }
                    } else if let Ok(c) = rx.try_recv() {
                        pending_cmds.push_back(c); // 命令插队：下一轮主循环处理
                    }
                }
                // 衔接曲开播检测（首次拉取样本时刻 = 精确换曲点）
                if let Some(flag) = next_flag.as_ref() {
                    if flag.started() {
                        eprintln!("[xfade] 衔接开播→notify");
                        st.pos_base_ms.store(0, Ordering::Relaxed);
                        *st.last_resume.lock().unwrap() = Some(flag.started_at());
                        st.playing.store(true, Ordering::Relaxed);
                        st.ended.store(false, Ordering::Relaxed);
                        st.notify_switch.store(true, Ordering::Relaxed);
                        if let Some(p) = next_path.take() {
                            cur_path = Some(p);
                        }
                        next_flag = None;
                    }
                }
                // 超时：检测自然播完（sink 队列耗尽且确实加载过；丢弃进行中不判）
                // flag_open：无衔接，或衔接曲已开播——衔接未开播时不算"播完"
                let flag_open = next_flag.as_ref().map(|f| f.started()).unwrap_or(true);
                let drained = sink.as_ref().map(|s| s.empty()).unwrap_or(false);
                if drained
                    && dec.is_some()
                    && st.playing.load(Ordering::Relaxed)
                    && st.loaded.load(Ordering::Relaxed)
                    && flag_open
                    && skip.is_none()
                {
                    empty_streak += 1;
                    if empty_streak >= 4 {
                        // 连续 4 轮（~100ms）确认耗尽：衔接失败兜底也走这里
                        eprintln!("[xfade] drained 确认→ended（衔接失败兜底或自然播完）");
                        settle(&st);
                        st.playing.store(false, Ordering::Relaxed);
                        st.ended.store(true, Ordering::Relaxed);
                        empty_streak = 0;
                    }
                } else {
                    empty_streak = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成单声道 16-bit PCM WAV（正弦波），返回样本数
    fn write_sine_wav(path: &std::path::Path, sr: u32, hz: f32, secs: f32) -> usize {
        let n = (sr as f32 * secs) as usize;
        let mut data = Vec::with_capacity(44 + n * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + n as u32 * 2).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes()); // PCM
        data.extend_from_slice(&1u16.to_le_bytes()); // mono
        data.extend_from_slice(&sr.to_le_bytes());
        data.extend_from_slice(&(sr * 2).to_le_bytes()); // byte rate
        data.extend_from_slice(&2u16.to_le_bytes()); // block align
        data.extend_from_slice(&16u16.to_le_bytes()); // bits
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(n as u32 * 2).to_le_bytes());
        for i in 0..n {
            let v = (2.0 * std::f32::consts::PI * hz * i as f32 / sr as f32).sin();
            data.extend_from_slice(&((v * 20000.0) as i16).to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
        n
    }

    /// 采样率一致 + 原速 + EQ 关：全链路 bit-exact（f32 无量化损失）
    #[test]
    fn passthrough_is_bit_exact() {
        let p = std::env::temp_dir().join("lyra_test_passthrough.wav");
        write_sine_wav(&p, 44100, 1000.0, 1.0);
        let expect: Vec<f32> = open_decoder(&p).unwrap().collect();
        assert!(!expect.is_empty());

        let dec = open_decoder(&p).unwrap();
        let shared: SharedDec = Arc::new(Mutex::new(Some(dec)));
        let eq = Arc::new(EqShared::new());
        let src = SharedSource::new(shared, None, 0, eq, 1.0, 44100);
        let got: Vec<f32> = src.collect();

        assert_eq!(expect.len(), got.len(), "直通不应增减样本");
        for (i, (a, b)) in expect.iter().zip(got.iter()).enumerate() {
            assert_eq!(a, b, "样本 {i} 不一致：{a} vs {b}");
        }
    }

    /// 44.1k→48k 重采样：时长、频率、幅度均保持
    #[test]
    fn resample_keeps_length_tone_and_gain() {
        let p = std::env::temp_dir().join("lyra_test_resample.wav");
        let n_in = write_sine_wav(&p, 44100, 1000.0, 2.0);
        let mut dec = open_decoder(&p).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel::<f32>(PIPE_CAP);
        resample_push(&mut dec, &tx, 44100.0, 48000);
        drop(tx);
        let out: Vec<f32> = rx.into_iter().collect();

        // 时长：输出 ≈ 输入 × 48/44.1（滤波器冲出的尾巴 < 一小段）
        let expect = n_in as f64 * 48000.0 / 44100.0;
        let ratio = out.len() as f64 / expect;
        assert!(
            ratio > 0.999 && ratio < 1.01,
            "输出样本数 {}，期望约 {}（ratio={ratio}）",
            out.len(),
            expect
        );

        // 频率：正过零计数。跳过头部（滤波器启动）与尾部（EOF 补零 flush 的
        // 衰减振铃）各 100ms，只测主体——主体必须精确保持原频率。
        let body = &out[4800..out.len() - 4800];
        let mut zeros = 0usize;
        let mut prev = body[0];
        for &s in &body[1..] {
            if prev <= 0.0 && s > 0.0 {
                zeros += 1;
            }
            prev = s;
        }
        let dur = body.len() as f64 / 48000.0;
        let freq = zeros as f64 / dur;
        assert!((freq - 1000.0).abs() < 5.0, "频率 {freq:.1} Hz 偏差过大");

        // 幅度：sinc 增益 ≈ 1（1kHz 处滚降应 < 0.5dB）
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let expect_peak = 20000.0 / 32768.0;
        assert!(
            (peak - expect_peak).abs() / expect_peak < 0.06,
            "峰值 {peak:.4}，期望约 {expect_peak:.4}"
        );
    }

    /// 变速折算：44.1k 输入 ×2.0 速 → 虚拟 88.2k → 44.1k 输出，长度减半
    #[test]
    fn speed_folds_into_resample_ratio() {
        let p = std::env::temp_dir().join("lyra_test_speed.wav");
        let n_in = write_sine_wav(&p, 44100, 1000.0, 2.0);
        let mut dec = open_decoder(&p).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel::<f32>(PIPE_CAP);
        resample_push(&mut dec, &tx, 44100.0 * 2.0, 44100);
        drop(tx);
        let out: Vec<f32> = rx.into_iter().collect();
        let ratio = out.len() as f64 / n_in as f64;
        assert!(
            ratio > 0.49 && ratio < 0.52,
            "2× 速输出长度比 {ratio:.3}，应约 0.5"
        );
    }

    /// 平坦 EQ（开启但全 0dB）：平坦段跳过 → 零段直通 → bit-exact
    #[test]
    fn eq_flat_is_transparent() {
        let p = std::env::temp_dir().join("lyra_test_eqflat.wav");
        write_sine_wav(&p, 44100, 1000.0, 1.0);
        let expect: Vec<f32> = open_decoder(&p).unwrap().collect();
        let dec = open_decoder(&p).unwrap();
        let shared: SharedDec = Arc::new(Mutex::new(Some(dec)));
        let eq = Arc::new(EqShared::new());
        eq.enabled.store(true, Ordering::Relaxed); // 开启但增益全 0
        let src = SharedSource::new(shared, None, 0, eq, 1.0, 44100);
        let got: Vec<f32> = src.collect();
        assert_eq!(expect.len(), got.len());
        for (i, (a, b)) in expect.iter().zip(&got).enumerate() {
            assert_eq!(a, b, "平坦 EQ 样本 {i} 不一致：{a} vs {b}");
        }
    }

    /// 满幅音源 + 低频搁架 +12dB：headroom 防削波——
    /// 无 clamp 饱和样本、波形对称、boost 平台段保持等响度
    #[test]
    fn eq_boost_never_clips() {
        let p = std::env::temp_dir().join("lyra_test_eqboost.wav");
        write_sine_wav(&p, 44100, 100.0, 1.0); // 100Hz 处于低架平台内
        let dec = open_decoder(&p).unwrap();
        let shared: SharedDec = Arc::new(Mutex::new(Some(dec)));
        let eq = Arc::new(EqShared::new());
        eq.set_tone(true, 12.0, 0.0); // 低频搁架 +12dB
        let src = SharedSource::new(shared, None, 0, eq, 1.0, 44100);
        let out: Vec<f32> = src.collect();
        // 无硬削波（clamp 会产生恰好 ±1.0 的饱和样本）
        assert!(
            out.iter().all(|s| s.abs() < 1.0),
            "存在饱和样本，headroom 未生效"
        );
        // 稳态段（后半 0.5s）：波形正负对称（削波会破坏对称性）
        let tail = &out[out.len() / 2..];
        let mx = tail.iter().fold(0.0f32, |m, s| m.max(*s));
        let mn = tail.iter().fold(0.0f32, |m, s| m.min(*s));
        assert!(
            (mx + mn).abs() / mx < 0.05,
            "波形不对称（max={mx}, min={mn}），疑似削波"
        );
        // headroom 恰好抵消 12dB 平台增益：boost 段保持等响度
        let expect_peak = 20000.0 / 32768.0;
        assert!(
            mx > expect_peak * 0.8 && mx < expect_peak * 1.2,
            "峰值 {mx:.4} 异常（期望 ≈{expect_peak:.4}）"
        );
    }
}
