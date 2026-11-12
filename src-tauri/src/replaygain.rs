//! ReplayGain 2.0 分析（EBU R128 门控响度，参考 -18 LUFS）与专辑聚合。
//! 独立于播放链路：换曲时后台把文件重新解码一遍喂给分析器（顺序解码 <1s），
//! 结果落库（rg_track / rg_peak），专辑曲目齐后聚合 rg_album。

/// RBJ 双线性 biquad：(b0,b1,b2,a1,a2)（a0 已归一）
type Biquad = [f64; 5];

fn highshelf(fs: f64, f0: f64, gain_db: f64, q: f64) -> Biquad {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = std::f64::consts::TAU * f0 / fs;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * q);
    let sq = 2.0 * a.sqrt() * alpha;
    let a0 = (a + 1.0) - (a - 1.0) * cw + sq;
    [
        (a * ((a + 1.0) + (a - 1.0) * cw + sq)) / a0,
        (-2.0 * a * ((a - 1.0) + (a + 1.0) * cw)) / a0,
        (a * ((a + 1.0) + (a - 1.0) * cw - sq)) / a0,
        (2.0 * ((a - 1.0) - (a + 1.0) * cw)) / a0,
        ((a + 1.0) - (a - 1.0) * cw - sq) / a0,
    ]
}

fn highpass(fs: f64, f0: f64, q: f64) -> Biquad {
    let w0 = std::f64::consts::TAU * f0 / fs;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * q);
    let a0 = 1.0 + alpha;
    [
        (1.0 + cw) / 2.0 / a0,
        -(1.0 + cw) / a0,
        (1.0 + cw) / 2.0 / a0,
        (-2.0 * cw) / a0,
        (1.0 - alpha) / a0,
    ]
}

/// 单级 DF1 状态
#[derive(Default, Clone, Copy)]
struct Df1 {
    x1: f64, x2: f64, y1: f64, y2: f64,
}

impl Df1 {
    #[inline]
    fn run(&mut self, bq: &Biquad, x: f64) -> f64 {
        let y = bq[0] * x + bq[1] * self.x1 + bq[2] * self.x2 - bq[3] * self.y1 - bq[4] * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// K 加权分析器（BS.1770）：预滤波（搁置+高通，f64 DF1，每通道独立）
/// → 400ms 块门控响度（绝对门 -70 LUFS + 相对门 -10 LU）→ 增益 dB + 样本峰值
pub struct RgAnalyzer {
    shelf: Biquad,
    hp: Biquad,
    stage1: Vec<Df1>, // 搁置级
    stage2: Vec<Df1>, // 高通级
    ch: usize,
    ch_i: usize,
    block_len: usize, // 样本数（含通道）
    filled: usize,
    block_sum: f64,
    block_means: Vec<f64>,
    peak: f32,
}

impl RgAnalyzer {
    pub fn new(fs: u32, channels: usize) -> Self {
        let fsf = fs.max(1) as f64;
        let ch = channels.max(1);
        RgAnalyzer {
            // K 加权：高搁置 +3.999dB @1681.97Hz Q0.7071；高通 38.13Hz Q0.5003
            shelf: highshelf(fsf, 1681.97, 3.999, 0.7071),
            hp: highpass(fsf, 38.13, 0.5003),
            stage1: vec![Df1::default(); ch],
            stage2: vec![Df1::default(); ch],
            ch,
            ch_i: 0,
            block_len: (fs.max(1) as usize * 400 / 1000).max(1) * ch,
            filled: 0,
            block_sum: 0.0,
            block_means: Vec::new(),
            peak: 0.0,
        }
    }

    /// 喂 interleaved f32 样本（任意长度；源采样率数据，不经 EQ/重采样）
    pub fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            self.peak = self.peak.max(s.abs());
            let c = self.ch_i;
            let ys = self.stage1[c].run(&self.shelf, s as f64);
            let yh = self.stage2[c].run(&self.hp, ys);
            self.block_sum += yh * yh;
            self.filled += 1;
            if self.filled >= self.block_len {
                self.block_means.push(self.block_sum / self.block_len as f64);
                self.filled = 0;
                self.block_sum = 0.0;
            }
            self.ch_i = (self.ch_i + 1) % self.ch;
        }
    }

    /// 结束 → (track gain dB, peak 0..=1)。尾部不足一块按长度加权补入。
    pub fn finish(mut self) -> (f64, f32) {
        if self.filled > 0 {
            let w = self.filled as f64 / self.block_len as f64;
            self.block_means.push(self.block_sum / self.filled as f64 * w);
        }
        let gain = if self.block_means.is_empty() {
            0.0
        } else {
            RG_REF - gated_loudness(&self.block_means)
        };
        (gain, self.peak)
    }
}

const RG_REF: f64 = -18.0; // ReplayGain 2.0 参考响度（LUFS）

/// BS.1770 门控：绝对门 -70 LUFS，相对门（门内均值 -10 LU）
fn gated_loudness(blocks: &[f64]) -> f64 {
    const ABS_GATE: f64 = -70.0;
    const REL_GATE: f64 = 10.0;
    let lufs = |e: f64| -0.691 + 10.0 * e.log10();
    let pass1: Vec<f64> = blocks.iter().copied().filter(|&e| lufs(e) > ABS_GATE).collect();
    if pass1.is_empty() {
        return -70.0;
    }
    let mean1 = pass1.iter().sum::<f64>() / pass1.len() as f64;
    let thresh = lufs(mean1) - REL_GATE;
    let pass2: Vec<f64> = pass1.into_iter().filter(|&e| lufs(e) > thresh).collect();
    if pass2.is_empty() {
        return -70.0;
    }
    lufs(pass2.iter().sum::<f64>() / pass2.len() as f64)
}

/// 专辑聚合：各曲（track gain, 时长）按能量-时长加权合成专辑增益
/// （数学上等价整专辑拼接信号的 RMS，门控近似误差 <0.1dB）
pub fn album_gain(tracks: &[(f64, f64)]) -> f64 {
    let mut sum_e = 0.0;
    let mut sum_d = 0.0;
    for &(g, d) in tracks {
        if d <= 0.0 {
            continue;
        }
        // LUFS = -0.691 + 10lg(E) → E = 10^((LUFS+0.691)/10)（-0.691 只在两端各计一次）
        let lufs = RG_REF - g;
        sum_e += d * 10f64.powf((lufs + 0.691) / 10.0);
        sum_d += d;
    }
    if sum_d <= 0.0 {
        return 0.0;
    }
    let lufs = -0.691 + 10.0 * (sum_e / sum_d).log10();
    RG_REF - lufs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(secs: usize, fs: u32, amp: f64) -> Vec<f32> {
        let step = std::f64::consts::TAU * 1000.0 / fs as f64;
        let mut ph = 0.0f64;
        (0..fs as usize * secs)
            .map(|_| {
                let v = (amp * ph.sin()) as f32;
                ph += step;
                v
            })
            .collect()
    }

    /// 校准锚点：1kHz 正弦 -20dBFS（峰 0.1）单声道。
    /// K 加权 1kHz ≈ 0dB → 均方 0.005 → LUFS ≈ -23.7 → gain ≈ +5.7dB
    #[test]
    fn sine_calibration() {
        let mut an = RgAnalyzer::new(44100, 1);
        for chunk in sine(5, 44100, 0.1).chunks(4096) {
            an.feed(chunk);
        }
        let (gain, peak) = an.finish();
        assert!((gain - 5.7).abs() < 0.35, "gain {gain:.2} 偏离 +5.7dB 过多");
        assert!((peak - 0.1).abs() < 1e-6);
    }

    /// 双声道与单声道结果一致（能量按通道平均）
    #[test]
    fn stereo_matches_mono() {
        let mono = {
            let mut an = RgAnalyzer::new(48000, 1);
            an.feed(&sine(3, 48000, 0.1));
            an.finish().0
        };
        let stereo = {
            let src = sine(3, 48000, 0.1);
            let il: Vec<f32> = src.iter().flat_map(|&v| [v, v]).collect();
            let mut an = RgAnalyzer::new(48000, 2);
            an.feed(&il);
            an.finish().0
        };
        assert!((mono - stereo).abs() < 0.02, "单声道 {mono:.2} vs 立体声 {stereo:.2}");
    }

    /// 门控：前半静音不拉低后半正弦的响度
    #[test]
    fn gating_ignores_silence() {
        let mut an = RgAnalyzer::new(48000, 1);
        an.feed(&vec![0f32; 48000 * 3]);
        an.feed(&sine(3, 48000, 0.1));
        let (gain, _) = an.finish();
        assert!((gain - 5.7).abs() < 0.35, "静音段未被门控：gain {gain:.2}");
    }

    /// 专辑聚合：等响度 → 同值；一半 -6dB（能量 4:1）→ 能量平均 = +3.97dB → 5.7-3.97=1.73
    /// （dB 域不对称是能量平均的本性：均值总被更响的一首拉高）
    #[test]
    fn album_aggregation() {
        let a = album_gain(&[(5.7, 180.0), (5.7, 180.0)]);
        assert!((a - 5.7).abs() < 0.05, "等响度专辑 {a:.2}");
        let b = album_gain(&[(5.7, 180.0), (-0.3, 180.0)]);
        assert!((b - 1.73).abs() < 0.05, "半程 -6dB 专辑 {b:.2}");
        // +6dB 首更安静（E/4）：均值 0.625E = -2.04dB → gain = 5.7+2.04 = 7.74
        let c = album_gain(&[(5.7, 180.0), (11.7, 180.0)]);
        assert!((c - 7.74).abs() < 0.05, "半程 +6dB 专辑 {c:.2}");
    }
}
