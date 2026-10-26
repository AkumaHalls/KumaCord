//! A small, defensive WebM / Matroska demuxer for Opus audio (`.webm`, `.mka`, `.mkv`: what a
//! YouTube audio download or a browser recording is).
//!
//! Why not symphonia's own `mkv` feature: its demuxer panics on truncated or damaged files (an
//! `expect` on an element header at end of stream in the Cues and track parsing, a slice in lacing
//! and a missing bounds check, found by cutting real WebM files at 200 points and flipping bytes: about
//! 40 % of the damaged inputs panic), and this app is built with `panic = "abort"`, so a cut
//! download would close the program. This reader returns an error instead, for any input: no
//! indexing without a bounds check, no allocation sized by the file (blocks are capped), nothing
//! that recurses.
//!
//! It is deliberately flat: it walks the element stream in order, "descends" into the few master
//! elements it cares about (Segment, Tracks, TrackEntry, Audio, Cluster, BlockGroup) by simply
//! reading on, and skips everything else by its size. That also makes unknown-size (live) segments
//! and clusters work, and it never needs the seek index. Packets come out as symphonia
//! [`Packet`]s so the decode loop in [`super::process`] treats them like any other container's.
//!
//! What it handles: the first `A_OPUS` audio track (`CodecPrivate` is the `OpusHead` header),
//! `SimpleBlock` and `Block` with no, Xiph, fixed-size or EBML lacing, and `DiscardPadding` on the
//! last block (Opus's end padding, in ns). The start is trimmed by the header's pre-skip.
//! Other codecs in Matroska (Vorbis, AAC, FLAC...) are refused with a message.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Seek};

use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::Packet;
use symphonia::core::io::MediaSource;

use super::opus::{OpusHead, OPUS_RATE};
use super::process::ProcessError;

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_AUDIO: u32 = 0xE1;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_DISCARD_PADDING: u32 = 0x75A2;

/// A block (one or more Opus packets) bigger than this is not Opus audio.
const MAX_BLOCK: u64 = 4 * 1024 * 1024;
/// `CodecPrivate` (an `OpusHead`) and other small values.
const MAX_SMALL: u64 = 64 * 1024;
/// Most packets one laced block may hold.
const MAX_LACES: usize = 255;
/// Track type of audio.
const TRACK_AUDIO: u64 = 2;

/// Is this the start of an EBML (Matroska / WebM) file?
pub fn is_matroska(first_bytes: &[u8]) -> bool {
    first_bytes.starts_with(&ID_EBML.to_be_bytes())
}

/// One element's header.
struct Header {
    id: u32,
    /// `None` = unknown size (live streams).
    size: Option<u64>,
}

#[derive(Default, Debug)]
struct TrackInfo {
    number: u64,
    kind: u64,
    codec: String,
    private: Vec<u8>,
}

/// Why reading stopped.
enum Stop {
    /// A clean end of the data.
    Eof,
    /// The structure is damaged.
    Bad(&'static str),
}

type Rd = BufReader<Box<dyn MediaSource>>;

pub struct WebmOpus {
    rd: Rd,
    track: u64,
    /// Packets ready to hand over, and the last one of the latest block, held back until the
    /// element after it shows whether it carries `DiscardPadding`.
    ready: VecDeque<Packet>,
    held: Option<Packet>,
    ended: Option<Stop>,
    /// Within a cluster: the packet counter, as a (fake) timestamp.
    ts: u64,
}

impl WebmOpus {
    /// Read up to the first cluster and find the Opus track. The decoder's header is returned too.
    pub fn open(mut source: Box<dyn MediaSource>) -> Result<(Self, OpusHead), ProcessError> {
        source.rewind().map_err(|_| unsupported("não consegui ler o arquivo"))?;
        let mut rd: Rd = BufReader::with_capacity(64 * 1024, source);
        let bad = |why: &str| unsupported(format!("WebM/Matroska inválido ({why})"));
        // The EBML header, then the segment: the tracks come before the first cluster.
        let first = read_header(&mut rd).map_err(|_| bad("sem cabeçalho"))?;
        if first.id != ID_EBML {
            return Err(bad("sem cabeçalho"));
        }
        skip(&mut rd, first.size).map_err(|_| bad("cabeçalho EBML"))?;
        let mut tracks: Vec<TrackInfo> = Vec::new();
        loop {
            let h = match read_header(&mut rd) {
                Ok(h) => h,
                Err(Stop::Eof) => return Err(unsupported("WebM/Matroska sem áudio")),
                Err(Stop::Bad(w)) => return Err(bad(w)),
            };
            match h.id {
                ID_SEGMENT | ID_TRACKS | ID_AUDIO => {}
                ID_TRACK_ENTRY => tracks.push(TrackInfo::default()),
                ID_TRACK_NUMBER => {
                    let v = read_uint(&mut rd, h.size).map_err(|_| bad("trilha"))?;
                    if let Some(t) = tracks.last_mut() {
                        t.number = v;
                    }
                }
                ID_TRACK_TYPE => {
                    let v = read_uint(&mut rd, h.size).map_err(|_| bad("trilha"))?;
                    if let Some(t) = tracks.last_mut() {
                        t.kind = v;
                    }
                }
                ID_CODEC_ID => {
                    let v = read_bytes(&mut rd, h.size, MAX_SMALL).map_err(|_| bad("codec"))?;
                    if let Some(t) = tracks.last_mut() {
                        t.codec = String::from_utf8_lossy(&v).trim_end_matches('\0').to_string();
                    }
                }
                ID_CODEC_PRIVATE => {
                    let v = read_bytes(&mut rd, h.size, MAX_SMALL).map_err(|_| bad("codec"))?;
                    if let Some(t) = tracks.last_mut() {
                        t.private = v;
                    }
                }
                // The first cluster: the track list is complete.
                ID_CLUSTER => break,
                _ => skip(&mut rd, h.size).map_err(|_| bad("elemento"))?,
            }
        }
        let opus = tracks.iter().find(|t| t.codec == "A_OPUS" && (t.kind == TRACK_AUDIO || t.kind == 0));
        let Some(track) = opus else {
            let other = tracks.iter().find(|t| t.kind == TRACK_AUDIO).map(|t| t.codec.clone());
            return Err(match other {
                Some(c) => unsupported(format!("codec não suportado em WebM/Matroska: {c}; só Opus")),
                None => unsupported("WebM/Matroska sem áudio"),
            });
        };
        let head = OpusHead::parse(&track.private)?;
        let reader = Self { rd, track: track.number, ready: VecDeque::new(), held: None, ended: None, ts: 0 };
        Ok((reader, head))
    }

    /// The next Opus packet; `trim_end` is the frames to cut from its decoded end.
    pub fn track_id(&self) -> u32 {
        self.track as u32
    }

    pub fn next_packet(&mut self) -> Result<Packet, SymError> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(p);
            }
            match self.ended.take() {
                Some(Stop::Eof) => {
                    self.ended = Some(Stop::Eof);
                    return Err(SymError::IoError(std::io::ErrorKind::UnexpectedEof.into()));
                }
                Some(Stop::Bad(why)) => {
                    self.ended = Some(Stop::Eof);
                    return Err(SymError::DecodeError(why));
                }
                None => {}
            }
            if let Err(stop) = self.step() {
                if let Some(h) = self.held.take() {
                    self.ready.push_back(h);
                }
                self.ended = Some(stop);
            }
        }
    }

    /// Read one element.
    fn step(&mut self) -> Result<(), Stop> {
        let h = read_header(&mut self.rd)?;
        match h.id {
            // Masters entered by reading on.
            ID_SEGMENT | ID_CLUSTER | ID_BLOCK_GROUP => Ok(()),
            ID_SIMPLE_BLOCK | ID_BLOCK => {
                let size = h.size.ok_or(Stop::Bad("bloco sem tamanho"))?;
                if size > MAX_BLOCK {
                    return skip(&mut self.rd, h.size);
                }
                let data = read_bytes(&mut self.rd, h.size, MAX_BLOCK)?;
                self.take_block(&data)
            }
            ID_DISCARD_PADDING => {
                let ns = read_int(&mut self.rd, h.size)?;
                if let (Some(held), true) = (self.held.as_mut(), ns > 0) {
                    held.trim_end = (ns as u128 * OPUS_RATE as u128 / 1_000_000_000) as u32;
                }
                Ok(())
            }
            _ => skip(&mut self.rd, h.size),
        }
    }

    /// Split a block into its packets (lacing) and queue them.
    fn take_block(&mut self, block: &[u8]) -> Result<(), Stop> {
        let (track, used) = read_vint(block).ok_or(Stop::Bad("bloco curto"))?;
        let rest = block.get(used..).ok_or(Stop::Bad("bloco curto"))?;
        if rest.len() < 3 {
            return Err(Stop::Bad("bloco curto"));
        }
        if track != self.track {
            return Ok(());
        }
        let flags = rest[2];
        let body = &rest[3..];
        let frames = split_laces((flags >> 1) & 3, body)?;
        // The previous block's held packet is final now.
        if let Some(h) = self.held.take() {
            self.ready.push_back(h);
        }
        let n = frames.len();
        for (i, f) in frames.into_iter().enumerate() {
            self.ts += 1;
            let p = Packet::new_from_boxed_slice(self.track as u32, self.ts, 0, f.into_boxed_slice());
            if i + 1 == n {
                self.held = Some(p);
            } else {
                self.ready.push_back(p);
            }
        }
        Ok(())
    }
}

fn unsupported(e: impl std::fmt::Display) -> ProcessError {
    ProcessError::Unsupported(e.to_string())
}

fn io_stop(e: std::io::Error) -> Stop {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Stop::Eof
    } else {
        Stop::Bad("erro de leitura")
    }
}

fn read_u8(rd: &mut Rd) -> Result<u8, Stop> {
    let mut b = [0u8; 1];
    rd.read_exact(&mut b).map_err(io_stop)?;
    Ok(b[0])
}

/// An element ID: 1 to 4 bytes, the length marked by the leading ones (the marker bits are kept).
fn read_header(rd: &mut Rd) -> Result<Header, Stop> {
    let b0 = read_u8(rd)?;
    let id_len = b0.leading_zeros() as usize + 1;
    if b0 == 0 || id_len > 4 {
        return Err(Stop::Bad("id inválido"));
    }
    let mut id = b0 as u32;
    for _ in 1..id_len {
        id = id << 8 | read_u8(rd)? as u32;
    }
    // The size: 1 to 8 bytes, marker removed; all ones = unknown.
    let s0 = read_u8(rd)?;
    if s0 == 0 {
        return Err(Stop::Bad("tamanho inválido"));
    }
    let len = s0.leading_zeros() as usize + 1;
    let mut size = (s0 as u64) & (0xFF >> len);
    let mut all_ones = size == (0xFF >> len) as u64;
    for _ in 1..len {
        let b = read_u8(rd)?;
        all_ones &= b == 0xFF;
        size = size << 8 | b as u64;
    }
    Ok(Header { id, size: if all_ones { None } else { Some(size) } })
}

/// A variable-length integer at the start of `b`: (value with the marker removed, bytes used).
fn read_vint(b: &[u8]) -> Option<(u64, usize)> {
    let b0 = *b.first()?;
    if b0 == 0 {
        return None;
    }
    let len = b0.leading_zeros() as usize + 1;
    let bytes = b.get(..len)?;
    let mut v = (b0 as u64) & (0xFF >> len);
    for &x in &bytes[1..] {
        v = v << 8 | x as u64;
    }
    Some((v, len))
}

/// Skip an element's data (an unknown size is only valid for masters, which are never skipped).
fn skip(rd: &mut Rd, size: Option<u64>) -> Result<(), Stop> {
    let n = size.ok_or(Stop::Bad("elemento sem tamanho"))?;
    if n > i64::MAX as u64 / 2 {
        return Err(Stop::Bad("tamanho absurdo"));
    }
    rd.seek_relative(n as i64).map_err(io_stop)
}

/// Read `size` bytes (at most `cap`). A file that ends first is a clean end.
fn read_bytes(rd: &mut Rd, size: Option<u64>, cap: u64) -> Result<Vec<u8>, Stop> {
    let n = size.ok_or(Stop::Bad("elemento sem tamanho"))?;
    if n > cap {
        return Err(Stop::Bad("elemento grande demais"));
    }
    let mut v = Vec::new();
    rd.by_ref().take(n).read_to_end(&mut v).map_err(io_stop)?;
    if (v.len() as u64) < n {
        return Err(Stop::Eof);
    }
    Ok(v)
}

fn read_uint(rd: &mut Rd, size: Option<u64>) -> Result<u64, Stop> {
    let b = read_bytes(rd, size, 8)?;
    Ok(b.iter().fold(0u64, |a, &x| a << 8 | x as u64))
}

fn read_int(rd: &mut Rd, size: Option<u64>) -> Result<i64, Stop> {
    let b = read_bytes(rd, size, 8)?;
    let Some(&first) = b.first() else { return Ok(0) };
    let init = if first & 0x80 != 0 { -1i64 } else { 0 };
    Ok(b.iter().fold(init, |a, &x| a << 8 | x as i64))
}

/// The packets of a block body, by lacing type (0 none, 1 Xiph, 2 fixed-size, 3 EBML).
fn split_laces(lacing: u8, body: &[u8]) -> Result<Vec<Vec<u8>>, Stop> {
    let bad = Stop::Bad("lacing inválido");
    if lacing == 0 {
        return Ok(vec![body.to_vec()]);
    }
    let count = *body.first().ok_or(Stop::Bad("lacing inválido"))? as usize + 1;
    if count > MAX_LACES {
        return Err(bad);
    }
    let mut pos = 1usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut n = 0usize;
                loop {
                    let b = *body.get(pos).ok_or(Stop::Bad("lacing inválido"))?;
                    pos += 1;
                    n += b as usize;
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(n);
            }
        }
        3 => {
            // The first size is plain, the others are signed differences from the one before; the
            // last is what remains (so one packet has no size field at all).
            let mut prev = 0i64;
            for i in 0..count - 1 {
                let (raw, used) = read_vint(body.get(pos..).ok_or(Stop::Bad("lacing inválido"))?).ok_or(Stop::Bad("lacing inválido"))?;
                pos += used;
                prev = if i == 0 { raw as i64 } else { prev + raw as i64 - ((1i64 << (7 * used - 1)) - 1) };
                if prev < 0 {
                    return Err(bad);
                }
                sizes.push(prev as usize);
            }
        }
        _ => {
            let rest = body.len() - pos;
            if !rest.is_multiple_of(count) {
                return Err(bad);
            }
            sizes.extend(std::iter::repeat_n(rest / count, count - 1));
        }
    }
    let data = body.get(pos..).ok_or(Stop::Bad("lacing inválido"))?;
    let used: usize = sizes.iter().sum();
    let last = data.len().checked_sub(used).ok_or(Stop::Bad("lacing inválido"))?;
    sizes.push(last);
    let mut out = Vec::with_capacity(count);
    let mut at = 0usize;
    for n in sizes {
        out.push(data.get(at..at + n).ok_or(Stop::Bad("lacing inválido"))?.to_vec());
        at += n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn vint_size(n: usize) -> Vec<u8> {
        // 4-byte size.
        let v = (n as u32) | 0x1000_0000;
        v.to_be_bytes().to_vec()
    }

    fn el(id: u32, data: &[u8]) -> Vec<u8> {
        let mut idb = id.to_be_bytes().to_vec();
        while idb.len() > 1 && idb[0] == 0 {
            idb.remove(0);
        }
        let mut v = idb;
        v.extend(vint_size(data.len()));
        v.extend(data);
        v
    }

    fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.extend([1, channels]);
        v.extend(pre_skip.to_le_bytes());
        v.extend(48_000u32.to_le_bytes());
        v.extend(0i16.to_le_bytes());
        v.push(0);
        v
    }

    fn block(track: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x80 | track, 0, 0, flags];
        v.extend(body);
        v
    }

    fn file(tracks: &[u8], clusters: &[u8]) -> Vec<u8> {
        let mut seg = el(ID_TRACKS, tracks);
        seg.extend(clusters);
        let mut f = el(ID_EBML, &el(0x4282, b"webm"));
        f.extend(el(ID_SEGMENT, &seg));
        f
    }

    fn opus_track(number: u8) -> Vec<u8> {
        let mut t = el(ID_TRACK_NUMBER, &[number]);
        t.extend(el(ID_TRACK_TYPE, &[2]));
        t.extend(el(ID_CODEC_ID, b"A_OPUS"));
        t.extend(el(ID_CODEC_PRIVATE, &opus_head(2, 312)));
        t.extend(el(ID_AUDIO, &el(0x9F, &[2])));
        el(ID_TRACK_ENTRY, &t)
    }

    fn packets(f: &[u8]) -> (Vec<(Vec<u8>, u32)>, OpusHead) {
        let (mut r, head) = WebmOpus::open(Box::new(Cursor::new(f.to_vec()))).unwrap();
        let mut out = Vec::new();
        while let Ok(p) = r.next_packet() {
            out.push((p.data.to_vec(), p.trim_end));
        }
        (out, head)
    }

    #[test]
    fn blocks_become_packets_and_the_last_one_gets_its_discard_padding() {
        let mut cl = el(0xE7, &[0]); // timecode, skipped
        cl.extend(el(ID_SIMPLE_BLOCK, &block(1, 0x80, b"AAAA")));
        cl.extend(el(ID_SIMPLE_BLOCK, &block(2, 0x80, b"other track")));
        let mut group = el(ID_BLOCK, &block(1, 0, b"BB"));
        group.extend(el(0x9B, &[20])); // BlockDuration
        group.extend(el(ID_DISCARD_PADDING, &[0x0B, 0x71, 0xB0])); // 750 000 ns = 36 samples
        cl.extend(el(ID_BLOCK_GROUP, &group));
        let f = file(&opus_track(1), &el(ID_CLUSTER, &cl));
        let (p, head) = packets(&f);
        assert_eq!((head.channels, head.pre_skip), (2, 312));
        assert_eq!(p, vec![(b"AAAA".to_vec(), 0), (b"BB".to_vec(), 36)]);
    }

    #[test]
    fn all_three_lacings_split_into_the_same_packets() {
        let xiph = {
            let mut b = vec![2u8, 3, 2]; // 3 frames, sizes 3 and 2 (then the rest)
            b.extend(b"aaabbcccc");
            block(1, 0x02, &b)
        };
        let fixed = block(1, 0x04, &[&[2u8][..], b"aabbcc"].concat());
        let ebml = {
            // 3 frames: first size 3 (vint 0x83), then a signed difference of -1 (0xBE = 62 - 63).
            let mut b = vec![2u8, 0x83, 0xBE];
            b.extend(b"aaabbcccc");
            block(1, 0x06, &b)
        };
        let want = |sizes: &[usize]| {
            let all = b"aaabbcccc";
            let mut at = 0;
            sizes.iter().map(|&n| { at += n; (all[at - n..at].to_vec(), 0u32) }).collect::<Vec<_>>()
        };
        let one = |blk: Vec<u8>| packets(&file(&opus_track(1), &el(ID_CLUSTER, &el(ID_SIMPLE_BLOCK, &blk)))).0;
        assert_eq!(one(xiph), want(&[3, 2, 4]));
        assert_eq!(one(ebml), want(&[3, 2, 4]));
        assert_eq!(one(fixed), vec![(b"aa".to_vec(), 0), (b"bb".to_vec(), 0), (b"cc".to_vec(), 0)]);
    }

    #[test]
    fn unknown_sizes_and_other_codecs() {
        // A live stream: the segment and the cluster have unknown size (all ones).
        let mut f = el(ID_EBML, &[]);
        f.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        f.extend(el(ID_TRACKS, &opus_track(1)));
        f.extend([0x1F, 0x43, 0xB6, 0x75, 0xFF]);
        f.extend(el(ID_SIMPLE_BLOCK, &block(1, 0x80, b"live")));
        assert_eq!(packets(&f).0, vec![(b"live".to_vec(), 0)]);
        // Vorbis in WebM is refused with its name.
        let mut t = el(ID_TRACK_NUMBER, &[1]);
        t.extend(el(ID_TRACK_TYPE, &[2]));
        t.extend(el(ID_CODEC_ID, b"A_VORBIS"));
        let f = file(&el(ID_TRACK_ENTRY, &t), &el(ID_CLUSTER, &[]));
        match WebmOpus::open(Box::new(Cursor::new(f))) {
            Err(ProcessError::Unsupported(m)) => assert!(m.contains("A_VORBIS"), "{m}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn damaged_input_is_an_error_never_a_panic() {
        let mut cl = Vec::new();
        for i in 0..20u8 {
            cl.extend(el(ID_SIMPLE_BLOCK, &block(1, 0x80, &[i; 40])));
        }
        let f = file(&opus_track(1), &el(ID_CLUSTER, &cl));
        // Every cut: open fails or yields some packets.
        for cut in 0..f.len() {
            if let Ok((mut r, _)) = WebmOpus::open(Box::new(Cursor::new(f[..cut].to_vec()))) {
                while r.next_packet().is_ok() {}
            }
        }
        // Every single-byte change, to a few values.
        for at in 0..f.len() {
            for v in [0u8, 0xFF, 0x80, 0x7F] {
                let mut g = f.clone();
                g[at] = v;
                if let Ok((mut r, _)) = WebmOpus::open(Box::new(Cursor::new(g))) {
                    while r.next_packet().is_ok() {}
                }
            }
        }
        // Not Matroska at all.
        assert!(WebmOpus::open(Box::new(Cursor::new(b"RIFF....WAVE".to_vec()))).is_err());
        assert!(!is_matroska(b"OggS") && is_matroska(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]));
    }
}
