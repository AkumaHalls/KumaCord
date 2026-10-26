//! The upload pipeline on real encoded files: MP3, OGG (Vorbis) and FLAC fixtures generated from a
//! sine (`tests/fixtures`: 0.3 s silence, 0.8 s of 440 Hz, 0.3 s silence; made with ffmpeg, no
//! user audio). Each must come out as a clip the hub's own check (`vox_proto::parse_wav`)
//! accepts: 48 kHz, trimmed to about the tone, at about -16 LUFS, with the pitch intact.

use vox_client_core::soundboard::{prepare, ProcessError};
use vox_proto::{parse_wav, wav_samples};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

fn check(name: &str, source_rate: u32, source_channels: usize, channels: u16) {
    let p = prepare(&fixture(name), Some(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!((p.source_rate, p.source_channels, p.channels), (source_rate, source_channels, channels), "{name}");
    let info = parse_wav(&p.wav).unwrap_or_else(|e| panic!("{name}: the hub would refuse it: {e}"));
    assert_eq!(info.channels, channels);
    // 0.8 s of tone plus the 10 ms pads; encoder delay and padding must not leave silence behind.
    assert!((780..=880).contains(&p.duration_ms), "{name}: {} ms (trimmed {} + {})", p.duration_ms, p.trimmed_start_ms, p.trimmed_end_ms);
    assert!(p.trimmed_start_ms >= 250 && p.trimmed_end_ms >= 250, "{name}: silence not trimmed");
    let s = wav_samples(&p.wav, &info);
    let ch = channels as usize;
    let left: Vec<f32> = s.iter().step_by(ch).map(|&x| x as f32 / 32768.0).collect();
    let mid = &left[2_000..left.len() - 2_000];
    let rms = (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt();
    let db = 20.0 * rms.log10();
    assert!((-18.0..-14.0).contains(&db), "{name}: {db} dBFS RMS (a steady tone is about its LUFS)");
    let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    let hz = crossings as f32 / 2.0 / (mid.len() as f32 / 48_000.0);
    assert!((hz - 440.0).abs() < 10.0, "{name}: {hz} Hz");
    // Not clipped, and quiet at both ends.
    assert!(p.peak < 1.0);
    assert!(s[0].abs() < 100 && s[s.len() - 1].abs() < 100, "{name}: edges {} {}", s[0], s[s.len() - 1]);
}

#[test]
fn mp3_stereo_44k() {
    // The two channels are the same sine: stored as mono.
    check("tone-44k-stereo.mp3", 44_100, 2, 1);
}

#[test]
fn ogg_vorbis_mono_22k() {
    check("tone-22k-mono.ogg", 22_050, 1, 1);
}

#[test]
fn flac_stereo_48k() {
    check("tone-48k-stereo.flac", 48_000, 2, 1);
}

#[test]
fn the_extension_is_only_a_hint() {
    // A FLAC named .mp3 and an OGG with no name at all are still recognised by their content.
    assert!(prepare(&fixture("tone-48k-stereo.flac"), Some("wrong.mp3")).is_ok());
    assert!(prepare(&fixture("tone-22k-mono.ogg"), None).is_ok());
}

#[test]
fn a_seven_second_ogg_is_too_long_and_says_so() {
    match prepare(&fixture("long-7s.ogg"), Some("long-7s.ogg")) {
        Err(ProcessError::TooLong { secs }) => assert!((6.9..7.1).contains(&secs), "{secs}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn truncated_files_still_give_what_could_be_decoded_or_a_clean_error() {
    let mp3 = fixture("tone-44k-stereo.mp3");
    // Cut in the middle of the tone: the first part is usable.
    let cut = prepare(&mp3[..mp3.len() * 2 / 3], Some("x.mp3"));
    match cut {
        Ok(p) => assert!(parse_wav(&p.wav).is_ok()),
        Err(ProcessError::Unsupported(_)) | Err(ProcessError::Silent) => {}
        Err(e) => panic!("{e:?}"),
    }
    // Garbage in the middle of a valid header never panics.
    let mut bad = fixture("tone-22k-mono.ogg");
    for b in bad.iter_mut().skip(200).step_by(7) {
        *b ^= 0x5a;
    }
    let _ = prepare(&bad, Some("x.ogg"));
}

#[test]
fn every_format_can_be_read_for_the_editor_and_cut_by_selection() {
    use std::sync::atomic::AtomicBool;
    use vox_client_core::soundboard::{analyze, prepare_file, Selection};
    for name in ["tone-44k-stereo.mp3", "tone-22k-mono.ogg", "tone-48k-stereo.flac"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        // 0.3 s of silence, 0.8 s of tone, 0.3 s of silence (a codec adds a few ms of padding).
        let o = analyze(&path, &|_| {}, &AtomicBool::new(false)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!((1.35..1.5).contains(&o.duration()), "{name}: {} s", o.duration());
        let peak = o.peak();
        assert!((0.005..1.0).contains(&peak), "{name}: peak {peak}");
        // The tone is in the middle: its columns have signal, the first 0.2 s has none.
        let cols = o.columns(0.0, o.duration(), 14);
        assert!(cols[0].1 < peak * 0.2 && cols[7].1 > peak * 0.5, "{name}: {cols:?}");
        // 0.5 s taken from inside the tone.
        let sel = Selection::new(0.5, 1.0, o.duration());
        let p = prepare_file(&path, Some(&sel.to_trim(true, true, true))).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!((495..=510).contains(&p.duration_ms), "{name}: {} ms", p.duration_ms);
        assert!(parse_wav(&p.wav).is_ok());
        // Only the leading silence: nothing to make a sound of.
        let quiet = Selection::new(0.0, 0.2, o.duration());
        assert_eq!(prepare_file(&path, Some(&quiet.to_trim(true, true, true))).unwrap_err(), ProcessError::Silent, "{name}");
    }
}

// ---------------------------------------------------------------- Opus, WebM, M4A / AAC / MP4
//
// These fixtures are made at test time with ffmpeg (a 1.4 s file: 0.3 s of silence, 0.8 s of tone,
// 0.3 s of silence) into a temp dir, so no encoded audio is committed. Without ffmpeg on PATH (or
// without the encoder a case needs) the case says so and passes, unless `VOX_REQUIRE_FFMPEG` is
// set, which makes a missing tool a failure (for a machine that is supposed to have it).

mod ffmpeg_fixtures {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;

    use vox_client_core::soundboard::{analyze, prepare, prepare_file, ProcessError, Selection};
    use vox_proto::{parse_wav, wav_samples};

    /// 440 Hz in the left channel, 660 Hz in the right.
    const STEREO_SRC: &str = "aevalsrc='if(between(t,0.3,1.1),0.3*sin(2*PI*440*t),0)|if(between(t,0.3,1.1),0.3*sin(2*PI*660*t),0)':s=48000:d=1.4";
    const MONO_SRC: &str = "aevalsrc='if(between(t,0.3,1.1),0.3*sin(2*PI*440*t),0)':s=48000:d=1.4";

    /// Makes `name` in `dir` with ffmpeg: the lavfi source `src`, then `out_args`. `None` (after
    /// saying why) when the tool or its encoder is missing.
    fn make(dir: &Path, name: &str, src: &str, out_args: &[&str]) -> Option<PathBuf> {
        let path = dir.join(name);
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "lavfi", "-i", src])
            .args(out_args)
            .arg(&path)
            .stdin(Stdio::null())
            .output();
        match out {
            Err(_) if std::env::var_os("VOX_REQUIRE_FFMPEG").is_none() => {
                eprintln!("SKIPPED {name}: ffmpeg is not on PATH");
                None
            }
            Err(e) => panic!("{name}: ffmpeg is required (VOX_REQUIRE_FFMPEG): {e}"),
            Ok(o) if !o.status.success() => {
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                let missing = err.contains("Unknown encoder") || err.contains("Encoder not found") || err.contains("not found");
                if missing && std::env::var_os("VOX_REQUIRE_FFMPEG").is_none() {
                    eprintln!("SKIPPED {name}: this ffmpeg lacks an encoder ({})", err.lines().next().unwrap_or(""));
                    None
                } else {
                    panic!("{name}: ffmpeg failed: {err}");
                }
            }
            Ok(_) => Some(path),
        }
    }

    fn crossings_hz(left: &[f32]) -> f32 {
        let n = left.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        n as f32 / 2.0 / (left.len() as f32 / 48_000.0)
    }

    /// What the whole pipeline (the "prepare from bytes" path) makes of the file.
    fn check_prepare(path: &Path, source_rate: u32, source_channels: usize, out_channels: u16) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(path).unwrap();
        let p = prepare(&bytes, Some(&name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((p.source_rate, p.source_channels, p.channels), (source_rate, source_channels, out_channels), "{name}");
        let info = parse_wav(&p.wav).unwrap_or_else(|e| panic!("{name}: the hub would refuse it: {e}"));
        // 0.8 s of tone plus the 10 ms pads (a codec's priming may add a few ms of ramp).
        assert!((780..=880).contains(&p.duration_ms), "{name}: {} ms (trimmed {} + {})", p.duration_ms, p.trimmed_start_ms, p.trimmed_end_ms);
        assert!(p.trimmed_start_ms >= 250 && p.trimmed_end_ms >= 250, "{name}: silence not trimmed");
        let s = wav_samples(&p.wav, &info);
        let ch = out_channels as usize;
        let left: Vec<f32> = s.iter().step_by(ch).map(|&x| x as f32 / 32768.0).collect();
        let mid = &left[2_000..left.len() - 2_000];
        let rms = (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt();
        let db = 20.0 * rms.log10();
        // LUFS adds the channels' powers: a stereo clip at -16 LUFS has each channel about 3 dB lower.
        let range = if ch == 2 { -21.0..-16.5 } else { -18.0..-14.0 };
        assert!(range.contains(&db), "{name}: {db} dBFS RMS");
        let hz = crossings_hz(mid);
        assert!((hz - 440.0).abs() < 10.0, "{name}: {hz} Hz");
        assert!(p.peak < 1.0);
        assert!(s[0].abs() < 100 && s[s.len() - 1].abs() < 100, "{name}: edges {} {}", s[0], s[s.len() - 1]);
        if ch == 2 {
            // The right channel carries the other tone (660 Hz): the channels were not mixed up.
            let right: Vec<f32> = s.iter().skip(1).step_by(2).map(|&x| x as f32 / 32768.0).collect();
            let hz = crossings_hz(&right[2_000..right.len() - 2_000]);
            assert!((hz - 660.0).abs() < 12.0, "{name}: right channel {hz} Hz");
        }
    }

    /// The editor's two passes on the file: the overview (whole file) and the selection re-decode.
    /// `tolerance` is how far the decoded length may be from 1.4 s, in seconds.
    fn check_editor(path: &Path, tolerance: f64) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let o = analyze(path, &|_| {}, &AtomicBool::new(false)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!((o.duration() - 1.4).abs() <= tolerance, "{name}: {} s, expected 1.4 +- {tolerance}", o.duration());
        let peak = o.peak();
        assert!((0.1..0.5).contains(&peak), "{name}: peak {peak} (the tone is 0.3)");
        let cols = o.columns(0.0, o.duration(), 14);
        assert!(cols[0].1 < peak * 0.2 && cols[7].1 > peak * 0.5, "{name}: {cols:?}");
        let sel = Selection::new(0.5, 1.0, o.duration());
        let p = prepare_file(path, Some(&sel.to_trim(true, true, true))).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!((495..=510).contains(&p.duration_ms), "{name}: {} ms", p.duration_ms);
        assert!(parse_wav(&p.wav).is_ok());
        // The part of the tone the selection covers is at the right pitch (the start is exact).
        let info = parse_wav(&p.wav).unwrap();
        let s = wav_samples(&p.wav, &info);
        let ch = info.channels as usize;
        let left: Vec<f32> = s.iter().step_by(ch).map(|&x| x as f32 / 32768.0).collect();
        let hz = crossings_hz(&left[1_000..left.len() - 1_000]);
        assert!((hz - 440.0).abs() < 15.0, "{name}: selection at {hz} Hz");
        // Only the leading silence: nothing to make a sound of.
        let quiet = Selection::new(0.0, 0.2, o.duration());
        assert_eq!(prepare_file(path, Some(&quiet.to_trim(true, true, true))).unwrap_err(), ProcessError::Silent, "{name}");
        // The whole file through the file path too (no selection).
        let whole = prepare_file(path, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!((780..=880).contains(&whole.duration_ms), "{name}: {} ms", whole.duration_ms);
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn ogg_opus_stereo() {
        let d = tmp();
        let Some(f) = make(d.path(), "tone-stereo.ogg", STEREO_SRC, &["-c:a", "libopus", "-b:a", "96k"]) else { return };
        // Opus is always decoded at 48 kHz, whatever the file came from; pre-skip is dropped, so
        // the length is exact.
        check_prepare(&f, 48_000, 2, 2);
        check_editor(&f, 0.005);
    }

    #[test]
    fn ogg_opus_mono() {
        let d = tmp();
        let Some(f) = make(d.path(), "tone-mono.ogg", MONO_SRC, &["-c:a", "libopus", "-b:a", "48k"]) else { return };
        check_prepare(&f, 48_000, 1, 1);
        check_editor(&f, 0.005);
    }

    #[test]
    fn opus_extension_and_other_input_rates() {
        let d = tmp();
        // A 16 kHz source: the Opus file still decodes at 48 kHz, with the same length.
        let Some(f) = make(d.path(), "voice.opus", MONO_SRC, &["-ar", "16000", "-c:a", "libopus", "-b:a", "32k"]) else { return };
        check_prepare(&f, 48_000, 1, 1);
        check_editor(&f, 0.005);
        // The name is only a hint: the same bytes called .mp3 or with no name decode all the same.
        let bytes = std::fs::read(&f).unwrap();
        assert!(prepare(&bytes, Some("whatsapp.mp3")).is_ok());
        assert!(prepare(&bytes, None).is_ok());
    }

    #[test]
    fn webm_opus_and_mka() {
        let d = tmp();
        let Some(f) = make(d.path(), "yt.webm", STEREO_SRC, &["-c:a", "libopus", "-b:a", "96k"]) else { return };
        check_prepare(&f, 48_000, 2, 2);
        // Matroska has no sample-exact length for the decoder: up to one 20 ms packet of padding.
        check_editor(&f, 0.025);
        let Some(f) = make(d.path(), "audio.mka", MONO_SRC, &["-c:a", "libopus", "-b:a", "48k"]) else { return };
        check_prepare(&f, 48_000, 1, 1);
        check_editor(&f, 0.025);
    }

    #[test]
    fn opus_surround_is_folded_to_mono_or_stereo() {
        let d = tmp();
        // 5.1 in a multistream Opus (mapping family 1): the tone is in the front-left channel only.
        let src = "aevalsrc='if(between(t,0.3,1.1),0.3*sin(2*PI*440*t),0)|0|0|0|0|0':s=48000:d=1.4:c=5.1";
        let Some(f) = make(d.path(), "surround.opus", src, &["-c:a", "libopus", "-b:a", "192k", "-mapping_family", "1"]) else { return };
        let p = prepare(&std::fs::read(&f).unwrap(), Some("surround.opus")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((p.source_rate, p.source_channels, p.channels), (48_000, 6, 1));
        assert!((780..=880).contains(&p.duration_ms), "{} ms", p.duration_ms);
        let info = parse_wav(&p.wav).unwrap();
        let s = wav_samples(&p.wav, &info);
        let left: Vec<f32> = s.iter().map(|&x| x as f32 / 32768.0).collect();
        let hz = crossings_hz(&left[2_000..left.len() - 2_000]);
        assert!((hz - 440.0).abs() < 10.0, "{hz} Hz");
        check_editor(&f, 0.005);
    }

    #[test]
    fn the_opus_output_gain_is_applied() {
        let d = tmp();
        let Some(f) = make(d.path(), "gain.ogg", MONO_SRC, &["-c:a", "libopus", "-b:a", "64k"]) else { return };
        let plain = analyze(&f, &|_| {}, &AtomicBool::new(false)).unwrap().peak();
        // +6.02 dB in the header (Q7.8), which changes the page checksum too.
        let mut bytes = std::fs::read(&f).unwrap();
        let at = bytes.windows(8).position(|w| w == b"OpusHead").unwrap();
        bytes[at + 16..at + 18].copy_from_slice(&1541i16.to_le_bytes());
        let page_start = bytes.windows(4).position(|w| w == b"OggS").unwrap();
        let segs = bytes[page_start + 26] as usize;
        let page_len = 27 + segs + bytes[page_start + 27..page_start + 27 + segs].iter().map(|&b| b as usize).sum::<usize>();
        let page = &mut bytes[page_start..page_start + page_len];
        page[22..26].fill(0);
        let crc = ogg_crc(page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        let louder = d.path().join("gain-up.ogg");
        std::fs::write(&louder, &bytes).unwrap();
        let boosted = analyze(&louder, &|_| {}, &AtomicBool::new(false)).unwrap().peak();
        assert!((boosted / plain - 2.0).abs() < 0.1, "peak {plain} -> {boosted}");
    }

    /// Ogg's CRC-32: polynomial 0x04c11db7, no reflection, initial value 0.
    fn ogg_crc(page: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &b in page {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
            }
        }
        crc
    }

    #[test]
    fn m4a_aac_with_the_index_at_the_end_or_the_start() {
        let d = tmp();
        // ffmpeg's default puts the index (moov) after the audio, like a phone recording.
        let Some(f) = make(d.path(), "memo.m4a", STEREO_SRC, &["-c:a", "aac", "-b:a", "128k"]) else { return };
        check_prepare(&f, 48_000, 2, 2);
        // AAC's encoder delay (about 21 ms) may show as extra leading samples.
        check_editor(&f, 0.06);
        let Some(f) = make(d.path(), "fast.m4a", MONO_SRC, &["-c:a", "aac", "-b:a", "64k", "-ar", "44100", "-movflags", "+faststart"]) else { return };
        check_prepare(&f, 44_100, 1, 1);
        check_editor(&f, 0.06);
    }

    #[test]
    fn raw_aac_adts() {
        let d = tmp();
        let Some(f) = make(d.path(), "stream.aac", MONO_SRC, &["-c:a", "aac", "-b:a", "64k", "-f", "adts"]) else { return };
        check_prepare(&f, 48_000, 1, 1);
        check_editor(&f, 0.06);
    }

    #[test]
    fn m4a_alac_lossless() {
        let d = tmp();
        let Some(f) = make(d.path(), "lossless.m4a", STEREO_SRC, &["-c:a", "alac"]) else { return };
        check_prepare(&f, 48_000, 2, 2);
        check_editor(&f, 0.01);
    }

    #[test]
    fn mp4_with_a_video_track_uses_the_audio() {
        let d = tmp();
        let path = d.path().join("clip.mp4");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:r=10:d=1.4", "-f", "lavfi", "-i", STEREO_SRC])
            .args(["-c:v", "mpeg4", "-c:a", "aac", "-b:a", "128k", "-shortest"])
            .arg(&path)
            .stdin(Stdio::null())
            .output();
        match status {
            Ok(o) if o.status.success() => {}
            Ok(o) if !o.status.success() && std::env::var_os("VOX_REQUIRE_FFMPEG").is_none() => {
                eprintln!("SKIPPED mp4: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or(""));
                return;
            }
            Ok(o) => panic!("ffmpeg failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(_) if std::env::var_os("VOX_REQUIRE_FFMPEG").is_none() => {
                eprintln!("SKIPPED mp4: ffmpeg is not on PATH");
                return;
            }
            Err(e) => panic!("ffmpeg is required: {e}"),
        }
        check_prepare(&path, 48_000, 2, 2);
        check_editor(&path, 0.08);
    }

    #[test]
    fn damaged_new_formats_never_panic_and_say_why() {
        let d = tmp();
        for (name, src, args) in [
            ("a.ogg", MONO_SRC, vec!["-c:a", "libopus"]),
            ("b.webm", MONO_SRC, vec!["-c:a", "libopus"]),
            ("c.m4a", MONO_SRC, vec!["-c:a", "aac"]),
        ] {
            let Some(f) = make(d.path(), name, src, &args) else { return };
            let good = std::fs::read(&f).unwrap();
            // Cut in the middle: something usable, or a clean error.
            for cut in [good.len() / 3, good.len() * 2 / 3, good.len() - 3] {
                match prepare(&good[..cut], Some(name)) {
                    Ok(p) => assert!(parse_wav(&p.wav).is_ok(), "{name}"),
                    Err(ProcessError::Unsupported(_)) | Err(ProcessError::Silent) => {}
                    Err(e) => panic!("{name} cut at {cut}: {e:?}"),
                }
            }
            // Flipped bytes in the middle (the headers stay intact).
            let mut bad = good.clone();
            for b in bad.iter_mut().skip(good.len() / 2).step_by(5) {
                *b ^= 0x5a;
            }
            let _ = prepare(&bad, Some(name));
            // Not audio at all, with the new extensions.
            assert!(matches!(prepare(b"just some text, definitely not audio", Some(name)), Err(ProcessError::Unsupported(_))), "{name}");
        }
        // The message lists the formats the dialog accepts.
        let msg = ProcessError::Unsupported("x".into()).to_string();
        assert!(msg.contains("WAV, MP3, OGG, OPUS, M4A, WEBM ou FLAC"), "{msg}");
    }
}
