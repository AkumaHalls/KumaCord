//! Encoder tuning knobs per backend. Production uses [`Tuning::default`]; the quality benchmark
//! (`capture_probe bench`) sweeps other values through [`NvTuning::parse`] / [`MfTuning::parse`].
//! Results and the reasoning behind the defaults: `docs/quality-bench.md`.

use std::fmt;

/// NVENC settings (on top of: H.264 Main, IPPP, infinite GOP, IDR on demand, no lookahead).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NvTuning {
    /// Preset P1 (fastest) ..= P7 (best).
    pub preset: u8,
    /// `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` (true) or `LOW_LATENCY` (false).
    pub ull: bool,
    /// VBR with `maxBitRate == averageBitRate` (constrained) instead of CBR.
    pub vbr: bool,
    /// VBV size in frames at the configured rate.
    pub vbv_frames: f32,
    /// Spatial AQ: `None` off, `Some(0)` auto strength, `Some(1..=15)`.
    pub aq: Option<u8>,
    pub temporal_aq: bool,
    /// `None` = preset default, else `NV_ENC_MULTI_PASS` (0 off, 1 quarter-res, 2 full-res).
    pub multipass: Option<u8>,
    /// `lowDelayKeyFrameScale` (I/P bit ratio with a 1-frame VBV); 0 = preset default.
    pub key_scale: u8,
    /// Max QP for every frame type (0 = none).
    pub max_qp: u8,
}

/// Media Foundation H.264 MFT settings (on top of: low-latency mode, no B-frames, long GOP).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MfTuning {
    /// `eAVEncCommonRateControlMode` (0 CBR, 1 PeakConstrainedVBR, 3 Quality, 4 LowDelayVBR).
    pub rc: u32,
    /// `CODECAPI_AVEncCommonQualityVsSpeed` 0 (speed) ..= 100 (quality); `None` = MFT default.
    pub quality_vs_speed: Option<u32>,
    /// `CODECAPI_AVEncVideoMaxQP` / `MinQP`; `None` = MFT default.
    pub max_qp: Option<u32>,
    pub min_qp: Option<u32>,
    /// `CODECAPI_AVEncCommonBufferSize` in frames.
    pub vbv_frames: f32,
    /// VBR modes: `CODECAPI_AVEncCommonMaxBitRate = peak_factor * mean`.
    pub peak_factor: f32,
    /// Start the MFT at 3x the target and lower it once streaming (the NVIDIA MFT never goes
    /// above its start bitrate, so without this raising the bitrate mid-stream does nothing).
    pub headroom: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub nv: NvTuning,
    pub mf: MfTuning,
}

impl NvTuning {
    /// What v0.1 shipped: P4 + ULL, CBR, 1-frame VBV, preset defaults otherwise.
    pub const V01: Self = Self {
        preset: 4,
        ull: true,
        vbr: false,
        vbv_frames: 1.0,
        aq: None,
        temporal_aq: false,
        multipass: None,
        key_scale: 0,
        max_qp: 0,
    };

    /// v0.2 default (see `docs/quality-bench.md`).
    /// v0.2 default: P4 with LOW_LATENCY tuning. ULL under-spends the target on text (70% of
    /// it at 1080p60) and its first frames are much worse; P5/P6 add ~1 ms for no measurable
    /// gain; AQ, VBR, a 2-frame VBV and a max QP were neutral or worse (`docs/quality-bench.md`).
    pub const DEFAULT: Self = Self {
        preset: 4,
        ull: false,
        vbr: false,
        vbv_frames: 1.0,
        aq: None,
        temporal_aq: false,
        multipass: None,
        key_scale: 0,
        max_qp: 0,
    };

    /// Apply comma-separated overrides to `self`:
    /// `p1..p7`, `ull`, `ll`, `cbr`, `vbr`, `vbv<frames>`, `aq` / `aq<1-15>` / `noaq`, `taq`,
    /// `mp0|mp1|mp2`, `ks<n>`, `maxqp<n>`, `v01` (reset to the v0.1 settings), `def`.
    pub fn parse(mut self, spec: &str) -> Result<Self, String> {
        for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let num = |p: &str| -> Result<f32, String> {
                tok[p.len()..].parse::<f32>().map_err(|_| format!("bad NVENC option {tok:?}"))
            };
            match tok {
                "v01" => self = Self::V01,
                "def" => self = Self::DEFAULT,
                "ull" => self.ull = true,
                "ll" => self.ull = false,
                "cbr" => self.vbr = false,
                "vbr" => self.vbr = true,
                "aq" => self.aq = Some(0),
                "noaq" => self.aq = None,
                "taq" => self.temporal_aq = true,
                "notaq" => self.temporal_aq = false,
                t if t.starts_with("vbv") => self.vbv_frames = num("vbv")?.clamp(0.25, 30.0),
                t if t.starts_with("aq") => self.aq = Some(num("aq")?.clamp(1.0, 15.0) as u8),
                t if t.starts_with("mp") => self.multipass = Some(num("mp")?.clamp(0.0, 2.0) as u8),
                t if t.starts_with("ks") => self.key_scale = num("ks")?.clamp(0.0, 10.0) as u8,
                t if t.starts_with("maxqp") => self.max_qp = num("maxqp")?.clamp(0.0, 51.0) as u8,
                t if t.starts_with('p') && t.len() == 2 => {
                    self.preset = num("p")?.clamp(1.0, 7.0) as u8
                }
                _ => return Err(format!("unknown NVENC option {tok:?}")),
            }
        }
        Ok(self)
    }
}

impl fmt::Display for NvTuning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "P{} {} {} vbv{}",
            self.preset,
            if self.ull { "ULL" } else { "LL" },
            if self.vbr { "VBR" } else { "CBR" },
            self.vbv_frames
        )?;
        match self.aq {
            Some(0) => write!(f, " aq")?,
            Some(s) => write!(f, " aq{s}")?,
            None => {}
        }
        if self.temporal_aq {
            write!(f, " taq")?;
        }
        if let Some(m) = self.multipass {
            write!(f, " mp{m}")?;
        }
        if self.key_scale > 0 {
            write!(f, " ks{}", self.key_scale)?;
        }
        if self.max_qp > 0 {
            write!(f, " maxqp{}", self.max_qp)?;
        }
        Ok(())
    }
}

impl MfTuning {
    /// What v0.1 shipped: CBR, 1-frame buffer, MFT defaults otherwise.
    pub const V01: Self = Self {
        rc: 0,
        quality_vs_speed: None,
        max_qp: None,
        min_qp: None,
        vbv_frames: 1.0,
        peak_factor: 1.0,
        headroom: false,
    };

    /// v0.2 default: quality-vs-speed 100 (text at 1080p30: SSIM 0.977 -> 0.985, worst 5%
    /// 0.885 -> 0.931; neutral on other content). The NVIDIA MFT ignores the VBR modes.
    pub const DEFAULT: Self = Self {
        rc: 0,
        quality_vs_speed: Some(100),
        max_qp: None,
        min_qp: None,
        vbv_frames: 1.0,
        peak_factor: 1.0,
        headroom: true,
    };

    /// Comma-separated overrides: `cbr`, `pvbr`, `qvbr`, `ldvbr`, `qs<0-100>`, `maxqp<n>`,
    /// `minqp<n>`, `vbv<frames>`, `peak<factor>`, `v01`, `def`.
    pub fn parse(mut self, spec: &str) -> Result<Self, String> {
        for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let num = |p: &str| -> Result<f32, String> {
                tok[p.len()..].parse::<f32>().map_err(|_| format!("bad MF option {tok:?}"))
            };
            match tok {
                "v01" => self = Self::V01,
                "def" => self = Self::DEFAULT,
                "cbr" => self.rc = 0,
                "pvbr" => self.rc = 1,
                "qvbr" => self.rc = 3,
                "ldvbr" => self.rc = 4,
                "head" => self.headroom = true,
                "nohead" => self.headroom = false,
                t if t.starts_with("qs") => self.quality_vs_speed = Some(num("qs")?.clamp(0.0, 100.0) as u32),
                t if t.starts_with("maxqp") => self.max_qp = Some(num("maxqp")?.clamp(0.0, 51.0) as u32),
                t if t.starts_with("minqp") => self.min_qp = Some(num("minqp")?.clamp(0.0, 51.0) as u32),
                t if t.starts_with("vbv") => self.vbv_frames = num("vbv")?.clamp(0.25, 30.0),
                t if t.starts_with("peak") => self.peak_factor = num("peak")?.clamp(1.0, 4.0),
                _ => return Err(format!("unknown MF option {tok:?}")),
            }
        }
        Ok(self)
    }
}

impl fmt::Display for MfTuning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rc = match self.rc {
            0 => "CBR",
            1 => "PeakVBR",
            3 => "QualityVBR",
            4 => "LowDelayVBR",
            _ => "rc?",
        };
        write!(f, "{rc} vbv{}", self.vbv_frames)?;
        if self.rc != 0 {
            write!(f, " peak{}", self.peak_factor)?;
        }
        if let Some(q) = self.quality_vs_speed {
            write!(f, " qs{q}")?;
        }
        if let Some(q) = self.max_qp {
            write!(f, " maxqp{q}")?;
        }
        if let Some(q) = self.min_qp {
            write!(f, " minqp{q}")?;
        }
        if self.headroom {
            write!(f, " head")?;
        }
        Ok(())
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            nv: NvTuning::DEFAULT,
            mf: MfTuning::DEFAULT,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nv() {
        let t = NvTuning::V01.parse("p6, ll, vbr, vbv1.5, aq8, taq, mp1, ks2, maxqp42").unwrap();
        assert_eq!(t.preset, 6);
        assert!(!t.ull && t.vbr && t.temporal_aq);
        assert_eq!(t.vbv_frames, 1.5);
        assert_eq!(t.aq, Some(8));
        assert_eq!(t.multipass, Some(1));
        assert_eq!((t.key_scale, t.max_qp), (2, 42));
        assert_eq!(t.parse("v01").unwrap(), NvTuning::V01);
        assert_eq!(NvTuning::V01.parse("aq").unwrap().aq, Some(0));
        assert!(NvTuning::V01.parse("p9x").is_err());
        assert!(NvTuning::V01.parse("turbo").is_err());
        assert_eq!(format!("{}", NvTuning::V01), "P4 ULL CBR vbv1");
    }

    #[test]
    fn parse_mf() {
        let t = MfTuning::V01.parse("pvbr,qs70,maxqp40,minqp10,vbv2,peak1.5,head").unwrap();
        assert!(t.headroom);
        assert_eq!(t.rc, 1);
        assert_eq!(t.quality_vs_speed, Some(70));
        assert_eq!((t.max_qp, t.min_qp), (Some(40), Some(10)));
        assert_eq!((t.vbv_frames, t.peak_factor), (2.0, 1.5));
        assert!(MfTuning::V01.parse("x").is_err());
        assert_eq!(format!("{}", MfTuning::V01), "CBR vbv1");
    }
}
