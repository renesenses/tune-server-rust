//! DJ mode as a native [`TunePlugin`] (#917).
//!
//! Extracted verbatim from `tune-server`'s always-on core (`routes/dj.rs`) so
//! the stock server no longer carries it: build `tune-server --features dj` to
//! get these routes back, mounted by the plugin host at
//! `/api/v1/ext/dj/…` (the host derives the prefix from `name()` — a plugin
//! never chooses its own).
//!
//! DJ is **native**, not WASM: `waveform`/`analyze` need full audio access and
//! call [`tune_core::audio::decode::decode_to_pcm`] directly.
//!
//! Host dependencies are passed explicitly at construction via [`HostServices`]
//! — matching the wiring pattern documented in `tune-server/src/plugins.rs`, so
//! a plugin's real dependencies are visible at the registration site. DJ only
//! needs the DB backend (settings + track lookups); its router captures that
//! backend in its own state rather than sharing the host's `AppState`, which
//! keeps `tune-core` free of any `tune-server` type.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::audio::decode::decode_to_pcm;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Host services handed to the DJ plugin at construction.
///
/// Passed explicitly (not pulled from [`PluginContext`]) so the plugin's real
/// dependencies are visible where it is wired up in
/// `register_builtin_plugins`. DJ needs only the DB backend.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
}

/// The DJ plugin. Owns the DB backend its router needs.
pub struct DjPlugin {
    backend: Arc<dyn DbBackend>,
}

impl DjPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
        }
    }
}

#[async_trait]
impl TunePlugin for DjPlugin {
    fn name(&self) -> &str {
        "dj"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "DJ mode: crossfade, decks, waveform and BPM analysis"
    }
    // Opt-in: a niche mode that stays dormant until the user installs it from
    // the plugin manager, rather than running for everyone by default.
    fn default_enabled(&self) -> bool {
        false
    }

    // Hors catalogue (#2090). Le gestionnaire ne doit pas proposer d'installer
    // DJ, parce que DJ ne fait pas ce que sa description annonce.
    //
    // « crossfade, decks » : le greffon ne reçoit QUE la base
    // (`HostServices { backend }`) — ni `PlaybackManager`, ni registre de
    // sorties. Il n'a aucun accès au chemin audio, donc aucun moyen de faire
    // jouer, de fondre ou de charger une platine, quel que soit le contenu des
    // handlers. Et de fait, sur les 13 routes déclarées plus bas, 11 ne
    // changent rien :
    //
    //   * 7 renvoient l'argument reçu sans rien écrire — `play`, `pause`,
    //     `crossfade`, `crossfader`, `auto-crossfade`, `load`, `volume` ;
    //   * `sync-tempo` répond littéralement « tempo sync not yet implemented » ;
    //   * `enable`, `disable` et `status` n'écrivent et ne relisent que
    //     `dj_enabled_{zone}`, un réglage dont ces trois handlers sont les
    //     SEULS lecteurs du dépôt. `status` renvoie par-dessus des platines
    //     toujours `loaded: false` et un `crossfader: 0.5` en dur — il contredit
    //     donc `load` et `crossfader` juste après leur succès annoncé.
    //
    // Restent 2 routes qui travaillent vraiment : `waveform` et `analyze`
    // (décodage PCM natif).
    // Elles restent servies : le greffon est toujours compilé, toujours testé
    // (`tests/dj_plugin.rs`), et se charge encore si l'on pose
    // `plugin_dj_installed=true` à la main. Ce qui cesse, c'est la promesse.
    //
    // À rebasculer à `true` le jour où les platines existent pour de bon.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// DJ reacts to no events today — auto-crossfade is a client-driven poke at
    /// `/auto-crossfade`, not a server-side hook. Left as a no-op override so the
    /// plugin does not receive every event on the bus for nothing.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

/// Plugin-owned router state. Captures the host's DB backend so the router can
/// be a `Router<()>` (as the host requires) without leaking `AppState`.
#[derive(Clone)]
struct DjState {
    backend: Arc<dyn DbBackend>,
}

/// The DJ router, `Router<()>` for the plugin host to mount under
/// `/api/v1/ext/dj`. Routes are identical to the old `routes/dj.rs`.
pub fn router(backend: Arc<dyn DbBackend>) -> Router<()> {
    Router::new()
        .route("/enable/{zone_id}", post(enable_dj))
        .route("/disable/{zone_id}", post(disable_dj))
        .route("/status/{zone_id}", get(dj_status))
        .route("/play", post(dj_play))
        .route("/pause", post(dj_pause))
        .route("/crossfade", post(dj_crossfade))
        .route("/crossfader", post(dj_crossfader))
        .route("/auto-crossfade", post(dj_auto_crossfade))
        .route("/load/{zone_id}/{deck}", post(dj_load))
        .route("/volume/{zone_id}/{deck}", post(dj_volume))
        .route("/sync-tempo/{zone_id}", post(dj_sync_tempo))
        .route("/waveform/{track_id}", get(dj_waveform))
        .route("/analyze/{track_id}", post(dj_analyze))
        .with_state(DjState { backend })
}

async fn enable_dj(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set(&format!("dj_enabled_{zone_id}"), "true").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": true}))
}

async fn disable_dj(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set(&format!("dj_enabled_{zone_id}"), "false").ok();
    Json(json!({"zone_id": zone_id, "dj_mode": false}))
}

async fn dj_status(State(state): State<DjState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get(&format!("dj_enabled_{zone_id}"))
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({
        "zone_id": zone_id,
        "dj_mode": enabled,
        "deck_a": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "deck_b": {"loaded": false, "track": null, "position_ms": 0, "bpm": null},
        "crossfader": 0.5,
        "auto_crossfade": false,
    }))
}

#[derive(Deserialize)]
struct DjPlayRequest {
    zone_id: i64,
}

async fn dj_play(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": true}))
}

async fn dj_pause(Json(body): Json<DjPlayRequest>) -> Json<Value> {
    Json(json!({"zone_id": body.zone_id, "playing": false}))
}

#[derive(Deserialize)]
struct CrossfadeRequest {
    zone_id: i64,
    duration_ms: Option<i64>,
}

async fn dj_crossfade(Json(body): Json<CrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfade_started": true,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct CrossfaderRequest {
    zone_id: i64,
    position: f64,
}

async fn dj_crossfader(Json(body): Json<CrossfaderRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "crossfader": body.position.clamp(0.0, 1.0),
    }))
}

#[derive(Deserialize)]
struct AutoCrossfadeRequest {
    zone_id: i64,
    enabled: bool,
    duration_ms: Option<i64>,
}

async fn dj_auto_crossfade(Json(body): Json<AutoCrossfadeRequest>) -> Json<Value> {
    Json(json!({
        "zone_id": body.zone_id,
        "auto_crossfade": body.enabled,
        "duration_ms": body.duration_ms.unwrap_or(5000),
    }))
}

#[derive(Deserialize)]
struct LoadDeckRequest {
    track_id: i64,
}

async fn dj_load(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<LoadDeckRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "track_id": body.track_id,
        "loaded": true,
    }))
}

#[derive(Deserialize)]
struct DeckVolumeRequest {
    volume: f64,
}

async fn dj_volume(
    Path((zone_id, deck)): Path<(i64, String)>,
    Json(body): Json<DeckVolumeRequest>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "deck": deck,
        "volume": body.volume.clamp(0.0, 1.0),
    }))
}

async fn dj_sync_tempo(Path(zone_id): Path<i64>) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "synced": true,
        "message": "tempo sync not yet implemented",
    }))
}

async fn dj_waveform(State(state): State<DjState>, Path(track_id): Path<i64>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively, then downsample to ~8kHz equivalent by striding
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples_i32.is_empty() => {
            let waveform = onde(&audio.samples_i32, audio.sample_rate, audio.bit_depth);

            Json(json!({
                "track_id": track_id,
                "points": waveform.len(),
                "waveform": waveform,
            }))
            .into_response()
        }
        _ => Json(json!({
            "track_id": track_id,
            "waveform": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}

async fn dj_analyze(State(state): State<DjState>, Path(track_id): Path<i64>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = repo.get(track_id).ok().flatten();
    let Some(track) = track else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "track not found"})),
        )
            .into_response();
    };
    let Some(ref path) = track.file_path else {
        return Json(json!({"track_id": track_id, "error": "no file path"})).into_response();
    };

    // Decode to mono PCM natively for energy-based beat detection
    let path_owned = path.clone();
    let decoded =
        tokio::task::spawn_blocking(move || decode_to_pcm(&path_owned, None, Some(1), 0.0, 0.0))
            .await;

    match decoded {
        Ok(Ok(audio)) if !audio.samples_i32.is_empty() => {
            match analyser(&audio.samples_i32, audio.sample_rate, audio.bit_depth) {
                Ok(a) => Json(json!({
                    "track_id": track_id,
                    "bpm": a.bpm,
                    "duration_s": a.duree_s.round(),
                    "beats_detected": a.beats,
                }))
                .into_response(),
                Err(motif) => Json(json!({
                    "track_id": track_id,
                    "bpm": null,
                    "error": motif,
                }))
                .into_response(),
            }
        }
        _ => Json(json!({
            "track_id": track_id,
            "bpm": null,
            "error": "native decode failed",
        }))
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Le calcul — la seule chose que ce greffon fasse vraiment
// ---------------------------------------------------------------------------
//
// Sur les treize routes déclarées plus haut, onze ne changent rien (voir la
// note de `catalogued`). Restent `waveform` et `analyze`, et tout leur calcul
// vivait EN LIGNE dans les deux gestionnaires — donc derrière une base, un
// fichier sur disque et le décodeur. Les seuls essais qui l'atteignaient
// (`tune-server/tests/dj_plugin.rs`) n'en gardaient que la sortie grossière :
// « `points` > 0 » et « `beats_detected` est un nombre », sur un unique fichier
// 16 bits / 44,1 kHz. Une onde 256 fois trop grande sur un fichier 24 bits, ou
// un tempo de 14 BPM annoncé sans broncher, passaient au vert.
//
// Le calcul est repris MOT POUR MOT ; seul le point d'entrée change. Les
// gestionnaires `dj_waveform` et `dj_analyze` ci-dessus sont les SEULS
// appelants, et les essais du bas de ce fichier appellent ces mêmes fonctions.

/// Nombre de points visé par [`onde`].
///
/// Publique pour qu'un essai lise la VRAIE borne plutôt que d'en réécrire une :
/// un essai qui coderait `200` en dur resterait vert le jour où le greffon
/// changerait de résolution.
pub const POINTS: usize = 200;

/// Fréquence visée par le décimage de l'onde, en Hz.
const ONDE_HZ: usize = 8_000;

/// Fréquence visée par le décimage d'analyse, en Hz.
const ANALYSE_HZ: usize = 22_050;

/// Bornes du tempo que le greffon accepte de publier. Hors de là, `bpm` est
/// `null` : mieux vaut ne rien dire que d'annoncer 14 BPM sur un nocturne.
pub const BPM_MIN: f64 = 60.0;
/// Voir [`BPM_MIN`].
///
/// ⚠️ **Mesuré, jamais atteint par le haut** (#3640). Un « front » est une
/// fenêtre d'énergie de 250 ms qui passe au-dessus du seuil alors que la
/// précédente était en dessous : il faut donc au moins une fenêtre creuse entre
/// deux fronts, soit un plafond structurel de l'ordre de 120 BPM. Au-delà,
/// l'estimateur ne rend pas un tempo trop grand, il n'en rend plus du tout
/// (zéro front mesuré à 240 BPM). Cette borne haute ne filtre donc rien
/// aujourd'hui — c'est un constat, pas un correctif : voir
/// `contre_epreuve_un_morceau_a_deux_cent_quarante_bpm_ne_recoit_aucun_tempo`.
pub const BPM_MAX: f64 = 200.0;

/// Motif rendu quand le morceau ne porte pas quatre fenêtres d'énergie de
/// 250 ms. C'est une valeur de contrat — elle part au client dans le champ
/// `error` — donc elle se nomme une fois et ne se réécrit pas.
pub const TROP_COURT: &str = "audio too short for analysis";

/// Le diviseur qui ramène un échantillon entier dans [-1,0 ; 1,0].
///
/// `decode_to_pcm` rend des entiers **cadrés à droite** : un 24 bits tient dans
/// ±2^23 — `DecodedAudio::pcm_bytes` n'en réémet que les trois octets bas — et
/// un 32 bits dans ±2^31. Se tromper d'échelle ne fait rougir aucune
/// compilation et ne se voit que chez l'auditeur : sur un fichier haute
/// résolution, l'onde sort 256 fois trop petite (une ligne plate) ou 256 fois
/// trop grande (un bloc plein).
fn echelle(bit_depth: u16) -> f32 {
    match bit_depth {
        24 => (1i64 << 23) as f32,
        32 => (1i64 << 31) as f32,
        _ => 32768.0,
    }
}

/// L'onde rendue par `GET /waveform/{track_id}` : au plus [`POINTS`] valeurs,
/// chacune l'amplitude **crête** d'une tranche, dans [0,0 ; 1,0].
pub fn onde(samples_i32: &[i32], sample_rate: u32, bit_depth: u16) -> Vec<f32> {
    let source_rate = sample_rate as usize;
    let scale = echelle(bit_depth);
    // Stride factor to approximate 8kHz from native rate
    let stride = (source_rate / ONDE_HZ).max(1);
    let samples: Vec<f32> = samples_i32
        .iter()
        .step_by(stride)
        .map(|&s| s as f32 / scale)
        .collect();

    // Downsample to ~200 points (peak amplitude per chunk)
    let chunk_size = (samples.len() / POINTS).max(1);
    samples
        .chunks(chunk_size)
        .map(|chunk| chunk.iter().map(|s| s.abs()).fold(0.0f32, f32::max))
        .collect()
}

/// Ce que `POST /analyze/{track_id}` rend d'un morceau.
#[derive(Debug, Clone, PartialEq)]
pub struct Analyse {
    /// Le tempo estimé, `None` hors de [`BPM_MIN`]..=[`BPM_MAX`].
    pub bpm: Option<f64>,
    /// La durée vue par l'analyse, en secondes.
    pub duree_s: f64,
    /// Le nombre de fronts d'énergie comptés.
    pub beats: u32,
}

/// L'estimation de tempo par énergie, en fenêtres de 250 ms.
///
/// Rend [`TROP_COURT`] plutôt qu'un tempo fabriqué quand le morceau ne porte
/// pas de quoi calculer.
pub fn analyser(
    samples_i32: &[i32],
    sample_rate: u32,
    bit_depth: u16,
) -> Result<Analyse, &'static str> {
    let source_rate = sample_rate as usize;
    let scale = echelle(bit_depth);
    // Stride to approximate 22050 Hz from native rate
    let stride = (source_rate / ANALYSE_HZ).max(1);
    let effective_rate: usize = source_rate / stride;

    let samples: Vec<f32> = samples_i32
        .iter()
        .step_by(stride)
        .map(|&s| s as f32 / scale)
        .collect();

    // 250 ms windows for energy computation
    let window_size = effective_rate / 4;
    if window_size == 0 {
        return Err(TROP_COURT);
    }

    let energies: Vec<f32> = samples
        .chunks(window_size)
        .map(|chunk| chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32)
        .collect();

    if energies.len() < 4 {
        return Err(TROP_COURT);
    }

    let avg_energy: f32 = energies.iter().sum::<f32>() / energies.len() as f32;
    let threshold = avg_energy * 1.3;

    // Count onset peaks (energy crossing above threshold)
    let mut beats = 0u32;
    let mut prev_above = false;
    for &e in &energies {
        let above = e > threshold;
        if above && !prev_above {
            beats += 1;
        }
        prev_above = above;
    }

    let duree_s = samples.len() as f64 / effective_rate as f64;
    let bpm_raw = if duree_s > 0.0 {
        (beats as f64 / duree_s * 60.0).round()
    } else {
        0.0
    };
    // Only report BPM in plausible range
    let bpm = if (BPM_MIN..=BPM_MAX).contains(&bpm_raw) {
        Some(bpm_raw)
    } else {
        None
    };

    Ok(Analyse {
        bpm,
        duree_s,
        beats,
    })
}

// ---------------------------------------------------------------------------
// Essais (#3640)
// ---------------------------------------------------------------------------
//
// Cette caisse rendait `tune_dj: 0 passed` dans les deux jobs qui la nomment —
// un vert qui ne couvre rien. Ce qui suit garde les deux seules routes de DJ
// qui calculent quelque chose, par les fonctions que `dj_waveform` et
// `dj_analyze` appellent juste au-dessus.
//
// Ce que ces essais NE font pas : rejouer ce que
// `tune-server/tests/dj_plugin.rs` garde déjà (montage sous `/api/v1/ext/dj`,
// 404 sur une piste absente, persistance de `dj_enabled_{zone}`, décodage d'un
// vrai fichier). Ils prennent la suite là où cet essai s'arrête — il vérifie
// que le décodage a eu lieu, pas ce que le calcul en fait.

#[cfg(test)]
mod essais {
    use super::*;

    /// Longueur d'une salve, en échantillons. Assez courte pour tenir dans une
    /// fenêtre d'énergie de 250 ms à toutes les fréquences essayées ici.
    const SALVE: usize = 1_000;

    /// La plus grande valeur que `decode_to_pcm` puisse rendre à cette
    /// profondeur — cadrée à droite, comme le décodeur.
    fn crete(bit_depth: u16) -> i32 {
        match bit_depth {
            24 => (1 << 23) - 1,
            32 => i32::MAX,
            _ => 32_767,
        }
    }

    /// Une alternance pleine échelle : le signal le plus fort qu'un fichier
    /// puisse porter à cette profondeur.
    fn pleine_echelle(bit_depth: u16, n: usize) -> Vec<i32> {
        let c = crete(bit_depth);
        (0..n).map(|i| if i % 2 == 0 { c } else { -c }).collect()
    }

    /// Un train d'impulsions dont on connaît le tempo d'avance : une salve
    /// pleine échelle toutes les `periode_s` secondes, silence entre deux.
    fn train(sample_rate: u32, duree_s: f64, periode_s: f64, bit_depth: u16) -> Vec<i32> {
        let total = (f64::from(sample_rate) * duree_s) as usize;
        let periode = (f64::from(sample_rate) * periode_s) as usize;
        let c = crete(bit_depth);
        let mut v = vec![0i32; total];
        let mut debut = 0usize;
        while debut < total {
            let fin = (debut + SALVE).min(total);
            for (i, e) in v[debut..fin].iter_mut().enumerate() {
                // Alternance : une salve continue n'aurait pas d'énergie utile.
                *e = if (debut + i) % 2 == 0 { c } else { -c };
            }
            debut += periode;
        }
        v
    }

    // -----------------------------------------------------------------------
    // `onde` — appelée par `dj_waveform`
    // -----------------------------------------------------------------------

    /// ⭐ **Le garde qui compte pour l'auditeur.** Un signal pleine échelle doit
    /// rendre une onde pleine échelle, quelle que soit la profondeur du
    /// fichier. Le diviseur de [`echelle`] est le seul endroit où 16, 24 et 32
    /// bits se séparent ; s'y tromper d'un facteur 256 ne casse aucune
    /// compilation et n'apparaît que sur l'écran, en ligne plate ou en bloc
    /// plein — exactement le défaut qu'aucun essai ne pouvait voir, celui de
    /// `dj_plugin.rs` n'écrivant que des fichiers 16 bits.
    #[test]
    fn une_onde_pleine_echelle_vaut_un_a_seize_vingt_quatre_et_trente_deux_bits() {
        for bit_depth in [16u16, 24, 32] {
            let pcm = pleine_echelle(bit_depth, 44_100);
            let onde = onde(&pcm, 44_100, bit_depth);
            assert!(!onde.is_empty(), "{bit_depth} bits : onde vide");
            let plus_petit = onde.iter().copied().fold(f32::INFINITY, f32::min);
            let plus_grand = onde.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            assert!(
                (0.999..=1.0).contains(&plus_petit) && (0.999..=1.0).contains(&plus_grand),
                "{bit_depth} bits : crete hors de [0,999 ; 1,0] — min {plus_petit}, max {plus_grand}"
            );
        }
    }

    /// L'onde est un budget d'affichage, pas un flux : elle ne doit jamais
    /// partir en dizaines de milliers de points parce que le fichier est long.
    /// Le reste de la division donne au plus une tranche de queue, d'où le
    /// `POINTS + 1`.
    #[test]
    fn une_onde_ne_depasse_pas_le_nombre_de_points_annonce() {
        for secondes in [1u32, 30, 300] {
            let pcm = pleine_echelle(16, 44_100 * secondes as usize);
            let n = onde(&pcm, 44_100, 16).len();
            assert!(
                (POINTS..=POINTS + 1).contains(&n),
                "{secondes} s : {n} points, attendu {POINTS} ou {}",
                POINTS + 1
            );
        }
    }

    /// Une piste haute résolution passe par un décimage plus agressif
    /// (192 kHz → un échantillon sur 24). Le budget de points ne doit pas en
    /// dépendre : deux masters du même morceau donnent la même largeur d'onde.
    #[test]
    fn le_budget_de_points_ne_depend_pas_de_la_frequence() {
        let a = onde(&pleine_echelle(16, 44_100 * 30), 44_100, 16).len();
        let b = onde(&pleine_echelle(24, 192_000 * 30), 192_000, 24).len();
        assert_eq!(a, b, "44,1 kHz rend {a} points, 192 kHz en rend {b}");
    }

    /// Un silence rend des zéros, jamais un `NaN` : `serde_json` refuse de
    /// sérialiser un flottant non fini, et la route rendrait alors une erreur
    /// 500 sur un fichier parfaitement lisible.
    #[test]
    fn un_silence_rend_des_zeros_et_aucun_nan() {
        let onde = onde(&vec![0i32; 44_100], 44_100, 16);
        assert!(!onde.is_empty());
        assert!(
            onde.iter().all(|v| v.is_finite() && *v == 0.0),
            "un silence doit rendre des zeros finis : {onde:?}"
        );
        assert!(
            serde_json::to_string(&onde).is_ok(),
            "l'onde doit rester serialisable"
        );
    }

    /// Un extrait plus court que le budget de points ne doit ni paniquer sur
    /// une division par zéro, ni perdre ses échantillons.
    #[test]
    fn un_extrait_plus_court_que_le_budget_ne_panique_pas() {
        assert!(onde(&[], 44_100, 16).is_empty());
        let onde = onde(&pleine_echelle(16, 5), 8_000, 16);
        assert_eq!(
            onde.len(),
            5,
            "cinq echantillons doivent rendre cinq points"
        );
    }

    // -----------------------------------------------------------------------
    // `analyser` — appelée par `dj_analyze`
    // -----------------------------------------------------------------------

    /// ⭐ Un train d'impulsions à 120 BPM doit être reconnu à 120 BPM, et le
    /// même train dans un master 96 kHz / 24 bits doit rendre le MÊME chiffre.
    /// C'est ce que le décimage vers ~22 kHz est censé garantir ; l'essai de
    /// `dj_plugin.rs` ne pouvait pas le voir, une sinusoïde pure n'ayant aucun
    /// tempo.
    #[test]
    fn un_train_a_cent_vingt_bpm_est_reconnu_a_toutes_les_resolutions() {
        for (rate, bit_depth) in [(44_100u32, 16u16), (96_000, 24)] {
            let pcm = train(rate, 10.0, 0.5, bit_depth);
            let a = analyser(&pcm, rate, bit_depth).expect("dix secondes suffisent");
            assert_eq!(
                a.bpm,
                Some(120.0),
                "{rate} Hz / {bit_depth} bits : tempo {:?} au lieu de 120",
                a.bpm
            );
            assert_eq!(a.beats, 20, "{rate} Hz : vingt salves en dix secondes");
            assert!(
                (a.duree_s - 10.0).abs() < 0.01,
                "{rate} Hz : duree {} au lieu de 10",
                a.duree_s
            );
        }
    }

    /// ⭐ Le filtre de vraisemblance doit mordre. Un train à 54 BPM est bien
    /// compté — `beats` le dit — mais le tempo n'est PAS publié : annoncer
    /// « 54 BPM » sur un morceau dont on n'a détecté que neuf fronts en dix
    /// secondes serait une invention, et l'écran l'afficherait comme un fait.
    #[test]
    fn un_tempo_hors_de_la_plage_plausible_n_est_pas_publie() {
        let pcm = train(44_100, 10.0, 1.2, 16);
        let a = analyser(&pcm, 44_100, 16).expect("dix secondes suffisent");
        assert_eq!(a.beats, 9, "les fronts restent comptes");
        assert_eq!(
            a.bpm, None,
            "54 BPM est sous {BPM_MIN} : rien ne doit etre publie"
        );
    }

    /// Contre-épreuve de la borne haute : un morceau à 240 BPM ne doit se voir
    /// attribuer AUCUN tempo. Le garde porte sur ce que l'auditeur lit — rien
    /// plutôt qu'un chiffre faux — et non sur la mécanique qui y mène.
    ///
    /// ⚠️ **Constat de mesure, à ne pas confondre avec ce que ce témoin
    /// garde.** À 240 BPM, l'estimateur ne compte pas 240 : il compte **zéro**
    /// front. Un front est une fenêtre d'énergie de 250 ms qui passe au-dessus
    /// du seuil alors que la précédente était en dessous ; à une salve toutes
    /// les 250 ms, toutes les fenêtres se valent et aucune ne « monte ». Le
    /// plafond réellement atteignable par ce calcul est donc de l'ordre de
    /// 120 BPM — une fenêtre pleine, une fenêtre vide — et [`BPM_MAX`] à 200
    /// n'est jamais consulté par le haut. Ce n'est pas corrigé ici : ce lot
    /// pose des témoins, il ne réécrit pas l'estimateur.
    ///
    /// L'assertion reste vraie des deux côtés d'un tel correctif : 240 est
    /// au-dessus de [`BPM_MAX`], donc invariablement tu.
    #[test]
    fn contre_epreuve_un_morceau_a_deux_cent_quarante_bpm_ne_recoit_aucun_tempo() {
        // Une salve toutes les 0,25 s = 240 BPM.
        let pcm = train(44_100, 10.0, 0.25, 16);
        let a = analyser(&pcm, 44_100, 16).expect("dix secondes suffisent");
        assert_eq!(
            a.bpm, None,
            "240 BPM est au-dessus de {BPM_MAX} : rien ne doit etre publie"
        );
    }

    /// Trop court pour quatre fenêtres de 250 ms : le greffon doit le DIRE,
    /// pas rendre un tempo tiré de trois valeurs d'énergie.
    #[test]
    fn un_extrait_trop_court_est_refuse_au_lieu_d_etre_devine() {
        // 100 ms : une seule fenêtre d'énergie, très loin des quatre requises.
        let pcm = pleine_echelle(16, 4_410);
        assert_eq!(analyser(&pcm, 44_100, 16), Err(TROP_COURT));
        // Fréquence dégénérée : la fenêtre de 250 ms ferait zéro échantillon,
        // et `chunks(0)` panique. Le refus doit venir AVANT.
        assert_eq!(analyser(&pleine_echelle(16, 64), 3, 16), Err(TROP_COURT));
        // Aucun échantillon du tout.
        assert_eq!(analyser(&[], 44_100, 16), Err(TROP_COURT));
    }
}
