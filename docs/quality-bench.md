# Go Live quality benchmark (v0.2 item 5)

Host: uma RTX série 50 (driver recente), um processador de 8 núcleos. Tool: `capture_probe bench` (in
`crates/vox-capture/examples/common/`). It renders deterministic synthetic scenes on the CPU,
uploads them to a D3D11 texture and pushes them through the **real** path: video processor
(BGRA -> NV12) -> production `EncoderSink` (rate compensation, keyframe gate, degradation policy)
-> hardware encoder. The NV12 the encoder received is the reference; ffmpeg scores the decoded
H.264 against it (`psnr`, `ssim` per frame). 1920x1080, 4 s per run, frames fed one at a time.

Scenes (all hard on purpose):
- **pointcloud**: 450k coloured points of a rotating 3D cloud, like the field case (noise-like).
- **text**: pseudo-text page flung at ~1400 px/s with a sideways pan (code / web page scrolling).
- **game**: textured perspective floor, turning and moving camera, clouds, sprites, static HUD.

Columns: SSIM = mean, p5 = 5th-percentile frame, min = worst frame. "out" = real output kbps.
Latency = submit -> access unit. CPU = process CPU (all threads) for convert+encode, % of the
16-thread machine at the nominal rate.

Configs: `v01` = what v0.1 shipped (MF: CBR, 1-frame VBV; NVENC: P4 + ULTRA_LOW_LATENCY).
`v02` = new defaults (MF: + QualityVsSpeed 100 + start headroom; NVENC: P4 + LOW_LATENCY).

## Same bitrate, before vs after (SSIM mean / worst frame)

| scene @ rate | MF v01 (v0.1 Auto) | MF v02 | NVENC v01 | **NVENC v02 (v0.2 Auto)** |
|---|---|---|---|---|
| text 1080p30 @ 8 Mbps | 0.9845 / 0.892 | 0.9842 / 0.880 | 0.9890 / 0.875 (5.8 Mbps out) | **0.9899 / 0.914** |
| game 1080p30 @ 8 Mbps | 0.8925 / 0.876 | 0.9006 / 0.886 | 0.8896 / 0.876 | **0.8921 / 0.876** |
| pointcloud 1080p30 @ 8 Mbps | 0.7212 / 0.609 | 0.7266 / 0.616 | 0.7331 / 0.636 (8.5 out) | **0.7211 / 0.608** |
| text 1080p60 @ 12 Mbps | 0.9948 / 0.976 | 0.9922 / 0.901 | 0.9912 / 0.855 (8.0 out) | **0.9948 / 0.976** |
| game 1080p60 @ 12 Mbps | 0.8888 / 0.875 | 0.8962 / 0.882 | 0.8875 / 0.858 | **0.8900 / 0.875** |
| latency (avg) | 1.9-2.1 ms | 3.0-3.6 ms | 2.4-2.8 ms | **2.0-2.3 ms** |
| CPU 1080p60 | 0.15-0.19 % | 0.15-0.21 % | 0.14-0.19 % | **0.12-0.16 %** |

At a constant frame rate the encoders are close (the NVIDIA MFT is NVENC underneath); the
v0.1 NVENC tuning (ULL) under-spent the target on text (5.8 of 8 Mbps) and had bad first frames.
Tuning knobs swept at 1080p30 @ 6 Mbps and 1080p60 @ 10 Mbps (`phase1`): P5/P6 (+1 ms, no
gain), spatial/temporal AQ (+-0.003, mixed), VBR with max = target (identical to CBR), 2-frame VBV
(spends less, worse), multipass off/full (mixed), max QP 40 (fixes worst frames but blows the
bitrate to 2.5x on noise-like content: rejected). MF: the NVIDIA MFT ignores the VBR modes;
QualityVsSpeed 100 helps text at 30 fps (SSIM 0.977 -> 0.985) and costs ~1 ms.

## Window capture: frames only on change (the field problem)

Encoder configured for 30 fps, source delivering **12 fps** (like the field log), target 4 Mbps:

| | out kbps | text SSIM | game SSIM |
|---|---|---|---|
| MF v01 (v0.1) | 2000 / 1690 | 0.882 | 0.827 |
| NVENC v01 | 1545 / 1524 | 0.920 | 0.820 |
| MF v02 | 3079 / 3891 | 0.925 | 0.892 |
| **NVENC v02** | 3052 / 3544 | **0.962** | **0.884** |

Root cause: hardware encoders budget `bitrate / configured_fps` per frame, so at 12 fps the
v0.1 stream used ~40% of its target (the field log's 261-1679 kbps). v0.2 measures the real input
rate and scales the encoder bitrate (`RateAdapter`, at most 3x, fast attack / slow release).
Bonus finding: the NVIDIA MFT never raises its bitrate above the start value, so v0.1 could not
even recover after congestion on MF; v0.2 starts the MFT at 3x and lowers it once streaming.

## Bitrate presets (diminishing returns, v02 Auto)

| | 4/6 Mbps | 8/12 Mbps | 12/16 Mbps |
|---|---|---|---|
| 1080p30 text / game | 0.973 / 0.842 | **0.990 / 0.892** | 0.993 / 0.914 |
| 1080p60 text / game | 0.989 / 0.849 | **0.995 / 0.890** | 0.996 / 0.907 |

New presets: 720p30 **4 Mbps**, 1080p30 **8 Mbps** (was 4), 1080p60 **12 Mbps** (was 6), and
"Máxima" 1080p60 **16 Mbps** (+0.017 SSIM on game; only for a good upload). Before -> after
with the default preset: 1080p30 text 0.969 -> 0.990, game 0.845 -> 0.892; 1080p60 game 0.849
-> 0.890. Noise-like content (pointcloud) stays hard at any rate (0.70 -> 0.73): there the
frame-rate cap is the real lever.

Host upload per viewer = the preset rate (the SFU forwards one copy per viewer): 5 viewers of
1080p60 need ~60 Mbps of host upload, 1080p30 ~40 Mbps (logged at publish: "host upload ~N
Mbps per viewer"). The publisher's congestion controller only sees the streamer -> SFU link
(that is what drives the fps-first policy); the SFU -> viewer links are not adapted (single
layer, no simulcast), so a host upload below rate x viewers shows up as loss / freezes on viewers.

## Congestion: fps-first degradation

1080p60 @ 12 Mbps, target cut to 4 Mbps from 3 s to 6 s, restored after (10 s, MF v02):

| | game SSIM / p5 | text SSIM / p5 | fps |
|---|---|---|---|
| Priorizar fluidez (keep 60) | 0.855 / 0.775 | 0.992 / 0.981 | 60 |
| **Priorizar nitidez** (default) | **0.884 / 0.834** | 0.993 / 0.980 | 60 > 30 > 60 |

Policy (`vox-capture/src/win/encode/policy.rs`): below 0.05 bits/pixel/frame at the current
rate, step down 60 -> 30 -> 20 -> 15 at once; step back up one level only with >= 0.075 bpp at
the higher rate held for 3 s.

## Keyframes

Requested IDR every 30 frames at 1080p30 @ 8 Mbps: an IDR is 2-4x a P frame with the 1-frame
VBV (max ~70 ms of link time at the target rate), SSIM dip ~0.02 on text. `lowDelayKeyFrameScale
3` raises text SSIM 0.979 -> 0.982 but makes IDRs 4.8x (116 ms spikes): not adopted. Requests
closer than 250 ms are now folded into one IDR (several viewers joining, PLI bursts).

## Deferred

- Intra refresh (WebRTC receivers need a real IDR to start decoding anyway), VMAF scoring,
  exhaustive matrices (720p, 1440p, AMD/Intel MFTs: the MF headroom trick and QualityVsSpeed are
  only verified on the NVIDIA MFT).
- NVENC overshoots the target at very low bits/pixel on noise-like content (QP floor); the fps
  cap keeps the policy out of that region.
- NVENC as Auto: private bytes +10 MB over the first 4 start/stop cycles then flat (~76 MB),
  ~4 handles per start (driver).
- Viewer: decoded/dropped/freeze/PLI counts and "replaced undrawn" frames are logged per stats
  interval; no UI for them yet.

## Pre-encoded frames dropped by WebRTC (v0.2.2 field bug: smearing without packet loss)

Symptom: viewers see smeared blocks that persist until the next keyframe, while their stats show
`packets_lost=0 plis=0`. Cause: libwebrtc's `VideoStreamEncoder` treats our pre-encoded access
units like raw frames. The pass-through encoder does not set `has_trusted_rate_controller`, so
the frame dropper (a leaky bucket at the congestion-control target that starts dropping once it
holds 0.5 s of the target) stays on; the encoder-queue overload check, a paused encoder (target
0) and the duplicate-timestamp check can also drop. A dropped P frame is never packetized: RTP
sequence numbers stay contiguous (no loss, no NACK, no PLI) and FFmpeg conceals the reference
mismatch, so viewers never ask for a keyframe. None of this can be switched off from Rust (no
field-trial or `EncoderInfo` access without forking webrtc-sys).

Detection: every pushed frame carries a `frame_id`; the sender's publish-timing events
(`PublishTimingStage::EncoderOutput`, observe-only transformer, packet trailers stay off) report
which ids WebRTC actually encoded. A hole = dropped frames.

Measured (dev LiveKit on loopback, synthetic high-motion window, 10 s stats):

| run | handed / encoded by WebRTC | drops | broken frames sent | viewer |
|---|---|---|---|---|
| h264 file 6 Mbps into a 2 Mbps track | 300 / 100 | 200 | - | 100 received, lost 0 |
| MF, 720p window, 60 fps config, 30 fps source, 3 Mbps, v0.2.1 | 300 / 294-297 | 3-6 | 279-306 (every frame, 40 s) | 30 fps, lost 0, PLI 0, freezes 0 |
| same, fixed (2 runs) | 300 / 300 | 0 | 0 | 30 fps |
| NVENC + test overshoot 3x, 2 Mbps, v0.2.1 | 300 / 112 | 187 | (all) | 11 fps, lost 0 |
| same, fixed | 300 / 300 | 0 | 0 | 30 fps, 0 freezes |

MF with the input-rate compensation (2x here) ran 3-6% over the target, which is enough: the
dropper's bucket fills linearly with any sustained overshoot. NVENC stayed under the target in
these runs.

Fix (`golive/publish.rs`): (1) **budget guard**: mirrors the dropper's bucket with our frame
sizes and WebRTC's target and scales the encoder target down while it fills (integral control,
floor 10%, recovers 2%/frame); (2) **recovery**: any detected hole forces an IDR (250 ms
coalescing in vox-capture) and cuts the guard by 30%. Even an unrealistic 10x overshoot is then
bounded to ~2.5 broken frames per drop instead of a frozen picture. `VOX_DROP_RECOVERY=0` turns
both off (for A/B). Test hook: vox-cli `overshoot <percent>`.

New stats: `encoder feedback ... webrtc_encoded webrtc_dropped drop_events recovery_idrs
broken_frames guard_min_pct`; `stream stats` (send) `tx=src encoded sent keyframes huge plis_in`
(WebRTC's own counters: `src - encoded` = WebRTC drops); (recv) `received key_decoded nacks`.
