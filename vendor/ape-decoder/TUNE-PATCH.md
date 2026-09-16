# Tune patch for #4191

Base: published `ape-decoder` **0.3.2**, MIT OR Apache-2.0.
The crate archive SHA-256 is
`95097ef8efebf7b1e1d638b77935cfce25576163614427b464efe256bb8b2ec1`.

The published constructor accepts version 3950 onward, but all residuals
were decoded with the entropy syntax introduced in 3990. A 3970 stream
therefore used the wrong arithmetic probability model and residual layout.

Local source changes are confined to:

- `range_coder.rs`: pre-3990 quotient cumulative frequencies and interval update.
- `entropy.rs`: version dispatch and legacy adaptive Rice remainder/escape.
- `decoder.rs`: pass the existing header version at all 11 residual call sites.

The existing >=3990 decoder remains the modern branch. Predictors, NN filters,
frame CRC, sample overflow checks, seeking and public decoder APIs are unchanged.
The cumulative frequencies are bitstream format constants; the Rust extension
is implemented here. Neither the historical C++ SDK nor FFmpeg is linked or
shipped as part of this dependency.

Reproduction: `integration_contracts::ape_legacy_4191` exercises the actual
Tune decode and streaming entry points. Synthetic 3970 fixtures were encoded
with the historical 3.97 SDK; their original PCM is the oracle. See
`tune-core/tests/fixtures/ape/legacy3970/README.md` from the repository root.
An independent FFmpeg decode during fixture preparation matched all input PCM.
CI needs neither reference tool nor network access to execute these tests.

The musical public sample `https://samples.ffmpeg.org/monkeyaudio/sh3.ape`
(3970/2000, SHA-256
`9b8e89b81a87001648d58dc9ef440a5b9b8c214a4df07bd22776da1ff6e32004`)
reproduced the original `overflow range_total out of bounds` on frame 0.
All three frames now decode byte-identically to the independent reference,
PCM SHA-256
`57cb3908bb26e91f372c1f0e8f10e68a699f90107d486546ceef9ff695fc2a7b`.
This external musical file is not included in the repository.
The reporter's complete CD image has not been available for validation.

The archive's own `Cargo.lock` is omitted; the workspace lock owns dependency
resolution. All other upstream files retain their original contents except
the three source files named above. Remove this fork when an upstream release
provides the legacy entropy support and passes the retained regression tests.
