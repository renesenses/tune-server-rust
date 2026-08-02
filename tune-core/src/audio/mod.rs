pub mod aiff;
pub mod analyzer;
pub mod ape;
pub mod channels;
pub mod convolver;
pub mod dash_growth;
pub mod decode;
pub mod dff;
pub mod dsd_to_dop;
pub mod dsd_to_pcm;
pub mod dsf;
#[cfg(feature = "audio-embedding")]
pub mod embedding;
/// READ side of audio embeddings (storage + cosine) — always compiled, no ort.
pub mod embedding_store;
pub mod encoder;
pub mod eq;
pub mod faststart;
pub mod formats;
pub mod iso_sacd;
pub mod levels;
pub mod mixer;
pub mod pipeline;
pub mod replaygain;
/// Runtime provisioning of the onnxruntime shared lib (`load-dynamic`).
#[cfg(feature = "audio-embedding")]
pub mod runtime;
/// CLAP text tower for natural-language acoustic search (Phase 3).
#[cfg(feature = "audio-embedding")]
pub mod text_embedding;
pub mod tap;
pub mod wav;
pub mod wavpack;
