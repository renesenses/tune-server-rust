# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.1] - 2026-03-18

### Fixed
- `cargo fmt` formatting violations that caused CI failure

## [0.3.0] - 2026-03-18

### Added
- Stress-test integration suite: 6 complex audio signals at all 5 compression levels
- Per-frame CRC validation tests with corruption detection
- Fuzz testing setup with 3 targets (full decode, frame decode, parser)
- Real-world verification script (`scripts/verify_real_world.sh`)
- APE-to-WAV decode example (`examples/decode_to_file.rs`)
- Testing & performance section in README

### Fixed
- NN filter `adapt_16`/`adapt_32` arithmetic overflow on complex audio (use wrapping ops to match C++ semantics)
- Predictor arithmetic overflow in prediction computation and coefficient adaptation
- Mid-side decorrelation overflow in `unprepare` module
- OOM from malformed headers: seek table, WAV header, and frame data allocation caps
- BitReader out-of-bounds panic on truncated/malformed frame data
- Metadata computation overflow (`total_blocks * block_align`, etc.) using saturating arithmetic
- `file_bytes - terminating_data_bytes` underflow on crafted files

## [0.2.0] - 2026-03-18

### Added
- WAV header generation for decoded APE audio
- APE tag writing and removal
- MD5 file integrity verification
- Parallel multi-threaded decoding
- Range decoding (decode a subset of samples)
- ID3v2 tag parsing (v2.3 and v2.4)
- Progress callbacks with cancellation
- Runnable decode example (`examples/decode.rs`)
- CI: cross-platform matrix (Linux, macOS, Windows), MSRV verification, doc checks
- Dependabot for Cargo and GitHub Actions dependencies
- Automated crates.io publish on tag push
- CHANGELOG.md, README badges, docs.rs metadata

### Changed
- Internal codec modules are now `pub(crate)` (not part of public API)
- `ApeError` is now `#[non_exhaustive]`

### Fixed
- Hardcoded test path that broke CI on GitHub Actions

## [0.1.1] - 2026-03-18

### Added
- Precise sample-level seeking
- File metadata access (sample rate, channels, bit depth, duration)

### Changed
- Improved release script error handling and output

## [0.1.0] - 2026-03-17

### Added
- Initial release
- APE frame decoding for all compression levels (Fast, Normal, High, Extra High, Insane)
- All bit depths (8, 16, 24, 32-bit) and channel layouts (mono, stereo)
- Streaming frame-by-frame decode with iterator
- CI workflow
- Automated release script

[Unreleased]: https://github.com/OMBS-IO/ape-decoder/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/OMBS-IO/ape-decoder/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/OMBS-IO/ape-decoder/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/OMBS-IO/ape-decoder/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/OMBS-IO/ape-decoder/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/OMBS-IO/ape-decoder/releases/tag/v0.1.0
