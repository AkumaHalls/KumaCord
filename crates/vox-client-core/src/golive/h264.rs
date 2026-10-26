//! Minimal H.264 Annex-B helpers for the `share h264file` test source: split a byte stream into
//! access units and read the coded picture size from SPS NAL units.
//!
//! Not a general parser: it assumes one primary coded picture per access unit (no redundant
//! pictures, no arbitrary slice order), which is what x264/NVENC/MF produce.

use std::ops::Range;

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// One NAL unit: `unit` covers the NAL header + payload, `start` includes the start code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nal {
    pub start: usize,
    pub unit: Range<usize>,
}

impl Nal {
    pub fn nal_type(&self, data: &[u8]) -> u8 {
        data.get(self.unit.start).map(|b| b & 0x1f).unwrap_or(0)
    }
}

/// Find every NAL unit in an Annex-B stream (3- or 4-byte start codes).
pub fn nal_units(data: &[u8]) -> Vec<Nal> {
    let mut starts = Vec::new(); // (start code position, payload position)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(sc, payload)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|n| n.0).unwrap_or(data.len());
        // Trailing zero bytes before the next start code belong to no NAL.
        while end > payload && data[end - 1] == 0 {
            end -= 1;
        }
        if end > payload {
            out.push(Nal { start: sc, unit: payload..end });
        }
    }
    out
}

/// One access unit (a whole coded picture plus its parameter sets / SEI), as a byte range of
/// the stream including start codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub range: Range<usize>,
    pub keyframe: bool,
    /// Picture size from the SPS in effect (the latest one seen at or before this AU).
    pub width: u32,
    pub height: u32,
}

fn is_vcl(t: u8) -> bool {
    (1..=5).contains(&t)
}

/// Split an Annex-B stream into access units. AUs before the first SPS are dropped (size
/// unknown, and they can't be decoded anyway).
pub fn access_units(data: &[u8]) -> Vec<AccessUnit> {
    let nals = nal_units(data);
    let mut out = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_has_vcl = false;
    let mut cur_key = false;
    let mut size: Option<(u32, u32)> = None;
    let mut cur_size: Option<(u32, u32)> = None;

    let flush = |start: usize, end: usize, key: bool, sz: Option<(u32, u32)>, out: &mut Vec<AccessUnit>| {
        if let Some((w, h)) = sz {
            out.push(AccessUnit { range: start..end, keyframe: key, width: w, height: h });
        }
    };

    for nal in &nals {
        let t = nal.nal_type(data);
        // First slice of a new picture: first_mb_in_slice == 0, i.e. ue(v) starts with a 1 bit.
        let new_picture = is_vcl(t) && data.get(nal.unit.start + 1).is_some_and(|b| b & 0x80 != 0);
        let starts_au = matches!(t, NAL_AUD | NAL_SPS | NAL_PPS | NAL_SEI | 14..=18) || new_picture;
        if starts_au && cur_has_vcl {
            if let Some(s) = cur_start.take() {
                flush(s, nal.start, cur_key, cur_size, &mut out);
            }
            cur_has_vcl = false;
            cur_key = false;
        }
        if cur_start.is_none() {
            cur_start = Some(nal.start);
        }
        if t == NAL_SPS {
            if let Some(sz) = sps_resolution(&data[nal.unit.clone()]) {
                size = Some(sz);
            }
        }
        if is_vcl(t) {
            if !cur_has_vcl {
                cur_size = size;
            }
            cur_has_vcl = true;
            cur_key |= t == NAL_IDR;
        }
    }
    if let (Some(s), true) = (cur_start, cur_has_vcl) {
        let end = nals.last().map(|n| n.unit.end).unwrap_or(data.len());
        flush(s, end, cur_key, cur_size, &mut out);
    }
    out
}

/// Remove emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Cropped picture size from an SPS NAL unit (header byte included).
pub fn sps_resolution(nal: &[u8]) -> Option<(u32, u32)> {
    if nal.first()? & 0x1f != NAL_SPS {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut b = Bits { data: &rbsp, pos: 0 };
    let profile_idc = b.bits(8)?;
    b.bits(16)?; // constraint flags + level_idc
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = 0;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = b.bit()?;
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?; // mb_adaptive_frame_field_flag
    }
    b.bit()?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.bit()? == 1 {
        cl = b.ue()?;
        cr = b.ue()?;
        ct = b.ue()?;
        cb = b.ue()?;
    }
    let chroma_array_type = if separate_colour_plane == 1 { 0 } else { chroma_format_idc };
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let (crop_x, crop_y) = if chroma_array_type == 0 { (1, 2 - frame_mbs_only) } else { (sub_w, sub_h * (2 - frame_mbs_only)) };
    let width = (w_mbs * 16).checked_sub(crop_x * (cl + cr))?;
    let height = ((2 - frame_mbs_only) * h_map * 16).checked_sub(crop_y * (ct + cb))?;
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real SPS NAL units written by ffmpeg/libx264 (with emulation-prevention bytes).
    // Baseline 1920x1080 (1088 coded rows, cropped by 8).
    const SPS_1080: [u8; 26] = [
        0x67, 0x42, 0xc0, 0x28, 0xd9, 0x00, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf2, 0x3c,
        0x60, 0xc9, 0x20,
    ];
    // High profile 1280x720.
    const SPS_720_HIGH: [u8; 24] = [
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xb2, 0x00, 0xa0, 0x0b, 0x76, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x07, 0x91, 0xe3, 0x06,
        0x49,
    ];

    #[test]
    fn sps_sizes() {
        assert_eq!(sps_resolution(&SPS_1080), Some((1920, 1080)));
        assert_eq!(sps_resolution(&SPS_720_HIGH), Some((1280, 720)));
        assert_eq!(sps_resolution(&[0x68, 0xce]), None);
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 5]), vec![0, 0, 1, 0, 0, 0, 5]);
    }

    fn stream() -> Vec<u8> {
        let mut s = Vec::new();
        let push = |s: &mut Vec<u8>, four: bool, nal: &[u8]| {
            s.extend_from_slice(if four { &[0, 0, 0, 1] } else { &[0, 0, 1] });
            s.extend_from_slice(nal);
        };
        // AU 0: SPS PPS SEI IDR(2 slices)
        push(&mut s, true, &SPS_1080);
        push(&mut s, true, &[0x68, 0xce, 0x38, 0x80]);
        push(&mut s, false, &[0x06, 0x05, 0x01, 0x80]);
        push(&mut s, false, &[0x65, 0x88, 0x84, 0x80]); // first_mb = 0
        push(&mut s, false, &[0x65, 0x40, 0x84, 0x80]); // first_mb != 0 (same picture)
        // AU 1: P slice
        push(&mut s, false, &[0x41, 0x9a, 0x02, 0x80]);
        // AU 2: AUD + P slice
        push(&mut s, true, &[0x09, 0xf0]);
        push(&mut s, false, &[0x41, 0x9a, 0x04, 0x80]);
        s
    }

    #[test]
    fn split_access_units() {
        let s = stream();
        let nals = nal_units(&s);
        assert_eq!(nals.len(), 8);
        assert_eq!(nals[0].nal_type(&s), NAL_SPS);
        let aus = access_units(&s);
        assert_eq!(aus.len(), 3, "{aus:?}");
        assert!(aus[0].keyframe && !aus[1].keyframe && !aus[2].keyframe);
        assert_eq!((aus[0].width, aus[0].height), (1920, 1080));
        assert_eq!((aus[2].width, aus[2].height), (1920, 1080));
        // AUs are contiguous and cover the stream.
        assert_eq!(aus[0].range.start, 0);
        assert_eq!(aus[0].range.end, aus[1].range.start);
        assert_eq!(aus[1].range.end, aus[2].range.start);
        assert_eq!(aus[2].range.end, s.len());
        // AU 0 contains both IDR slices; AU 2 starts with its AUD.
        assert_eq!(nal_units(&s[aus[0].range.clone()]).len(), 5);
        assert_eq!(s[aus[2].range.start..aus[2].range.start + 5], [0, 0, 0, 1, 0x09]);
    }

    #[test]
    fn drops_aus_before_first_sps() {
        let mut s = vec![0, 0, 1, 0x41, 0x9a, 0x02];
        s.extend_from_slice(&stream());
        let aus = access_units(&s);
        assert_eq!(aus.len(), 3);
        assert!(aus[0].keyframe);
    }
}
