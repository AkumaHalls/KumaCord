# deep_filter (vendored subset of libDF)

Source: <https://github.com/Rikorose/DeepFilterNet>, `libDF/src/{lib.rs,tract.rs}` and
`models/DeepFilterNet3_onnx.tar.gz` at commit `d375b2d8309e0935d165700c91da9de862a99c31`.
License: MIT or Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`), same as upstream.

Only the streaming inference path is kept (the `tract` feature with the embedded DeepFilterNet3
model); the dataset, training, C API, WASM and resampling code are gone, so none of their
dependencies (hdf5, rubato, ...) are pulled in. The crate on crates.io (0.2.5) predates the tract
backend, hence the vendoring.

Changes from upstream:

- `lib.rs`: dropped the module declarations of the removed files; lints allowed crate-wide.
- `tract.rs`: `ndarray` comes through `tract_core::ndarray` (re-exported as `df::ndarray`), the
  embedded model path points at `models/` of this crate, `DEFAULT_MODEL` exposes the embedded model
  bytes (so a load error is a `Result` instead of a panic), and a new `DfTract::reset()` (network
  states, STFT memory, rolling spectra and normalisation back to their initial values) serves
  voxhub's mute flush.
- `Cargo.toml`: only the inference dependencies; tract pinned to `=0.21.4`, the version upstream
  locks. tract 0.21.10 builds (after small API changes) but gives wrong output with these pulsed
  models (clean speech comes out with a negative SI-SDR), so do not bump it without re-running
  `denoise_eval`.

Used by `vox-client-core::audio::dfn` (the "IA máxima" noise suppression level).
