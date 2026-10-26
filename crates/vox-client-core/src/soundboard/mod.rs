//! Server soundboard, client side (the contract is in `vox_proto::sound`):
//!
//! - [`process`]: whatever audio file the user picks -> the 48 kHz WAV the hub stores.
//! - [`trim`]: the editor's model (waveform overview, the 5 s selection) for picking a part of a long file.
//! - [`cache`]: downloaded clips, on disk by `{id}-{hash}` (a clip never changes for a hash) and a
//!   few decoded ones in memory, ready for the playback mixer.

pub mod cache;
pub mod opus;
pub mod process;
pub mod trim;
mod webm;

pub use cache::{SoundCache, SoundPcm};
pub use process::{apply_fades, prepare, prepare_file, Prepared, ProcessError, Trim, DEFAULT_FADE_MS, MAX_EDIT_SECS, MAX_SOURCE_BYTES, SUPPORTED_FORMATS};
pub use trim::{analyze, format_time, Overview, OverviewBuilder, Selection, MAX_SELECTION, MIN_SELECTION};
