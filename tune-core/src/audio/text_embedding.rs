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
use tokio::sync::Mutex;
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

        let ids_t = Tensor::from_array(([1usize, CONTEXT], ids))
            .map_err(|e| format!("ort input_ids: {e}"))?;
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

/// Session chargée à la demande et relâchée après une période d'inactivité.
///
/// Le modèle texte CLAP pèse ~500 Mo résidents. Il vivait dans un `OnceCell` :
/// chargé à la première recherche par texte, gardé ensuite pour toute la vie du
/// processus — relevé sur le .18 le 08/10, un bloc de 500 Mo jamais rendu,
/// alors qu'une recherche « ambiance » est un geste ponctuel. Désormais la
/// session se relâche après [`TEXT_IDLE`] sans recherche, et la recherche
/// suivante la recharge (quelques secondes, une fois).
pub(crate) struct ALaDemande<T> {
    valeur: Option<T>,
    dernier_usage: Option<std::time::Instant>,
}

impl<T> ALaDemande<T> {
    pub(crate) const fn vide() -> Self {
        Self {
            valeur: None,
            dernier_usage: None,
        }
    }

    /// La valeur, chargée par `charger` si elle ne l'est pas. Le booléen dit
    /// si elle vient d'être chargée. Un échec de chargement n'est pas retenu :
    /// la demande suivante réessaie.
    pub(crate) async fn obtenir<F, Fut>(
        &mut self,
        charger: F,
        maintenant: std::time::Instant,
    ) -> Result<(&mut T, bool), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        let chargee = self.valeur.is_none();
        if chargee {
            self.valeur = Some(charger().await?);
        }
        self.dernier_usage = Some(maintenant);
        match self.valeur.as_mut() {
            Some(v) => Ok((v, chargee)),
            None => Err("session texte absente".into()),
        }
    }

    /// Relâcher la valeur si elle n'a pas servi depuis `delai`. `true` quand
    /// une valeur vivante vient d'être relâchée.
    pub(crate) fn relacher_si_inactive(
        &mut self,
        maintenant: std::time::Instant,
        delai: std::time::Duration,
    ) -> bool {
        let inactive = self
            .dernier_usage
            .is_none_or(|t| maintenant.saturating_duration_since(t) >= delai);
        if self.valeur.is_some() && inactive {
            self.valeur = None;
            return true;
        }
        false
    }

    pub(crate) fn est_chargee(&self) -> bool {
        self.valeur.is_some()
    }
}

/// Inactivité au-delà de laquelle le modèle texte est relâché.
const TEXT_IDLE: std::time::Duration = std::time::Duration::from_secs(600);
/// Cadence de la veille qui relâche le modèle texte inactif.
const TEXT_VEILLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Process-global text embedder, loaded on demand and released after
/// [`TEXT_IDLE`] without a search.
static TEXT_EMBEDDER: Mutex<ALaDemande<TextEmbedder>> = Mutex::const_new(ALaDemande::vide());

/// Veille lancée à chaque chargement : relâche le modèle texte après
/// [`TEXT_IDLE`] sans recherche, rend la mémoire, et s'arrête. Une seule à la
/// fois : elle ne naît qu'au chargement, qui n'a lieu que session absente, et
/// elle s'éteint dès que la session l'est.
fn veiller_sur_le_modele_texte(backend: Arc<dyn DbBackend>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TEXT_VEILLE).await;
            let mut slot = TEXT_EMBEDDER.lock().await;
            if !slot.est_chargee() {
                return;
            }
            if slot.relacher_si_inactive(std::time::Instant::now(), TEXT_IDLE) {
                drop(slot);
                info!(
                    idle_s = TEXT_IDLE.as_secs(),
                    "text_embedder_released — modèle texte relâché après inactivité"
                );
                super::embedding::rendre_la_memoire_si_rien_ne_joue(&backend).await;
                return;
            }
        }
    });
}

/// Resolve the text model + tokenizer cache paths. Both live next to the
/// configured audio model (sharing the onnxruntime dylib) when the audio sweep
/// has provisioned one; otherwise they fall back to a default `embedding_models`
/// directory so acoustic **search** self-provisions on first use even if the
/// audio-embedding sweep was never enabled (#1288/Fabien: "Menu Ambiance → 503",
/// `audio_embedding_model_path unset`). Mirrors the relative `artwork_cache`
/// convention — resolved against the server's working directory.
fn text_paths(settings: &SettingsRepo) -> (PathBuf, PathBuf) {
    let dir = settings
        .get("audio_embedding_model_path")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_AUDIO_EMBED_MODEL").ok())
        .and_then(|audio| Path::new(&audio).parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("embedding_models"));
    let model = settings
        .get(TEXT_MODEL_PATH_KEY)
        .ok()
        .flatten()
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("clap-text-music-2023.onnx"));
    let tokenizer = dir.join("clap-music-tokenizer.json");
    (model, tokenizer)
}

/// Embed a natural-language query into the CLAP joint space for acoustic search.
///
/// Lazily provisions the runtime + text model + tokenizer on first call and
/// caches a single loaded session — released after [`TEXT_IDLE`] without a
/// search, reloaded by the next one — serialised by a mutex: ORT sessions are not
/// concurrent-run friendly and a query embed is sub-100 ms, so serialising query
/// requests is fine. Returns an `Err` string the handler maps to 503 when the
/// model cannot be provisioned (offline, unconfigured, checksum failure).
pub async fn embed_query(backend: &Arc<dyn DbBackend>, query: &str) -> Result<Vec<f32>, String> {
    let mut slot = TEXT_EMBEDDER.lock().await;
    let (embedder, chargee) = slot
        .obtenir(
            || async {
                let settings = SettingsRepo::with_backend(backend.clone());
                let (model, tokenizer) = text_paths(&settings);
                super::embedding::ensure_file(
                    &model,
                    TEXT_MODEL_URL,
                    TEXT_MODEL_SHA256,
                    "text_model",
                )
                .await?;
                super::embedding::ensure_file(
                    &tokenizer,
                    TOKENIZER_URL,
                    TOKENIZER_SHA256,
                    "text_tokenizer",
                )
                .await?;
                let dir = model
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                super::runtime::ensure_loaded(&dir).await?;
                // Hors de l'exécuteur : bâtir une session de 500 Mo est du
                // travail bloquant, et il revient désormais après chaque
                // relâchement pour inactivité.
                let m = model.clone();
                let embedder =
                    tokio::task::spawn_blocking(move || TextEmbedder::load(&m, &tokenizer))
                        .await
                        .map_err(|e| format!("text embedder load task: {e}"))??;
                info!(model = %model.display(), "text_embedder_loaded");
                Ok(embedder)
            },
            std::time::Instant::now(),
        )
        .await?;
    let vecteur = embedder.embed_text(query);
    drop(slot);
    if chargee {
        veiller_sur_le_modele_texte(backend.clone());
    }
    vecteur
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Le modèle texte se charge à la première recherche, pas aux suivantes,
    /// se relâche après l'inactivité — pas avant — et la recherche d'après le
    /// recharge : la recherche par texte ne casse pas.
    #[tokio::test]
    async fn le_modele_texte_se_relache_apres_inactivite_et_se_recharge_a_la_demande() {
        let chargements = AtomicUsize::new(0);
        let charger = || async {
            chargements.fetch_add(1, Ordering::SeqCst);
            Ok::<u32, String>(7)
        };
        let mut slot = ALaDemande::vide();
        let t0 = Instant::now();

        let (v, chargee) = slot.obtenir(charger, t0).await.unwrap();
        assert_eq!((*v, chargee), (7, true));
        let (_, chargee) = slot
            .obtenir(charger, t0 + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            !chargee,
            "deuxième recherche : la session sert, pas de rechargement"
        );
        assert_eq!(chargements.load(Ordering::SeqCst), 1);

        assert!(
            !slot.relacher_si_inactive(t0 + Duration::from_secs(60), TEXT_IDLE),
            "une minute après la dernière recherche : trop tôt pour relâcher"
        );
        assert!(slot.est_chargee());
        assert!(
            slot.relacher_si_inactive(t0 + Duration::from_secs(5) + TEXT_IDLE, TEXT_IDLE),
            "après TEXT_IDLE sans recherche, les ~500 Mo doivent être rendus"
        );
        assert!(!slot.est_chargee());
        assert!(
            !slot.relacher_si_inactive(t0 + TEXT_IDLE * 3, TEXT_IDLE),
            "rien à relâcher deux fois"
        );

        let (v, chargee) = slot.obtenir(charger, t0 + TEXT_IDLE * 4).await.unwrap();
        assert_eq!((*v, chargee), (7, true), "la recherche suivante recharge");
        assert_eq!(chargements.load(Ordering::SeqCst), 2);
    }

    /// Un échec de chargement n'est pas retenu : la recherche suivante réessaie
    /// (c'était déjà le cas avec `OnceCell::get_or_try_init`).
    #[tokio::test]
    async fn un_echec_de_chargement_n_est_pas_retenu() {
        let mut slot: ALaDemande<u32> = ALaDemande::vide();
        let t0 = Instant::now();
        assert!(
            slot.obtenir(|| async { Err("hors ligne".to_string()) }, t0)
                .await
                .is_err()
        );
        assert!(!slot.est_chargee());
        let (v, chargee) = slot.obtenir(|| async { Ok(3) }, t0).await.unwrap();
        assert_eq!((*v, chargee), (3, true));
    }
}
