//! Audio plumbing shared by voice and Go Live: device I/O, playback mixer, helpers.

pub mod alerts;
pub mod denoise;
pub mod dfn;
#[cfg(test)]
mod denoise_eval;
pub mod device;
pub mod gate;
pub mod mic_test;
pub mod mixer;
pub mod prefs;
pub mod resample;
pub mod sine;
pub mod ui_player;
pub mod voicefx;
#[cfg(test)]
mod voicefx_eval;

pub use mixer::{Gain, Mixer, SourceCtl, SourceId, MAX_GAIN, MIX_10MS, MIX_CHANNELS, MIX_RATE};
pub use alerts::AlertKind;
pub use prefs::{AudioPrefs, UserAudio};
pub use denoise::{NoiseControl, NoiseLevel};
pub use gate::{GateConfig, GateMode, VoiceGate};
pub use mic_test::{MicTest, MicTestConfig};
pub use ui_player::UiPlayer;
pub use voicefx::{VoiceEffect, VoiceFxConfig, VoiceFxControl};
