//! CLAP text tower for natural-language acoustic search (Phase 3).
//!
//! The text tower shares the joint 512-d space with the music audio tower, so a
//! free-text query ("warm analog jazz", "driving late-night techno") embeds into
//! the very space the library's audio embeddings live in — a cosine ranking then
//! returns acoustically matching tracks, regardless of their tags. onnxruntime is
//! loaded dynamically at runtime (`load-dynamic`) and shared with the audio sweep
//! via [`super::runtime::ensure_loaded`]; this side is feature-gated behind
//! `audio-embedding`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;
use tokio::sync::{Mutex, OnceCell};
use tracing::info;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

use super::embedding_store::EMBED_DIM;

/// RoBERTa context length the text tower was exported with (fixed `[B, 77]`).
const CONTEXT: usize = 77;

/// Published CLAP music text tower (ONNX, 512-d), same release as the audio tower.
const TEXT_MODEL_URL: &str = "https://github.com/renesenses/tune-server-rust/releases/download/models/clap-music-2023/clap-text-music-2023.onnx";
const TEXT_MODEL_SHA256: &str = "df933a849ffaccb3692306b1dea8cf9247d0b6abf29613cb719a24e41974e81e";
/// RoBERTa tokenizer JSON for the text tower (loaded by the `tokenizers` crate).
/// Its padding/truncation are NOT baked in — pinned to 77 at load time below.
const TOKENIZER_URL: &str = "https://github.com/renesenses/tune-server-rust/releases/download/models/clap-music-2023/clap-music-tokenizer.json";
const TOKENIZER_SHA256: &str = "847bbeab6174d66a88898f729d52fa8d355fafe1bea101cf960dd404581df70e";

/// Optional setting overriding where the text model is cached; by default it
/// sits next to the audio model so both share one onnxruntime dylib.
const TEXT_MODEL_PATH_KEY: &str = "audio_text_model_path";

/// A loaded CLAP text embedder: ONNX session + its RoBERTa tokenizer.
pub struct TextEmbedder {
    session: Session,
    tokenizer: Tokenizer,
}

impl TextEmbedder {
    /// Load the text tower + tokenizer from disk. The onnxruntime shared lib must
    /// already be loaded globally (`super::runtime::ensure_loaded`).
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self, String> {
        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| format!("load tokenizer {}: {e}", tokenizer_path.display()))?;
        // The published JSON carries no padding/truncation config, but the tower
        // takes a fixed [B, 77] window — pad short queries to 77 and truncate long
        // ones, matching the reference (`padding='max_length', max_length=77`).
        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::Fixed(CONTEXT),
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: CONTEXT,
                ..Default::default()
            }))
            .map_err(|e| format!("tokenizer truncation: {e}"))?;

        let session = Session::builder()
            .map_err(|e| format!("ort builder: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| format!("ort load {}: {e}", model_path.display()))?;
        Ok(Self { session, tokenizer })
    }

    /// Embed a natural-language query into a normalised 512-d vector in the CLAP
    /// joint space (comparable to the stored audio embeddings by cosine).
    pub fn embed_text(&mut self, query: &str) -> Result<Vec<f32>, String> {
        let enc = self
            .tokenizer
            .encode(query, true)
            .map_err(|e| format!("tokenize: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
        if ids.len() != CONTEXT || mask.len() != CONTEXT {
            return Err(format!(
                "tokenizer produced {} ids / {} mask (want {CONTEXT})",
                ids.len(),
                mask.len()
            ));
        }

        let ids_t =
            Tensor::from_array(([1usize, CONTEXT], ids)).map_err(|e| format!("ort input_ids: {e}"))?;
        let mask_t = Tensor::from_array(([1usize, CONTEXT], mask))
            .map_err(|e| format!("ort attention_mask: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t])
            .map_err(|e| format!("ort run: {e}"))?;
        let (_shape, data) = outputs["text_embedding"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("ort extract: {e}"))?;

        let mut v: Vec<f32> = data.to_vec();
        if v.len() != EMBED_DIM {
            return Err(format!(
                "unexpected embedding dim {} (want {EMBED_DIM})",
                v.len()
            ));
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in &mut v {
            *x /= norm;
        }
        Ok(v)
    }
}

/// Process-global text embedder, provisioned + loaded once on first search.
static TEXT_EMBEDDER: OnceCell<Mutex<TextEmbedder>> = OnceCell::const_new();

/// Resolve the text model + tokenizer cache paths. Both default next to the
/// configured audio model (so they share the onnxruntime dylib). Returns `None`
/// when acoustic embedding was never configured (no audio model path) — the
/// handler maps that to "acoustic search unavailable".
fn text_paths(settings: &SettingsRepo) -> Option<(PathBuf, PathBuf)> {
    let audio = settings
        .get("audio_embedding_model_path")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_AUDIO_EMBED_MODEL").ok())?;
    let dir = Path::new(&audio).parent()?.to_path_buf();
    let model = settings
        .get(TEXT_MODEL_PATH_KEY)
        .ok()
        .flatten()
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("clap-text-music-2023.onnx"));
    let tokenizer = dir.join("clap-music-tokenizer.json");
    Some((model, tokenizer))
}

/// Embed a natural-language query into the CLAP joint space for acoustic search.
///
/// Lazily provisions the runtime + text model + tokenizer on first call and
/// caches a single loaded session, serialised by a mutex: ORT sessions are not
/// concurrent-run friendly and a query embed is sub-100 ms, so serialising query
/// requests is fine. Returns an `Err` string the handler maps to 503 when the
/// model cannot be provisioned (offline, unconfigured, checksum failure).
pub async fn embed_query(backend: &Arc<dyn DbBackend>, query: &str) -> Result<Vec<f32>, String> {
    let cell = TEXT_EMBEDDER
        .get_or_try_init(|| async {
            let settings = SettingsRepo::with_backend(backend.clone());
            let (model, tokenizer) = text_paths(&settings).ok_or_else(|| {
                "acoustic search not configured (audio_embedding_model_path unset)".to_string()
            })?;
            super::embedding::ensure_file(&model, TEXT_MODEL_URL, TEXT_MODEL_SHA256, "text_model")
                .await?;
            super::embedding::ensure_file(
                &tokenizer,
                TOKENIZER_URL,
                TOKENIZER_SHA256,
                "text_tokenizer",
            )
            .await?;
            let dir = model.parent().unwrap_or_else(|| Path::new("."));
            super::runtime::ensure_loaded(dir).await?;
            let embedder = TextEmbedder::load(&model, &tokenizer)?;
            info!(model = %model.display(), "text_embedder_loaded");
            Ok::<Mutex<TextEmbedder>, String>(Mutex::new(embedder))
        })
        .await?;
    let mut guard = cell.lock().await;
    guard.embed_text(query)
}
