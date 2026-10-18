# Research: hardware encode/decode and AEC with the LiveKit Rust SDK (Windows)

Written by the client-core agent for Phase 3. Every statement below comes from the crate sources
in `~/.cargo/registry` for the versions the client actually builds with:
`livekit 0.9.4`, `libwebrtc 0.3.51`, `webrtc-sys 0.3.48` (prebuilt libwebrtc
`win-x64-release-webrtc-89d790b`). File references are relative to those crates.

## TL;DR

| Question | Answer on Windows |
|---|---|
| (a) HW H.264 encode via the SDK? | **No.** NVENC/VAAPI/Jetson factories are compiled **only on Linux** (`webrtc-sys/build.rs`). Windows gets OpenH264 (software). HW decode: **no** either (FFmpeg software H.264). |
| (b) Pass a D3D11 texture as a frame? | **No.** `NativeBuffer` exists only for CVPixelBuffer (macOS/iOS) and DMA-BUF (Linux). On Windows frames must be CPU buffers (I420/NV12/...). |
| (c) Publish pre-encoded H.264? | **Yes.** `NativeVideoSource::new_encoded` + `capture_encoded_frame` + `VideoEncoderBackend::PreEncoded` (a pass-through encoder inside webrtc-sys). Keyframe and bitrate requests come back to us. |
| (d) Does `NativeAudioSource`'s APM get a far-end reference? | **No, and it doesn't run at all.** Frames pushed into `NativeAudioSource` bypass WebRTC's APM. The client runs its own SDK APM (`webrtc::native::apm`) and feeds it the playback mix as the reverse stream. That's implemented now. |
| (e) Recommended Phase 3 path | WGC texture -> **our own HW encoder** (NVENC on D3D11 input, or the Media Foundation HW H.264 MFT for AMD/Intel) -> **pre-encoded publish**. No CPU readback, no CPU color conversion, no OpenH264. |

Measured in Phase 1 (1280x720@30 test pattern, release build, 16-thread CPU): the publisher used
1.4-1.8% of the machine (23-29% of one core) and the viewer 0.6-0.8% (10-13% of one core). The SDK reports
`encoder_implementation = "OpenH264"` on the publisher and `decoder_implementation = "FFmpeg"`
on the viewer, which confirms (a). At runtime, `VideoEncoderBackend::list_available()` returns only
`[Auto, Software, PreEncoded]` on this machine (logged by vox-client-core when it joins a room).

---

## (a) Hardware H.264 encoding and decoding

**Encoder factory** (`webrtc-sys/src/video_encoder_factory.cpp`):

- The software set is a `VideoEncoderFactoryTemplate` of libvpx VP8/VP9, libaom AV1 and,
  under `WEBRTC_USE_H264`, **OpenH264**. The Windows libwebrtc build script
  (`webrtc-sys/libwebrtc/build_windows.cmd`) sets `rtc_use_h264=true ffmpeg_branding="Chrome"`,
  so H.264 means OpenH264 (encode) + FFmpeg (decode), both on the CPU.
- The hardware backends are behind compile-time defines:
  `USE_NVIDIA_VIDEO_CODEC` (NVENC via CUDA), `USE_VAAPI_VIDEO_CODEC` and `USE_JETSON_VIDEO_CODEC`.
  `VideoToolbox` is used on Apple and the Android HW factory on Android.
- `webrtc-sys/build.rs` adds the NVIDIA sources (`src/nvidia/*`, `NvEncoderCuda`) and the define
  **only inside the `"linux"` match arm**, and only when `cuda.h` is found under `$CUDA_HOME`. In
  the `"windows"` arm the VAAPI-on-Windows lines are commented out and there is no NVIDIA branch.
  So on Windows:
  - `VideoEncoderBackend::{Nvenc, Vaapi, Hardware}` exist in the Rust enum, but they are never
    listed or usable. Requesting them logs "support is not compiled in" and falls back.
  - `LIVEKIT_PREFERRED_HW_ENCODER=nvenc` (env var read by the factory) has no effect.
  - No feature flag changes this. `webrtc-sys` has `default = []` and no features, and `livekit`
    features only select the TLS stack.
- Even on Linux, the NVIDIA path is `NvEncoderCuda` fed from CPU I420/NV12 buffers uploaded to
  CUDA (`nvidia/i420_buffer_cuda.h`). It is not a zero-copy D3D/GL texture path.

**Decoder factory** (`webrtc-sys/src/video_decoder_factory.cpp`): Apple (VideoToolbox), Android,
and `USE_NVIDIA_VIDEO_CODEC` (Linux only, again). On Windows H.264 decode is
`webrtc::H264Decoder::Create()`, i.e. FFmpeg software. The Rust API has no way to plug in our
own decoder or to receive the encoded frames of a remote track (no receiver frame transformer is
exposed for video, apart from the packet-trailer internals).

**How to get HW codecs inside the SDK anyway:** fork `webrtc-sys` and add a Windows factory
(NVENC via `NV_ENC_DEVICE_TYPE_DIRECTX`, or MF transforms) to `video_encoder_factory.cpp` and
`video_decoder_factory.cpp`. That is feasible (the factory already supports multiple backends), but
it means maintaining a C++ fork and its build. For encoding, (c) avoids it completely.

## (b) GPU handles as video frames

`libwebrtc/src/video_frame.rs`, `pub mod native`: `NativeBuffer` has exactly two constructors:
`from_cv_pixel_buffer` (`#[cfg(any(target_os = "macos", target_os = "ios"))]`) and `from_dmabuf`
(`#[cfg(target_os = "linux")]`, Jetson/V4L2). There's nothing for D3D11 / DXGI shared handles. On
Windows `NativeVideoSource::capture_frame` takes CPU buffers only: `I420Buffer`, `I420ABuffer`,
`I422Buffer`, `I444Buffer`, `I010Buffer`, `NV12Buffer`.

That's why vox-capture does GPU BGRA->NV12 plus one staging readback, and the client copies
NV12 straight into an `NV12Buffer` (two plane memcpys, no conversion). libwebrtc then converts
NV12->I420 internally, because OpenH264 wants I420. That is one more CPU pass per frame that
only goes away with (c).

## (c) Pre-encoded H.264 publishing

Supported in this SDK version. The pieces:

- `NativeVideoSource::new_encoded(resolution)`: a source that doesn't inject raw keep-alive
  frames.
- `NativeVideoSource::capture_encoded_frame(&EncodedVideoFrame { codec: EncodedVideoCodec::H264,
  payload, timestamp_us, frame_type: Key|Delta, resolution, frame_metadata })`: one access unit
  per call (Annex-B byte stream. The pass-through uses `common_video/h264/h264_common.h` to split
  NAL units and sets `packetization_mode = NonInterleaved`).
- Publish with `TrackPublishOptions { video_codec: VideoCodec::H264, video_encoder:
  VideoEncoderBackend::PreEncoded, simulcast: false, .. }`. `rtc_session.rs` calls
  `sender.set_video_encoder_backend(options.video_encoder)`, and the factory then instantiates
  `PassthroughVideoEncoder` (`webrtc-sys/src/passthrough_video_encoder.cpp`).
- Feedback to our encoder:
  - `take_keyframe_request() -> bool`: set when the RTP layer needs a keyframe (PLI/FIR, new
    subscriber). The pass-through can't make one itself and logs that it is waiting, so
    **we must force an IDR**.
  - `take_rate_control_request() -> Option<EncodedRateControl { target_bitrate_bps,
    framerate_fps }>`: WebRTC's congestion controller target (from `SetRates`). Apply it to the
    HW encoder (NVENC `nvEncReconfigureEncoder`, or MF `CODECAPI_AVEncCommonMeanBitRate`).
- Limits: pass-through formats are only advertised for codecs that some real encoder also
  supports (H.264 is fine on Windows thanks to OpenH264). It is a single layer, so there is no
  simulcast unless we encode several layers ourselves. That's fine for 15 people on one SFU.
  Viewers still decode with FFmpeg (software).

## (d) Mic echo cancellation with our own cpal playback

What the SDK does:

- `AudioSourceOptions { echo_cancellation, noise_suppression, auto_gain_control }` on a
  `NativeAudioSource` are just stored on the C++ source (`webrtc-sys/src/audio_track.cpp`,
  `InternalSource::set_options`).
- WebRTC applies the APM inside `AudioTransportImpl`, to audio captured by the **ADM**. The
  `external_audio_source.patch` that LiveKit applies to libwebrtc makes `NativeAudioSource` an
  "external source": frames go straight to `AudioSendStream` through `AddSink` and are **not**
  registered with `AudioState`. So pushed mic frames are never processed by WebRTC's APM.
  The options have no effect, and there is no far-end reference either.
- Under the default "synthetic" ADM, remote audio that we pull through `NativeAudioStream` and
  play through cpal is invisible to any APM.

Options:

1. **Own APM instance (implemented in Phase 1).** `livekit::webrtc::native::apm::
   AudioProcessingModule` is a thin wrapper over WebRTC's `BuiltinAudioProcessingBuilder`
   (AEC3, AGC, high-pass filter, NS). It is `Send + Sync`. vox-client-core:
   - mic callback: 10 ms mono 48 kHz -> `process_stream` -> `NativeAudioSource` (whose options
     are left off);
   - output callback: every 10 ms block that's actually played (post-mixer, pre-device) ->
     `process_reverse_stream`;
   - `set_stream_delay_ms(40)` as a hint (AEC3 estimates the delay itself).
   Remaining work: measure the real device latency (cpal callback timestamps) for a better delay
   hint, and validate with a real headset/speaker pair (not done in Phase 1: no audible tests).
2. **`PlatformAudio` (SDK 0.9.4).** It switches the SDK to WebRTC's real WASAPI ADM: WebRTC
   captures the mic and plays remote audio itself, and its APM gets both directions, so AEC works
   "for free". Costs: we lose our own mixer (per-user volume, deafen and screenshare audio routing
   go through track enable/disable), the device handling is WebRTC's, and every remote track
   (including screenshare audio) is played by the ADM. It's a valid fallback if option 1 gives
   poor AEC quality.
3. Windows' built-in communications processing (Voice Capture DSP / `AUDIO_EFFECT_TYPE_
   ACOUSTIC_ECHO_CANCELLATION` on a communications-category stream). The quality depends on the
   driver, and cpal doesn't expose it. Not recommended.

For headset users (most people working from home) AEC barely matters. NS + AGC matter more, and
option 1 already provides them.

## (e) Recommended Phase 3 path: 1080p60 sharing under ~5% CPU on an RTX 50-series GPU

Today's per-frame CPU work on the publisher: GPU readback of NV12 (vox-capture), a memcpy into
`NV12Buffer`, NV12->I420 in libwebrtc, then **OpenH264 software encode** (the dominant cost).
Measured: 1.4-1.8% of the machine (about 25% of one core) for 720p30 of synthetic content.
1080p60 is 4.5x the pixels per second, and real screen/game content is harder to encode than the
test pattern, so expect about 6-10% of the machine (1-1.5 cores). That's over budget.

Target pipeline (zero-copy, all on the GPU):

```
WGC frame (ID3D11Texture2D BGRA)
  -> [optional GPU downscale via ID3D11VideoProcessor, as vox-capture does today]
  -> NVENC (NV_ENC_DEVICE_TYPE_DIRECTX, nvEncRegisterResource on the D3D11 texture,
            ARGB input: NVENC does the color conversion itself)
     preset P3-P4, tuning ULTRA_LOW_LATENCY, CBR, VBV ~1 frame, no B-frames,
     infinite GOP + IDR on demand, repeat SPS/PPS on IDR
  -> Annex-B access unit (CPU memory, ~10-25 KB/frame)
  -> NativeVideoSource::capture_encoded_frame  (VideoEncoderBackend::PreEncoded)
     loop: take_keyframe_request() -> force IDR; take_rate_control_request() -> reconfigure bitrate
```

- Expected CPU: WGC + NVENC submit/lock are a few hundred µs per frame, and RTP packetization of
  about 6 Mbps is small. 1080p60 should land around 1-3% of the machine. The GPU does capture,
  scaling and encode (NVENC is a separate ASIC, so games aren't affected much).
- Vendor coverage for the other 14 people: put the encoder behind a small trait
  (`HwH264Encoder`) with two backends:
  - **NVENC SDK** (`nvEncodeAPI64.dll`, loaded at runtime, no link-time dependency): best
    latency and control on NVIDIA.
  - **Media Foundation hardware H.264 MFT** (`MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE`, D3D11
    input through `IMFDXGIDeviceManager`): one API that covers AMD AMF and Intel QSV (and
    NVIDIA as a fallback). Set `CODECAPI_AVLowLatencyMode`, CBR, and force a keyframe with
    `CODECAPI_AVEncVideoForceKeyFrame`.
  - Fallback when neither exists: the current raw path (SDK + OpenH264), which already works.
- Where the code lives: the encoder needs the WGC texture, so it belongs next to the capture
  code (vox-capture, or a new `vox-encode` crate that shares its D3D11 device). The contract
  would gain something like `start_video_encoded(target, cfg, on_access_unit, control)`, where
  `control` carries keyframe requests and bitrate targets back to the encoder. **This is a
  vox-capture contract change, so it goes through the orchestrator.**
- Viewers: decode stays FFmpeg software (measured 0.6-0.8% of the machine / 10-13% of one core
  for 720p30, including YUV->RGB and the softbuffer present. 1080p60 extrapolates to roughly
  3-4% of the machine). For 1080p60 viewing under 5%, the options are (1) fork
  webrtc-sys to add a D3D11VA/MF H.264 decoder factory and a GPU renderer (wgpu/D3D11 swapchain
  instead of softbuffer), or (2) accept software decode and move only the YUV->RGB conversion
  and scaling to the GPU. Do (2) first (it's in our crate), measure, and fork only if needed.
- Bitrate: keep about 6 Mbps for 1080p60 and 4 Mbps for 1080p30 (`screenshare_bitrate()` in
  vox-client-core), but let `take_rate_control_request()` lower it when the network asks.
