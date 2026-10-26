# livekit 0.9.4, vendored with one change

This is the `livekit` crate 0.9.4 from crates.io (Apache-2.0), reduced to `src/`, `build.rs` and
`Cargo.toml` (examples and tests dropped, so their `[[test]]` / `[[example]]` tables are removed),
plus `#![allow(warnings)]` on top of `src/lib.rs`.

The only functional change: `TrackPublishOptions::stereo` (default `false`).

* `src/room/options.rs`: the new field.
* `src/room/participant/local_participant.rs`: for an audio track published with `stereo: true`
  the `AddTrackRequest` gets `stereo = true` and the `TF_STEREO` audio feature.

Why: the SDK never asks the server for stereo. Without it the server answers the publisher's offer
without `stereo=1` and libwebrtc encodes mono (a two-channel source is downmixed), so the
"Go Live" audio (a screen share's sound, or an audio-only share) went out as mono. With the flag,
the sender's send codec says `stereo=1` and a left/right-different source arrives with its stereo
image (checked by the `audio-a` / `audio-b` autotests).

Remove this directory and the `[patch.crates-io]` section of the root `Cargo.toml` when the
upstream crate has an equivalent option.
