use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::audio::formats::AudioFormat;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::{AutoplayMode, Zone, ZoneRepo};
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::http::streamer::StreamInfo;
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::outputs::traits::{
    OutputDspState, OutputSignalPathStatus, OutputSignalReason, OutputVolumeState,
};
use tune_core::playback::{PlayState, ZoneState};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateZone {
    name: String,
    output_type: Option<String>,
    output_device_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateVolume {
    /// Accepts both 0.0-1.0 (float from web client) and 0-100 (integer legacy).
    ///
    /// `Option` depuis #1274 : une requête qui ne parle qu'en dB n'a pas à
    /// porter ce champ. `tune-remote` et `tune-widget` continuent de
    /// l'envoyer, le premier en 0..1 et le second en 0..100 — l'heuristique
    /// historique les départage et n'est pas touchée.
    volume: Option<f64>,
    /// Atténuation demandée en dB (≤ 0 ; `0` = pleine échelle). Exclusif avec
    /// `volume`. Ce champ ne connaît PAS l'ambiguïté 0..1 / 0..100 : un dB est
    /// un dB (#1274).
    volume_db: Option<f64>,
}

#[derive(Deserialize)]
struct UpdateMuted {
    muted: bool,
}

#[derive(Deserialize)]
struct RenameZone {
    name: String,
}

/// Rend un `null` JSON distinguable d'un champ absent : `Deserialize` n'est
/// appelé que si le champ est PRÉSENT (le `default` couvre l'absence), donc
/// envelopper son résultat dans `Some` donne `Some(None)` pour `null` et
/// `Some(Some(v))` pour une valeur.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

#[derive(Deserialize)]
struct PatchZone {
    name: Option<String>,
    volume: Option<i32>,
    /// Volume demandé en dB (≤ 0 ; `0` = 100 %). Exclusif avec `volume`.
    ///
    /// #1274 — `volume` est un ENTIER 0..100 : entre deux crans, l'écart va de
    /// 0,09 dB en haut d'échelle à 6 dB entre 1 % et 2 %. Ce champ contourne
    /// cette quantification à l'écriture ; la valeur est passée telle quelle à
    /// l'orchestrateur, qui la garde en `f64`.
    volume_db: Option<f64>,
    muted: Option<bool>,
    output_device_id: Option<String>,
    output_type: Option<String>,
    gapless_enabled: Option<bool>,
    sync_delay_ms: Option<i32>,
    /// Max output sample rate in Hz (e.g. 96000, 88200). null = no limit (passthrough).
    ///
    /// `Option<Option<_>>` seul ne suffit PAS : serde désérialise un `null`
    /// explicite en `None` extérieur, indistinguable d'un champ absent — le
    /// handler ne voyait donc jamais la demande d'effacement, et « Aucune »
    /// dans l'UI n'a jamais été enregistrable (Cyrille, forum 1320). Le
    /// désérialiseur dédié rétablit les trois états : champ absent → `None`,
    /// `null` → `Some(None)` (effacer), valeur → `Some(Some(v))`.
    #[serde(default, deserialize_with = "double_option")]
    max_sample_rate: Option<Option<u32>>,
    /// When enabled, sends audio at 100% volume (bit-perfect) and disables volume sync from device.
    fixed_volume: Option<bool>,
    /// Accord ponctuel de l'utilisateur pour une activation qui porte
    /// immédiatement la zone à 100 %. Exigé sur **toute** sortie, sans
    /// exception de type (#2395) : c'est le niveau qui sort des haut-parleurs
    /// qui est en jeu, pas l'identité de ce qu'on commande. Ce jeton appartient
    /// à la requête et n'est jamais persisté avec la zone.
    #[serde(default)]
    confirm_full_volume: bool,
    /// When enabled, automatically generates and queues similar tracks when the queue ends.
    ///
    /// #2271 — conserve pour les clients existants. `true` vaut
    /// `autoplay_mode: "similar"`, `false` vaut `"off"`. Si les deux champs
    /// arrivent ensemble, `autoplay_mode` l'emporte : il est le plus precis.
    autoplay_enabled: Option<bool>,
    /// Ce qui s'enchaine quand la file se vide : `"off"` ou `"similar"` (#2271).
    ///
    /// Remplace `autoplay_enabled`, qui ne pouvait pas porter un choix de
    /// source. Le catalogue est volontairement limite aux deux comportements
    /// qui existent reellement aujourd'hui — voir `AutoplayMode`.
    autoplay_mode: Option<String>,
    /// DSD output mode: "auto" (probe renderer), "native" (always passthrough), "pcm" (always transcode).
    dsd_mode: Option<String>,
    /// Décalage des paroles synchronisées, en ms (positif = paroles retardées).
    /// Compense la latence entre ce que le serveur sait et ce que l'auditeur
    /// entend — tampon de Tune puis du renderer (#1328).
    lyrics_offset_ms: Option<i32>,
    /// Force native FLAC to a DLNA renderer even if it doesn't advertise FLAC
    /// (empty/failed GetProtocolInfo Sink) — for renderers that decode FLAC but
    /// under-report (Denon Ceol N12).
    dlna_native_flac: Option<bool>,
    /// When enabled, serve ALAC straight to the renderer (bit-perfect, no FLAC
    /// transcode). Only for renderers that decode ALAC natively.
    alac_passthrough: Option<bool>,
    /// Servir l'AAC tel quel au renderer qui le décode, au lieu de le
    /// transcoder en FLAC (#1424). Opt-in : un renderer peut annoncer l'AAC
    /// et le refuser en pratique.
    aac_passthrough: Option<bool>,
    /// When enabled, transcode lossless to WAV/LPCM (not FLAC) for this DLNA
    /// renderer — skips the slow FLAC encoder for hi-res and avoids renderers
    /// whose ALAC decoder pops at start (LHC-56). Overrides alac_passthrough.
    dlna_lpcm: Option<bool>,
    /// When enabled, cap output to 16-bit for this DLNA renderer. For renderers
    /// that advertise `audio/flac` but only decode 16-bit (Ruark R3, #1137):
    /// downconverts hi-res to 16-bit FLAC instead of serving silent 24-bit direct.
    dlna_cap_16bit: Option<bool>,
    /// When enabled, serve genuine 24-bit WAV (instead of the 16-bit LPCM
    /// fallback) to a DLNA renderer that advertises `audio/L24`. The UI only
    /// offers this after the capability probe reports 24-bit LPCM support.
    dlna_wav24: Option<bool>,
    /// Per-zone SetAVTransportURI→Play delay in ms (0 = use config default). Lets
    /// a renderer with a cold-start under-run buffer before its transport clock
    /// starts (first seconds hachées — Cyrille, Yamaha R-N2000A).
    dlna_play_delay_ms: Option<i64>,
    /// Marque choisie par l'utilisateur dans le catalogue (ou « Autre »).
    /// Persistée en setting `zone_{id}_brand`. Chaîne vide = efface l'override.
    brand: Option<String>,
    /// Trim de gain du renderer en dB (±12), pour harmoniser le niveau perçu
    /// entre appareils. Persisté en setting `zone_{id}_gain_trim_db` ; `0`
    /// efface. Appliqué UNIQUEMENT au volume envoyé au device — le volume
    /// affiché et persisté reste celui de l'utilisateur. Sans effet sur une
    /// zone `fixed_volume` (bit-perfect assumé, le DAC gère).
    gain_trim_db: Option<f64>,
    /// La zone s'annonce en MediaRenderer UPnP (#1750). Persisté en setting
    /// `zone_{id}_upnp_renderer` ; défaut off. L'activation réveille
    /// l'annonceur SSDP pour une annonce immédiate.
    upnp_renderer: Option<bool>,
    /// « Silence UPnP » : ne plus rien demander du tout au renderer DLNA
    /// pendant la lecture (#2263). Persisté en setting
    /// `zone_{id}_upnp_silence` ; défaut off, STRICTEMENT opt-in.
    ///
    /// Par défaut, une zone DLNA abonnée aux évènements coûte déjà UNE action
    /// SOAP par seconde au lieu de trois — l'état, le volume et la coupure
    /// arrivent poussés, seule la position est encore mesurée. Cette option
    /// supprime cette dernière mesure et fait tomber le trafic à zéro.
    ///
    /// **Elle dégrade deux choses, et les deux se lisent dans
    /// `GET /api/devices/{id}/status` :**
    /// * la position devient une ESTIMATION (dernière position connue +
    ///   horloge murale), plus une valeur lue sur l'appareil ;
    /// * un déplacement fait sur la FAÇADE de l'appareil (télécommande,
    ///   molette) n'est vu qu'au prochain évènement du renderer.
    ///
    /// Sans abonnement tenu, l'option ne fait rien : la sortie retombe sur le
    /// sondage complet plutôt que de servir un état inventé.
    upnp_silence: Option<bool>,
    /// Modèle choisi par l'utilisateur (filtré par marque, ou texte libre).
    /// Persisté en setting `zone_{id}_model`. Chaîne vide = efface l'override.
    model: Option<String>,
    /// Sortie mono : sommer `M = (L + R) / 2` et émettre `M` sur les DEUX voies
    /// de la zone (#2362). Persisté en setting `zone_{id}_mono_downmix` ; défaut
    /// off, donc le comportement d'aujourd'hui ne change pas d'un bit tant que
    /// personne ne coche.
    ///
    /// **Sortie LOCALE uniquement**, et c'est le périmètre demandé : les
    /// sorties réseau ne sont pas touchées. Le réglage se persiste sur
    /// n'importe quelle zone, mais il n'agit que là où la chaîne DSP locale
    /// existe.
    ///
    /// Pour qui a une seule enceinte câblée sur un canal, la moitié de la
    /// musique est aujourd'hui inaudible (Nicolas Tardif, fil forum 1532 :
    /// « je perds toute la musique qui passe par le canal droit »). Ce n'est
    /// donc PAS derrière la barrière Premium : c'est une compensation de
    /// câblage, pas un effet de confort.
    mono_downmix: Option<bool>,
}

/// Une transition vers le volume fixe est une commande de volume à 100 %, pas
/// un simple réglage. Le serveur l'impose à tous les clients, y compris aux
/// anciennes interfaces et aux appels directs qui contournent le Web.
///
/// **Aucun type de sortie n'est dispensé** (#2395). La garde protège le niveau
/// qui sort des haut-parleurs, pas l'identité de ce qu'on commande : qui
/// écoutait à 20 % a compensé au gain de son ampli, et passer à pleine échelle
/// lui rend une quinzaine de décibels d'un coup — que l'atténuation vive dans
/// un renderer DLNA, dans la chaîne locale, ou dans le client web d'une zone
/// `browser`, souvent un casque sur un portable.
///
/// La garde ne lit donc plus le type de sortie du tout, et devient
/// trivialement fail-closed : il n'y a plus de branche à oublier, plus de type
/// inconnu à classer, et plus d'écart possible entre les deux gardes sœurs
/// (`full_volume_confirmation_required` pour le mode PURE, et celle du réglage
/// global dans `system/config.rs`), qui n'ont jamais rien dispensé.
fn fixed_volume_confirmation_required(zone: &Zone, body: &PatchZone) -> bool {
    body.fixed_volume == Some(true) && !zone.fixed_volume && !body.confirm_full_volume
}

/// Injecte l'identité appareil d'une zone dans son JSON de sortie :
/// - `brand` / `model` : override choisi par l'utilisateur (peut être `null`) ;
/// - `detected_manufacturer` / `detected_model` : détection UPnP du device
///   assigné (peut être `null`).
///
/// Le client affiche en priorité l'override utilisateur, sinon la détection
/// UPnP (override > détection).
fn inject_device_identity(
    obj: &mut serde_json::Map<String, Value>,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    detected: Option<&tune_core::discovery::device::DiscoveredDevice>,
) {
    let settings = SettingsRepo::with_backend(backend.clone());
    let brand = settings
        .get(&format!("zone_{zone_id}_brand"))
        .ok()
        .flatten();
    let model = settings
        .get(&format!("zone_{zone_id}_model"))
        .ok()
        .flatten();
    obj.insert("brand".into(), json!(brand));
    obj.insert("model".into(), json!(model));
    let trim = settings
        .get(&format!("zone_{zone_id}_gain_trim_db"))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    obj.insert("gain_trim_db".into(), json!(trim));
    let upnp_renderer = settings
        .get(&format!("zone_{zone_id}_upnp_renderer"))
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    obj.insert("upnp_renderer".into(), json!(upnp_renderer));
    // #2263 — « silence UPnP ». L'interrupteur ne part JAMAIS seul : ce qu'il
    // coûte part avec lui, pour qu'un client ne puisse pas le présenter comme
    // une simple économie. Les deux effets sont nommés en clair ; un client qui
    // ne lit que le booléen affiche au moins l'interrupteur au bon état.
    let upnp_silence = settings
        .get(&crate::config::cle_silence_upnp(zone_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    obj.insert("upnp_silence".into(), json!(upnp_silence));
    obj.insert(
        "upnp_silence_effets".into(),
        json!({
            "position_estimee": upnp_silence,
            "deplacement_facade_differe": upnp_silence,
            "texte": "Le serveur ne demande plus rien au lecteur réseau : la position affichée est estimée, et une avance faite sur l'appareil lui-même n'apparaît qu'au prochain changement qu'il signale."
        }),
    );
    // Sortie mono (#2362). Lue telle qu'elle est PERSISTÉE, sans la garde PURE :
    // c'est l'état de l'interrupteur que le client doit afficher. Ce que le
    // signal subit RÉELLEMENT est dit par le chemin du signal, qui applique la
    // règle PURE (`PlaybackOrchestrator::zone_mono_downmix_with`).
    let mono_downmix = settings
        .get(&format!("zone_{zone_id}_mono_downmix"))
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    obj.insert("mono_downmix".into(), json!(mono_downmix));
    obj.insert(
        "detected_manufacturer".into(),
        json!(detected.and_then(|d| d.manufacturer.clone())),
    );
    obj.insert(
        "detected_model".into(),
        json!(detected.and_then(|d| d.model.clone())),
    );
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(list_zones).post(create_zone).delete(delete_all_zones),
        )
        .route("/{id}", get(get_zone).patch(patch_zone).delete(delete_zone))
        .route("/{id}/volume", put(update_volume))
        .route("/{id}/muted", put(update_muted))
        .route("/{id}/dsp", get(get_zone_dsp).put(set_zone_dsp))
        .route("/{id}/convolver/response", get(convolver_response))
        .route("/{id}/renderer-capabilities", post(renderer_capabilities))
        .route("/{id}/device-presets", get(get_device_presets))
        .route("/{id}/name", put(rename_zone))
        .route("/sync-status", get(sync_status))
        .route("/{id}/network-health", get(network_health))
        .route("/group-delays", get(list_group_delays).put(set_group_delay))
        .route("/group", get(list_groups).post(create_group))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/list", get(list_groups))
        .route(
            "/group/{group_id}",
            axum::routing::patch(patch_group).delete(delete_group),
        )
        .route(
            "/groups/{group_id}",
            axum::routing::patch(patch_group).delete(delete_group),
        )
        .route(
            "/groups/{group_id}/volume",
            axum::routing::post(group_volume),
        )
        .route(
            "/groups/{group_id}/calibrate",
            axum::routing::post(calibrate_group),
        )
        .route("/groups/{group_id}/health", get(group_health))
        .route(
            "/stereo-pairs",
            get(list_stereo_pairs).post(create_stereo_pair),
        )
        .route(
            "/stereo-pairs/{pair_id}",
            axum::routing::delete(delete_stereo_pair),
        )
}

pub async fn list_zones_handler(State(state): State<AppState>) -> Json<Value> {
    list_zones(State(state)).await
}

async fn get_zone_dsp(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let eq_key = format!("zone_{id}_eq_profile");
    let eq_profile: Option<tune_core::audio::eq::EqProfile> = settings
        .get(&eq_key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Headphone crossfeed config (local output only). Defaults when unset:
    // disabled, amount 0.30, delay 0.30 ms.
    let crossfeed = read_crossfeed_config(&settings, id);

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
        }))
        .into_response(),
    }
}

/// Cache of computed convolver responses, keyed by zone id. The value pairs
/// the filter fingerprint (path + size + mtime) with the full response body:
/// re-uploading an IR rewrites the file, so the fingerprint changes and the
/// entry is recomputed on the next read — no explicit invalidation hook needed.
static CONVOLVER_RESPONSE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<i64, (String, Value)>>,
> = std::sync::OnceLock::new();

/// `GET /zones/{id}/convolver/response` — frequency response of the zone's FIR
/// convolver, for visualisation. Not premium-gated: applying an IR is, reading
/// the resulting curve is not.
///
/// The running convolver only keeps its IR in FFT-partitioned form, so the taps
/// are re-read from the persisted IR file (`ir_path_{zone_id}` setting — same
/// source of truth as `restore_convolvers` and the transcode path). Multi-
/// channel IRs are summarised by channel 0: averaging L/R taps would let
/// inter-channel phase differences cancel and distort the magnitude curve.
async fn convolver_response(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(_)) => {}
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    }

    let ir_path = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("ir_path_{id}"))
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let Some(ir_path) = ir_path else {
        return Json(json!({"loaded": false})).into_response();
    };
    let Ok(meta) = std::fs::metadata(&ir_path) else {
        // Path persisted but file gone (moved data dir…): nothing to plot.
        return Json(json!({"loaded": false})).into_response();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fingerprint = format!("{ir_path}|{}|{mtime}", meta.len());

    let cache = CONVOLVER_RESPONSE_CACHE.get_or_init(Default::default);
    if let Some((fp, body)) = cache.lock().expect("convolver cache poisoned").get(&id) {
        if *fp == fingerprint {
            return Json(body.clone()).into_response();
        }
    }

    // ~200 log-spaced points × up to 128k taps of f64 accumulation: fast, but
    // not "handler on the async runtime" fast — compute on the blocking pool.
    let computed = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let (ir, sample_rate) = tune_core::audio::convolver::Convolver::read_ir_taps(&ir_path)?;
        if sample_rate == 0 {
            return Err("IR sample rate is 0".into());
        }
        let taps = &ir[0]; // channel 0 (see handler doc)
        let f_hi = 20_000.0f64.min(sample_rate as f64 * 0.45);
        let freqs = tune_core::audio::convolver::log_freq_grid(200, 20.0, f_hi);
        let points: Vec<Value> =
            tune_core::audio::convolver::fir_frequency_response(taps, sample_rate, &freqs)
                .into_iter()
                .map(|p| {
                    json!({
                        "f": (p.freq_hz * 10.0).round() / 10.0,
                        "db": (p.magnitude_db * 100.0).round() / 100.0,
                        "phase_deg": (p.phase_deg * 100.0).round() / 100.0,
                    })
                })
                .collect();
        let latency_ms = taps.len() as f64 / 2.0 / sample_rate as f64 * 1000.0;
        Ok(json!({
            "loaded": true,
            "taps": taps.len(),
            "sample_rate": sample_rate,
            "latency_ms": (latency_ms * 10.0).round() / 10.0,
            "points": points,
        }))
    })
    .await;

    match computed {
        Ok(Ok(body)) => {
            cache
                .lock()
                .expect("convolver cache poisoned")
                .insert(id, (fingerprint, body.clone()));
            Json(body).into_response()
        }
        Ok(Err(e)) => {
            warn!(zone_id = id, error = %e, "convolver_response_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("read IR: {e}")})),
            )
                .into_response()
        }
        Err(e) => {
            warn!(zone_id = id, error = %e, "convolver_response_join_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "response computation failed"})),
            )
                .into_response()
        }
    }
}

/// Read the `zone_{id}_crossfeed` settings row into a normalised JSON object,
/// falling back to defaults (disabled, amount 0.30, delay 0.30 ms) for any
/// missing/invalid field. Shape: `{ enabled, amount, delay_ms }`.
fn read_crossfeed_config(settings: &tune_core::db::settings_repo::SettingsRepo, id: i64) -> Value {
    let stored: Option<Value> = settings
        .get(&format!("zone_{id}_crossfeed"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let v = stored.unwrap_or(Value::Null);
    let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false);
    let amount = v.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.30);
    let delay_ms = v.get("delay_ms").and_then(|d| d.as_f64()).unwrap_or(0.30);
    json!({
        "enabled": enabled,
        "amount": amount,
        "delay_ms": delay_ms,
    })
}

async fn set_zone_dsp(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Premium gate: DSP & EQ mutations require Premium. Le refus parle la
    // langue de l'application (#2419) — c'est le même écran « Égaliseur » que
    // `POST /zones/{id}/eq`, et il tire ses deux moitiés d'ici et de là.
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
    .await
    {
        return resp;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    // Handle eq_profile if present
    let mut eq_applique_a_chaud = false;
    if let Some(eq_val) = body.get("eq_profile") {
        if let Ok(profile) =
            serde_json::from_value::<tune_core::audio::eq::EqProfile>(eq_val.clone())
        {
            let key = format!("zone_{id}_eq_profile");
            let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());
            // Persister ne suffit pas : sans ceci le reglage n'atteint le son
            // qu'a la piste SUIVANTE sur une zone locale (#1725). `POST
            // /zones/{id}/eq` le fait deja ; cette route ecrit la MEME cle et
            // ne le faisait pas.
            eq_applique_a_chaud = state.orchestrator.apply_eq_change(id).await;
        }
    }

    // Handle crossfeed sub-object if present (local-output headphone effect).
    // Same premium gate (Feature::DspEq) as the EQ path above. Ranges clamped:
    // amount 0..0.5, delay_ms 0..5. Persisted to `zone_{id}_crossfeed`.
    let mut crossfeed_saved: Option<Value> = None;
    let mut cf_applique_a_chaud = false;
    if let Some(cf_val) = body.get("crossfeed") {
        let enabled = cf_val
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let amount = cf_val
            .get("amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 0.5);
        let delay_ms = cf_val
            .get("delay_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 5.0);
        let normalised = json!({
            "enabled": enabled,
            "amount": amount,
            "delay_ms": delay_ms,
        });
        let key = format!("zone_{id}_crossfeed");
        let _ = settings.set(
            &key,
            &serde_json::to_string(&normalised).unwrap_or_default(),
        );
        crossfeed_saved = Some(normalised);
        // Meme raison que pour l'egaliseur juste au-dessus : persister ne
        // suffit pas. Sans ceci, activer le crossfeed ou deplacer `amount` /
        // `delay_ms` en ecoutant ne changeait rien avant la piste suivante
        // (#1786).
        cf_applique_a_chaud = state.orchestrator.refresh_zone_crossfeed(id).await;
    }

    let preset_id = body["dsp_preset_id"].as_i64();
    let enabled = body["dsp_enabled"].as_bool().unwrap_or(false);
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let _ = repo.update_dsp(id, preset_id, enabled);

    Json(json!({
        "zone_id": id,
        "dsp_preset_id": preset_id,
        "dsp_enabled": enabled,
        "eq_profile": body.get("eq_profile"),
        "crossfeed": crossfeed_saved,
        // Meme contrat que `POST /zones/{id}/eq` : vrai quand le reglage vient
        // d'atteindre le son d'un flux en cours. Faux ne signale PAS un echec
        // (rien ne joue, zone non locale, mode PURE) — c'est ce qui permet a un
        // client de dire « prendra effet a la piste suivante » au lieu de
        // laisser croire a un egaliseur muet.
        "eq_applied_live": eq_applique_a_chaud,
        // Idem pour le crossfeed (#1786).
        "crossfeed_applied_live": cf_applique_a_chaud,
    }))
    .into_response()
}

async fn sync_status(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let metrics = state.poller_metrics.lock().await;

    let mut zone_data = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let poller = metrics.get(&zone_id).cloned().unwrap_or_default();
        let group_id = z.group_id.as_deref();
        zone_data.push(json!({
            "zone_id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "position_ms": ps.position_ms,
            "duration_ms": ps.now_playing.as_ref().map(|np| np.duration_ms).unwrap_or(0),
            "now_playing": ps.now_playing.as_ref().map(|np| json!({
                "title": np.title,
                "artist": np.artist_name,
                "album": np.album_title,
            })),
            "group_id": group_id,
            "poller": poller,
        }));
    }

    Json(json!({
        "zones": zone_data,
        "groups": groups,
        "total_zones": zones.len(),
        "playing_count": zone_data.iter().filter(|z| z["state"] == "playing").count(),
    }))
}

/// Durée d'observation minimale avant d'oser annoncer un débit.
///
/// Les premiers blocs d'une session partent en rafale — remplissage du tampon,
/// en-tête WAV, réponse au `Range` initial. Rapportés aux quelques dizaines de
/// millisecondes qui viennent de s'écouler, ils donnent un débit à cinq
/// chiffres qui ne décrit rien. Une seconde suffit à lisser l'amorçage.
const FENETRE_MINIMALE: std::time::Duration = std::time::Duration::from_secs(1);

/// Les deux faits d'une mesure de débit, lus ENSEMBLE sur la MÊME session.
///
/// C'est le point du correctif : le compteur d'octets et la durée pendant
/// laquelle ils sont partis doivent décrire le même objet. Les lire à deux
/// endroits différents est exactement ce qui avait permis de diviser les
/// octets d'une session par l'ancienneté du SERVEUR.
///
/// Ce que compte `bytes_sent` : TOUT ce que le serveur a émis pour cette
/// session, tous chemins de sortie confondus (fichier, radio, mandataire —
/// voir `corps_compte` dans `tune-core/src/http/streamer.rs`). Sortie locale,
/// renderer DLNA et — depuis #2738 — le relais du pont y sont additionnés. Ce
/// n'est donc pas « ce que reçoit un navigateur », c'est ce que la zone a fait
/// sortir.
fn mesure_de_session(
    session: &tune_core::http::streamer::StreamSession,
) -> (u64, std::time::Duration) {
    (
        session
            .bytes_sent
            .load(std::sync::atomic::Ordering::Relaxed),
        session.created_at.elapsed(),
    )
}

/// Le débit MOYEN observé sur la vie du flux, en kbit/s — ou `None`.
///
/// `None` n'est pas `0.0`. Les deux se lisent pareil à l'écran et ne disent
/// pas la même chose : `0.0` affirme que rien ne circule, `None` dit qu'on n'a
/// pas de quoi mesurer. Le champ rendait `0.0` dans les deux cas, si bien
/// qu'un flux qui démarre était annoncé muet.
///
/// Le calcul reste en flottant de bout en bout. `octets * 8 / 1000` était une
/// division ENTIÈRE, faite avant celle par le temps : les décimales étaient
/// jetées là, et l'arrondi final à la décimale près ne rattrapait qu'un
/// chiffre déjà faux.
///
/// C'est une MOYENNE sur la session, pas un débit instantané : une pause en
/// cours de piste continue de creuser la fenêtre et tire la valeur vers le
/// bas. Ce qui est garanti, c'est que la fenêtre appartient au flux mesuré.
fn debit_observe_kbps(octets_envoyes: u64, fenetre: std::time::Duration) -> Option<f64> {
    if octets_envoyes == 0 || fenetre < FENETRE_MINIMALE {
        return None;
    }
    let kbps = octets_envoyes as f64 * 8.0 / 1000.0 / fenetre.as_secs_f64();
    Some((kbps * 10.0).round() / 10.0)
}

async fn network_health(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let metrics = state.poller_metrics.lock().await;
    let poller = metrics.get(&id).cloned().unwrap_or_default();
    let ps = state.playback.get_state(id).await;

    let mesure: Option<(u64, std::time::Duration)> = if let Some(ref np) = ps.now_playing
        && let Some(ref sid) = np.stream_id
    {
        let sessions = state.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions.get(sid.as_str()).map(|s| mesure_de_session(s))
    } else {
        None
    };

    let stream_bytes = mesure.map_or(0, |(octets, _)| octets);
    let bitrate_kbps = mesure.and_then(|(octets, fenetre)| debit_observe_kbps(octets, fenetre));

    Json(json!({
        "zone_id": id,
        "bytes_sent": stream_bytes,
        "bitrate_kbps": bitrate_kbps,
        "poll_latency_ms": poller.last_latency_ms,
        "max_latency_ms": poller.max_latency_ms,
        "poll_errors": poller.total_errors,
        "total_polls": poller.total_polls,
    }))
}

#[cfg(test)]
mod debit_de_zone_tests {
    use super::{FENETRE_MINIMALE, debit_observe_kbps, mesure_de_session};
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Duration;
    use tune_core::http::streamer::{StreamInfo, StreamSession};

    fn session_de_test() -> StreamSession {
        StreamSession::new(
            "session-de-test".to_string(),
            StreamInfo {
                format: "flac".to_string(),
                mime_type: "audio/flac".to_string(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                file_size: None,
                duration_ms: None,
                seek_ms: None,
            },
            true,
            8,
        )
    }

    /// La fenêtre de mesure appartient au FLUX, pas au serveur.
    ///
    /// Une session qui vient de naître n'a rien à annoncer, quel que soit le
    /// nombre d'octets déjà comptés : on n'a pas encore observé assez
    /// longtemps. L'horloge du serveur, elle, aurait rendu un chiffre — c'est
    /// tout le défaut : elle avance depuis le démarrage du processus et ne
    /// sait rien de ce flux-ci.
    #[test]
    fn la_fenetre_de_mesure_est_celle_de_la_session() {
        let session = session_de_test();
        session.bytes_sent.store(1_000_000, Relaxed);

        let (octets, fenetre) = mesure_de_session(&session);

        assert_eq!(octets, 1_000_000, "le compteur de la session doit être lu");
        assert!(
            fenetre < FENETRE_MINIMALE,
            "session tout juste créée : sa fenêtre vaut {fenetre:?}, \
             elle ne peut pas déjà dépasser {FENETRE_MINIMALE:?}"
        );
        assert_eq!(
            debit_observe_kbps(octets, fenetre),
            None,
            "trop tôt pour mesurer ce flux — une horloge de serveur, elle, \
             aurait fourni une fenêtre et donc un chiffre"
        );
    }

    /// Un débit qu'on n'a pas mesuré ne s'annonce pas.
    #[test]
    fn aucun_octet_ne_permet_aucune_annonce() {
        assert_eq!(
            debit_observe_kbps(0, Duration::from_secs(30)),
            None,
            "pas un octet envoyé : il n'y a rien à mesurer, donc rien à annoncer"
        );
    }

    /// Trop tôt pour mesurer : la rafale d'amorçage n'est pas un débit.
    #[test]
    fn une_fenetre_trop_courte_ne_permet_aucune_annonce() {
        assert_eq!(
            debit_observe_kbps(200_000, Duration::from_millis(120)),
            None,
            "120 ms de session : le remplissage du tampon n'est pas un débit"
        );
    }

    /// Le débit annoncé est celui qu'on a compté, pas un entier arrondi en
    /// chemin. `octets * 8 / 1000` en arithmétique entière jette les décimales
    /// AVANT la division par le temps.
    #[test]
    fn le_debit_annonce_est_la_mesure_pas_une_troncature() {
        assert_eq!(
            debit_observe_kbps(12_345, Duration::from_secs(1)),
            Some(98.8),
            "12 345 octets en 1 s = 98,76 kbit/s, arrondi à 98,8 — pas 98,0"
        );
    }

    /// Le cas nominal : un FLAC stéréo 16/44,1 tourne autour de 1 000 kbit/s.
    #[test]
    fn un_flac_s_annonce_a_son_vrai_debit() {
        assert_eq!(
            debit_observe_kbps(1_000_000, Duration::from_secs(8)),
            Some(1000.0),
            "1 Mo en 8 s = 1 000 kbit/s"
        );
    }
}

pub async fn create_zone_handler(
    state: State<AppState>,
    ConnectInfo(client_addr): ConnectInfo<std::net::SocketAddr>,
    mut body: Json<CreateZone>,
) -> impl IntoResponse {
    // Every web client creates its browser-output zone under the same generic
    // name ("Cet ordinateur"), so several clients show up as indistinguishable
    // duplicates in the zone list (Bertrand). Append the client IP so each
    // machine is identifiable. Guarded to avoid doubling the suffix on retries.
    if body.output_type.as_deref() == Some("browser") {
        let ip = client_addr.ip().to_string();
        if !body.name.contains(&ip) {
            body.name = format!("{} ({ip})", body.name.trim());
        }
    }
    create_zone(state, body).await
}

/// Public wrapper for use from ws.rs snapshot builder.
/// Complète le JSON `current_track` d'une zone avec l'ancrage temporel de
/// métadonnée : `metadata_changed_at` (epoch ms, horloge serveur) et
/// `metadata_age_ms` (âge calculé côté serveur — le client s'ancre dessus
/// sans dépendre de la synchronisation de son horloge). Utilisé par les
/// paroles des pistes radio pour caler les lignes sur le début du morceau
/// détecté dans le flux.
pub(crate) fn inject_metadata_anchor(obj: &mut serde_json::Map<String, Value>, ps: &ZoneState) {
    let (Some(ts), Some(age)) = (ps.metadata_changed_at_ms, ps.metadata_age_ms()) else {
        return;
    };
    if let Some(track) = obj.get_mut("current_track").and_then(|v| v.as_object_mut()) {
        track.insert("metadata_changed_at".into(), json!(ts));
        track.insert("metadata_age_ms".into(), json!(age));
    }
}
/// La nature et l'identifiant de ce que l'auditeur a DEMANDÉ, dits au client.
///
/// `POST /zones/:id/play` les déduit déjà du corps de la requête
/// (`contexte_de_lecture`) et les pose sur la session de la zone
/// (`set_session_context`, #2441). Jusqu'ici l'unique lecteur était
/// l'orchestrateur, au moment de tamponner `listen_history` : le serveur
/// savait quel album Qobuz jouait sur quelle zone, et ne le disait à personne.
///
/// Le client ne POUVAIT donc pas bâtir le « Retour à l'album en cours » que
/// Cyrille Moutia réclame depuis le 30/06/2026 (#1361).
/// `current_track.album_id` / `artist_id` sont des `i64` de BIBLIOTHÈQUE :
/// toujours `null` sur une piste de service, faute de ligne en base. Il ne
/// restait que `source` + `source_id`, l'identifiant de la PISTE — d'où un
/// aller-retour `GET /streaming/{service}/tracks/{track_id}` à chaque
/// changement de piste pour seulement en retrouver l'album.
///
/// Au niveau de la ZONE, pas de `current_track` : le contexte décrit le geste,
/// pas la piste. Il survit aux avances automatiques — la deuxième piste d'un
/// album reste une écoute « album » — là où `current_track` change à chaque
/// piste. C'est exactement ce qui fait un « retour à l'album » stable.
///
/// Toujours écrits, `null` compris. Un champ ABSENT dit « ce serveur ne
/// connaît pas la notion » ; un champ `null` dit « aucun contexte sur cette
/// session ». Le client doit distinguer les deux pour MASQUER le raccourci ou
/// le GRISER, et un défaut silencieux côté client masquerait la disparition du
/// champ au lieu de la signaler (leçon de `volume_db`, #1274).
///
/// `GET /zones/{id}/status` les publiait déjà — il sérialise le `ZoneState`
/// entier — mais sous une charge de forme différente (`now_playing`). Les
/// trois surfaces qui portent `current_track` restaient muettes ; ce
/// fabricant les aligne, à l'identique de `inject_metadata_anchor`, et aux
/// mêmes trois points d'appel, pour qu'elles ne puissent plus diverger.
///
/// `session_context_source` complète la paire, et sans lui elle ne suffisait
/// pas à ROUVRIR ce qui joue. L'identifiant est une chaîne nue tirée de deux
/// espaces de noms : un `i64` de la table `albums`, ou l'identifiant
/// d'édition d'un service. Le chemin LOCAL s'en tirait tout seul — la
/// bibliothèque est l'espace de noms implicite, `"42"` s'ouvre par
/// `GET /albums/42`. Le chemin QOBUZ, celui du ticket, restait nu :
/// `("album", "0060254735822")` ne dit pas chez qui l'ouvrir, quand
/// `GET /streaming/{service}/albums/{id}` réclame ce `{service}`. Le client
/// n'avait plus qu'à supposer que le service affiché à l'écran est celui qui
/// joue — faux dès qu'on regarde Tidal en écoutant Qobuz, et la devinette
/// même que #1284 a condamnée.
pub(crate) fn inject_session_context(obj: &mut serde_json::Map<String, Value>, ps: &ZoneState) {
    obj.insert(
        "session_context_type".into(),
        json!(ps.session_context_type),
    );
    obj.insert("session_context_id".into(), json!(ps.session_context_id));
    obj.insert(
        "session_context_source".into(),
        json!(ps.session_context_source),
    );
}

/// Qui a le droit de recevoir l'adresse du flux interne — la règle, UNE fois.
///
/// `/stream/{id}` n'admet qu'UN consommateur (`streamer.rs`, un canal mpsc) :
/// une seconde connexion sur la même session fait `break` sur la première. La
/// coupure est propre — un `EOF`, et la sortie journalise
/// `local_audio_stream_eof` — mais elle ARRÊTE la lecture en cours.
///
/// Publier cette adresse à une zone dont la sortie n'est PAS l'onglet, c'est
/// donc donner à cet onglet de quoi voler le flux au renderer (DLNA /
/// Chromecast / AirPlay / SlimProto / local). C'est le défaut d'eric (#954) :
/// « je ferme l'onglet et le son revient ».
///
/// Une zone `browser` la reçoit : là, l'onglet EST la sortie, et le client web
/// branche son `<audio>` dessus (`stores/zones.ts`, `handleBrowserPlayback`).
///
/// #3164 — cette règle était ÉCRITE cinq fois et POSÉE une seule
/// (`build_zone_json`). `GET /zones`, `GET /zones/{id}`,
/// `GET /zones/{id}/status`, les vingt routes de lecture qui passent par
/// `build_zone_json_with_result` et `POST /radios/{id}/play/{zone_id}`
/// publiaient l'adresse à tout le monde. Elle vit désormais ici, et
/// [`inject_stream_url`] est le seul chemin qui la pose.
pub fn zone_recoit_l_adresse_du_flux(output_type: Option<&str>) -> bool {
    output_type == Some("browser")
}

/// Pose `stream_url` (et `stream_url_remote`, quand le pont est actif) sur la
/// charge utile d'une zone — ou ne pose RIEN quand
/// [`zone_recoit_l_adresse_du_flux`] le refuse.
///
/// Rend `true` quand l'adresse a été publiée, pour que l'appelant puisse le
/// dire sans relire la charge utile.
pub(crate) fn inject_stream_url(
    obj: &mut serde_json::Map<String, Value>,
    state: &AppState,
    output_type: Option<&str>,
    stream_id: Option<&str>,
) -> bool {
    if !zone_recoit_l_adresse_du_flux(output_type) {
        return false;
    }
    let Some(stream_id) = stream_id else {
        return false;
    };
    let server_ip = state.config.advertised_ip.clone().unwrap_or_else(|| {
        tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into())
    });
    const EXT: &str = "flac";
    obj.insert(
        "stream_url".into(),
        json!(format!(
            "http://{}:{}/stream/{}.{}",
            server_ip, state.port, stream_id, EXT
        )),
    );
    // Adresse joignable de l'exterieur, quand le pont est actif.
    if let Some(distant) =
        crate::routes::stream_handler::stream_url_distant(state.backend.clone(), stream_id, EXT)
    {
        obj.insert("stream_url_remote".into(), json!(distant));
    }
    true
}

/// Délai au-delà duquel une zone navigateur qui « joue » sans que personne ne
/// tire son flux n'est plus un démarrage lent mais un silence.
///
/// Le client web branche son `<audio>` sur `stream_url` dès qu'il a l'état de
/// la zone ; les premiers octets partent en une seconde ou deux. Douze
/// secondes laissent de la marge à un poste lent sans laisser l'utilisateur
/// dans le noir.
///
/// La valeur vit dans `tune-core` : le poller s'en sert pour ABANDONNER une
/// lecture que personne ne reçoit (#2630), et cette vue pour la DIRE. Deux
/// seuils distincts, et Tune afficherait à la fois « aucun onglet ne reçoit le
/// son » et une lecture en cours — c'est précisément le défaut signalé.
const BROWSER_UNATTENDED_GRACE: std::time::Duration = tune_core::poller::DELAI_SILENCE_ETABLI;
/// Combien de temps l'explication du silence survit à la lecture qui l'a
/// produite.
///
/// `output_reach` décrit l'instant présent, et le bandeau qui le rend est le
/// SEUL endroit où Tune dit pourquoi une zone navigateur ne sonne pas. Les
/// deux ensemble s'annulaient : la valeur ne pouvait être
/// `browser_unattended` qu'en lecture, or la lecture cesse exactement quand
/// le défaut se manifeste — l'utilisateur arrête une zone muette, ou le
/// poller l'abandonne au même seuil (`DELAI_SILENCE_ETABLI`, #2630). Le
/// message s'effaçait au geste qu'il était censé prévenir : Pierre M l'a vu
/// passer, n'a pas pu le relire, et l'a rapporté de travers — ce contresens
/// a détourné l'instruction de #2571 pendant plusieurs échanges (#2588).
///
/// Deux minutes : de quoi lire deux phrases, aller chercher l'onglet nommé
/// par le message, et le retrouver encore affiché en revenant. Borné dans
/// l'autre sens pour qu'une zone laissée tranquille cesse d'accuser un onglet
/// dont plus personne ne se soucie. Toute nouvelle lecture l'efface avant
/// l'échéance : la question est rouverte, la réponse d'avant ne vaut plus.
const BROWSER_UNATTENDED_RETENTION: std::time::Duration = std::time::Duration::from_secs(120);

/// Où va réellement le son de cette zone, dit au client.
///
/// `online` ne suffit pas : il répond « la sortie répond-elle ? », pas « y
/// a-t-il une sortie ? », et il vaut `true` en permanence pour une zone
/// navigateur — y compris quand aucun navigateur n'écoute. Or ces deux états
/// produisent exactement le même tableau qu'une lecture réussie : la file se
/// remplit, le flux est décodé, la position avance, et rien ne sort.
///
/// Ils n'étaient visibles que dans un WARN de journal. Bilou (#1499) a ouvert
/// **deux** fils forum sur un défaut BluOS inexistant avant qu'on lise ses
/// logs : neuf lectures sur la zone « Ce PC », `output_sent=false` neuf fois.
/// L'interface ne lui laissait aucune autre piste que son matériel.
///
/// - `"ok"` — le son a une destination.
/// - `"no_output"` — aucune sortie associée à la zone. La lecture est refusée
///   en amont (`zone_no_output_device`) ; ce champ le dit **avant** le clic.
/// - `"browser_unattended"` — zone navigateur en lecture depuis assez
///   longtemps pour que ce ne soit plus un démarrage, dont pas un octet n'a
///   été tiré : l'onglet qui devait jouer n'est pas là.
/// Les VU-mètres de cette zone ont-ils une source ?
///
/// Le client ne peut pas distinguer « aucune mesure sur ce chemin » de « des
/// mesures qui n'arrivent pas encore » : dans les deux cas l'aiguille ne bouge
/// pas. Une aiguille figée MENT — elle annonce un signal constant. Grisée, elle
/// dit la vérité : on ne mesure pas ici. D'où ce champ, que le serveur seul
/// peut renseigner. Cas unique aujourd'hui : OAAT en DSD natif (Xavier/Zicmu).
pub(crate) async fn levels_available(state: &AppState, zone: &Zone) -> bool {
    state
        .orchestrator
        .output_produces_levels(zone.output_device_id.as_deref())
        .await
}

/// Contrat de commandes de la sortie réellement enregistrée pour une zone.
///
/// `None` couvre les zones navigateur et les sorties disparues. Le client ne
/// doit pas transformer cette absence en une liste de capacités inventée.
pub(crate) async fn output_capabilities(
    state: &AppState,
    output_device_id: Option<&str>,
) -> Option<tune_core::outputs::OutputCapabilities> {
    let device_id = output_device_id?;
    let output = { state.outputs.lock().await.get(device_id) }?;
    Some(output.lock().await.capabilities())
}

/// Motif pour lequel la sortie d'une zone ne peut pas tenir une consigne en dB.
///
/// `None` = rien à redire : sortie inconnue (zone navigateur, sortie
/// disparue), réglage continu, ou consigne dans la portée de la grille. Voir
/// [`tune_core::audio::volume_scale::refus_de_resolution`] pour le cas qui
/// mord et pourquoi il vaut mieux le dire que le simuler (#1274).
pub(crate) async fn refus_de_resolution_volume(
    state: &AppState,
    output_device_id: Option<&str>,
    db: f64,
) -> Option<String> {
    let capabilities = output_capabilities(state, output_device_id).await?;
    tune_core::audio::volume_scale::refus_de_resolution(capabilities.volume_resolution, db)
}

pub(crate) async fn output_reach(state: &AppState, zone: &Zone, ps: &ZoneState) -> &'static str {
    // Le seul fait qu'on ne puisse pas déduire : quelqu'un tire-t-il le flux ?
    // On ne le demande au streamer que pour une zone navigateur en lecture,
    // pour ne rien changer au coût de la liste des zones.
    let pulled = if zone.output_type.as_deref() == Some("browser")
        && matches!(ps.state, PlayState::Playing)
    {
        match ps
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        {
            Some(sid) => state
                .streamer
                .stream_bytes_sent(sid)
                .await
                .is_some_and(|n| n > 0),
            None => false,
        }
    } else {
        false
    };
    let reach = output_reach_of(zone, ps, pulled);
    // Le serveur ne dit pas `browser_unattended` à un client sans en garder
    // trace : c'est cette trace, et elle seule, qui permet au bandeau de
    // survivre à l'arrêt de la lecture (#2588). Rafraîchie tant que le
    // silence dure, levée dès que l'onglet tire enfin le flux. Écriture
    // réservée à la zone navigateur EN LECTURE — le cas anormal — pour ne
    // rien changer au coût de la liste des zones.
    if let Some(zone_id) = zone.id
        && zone.output_type.as_deref() == Some("browser")
        && matches!(ps.state, PlayState::Playing)
    {
        let constate = reach == "browser_unattended";
        if constate || ps.browser_unattended_at.is_some() {
            state
                .playback
                .note_browser_unattended(zone_id, constate)
                .await;
        }
    }
    reach
}

/// La décision seule, sans I/O — c'est elle que les tests couvrent.
fn output_reach_of(zone: &Zone, ps: &ZoneState, browser_stream_pulled: bool) -> &'static str {
    if zone.output_type.as_deref() != Some("browser") {
        return if zone.output_device_id.is_none() {
            "no_output"
        } else {
            "ok"
        };
    }

    // Zone navigateur : la sortie, c'est l'onglet. On ne peut pas la
    // découvrir, seulement constater qu'elle consomme — ou non.
    if !matches!(ps.state, PlayState::Playing) {
        // La lecture a cessé, mais pas forcément la question. Si le silence a
        // été CONSTATÉ pendant cette lecture, on continue de le dire un temps
        // borné : sans cela l'explication disparaissait à l'instant même de
        // l'arrêt — celui où l'utilisateur réagit à l'absence de son (#2588).
        return if ps
            .browser_unattended_at
            .is_some_and(|t| t.elapsed() < BROWSER_UNATTENDED_RETENTION)
        {
            "browser_unattended"
        } else {
            "ok"
        };
    }
    // `last_play_started_at` est `#[serde(skip)]` : après une restauration
    // d'état il vaut `None`, et on ne conclut rien. Le sens du défaut est le
    // bon — on préfère taire un silence réel que d'en inventer un.
    if !ps
        .last_play_started_at
        .is_some_and(|t| t.elapsed() >= BROWSER_UNATTENDED_GRACE)
    {
        return "ok";
    }
    if browser_stream_pulled {
        "ok"
    } else {
        "browser_unattended"
    }
}

pub fn build_signal_path_pub(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    wire: Option<&StreamInfo>,
) -> Option<Value> {
    build_signal_path(ps, zone, backend, renderer_label, audio_backend, wire)
}

/// #1395 — sur la ZONE, dire quel backend de sortie locale tourne vraiment,
/// lequel était demandé, et pourquoi ils diffèrent.
///
/// Le chemin du signal nomme déjà le backend ACTIF dans son étape Transport
/// (« ASIO (exclusive) », « WASAPI »…), et il dit vrai depuis #1414. Ce qui
/// manquait, c'est le terme de comparaison : Bilou règle « Ce PC / Hauts
/// Parleurs » sur ASIO, lit « WASAPI », et ne peut pas savoir si son réglage
/// n'a pas pris ou si le serveur a basculé. Le motif du basculement existait
/// — `local_audio_asio_no_devices` — mais seulement dans le journal ; il a
/// fallu qu'il en poste une capture pour que le fil avance.
///
/// `None` pour toute zone qui n'est pas une sortie locale : un renderer DLNA
/// ou Chromecast n'a rien à voir avec ASIO, et lui accrocher un motif de repli
/// serait exactement l'annonce fantôme que #2053 et #1315 ont déjà coûtée.
/// `None` aussi quand la sortie locale n'est pas compilée.
#[cfg(feature = "local-audio")]
pub fn local_backend_status_value(output_type: Option<&str>, requested: &str) -> Option<Value> {
    // Même convention que `build_signal_path` : une zone sans `output_type`
    // est une sortie locale.
    if output_type.unwrap_or("local") != "local" {
        return None;
    }
    serde_json::to_value(tune_core::outputs::local::active_backend_status(requested)).ok()
}

/// Variante sans sortie locale compilée : il n'y a aucun backend à décrire.
#[cfg(not(feature = "local-audio"))]
pub fn local_backend_status_value(_output_type: Option<&str>, _requested: &str) -> Option<Value> {
    None
}

/// Build the `signal_path` object for a zone's current playback.
/// Returns `None` when the zone is not playing.
///
/// `audio_backend` is the active audio backend name ("ASIO", "WASAPI",
/// "CoreAudio", "ALSA") used for local zones' signal path display.
///
/// `wire` décrit ce qui part RÉELLEMENT sur le fil pour la session en cours
/// (`AudioStreamer::stream_output_wire`) : conteneur, fréquence, profondeur.
/// `None` quand il n'y a pas de session vivante (sortie locale, avant démarrage).
///
/// C'est la source de vérité, et elle prime sur toute déduction. Cette fonction
/// rejouait les règles de l'orchestrateur pour deviner ce qui était servi ; à
/// chaque évolution du chemin audio il fallait répliquer la règle ici, et un
/// oubli faisait mentir l'affichage. Le renderer, lui, affiche ce qu'il reçoit
/// — d'où les écarts constatés par Yves sur darTZeel LHC-208 et Eversolo
/// DMP-A10, tous deux en passthrough natif.
/// Is a WAV/LPCM wire feed to a DLNA/OpenHome renderer bit-perfect?
///
/// Three cases share the WAV wire: a native WAV source (passthrough), the
/// zone forcing 16-bit LPCM (`dlna_lpcm`), or a FLAC/ALAC source that the
/// orchestrator fell back to WAV for. A **native WAV** source is sent
/// byte-for-byte at any bit depth, so it is always bit-perfect. The FLAC/ALAC→WAV
/// fallback is plain 16-bit LPCM unless `dlna_wav24` preserves the full 24 bits,
/// so it is bit-perfect only when the source already fits 16 bits or the 24-bit
/// override is on.
/// L'égaliseur de cette zone modifie-t-il réellement le signal ?
///
/// Miroir exact de `Orchestrator::load_eq_processor` : mode PURE d'abord — il
/// court-circuite tout traitement, donc un profil enregistré n'y change rien —
/// puis profil activé ET gains audibles. Sans ce miroir, l'indicateur
/// bit-perfect et le chemin audio répondraient à deux questions différentes.
fn active_zone_eq_profile(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<tune_core::audio::eq::EqProfile> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    // PURE : le PCM atteint la sortie intact, l'égaliseur n'est jamais construit.
    if tune_core::audio::audiophile::zone_enabled(backend, zone_id) {
        return None;
    }
    let profile = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<tune_core::audio::eq::EqProfile>(&s).ok())?;
    if !profile.enabled {
        return None;
    }
    // 44100/2 n'est qu'une sonde : is_enabled() dépend des gains, pas du débit.
    tune_core::audio::eq::EqProcessor::new(&profile, 44100, 2)
        .is_enabled()
        .then_some(profile)
}

fn zone_eq_alters_signal(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> bool {
    active_zone_eq_profile(backend, zone_id).is_some()
}

/// Description du traitement EQ réellement configuré, y compris le headroom
/// automatique. Le limiteur est nommé comme absent : le pré-gain réserve la
/// marge des boosts, il ne faut plus confondre l'EQ avec une protection de crête.
fn zone_eq_step_description(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<String> {
    let profile = active_zone_eq_profile(backend, zone_id)?;
    let left = profile.automatic_headroom_db(0);
    let right = profile.automatic_headroom_db(1);
    if (left - right).abs() < 0.01 {
        Some(format!(
            "EQ actif (pré-gain auto {left:.1} dB, sans limiteur)"
        ))
    } else {
        Some(format!(
            "EQ actif (pré-gain auto G {left:.1} dB / D {right:.1} dB, sans limiteur)"
        ))
    }
}

/// Le ReplayGain modifie-t-il réellement le signal de cette zone — et comment ?
///
/// Miroir de `Orchestrator::zone_replaygain_changes_audio`, pour la même
/// raison que `zone_eq_alters_signal` : sans lui, le panneau annoncerait
/// « Bit-Perfect » pendant qu'un gain multiplie chaque échantillon (même
/// famille d'écart que l'EQ ignoré du verdict, #1548/#1559 — ici #1627).
/// Mode PURE d'abord : le gain n'y est jamais appliqué, donc jamais d'étape.
/// Ensuite le facteur EFFECTIF (tags + pré-ampli + anti-écrêtage) : un mode
/// « track » sans tag stocké ne change rien au signal et n'affiche donc rien.
///
/// Retourne la description de l'étape (« ReplayGain (track, -4.2 dB, tags du
/// fichier) ») quand le gain s'applique, `None` sinon. La granularité affichée
/// est celle qui a FOURNI la valeur : en mode album sans tags d'album, c'est le
/// gain de piste qui joue, et c'est lui qu'on nomme.
///
/// La PROVENANCE est le reste de #1627 : le panneau disait ce qui s'applique et
/// de combien, jamais d'où ça vient. « Tune utilise-t-il mes tags rsgain ? »
/// (#1382) se répondait alors partout sauf à l'endroit où la question se pose.
fn zone_replaygain_step(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    track_id: Option<i64>,
) -> Option<ReplayGainStep> {
    use tune_core::audio::replaygain::{
        GainSource, ReplayGainSettings, gain_factor, stored_gain_detail, stored_gain_source,
    };
    // PURE : le PCM atteint la sortie intact, le gain n'est jamais appliqué.
    if tune_core::audio::audiophile::zone_enabled(backend, zone_id) {
        return None;
    }
    let tid = track_id?;
    let settings = ReplayGainSettings::load(backend);
    let (gain, source) = stored_gain_detail(backend, tid, settings.mode)?;
    let factor = gain_factor(gain, settings);
    // Même seuil que l'orchestrateur (`zone_replaygain_changes_audio`).
    if (factor - 1.0).abs() <= 1e-6 {
        return None;
    }
    // Le dB affiché est celui qui multiplie réellement les échantillons
    // (pré-ampli et anti-écrêtage compris), pas le tag brut.
    let applied_db = 20.0 * factor.log10();
    let label = match source {
        tune_core::audio::replaygain::ReplayGainMode::Album => "album",
        _ => "track",
    };
    // La provenance porte sur la granularité qui a fourni la valeur, pas sur
    // le mode demandé. Une base illisible ne doit rien inventer : on retombe
    // sur la description d'avant, sans mention d'origine.
    let origin = stored_gain_source(backend, tid, source);
    let description = match origin {
        Some(src) => format!(
            "ReplayGain ({label}, {applied_db:+.1} dB, {})",
            src.label_fr()
        ),
        None => format!("ReplayGain ({label}, {applied_db:+.1} dB)"),
    };
    Some(ReplayGainStep {
        description,
        granularity: label,
        source: origin.map(GainSource::as_str),
    })
}

/// L'étape ReplayGain du chemin du signal, description ET faits bruts.
///
/// Les deux champs structurés sont ADDITIFS : le client qui ne lit que
/// `description` continue de fonctionner à l'identique.
struct ReplayGainStep {
    description: String,
    /// `"track"` ou `"album"` — celle qui a fourni la valeur.
    granularity: &'static str,
    /// `"file_tags"` ou `"analysis"`, absent si la base n'a pas répondu.
    source: Option<&'static str>,
}

/// La zone replie-t-elle sa sortie LOCALE en mono — et si oui, que dire ?
///
/// Miroir exact de ce que l'orchestrateur pousse à la sortie locale
/// (`PlaybackOrchestrator::zone_mono_downmix_with`, PURE compris), et restreint aux
/// sorties locales : c'est le seul chemin où le repli est appliqué, et une
/// étape affichée sur une zone DLNA décrirait un traitement qui n'a pas lieu.
///
/// Sans ce miroir, le panneau annoncerait un chemin intouché pendant que chaque
/// échantillon est réécrit — la faute exacte de #1548/#1559 (égaliseur oublié
/// du verdict) et de #1627 (ReplayGain). Ici la transformation est réelle et
/// doit APPARAÎTRE : #2825 vient de corriger le cas inverse, où le volume
/// logiciel prétendait à tort dégrader.
fn zone_mono_downmix_step(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    output_type: &str,
) -> Option<String> {
    if output_type != "local" {
        return None;
    }
    tune_core::orchestrator::PlaybackOrchestrator::zone_mono_downmix_with(backend, zone_id)
        .then(|| "Sortie mono : (G + D) / 2 sur les deux voies".to_string())
}

/// La famille DSD que désigne une cadence brute (2,8 MHz → DSD64, etc.).
///
/// Une seule table pour la ligne Source et pour l'étage de sortie : elles
/// nommaient la même chose à deux endroits, et rien ne garantissait qu'elles
/// disent la même chose.
fn dsd_family_name(sample_rate: i32) -> &'static str {
    match sample_rate {
        r if r >= 22_000_000 => "DSD512",
        r if r >= 11_000_000 => "DSD256",
        r if r >= 5_000_000 => "DSD128",
        _ => "DSD64",
    }
}

/// « DSD128 5.6 MHz » — un DSD se dit par sa famille et sa cadence en MHz,
/// jamais en kHz/bit.
fn dsd_resolution_label(sample_rate: i32) -> String {
    format!(
        "{name} {mhz:.1} MHz",
        name = dsd_family_name(sample_rate),
        mhz = sample_rate as f64 / 1_000_000.0
    )
}

/// Ces chiffres décrivent-ils du DSD ? 1 bit, ou une cadence en MHz.
///
/// Aucun PCM n'atteint le mégahertz (768 kHz est le maximum du marché) et
/// aucun conteneur PCM ne porte 1 bit : les deux tests sont sans recouvrement
/// possible avec du PCM légitime.
fn is_dsd_resolution(sample_rate: i32, bit_depth: i32) -> bool {
    // `== 1` et non `<= 1` : une profondeur de 0 est une valeur MANQUANTE, pas
    // du DSD, et la traiter comme telle inventerait un « DSD64 0.0 MHz ».
    bit_depth == 1 || sample_rate >= 1_000_000
}

/// Libellé d'un étage de SORTIE — le garde-fou structurel du #1315.
///
/// « FLAC 5644kHz/1bit » est un libellé IMPOSSIBLE : aucun FLAC ne transporte
/// du 1 bit à 5,6 MHz. Il s'affichait pourtant sur l'Eversolo DMP-A6 de
/// Stéphane Villerio, parce que les deux moitiés de la ligne n'ont pas la même
/// origine — le nom du conteneur est DEVINÉ (`dlna_transcode_target`, une
/// cible statique), les chiffres viennent du FIL, c'est-à-dire de ce qui part
/// vraiment.
///
/// Quand les deux se contredisent, ce sont les chiffres qui gagnent : c'est
/// déjà la règle du reste de `build_signal_path` (« le fil prime »), et c'est
/// la seule moitié qui soit une mesure. Le libellé impossible ne peut donc
/// plus sortir d'ici, quelle que soit la cible qu'on lui passe.
fn output_stage_label(container: &str, sample_rate: i32, bit_depth: i32) -> String {
    if is_dsd_resolution(sample_rate, bit_depth) {
        if !container.starts_with("DSD") {
            tracing::warn!(
                container,
                sample_rate,
                bit_depth,
                "signal_path_libelle_impossible_ecarte — une résolution DSD \
                 annoncée sous un conteneur PCM ; le fil tranche (#1315)"
            );
        }
        return dsd_resolution_label(sample_rate);
    }
    if sample_rate >= 1000 {
        format!(
            "{container} {sr}kHz/{bit_depth}bit",
            sr = sample_rate / 1000
        )
    } else {
        format!("{container} {sample_rate}Hz/{bit_depth}bit")
    }
}

/// Le fil porte-t-il du DSD BRUT — le fichier .dsf/.dff tel quel ?
///
/// Miroir du `dsd_passthrough` de l'orchestrateur, et il manquait. `zones.rs`
/// mire l'ALAC et l'AAC ; le commentaire du miroir ALAC dit lui-même qu'il a
/// été ajouté pour tuer une étape fantôme « ALAC→FLAC » (#1131). Le même
/// fantôme existait pour le DSD : `needs_transcode_for_output` restait vrai et
/// le panneau annonçait une conversion vers FLAC pendant que l'orchestrateur
/// envoyait le .dsf brut (#1315, Yves Corbat / Stéphane Villerio, DMP-A6).
///
/// La décision elle-même (`should_dsd_passthrough`) dépend d'un sondage SOAP
/// asynchrone que ce constructeur synchrone ne peut pas rejouer — et la
/// rejouer serait un septième miroir à maintenir. On lit donc ce que la
/// session sert VRAIMENT : le passthrough crée sa session avec l'extension
/// source et un MIME DSD (`orchestrator.rs`, branche « Standard passthrough:
/// serve the raw file »), là où toutes les autres branches produisent du
/// `wav`/`flac`. C'est le même principe que `wire_wav`, et il est plus fort
/// qu'un miroir : il constate au lieu de deviner.
fn wire_carries_raw_dsd(wire: Option<&StreamInfo>) -> bool {
    wire.is_some_and(|w| {
        tune_core::orchestrator::est_source_dsd(Some(&w.format))
            || tune_core::orchestrator::est_dsd_brut(&w.mime_type)
    })
}

fn wav_wire_bit_perfect(
    is_lossless: bool,
    source_is_wav: bool,
    dlna_wav24: bool,
    bit_depth: i32,
) -> bool {
    is_lossless && (source_is_wav || dlna_wav24 || bit_depth <= 16)
}

/// Le fil est-il intact, du point de vue du VERDICT affiché ?
///
/// La sonde Windows (#2205/#2233) est autoritaire sur ce qui a atteint le ring,
/// et son `bit_perfect` vaut `reasons.is_empty()` : le volume logiciel y figure
/// au même rang que le DSP ou le transport flottant. Or `build_signal_path`
/// applique depuis #1627 la règle inverse — « Volume is excluded, it's a user
/// preference, not a signal degradation » — et l'applique encore, deux cents
/// lignes plus bas, à toutes les autres sorties et à toutes les autres
/// plateformes (macOS, Linux et le navigateur ne publient aucune sonde, donc
/// `unwrap_or(true)`).
///
/// Conséquence vécue (#2053) : sous Windows, descendre le curseur à 85 % sur un
/// FLAC sans égaliseur, sans ReplayGain et sans plafond de fréquence suffisait à
/// faire tomber le verdict — et le client n'a qu'un seul mot pour dire « pas
/// bit-perfect » : **« Transcodé »**. Un testeur qui n'a touché que son volume
/// lisait donc qu'on transcodait sa musique.
///
/// On ne relève JAMAIS le verdict du producteur en promesse de pureté : seul le
/// cas où le volume est la SEULE cause est neutralisé. Une raison de plus (DSP,
/// transport flottant, état indéterminé) et le verdict reste négatif. La cause
/// n'est pas effacée pour autant : elle reste dans `runtime_reasons` et dans le
/// détail de l'étape Transport.
fn runtime_transport_is_intact(status: &OutputSignalPathStatus) -> bool {
    status.bit_perfect
        || (!status.reasons.is_empty()
            && status
                .reasons
                .iter()
                .all(|reason| matches!(reason, OutputSignalReason::SoftwareVolume)))
}

fn runtime_signal_reason_detail(status: &OutputSignalPathStatus) -> Option<String> {
    let details: Vec<&str> = status
        .reasons
        .iter()
        .map(|reason| match reason {
            OutputSignalReason::FloatTransport => "Transport flottant imposé par le callback",
            OutputSignalReason::DspApplied => "DSP appliqué",
            OutputSignalReason::DspStateUnknown => "État DSP indéterminé",
            OutputSignalReason::SoftwareVolume => "Volume logiciel appliqué",
        })
        .collect();
    (!details.is_empty()).then(|| details.join(" ; "))
}

fn build_signal_path(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    wire: Option<&StreamInfo>,
) -> Option<Value> {
    if ps.state == PlayState::Stopped {
        return None;
    }

    let np = ps.now_playing.as_ref()?;

    // Conteneur réellement servi (None hors session : sortie locale, démarrage).
    let output_container = wire.map(|w| w.format.as_str());
    // Fréquence et profondeur réellement émises. Une session fraîchement créée
    // peut encore porter des zéros (`StreamInfo::default`) : on ne retient que
    // des valeurs renseignées, sans quoi l'affichage annoncerait « 0kHz/0bit ».
    let wire_sample_rate = wire.map(|w| w.sample_rate).filter(|v| *v > 0);
    let wire_bit_depth = wire.map(|w| w.bit_depth).filter(|v| *v > 0);
    // A decoded live radio has no library row and its NowPlaying resolution is
    // only the bootstrap value chosen before the decoder opens the upstream.
    // Once the session publishes its detected PCM format, that observation is
    // authoritative for the source line too (France Musique: 48 kHz, not the
    // 44.1 kHz bootstrap value from session creation — #2427).
    let radio_wire_sample_rate = (np.source == "radio")
        .then_some(wire_sample_rate)
        .flatten()
        .map(|v| v as i32);
    let radio_wire_bit_depth = (np.source == "radio")
        .then_some(wire_bit_depth)
        .flatten()
        .map(|v| v as i32);

    // Look up track details for format/sample_rate/bit_depth
    let track = np.track_id.and_then(|tid| {
        TrackRepo::with_backend(backend.clone())
            .get(tid)
            .ok()
            .flatten()
    });

    let fmt_str = np
        .format
        .clone()
        .or_else(|| track.as_ref().and_then(|t| t.format.clone()))
        .unwrap_or_else(|| "flac".into());
    let source_format = AudioFormat::from_extension(&fmt_str);
    let is_dsd = matches!(fmt_str.as_str(), "dsd" | "dsf" | "dff");
    // For DSD files, prefer the track's original sample rate and bit depth
    // from the database (which represent the SOURCE format: e.g. 2822400 Hz
    // / 1-bit for DSD64) over the NowPlaying values, which may contain the
    // TRANSCODED PCM values (e.g. 176400 Hz / 24-bit) when the file was
    // converted for network output (DLNA, OpenHome, etc.).
    let sample_rate = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.sample_rate)
            .or_else(|| np.sample_rate.map(|v| v as i32))
            .unwrap_or(2_822_400)
    } else {
        radio_wire_sample_rate
            .or_else(|| np.sample_rate.map(|v| v as i32))
            .or_else(|| track.as_ref().and_then(|t| t.sample_rate))
            // Dernier recours quand ni la lecture en cours ni la base ne
            // savent : le fil, qui décrit ce qui part vraiment. Sans lui on
            // affichait 44100 en dur — une valeur inventée, affirmée avec le
            // même aplomb qu'une vraie mesure, et fausse dès que le fichier
            // était en Hi-Res (métadonnées non lues au scan).
            .or_else(|| wire_sample_rate.map(|v| v as i32))
            .unwrap_or(44100)
    };
    let bit_depth = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.bit_depth)
            .or_else(|| np.bit_depth.map(|v| v as i32))
            .unwrap_or(1)
    } else {
        radio_wire_bit_depth
            .or_else(|| np.bit_depth.map(|v| v as i32))
            .or_else(|| track.as_ref().and_then(|t| t.bit_depth))
            .or_else(|| wire_bit_depth.map(|v| v as i32))
            .unwrap_or(16)
    };

    let format_name = if is_dsd {
        dsd_family_name(sample_rate)
    } else if let Some(f) = source_format.as_ref() {
        f.display_name()
    } else {
        // A UPnP/NAS media-server source reports its codec as a MIME type or DLNA
        // profile (e.g. "audio/mp4", "AAC_ISO_320"), not a file extension, so
        // from_extension() returned None and the signal path showed "Unknown"
        // (Yves: NAS as source). Recognize the codec from the raw string instead.
        let l = fmt_str.to_lowercase();
        let is_m4a = l.contains("mp4") || l.contains("m4a") || l.contains("aac");
        if l.contains("alac") || (is_m4a && bit_depth >= 24) {
            // audio/mp4 (M4A) is ambiguous ALAC vs AAC — same container/MIME. A
            // DIDL res@bitsPerSample >= 24 means lossless ALAC, not lossy AAC
            // (Yves: NAS ALAC read 24-bit by the DartZeel but shown as AAC here).
            "ALAC"
        } else if is_m4a {
            "AAC"
        } else if l.contains("mp3") || l.contains("mpeg") {
            "MP3"
        } else if l.contains("flac") {
            "FLAC"
        } else if l.contains("wav") {
            "WAV"
        } else if l.contains("ogg") || l.contains("vorbis") {
            "OGG"
        } else if l.contains("opus") {
            "OPUS"
        } else {
            "Unknown"
        }
    };
    // For a media-server source (no from_extension AudioFormat) the lossless
    // verdict follows the recognized codec name, so a 24-bit ALAC is no longer
    // shown "Avec perte" (Yves).
    let is_lossless = source_format
        .as_ref()
        .map(|f| f.is_lossless())
        .unwrap_or_else(|| matches!(format_name, "ALAC" | "FLAC" | "WAV"));

    let output_type = zone.output_type.as_deref().unwrap_or("local");
    // Pour une sortie locale qui sait observer son dernier callback, le réel
    // prime sur toute déduction depuis les réglages. Les autres sorties
    // conservent le calcul historique jusqu'à ce qu'elles publient leur propre
    // sonde via le contrat additif d'OutputTarget.
    let runtime_signal_path = (output_type == "local")
        .then(|| ps.output_signal_path.as_ref())
        .flatten();

    // Determine if DSP is active.
    //
    // Deux sources, et il faut les DEUX : la colonne dsp_preset_id/dsp_enabled
    // de la zone, et le profil d'égaliseur `zone_{id}_eq_profile`. C'est ce
    // dernier qu'écrit le panneau EQ de « Lecture en cours » et que lit le
    // chemin audio (`Orchestrator::zone_has_active_eq`) — l'indicateur ne le
    // consultait pas.
    //
    // Conséquence : Tune pouvait afficher « Bit-Perfect » alors qu'un
    // égaliseur modifiait réellement le signal. Pour un logiciel dont c'est
    // l'argument central, promettre une pureté qu'on ne tient pas est le pire
    // des deux sens possibles de l'erreur (signalement Bilou).
    let zid = zone.id.unwrap_or(0);
    let configured_dsp_enabled = ZoneRepo::with_backend(backend.clone())
        .get_dsp_config(zid)
        .map(|(preset_id, enabled)| enabled && preset_id.is_some())
        .unwrap_or(false)
        || zone_eq_alters_signal(&backend, zid);
    let dsp_enabled = runtime_signal_path
        .map(|status| status.dsp == OutputDspState::Applied)
        .unwrap_or(configured_dsp_enabled);
    let eq_step_description = zone_eq_step_description(&backend, zid);

    // ReplayGain effectivement appliqué à la piste en cours (#1627) : même
    // traitement que l'EQ — une étape dans le chemin, et le verdict bit-perfect
    // en tient compte. `None` en PURE, en mode off, ou sans gain stocké.
    let replaygain_step = zone_replaygain_step(&backend, zid, np.track_id);

    // Sortie mono (#2362) : sortie locale seulement, jamais en PURE. C'est une
    // vraie transformation — elle réécrit chaque échantillon — donc elle porte
    // une étape et fait tomber le verdict bit-perfect, comme le ReplayGain.
    let mono_downmix_step = zone_mono_downmix_step(&backend, zid, output_type);

    // Volume at 100% means no software volume adjustment.
    // Fixed-volume zones always output at full volume (bit-perfect).
    //
    // La valeur affichée est `zone.volume` (la base), PAS `ps.volume` : c'est
    // elle que la page expose comme curseur (GET /zones/{id}). `ps.volume` est
    // une copie mémoire qui ment dans deux cas : jamais initialisée depuis la
    // base au démarrage (0,5 par défaut pour une zone locale/navigateur —
    // seules les zones réseau sont resemées à la découverte), et modifiée par
    // les régleurs internes (alarmes, minuterie de sommeil, IA) qui n'écrivaient
    // pas la base. Résultat : le panneau bit-perfect affichait « Volume 20 % »
    // face à un curseur ailleurs, jusqu'à ce qu'on touche le volume — le PUT
    // réécrit alors les deux sources (#1504 Jean Valjean, même symptôme
    // Bebelalu55 #1480). Une seule source pour les deux affichages.
    let ui_volume = (zone.volume / 100.0).clamp(0.0, 1.0);
    let volume_full = zone.fixed_volume || ui_volume >= 1.0 || ui_volume <= 0.0; // 0.0 means no software vol set

    // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC) for network outputs.
    // FLAC, WAV, MP3, AAC are natively supported and pass through without transcoding.
    let is_network_output = matches!(
        output_type,
        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
    );
    // Passthrough DSD natif : l'orchestrateur sert le .dsf/.dff brut au
    // renderer (`orchestrator.rs` `dsd_passthrough`). Constaté sur le fil, pas
    // deviné — cf. `wire_carries_raw_dsd`. Sans ce miroir, une piste DSD128
    // envoyée telle quelle à un Eversolo DMP-A6 s'affichait « DSD128 5.6 MHz →
    // FLAC 5644kHz/1bit » : un transcodage qui n'a pas lieu, vers un conteneur
    // qui ne peut pas exister (#1315).
    let dsd_passthrough = is_dsd && is_network_output && wire_carries_raw_dsd(wire);
    // Un égaliseur ARMÉ n'atteint pas un flux DSD servi BRUT hors sortie
    // locale, et l'orchestrateur s'en abstient DÉLIBÉRÉMENT : convertir du DSD
    // natif en PCM pour y passer un EQ serait une dégradation décidée à la
    // place de l'auditeur. Les deux gardes sont explicites côté audio —
    // `pull_output_needs_dsp_transcode` rend `false` sur `AudioFormat::Dsd`,
    // et `eq_forces_transcode` est gardé par `!dsd_passthrough`.
    //
    // Le panneau, lui, annonçait l'étape DSP sur la seule foi du RÉGLAGE en
    // base (`configured_dsp_enabled`) : un traitement qui n'a pas lieu, plus
    // un verdict bit-perfect qu'il faisait tomber alors que le fil est intact.
    // C'est la faute de #1315 et #2053 — ne pas annoncer ce qui n'a pas lieu —
    // et c'est le versant visible du signalement d'Eric (#1393, renderer
    // Diretta et PC vu comme zone DLNA) : des réglages sans effet, et rien qui
    // le dise.
    //
    // `is_network_output` n'est PAS la bonne borne : une sortie PULL hors
    // dépôt (`diretta`) va chercher le .dsf elle-même sans être « réseau » au
    // sens de ce fichier, et c'est justement la zone du signalement. Le fil
    // est CONSTATÉ (`wire_carries_raw_dsd`), pas déduit.
    //
    // La sortie LOCALE est exclue : elle a sa sonde d'exécution, qui dit déjà
    // « DSP contourné pour DoP » quand c'est le cas, et qui est plus juste que
    // toute déduction faite ici.
    let dsd_brut_hors_sortie_locale =
        is_dsd && output_type != "local" && wire_carries_raw_dsd(wire);
    // « Armé » et « appliqué » ne sont pas la même chose.
    let dsp_applique = dsp_enabled && !dsd_brut_hors_sortie_locale;
    let dsp_contourne_par_le_dsd = dsp_enabled && dsd_brut_hors_sortie_locale;
    // ALAC native passthrough (opt-in per zone): the orchestrator serves the ALAC
    // file straight to a renderer that decodes it (bit-perfect, no FLAC transcode).
    // Mirror the orchestrator's condition (see orchestrator.rs `alac_passthrough`)
    // so the signal path does not show a phantom ALAC→FLAC transcode step when the
    // wire is really ALAC (forum #1131: DartZeel DAC displays ALAC at the right
    // resolution, yet the signal path claimed an ALAC→FLAC transcode).
    // A zone forced to serve WAV/LPCM (`dlna_lpcm`) always transcodes, so it takes
    // precedence over ALAC passthrough — matching the orchestrator.
    let zone_id = zone.id.unwrap_or(0);
    // `!dsd_passthrough` : même précédence que l'orchestrateur, où le forçage
    // WAV ne peut pas s'appliquer à un flux DSD servi brut (`dlna_needs_wav`
    // exige `will_be_flac`, faux dès que `needs_transcode_for_output` tombe).
    // Sans cette garde, une zone cochée « LPCM » annoncerait du WAV sur un fil
    // qui porte du DSD.
    let dlna_lpcm = is_network_output
        && !dsd_passthrough
        && ZoneRepo::with_backend(backend.clone()).get_dlna_lpcm(zone_id);
    // Zone opt-in 16-bit cap (Ruark R3, #1137): mirrors the orchestrator so the
    // signal path shows a real 16-bit downconvert instead of a phantom
    // bit-perfect passthrough when the source is hi-res.
    let dlna_cap_16bit = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_cap_16bit(zone_id);
    // Zone opt-in: serve genuine 24-bit WAV (audio/L24) instead of the 16-bit
    // LPCM fallback. Mirrors orchestrator.rs `dlna_wav24` so the signal path
    // shows a lossless 24-bit WAV wire (not a phantom 16-bit truncation).
    let dlna_wav24 = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_wav24(zone_id);
    // Même règle que l'orchestrateur, par la MÊME fonction : sur une source
    // FLAC dont la zone demande le FLAC natif, le forçage WAV ne s'applique pas
    // — il vise le décodeur ALAC du renderer. Sans ce miroir, le chemin du
    // signal annoncerait un transcodage vers WAV là où le fil porte du FLAC,
    // c'est-à-dire exactement le genre d'affichage inventé que ce dépôt traque.
    let source_is_flac = source_format == Some(AudioFormat::Flac);
    let native_flac_opt_in =
        is_network_output && ZoneRepo::with_backend(backend.clone()).get_dlna_native_flac(zone_id);
    let dlna_lpcm = tune_core::orchestrator::wav_override_applies(
        dlna_lpcm,
        source_is_flac,
        native_flac_opt_in,
    );
    let dlna_wav24 = tune_core::orchestrator::wav_override_applies(
        dlna_wav24,
        source_is_flac,
        native_flac_opt_in,
    );
    let alac_passthrough = source_format == Some(AudioFormat::Alac)
        && is_network_output
        && !dlna_lpcm
        && !dlna_wav24
        && !dlna_cap_16bit
        && ZoneRepo::with_backend(backend.clone()).get_alac_passthrough(zone_id);
    // Miroir de la condition AAC de l'orchestrateur (voir orchestrator.rs).
    let aac_passthrough = source_format == Some(AudioFormat::Aac)
        && is_network_output
        && !dlna_lpcm
        && !dlna_wav24
        && ZoneRepo::with_backend(backend.clone()).get_aac_passthrough(zone_id);
    let needs_transcode_for_output = is_network_output
        && !dsd_passthrough
        && !alac_passthrough
        && !aac_passthrough
        && source_format
            .as_ref()
            .is_some_and(|f| f.needs_transcode_for_dlna());
    // OAAT transcodes everything to WAV except WAV itself
    let is_oaat = output_type == "oaat";
    let oaat_transcodes = is_oaat
        && source_format
            .as_ref()
            .is_some_and(|f| *f != AudioFormat::Wav);

    // The renderer may be served WAV/LPCM even for a FLAC/ALAC source when it
    // does not advertise `audio/flac` (`orchestrator::dlna_needs_wav`, decided
    // by async SOAP negotiation this synchronous builder cannot replay). Trust
    // the live session's real container over the static transcode-target guess
    // so the path shows "ALAC → WAV" instead of a phantom "ALAC → FLAC" (Sevy,
    // LHC-52). Only "wav" changes the verdict; anything else keeps prior logic.
    let wire_wav = output_container.is_some_and(|c| c.eq_ignore_ascii_case("wav"));

    let (transport_bit_perfect, transport_desc, output_format_name) = match output_type {
        "dlna" | "openhome" => {
            if wire_wav || dlna_lpcm || dlna_wav24 {
                // Renderer served WAV/LPCM, not FLAC — the signal path must say
                // so (a renderer showing "WAV/PCM" otherwise contradicted Tune's
                // "→ FLAC" label, LHC). Three causes, same wire: the zone forces
                // 16-bit LPCM (`dlna_lpcm`) or genuine 24-bit WAV (`dlna_wav24`),
                // or the renderer doesn't advertise `audio/flac` and the
                // orchestrator fell back to WAV (`dlna_needs_wav`) — detected here
                // from the live session's real container (`wire_wav`), which the
                // synchronous builder cannot renegotiate. The plain LPCM fallback
                // is 16-bit (audio/L16), bit-perfect only when the lossless source
                // already fits 16 bits (Sevy, #1137). The opt-in `dlna_wav24` path
                // preserves the full 24-bit source, so it stays bit-perfect.
                // A *native* WAV source is served byte-for-byte (WAV never
                // transcodes for DLNA), so it is bit-perfect at any depth
                // regardless of `dlna_wav24` — which only governs the FLAC/ALAC→WAV
                // fallback (Sandro/Progman: WAV 24-bit direct showed red without it).
                let wav_bit_perfect = wav_wire_bit_perfect(
                    is_lossless,
                    matches!(source_format, Some(AudioFormat::Wav)),
                    dlna_wav24,
                    bit_depth,
                );
                (wav_bit_perfect, "DLNA/UPnP", "WAV")
            } else if needs_transcode_for_output || dlna_cap_16bit {
                // Cap forces a 16-bit FLAC downconvert (not bit-perfect) even for
                // an otherwise-direct FLAC source (Ruark R3, #1137).
                let target = source_format
                    .map(|f| f.dlna_transcode_target())
                    .unwrap_or(AudioFormat::Flac);
                (false, "DLNA/UPnP", target.display_name())
            } else {
                // FLAC, WAV, MP3, AAC → passthrough (bit-perfect for lossless)
                (true, "DLNA/UPnP", format_name)
            }
        }
        "oaat" => {
            // Lossless PCM → WAV preserves every bit, but DSD → WAV is a domain
            // conversion (1-bit sigma-delta decimated to multi-bit PCM), so it is
            // NOT bit-perfect even though DSD counts as a lossless *format*.
            (
                (is_lossless && !is_dsd) || !oaat_transcodes,
                "OAAT",
                if oaat_transcodes { "WAV" } else { format_name },
            )
        }
        "airplay" => (false, "AirPlay", "ALAC"),
        "chromecast" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "Chromecast", target.display_name())
            } else {
                (false, "Chromecast", format_name)
            }
        }
        "bluos" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "BluOS", target.display_name())
            } else {
                (true, "BluOS", format_name)
            }
        }
        "squeezebox" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "Squeezebox", target.display_name())
            } else {
                (true, "Squeezebox", format_name)
            }
        }
        "browser" => (true, "Browser", format_name),
        "local" => {
            // Show the actual audio backend (ASIO / WASAPI / CoreAudio / ALSA)
            let transport = match audio_backend {
                "ASIO" => "ASIO (exclusive)",
                "WASAPI" => "WASAPI",
                "CoreAudio" => "CoreAudio",
                "ALSA" => "ALSA",
                other => other,
            };
            (
                runtime_signal_path
                    .map(runtime_transport_is_intact)
                    .unwrap_or(true),
                transport,
                format_name,
            )
        }
        other => (false, other, format_name),
    };

    // Detect sample rate capping (DSD excluded — the DSD→PCM transcode
    // already handles rate conversion; showing a separate resampler step
    // would be misleading since sample_rate here is the DSD MHz rate).
    let resampling_active = !is_dsd
        && zone
            .max_sample_rate
            .is_some_and(|max| (sample_rate as u32) > max);

    // Overall bit-perfect: lossless source + no transcoding + no DSP + no
    // resampling + no ReplayGain. Volume is excluded — it's a user preference,
    // not a signal degradation. ReplayGain, lui, multiplie chaque échantillon :
    // l'orchestrateur le traite déjà comme l'EQ (`zone_replaygain_changes_audio`
    // force le chemin transcodé), le verdict doit dire la même chose (#1627).
    // + le repli mono (#2362) : sommer les deux voies et les réémettre
    // identiques réécrit chaque échantillon. Une zone qui l'active n'est PAS
    // bit-perfect, et le panneau doit le dire — c'est exactement la promesse
    // que #1548/#1559 (EQ) et #1627 (ReplayGain) avaient laissé mentir.
    // `dsp_applique`, et non `dsp_enabled` : un EQ armé qu'un flux DSD brut
    // met hors de portée ne touche AUCUN échantillon. Le faire tomber le
    // verdict serait mentir dans l'autre sens (#1393).
    let bit_perfect = is_lossless
        && transport_bit_perfect
        && !dsp_applique
        && !resampling_active
        && replaygain_step.is_none()
        && mono_downmix_step.is_none();

    // Débit de la SOURCE, annoncé seulement quand elle le nomme elle-même.
    //
    // C'est le message que voit l'utilisateur pour le mp3-128 de Bandcamp
    // (#2074). La règle écrite dans le plugin — « un flux à 128 kbit/s doit
    // être annoncé comme tel PARTOUT où il apparaît »
    // (`plugins/tune-bandcamp/src/lib.rs`) — n'était tenue que sur l'écran
    // Bandcamp. Passée en zone, la même piste s'affichait « MP3 44kHz/16bit »,
    // exactement comme un 320 : le seul public de ce logiciel est celui qui
    // règle sa chaîne au bit près, et c'est précisément à lui que la
    // différence était cachée.
    //
    // Filtré sur le verdict avec perte : un débit sur un FLAC n'aurait aucun
    // sens, et un album Bandcamp ACHETÉ en lossless ne doit surtout pas
    // hériter du chiffre de l'extrait.
    let bitrate_label = np
        .bitrate_kbps
        .filter(|kbps| *kbps > 0 && !is_lossless)
        .map(|kbps| format!(" {kbps} kbit/s"))
        .unwrap_or_default();

    // Build steps
    let source_desc = if is_dsd {
        // DSD rates are in MHz range — display as e.g. "DSD64 2.8 MHz" or "DSD128 5.6 MHz"
        dsd_resolution_label(sample_rate)
    } else if sample_rate >= 1000 {
        format!(
            "{format_name}{bitrate_label} {sr}kHz/{bit_depth}bit",
            sr = sample_rate / 1000
        )
    } else {
        format!("{format_name}{bitrate_label} {sample_rate}Hz/{bit_depth}bit")
    };

    let mut steps = vec![json!({
        "name": "Source",
        "description": source_desc,
        "bit_perfect": true,
    })];

    // Decoder step. Skipped for DSD: the Source already reads e.g.
    // "DSD64 2.8 MHz" and the DSD→PCM/FLAC conversion is shown by the Transcoder
    // step, so a bare "DSD64" decoder line was just a confusing duplicate.
    if !is_dsd {
        steps.push(json!({
            "name": "Decoder",
            "description": format_name,
            "bit_perfect": is_lossless,
        }));
    }

    // Transcoding step (only if transcoding occurs). Include the zone-forced
    // WAV/LPCM (dlna_lpcm), 16-bit-cap (dlna_cap_16bit) and async WAV-fallback
    // (wire_wav) paths: all re-encode the stream, so the step must appear even
    // when the source format itself wouldn't need transcoding for DLNA (a FLAC
    // source with LPCM/cap-16, or an ALAC/FLAC source to a renderer that fell
    // back to WAV) — otherwise the path claimed a bit-perfect passthrough that
    // isn't happening (LHC: renderer shows WAV 16/44 while Tune showed ALAC→FLAC;
    // Sevy: LHC-52 served WAV while Tune showed ALAC→FLAC).
    let wire_transcode = wire_wav && !matches!(format_name, "WAV");
    let transcode_active = needs_transcode_for_output
        || oaat_transcodes
        || output_type == "airplay"
        || dlna_lpcm
        || dlna_wav24
        || dlna_cap_16bit
        || wire_transcode;
    if transcode_active {
        // OAAT lossless PCM → WAV preserves all audio data, but DSD → WAV is a
        // lossy domain conversion (see the "oaat" transport arm above). A DLNA
        // WAV/LPCM output likewise preserves the samples only when the source
        // already fits the 16-bit LPCM cap — unless the zone opted into genuine
        // 24-bit WAV (`dlna_wav24`), which keeps the full depth.
        let wav_output = wire_wav || dlna_lpcm || dlna_wav24;
        let transcode_lossless = (is_oaat && is_lossless && !is_dsd)
            || (wav_output && is_lossless && (dlna_wav24 || bit_depth <= 16));
        // Reflect the OUTPUT resolution the renderer actually receives: 24-bit
        // for the opt-in 24-bit WAV path, 16-bit when the zone caps to 16-bit OR
        // serves the plain LPCM fallback (audio/L16 is 16-bit), and the
        // max-sample-rate cap when set.
        //
        // Quand le fil renseigne ces valeurs, elles PRIMENT : elles décrivent
        // ce que le renderer reçoit, là où les règles ci-dessous ne font que
        // rejouer les décisions de l'orchestrateur et prennent du retard à
        // chaque évolution du chemin audio.
        let out_bit_depth = wire_bit_depth.map(|v| v as i32).unwrap_or(if dlna_wav24 {
            bit_depth.min(24)
        } else if dlna_cap_16bit || wav_output {
            bit_depth.min(16)
        } else {
            bit_depth
        });
        let out_sample_rate = wire_sample_rate.map(|v| v as i32).unwrap_or_else(|| {
            zone.max_sample_rate
                .map(|m| (sample_rate as u32).min(m) as i32)
                .unwrap_or(sample_rate)
        });
        // Garde-fou #1315 : le nom du conteneur est deviné, les chiffres sont
        // mesurés. Une résolution DSD ne peut donc pas sortir d'ici sous un
        // nom de conteneur PCM, quelle que soit la cible de transcodage.
        let out_desc = output_stage_label(output_format_name, out_sample_rate, out_bit_depth);
        steps.push(json!({
            "name": "Transcoder",
            "description": format!("{source_desc} \u{2192} {out_desc}"),
            "bit_perfect": transcode_lossless,
        }));
    }

    // Resampler step (when zone max_sample_rate caps the output)
    if resampling_active {
        let max_sr = zone.max_sample_rate.unwrap();
        let src_khz = sample_rate / 1000;
        let dst_khz = max_sr / 1000;
        steps.push(json!({
            "name": "Resampler",
            "description": format!("{src_khz}kHz \u{2192} {dst_khz}kHz"),
            "bit_perfect": false,
        }));
    }

    // ReplayGain step — placé avant Volume/DSP, comme dans le chemin réel
    // (le gain est appliqué avant l'égaliseur, orchestrator.rs). Jamais en
    // PURE, jamais en mode off, jamais sans gain stocké : l'étape n'existe
    // que quand un facteur ≠ 1 multiplie réellement les échantillons.
    if let Some(rg) = &replaygain_step {
        steps.push(json!({
            "name": "ReplayGain",
            "description": rg.description,
            "bit_perfect": false,
            // Additifs (#1627) : la description reste le libellé prêt à
            // afficher, ces deux champs permettent au client de composer le
            // sien (icône, traduction) sans analyser une chaîne française.
            "granularity": rg.granularity,
            "gain_source": rg.source,
        }));
    }

    // La sonde locale tranche si le gain a réellement été appliqué. Pour les
    // sorties sans sonde, conserver l'affichage historique fondé sur le
    // réglage de zone.
    if let Some(runtime) = runtime_signal_path {
        match runtime.volume {
            // Le même fait ne peut pas être peint de deux couleurs selon la
            // plateforme : la branche sans sonde (macOS, Linux, navigateur,
            // toutes les sorties réseau, quelques lignes plus bas) marque déjà
            // l'étape Volume comme intacte, parce que le volume est une
            // préférence et non une dégradation. L'étape reste affichée, avec
            // son pourcentage : rien n'est caché, seule la couleur cesse de
            // contredire le verdict (#2053).
            OutputVolumeState::Applied => steps.push(json!({
                "name": "Volume",
                "description": format!("Volume logiciel {}%", (ui_volume * 100.0).round() as i32),
                "bit_perfect": true,
            })),
            OutputVolumeState::BypassedDop => steps.push(json!({
                "name": "Volume",
                "description": "Volume contourné pour DoP",
                "bit_perfect": true,
            })),
            OutputVolumeState::Unity => {}
        }
    } else if !volume_full {
        steps.push(json!({
            "name": "Volume",
            "description": format!("Volume {}%", (ui_volume * 100.0).round() as i32),
            "bit_perfect": true,
        }));
    }

    // L'état d'exécution distingue traitement et contournement. C'est le
    // reliquat commun de #2205/#2233 : un réglage enregistré ne disait pas ce
    // qui avait effectivement atteint le ring Windows.
    let dsp_metrics = ps.output_dsp_metrics.map(|metrics| {
        json!({
            "eq_overs": metrics.eq_overs,
            "eq_non_finite_samples": metrics.eq_non_finite_samples,
        })
    });
    if let Some(runtime) = runtime_signal_path {
        let dsp_step = match runtime.dsp {
            OutputDspState::Applied => Some((
                eq_step_description.as_deref().unwrap_or("DSP appliqué"),
                false,
            )),
            OutputDspState::BypassedPure => Some(("DSP contourné par PURE", true)),
            OutputDspState::BypassedDop => Some(("DSP contourné pour DoP", true)),
            OutputDspState::Unknown => Some(("État DSP indéterminé", false)),
            OutputDspState::Inactive => None,
        };
        if let Some((description, intact)) = dsp_step {
            steps.push(json!({
                "name": "DSP",
                "description": description,
                "bit_perfect": intact,
                "metrics": dsp_metrics.clone(),
            }));
        }
    } else if dsp_contourne_par_le_dsd {
        // DIRE le contournement plutôt que de le taire. L'auditeur a un
        // égaliseur ARMÉ et n'entend rien changer : c'est exactement ce qu'Eric
        // a signalé (#1393). Faire disparaître l'étape le laisserait devant le
        // même curseur inerte, sans explication ; l'annoncer « actif » serait
        // le mensonge que #1315 et #2053 ont déjà coûté. On dit donc les deux
        // choses : il y a un DSP, et il ne s'applique pas ici.
        //
        // `bit_perfect: true` — le fil porte le DSD tel quel, rien n'y a
        // touché. Même convention que « DSP contourné pour DoP », que la sonde
        // de la sortie locale publie déjà.
        steps.push(json!({
            "name": "DSP",
            "description": "DSP contourné (DSD natif servi brut)",
            "bit_perfect": true,
        }));
    } else if dsp_applique {
        steps.push(json!({
            "name": "DSP",
            "description": eq_step_description.as_deref().unwrap_or("EQ/DSP active"),
            "bit_perfect": false,
        }));
    }

    // Étape « Mono » (#2362) — APRÈS le DSP et juste avant le transport, parce
    // que c'est exactement là qu'elle a lieu dans la chaîne : le repli tombe en
    // dernier dans `apply_local_dsp`, après l'égaliseur, le convolveur et le
    // crossfeed, qui ont tous besoin de leur contexte stéréo.
    //
    // `bit_perfect: false` sans hésitation : la profondeur et la fréquence sont
    // conservées, mais le CONTENU des deux voies est remplacé par leur demi-
    // somme. Ce n'est pas une préférence d'écoute comme le volume, c'est une
    // transformation du signal — et l'utilisateur qui la demande a le droit de
    // savoir ce qu'il échange.
    if let Some(desc) = &mono_downmix_step {
        steps.push(json!({
            "name": "Mono",
            "description": desc,
            "bit_perfect": false,
        }));
    }

    // Transport step
    steps.push(json!({
        "name": "Transport",
        "description": transport_desc,
        "bit_perfect": transport_bit_perfect,
        "detail": runtime_signal_path.and_then(runtime_signal_reason_detail),
    }));

    let renderer_name = renderer_label
        .or(zone.output_device_id.as_deref())
        .unwrap_or(output_type);
    steps.push(json!({
        "name": "Renderer",
        "description": renderer_name,
        "bit_perfect": transport_bit_perfect,
    }));

    // Build summary
    let bp_label = if bit_perfect { " (bit-perfect)" } else { "" };
    let summary = if transcode_active {
        format!(
            "{format_name} \u{2192} {output_format_name} transcode \u{2192} {transport_desc}{bp_label}"
        )
    } else {
        format!("{format_name} \u{2192} {transport_desc}{bp_label}")
    };

    Some(json!({
        "bit_perfect": bit_perfect,
        // Whether the *source* is a lossless format (FLAC, ALAC, WAV, DSD, …).
        // Distinct from bit_perfect: a lossless source transcoded to another
        // lossless container (DSD→FLAC, ALAC→FLAC for a DLNA renderer) is not
        // bit-perfect but is still lossless — the UI must not call it "lossy".
        "lossless": is_lossless,
        "summary": summary,
        "steps": steps,
        "runtime_observed": runtime_signal_path.is_some(),
        "runtime_reasons": runtime_signal_path.map(|status| &status.reasons),
        "dsp_metrics": dsp_metrics,
    }))
}

async fn list_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.list().unwrap_or_default();
    let devices = state.scanner.devices().await;
    // Manually-added devices (e.g. legacy DLNA renderers that never appear in
    // SSDP discovery) are registered as outputs but absent from `devices`.
    // Treat a registered output as online too, otherwise its zone is shown
    // offline even though playback works.
    let registered_output_ids: std::collections::HashSet<String> =
        state.outputs.lock().await.list().into_iter().collect();
    let default_zone_id: Option<i64> =
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("default_zone_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
    let audio_backend_pref = state.display_audio_backend();
    #[cfg(feature = "local-audio")]
    let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let mut v = serde_json::to_value(z).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "state".into(),
                json!(match ps.state {
                    tune_core::playback::PlayState::Playing => "playing",
                    tune_core::playback::PlayState::Paused => "paused",
                    tune_core::playback::PlayState::Stopped => "stopped",
                }),
            );
            obj.insert("current_track".into(), json!(ps.now_playing));
            inject_metadata_anchor(obj, &ps);
            inject_session_context(obj, &ps);
            obj.insert("position_ms".into(), json!(ps.position_ms));
            obj.insert("queue_length".into(), json!(ps.queue_length));
            obj.insert(
                "can_skip_next".into(),
                json!(crate::routes::playback::can_skip_next(&ps)),
            );
            // L'aleatoire et la repetition appartiennent a la ZONE, et ils
            // survivent aux redemarrages : `queue_persistence` les enregistre
            // avec la file, `startup.rs` les restaure.
            //
            // Cette charge utile ne les portait pas. Le client naissait donc a
            // `shuffleEnabled = false` et n'avait aucun moyen d'apprendre le
            // contraire : ses deux sites de recalage lisent `zone.shuffle` /
            // `zone.repeat` (`App.svelte`, `syncTransportFromZone`), c'est-a-dire
            // des champs que personne n'envoyait. Seul un CLIC sur le bouton
            // remettait l'ecran d'accord avec le serveur — le geste qu'on
            // cherche justement a eviter.
            //
            // Resultat vecu par Tades (#2092) : un aleatoire actif cote serveur
            // et eteint a l'ecran, sans limite de duree. L'album part dans le
            // desordre, « suivant » saute au hasard, et le bouton qui
            // expliquerait tout parait inactif. Il a ouvert deux fils, en
            // ecrivant « je ne pense pas avoir parametre cela » : il avait
            // raison de ne pas s'en souvenir, rien ne le lui montrait.
            //
            // Le WebSocket, lui, les envoyait deja (`ws.rs`) : c'est REST qui
            // etait en retard, et c'est REST que le client lit au changement de
            // zone et apres chaque evenement de lecture.
            obj.insert("shuffle".into(), json!(ps.shuffle));
            obj.insert("repeat".into(), json!(ps.repeat));
            // #1274 — `volume` (linéaire, 0..1) et `volume_db` (atténuation en
            // dB, `null` = silence) sortent ensemble du même nombre. Le champ
            // dB est ADDITIF : aucun client déployé ne perd `volume`.
            tune_core::audio::volume_scale::inserer_volume(
                obj,
                if ps.volume > 0.0 {
                    ps.volume
                } else {
                    z.volume / 100.0
                },
            );
            let renderer_label = z
                .output_device_id
                .as_deref()
                .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
            let wire = match ps
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref())
            {
                Some(sid) => state.streamer.stream_output_wire(sid).await,
                None => None,
            };
            let signal_path = build_signal_path(
                &ps,
                z,
                &state.backend,
                renderer_label,
                audio_backend,
                wire.as_ref(),
            );
            obj.insert("signal_path".into(), json!(signal_path));
            // #1395 — quel backend local tourne VRAIMENT sur cette zone, face à
            // celui qui est réglé. Absent des zones non locales.
            if let Some(status) =
                local_backend_status_value(z.output_type.as_deref(), &audio_backend_pref)
            {
                obj.insert("audio_backend_status".into(), status);
            }
            // Recherche en cours (extraction YouTube longue) : l'interface peut le dire.
            obj.insert("resolving".into(), json!(ps.resolving));
            obj.insert("is_default".into(), json!(default_zone_id == Some(zone_id)));
            // Flux DoP en cours : le curseur de volume ne fait rien, et
            // l'interface doit le dire (#1735). Détecté sur les octets par la
            // sortie, pas déduit de `dsd_mode` — celui-ci dit ce qui a été
            // demandé, pas ce qui part sur le fil.
            obj.insert("dop_active".into(), json!(ps.dop_active));
            let zone_repo = ZoneRepo::with_backend(state.backend.clone());
            obj.insert("dsd_mode".into(), json!(zone_repo.get_dsd_mode(zone_id)));
            obj.insert(
                "lyrics_offset_ms".into(),
                json!(zone_repo.get_lyrics_offset_ms(zone_id)),
            );
            obj.insert(
                "dlna_native_flac".into(),
                json!(zone_repo.get_dlna_native_flac(zone_id)),
            );
            obj.insert(
                "alac_passthrough".into(),
                json!(zone_repo.get_alac_passthrough(zone_id)),
            );
            obj.insert(
                "aac_passthrough".into(),
                json!(zone_repo.get_aac_passthrough(zone_id)),
            );
            obj.insert("dlna_lpcm".into(), json!(zone_repo.get_dlna_lpcm(zone_id)));
            obj.insert(
                "dlna_cap_16bit".into(),
                json!(zone_repo.get_dlna_cap_16bit(zone_id)),
            );
            obj.insert(
                "dlna_wav24".into(),
                json!(zone_repo.get_dlna_wav24(zone_id)),
            );
            obj.insert(
                "dlna_play_delay_ms".into(),
                json!(zone_repo.get_dlna_play_delay_ms(zone_id)),
            );
            // `autoplay_enabled` est VOLONTAIREMENT absent de la requete SQL
            // de `ZoneRepo` (migration v36 pouvant echouer en silence sous
            // Windows), donc `row_to_zone` le met a `false` sans exception —
            // et la serialisation de la zone propageait ce faux jusqu'au
            // client. Le bouton AutoPlay retombait donc a chaque
            // resynchronisation, alors que le reglage etait bien en base et
            // correctement lu par le poller (Sandro, 0.9.70). On lit la vraie
            // valeur par l'accesseur prevu pour ca, comme les autres reglages
            // de zone ci-dessus.
            // #2271 — les deux champs sortent ensemble et decrivent la meme
            // colonne. `autoplay_enabled` reste emis tel quel : le client web
            // actuel ne lit que lui, et le retirer casserait le bouton.
            let autoplay_mode = zone_repo.get_autoplay_mode(zone_id);
            obj.insert(
                "autoplay_enabled".into(),
                json!(autoplay_mode != AutoplayMode::Off),
            );
            obj.insert("autoplay_mode".into(), json!(autoplay_mode.as_str()));
            let detected_dev = z
                .output_device_id
                .as_deref()
                .and_then(|did| devices.iter().find(|d| d.id == did));
            inject_device_identity(obj, &state.backend, zone_id, detected_dev);
            let online = match z.output_type.as_deref() {
                // Browser zones have no output device by design (the web
                // client pulls stream_url itself) — always online.
                Some("browser") => true,
                // A local zone is online as long as it still has a device
                // assigned; an orphan row without output_device_id can never
                // play (Yacine, 24/07) and must be reported offline so
                // clients grey it out. Other types already fall through to
                // unwrap_or(false) when output_device_id is NULL.
                Some("local") => z.output_device_id.is_some(),
                _ => z
                    .output_device_id
                    .as_deref()
                    .map(|id| {
                        devices.iter().any(|d| d.id == id) || registered_output_ids.contains(id)
                    })
                    .unwrap_or(false),
            };
            obj.insert("online".into(), json!(online));
            obj.insert(
                "output_reach".into(),
                json!(output_reach(&state, z, &ps).await),
            );
            obj.insert(
                "levels_available".into(),
                json!(levels_available(&state, z).await),
            );
            obj.insert(
                "output_capabilities".into(),
                json!(output_capabilities(&state, z.output_device_id.as_deref()).await),
            );
            // #3164 — l'adresse du flux ne se publie QUE pour une zone
            // navigateur. Ce site-ci la rendait à toutes : un onglet ouvert sur
            // la liste des zones tenait de quoi couper la lecture d'un renderer.
            inject_stream_url(
                obj,
                &state,
                z.output_type.as_deref(),
                ps.now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.as_deref()),
            );
        }
        result.push(v);
    }
    Json(json!(result))
}

async fn get_zone(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let audio_backend_pref = state.display_audio_backend();
    #[cfg(feature = "local-audio")]
    let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    match repo.get(id) {
        Ok(Some(zone)) => {
            let ps = state.playback.get_state(id).await;
            let mut v = serde_json::to_value(&zone).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "state".into(),
                    json!(match ps.state {
                        tune_core::playback::PlayState::Playing => "playing",
                        tune_core::playback::PlayState::Paused => "paused",
                        tune_core::playback::PlayState::Stopped => "stopped",
                    }),
                );
                obj.insert("current_track".into(), json!(ps.now_playing));
                inject_metadata_anchor(obj, &ps);
                inject_session_context(obj, &ps);
                obj.insert("position_ms".into(), json!(ps.position_ms));
                obj.insert("queue_length".into(), json!(ps.queue_length));
                // Expose the queue index too so the client can refresh the
                // "now playing" highlight on track change without refetching the
                // whole queue (expensive under a large shuffle queue, #1096).
                obj.insert("queue_position".into(), json!(ps.queue_position));
                obj.insert(
                    "can_skip_next".into(),
                    json!(crate::routes::playback::can_skip_next(&ps)),
                );
                // Meme raison qu'au-dessus (#2092) : c'est cette charge utile
                // que le client relit apres chaque evenement de lecture, et
                // c'est elle qui doit lui apprendre un aleatoire deja actif.
                obj.insert("shuffle".into(), json!(ps.shuffle));
                obj.insert("repeat".into(), json!(ps.repeat));
                // #1274 — même paire qu'au-dessus, depuis la même source :
                // ici la colonne `zones.volume`, arrondie au pour-cent.
                tune_core::audio::volume_scale::inserer_volume(obj, zone.volume / 100.0);
                let devices = state.scanner.devices().await;
                let registered_output_ids: std::collections::HashSet<String> =
                    state.outputs.lock().await.list().into_iter().collect();
                let renderer_label = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
                let wire = match ps
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.as_deref())
                {
                    Some(sid) => state.streamer.stream_output_wire(sid).await,
                    None => None,
                };
                let signal_path = build_signal_path(
                    &ps,
                    &zone,
                    &state.backend,
                    renderer_label,
                    audio_backend,
                    wire.as_ref(),
                );
                obj.insert("signal_path".into(), json!(signal_path));
                // #1395 — voir la note au site jumeau (`list_zones`).
                if let Some(status) =
                    local_backend_status_value(zone.output_type.as_deref(), &audio_backend_pref)
                {
                    obj.insert("audio_backend_status".into(), status);
                }
                // Recherche en cours (extraction YouTube longue) : l'interface peut le dire.
                obj.insert("resolving".into(), json!(ps.resolving));
                // Voir la note au site jumeau : DoP en cours ⇒ volume inerte.
                obj.insert("dop_active".into(), json!(ps.dop_active));
                obj.insert("dsd_mode".into(), json!(repo.get_dsd_mode(id)));
                obj.insert(
                    "lyrics_offset_ms".into(),
                    json!(repo.get_lyrics_offset_ms(id)),
                );
                obj.insert(
                    "dlna_native_flac".into(),
                    json!(repo.get_dlna_native_flac(id)),
                );
                obj.insert(
                    "alac_passthrough".into(),
                    json!(repo.get_alac_passthrough(id)),
                );
                obj.insert(
                    "aac_passthrough".into(),
                    json!(repo.get_aac_passthrough(id)),
                );
                obj.insert("dlna_lpcm".into(), json!(repo.get_dlna_lpcm(id)));
                obj.insert("dlna_cap_16bit".into(), json!(repo.get_dlna_cap_16bit(id)));
                obj.insert("dlna_wav24".into(), json!(repo.get_dlna_wav24(id)));
                obj.insert(
                    "dlna_play_delay_ms".into(),
                    json!(repo.get_dlna_play_delay_ms(id)),
                );
                // Meme correction que dans la liste : la valeur serialisee
                // depuis la struct vaut toujours `false`.
                // #2271 — meme paire que dans la liste.
                let autoplay_mode = repo.get_autoplay_mode(id);
                obj.insert(
                    "autoplay_enabled".into(),
                    json!(autoplay_mode != AutoplayMode::Off),
                );
                obj.insert("autoplay_mode".into(), json!(autoplay_mode.as_str()));
                let detected_dev = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|did| devices.iter().find(|d| d.id == did));
                inject_device_identity(obj, &state.backend, id, detected_dev);
                let online = match zone.output_type.as_deref() {
                    // Same rules as list_zones: browser zones need no device;
                    // a local zone without output_device_id is an orphan that
                    // can never play → offline.
                    Some("browser") => true,
                    Some("local") => zone.output_device_id.is_some(),
                    _ => zone
                        .output_device_id
                        .as_deref()
                        .map(|did| {
                            devices.iter().any(|d| d.id == did)
                                || registered_output_ids.contains(did)
                        })
                        .unwrap_or(false),
                };
                obj.insert("online".into(), json!(online));
                obj.insert(
                    "output_reach".into(),
                    json!(output_reach(&state, &zone, &ps).await),
                );
                obj.insert(
                    "levels_available".into(),
                    json!(levels_available(&state, &zone).await),
                );
                obj.insert(
                    "output_capabilities".into(),
                    json!(output_capabilities(&state, zone.output_device_id.as_deref()).await),
                );
                // #3164 — même règle que la liste, et le même trou : la fiche
                // d'une zone DLNA rendait l'adresse de son flux au client web.
                inject_stream_url(
                    obj,
                    &state,
                    zone.output_type.as_deref(),
                    ps.now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref()),
                );
            }
            Json(v).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Les valeurs que `output_type` peut prendre — celles que l'orchestrateur sait
/// router (`orchestrator.rs`). Une zone dont le type est inconnu ne joue nulle
/// part : la refuser à l'écriture vaut mieux que la découvrir au premier « Lire ».
const TYPES_DE_SORTIE: [&str; 8] = [
    "local",
    "browser",
    "dlna",
    "openhome",
    "chromecast",
    "bluos",
    "squeezebox",
    "oaat",
];

/// Les modes DSD reconnus par `should_dsd_passthrough` et `dop_requested`.
/// Tout le reste retombe dans le fourre-tout « auto » sans le dire.
const MODES_DSD: [&str; 4] = ["auto", "native", "pcm", "dop"];

/// Une écriture du PATCH a échoué côté base : **journaliser**, puis 500.
///
/// Ces retours étaient muets : trente blocs rendaient
/// `(INTERNAL_SERVER_ERROR, e)` sans qu'aucune ligne ne parte dans les
/// journaux. Un 500 signalé par un testeur ne laissait donc **aucune trace
/// exploitable** — c'est ce qui a rendu #1964 impossible à instruire, et il a
/// fallu écrire à Gérard pour lui demander le corps de la réponse que le
/// serveur avait déjà entre les mains.
fn echec_ecriture(
    zone_id: i64,
    champ: &str,
    valeur: &str,
    erreur: String,
) -> axum::response::Response {
    tracing::error!(
        zone_id,
        champ,
        valeur,
        erreur = %erreur,
        "zone_patch_write_failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("écriture impossible du champ « {champ} » : {erreur}"),
    )
        .into_response()
}

/// La requête elle-même est fautive : **journaliser**, puis 400.
///
/// 500 veut dire « le serveur a un défaut ». L'envoyer pour une valeur que le
/// client aurait pu corriger lui interdit de faire la différence entre ce qu'il
/// doit réparer et ce qu'il doit signaler.
fn refus_de_valeur(
    zone_id: i64,
    champ: &str,
    valeur: &str,
    raison: &str,
) -> axum::response::Response {
    warn!(zone_id, champ, valeur, raison, "zone_patch_rejected");
    (
        StatusCode::BAD_REQUEST,
        format!("champ « {champ} » : {raison} (reçu : « {valeur} »)"),
    )
        .into_response()
}

async fn patch_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());

    // La zone existe-t-elle ? Sans ce contrôle, un PATCH sur un identifiant
    // inconnu exécutait la trentaine d'UPDATE — qui touchent zéro ligne et
    // réussissent — avant que `get_zone` ne rende 404 tout à la fin. Le 404
    // était juste, mais il arrivait après trente écritures inutiles et ne
    // disait pas laquelle avait échoué en cas de vrai problème.
    let zone_before = match repo.get(id) {
        Ok(Some(zone)) => zone,
        Ok(None) => {
            warn!(zone_id = id, "zone_patch_unknown_zone");
            return (StatusCode::NOT_FOUND, format!("zone {id} inconnue")).into_response();
        }
        Err(e) => return echec_ecriture(id, "zone", &id.to_string(), e),
    };

    // Les valeurs que cette route peut juger seule, avant toute écriture : un
    // PATCH est atomique du point de vue de l'utilisateur, il ne doit pas
    // laisser la moitié de ses champs écrits derrière lui.
    if let Some(ref ot) = body.output_type
        && !TYPES_DE_SORTIE.contains(&ot.as_str())
    {
        return refus_de_valeur(
            id,
            "output_type",
            ot,
            &format!(
                "type de sortie inconnu (attendu : {})",
                TYPES_DE_SORTIE.join(", ")
            ),
        );
    }
    if let Some(ref mode) = body.dsd_mode
        && !MODES_DSD.contains(&mode.as_str())
    {
        return refus_de_valeur(
            id,
            "dsd_mode",
            mode,
            &format!("mode DSD inconnu (attendu : {})", MODES_DSD.join(", ")),
        );
    }
    // #2271 — un mode inconnu est REFUSE, jamais range en base. Sans ce
    // garde-fou une faute de frappe s'ecrirait telle quelle et la lecture
    // tolerante de `get_autoplay_mode` la rattraperait en `similar` : la zone
    // se mettrait a enchainer alors que l'auditeur croyait l'eteindre.
    if let Some(ref mode) = body.autoplay_mode
        && AutoplayMode::from_str_stocke(mode).is_none()
    {
        return refus_de_valeur(
            id,
            "autoplay_mode",
            mode,
            &format!(
                "mode de continuation inconnu (attendu : {})",
                AutoplayMode::NOMS.join(", ")
            ),
        );
    }
    if let Some(vol) = body.volume
        && !(0..=100).contains(&vol)
    {
        return refus_de_valeur(id, "volume", &vol.to_string(), "hors de 0..100");
    }
    // #1274 — `volume` et `volume_db` sont exclusifs, et la validation du dB
    // vit dans `volume_scale`. Ce PATCH ne peut pas déléguer complètement :
    // son champ historique est un entier 0..100, il doit donc le ramener sur
    // 0..1 lui-même. Le refus, lui, est rendu sous la forme que le reste du
    // handler emploie.
    let volume_demande = match tune_core::audio::volume_scale::demande_lineaire(
        body.volume.map(f64::from).map(|v| v / 100.0),
        body.volume_db,
    ) {
        Ok(v) => Some(v),
        // Aucun des deux champs n'est présent : ce PATCH ne parle pas de
        // volume, et c'est le cas le plus courant.
        Err(_) if body.volume.is_none() && body.volume_db.is_none() => None,
        Err(motif) => {
            let recu = match (body.volume, body.volume_db) {
                (Some(v), Some(db)) => format!("volume={v} volume_db={db}"),
                (_, Some(db)) => db.to_string(),
                (Some(v), _) => v.to_string(),
                _ => String::new(),
            };
            return refus_de_valeur(id, "volume_db", &recu, motif);
        }
    };
    if let Some(ref device_id) = body.output_device_id
        && device_id.trim().is_empty()
    {
        // Une chaîne vide n'efface pas la sortie, elle la rend introuvable :
        // la zone reste « configurée » et ne joue nulle part.
        return refus_de_valeur(
            id,
            "output_device_id",
            device_id,
            "vide — pour retirer la sortie, envoyer output_type",
        );
    }
    if let Some(ref name) = body.name
        && name.trim().is_empty()
    {
        return refus_de_valeur(id, "name", name, "vide");
    }

    // Ce refus précède strictement la première écriture : un PATCH qui porte
    // d'autres champs ne doit rien modifier si l'accord manque.
    if fixed_volume_confirmation_required(&zone_before, &body) {
        warn!(zone_id = id, "fixed_volume_confirmation_required");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "full_volume_confirmation_required",
                "message": "Enabling fixed volume raises this zone to full scale (100%). Confirm with `confirm_full_volume` to proceed.",
            })),
        )
            .into_response();
    }

    // Volume et mute sont des commandes, pas de simples préférences. Le
    // renderer doit les accepter avant qu'un PATCH puisse annoncer leur
    // réussite ou laisser une valeur mensongère en base. Si le PATCH change
    // aussi de sortie, la commande vise explicitement la nouvelle sortie.
    let command_device_id = body
        .output_device_id
        .as_deref()
        .or(zone_before.output_device_id.as_deref());
    // #1274 — même garde-fou que sur PUT/POST …/volume : ce PATCH est la
    // troisième porte d'écriture du volume, et la consigne y arrive aussi en
    // dB. `command_device_id` porte déjà la sortie VISÉE, celle que ce même
    // PATCH est peut-être en train d'attribuer.
    if let Some(db) = body.volume_db
        && let Some(motif) = refus_de_resolution_volume(&state, command_device_id, db).await
    {
        return refus_de_valeur(id, "volume_db", &db.to_string(), &motif);
    }

    // #1274 — `volume_demande` porte déjà la valeur linéaire, qu'elle vienne
    // du pour-cent entier ou des dB. L'orchestrateur la reçoit en `f64` et la
    // garde telle quelle dans l'état de lecture, vers le device et en base
    // (la colonne n'arrondit plus au pour-cent depuis #2886).
    if let Some(volume) = volume_demande
        && let Err(error) = state
            .orchestrator
            .set_volume(id, volume, command_device_id)
            .await
    {
        return crate::routes::playback::output_command_error_response(error);
    }
    if let Some(muted) = body.muted
        && let Err(error) = state
            .orchestrator
            .set_mute(id, muted, command_device_id)
            .await
    {
        return crate::routes::playback::output_command_error_response(error);
    }

    /// Écrit un champ, ou s'arrête en journalisant la cause.
    ///
    /// Une macro et non une closure : chaque échec doit **sortir** du handler,
    /// et une closure ne peut pas rendre la main à sa place. C'est aussi ce qui
    /// garantit qu'aucun des trente blocs ne puisse redevenir muet — il n'y a
    /// plus qu'un seul endroit où le `return` est écrit.
    macro_rules! ecrire {
        ($champ:literal, $valeur:expr, $ecriture:expr) => {
            if let Err(e) = $ecriture {
                return echec_ecriture(id, $champ, &$valeur.to_string(), e);
            }
        };
    }

    if let Some(ref name) = body.name {
        ecrire!("name", name, repo.update_name(id, name));
    }
    // volume/muted ont été confirmés et persistés par l'orchestrateur ci-dessus.
    if let Some(ref device_id) = body.output_device_id {
        ecrire!(
            "output_device_id",
            device_id,
            repo.update_output_device(id, device_id)
        );
    }
    if let Some(ref ot) = body.output_type {
        ecrire!("output_type", ot, repo.update_output_type(id, ot));
    }
    if let Some(gapless) = body.gapless_enabled {
        ecrire!(
            "gapless_enabled",
            gapless,
            repo.update_gapless_enabled(id, gapless)
        );
    }
    if let Some(ms) = body.sync_delay_ms {
        ecrire!("sync_delay_ms", ms, repo.update_sync_delay(id, ms));
    }
    if let Some(rate) = body.max_sample_rate {
        ecrire!(
            "max_sample_rate",
            rate.map(|r| r.to_string()).unwrap_or_else(|| "null".into()),
            repo.update_max_sample_rate(id, rate)
        );
    }
    if let Some(fixed) = body.fixed_volume {
        // #2395 — le mode bit-perfect fait UN saut, annoncé et réversible.
        //
        // Seules les TRANSITIONS agissent : un PATCH qui réaffirme l'état
        // courant ne commande rien. C'est ce qui rend le saut unique — sans
        // cette garde, chaque `{"fixed_volume": true}` d'un client bavard
        // renverrait 100 % à l'appareil, et on aurait remplacé la réassertion
        // à la lecture par une réassertion au PATCH.
        let etait_fixe = zone_before.fixed_volume;
        ecrire!("fixed_volume", fixed, repo.update_fixed_volume(id, fixed));
        if fixed && !etait_fixe {
            // Mémoriser AVANT de commander : une fois le 100 % appliqué, la
            // valeur d'origine n'est plus lisible nulle part. L'échec de la
            // mémorisation coûte la restauration, pas le mode — il est dit au
            // journal, il n'interrompt pas l'armement.
            if let Err(error) =
                tune_core::audio::fixed_volume::remember(&state.backend, id, zone_before.volume)
            {
                warn!(zone_id = id, %error, "fixed_volume_memoire_non_ecrite");
            }
            // `arm_fixed_volume` et non `set_volume` : ce dernier sort au plus
            // tôt sur une zone désormais `fixed_volume` et ne parlerait pas au
            // device. C'est ici, et nulle part ailleurs, que le 100 % part.
            if let Err(error) = state
                .orchestrator
                .arm_fixed_volume(id, command_device_id)
                .await
            {
                return crate::routes::playback::output_command_error_response(error);
            }
        } else if !fixed && etait_fixe {
            // Sortie du mode : rendre le volume d'avant. `update_fixed_volume`
            // est déjà écrit ci-dessus, donc `set_volume` ne sort plus au plus
            // tôt et commande réellement l'appareil.
            //
            // Sans mémoire (zone armée par une version antérieure à ce
            // correctif, ou écriture perdue), on ne devine pas : la zone reste
            // à 100 % et l'utilisateur garde la main. Commander une valeur
            // inventée serait le défaut qu'on corrige, à l'envers.
            match tune_core::audio::fixed_volume::take(&state.backend, id) {
                Some(pourcent) => {
                    if let Err(error) = state
                        .orchestrator
                        .set_volume(id, pourcent / 100.0, command_device_id)
                        .await
                    {
                        return crate::routes::playback::output_command_error_response(error);
                    }
                    info!(zone_id = id, volume = pourcent, "fixed_volume_restaure");
                }
                None => info!(zone_id = id, "fixed_volume_sans_memoire_rien_a_restaurer"),
            }
        }
    }
    // #2271 — les deux champs visent la MEME colonne. `autoplay_mode` est le
    // plus precis, il gagne ; `autoplay_enabled` n'est applique que seul, pour
    // que les clients qui ne connaissent que lui continuent de fonctionner.
    if let Some(ref mode) = body.autoplay_mode {
        // Deja valide plus haut : le `unwrap_or` n'est pas atteignable.
        let mode = AutoplayMode::from_str_stocke(mode).unwrap_or_default();
        ecrire!(
            "autoplay_mode",
            mode.as_str(),
            repo.update_autoplay_mode(id, mode)
        );
    } else if let Some(autoplay) = body.autoplay_enabled {
        ecrire!(
            "autoplay_enabled",
            autoplay,
            repo.update_autoplay_enabled(id, autoplay)
        );
    }
    if let Some(ref mode) = body.dsd_mode {
        ecrire!("dsd_mode", mode, repo.update_dsd_mode(id, mode));
    }
    if let Some(offset) = body.lyrics_offset_ms {
        // Borne large mais finie : au-dela d'une minute ce n'est plus un
        // reglage de latence, et une valeur folle desynchroniserait tout.
        let clamped = offset.clamp(-60_000, 60_000);
        ecrire!(
            "lyrics_offset_ms",
            clamped,
            repo.update_lyrics_offset_ms(id, clamped)
        );
    }
    if let Some(native_flac) = body.dlna_native_flac {
        ecrire!(
            "dlna_native_flac",
            native_flac,
            repo.update_dlna_native_flac(id, native_flac)
        );
    }
    if let Some(passthrough) = body.alac_passthrough {
        ecrire!(
            "alac_passthrough",
            passthrough,
            repo.update_alac_passthrough(id, passthrough)
        );
    }
    if let Some(passthrough) = body.aac_passthrough {
        ecrire!(
            "aac_passthrough",
            passthrough,
            repo.update_aac_passthrough(id, passthrough)
        );
    }
    if let Some(lpcm) = body.dlna_lpcm {
        ecrire!("dlna_lpcm", lpcm, repo.update_dlna_lpcm(id, lpcm));
    }
    if let Some(cap) = body.dlna_cap_16bit {
        ecrire!("dlna_cap_16bit", cap, repo.update_dlna_cap_16bit(id, cap));
    }
    if let Some(wav24) = body.dlna_wav24 {
        ecrire!("dlna_wav24", wav24, repo.update_dlna_wav24(id, wav24));
    }
    if let Some(delay) = body.dlna_play_delay_ms {
        let delay = delay.max(0) as u64;
        ecrire!(
            "dlna_play_delay_ms",
            delay,
            repo.update_dlna_play_delay_ms(id, delay)
        );
        // Apply live to the already-registered output so the new delay takes
        // effect on the next play without a rebuild/restart. 0 = fall back to the
        // config default (`[device_delays]` / `dlna_play_delay_ms`) by name.
        if let Some(device_id) = repo.get(id).ok().flatten().and_then(|z| z.output_device_id) {
            let output = { state.outputs.lock().await.get(&device_id) };
            if let Some(output) = output {
                let guard = output.lock().await;
                // `name()` is an OutputTarget trait method → read it on the trait
                // object before downcasting to the concrete DlnaOutput.
                let effective = if delay > 0 {
                    delay
                } else {
                    state.config.play_delay_for(guard.name())
                };
                if let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() {
                    dlna.set_play_delay(effective);
                }
            }
        }
    }
    // Marque / modèle choisis par l'utilisateur → settings zone_{id}_brand/model.
    // Chaîne vide = suppression de l'override (retour à la détection UPnP).
    if let Some(ref brand) = body.brand {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_brand");
        let r = if brand.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, brand.trim())
        };
        ecrire!("brand", brand, r);
    }
    if let Some(ref model) = body.model {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_model");
        let r = if model.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, model.trim())
        };
        ecrire!("model", model, r);
    }
    // Opt-in MediaRenderer UPnP (#1750) → setting zone_{id}_upnp_renderer.
    if let Some(enabled) = body.upnp_renderer {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_upnp_renderer");
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("upnp_renderer", enabled, r);
        // Annonce (ou retrait de l'annonce) sans attendre le cycle de 10 min.
        crate::routes::upnp_media_renderer::advertiser_wakeup().notify_one();
    }
    // Silence UPnP (#2263) → setting zone_{id}_upnp_silence. Même forme que
    // `upnp_renderer` : clé supprimée à la désactivation.
    if let Some(enabled) = body.upnp_silence {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = crate::config::cle_silence_upnp(id);
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("upnp_silence", enabled, r);
        // Appliqué en DIRECT à la sortie déjà enregistrée : persister ne suffit
        // pas, sans cela cocher la case en écoutant ne changerait rien avant la
        // piste suivante — même piège que le `dlna_play_delay_ms` ci-dessus.
        if let Some(device_id) = repo.get(id).ok().flatten().and_then(|z| z.output_device_id) {
            let output = { state.outputs.lock().await.get(&device_id) };
            if let Some(output) = output {
                let guard = output.lock().await;
                if let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() {
                    dlna.set_upnp_silence(enabled);
                    // Ce que l'utilisateur vient d'accepter, écrit noir sur
                    // blanc dans le journal : l'option n'est pas muette.
                    info!(
                        zone = id,
                        device = %device_id,
                        silence = enabled,
                        abonnable = dlna.peut_s_abonner(),
                        "zone_silence_upnp — position estimée et déplacement façade différé quand armé"
                    );
                }
            }
        }
    }
    // Sortie mono (#2362) → setting zone_{id}_mono_downmix. Même forme que
    // `upnp_renderer` juste au-dessus : la clé est supprimée à la désactivation
    // plutôt qu'écrite à « false », pour que l'absence de clé et le défaut
    // désarmé soient un seul et même état.
    if let Some(enabled) = body.mono_downmix {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_mono_downmix");
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("mono_downmix", enabled, r);
        // Persister ne suffit pas : sans ceci, cocher la case en écoutant ne
        // changerait rien avant la piste suivante (#1725, #1786). Or ce
        // réglage-ci se vérifie précisément à l'oreille, musique en cours.
        state.orchestrator.refresh_zone_mono_downmix(id).await;
    }
    // Trim de gain par renderer → setting zone_{id}_gain_trim_db (±12 dB, 0 = efface).
    if let Some(db) = body.gain_trim_db {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_gain_trim_db");
        let clamped = db.clamp(-12.0, 12.0);
        let r = if clamped == 0.0 {
            settings.delete(&key)
        } else {
            settings.set(&key, &format!("{clamped}"))
        };
        ecrire!("gain_trim_db", clamped, r);
        // Effet immédiat : re-pousser le volume courant au device (le trim est
        // composé dans orchestrator.set_volume). Sans ça, il faudrait attendre
        // le prochain coup de curseur.
        if let Ok(Some(z)) = repo.get(id) {
            if !z.fixed_volume {
                if let Some(ref did) = z.output_device_id {
                    if let Err(error) = state
                        .orchestrator
                        .set_volume(id, z.volume / 100.0, Some(did))
                        .await
                    {
                        warn!(zone_id = id, error = %error, "gain_trim_volume_refresh_failed");
                    }
                }
            }
        }
    }
    // Correction de marque/modele : la remonter a mozaiklabs.fr.
    //
    // Le catalogue d'appareils est fige dans le binaire ; ces corrections sont
    // la seule matiere qui permette de le faire evoluer a partir du parc reel.
    // Envoi anonyme et sans attente : la reponse HTTP a l'utilisateur ne doit
    // dependre en rien de la disponibilite du site.
    if body.brand.is_some() || body.model.is_some() {
        push_device_correction(&state, id).await;
        // Dans la foulée : les réglages qui marchent chez cet utilisateur pour
        // cet appareil identifié (#1743) — c'est au moment où il nomme son
        // appareil qu'on sait à quoi rattacher le préréglage.
        push_device_preset(&state, id).await;
    }

    get_zone(State(state), Path(id)).await.into_response()
}

/// Remonte au catalogue communautaire la marque/modele corriges d'une zone.
///
/// Ne part que si l'override est complet (marque ET modele) : une correction
/// partielle n'apprend rien de reutilisable au catalogue.
///
/// Soumis au meme consentement que la telemetrie (`TUNE_TELEMETRY`) : c'est la
/// porte deja etablie pour « cette instance parle-t-elle au cloud », et en
/// ajouter une seconde pour la meme question fragmenterait le reglage sans
/// rien clarifier.
///
/// Volontairement anonyme : ni identifiant d'instance, ni nom de zone. Le
/// serveur n'attend pas la reponse et n'echoue jamais la-dessus.
/// Réglages de renderer non-défaut d'une zone, sous la forme partagée avec le
/// catalogue communautaire (clés du RendererConfig + trim). Vide quand la zone
/// est aux défauts — un préréglage qui ne règle rien n'apprend rien.
fn renderer_settings_snapshot(state: &AppState, zone_id: i64) -> serde_json::Map<String, Value> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut out = serde_json::Map::new();
    if repo.get_dlna_native_flac(zone_id) {
        out.insert("dlna_native_flac".into(), json!(true));
    }
    if repo.get_alac_passthrough(zone_id) {
        out.insert("alac_passthrough".into(), json!(true));
    }
    if repo.get_aac_passthrough(zone_id) {
        out.insert("aac_passthrough".into(), json!(true));
    }
    if repo.get_dlna_lpcm(zone_id) {
        out.insert("dlna_lpcm".into(), json!(true));
    }
    if repo.get_dlna_cap_16bit(zone_id) {
        out.insert("dlna_cap_16bit".into(), json!(true));
    }
    if repo.get_dlna_wav24(zone_id) {
        out.insert("dlna_wav24".into(), json!(true));
    }
    let delay = repo.get_dlna_play_delay_ms(zone_id);
    if delay > 0 {
        out.insert("dlna_play_delay_ms".into(), json!(delay));
    }
    // #2263 — même famille que `dlna_play_delay_ms` : un réglage qu'on garde
    // parce que CET appareil-là le demande. Absent du relevé tant qu'il est au
    // défaut, comme tous ses voisins ici.
    if settings
        .get(&crate::config::cle_silence_upnp(zone_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
    {
        out.insert("upnp_silence".into(), json!(true));
    }
    let trim = settings
        .get(&format!("zone_{zone_id}_gain_trim_db"))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    if trim != 0.0 {
        out.insert("gain_trim_db".into(), json!(trim));
    }
    out
}

/// Identité (marque, modèle) d'une zone pour le catalogue communautaire :
/// override utilisateur d'abord, sinon détection UPnP de l'appareil assigné.
async fn zone_identity_for_catalog(state: &AppState, zone_id: i64) -> Option<(String, String)> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = |k: &str| {
        settings
            .get(&format!("zone_{zone_id}_{k}"))
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
    };
    let (mut brand, mut model) = (key("brand"), key("model"));
    if brand.is_none() || model.is_none() {
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()?;
        let devices = state.scanner.devices().await;
        let detected = zone
            .output_device_id
            .as_deref()
            .and_then(|did| devices.iter().find(|d| d.id == did));
        if brand.is_none() {
            brand = detected.and_then(|d| d.manufacturer.clone());
        }
        if model.is_none() {
            model = detected.and_then(|d| d.model.clone());
        }
    }
    match (brand, model) {
        (Some(b), Some(m)) => Some((b, m)),
        _ => None,
    }
}

/// GET /zones/{id}/device-presets — les préréglages communautaires pour
/// l'appareil de la zone (#1743). Proxy serveur vers mozaiklabs : le
/// navigateur ne parle jamais au site (CORS, vie privée), et un site
/// injoignable rend une liste vide — jamais une erreur, la page Appareils
/// n'a pas à dépendre du réseau extérieur.
async fn get_device_presets(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let empty = || Json(json!({"presets": []})).into_response();
    let Some((brand, model)) = zone_identity_for_catalog(&state, id).await else {
        return empty();
    };
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten();
    let Ok(client) = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return empty();
    };
    let mut req = client
        .get("https://mozaiklabs.fr/api/v1/community/devices/presets")
        .query(&[("brand", brand.as_str()), ("model", model.as_str())]);
    if let Some(ot) = zone.as_ref().and_then(|z| z.output_type.clone()) {
        req = req.query(&[("output_type", ot)]);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => Json(v).into_response(),
            Err(_) => empty(),
        },
        Ok(r) => {
            tracing::debug!(status = %r.status(), "device_presets_fetch_non_success");
            empty()
        }
        Err(e) => {
            tracing::debug!(error = %e, "device_presets_fetch_failed");
            empty()
        }
    }
}

/// Partage les réglages de renderer d'une zone identifiée avec le catalogue
/// communautaire (#1743). Mêmes principes que push_device_correction :
/// anonyme, gaté télémétrie, best-effort en tâche de fond. Ne part que si
/// marque ET modèle sont connus et qu'au moins un réglage diffère des
/// défauts.
async fn push_device_preset(state: &AppState, zone_id: i64) {
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled() {
        return;
    }
    let Some((brand, model)) = zone_identity_for_catalog(state, zone_id).await else {
        return;
    };
    let settings_map = renderer_settings_snapshot(state, zone_id);
    if settings_map.is_empty() {
        return;
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let payload = json!({
        "brand": brand,
        "model": model,
        "output_type": zone.and_then(|z| z.output_type),
        "settings": Value::Object(settings_map),
    });
    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices/presets")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_preset_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_preset_push_failed"),
        }
    });
}

async fn push_device_correction(state: &AppState, zone_id: i64) {
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled() {
        return;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let brand = settings
        .get(&format!("zone_{zone_id}_brand"))
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty());
    let model = settings
        .get(&format!("zone_{zone_id}_model"))
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty());
    // L'un OU l'autre suffit. Exiger les deux écartait le cas le plus fréquent :
    // la marque seule est corrigée, parce que c'est elle que la déduction par OUI
    // se trompe, tandis que le modèle est généralement bien annoncé par
    // l'appareil. Ces corrections partielles ne partaient jamais, et le catalogue
    // communautaire — qui n'existe que pour les recueillir — s'en trouvait privé
    // de sa matière la plus courante.
    if brand.is_none() && model.is_none() {
        return;
    }

    let zone = match ZoneRepo::with_backend(state.backend.clone()).get(zone_id) {
        Ok(Some(z)) => z,
        _ => return,
    };
    let devices = state.scanner.devices().await;
    let detected = zone
        .output_device_id
        .as_deref()
        .and_then(|did| devices.iter().find(|d| d.id == did));

    // Le champ non corrigé part en chaîne vide et non en null : côté site, ces
    // colonnes entrent dans la clé d'unicité, où un null est « jamais égal » —
    // chaque renvoi créerait une ligne de plus au lieu d'incrémenter le compteur.
    let payload = json!({
        "detected_manufacturer": detected.and_then(|d| d.manufacturer.clone()),
        "detected_model": detected.and_then(|d| d.model.clone()),
        "brand": brand.unwrap_or_default(),
        "model": model.unwrap_or_default(),
        "output_type": zone.output_type,
    });

    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_correction_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_correction_push_failed"),
        }
    });
}

async fn create_zone(
    State(state): State<AppState>,
    Json(body): Json<CreateZone>,
) -> impl IntoResponse {
    let output_type = body.output_type.as_deref();

    // Une sortie locale s'identifie par `local:{nom}` — c'est ce préfixe, et
    // lui seul, qui dit à l'orchestrateur « carte son » plutôt que « renderer
    // réseau » (`orchestrator.rs`, une dizaine de `starts_with("local:")`).
    //
    // Un client qui envoie le nom nu crée donc une zone que rien ne peut
    // jouer : la lecture part sur le chemin réseau, télécharge la piste
    // entière, la décode, la ré-encode, puis pousse une URL vers un appareil
    // qui n'existe pas. Plus d'une minute d'attente, et aucun son (DEvir,
    // #1823). La zone échappe en prime au dédoublonnage, qui regroupe par
    // `output_device_id` : elle double la zone correcte du même appareil.
    //
    // On répare ici plutôt qu'au seul appelant : le serveur se met à jour
    // avant le client, et un client déjà installé continuerait sinon à créer
    // des zones mortes.
    let device_id_normalise = body.output_device_id.as_deref().map(|d| {
        if output_type == Some("local") && !d.starts_with("local:") {
            warn!(
                device_id = d,
                corrige = format!("local:{d}"),
                "create_zone_local_device_id_sans_prefixe_corrige"
            );
            format!("local:{d}")
        } else {
            d.to_string()
        }
    });
    let output_device_id = device_id_normalise.as_deref();

    // If device already has a zone (visible OR hidden), return it (no premium check needed).
    // A previously soft-deleted zone (is_hidden=1) is resurrected so the user's
    // prior settings (volume, DSP, gapless, etc.) are preserved.
    if let Some(device_id) = output_device_id {
        let repo = ZoneRepo::with_backend(state.backend.clone());
        if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
            if let Some(id) = existing.id {
                // Unhide if the zone was soft-deleted
                if repo.is_device_hidden(device_id) {
                    info!(
                        zone_id = id,
                        device_id, "unhiding_previously_deleted_zone_via_api"
                    );
                    let _ = repo.unhide(id);
                    // Update name in case device was renamed
                    let _ = repo.update_name(id, &body.name);
                    if let Some(ref ot) = body.output_type {
                        let _ = repo.update_output_type(id, ot);
                    }
                }
                let _ = repo.update_online(id, true);
                // Le contrat client AVEC l'etat REEL. Une zone qui existe deja
                // peut etre en train de jouer : lui coller `state: "stopped"`
                // serait un second mensonge apres le volume. `build_zone_json`
                // sait deja produire ce contrat — s'en servir evite une
                // troisieme copie a faire deriver (#2284, revue JP Robbe).
                let v = crate::routes::playback::build_zone_json(&state, id).await;
                // Une zone masquee qui reapparait est un evenement : sans
                // annonce, les autres clients connectes ne la voient qu'au
                // prochain refetch independant.
                state
                    .event_bus
                    .emit("zone.updated", json!({ "zone_id": id }));
                info!(zone_id = id, device_id, "zone_already_exists_returning");
                return (StatusCode::OK, Json(v)).into_response();
            }
        }
    }

    // The free-tier zone cap is enforced at *activation* (first play) in
    // orchestrator.play(), not at creation: creating/discovering a zone is
    // always allowed and the zone starts dormant. This avoids blocking a free
    // user from creating their actual renderer just because auto-discovered
    // zones filled the old count. See PlaybackOrchestrator::enforce_zone_cap.

    // For DLNA/OpenHome zones, ensure the output is registered before persisting
    if let Some(device_id) = output_device_id {
        let is_dlna = matches!(output_type, Some("dlna") | Some("openhome"));
        if is_dlna {
            let already_registered = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !already_registered {
                // Look up the discovered device and register its DLNA output
                let scanner = &state.scanner;
                let devices = scanner.devices().await;

                let disc = devices.iter().find(|d| d.id == device_id);
                if let Some(dev) = disc {
                    let registered = register_dlna_output_from_device(dev, &state).await;
                    if !registered {
                        warn!(device_id, "create_zone_output_registration_failed");
                    }
                } else {
                    warn!(device_id, "create_zone_device_not_discovered");
                }
            }
        }

        // For local audio zones, verify the device exists in the OutputRegistry
        if matches!(output_type, Some("local")) && device_id.starts_with("local:") {
            let found = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !found {
                warn!(device_id, "create_zone_local_device_not_found");
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"detail": format!("Local audio device not found: {device_id}. Make sure the device is connected and detected.")})),
                )
                    .into_response();
            }
        }

        // #1281 — même appareil physique, seconde identité SSDP (DLNA +
        // OpenHome, ou deux UUID : buchardt A700). La découverte regroupe déjà
        // par hôte (`zone_id_by_host`), mais CE chemin manuel ne dédoublonnait
        // que par `output_device_id` exact : créer une zone depuis l'entrée
        // jumelle du sélecteur produisait une deuxième zone pour le même
        // renderer — « I tried creating a zone and it duplicates ». L'hôte
        // vient du registre des sorties (rempli à la découverte) ; s'il porte
        // déjà une zone visible, on la rend au lieu d'en créer une autre.
        if is_dlna {
            let host = { state.outputs.lock().await.host_of(device_id) };
            if let Some(host) = host {
                let repo = ZoneRepo::with_backend(state.backend.clone());
                if let Some(existing_id) = repo.zone_id_by_host(&host) {
                    let _ = repo.update_online(existing_id, true);
                    // Même contrat client que les deux autres retours
                    // anticipés (#2284) : l'état RÉEL de la zone.
                    let v = crate::routes::playback::build_zone_json(&state, existing_id).await;
                    state
                        .event_bus
                        .emit("zone.updated", json!({ "zone_id": existing_id }));
                    info!(
                        zone_id = existing_id,
                        device_id,
                        host = %host,
                        "zone_same_host_already_exists_returning"
                    );
                    return (StatusCode::OK, Json(v)).into_response();
                }
            }
        }
    }

    // Duplicate device assignment already handled above (early return)

    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.create(&body.name, output_type, output_device_id) {
        Ok(id) => {
            info!(zone_id = id, name = %body.name, output_type = ?output_type, "zone_created");

            // Build the full zone object for both HTTP response and WS event
            let zone = repo.get(id).ok().flatten();
            let v =
                tune_core::db::zone_repo::zone_creee_contrat_client(zone.as_ref(), id, &body.name);

            // Emit with full zone data so clients can merge without re-fetching
            state.event_bus.emit(
                "zone.created",
                json!({
                    "id": id,
                    "zone": &v,
                }),
            );

            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) if e.contains("UNIQUE constraint failed") => {
            // Safety net: a hidden zone with this device_id blocked the INSERT.
            // Unhide it and return it instead of erroring.
            if let Some(device_id) = output_device_id {
                if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
                    if let Some(id) = existing.id {
                        warn!(
                            zone_id = id,
                            device_id, "unique_constraint_recovery_unhiding_zone"
                        );
                        let _ = repo.unhide(id);
                        let _ = repo.update_name(id, &body.name);
                        let _ = repo.update_online(id, true);
                        // Meme contrat, meme raison qu'au-dessus (#2284).
                        let v = crate::routes::playback::build_zone_json(&state, id).await;
                        state
                            .event_bus
                            .emit("zone.updated", json!({ "zone_id": id }));
                        return (StatusCode::OK, Json(v)).into_response();
                    }
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": e})),
        )
            .into_response(),
    }
}

/// POST /zones/{id}/renderer-capabilities — on-demand "discovery check" for the
/// renderer-config UI. Probes the zone's DLNA renderer via GetProtocolInfo and
/// returns which audio formats its `Sink` advertises (FLAC, WAV/LPCM 16 & 24,
/// ALAC/AAC, MP3, DSD), so the user can pick a sensible output override with
/// evidence. Only meaningful for dlna/openhome zones with a live renderer.
async fn renderer_capabilities(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone = match repo.get(id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "zone_not_found" })),
            )
                .into_response();
        }
    };

    if !matches!(zone.output_type.as_deref(), Some("dlna") | Some("openhome")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "not_a_dlna_renderer",
                "message": "Renderer capability discovery is only available for DLNA/OpenHome zones.",
            })),
        )
            .into_response();
    }

    let Some(device_id) = zone.output_device_id.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no_output_device" })),
        )
            .into_response();
    };

    // The GetProtocolInfo probe needs the registered DlnaOutput (it holds the
    // ConnectionManager URL). If the renderer hasn't been played yet it may not
    // be registered — try to register it from the discovered device first, same
    // as create_zone does, so the check works without playing a track first.
    let mut output = { state.outputs.lock().await.get(device_id) };
    if output.is_none() {
        let disc = {
            let scanner = &state.scanner;
            let devices = scanner.devices().await;
            devices.iter().find(|d| d.id == device_id).cloned()
        };
        if let Some(dev) = disc {
            register_dlna_output_from_device(&dev, &state).await;
            output = state.outputs.lock().await.get(device_id);
        }
    }

    let Some(output) = output else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "probed": false,
                "reason": "renderer_offline",
            })),
        )
            .into_response();
    };

    // Hold the output lock for the SOAP round-trip (on-demand, user-initiated,
    // rare) — same pattern the orchestrator uses for its per-track probe.
    let caps = {
        let guard = output.lock().await;
        match guard.as_any().downcast_ref::<DlnaOutput>() {
            Some(dlna) => dlna.probe_capabilities().await,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "not_a_dlna_output" })),
                )
                    .into_response();
            }
        }
    };

    // Une sonde reussie est un fait d'interet communautaire : quel format cet
    // appareil annonce-t-il vraiment ? Remontee anonyme, apres la reponse a
    // l'UI (spawn best-effort), jamais pour une sonde vide (`probed: false`,
    // qui ne dit rien de l'appareil).
    if caps.probed {
        push_device_caps(&state, id, &caps).await;
    }

    Json(json!(caps)).into_response()
}

/// Partage le resultat du « Verifier le renderer » avec le catalogue
/// communautaire. La sonde GetProtocolInfo tourne sur le LAN de l'utilisateur
/// — le site ne peut pas interroger un appareil derriere une box ; seul le
/// RESULTAT peut voyager. Agrege par appareil cote site, c'est le rapport de
/// verification consolide sur le parc. Memes principes que
/// push_device_preset : anonyme, gate telemetrie, best-effort en tache de
/// fond, et ne part que si marque ET modele sont connus.
async fn push_device_caps(
    state: &AppState,
    zone_id: i64,
    caps: &tune_core::outputs::dlna::RendererCapabilities,
) {
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled() {
        return;
    }
    let Some((brand, model)) = zone_identity_for_catalog(state, zone_id).await else {
        return;
    };
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    // Les drapeaux seulement : `probed` est un etat de la sonde (garanti true
    // ici) et `sink` du debogage local qui n'a pas a voyager.
    let payload = json!({
        "brand": brand,
        "model": model,
        "output_type": zone.and_then(|z| z.output_type),
        "caps": {
            "flac": caps.flac,
            "wav": caps.wav,
            "lpcm16": caps.lpcm16,
            "lpcm24": caps.lpcm24,
            "alac": caps.alac,
            "aac": caps.aac,
            "mp3": caps.mp3,
            "dsd": caps.dsd,
        },
    });
    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices/caps")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_caps_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_caps_push_failed"),
        }
    });
}

/// Register a DLNA output from a discovered device.
/// Fetches the device description XML to find AVTransport/RenderingControl URLs,
/// then registers the output in the global registry.
/// Returns true if registration succeeded.
async fn register_dlna_output_from_device(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    state: &AppState,
) -> bool {
    // First, try to get service URLs from the device's cached capabilities
    let svc_urls = dev
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();

    let av_url = svc_urls
        .get("avtransport")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let rc_url = svc_urls
        .get("renderingcontrol")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let cm_url = svc_urls
        .get("connectionmanager")
        .or_else(|| svc_urls.get("ConnectionManager"))
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));

    // If cached service URLs are available, use them
    if let (Some(av), Some(rc)) = (av_url, rc_url) {
        let delay =
            crate::config::resolve_play_delay(&state.backend, &state.config, &dev.id, &dev.name);
        let evt_urls = dev
            .capabilities
            .get("event_sub_urls")
            .and_then(|v| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
            })
            .unwrap_or_default();
        let dlna = DlnaOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            av,
            rc,
            cm_url,
        )
        .with_play_delay(delay)
        .with_upnp_events(
            crate::startup::create_oh_listener().await,
            crate::discovery_setup::urls_evenements_dlna(&dev.host, dev.port, &evt_urls),
        )
        .with_upnp_silence(crate::config::resolve_upnp_silence(&state.backend, &dev.id));
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(dlna));
        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_on_zone_create");
        return true;
    }

    // Fallback: fetch device description from location URL
    if let Some(ref location) = dev.location {
        match fetch_device_description(location).await {
            Ok(desc) => {
                if desc.is_media_renderer() || desc.is_openhome() {
                    let service_urls = desc.service_urls();
                    let av = service_urls.get("avtransport");
                    let rc = service_urls.get("renderingcontrol");
                    if let (Some(av_path), Some(rc_path)) = (av, rc) {
                        let base = format!("http://{}:{}", dev.host, dev.port);
                        let cm_path = service_urls
                            .get("connectionmanager")
                            .or_else(|| service_urls.get("ConnectionManager"))
                            .map(|p| format!("{base}{p}"));
                        let delay = crate::config::resolve_play_delay(
                            &state.backend,
                            &state.config,
                            &dev.id,
                            &dev.name,
                        );
                        let dlna = DlnaOutput::new(
                            dev.name.clone(),
                            dev.id.clone(),
                            dev.host.clone(),
                            format!("{base}{av_path}"),
                            format!("{base}{rc_path}"),
                            cm_path,
                        )
                        .with_play_delay(delay)
                        .with_upnp_events(
                            crate::startup::create_oh_listener().await,
                            crate::discovery_setup::urls_evenements_dlna(
                                &dev.host,
                                dev.port,
                                &desc.event_sub_urls(),
                            ),
                        )
                        .with_upnp_silence(
                            crate::config::resolve_upnp_silence(&state.backend, &dev.id),
                        );
                        let mut outputs = state.outputs.lock().await;
                        outputs.register(Box::new(dlna));
                        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_via_description");
                        return true;
                    }
                }
            }
            Err(e) => {
                warn!(device = %dev.name, error = %e, "dlna_description_fetch_failed");
            }
        }
    }

    false
}

/// DELETE /zones — soft-delete every zone and clear the free-tier
/// activation markers, so a Free user whose 3-zone quota is consumed by
/// stale renderers can start over and explicitly re-create the zones he
/// wants (discovery never resurrects hidden zones, only POST /zones does).
async fn delete_all_zones(State(state): State<AppState>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let ids: Vec<i64> = repo
        .list()
        .map(|zs| zs.iter().filter_map(|z| z.id).collect())
        .unwrap_or_default();
    match repo.delete_all() {
        Ok(_) => {
            info!(count = ids.len(), "all_zones_deleted_quota_reset");
            for id in ids {
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::ZoneDeleted,
                    json!({"id": id}),
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_zone(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneDeleted,
                json!({"id": id}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateVolume>,
) -> impl IntoResponse {
    // Normalise: web client sends 0.0–1.0, legacy clients may send 0–100.
    let lineaire = body.volume.map(|v| if v > 1.0 { v / 100.0 } else { v });
    // #1274 — l'arbitrage `volume` / `volume_db` et la conversion des dB
    // vivent dans `volume_scale`, pas ici. Cette route ne fait que ramener sa
    // convention historique sur 0..1 avant de la lui passer.
    let volume_f = match tune_core::audio::volume_scale::demande_lineaire(lineaire, body.volume_db)
    {
        Ok(v) => v,
        Err(motif) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_volume", "message": motif })),
            )
                .into_response();
        }
    };
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    // #1274 — la consigne en dB doit avoir un endroit où arriver. Si la
    // sortie de la zone ne parle au périphérique qu'en entiers, un dB sous son
    // premier pas ne baisse pas le son : il l'éteint. On le refuse en le
    // nommant, plutôt que de répondre 204 sur un silence.
    if let Some(db) = body.volume_db
        && let Some(motif) = refus_de_resolution_volume(&state, device_id.as_deref(), db).await
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "volume_db_hors_resolution", "message": motif })),
        )
            .into_response();
    }

    match state
        .orchestrator
        .set_volume(id, volume_f, device_id.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => crate::routes::playback::output_command_error_response(error),
    }
}

async fn update_muted(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMuted>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    match state
        .orchestrator
        .set_mute(id, body.muted, device_id.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => crate::routes::playback::output_command_error_response(error),
    }
}

async fn rename_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.update_name(id, &body.name) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneUpdated,
                json!({ "id": id, "name": body.name }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateGroup {
    /// Optional. The web client groups a *selection* of zones and has no name
    /// to offer, so it never sent this field; when it was mandatory serde
    /// rejected the whole body and axum answered a bare `422 Unprocessable
    /// Entity` with no text at all — the unexplained code the tester saw
    /// (#1702). Absent or blank, the group is named after its zones.
    #[serde(default)]
    name: Option<String>,
    zone_ids: Vec<i64>,
    /// The zone the others follow. Sent by the web client; defaults to the
    /// first zone of the selection.
    #[serde(default)]
    leader_id: Option<i64>,
}

/// Why a set of zones cannot form a group.
///
/// Kept separate from the HTTP layer so the rules can be unit-tested without a
/// database, and so every refusal is forced to carry the words explaining it.
#[derive(Debug, PartialEq)]
enum GroupRefusal {
    /// Fewer than two *distinct* zones.
    NotEnoughZones,
    /// A zone id that no longer exists (stale list on the client's side).
    UnknownZone(i64),
    /// Two zones bound to the same output. Tune creates one zone per
    /// discovered output and duplicates do happen ("PC" and "Haut parleurs"
    /// on one sound card, #1702): grouping them would send the same stream
    /// twice to a single device. Both names travel with the refusal so the
    /// message can say *which* zones clash.
    SameOutput(String, String),
}

/// Check a grouping request against the zones that actually exist.
///
/// Returns the de-duplicated zone ids, in request order, when the group is
/// legitimate.
fn validate_group(zone_ids: &[i64], zones: &[Zone]) -> Result<Vec<i64>, GroupRefusal> {
    let mut unique: Vec<i64> = Vec::new();
    for &id in zone_ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if unique.len() < 2 {
        return Err(GroupRefusal::NotEnoughZones);
    }

    // (output device, name of the zone that claimed it first)
    let mut claimed: Vec<(&str, &str)> = Vec::new();
    for &id in &unique {
        let zone = zones
            .iter()
            .find(|z| z.id == Some(id))
            .ok_or(GroupRefusal::UnknownZone(id))?;
        // A zone with no output device is an orphan, not a duplicate: several
        // of them share "nothing", which is not the same output.
        let Some(device) = zone.output_device_id.as_deref().filter(|d| !d.is_empty()) else {
            continue;
        };
        if let Some((_, first)) = claimed.iter().find(|(d, _)| *d == device) {
            return Err(GroupRefusal::SameOutput(
                (*first).to_string(),
                zone.name.clone(),
            ));
        }
        claimed.push((device, zone.name.as_str()));
    }
    Ok(unique)
}

/// Turn a refusal into the error envelope the web client understands:
/// `error` is the machine code, `message` the sentence it displays.
fn group_refusal_response(refusal: &GroupRefusal, lang: &str) -> axum::response::Response {
    let (status, code, message) = match refusal {
        GroupRefusal::NotEnoughZones => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "group_needs_two_zones",
            crate::i18n::t(lang, "zonegroup.needsTwoZones"),
        ),
        GroupRefusal::UnknownZone(id) => (
            StatusCode::NOT_FOUND,
            "group_unknown_zone",
            crate::i18n::t(lang, "zonegroup.unknownZone").replace("{id}", &id.to_string()),
        ),
        GroupRefusal::SameOutput(a, b) => (
            StatusCode::CONFLICT,
            "group_same_output",
            crate::i18n::t(lang, "zonegroup.sameOutput")
                .replace("{a}", a)
                .replace("{b}", b),
        ),
    };
    warn!(code, message = %message, "zone_group_refused");
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Groups stored before #1702 have no `group_id`. The web client keys its
    // "dissolve" button on that field and was putting `undefined` in the URL,
    // so an existing group could not be undone. Derive it from `id` on read.
    for group in &mut groups {
        if group.get("group_id").is_none()
            && let Some(id) = group.get("id").and_then(|v| v.as_i64())
        {
            group["group_id"] = json!(id.to_string());
        }
    }
    Json(json!(groups))
}

async fn create_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateGroup>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: Multiroom sync requires Premium
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::MultiroomSync,
    )
    .await
    {
        return Ok(resp);
    }

    let lang = crate::i18n::lang_from_header(&headers);
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let zone_ids = match validate_group(&body.zone_ids, &zones) {
        Ok(ids) => ids,
        Err(refusal) => return Ok(group_refusal_response(&refusal, &lang)),
    };

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            zone_ids
                .iter()
                .filter_map(|id| zones.iter().find(|z| z.id == Some(*id)))
                .map(|z| z.name.as_str())
                .collect::<Vec<_>>()
                .join(" + ")
        });
    let leader_id = body
        .leader_id
        .filter(|id| zone_ids.contains(id))
        .unwrap_or(zone_ids[0]);

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = groups.len() as i64 + 1;
    let group = json!({
        "id": id,
        "group_id": id.to_string(),
        "name": name,
        "leader_id": leader_id,
        "zone_ids": zone_ids,
    });
    groups.push(group.clone());

    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupCreated,
        json!({ "id": id, "name": name, "zone_ids": zone_ids }),
    );
    Ok((StatusCode::CREATED, Json(group)).into_response())
}

#[derive(Deserialize)]
struct PatchGroup {
    name: Option<String>,
    zone_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
struct GroupVolumeRequest {
    master_volume: Option<f64>,
    offsets: Option<std::collections::HashMap<String, f64>>,
}

async fn patch_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Json(body): Json<PatchGroup>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match idx {
        Some(i) => {
            if let Some(ref name) = body.name {
                groups[i]["name"] = json!(name);
            }
            if let Some(ref zone_ids) = body.zone_ids {
                groups[i]["zone_ids"] = json!(zone_ids);
            }
            let result = groups[i].clone();
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::GroupUpdated,
                json!({ "id": group_id, "group": result }),
            );
            Ok(Json(result).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn group_volume(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Json(body): Json<GroupVolumeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match idx {
        Some(i) => {
            let master = body
                .master_volume
                .unwrap_or(groups[i]["master_volume"].as_f64().unwrap_or(0.5));
            groups[i]["master_volume"] = json!(master);
            if let Some(ref offsets) = body.offsets {
                groups[i]["offsets"] = json!(offsets);
            }
            let zone_ids: Vec<i64> = groups[i]["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();

            for zid in &zone_ids {
                let offset = body
                    .offsets
                    .as_ref()
                    .and_then(|o| o.get(&zid.to_string()))
                    .copied()
                    .unwrap_or(0.0);
                let effective = (master + offset).clamp(0.0, 1.0);
                let device_id = ZoneRepo::with_backend(state.backend.clone())
                    .get(*zid)
                    .ok()
                    .flatten()
                    .and_then(|zone| zone.output_device_id);
                if let Err(error) = state
                    .orchestrator
                    .set_volume(*zid, effective, device_id.as_deref())
                    .await
                {
                    return Ok(crate::routes::playback::output_command_error_response(
                        error,
                    ));
                }
            }
            Ok(Json(json!({"group_id": group_id, "master_volume": master})).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn calibrate_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(_) => crate::routes::zone_manager::audio_calibration_unavailable(group_id),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn group_health(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(group) => {
            let zone_ids: Vec<i64> = group["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            let repo = ZoneRepo::with_backend(state.backend.clone());
            let mut zones_health = Vec::new();
            for zid in &zone_ids {
                let ps = state.playback.get_state(*zid).await;
                let zone = repo.get(*zid).ok().flatten();
                let name = zone
                    .map(|z| z.name)
                    .unwrap_or_else(|| format!("Zone {zid}"));
                let online =
                    ps.state != tune_core::playback::PlayState::Stopped || ps.now_playing.is_some();
                zones_health.push(json!({
                    "zone_id": zid,
                    "name": name,
                    "status": if online { "online" } else { "offline" },
                }));
            }
            Json(json!(zones_health)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    groups.retain(|g| g.get("id").and_then(|v| v.as_i64()) != Some(group_id));
    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupDeleted,
        json!({ "id": group_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn list_stereo_pairs(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(pairs))
}

#[derive(Deserialize)]
struct CreateStereoPair {
    name: String,
    left_device_id: String,
    right_device_id: String,
}

async fn create_stereo_pair(
    State(state): State<AppState>,
    Json(body): Json<CreateStereoPair>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = pairs.len() as i64 + 1;
    pairs.push(json!({
        "id": id,
        "name": body.name,
        "left_device_id": body.left_device_id,
        "right_device_id": body.right_device_id,
    }));

    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

async fn delete_stereo_pair(
    State(state): State<AppState>,
    Path(pair_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    pairs.retain(|p| p.get("id").and_then(|v| v.as_i64()) != Some(pair_id));
    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn list_group_delays(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let raw = settings
        .get("group_delays")
        .unwrap_or(None)
        .unwrap_or_default();
    let delays: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    Json(json!(delays))
}

async fn set_group_delay(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut delays: Vec<Value> = settings
        .get("group_delays")
        .unwrap_or(None)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let tech_a = body.get("tech_a").and_then(|v| v.as_str()).unwrap_or("");
    let tech_b = body.get("tech_b").and_then(|v| v.as_str()).unwrap_or("");
    let delay_ms = body.get("delay_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    delays.retain(|d| {
        !(d.get("tech_a").and_then(|v| v.as_str()) == Some(tech_a)
            && d.get("tech_b").and_then(|v| v.as_str()) == Some(tech_b))
    });
    delays.push(json!({"tech_a": tech_a, "tech_b": tech_b, "delay_ms": delay_ms}));
    settings
        .set(
            "group_delays",
            &serde_json::to_string(&delays).unwrap_or_default(),
        )
        .ok();
    Json(json!(delays))
}

#[cfg(test)]
mod signal_path_tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::playback::NowPlaying;

    fn dlna_zone() -> (Arc<dyn DbBackend>, Zone) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // Ces contrats exercent les réglages DLNA ajoutés par migration. Sans
        // migration, ils restaient faussement verts tant que les écritures sur
        // une colonne absente étaient silencieusement ignorées (#2154).
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        (backend, zone)
    }

    // Hi-res ALAC source, currently playing, with a live stream session.
    fn alac_hires_playing() -> ZoneState {
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("alac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        }
    }

    /// Décrit un fil réel : conteneur + fréquence + profondeur effectivement
    /// servies. Passer 0 en fréquence ou profondeur simule une session qui ne
    /// les connaît pas encore — l'affichage doit alors retomber sur les règles.
    fn wire(format: &str, sample_rate: u32, bit_depth: u16) -> StreamInfo {
        StreamInfo {
            format: format.into(),
            sample_rate,
            bit_depth,
            ..Default::default()
        }
    }

    fn step_desc(v: &Value, name: &str) -> Option<String> {
        v.get("steps")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|s| s.get("description").and_then(|d| d.as_str()))
            .map(String::from)
    }

    fn step_detail(v: &Value, name: &str) -> Option<String> {
        v.get("steps")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|s| s.get("detail").and_then(|d| d.as_str()))
            .map(String::from)
    }

    /// #2074 — le message que voit l'utilisateur.
    ///
    /// Bandcamp ne sert que du `mp3-128` en écoute libre, et la règle écrite
    /// dans `plugins/tune-bandcamp/src/lib.rs` veut que ce débit soit
    /// « annoncé comme tel PARTOUT où il apparaît ». Il l'était sur l'écran
    /// Bandcamp et NULLE PART ailleurs : arrivée dans une zone, la piste
    /// s'affichait « MP3 44kHz/16bit », indiscernable d'un 320 devant un DAC
    /// de salon.
    #[test]
    fn a_lossy_source_announces_its_bitrate_in_the_signal_path() {
        let (backend, zone) = dlna_zone();
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Un extrait".into(),
                source: "bandcamp".into(),
                format: Some("mp3".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                bitrate_kbps: Some(128),
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        };

        let sp = build_signal_path(&ps, &zone, &backend, Some("Marantz"), "", None).unwrap();

        assert_eq!(
            step_desc(&sp, "Source").as_deref(),
            Some("MP3 128 kbit/s 44kHz/16bit"),
            "le débit doit être lisible AVANT que le son n'atteigne le DAC"
        );
        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    }

    /// #2074, cas de l'ACHAT — le pendant du test précédent.
    ///
    /// La règle porte sur la qualité réelle du flux, jamais sur la source
    /// « Bandcamp » en bloc : un album acheté descend en FLAC par la même
    /// porte, et lui coller « 128 kbit/s » serait le même mensonge dans
    /// l'autre sens.
    #[test]
    fn a_lossless_source_announces_no_bitrate() {
        let (backend, zone) = dlna_zone();
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Un album acheté".into(),
                source: "bandcamp".into(),
                format: Some("flac".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                bitrate_kbps: None,
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        };

        let sp = build_signal_path(&ps, &zone, &backend, Some("Marantz"), "", None).unwrap();

        assert_eq!(
            step_desc(&sp, "Source").as_deref(),
            Some("FLAC 44kHz/16bit"),
            "aucun débit ne doit apparaître sur un flux sans perte"
        );
    }

    /// #2212 — le chemin du signal nomme le pré-gain qui prévient les overs,
    /// et ne présente plus l'ancien saturateur implicite comme une protection.
    /// Une zone servie par une sortie PULL hors dépôt — le cas `diretta`.
    fn diretta_zone() -> (Arc<dyn DbBackend>, Zone) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo
            .create("Diretta", Some("diretta"), Some("diretta-1"))
            .unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        (backend, zone)
    }

    /// Un égaliseur ARMÉ sur la zone, écrit là où le chemin audio le lit.
    fn armer_l_eq(backend: &Arc<dyn DbBackend>, zone_id: i64) {
        let profile = tune_core::audio::eq::EqProfile {
            enabled: true,
            bands: vec![tune_core::audio::eq::EqBandSpec {
                gain: 6.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        SettingsRepo::with_backend(backend.clone())
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&profile).unwrap(),
            )
            .unwrap();
    }

    /// Source DSD128 en lecture, avec une session vivante.
    fn dsd_playing() -> ZoneState {
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Locatelli".into(),
                format: Some("dsf".into()),
                sample_rate: Some(5_644_800),
                bit_depth: Some(1),
                stream_id: Some("sid-dsd".into()),
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        }
    }

    /// #1393 — le panneau annonçait un égaliseur qui n'a PAS lieu.
    ///
    /// Eric (fil forum, Windows 0.9.61) : « l'égaliseur ne fait rien » sur un
    /// renderer Diretta et sur un PC vu comme zone DLNA. Le versant audible du
    /// cas PCM a été corrigé par #1430 (`pull_output_needs_dsp_transcode` force
    /// le chemin transcodé pour une sortie pull). Ce même correctif s'ABSTIENT
    /// délibérément sur le DSD natif — convertir un flux DSD en PCM pour y
    /// passer un EQ serait une dégradation décidée à la place de l'auditeur.
    ///
    /// Le chemin du signal, lui, ne connaissait pas cette abstention : il lisait
    /// `configured_dsp_enabled` — le RÉGLAGE en base — et affichait « EQ actif »
    /// pour un traitement qui n'existe pas, en faisant au passage tomber le
    /// verdict bit-perfect d'un fil que personne n'a touché. C'est la faute de
    /// #1315 et #2053 : ne pas annoncer ce qui n'a pas lieu.
    ///
    /// L'étape n'est pas SUPPRIMÉE : la faire disparaître laisserait l'auditeur
    /// devant le même curseur inerte, sans explication. Elle dit ce qui est.
    #[test]
    fn un_eq_arme_sur_du_dsd_brut_est_annonce_contourne_et_non_applique() {
        let (backend, zone) = diretta_zone();
        armer_l_eq(&backend, zone.id.unwrap());

        // Le fil porte le .dsf tel quel : c'est CONSTATÉ, pas déduit.
        let sp = build_signal_path(
            &dsd_playing(),
            &zone,
            &backend,
            Some("Diretta Host"),
            "",
            Some(&wire("dsf", 5_644_800, 1)),
        )
        .unwrap();

        assert_eq!(
            step_desc(&sp, "DSP").as_deref(),
            Some("DSP contourné (DSD natif servi brut)"),
            "un EQ que l'orchestrateur n'applique pas ne doit pas être annoncé actif"
        );
        let etape_dsp = sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "DSP")
            .unwrap();
        assert_eq!(
            etape_dsp["bit_perfect"].as_bool(),
            Some(true),
            "rien n'a touché le flux : l'étape ne doit pas se déclarer dégradante"
        );
    }

    /// CONTRE-ÉPREUVE de l'essai ci-dessus, et elle est PERMANENTE.
    ///
    /// Même zone `diretta`, même égaliseur armé, seul le FIL change : du FLAC au
    /// lieu du DSD brut. Là, `pull_output_needs_dsp_transcode` force bien le
    /// transcodage et l'EQ est réellement appliqué — le panneau doit donc
    /// l'annoncer actif, et le verdict bit-perfect doit tomber.
    ///
    /// Sans cette moitié, une garde trop large — « ne jamais annoncer le DSP
    /// hors sortie locale » — laisserait la première verte tout en rendant le
    /// panneau muet sur le cas d'Eric qui, lui, est bel et bien traité.
    #[test]
    fn le_meme_eq_sur_un_fil_pcm_reste_annonce_applique() {
        let (backend, zone) = diretta_zone();
        armer_l_eq(&backend, zone.id.unwrap());

        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Locatelli".into(),
                format: Some("flac".into()),
                sample_rate: Some(96_000),
                bit_depth: Some(24),
                stream_id: Some("sid-pcm".into()),
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        };

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Diretta Host"),
            "",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        let dsp = step_desc(&sp, "DSP").expect("l'étape DSP doit rester présente sur du PCM");
        assert!(
            dsp.starts_with("EQ actif"),
            "sur un fil PCM l'EQ est réellement appliqué : {dsp}"
        );
        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn eq_step_exposes_per_channel_headroom_and_no_limiter() {
        let (backend, zone) = dlna_zone();
        let zone_id = zone.id.unwrap();
        let profile = tune_core::audio::eq::EqProfile {
            enabled: true,
            bands: vec![
                tune_core::audio::eq::EqBandSpec {
                    gain: 6.0,
                    channel: None,
                    ..Default::default()
                },
                tune_core::audio::eq::EqBandSpec {
                    gain: 3.0,
                    channel: Some(0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        SettingsRepo::with_backend(backend.clone())
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&profile).unwrap(),
            )
            .unwrap();

        let sp = build_signal_path(
            &alac_hires_playing(),
            &zone,
            &backend,
            Some("Marantz"),
            "",
            Some(&wire("alac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(
            step_desc(&sp, "DSP").as_deref(),
            Some("EQ actif (pré-gain auto G -9.0 dB / D -6.0 dB, sans limiteur)")
        );
        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    }

    /// #2205/#2233 : le backend Windows connaît déjà le verdict exact à la
    /// frontière du callback. Le chemin public doit le croire plutôt que de
    /// continuer à déclarer statiquement toute sortie locale bit-perfect.
    #[test]
    fn local_signal_path_uses_the_runtime_backend_contract_and_its_reason() {
        use tune_core::outputs::traits::{
            OutputDspMetrics, OutputDspState, OutputSampleTransport, OutputSignalPathStatus,
            OutputSignalReason, OutputVolumeState,
        };

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo
            .create("DAC", Some("local"), Some("local:dac"))
            .unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        let mut ps = wav24_playing();
        ps.output_signal_path = Some(OutputSignalPathStatus {
            bit_perfect: false,
            sample_transport: OutputSampleTransport::Float,
            dsp: OutputDspState::Applied,
            volume: OutputVolumeState::Unity,
            reasons: vec![
                OutputSignalReason::FloatTransport,
                OutputSignalReason::DspApplied,
            ],
        });
        ps.output_dsp_metrics = Some(OutputDspMetrics {
            eq_overs: 17,
            eq_non_finite_samples: 2,
        });

        let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "ASIO", None).unwrap();

        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
        assert_eq!(
            sp.get("runtime_observed").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            sp.get("runtime_reasons"),
            Some(&json!(["float_transport", "dsp_applied"]))
        );
        assert_eq!(
            step_detail(&sp, "Transport").as_deref(),
            Some("Transport flottant imposé par le callback ; DSP appliqué")
        );
        assert_eq!(step_desc(&sp, "DSP").as_deref(), Some("DSP appliqué"));
        assert_eq!(sp["dsp_metrics"]["eq_overs"], 17);
        assert_eq!(sp["dsp_metrics"]["eq_non_finite_samples"], 2);
        assert_eq!(
            sp["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|step| step["name"] == "DSP")
                .unwrap()["metrics"]["eq_overs"],
            17
        );
    }

    /// Monte une zone locale Windows dont la sonde a publié `reasons`.
    fn local_runtime_zone(
        volume_percent: f64,
        volume: tune_core::outputs::traits::OutputVolumeState,
        reasons: Vec<OutputSignalReason>,
    ) -> (Zone, ZoneState, std::sync::Arc<dyn DbBackend>) {
        use tune_core::outputs::traits::{OutputSampleTransport, OutputSignalPathStatus};

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo
            .create("DAC", Some("local"), Some("local:dac"))
            .unwrap();
        let mut zone = repo.get(id).unwrap().unwrap();
        zone.volume = volume_percent;

        let mut ps = wav24_playing();
        ps.output_signal_path = Some(OutputSignalPathStatus {
            // Le producteur a bien quitté la branche brute : ce buffer est
            // passé par le flottant pour appliquer le facteur de volume.
            bit_perfect: false,
            sample_transport: OutputSampleTransport::NativeInteger,
            dsp: OutputDspState::Inactive,
            volume,
            reasons,
        });
        (zone, ps, backend)
    }

    /// #2053 — « Lecture annoncée comme transcodée alors que je ne pense pas
    /// avoir paramétré cela » (Tades, Windows).
    ///
    /// Le client n'a que deux mots pour ce champ : « Bit-perfect » ou
    /// « Transcodé » (`NowPlaying.svelte`). Tout ce qui n'est pas bit-perfect
    /// s'affiche donc comme un transcodage — y compris quand aucune conversion
    /// n'a lieu. Depuis la sonde Windows, un simple curseur de volume à 85 %
    /// suffisait à déclencher ce mot, sur une zone où rien n'a été paramétré.
    ///
    /// La règle inverse est écrite dans `build_signal_path` depuis #1627
    /// (« Volume is excluded — it's a user preference, not a signal
    /// degradation ») et reste appliquée à toutes les autres sorties et à
    /// toutes les autres plateformes. Elle vaut aussi ici.
    #[test]
    fn software_volume_alone_does_not_announce_a_transcode() {
        let (zone, ps, backend) = local_runtime_zone(
            85.0,
            OutputVolumeState::Applied,
            vec![OutputSignalReason::SoftwareVolume],
        );

        let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(true));
        // Rien n'est caché : l'étape reste là, avec son pourcentage, et la
        // cause reste nommée dans le contrat d'exécution.
        assert_eq!(
            step_desc(&sp, "Volume").as_deref(),
            Some("Volume logiciel 85%")
        );
        assert_eq!(
            sp["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|step| step["name"] == "Volume")
                .unwrap()["bit_perfect"],
            json!(true)
        );
        assert_eq!(sp.get("runtime_reasons"), Some(&json!(["software_volume"])));
        assert_eq!(
            step_detail(&sp, "Transport").as_deref(),
            Some("Volume logiciel appliqué")
        );
        assert!(!sp["summary"].as_str().unwrap().contains("transcode"));
    }

    /// Contre-épreuve : l'exemption ne vaut QUE pour le volume seul. Dès qu'une
    /// autre cause s'ajoute, le verdict du producteur reste négatif — on ne
    /// relève jamais son verdict en promesse de pureté.
    #[test]
    fn a_second_cause_beside_volume_keeps_the_negative_verdict() {
        let (zone, ps, backend) = local_runtime_zone(
            85.0,
            OutputVolumeState::Applied,
            vec![
                OutputSignalReason::FloatTransport,
                OutputSignalReason::SoftwareVolume,
            ],
        );

        let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
        assert_eq!(
            step_detail(&sp, "Transport").as_deref(),
            Some("Transport flottant imposé par le callback ; Volume logiciel appliqué")
        );
    }

    /// Et un verdict négatif SANS raison nommée n'est pas non plus relevé :
    /// l'exemption exige la liste explicite, jamais une liste vide.
    #[test]
    fn an_unexplained_negative_verdict_is_never_upgraded() {
        let (zone, ps, backend) = local_runtime_zone(85.0, OutputVolumeState::Applied, vec![]);

        let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    }

    // ------------------------------------------------------------------
    // Garde-fou : le fil prime, quelles que soient les combinaisons.
    //
    // Ce module a une raison d'être précise. `build_signal_path` rejouait les
    // décisions de l'orchestrateur pour deviner ce qui partait sur le réseau, si
    // bien que chaque évolution du chemin audio devait être répliquée ici à la
    // main. Le même bug est revenu six fois sous des formes différentes
    // (ALAC→FLAC fantôme, cap 16 bits, WAV 24, égaliseur ignoré) parce qu'on
    // ajoutait un miroir de plus à chaque fois, sans jamais supprimer la cause.
    //
    // Le test ci-dessous ne simule PAS l'orchestrateur — ce serait un faux
    // garde-fou, qui ne ferait que dupliquer une troisième fois les mêmes
    // règles. Il verrouille l'invariant qui rend les miroirs inoffensifs :
    // **quand la session de flux renseigne le format réellement servi, c'est lui
    // qui s'affiche, et aucun réglage de zone ne peut le contredire.**
    //
    // Concrètement : si quelqu'un rajoute demain une règle qui écrase la valeur
    // du fil, ce test casse, et il casse en nommant la combinaison fautive.
    #[test]
    fn wire_always_wins_over_every_zone_flag_combination() {
        // Source hi-res ALAC, fil réellement servi en WAV 96 kHz / 24 bits.
        // Plusieurs de ces réglages « voudraient » plafonner à 16 bits.
        let served = wire("wav", 96_000, 24);
        let expected = "ALAC 96kHz/24bit \u{2192} WAV 96kHz/24bit";

        for lpcm in [false, true] {
            for cap16 in [false, true] {
                for wav24 in [false, true] {
                    for alac_direct in [false, true] {
                        let (backend, zone) = dlna_zone();
                        let repo = ZoneRepo::with_backend(backend.clone());
                        let id = zone.id.unwrap();
                        repo.update_dlna_lpcm(id, lpcm).unwrap();
                        repo.update_dlna_cap_16bit(id, cap16).unwrap();
                        repo.update_dlna_wav24(id, wav24).unwrap();
                        repo.update_alac_passthrough(id, alac_direct).unwrap();
                        let zone = repo.get(id).unwrap().unwrap();

                        let sp = build_signal_path(
                            &alac_hires_playing(),
                            &zone,
                            &backend,
                            Some("darTZeel LHC-208"),
                            "none",
                            Some(&served),
                        )
                        .unwrap();

                        // Une combinaison peut légitimement ne pas afficher
                        // d'étape Transcodeur ; ce qui ne se pardonne pas, c'est
                        // d'en afficher une qui contredise le fil.
                        if let Some(desc) = transcoder_desc(&sp) {
                            assert_eq!(
                                desc, expected,
                                "lpcm={lpcm} cap16={cap16} wav24={wav24} alac_direct={alac_direct} : \
                                 l'affichage contredit le fil reellement servi"
                            );
                        }
                    }
                }
            }
        }
    }

    // Second invariant, complémentaire : le CONTENEUR affiché est celui du fil.
    // C'est le bug d'origine de Sevy (#1043) — le fil était en WAV et le chemin
    // annonçait FLAC — remis sous test de façon systématique.
    #[test]
    fn wire_container_is_never_contradicted() {
        for (container, label) in [("wav", "WAV"), ("flac", "FLAC")] {
            let (backend, zone) = dlna_zone();
            let sp = build_signal_path(
                &alac_hires_playing(),
                &zone,
                &backend,
                Some("Eversolo DMP-A10"),
                "none",
                Some(&wire(container, 96_000, 24)),
            )
            .unwrap();
            if let Some(desc) = transcoder_desc(&sp) {
                assert!(
                    desc.contains(label),
                    "fil={container} mais l'affichage dit: {desc}"
                );
            }
        }
    }

    fn transcoder_desc(v: &Value) -> Option<String> {
        v.get("steps")?
            .as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("Transcoder"))
            .and_then(|s| s.get("description").and_then(|d| d.as_str()))
            .map(String::from)
    }

    // ------------------------------------------------------------------
    // #1315 — l'affichage DSD sur Eversolo.
    //
    // Yves Corbat le 08/08, Stéphane Villerio le 28/08 avec les trois pièces :
    // un DMP-A6 en DLNA, mode audiophile, volume figé à 100 %, une piste
    // DSD128. Le panneau affichait un étage « DSD128 5.6 MHz → FLAC
    // 5644kHz/1bit » pendant que le journal du serveur disait, à la seconde
    // près, `dsd_passthrough_decide … dsd_mode=native passthrough=true` : le
    // .dsf partait BRUT. Le transcodage était inventé, et son libellé
    // impossible — aucun FLAC ne porte du 1 bit à 5,6 MHz.
    //
    // Les trois modes DSD d'une sortie réseau ont chacun leur test, pour que
    // la disparition de l'étage fantôme ne se paie pas par la disparition des
    // étages VRAIS.

    /// Une piste DSD128 jouée sur une zone DLNA, avec le fil qu'on veut.
    fn dsd128_playing() -> ZoneState {
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Une piste DSD".into(),
                format: Some("dsf".into()),
                sample_rate: Some(5_644_800),
                bit_depth: Some(1),
                stream_id: Some("sid-dsd".into()),
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        }
    }

    /// Un fil qui nomme aussi son MIME — c'est par là que le passthrough DSD
    /// se reconnaît quand le renderer impose le sien (Yamaha R-N2000A :
    /// `audio/dsf` et rien d'autre).
    fn wire_mime(format: &str, mime: &str, sample_rate: u32, bit_depth: u16) -> StreamInfo {
        StreamInfo {
            format: format.into(),
            mime_type: mime.into(),
            sample_rate,
            bit_depth,
            ..Default::default()
        }
    }

    /// Mode 1/3 — DSD NATIF : le .dsf part brut, aucun étage de transcodage.
    #[test]
    fn dsd_natif_sur_le_fil_n_affiche_aucun_transcodage() {
        let (backend, zone) = dlna_zone();
        let sp = build_signal_path(
            &dsd128_playing(),
            &zone,
            &backend,
            Some("DMP-A6"),
            "none",
            Some(&wire_mime("dsf", "application/x-dsd", 5_644_800, 1)),
        )
        .unwrap();

        assert_eq!(
            transcoder_desc(&sp),
            None,
            "le .dsf part brut : annoncer un transcodage decrit une operation \
             qui n'a pas lieu (#1315)"
        );
        assert_eq!(step_desc(&sp, "Source").as_deref(), Some("DSD128 5.6 MHz"));
        assert_eq!(step_desc(&sp, "Transport").as_deref(), Some("DLNA/UPnP"));
        assert_eq!(
            sp.get("bit_perfect").and_then(Value::as_bool),
            Some(true),
            "un flux brut servi tel quel EST bit-perfect"
        );
        let summary = sp.get("summary").and_then(Value::as_str).unwrap();
        assert!(
            !summary.contains("FLAC"),
            "le resume invente encore un FLAC : {summary}"
        );
    }

    /// Le MIME suffit, quand le renderer impose le sien (`audio/dsf`) et que
    /// la session porte l'extension du fichier.
    #[test]
    fn dsd_natif_se_reconnait_aussi_au_mime_annonce_par_le_renderer() {
        for mime in ["application/x-dsd", "audio/x-dsf", "audio/dff", "audio/dsf"] {
            let (backend, zone) = dlna_zone();
            let sp = build_signal_path(
                &dsd128_playing(),
                &zone,
                &backend,
                Some("Yamaha R-N2000A"),
                "none",
                Some(&wire_mime("", mime, 5_644_800, 1)),
            )
            .unwrap();
            assert_eq!(
                transcoder_desc(&sp),
                None,
                "mime={mime} : le fil porte du DSD brut, pas un transcodage"
            );
        }
    }

    /// Mode 2/3 — DoP : le DSD voyage EMBALLÉ dans des trames PCM 24 bits.
    /// L'étage existe vraiment et doit rester affiché, avec les chiffres du
    /// fil (352,8 kHz / 24 bits pour du DSD128), jamais ceux de la source.
    #[test]
    fn dsd_en_dop_affiche_l_etage_wav_du_fil() {
        let (backend, zone) = dlna_zone();
        let sp = build_signal_path(
            &dsd128_playing(),
            &zone,
            &backend,
            Some("Wiim Pro"),
            "none",
            Some(&wire_mime("wav", "audio/wav", 352_800, 24)),
        )
        .unwrap();

        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("DSD128 5.6 MHz \u{2192} WAV 352kHz/24bit"),
            "le DoP est un vrai emballage : l'etage doit rester, avec les \
             chiffres du fil"
        );
    }

    /// Mode 3/3 — TRANSCODÉ en PCM : l'étage est réel, et son libellé aussi.
    /// C'est le cas témoin du premier test : la même source, le même code, un
    /// fil différent — et l'étage revient.
    #[test]
    fn dsd_transcode_en_pcm_affiche_bien_son_etage() {
        let (backend, zone) = dlna_zone();
        let sp = build_signal_path(
            &dsd128_playing(),
            &zone,
            &backend,
            Some("DMP-A6"),
            "none",
            Some(&wire_mime("flac", "audio/flac", 176_400, 24)),
        )
        .unwrap();

        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("DSD128 5.6 MHz \u{2192} FLAC 176kHz/24bit"),
            "une conversion REELLE doit rester visible — supprimer le fantome \
             ne doit pas rendre le serveur muet sur ce qu'il fait vraiment"
        );
        assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    }

    /// Aucun réglage de zone ne peut contredire un fil qui porte du DSD brut.
    /// Le même invariant que `wire_always_wins_over_every_zone_flag_combination`,
    /// appliqué au DSD : c'est le réglage « LPCM » coché qui aurait ramené un
    /// « → WAV » sur un fil .dsf.
    #[test]
    fn aucun_reglage_de_zone_ne_transcode_un_fil_dsd_brut() {
        let served = wire_mime("dsf", "application/x-dsd", 5_644_800, 1);
        for lpcm in [false, true] {
            for cap16 in [false, true] {
                for wav24 in [false, true] {
                    for dsd_mode in ["auto", "native", "dop", "pcm"] {
                        let (backend, zone) = dlna_zone();
                        let repo = ZoneRepo::with_backend(backend.clone());
                        let id = zone.id.unwrap();
                        repo.update_dlna_lpcm(id, lpcm).unwrap();
                        repo.update_dlna_cap_16bit(id, cap16).unwrap();
                        repo.update_dlna_wav24(id, wav24).unwrap();
                        repo.update_dsd_mode(id, dsd_mode).unwrap();
                        let zone = repo.get(id).unwrap().unwrap();

                        let sp = build_signal_path(
                            &dsd128_playing(),
                            &zone,
                            &backend,
                            Some("DMP-A6"),
                            "none",
                            Some(&served),
                        )
                        .unwrap();

                        assert_eq!(
                            transcoder_desc(&sp),
                            None,
                            "lpcm={lpcm} cap16={cap16} wav24={wav24} \
                             dsd_mode={dsd_mode} : l'affichage contredit un fil \
                             qui porte du DSD brut"
                        );
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Contre-épreuve PERMANENTE du libellé impossible (#1315, point 2).
    //
    // Le test ci-dessus protège le chemin ; celui-ci protège la CLASSE. On
    // injecte de force, dans le formateur d'étage de sortie, la contradiction
    // exacte qui a produit « FLAC 5644kHz/1bit » — une résolution du domaine
    // DSD sous chaque nom de conteneur PCM du code. Aucune ne doit pouvoir en
    // ressortir. Si quelqu'un rétablit un jour le format naïf, ce test casse
    // en nommant le conteneur fautif.
    #[test]
    fn aucun_conteneur_pcm_ne_peut_porter_une_resolution_dsd() {
        for container in ["FLAC", "WAV", "ALAC", "AAC", "MP3", "AIFF", "Unknown"] {
            for (sr, bd) in [
                (5_644_800, 1),  // DSD128 brut, le cas de Stéphane Villerio
                (2_822_400, 1),  // DSD64
                (11_289_600, 1), // DSD256
                (22_579_200, 1), // DSD512
            ] {
                let label = output_stage_label(container, sr, bd);
                assert!(
                    !label.contains(container),
                    "injection acceptee : « {label} » — aucun {container} ne \
                     transporte du {bd} bit a {sr} Hz (#1315)"
                );
                assert!(
                    label.starts_with("DSD"),
                    "le fil porte du DSD, le libelle doit le dire : {label}"
                );
            }
        }
    }

    /// L'autre moitié de la contre-épreuve : le garde-fou ne doit pas mordre
    /// sur du PCM légitime, jusqu'au 768 kHz/32 bits du marché.
    #[test]
    fn le_garde_fou_laisse_passer_tout_le_pcm_legitime() {
        for (sr, bd, attendu) in [
            (44_100, 16, "FLAC 44kHz/16bit"),
            (96_000, 24, "FLAC 96kHz/24bit"),
            (352_800, 24, "FLAC 352kHz/24bit"),
            (768_000, 32, "FLAC 768kHz/32bit"),
        ] {
            assert_eq!(output_stage_label("FLAC", sr, bd), attendu);
        }
    }

    // Sevy, LHC-52: the renderer is served WAV/LPCM (it does not advertise
    // audio/flac), so the path must show the REAL wire container, not the
    // static ALAC→FLAC transcode guess. The output is 16-bit LPCM, so the
    // hi-res 24-bit source reads as downconverted (not bit-perfect).
    #[test]
    fn dlna_wav_wire_shows_alac_to_wav() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("LHC-52"),
            "none",
            Some(&wire("wav", 96_000, 16)),
        )
        .unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} WAV 96kHz/16bit")
        );
        // Hi-res source truncated to the 16-bit LPCM cap → not bit-perfect,
        // but still a lossless source.
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    // Regression guard: with no live session container (None) the display keeps
    // its prior behaviour — ALAC transcodes to FLAC for DLNA.
    #[test]
    fn dlna_without_session_keeps_flac_target() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp = build_signal_path(&ps, &zone, &backend, Some("LHC-52"), "none", None).unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
        );
    }

    // A FLAC-advertising renderer (wire = flac) is unaffected by the override.
    #[test]
    fn dlna_flac_wire_keeps_flac_target() {
        let (backend, zone) = dlna_zone();
        let ps = alac_hires_playing();
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
        );
    }

    // #1504 (Jean Valjean) / #1480 (Bebelalu55) : le panneau bit-perfect doit
    // afficher LE MÊME volume que la page. La page montre `zone.volume` (base) ;
    // `ps.volume` est une copie mémoire qui peut être périmée (0,5 par défaut
    // après un redémarrage, ou laissée par une alarme/minuterie qui n'écrivait
    // pas la base). L'étape Volume se lit donc depuis la base, quelle que soit
    // la valeur mémoire.
    #[test]
    fn volume_step_reads_persisted_zone_volume_not_stale_memory() {
        let (backend, zone) = dlna_zone();
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = zone.id.unwrap();
        repo.update_volume(id, 20.0).unwrap();
        let zone = repo.get(id).unwrap().unwrap();

        // Copie mémoire périmée : le défaut 0,5 d'un ZoneState jamais resemé.
        let mut ps = alac_hires_playing();
        ps.volume = 0.5;

        let sp = build_signal_path(&ps, &zone, &backend, Some("Node"), "none", None).unwrap();
        assert_eq!(step_desc(&sp, "Volume").as_deref(), Some("Volume 20%"));
    }

    // Réciproque : curseur de la page à 100 % → pas d'étape Volume, même si la
    // copie mémoire traîne à 20 % (c'était exactement l'affichage signalé).
    #[test]
    fn volume_step_hidden_when_persisted_volume_is_full() {
        let (backend, zone) = dlna_zone();
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = zone.id.unwrap();
        repo.update_volume(id, 100.0).unwrap();
        let zone = repo.get(id).unwrap().unwrap();

        let mut ps = alac_hires_playing();
        ps.volume = 0.2;

        let sp = build_signal_path(&ps, &zone, &backend, Some("Node"), "none", None).unwrap();
        assert_eq!(step_desc(&sp, "Volume"), None);
    }

    // Native WAV 24-bit source, served byte-for-byte over the WAV wire.
    fn wav24_playing() -> ZoneState {
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("wav".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        }
    }

    // Sandro/Progman: a NATIVE WAV 24-bit source is passthrough (WAV never
    // transcodes for DLNA), so it must read bit-perfect even with dlna_wav24 off
    // — the badge previously showed red for WAV 24-bit direct.
    #[test]
    fn dlna_native_wav24_is_bit_perfect() {
        let (backend, zone) = dlna_zone();
        let ps = wav24_playing();
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Diretta"),
            "none",
            Some(&wire("wav", 96_000, 24)),
        )
        .unwrap();
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    // Yves, darTZeel LHC-208 et Eversolo DMP-A10 : zones en passthrough natif,
    // donc AUCUNE étape de transcodage — la seule ligne portant une résolution
    // est « Source ». Quand le scan n'a pas renseigné la piste (bibliothèque
    // NAS), les valeurs retombaient sur 44100 Hz et 16 bits écrits en dur, et
    // Tune affichait donc une résolution inventée pendant que le DAC lisait la
    // vraie. Le fil est maintenant consulté avant d'en arriver là.
    #[test]
    fn passthrough_without_metadata_reads_the_wire_not_a_default() {
        let (backend, zone) = dlna_zone();
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("flac".into()),
            sample_rate: None,
            bit_depth: None,
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        };
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("darTZeel LHC-208"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();
        assert_eq!(
            step_desc(&sp, "Source").as_deref(),
            Some("FLAC 96kHz/24bit"),
            "sans metadonnees, la resolution doit venir du fil et non du repli 44100/16"
        );
    }

    // #2427: radio resolution is unknown when NowPlaying is created. Its
    // 44.1 kHz value is only a bootstrap for the WAV session; after probing,
    // the wire reports the PCM rate actually served and must win.
    #[test]
    fn decoded_radio_source_uses_the_detected_wire_rate() {
        let (backend, zone) = dlna_zone();
        let np = NowPlaying {
            title: "France Musique".into(),
            source: "radio".into(),
            format: Some("wav".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            stream_id: Some("sid-radio".into()),
            ..Default::default()
        };
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        };

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Renderer"),
            "none",
            Some(&wire("wav", 48_000, 16)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "Source").as_deref(), Some("WAV 48kHz/16bit"));
    }

    // Sans session ET sans métadonnées, il n'y a rien à lire : le repli reste
    // celui d'avant. Ce test existe pour que la suppression du repli soit un
    // choix explicite si elle a lieu un jour, pas un effet de bord.
    #[test]
    fn no_wire_no_metadata_still_falls_back() {
        let (backend, zone) = dlna_zone();
        let np = NowPlaying {
            title: "Track".into(),
            format: Some("flac".into()),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        };
        let sp = build_signal_path(&ps, &zone, &backend, Some("LHC"), "none", None).unwrap();
        assert_eq!(
            step_desc(&sp, "Source").as_deref(),
            Some("FLAC 44kHz/16bit")
        );
    }

    // Le fil prime sur la règle. Ici la zone force le LPCM 16 bits, mais la
    // session sert réellement du 24 bits : c'est le 24 qui doit s'afficher.
    // Auparavant la règle gagnait et l'affichage annonçait une troncature qui
    // n'avait pas lieu.
    #[test]
    fn wire_resolution_wins_over_mirrored_rule() {
        let (backend, zone) = dlna_zone();
        ZoneRepo::with_backend(backend.clone())
            .update_dlna_lpcm(zone.id.unwrap(), true)
            .unwrap();
        let ps = alac_hires_playing();
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Eversolo DMP-A10"),
            "none",
            Some(&wire("wav", 96_000, 24)),
        )
        .unwrap();
        assert_eq!(
            transcoder_desc(&sp).as_deref(),
            Some("ALAC 96kHz/24bit \u{2192} WAV 96kHz/24bit")
        );
    }

    #[test]
    fn wav_wire_native_wav_is_bit_perfect_any_depth() {
        assert!(wav_wire_bit_perfect(true, true, false, 24)); // native WAV 24-bit, flag off
        assert!(wav_wire_bit_perfect(true, true, false, 16));
    }

    #[test]
    fn wav_wire_flac_fallback_capped_at_16_bit() {
        // FLAC/ALAC → WAV fallback (source not WAV): 24-bit needs the override.
        assert!(!wav_wire_bit_perfect(true, false, false, 24));
        assert!(wav_wire_bit_perfect(true, false, false, 16)); // fits plain 16-bit LPCM
        assert!(wav_wire_bit_perfect(true, false, true, 24)); // dlna_wav24 preserves 24-bit
    }

    #[test]
    fn wav_wire_lossy_source_never_bit_perfect() {
        assert!(!wav_wire_bit_perfect(false, true, true, 16));
    }

    // ------------------------------------------------------------------
    // ReplayGain dans le chemin du signal (#1627). Miroir de
    // `Orchestrator::zone_replaygain_changes_audio` : le panneau ne doit pas
    // annoncer « Bit-Perfect » pendant qu'un gain multiplie chaque
    // échantillon — même famille d'écart que l'EQ ignoré du verdict
    // (#1548/#1559, signalement Bilou).

    /// Comme `dlna_zone()`, mais avec les migrations appliquées : les tags
    /// ReplayGain vivent dans `track_metadata`, table créée par la migration 34
    /// et NON par `init_schema()`. Sans elle, la lecture du gain échoue et le
    /// test « pas d'étape » passerait pour la mauvaise raison.
    fn dlna_zone_migrated() -> (Arc<dyn DbBackend>, Zone) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        (backend, zone)
    }

    /// Une piste FLAC en base, taguée `rg_track_gain` (et rien d'autre), et
    /// l'état de lecture qui la joue. Le fil sert du FLAC : sans ReplayGain ce
    /// chemin est un passthrough bit-perfect — le contraste que les tests
    /// veulent.
    fn flac_track_with_rg_tag(backend: &Arc<dyn DbBackend>, gain_tag: &str) -> (i64, ZoneState) {
        let mut t = tune_core::db::models::Track::new("Piste".into());
        t.format = Some("flac".into());
        t.sample_rate = Some(96_000);
        t.bit_depth = Some(24);
        let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
            .set(tid, "rg_track_gain", gain_tag)
            .unwrap();
        let np = NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        };
        (tid, ps)
    }

    // RG actif (mode track, tag -4.2 dB) → étape présente avec le gain
    // appliqué, et le verdict bit-perfect tombe — alors que le même chemin
    // sans RG est un passthrough FLAC bit-perfect.
    #[test]
    fn replaygain_active_shows_step_and_breaks_bit_perfect() {
        let (backend, zone) = dlna_zone_migrated();
        let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
        SettingsRepo::with_backend(backend.clone())
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(
            step_desc(&sp, "ReplayGain").as_deref(),
            Some("ReplayGain (track, -4.2 dB, tags du fichier)")
        );
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
        // Le RG ne rend pas la SOURCE lossy : le badge qualité reste vert.
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    // RG off (défaut) : la même piste taguée n'affiche rien et reste
    // bit-perfect — le réglage, pas le tag, décide.
    #[test]
    fn replaygain_off_shows_nothing_and_stays_bit_perfect() {
        let (backend, zone) = dlna_zone_migrated();
        let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "ReplayGain"), None);
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    }

    // ---- #2362 : sortie mono ------------------------------------------------

    /// Une zone LOCALE, seule à porter la chaîne DSP où le repli est appliqué.
    fn local_zone_migrated() -> (Arc<dyn DbBackend>, Zone) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo
            .create("Bureau", Some("local"), Some("local:dac-1"))
            .unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        (backend, zone)
    }

    fn flac_playing() -> ZoneState {
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Piste".into(),
                format: Some("flac".into()),
                sample_rate: Some(96_000),
                bit_depth: Some(24),
                stream_id: Some("sid-1".into()),
                ..Default::default()
            }),
            volume: 1.0,
            ..Default::default()
        }
    }

    fn armer_mono(backend: &Arc<dyn DbBackend>, zone_id: i64) {
        SettingsRepo::with_backend(backend.clone())
            .set(&format!("zone_{zone_id}_mono_downmix"), "true")
            .unwrap();
    }

    /// #2362 — le chemin du signal DIT la transformation.
    ///
    /// C'est la contrepartie de #2825, fusionnée cette nuit : là, le volume
    /// logiciel prétendait à tort dégrader ; ici, une vraie transformation
    /// devait apparaître et n'apparaissait pas. Le même chemin, mono désarmé,
    /// est un passthrough FLAC bit-perfect (test suivant) : c'est le RÉGLAGE
    /// qui décide, et lui seul.
    #[test]
    fn sortie_mono_affiche_son_etape_et_fait_tomber_le_verdict() {
        let (backend, zone) = local_zone_migrated();
        armer_mono(&backend, zone.id.unwrap());

        let sp = build_signal_path(
            &flac_playing(),
            &zone,
            &backend,
            Some("DAC"),
            "CoreAudio",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(
            step_desc(&sp, "Mono").as_deref(),
            Some("Sortie mono : (G + D) / 2 sur les deux voies")
        );
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
        // Le repli ne rend pas la SOURCE avec perte : le badge qualité reste vert.
        assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
    }

    /// Défaut désarmé : aucune étape inventée, verdict intact. Sans ce témoin,
    /// le test ci-dessus passerait aussi avec une étape affichée en permanence.
    #[test]
    fn sortie_mono_desarmee_ninvente_aucune_etape() {
        let (backend, zone) = local_zone_migrated();

        let sp = build_signal_path(
            &flac_playing(),
            &zone,
            &backend,
            Some("DAC"),
            "CoreAudio",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "Mono"), None);
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    }

    /// Le périmètre de l'issue est la zone LOCALE. Une zone réseau qui porte le
    /// réglage ne doit PAS afficher l'étape : rien ne l'applique sur ce chemin,
    /// et l'annoncer décrirait un traitement qui n'a pas lieu.
    #[test]
    fn sortie_mono_ne_deborde_pas_sur_une_zone_reseau() {
        let (backend, zone) = dlna_zone_migrated();
        armer_mono(&backend, zone.id.unwrap());

        let sp = build_signal_path(
            &flac_playing(),
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "Mono"), None);
    }

    /// Le mode PURE gouverne le repli comme il gouverne l'égaliseur, le
    /// crossfeed et le ReplayGain : rien ne touche le signal, donc aucune étape
    /// et le verdict tient. Miroir de `zone_mono_downmix_with`.
    #[test]
    fn le_mode_pure_desarme_la_sortie_mono() {
        let (backend, zone) = local_zone_migrated();
        let zid = zone.id.unwrap();
        armer_mono(&backend, zid);
        SettingsRepo::with_backend(backend.clone())
            .set(&format!("zone_{zid}_audiophile"), r#"{"enabled":true}"#)
            .unwrap();

        let sp = build_signal_path(
            &flac_playing(),
            &zone,
            &backend,
            Some("DAC"),
            "CoreAudio",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "Mono"), None);
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    }

    // Mode track SANS tag stocké : gain effectif = 1, donc rien — l'étape
    // suit le facteur réellement appliqué, pas le réglage (miroir du seuil
    // de `zone_replaygain_changes_audio`).
    #[test]
    fn replaygain_mode_on_without_stored_gain_shows_nothing() {
        let (backend, zone) = dlna_zone_migrated();
        let mut t = tune_core::db::models::Track::new("Piste".into());
        t.format = Some("flac".into());
        let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
        SettingsRepo::with_backend(backend.clone())
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        let np = NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        };
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(np),
            volume: 1.0,
            ..Default::default()
        };

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "ReplayGain"), None);
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    }

    // PURE : le gain n'est jamais appliqué (orchestrator.rs, sortie locale et
    // chemin transcodé), donc jamais d'étape — quel que soit le réglage.
    #[test]
    fn replaygain_never_shown_in_pure_mode() {
        let (backend, zone) = dlna_zone_migrated();
        let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
        let settings = SettingsRepo::with_backend(backend.clone());
        settings
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        settings
            .set(
                &format!("zone_{}_audiophile", zone.id.unwrap()),
                r#"{"enabled":true}"#,
            )
            .unwrap();

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(step_desc(&sp, "ReplayGain"), None);
        assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    }

    // Mode album sur une piste qui n'a que le tag de piste : c'est le gain de
    // piste qui s'applique (repli de `stored_gain_detail`), et l'étape doit
    // nommer ce qui joue vraiment — « track », pas le réglage « album ».
    #[test]
    fn replaygain_album_mode_falls_back_to_track_and_says_so() {
        let (backend, zone) = dlna_zone_migrated();
        let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
        SettingsRepo::with_backend(backend.clone())
            .set(tune_core::audio::replaygain::MODE_KEY, "album")
            .unwrap();

        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap();

        assert_eq!(
            step_desc(&sp, "ReplayGain").as_deref(),
            Some("ReplayGain (track, -4.2 dB, tags du fichier)")
        );
    }

    // ---- #1627 : d'où vient le gain -----------------------------------------

    /// L'étape ReplayGain complète, faits structurés compris.
    fn rg_step(sp: &serde_json::Value) -> serde_json::Value {
        sp.get("steps")
            .and_then(|s| s.as_array())
            .and_then(|steps| {
                steps
                    .iter()
                    .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("ReplayGain"))
            })
            .cloned()
            .expect("étape ReplayGain absente")
    }

    fn signal_path_mode_track(
        backend: &Arc<dyn DbBackend>,
        zone: &Zone,
        ps: &ZoneState,
    ) -> serde_json::Value {
        SettingsRepo::with_backend(backend.clone())
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        build_signal_path(
            ps,
            zone,
            backend,
            Some("Node"),
            "none",
            Some(&wire("flac", 96_000, 24)),
        )
        .unwrap()
    }

    // Un gain qui vient des tags du fichier (rsgain, foobar…) est nommé comme
    // tel : c'est la réponse à « Tune utilise-t-il mes tags ? » (#1382), rendue
    // à l'endroit où la question se pose.
    #[test]
    fn replaygain_gain_venu_des_tags_est_nomme_tags_du_fichier() {
        let (backend, zone) = dlna_zone_migrated();
        let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");

        let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

        assert_eq!(
            step.get("description").and_then(|d| d.as_str()),
            Some("ReplayGain (track, -4.2 dB, tags du fichier)")
        );
        assert_eq!(
            step.get("gain_source").and_then(|s| s.as_str()),
            Some("file_tags")
        );
        assert_eq!(
            step.get("granularity").and_then(|s| s.as_str()),
            Some("track")
        );
    }

    // Le même gain, mais MESURÉ par la passe EBU R128 : le témoin de
    // provenance écrit à côté de `rg_track_gain` fait basculer le libellé.
    // Sans lui les deux cas étaient indiscernables en base — et l'affichage
    // aurait dû inventer.
    #[test]
    fn replaygain_gain_mesure_par_tune_est_nomme_analyse() {
        let (backend, zone) = dlna_zone_migrated();
        let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
            .set(
                tid,
                tune_core::audio::replaygain::TRACK_SOURCE_KEY,
                tune_core::audio::replaygain::SOURCE_ANALYSIS,
            )
            .unwrap();

        let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

        assert_eq!(
            step.get("description").and_then(|d| d.as_str()),
            Some("ReplayGain (track, -4.2 dB, analyse Tune)")
        );
        assert_eq!(
            step.get("gain_source").and_then(|s| s.as_str()),
            Some("analysis")
        );
    }

    // Bibliothèque analysée AVANT que le témoin existe (le parc installé) :
    // `rg_analyzed` seul suffit à trancher, parce que le balayage n'analyse
    // QUE les pistes dépourvues de `rg_track_gain`. Sans ce repli, tout le
    // parc verrait « tags du fichier » sur des mesures Tune.
    #[test]
    fn replaygain_base_ancienne_retombe_sur_rg_analyzed() {
        let (backend, zone) = dlna_zone_migrated();
        let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
            .set(tid, "rg_analyzed", "1700000000")
            .unwrap();

        let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

        assert_eq!(
            step.get("gain_source").and_then(|s| s.as_str()),
            Some("analysis")
        );
    }
}

/// #1499 — une zone qui « joue » sans destination doit le dire.
///
/// Deux situations produisent le même symptôme (file remplie, position qui
/// avance, aucun son) et ne se distinguaient que dans les journaux : la zone
/// sans sortie associée, et la zone navigateur dont aucun onglet ne tire le
/// flux. Bilou a ouvert deux fils forum sur un défaut BluOS inexistant faute
/// de ce signal.
#[cfg(test)]
mod output_reach_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::playback::NowPlaying;

    fn zone_with(output_type: Option<&str>, device: Option<&str>) -> Zone {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend);
        let id = repo.create("Ce PC", output_type, device).unwrap();
        repo.get(id).unwrap().unwrap()
    }

    /// Zone navigateur en lecture depuis `started_ago`, avec une session.
    fn browser_playing_since(started_ago: Duration) -> ZoneState {
        ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Track".into(),
                stream_id: Some("sid-1".into()),
                ..Default::default()
            }),
            last_play_started_at: Instant::now().checked_sub(started_ago),
            ..Default::default()
        }
    }

    #[test]
    fn zone_sans_sortie_est_signalee_avant_le_clic() {
        let zone = zone_with(Some("local"), None);
        assert_eq!(
            output_reach_of(&zone, &ZoneState::default(), false),
            "no_output"
        );
    }

    #[test]
    fn zone_avec_sortie_ne_signale_rien() {
        let zone = zone_with(Some("dlna"), Some("dev-1"));
        assert_eq!(output_reach_of(&zone, &ZoneState::default(), false), "ok");
    }

    #[test]
    fn zone_navigateur_a_larret_ne_signale_rien() {
        // Une zone navigateur n'a jamais de périphérique : sans lecture en
        // cours il n'y a rien à reprocher.
        let zone = zone_with(Some("browser"), None);
        assert_eq!(output_reach_of(&zone, &ZoneState::default(), false), "ok");
    }

    #[test]
    fn zone_navigateur_qui_demarre_beneficie_du_delai() {
        let zone = zone_with(Some("browser"), None);
        let ps = browser_playing_since(Duration::from_secs(2));
        assert_eq!(
            output_reach_of(&zone, &ps, false),
            "ok",
            "un onglet qui vient de recevoir stream_url n'a pas encore tiré d'octets"
        );
    }

    #[test]
    fn zone_navigateur_ecoutee_ne_signale_rien() {
        let zone = zone_with(Some("browser"), None);
        let ps = browser_playing_since(Duration::from_secs(60));
        assert_eq!(output_reach_of(&zone, &ps, true), "ok");
    }

    #[test]
    fn zone_navigateur_sans_personne_au_bout_est_signalee() {
        let zone = zone_with(Some("browser"), None);
        let ps = browser_playing_since(Duration::from_secs(60));
        assert_eq!(
            output_reach_of(&zone, &ps, false),
            "browser_unattended",
            "une minute de lecture sans un octet tiré : personne n'écoute"
        );
    }

    /// Le bandeau et l'abandon doivent basculer au MÊME instant (#2630).
    ///
    /// Le poller arrête désormais une lecture que personne ne tire au bout de
    /// `tune_core::poller::DELAI_SILENCE_ETABLI`. Si cette vue concluait plus
    /// tard, l'utilisateur verrait la lecture s'arrêter sans avoir jamais lu
    /// pourquoi ; plus tôt, elle accuserait un onglet qui a encore le droit de
    /// démarrer. Un seuil re-codé en dur ici les ferait diverger en silence.
    #[test]
    fn le_bandeau_bascule_a_linstant_ou_le_poller_renonce() {
        let zone = zone_with(Some("browser"), None);
        let seuil = tune_core::poller::DELAI_SILENCE_ETABLI;
        assert_eq!(
            output_reach_of(&zone, &browser_playing_since(seuil), false),
            "browser_unattended",
            "à l'échéance du poller, le client doit déjà savoir pourquoi"
        );
        assert_eq!(
            output_reach_of(
                &zone,
                &browser_playing_since(seuil - Duration::from_secs(1)),
                false
            ),
            "ok",
            "une seconde avant, l'onglet peut encore démarrer"
        );
    }

    /// #2588 — l'explication du silence survit à l'arrêt qui la provoquait.
    ///
    /// C'est LE défaut du ticket : le bandeau « aucun onglet ne reçoit le
    /// son » est le seul endroit où Tune explique le silence d'une zone
    /// navigateur, et il disparaissait à l'instant même où l'utilisateur
    /// arrêtait la zone — c'est-à-dire au moment exact où il réagissait à
    /// l'absence de son. Pierre M l'a vu passer sans pouvoir le relire.
    #[test]
    fn le_constat_de_silence_survit_a_larret() {
        let zone = zone_with(Some("browser"), None);
        let mut ps = browser_playing_since(Duration::from_secs(60));
        ps.state = PlayState::Stopped;
        ps.browser_unattended_at = Some(Instant::now());
        assert_eq!(
            output_reach_of(&zone, &ps, false),
            "browser_unattended",
            "arrêtée juste après le constat, la zone doit encore dire pourquoi"
        );
    }
    /// La rétention est bornée : une zone laissée tranquille cesse d'accuser.
    #[test]
    fn le_constat_de_silence_finit_par_se_taire() {
        let zone = zone_with(Some("browser"), None);
        let mut ps = browser_playing_since(Duration::from_secs(60));
        ps.state = PlayState::Stopped;
        ps.browser_unattended_at =
            Instant::now().checked_sub(BROWSER_UNATTENDED_RETENTION + Duration::from_secs(1));
        assert_eq!(output_reach_of(&zone, &ps, false), "ok");
    }
    /// Une zone à l'arrêt qui n'a jamais rien eu à expliquer se tait.
    ///
    /// Contre-épreuve de la précédente : sans ce cas, un `return
    /// "browser_unattended"` inconditionnel passerait les deux autres.
    #[test]
    fn zone_a_larret_sans_constat_ne_dit_rien() {
        let zone = zone_with(Some("browser"), None);
        let mut ps = browser_playing_since(Duration::from_secs(60));
        ps.state = PlayState::Stopped;
        assert_eq!(
            output_reach_of(&zone, &ps, false),
            "ok",
            "aucun silence constaté : rien à dire"
        );
    }
    /// Le constat ne doit pas survivre à une lecture qui, elle, est reçue.
    ///
    /// `play()` efface la marque, et la vue la lève dès que l'onglet tire le
    /// flux. Tant que la zone joue, c'est la consommation qui tranche — la
    /// marque d'hier n'a pas voix au chapitre.
    #[test]
    fn une_lecture_recue_ignore_le_constat_precedent() {
        let zone = zone_with(Some("browser"), None);
        let mut ps = browser_playing_since(Duration::from_secs(60));
        ps.browser_unattended_at = Some(Instant::now());
        assert_eq!(output_reach_of(&zone, &ps, true), "ok");
    }
    #[test]
    fn etat_restaure_ne_conclut_rien() {
        // `last_play_started_at` est `#[serde(skip)]` : après un redémarrage il
        // vaut None. On ne doit pas inventer un silence sur cette absence.
        let zone = zone_with(Some("browser"), None);
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                title: "Track".into(),
                stream_id: Some("sid-1".into()),
                ..Default::default()
            }),
            last_play_started_at: None,
            ..Default::default()
        };
        assert_eq!(output_reach_of(&zone, &ps, false), "ok");
    }
}

#[cfg(test)]
mod patch_zone_deserialize_tests {
    use super::{PatchZone, fixed_volume_confirmation_required};
    use tune_core::db::zone_repo::Zone;

    fn zone(output_type: Option<&str>, fixed_volume: bool) -> Zone {
        Zone {
            id: Some(7),
            name: "Salon".into(),
            output_type: output_type.map(str::to_string),
            output_device_id: Some("renderer-1".into()),
            volume: 37.0,
            muted: false,
            online: true,
            gapless_enabled: false,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume,
            autoplay_enabled: false,
        }
    }

    /// #2271 — le nouveau champ de mode se deserialise, et l'ancien booleen
    /// continue de se deserialiser seul. Les deux ensemble sont acceptes au
    /// niveau serde ; c'est le handler qui tranche la precedence.
    #[test]
    fn autoplay_mode_se_deserialise() {
        let b: PatchZone = serde_json::from_str(r#"{"autoplay_mode":"similar"}"#).unwrap();
        assert_eq!(b.autoplay_mode.as_deref(), Some("similar"));
        assert_eq!(b.autoplay_enabled, None, "champ absent, pas `false`");

        let b: PatchZone = serde_json::from_str(r#"{"autoplay_enabled":true}"#).unwrap();
        assert_eq!(b.autoplay_enabled, Some(true));
        assert_eq!(
            b.autoplay_mode, None,
            "un client qui ne connait que le booleen n'envoie pas de mode"
        );

        let b: PatchZone = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(b.autoplay_mode, None);
        assert_eq!(b.autoplay_enabled, None);
    }

    // #1320 (Cyrille) — « Aucune » ne persistait jamais : un `null` explicite
    // sur `max_sample_rate` se désérialisait en `None` extérieur, donc le
    // handler le confondait avec un champ absent et n'effaçait rien. Ces
    // trois états sont le contrat du PATCH ; le premier test échoue contre
    // le code d'avant (sans `deserialize_with = "double_option"`).

    #[test]
    fn explicit_null_means_clear_the_cap() {
        let p: PatchZone = serde_json::from_str(r#"{"max_sample_rate": null}"#).unwrap();
        assert_eq!(
            p.max_sample_rate,
            Some(None),
            "un null explicite doit demander l'effacement, pas être ignoré"
        );
    }

    #[test]
    fn absent_field_means_leave_untouched() {
        let p: PatchZone = serde_json::from_str(r#"{"name": "Salon"}"#).unwrap();
        assert_eq!(p.max_sample_rate, None);
    }

    #[test]
    fn value_means_set_the_cap() {
        let p: PatchZone = serde_json::from_str(r#"{"max_sample_rate": 705600}"#).unwrap();
        assert_eq!(p.max_sample_rate, Some(Some(705_600)));
    }

    /// #2395 — AUCUN type de sortie n'est dispensé de l'accord.
    ///
    /// `local` et `browser` l'étaient jusqu'ici. La garde protège le niveau qui
    /// sort des haut-parleurs, pas l'identité de ce qu'on commande : une zone
    /// locale à 20 % monte bien à pleine échelle (`LocalOutput::set_volume` est
    /// un vrai gain), et une zone `browser` — souvent un casque sur un portable
    /// — voit son niveau appliqué par le client web à partir du volume de zone,
    /// celui que l'armement met à 100.
    ///
    /// Le `None` et le type inconnu sont dans la liste pour ce qu'ils prouvent :
    /// la garde ne classe plus rien, donc elle ne peut plus se tromper de
    /// classement.
    #[test]
    fn aucune_sortie_ne_s_arme_sans_accord() {
        let p: PatchZone = serde_json::from_str(r#"{"fixed_volume": true}"#).unwrap();
        for stored_type in [
            Some("dlna"),
            Some("airplay"),
            Some("chromecast"),
            Some("local"),
            Some("browser"),
            Some("un-type-que-personne-ne-connait"),
            None,
        ] {
            assert!(
                fixed_volume_confirmation_required(&zone(stored_type, false), &p),
                "{stored_type:?} : armer le volume fixe monte la zone a pleine echelle, \
                 l'accord explicite est du quel que soit le type de sortie"
            );
        }
    }

    /// L'accord donné, l'armement passe — sur n'importe quelle sortie.
    ///
    /// L'autre bord du test précédent : la garde exige un accord, elle ne
    /// bloque pas le mode. Sans ce cas, un `return true` inconditionnel
    /// passerait pour un correctif.
    #[test]
    fn l_accord_explicite_autorise_l_armement_sur_toute_sortie() {
        let p: PatchZone =
            serde_json::from_str(r#"{"fixed_volume": true, "confirm_full_volume": true}"#).unwrap();
        for stored_type in [
            Some("dlna"),
            Some("airplay"),
            Some("local"),
            Some("browser"),
            None,
        ] {
            assert!(
                !fixed_volume_confirmation_required(&zone(stored_type, false), &p),
                "{stored_type:?} : l'accord donne, l'armement doit passer"
            );
        }
    }

    /// Changer de type de sortie dans le PATCH qui arme ne change rien.
    ///
    /// Ce cas gardait autrefois la précédence du type envoyé sur le type
    /// stocké — une zone locale basculée en AirPlay ne devait pas profiter de
    /// l'exemption. Il n'y a plus d'exemption ni de lecture du type, donc plus
    /// de précédence à tenir ; le cas reste, comme garde de non-régression :
    /// aucune combinaison de types, dans un sens ou dans l'autre, ne doit
    /// rouvrir un chemin d'armement sans accord.
    #[test]
    fn un_changement_de_type_dans_le_meme_patch_reste_protege() {
        for (stocke, demande) in [
            (Some("local"), "airplay"),
            (Some("dlna"), "local"),
            (Some("browser"), "dlna"),
            (Some("airplay"), "browser"),
        ] {
            let p: PatchZone = serde_json::from_str(&format!(
                r#"{{"output_type": "{demande}", "fixed_volume": true}}"#
            ))
            .unwrap();
            assert!(
                fixed_volume_confirmation_required(&zone(stocke, false), &p),
                "{stocke:?} -> {demande} : toujours un accord"
            );
        }
    }

    /// Ce qui ne monte PAS le volume passe sans rien demander.
    ///
    /// Le contre-poids des deux premiers : la garde ne se déclenche que sur la
    /// transition qui monte réellement à pleine échelle. Une zone déjà armée
    /// qu'on réaffirme ne monte rien — le saut a eu lieu — et un désarmement
    /// fait redescendre. Sans ces cas, exiger l'accord partout se confondrait
    /// avec l'exiger tout le temps.
    ///
    /// La liste ne contient plus `local` ni `browser` : ces deux chemins
    /// montent bel et bien la zone à 100 %, et ils sont désormais éprouvés dans
    /// `aucune_sortie_ne_s_arme_sans_accord`. L'ancien nom de cet essai
    /// affirmait qu'ils « ne montent pas le volume » ; c'était faux.
    #[test]
    fn ce_qui_ne_monte_pas_le_volume_passe_sans_accord() {
        for (stored_type, stored_fixed, payload) in [
            // Déjà armée : le PATCH réaffirme, il ne monte rien.
            (Some("dlna"), true, r#"{"fixed_volume": true}"#),
            (Some("local"), true, r#"{"fixed_volume": true}"#),
            (Some("browser"), true, r#"{"fixed_volume": true}"#),
            // Désarmement : on redescend.
            (Some("dlna"), true, r#"{"fixed_volume": false}"#),
            (Some("local"), true, r#"{"fixed_volume": false}"#),
            // Le PATCH ne parle pas de volume fixe du tout.
            (Some("dlna"), false, r#"{"name": "Salon"}"#),
        ] {
            let p: PatchZone = serde_json::from_str(payload).unwrap();
            assert!(
                !fixed_volume_confirmation_required(&zone(stored_type, stored_fixed), &p),
                "le chemin {stored_type:?}/{stored_fixed}/{payload} ne monte aucune zone \
                 a pleine echelle : rien a confirmer"
            );
        }
    }
}

#[cfg(test)]
mod zone_group_tests {
    use super::{CreateGroup, GroupRefusal, validate_group};
    use tune_core::db::zone_repo::Zone;

    // #1702 (Bilou, fil 1392) — deux zones pointant sur la même sortie : le
    // groupement répondait « 422 unprocessable entity », un code nu, sans
    // phrase. Deux causes distinctes, testées séparément :
    //   1. le client web n'envoie pas de `name`, et serde rejetait le corps
    //      avant même d'atteindre le handler → 422 d'axum, sans texte ;
    //   2. rien ne vérifiait la sortie partagée, donc aucun message ne
    //      pouvait l'expliquer.

    fn zone(id: i64, name: &str, device: Option<&str>) -> Zone {
        Zone {
            id: Some(id),
            name: name.to_string(),
            output_type: Some("local".into()),
            output_device_id: device.map(str::to_string),
            volume: 50.0,
            muted: false,
            online: true,
            gapless_enabled: false,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume: false,
            autoplay_enabled: false,
        }
    }

    #[test]
    fn payload_without_name_is_accepted() {
        // Le corps exact qu'envoie le client web. Il échouait ici.
        let body: CreateGroup =
            serde_json::from_str(r#"{"leader_id": 1, "zone_ids": [1, 2]}"#).unwrap();
        assert_eq!(body.zone_ids, vec![1, 2]);
        assert_eq!(body.leader_id, Some(1));
        assert_eq!(body.name, None);
    }

    #[test]
    fn two_zones_on_the_same_output_are_refused_by_name() {
        let zones = vec![
            zone(1, "PC", Some("hw:0,0")),
            zone(2, "Haut parleurs", Some("hw:0,0")),
        ];
        assert_eq!(
            validate_group(&[1, 2], &zones),
            Err(GroupRefusal::SameOutput(
                "PC".into(),
                "Haut parleurs".into()
            )),
            "le refus doit nommer les deux zones pour que le message soit lisible"
        );
    }

    #[test]
    fn two_zones_on_distinct_outputs_are_accepted() {
        let zones = vec![
            zone(1, "Salon", Some("hw:0,0")),
            zone(2, "Cuisine", Some("hw:1,0")),
        ];
        assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
    }

    #[test]
    fn zones_without_an_output_are_not_duplicates_of_each_other() {
        // Deux zones orphelines ne partagent pas « la même sortie » : elles
        // n'en ont aucune. Les refuser ici afficherait un message faux.
        let zones = vec![zone(1, "Salon", None), zone(2, "Cuisine", None)];
        assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
    }

    #[test]
    fn the_same_zone_twice_is_not_a_group() {
        let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
        assert_eq!(
            validate_group(&[1, 1], &zones),
            Err(GroupRefusal::NotEnoughZones)
        );
    }

    #[test]
    fn a_single_zone_is_not_a_group() {
        let zones = vec![
            zone(1, "Salon", Some("hw:0,0")),
            zone(2, "Cuisine", Some("hw:1,0")),
        ];
        assert_eq!(
            validate_group(&[1], &zones),
            Err(GroupRefusal::NotEnoughZones)
        );
    }

    #[test]
    fn a_vanished_zone_is_named_in_the_refusal() {
        let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
        assert_eq!(
            validate_group(&[1, 7], &zones),
            Err(GroupRefusal::UnknownZone(7))
        );
    }

    #[test]
    fn duplicate_ids_are_collapsed_not_flagged_as_same_output() {
        let zones = vec![
            zone(1, "Salon", Some("hw:0,0")),
            zone(2, "Cuisine", Some("hw:1,0")),
        ];
        assert_eq!(validate_group(&[1, 2, 1], &zones), Ok(vec![1, 2]));
    }

    #[test]
    fn every_refusal_has_a_french_sentence() {
        for key in [
            "zonegroup.needsTwoZones",
            "zonegroup.unknownZone",
            "zonegroup.sameOutput",
        ] {
            let msg = crate::i18n::t("fr", key);
            assert_ne!(
                msg, key,
                "{key} n'a pas de traduction : le client afficherait la clé"
            );
            assert!(msg.len() > 20, "{key} doit expliquer, pas juste nommer");
        }
    }
}

#[cfg(test)]
mod aac_passthrough_tests {
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::zone_repo::ZoneRepo;

    fn zone_repo() -> (Arc<dyn DbBackend>, i64) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend.clone());
        let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
        (backend, id)
    }

    /// Le réglage doit être ÉTEINT par défaut.
    ///
    /// C'est l'invariant central de cette fonctionnalité : un renderer qui
    /// annonce l'AAC peut le refuser dans un conteneur ou à un débit donné.
    /// Activé d'office, cela produirait un silence inexpliqué chez ceux dont le
    /// matériel a menti — le pire symptôme, celui qu'on ne relie jamais à sa
    /// cause. Celui qui l'active sait ce que son appareil fait vraiment.
    #[test]
    fn aac_passthrough_is_off_until_the_user_asks_for_it() {
        let (backend, id) = zone_repo();
        let repo = ZoneRepo::with_backend(backend);
        assert!(
            !repo.get_aac_passthrough(id),
            "le passthrough AAC ne doit jamais être actif par défaut"
        );
        repo.update_aac_passthrough(id, true).unwrap();
        assert!(repo.get_aac_passthrough(id));
        repo.update_aac_passthrough(id, false).unwrap();
        assert!(!repo.get_aac_passthrough(id));
    }

    /// Les deux réglages sont indépendants : activer l'AAC ne doit pas activer
    /// l'ALAC, et réciproquement. Ils partagent le conteneur MP4 côté format,
    /// ce qui rend la confusion facile à écrire et invisible à l'usage.
    #[test]
    fn aac_and_alac_settings_never_leak_into_each_other() {
        let (backend, id) = zone_repo();
        let repo = ZoneRepo::with_backend(backend);
        repo.update_aac_passthrough(id, true).unwrap();
        assert!(repo.get_aac_passthrough(id));
        assert!(
            !repo.get_alac_passthrough(id),
            "activer l'AAC a activé l'ALAC"
        );
        repo.update_aac_passthrough(id, false).unwrap();
        repo.update_alac_passthrough(id, true).unwrap();
        assert!(repo.get_alac_passthrough(id));
        assert!(
            !repo.get_aac_passthrough(id),
            "activer l'ALAC a activé l'AAC"
        );
    }
}

/// Garde-fou : le PATCH d'une zone ne doit plus jamais échouer en silence.
///
/// Gérard Brot (#1964) : « si je change manuellement la sortie en local […]
/// j'ai erreur 500 en retour du server ». Le message de la cause était dans le
/// corps de la réponse — donc entre les mains du serveur — et **aucune ligne
/// n'était journalisée**. Il a fallu lui écrire pour lui redemander ce que le
/// serveur savait déjà.
///
/// Trente blocs partageaient le même moule `return (INTERNAL_SERVER_ERROR, e)`.
/// Ils passent tous par la macro `ecrire!`, qui journalise avant de rendre la
/// main. Ce test relit la source plutôt que d'exercer la route : la propriété à
/// tenir est structurelle — « aucun 500 nu dans ce handler » — et c'est
/// justement le genre qu'un nouveau champ ajouté par copier-coller ré-introduit.
#[cfg(test)]
mod patch_zone_error_guard {
    use std::fs;
    use std::path::Path;

    /// Le corps de `patch_zone`, des `async fn patch_zone(` jusqu'au `\n}\n`
    /// qui le ferme. Découpé sur la source plutôt que sur des numéros de ligne,
    /// qui dérivent à chaque édition.
    fn corps_du_handler() -> String {
        let source =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes/zones.rs"))
                .expect("lecture de zones.rs");
        let debut = source
            .find("async fn patch_zone(")
            .expect("`patch_zone` a été renommé — ce garde-fou ne garde plus rien");
        let reste = &source[debut..];
        let fin = reste
            .find("\n}\n")
            .expect("fin de `patch_zone` introuvable");
        reste[..fin].to_string()
    }

    #[test]
    fn no_bare_500_survives_in_patch_zone() {
        let corps = corps_du_handler();
        assert!(
            !corps.contains("(StatusCode::INTERNAL_SERVER_ERROR, e)"),
            "un `return (StatusCode::INTERNAL_SERVER_ERROR, e)` nu subsiste dans \
             `patch_zone` : la cause partira sans laisser de trace, et un 500 \
             signalé par un testeur sera de nouveau impossible à instruire \
             (#1964). Utiliser la macro `ecrire!`, qui journalise."
        );
    }

    #[test]
    fn every_write_goes_through_the_logging_macro() {
        let corps = corps_du_handler();
        let ecritures = corps.matches("ecrire!(").count();
        // 22 à la rédaction. Le seuil protège contre l'inverse du test
        // précédent : quelqu'un qui remplacerait les blocs par des `.ok()`
        // silencieux passerait le premier test et perdrait tout autant les
        // causes.
        assert!(
            ecritures >= 20,
            "seulement {ecritures} appels à `ecrire!` dans `patch_zone` — \
             des écritures ont-elles été retirées du chemin journalisé ?"
        );
    }

    /// Les valeurs jugeables par la route doivent l'être AVANT la première
    /// écriture. Un PATCH à moitié appliqué est pire qu'un PATCH refusé : la
    /// zone se retrouve dans un état que l'utilisateur n'a pas demandé.
    #[test]
    fn value_checks_come_before_any_write() {
        let corps = corps_du_handler();
        let premier_refus = corps
            .find("refus_de_valeur(")
            .expect("aucune validation de valeur dans `patch_zone`");
        let premiere_ecriture = corps
            .find("ecrire!(")
            .expect("aucune écriture dans `patch_zone`");
        assert!(
            premier_refus < premiere_ecriture,
            "une validation arrive APRÈS une écriture : un PATCH refusé aurait \
             déjà modifié la zone"
        );
    }

    #[test]
    fn full_volume_refusal_comes_before_any_write() {
        let corps = corps_du_handler();
        let refus = corps
            .find("fixed_volume_confirmation_required(&zone_before, &body)")
            .expect("le PATCH ne protège plus l'armement du volume fixe");
        let premiere_ecriture = corps
            .find("ecrire!(")
            .expect("aucune écriture dans `patch_zone`");
        assert!(
            refus < premiere_ecriture,
            "la confirmation du volume fixe est vérifiée APRÈS une écriture : \
             un PATCH refusé aurait déjà modifié la zone"
        );
    }
}

/// Garde-fou #2092 : les charges utiles d'une zone ne doivent plus diverger.
///
/// L'état d'aléatoire et de répétition appartient à la zone et **survit aux
/// redémarrages** (`queue_persistence`, `startup.rs`). Le WebSocket l'envoyait
/// déjà ; les charges REST, non — et ce sont elles que le client relit au
/// changement de zone et après chaque événement de lecture.
///
/// Tades (#2092) a donc écouté des albums dans le désordre avec un bouton
/// « aléatoire » éteint, sans limite de durée, et a ouvert deux fils en
/// écrivant « je ne pense pas avoir paramétré cela ». Il avait raison de ne pas
/// s'en souvenir : rien ne le lui montrait.
///
/// La cause de fond n'est pas l'oubli d'un champ, c'est que **cette charge
/// utile est construite à plusieurs endroits**. Deux copies avaient déjà
/// divergé. Ce contrôle exige qu'elles restent d'accord — c'est la même
/// famille que #2012 (« le rapport de fin de scan est construit trois fois, et
/// les copies ont déjà divergé »).
#[cfg(test)]
mod charge_utile_zone_guard {
    /// ⚠️ La source est tronquée AVANT ce module.
    ///
    /// `include_str!` rend le fichier entier, module de test compris — et les
    /// motifs cherchés ci-dessous y figurent mot pour mot. Un `contains` sur le
    /// fichier complet se trouverait lui-même et rendrait vrai quoi qu'il
    /// arrive. Vécu le jour même sur un autre garde-fou (#2082) : il avait
    /// survécu au sabotage de la condition qu'il prétendait garder.
    fn code_de_production() -> &'static str {
        const TOUT: &str = include_str!("zones.rs");
        const BORNE: &str = "mod charge_utile_zone_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("module renommé : la découpe ne protège plus rien"));
        &TOUT[..fin]
    }

    /// 🔴 Le point aveugle qui a laissé passer la troisième copie (#2055).
    ///
    /// Ce garde-fou affirmait « TOUTE charge utile de zone » en ne lisant qu'un
    /// seul fichier. La charge utile est pourtant construite dans deux : les
    /// deux `obj.insert(…)` de `zones.rs`, et le `json!` de `build_zone_json`
    /// (`playback.rs`) — celui que rendent une vingtaine de routes de lecture.
    /// Cette troisième copie portait `queue_length`, `queue_position` et
    /// `can_skip_next`, mais ni `shuffle` ni `repeat` : exactement la
    /// divergence que ce contrôle prétendait interdire, un fichier plus loin.
    ///
    /// On ne rend ici que le CORPS de `build_zone_json`. Le fichier entier
    /// apporterait `Json(json!({ "shuffle": enabled }))` de `toggle_shuffle` et
    /// son jumeau `"repeat"` de `toggle_repeat` — deux réponses qui ne décrivent
    /// pas une zone — et les compteurs ne diraient plus rien.
    fn corps_de_build_zone_json() -> &'static str {
        const TOUT: &str = include_str!("playback.rs");
        const DEBUT: &str = "pub(crate) async fn build_zone_json(";
        const FIN: &str = "\nasync fn build_zone_json_with_result(";
        let debut = TOUT.find(DEBUT).unwrap_or_else(|| {
            panic!("`build_zone_json` renommée : la découpe ne garde plus rien")
        });
        let fin = TOUT[debut..]
            .find(FIN)
            .map(|i| debut + i)
            .unwrap_or_else(|| panic!("`build_zone_json_with_result` renommée : découpe perdue"));
        &TOUT[debut..fin]
    }

    /// `queue_length` sert de marqueur : c'est le champ que porte toute charge
    /// utile décrivant l'état de lecture d'une zone. Chacune doit porter aussi
    /// l'aléatoire, la répétition et la décision autoritaire « suivant ».
    #[test]
    fn toute_charge_utile_de_zone_porte_le_transport_et_la_decision_suivant() {
        // Les deux fichiers qui construisent la charge utile. Compter sur un
        // seul, c'était garder la moitié du code en croyant tout tenir (#2055).
        let src = format!("{}{}", code_de_production(), corps_de_build_zone_json());
        // Les motifs ne portent PAS le `obj.insert(` qui les précède : rustfmt
        // coupe un appel long sur trois lignes dès que ses arguments grossissent,
        // et le compteur retomberait alors à zéro sans qu'une seule charge utile
        // ait changé. Un garde-fou sensible à la mise en forme lâche en silence,
        // au pire moment — c'est la première version de celui-ci qui l'a montré.
        //
        // Deux écritures possibles pour la même clé : `"x".into()` dans un
        // `Map` (zones.rs) et `"x":` dans un `json!` (build_zone_json). Les
        // compter toutes les deux, sinon ajouter le champ dans la mauvaise
        // syntaxe laisserait le contrôle rouge sans faute — ou vert avec.
        let compter = |cle: &str| {
            src.matches(&format!(r#""{cle}".into()"#)).count()
                + src.matches(&format!(r#""{cle}":"#)).count()
        };
        let etats = compter("queue_length");
        let aleatoire = compter("shuffle");
        let repetition = compter("repeat");
        let suivant = compter("can_skip_next");

        assert!(
            etats >= 3,
            "le marqueur `queue_length` n'apparaît que {etats} fois — la forme \
             des charges utiles a changé, et ce contrôle ne garde plus rien. \
             Il en faut au moins TROIS : les deux de `zones.rs` et celle de \
             `build_zone_json` (#2055)."
        );
        assert_eq!(
            aleatoire, etats,
            "{etats} charge(s) utile(s) de zone, mais {aleatoire} portent \
             `shuffle` : une copie a divergé. Le client naîtrait de nouveau à \
             « aléatoire éteint » devant un serveur qui l'a activé (#2092)."
        );
        assert_eq!(
            repetition, etats,
            "{etats} charge(s) utile(s) de zone, mais {repetition} portent \
             `repeat` : même divergence, autre réglage."
        );
        assert_eq!(
            suivant, etats,
            "{etats} charge(s) utile(s) de zone, mais {suivant} portent \
             `can_skip_next` : le client recommencerait à deviner la fin de la \
             permutation depuis l'ordre brut de la file (#2337)."
        );
    }
}

#[cfg(test)]
mod contrat_des_retours_anticipes {
    use crate::state::AppState;
    use tune_core::db::zone_repo::ZoneRepo;

    /// Et le VERROU de branchement, sans lequel le test ci-dessous ne prouve
    /// rien : il valide `build_zone_json`, pas le fait que les retours
    /// anticipés s'en servent. Rebrancher un `to_value(z)` le laisserait vert.
    ///
    /// C'est le même écart que JP a relevé quatre fois cette nuit — tester que
    /// la fonction marche, pas qu'on l'appelle.
    #[test]
    fn les_retours_anticipes_passent_par_le_contrat() {
        let src = std::fs::read_to_string(std::path::Path::new("src/routes/zones.rs"))
            .expect("zones.rs doit être lisible depuis la racine du crate");
        let debut = src
            .find("async fn create_zone(")
            .expect("create_zone doit exister");
        let fin = src[debut..]
            .find("\nasync fn ")
            .map(|i| debut + i)
            .unwrap_or(src.len());
        let corps = &src[debut..fin];

        assert_eq!(
            corps.matches("build_zone_json(").count(),
            3,
            "les TROIS retours anticipés doivent passer par le contrat client : \
             zone déjà associée au device, zone du même hôte sous une autre \
             identité SSDP (#1281), et rattrapage après collision UNIQUE"
        );
        assert!(
            !corps.contains("serde_json::to_value(z)"),
            "un retour anticipé sérialise encore la ligne brute : volume 50 au \
             lieu de 0.5, et les six champs d'état absents (#2284)"
        );
    }

    /// La contre-épreuve de JP Robbe sur #2284 : une zone qui existe déjà doit
    /// ressortir dans le CONTRAT client, pas dans la forme brute de la base.
    ///
    /// Les deux retours anticipés de `POST /zones` — zone déjà associée au
    /// `output_device_id`, et rattrapage après collision `UNIQUE` — faisaient un
    /// `serde_json::to_value(&zone)` : `volume: 50` au lieu de `0.5`, et les six
    /// champs d'état absents. Le client ajoute cet objet à son magasin, donc le
    /// curseur repartait au maximum malgré #2278.
    #[tokio::test]
    async fn une_zone_existante_ressort_au_contrat_client() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let id = repo
            .create("Salon", Some("dlna"), Some("uuid:abcd"))
            .unwrap();
        repo.update_volume(id, 50.0).unwrap();

        let v = crate::routes::playback::build_zone_json(&state, id).await;

        assert_eq!(
            v.get("volume").and_then(|x| x.as_f64()),
            Some(0.5),
            "volume en contrat client (0..1), pas la valeur de la base"
        );
        for champ in [
            "state",
            "current_track",
            "position_ms",
            "queue_length",
            "can_skip_next",
        ] {
            assert!(
                v.get(champ).is_some(),
                "{champ} absent : le client garderait la valeur d'une autre zone"
            );
        }
    }

    /// #2055 / #2092 — la charge utile rendue par les routes de LECTURE doit
    /// dire l'aléatoire et la répétition du moteur, pas les taire.
    ///
    /// Tades, quatre messages le 20/08 : « lecture aléatoire non demandée de
    /// l'album », « quand j'appuie sur suivant, il choisit une piste au
    /// hasard », « je ne pense pas avoir paramétré cela ». Le correctif #2153 a
    /// rendu ces deux champs aux charges utiles de `zones.rs` ; celle de
    /// `build_zone_json` — `play`, `pause`, `resume`, `stop`, `queue/jump`,
    /// `pins/{i}/invoke` — ne les portait toujours pas, alors qu'elle porte déjà
    /// `can_skip_next`, la décision qui DÉPEND de l'aléatoire (#2337).
    ///
    /// La présence seule ne prouve rien : deux constantes en dur passeraient.
    /// On arme donc le moteur et on exige que la charge utile le répète.
    #[tokio::test]
    async fn le_contrat_de_lecture_dit_l_aleatoire_et_la_repetition_du_moteur() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let id = repo
            .create("Salon", Some("dlna"), Some("uuid:abcd"))
            .unwrap();

        // Zone au repos : les deux réglages sortent à leur valeur de départ,
        // et pas en `null` ni en champ absent.
        let v = crate::routes::playback::build_zone_json(&state, id).await;
        assert_eq!(
            v.get("shuffle"),
            Some(&serde_json::json!(false)),
            "l'aléatoire est absent de la charge utile de lecture : le client \
             naîtrait de nouveau à « éteint » sans moyen d'apprendre le \
             contraire (#2092)"
        );
        assert_eq!(
            v.get("repeat"),
            Some(&serde_json::json!("off")),
            "la répétition est absente de la charge utile de lecture"
        );

        // Moteur armé — la file compte, sinon `set_shuffle` ne fabrique aucune
        // permutation et `can_skip_next` resterait faux pour une autre raison.
        state.playback.update_queue_info(id, 0, 5).await;
        state
            .playback
            .set_repeat(id, tune_core::playback::RepeatMode::All)
            .await;
        state.playback.set_shuffle(id, true).await;

        let v = crate::routes::playback::build_zone_json(&state, id).await;
        assert_eq!(
            v.get("shuffle"),
            Some(&serde_json::json!(true)),
            "le moteur tire au sort et la charge utile dit « non » : c'est \
             exactement l'écart vécu par Tades (#2055)"
        );
        assert_eq!(
            v.get("repeat"),
            Some(&serde_json::json!("all")),
            "`repeat` doit sortir en variante sérialisée (« all »), comme dans \
             `zones.rs` et sur le WebSocket — pas en « All » ni en nombre"
        );
    }

    /// #1281 — buchardt A700 : un appareil annoncé sous DEUX identités SSDP
    /// (deux UUID, même hôte) apparaît deux fois dans le sélecteur. « I tried
    /// creating a zone and it duplicates the zone output » : POST /zones avec
    /// l'identité jumelle ne dédoublonnait que par `output_device_id` exact et
    /// créait une deuxième zone pour le même renderer physique. Le regroupement
    /// par hôte de la découverte doit s'appliquer ici aussi : la zone existante
    /// est rendue (200), rien n'est créé.
    #[tokio::test]
    async fn poster_la_seconde_identite_ssdp_rend_la_zone_existante() {
        use axum::response::IntoResponse;

        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let zid = repo
            .create("buchardt A700", Some("dlna"), Some("uuid:a700-dlna"))
            .unwrap();
        // L'identité physique que la découverte persiste (#942/#1239).
        repo.set_host(zid, "192.168.1.50").unwrap();

        // La jumelle du même appareil, déjà enregistrée comme sortie par la
        // découverte : même hôte, autre UUID.
        {
            let mut reg = state.outputs.lock().await;
            reg.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
                "buchardt A700".into(),
                "uuid:a700-oh".into(),
                "192.168.1.50".into(),
                "http://192.168.1.50:49152/av".into(),
                "http://192.168.1.50:49152/rc".into(),
                None,
            )));
        }

        let resp = super::create_zone(
            axum::extract::State(state.clone()),
            axum::Json(super::CreateZone {
                name: "buchardt A700".into(),
                output_type: Some("dlna".into()),
                output_device_id: Some("uuid:a700-oh".into()),
            }),
        )
        .await
        .into_response();

        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "la zone existante du même hôte est rendue, pas créée (201)"
        );
        assert_eq!(
            repo.list().unwrap().len(),
            1,
            "toujours une seule zone pour l'appareil physique"
        );
    }
}

/// #1395 — le backend de sortie locale réellement actif, et le motif du repli,
/// doivent arriver jusqu'au client.
#[cfg(test)]
mod backend_local_annonce_tests {
    use super::local_backend_status_value;

    /// La famille des types de sortie, mutée en entier : seule une zone locale
    /// porte le champ. Annoncer un repli ASIO sur un renderer DLNA serait
    /// l'annonce fantôme que #2053 et #1315 ont déjà coûtée.
    #[test]
    fn seule_une_zone_locale_porte_le_statut() {
        // Une zone sans `output_type` est locale — même convention que
        // `build_signal_path`. Sans sortie locale compilée il n'y a AUCUN
        // backend à décrire, et le champ doit rester absent partout : c'est la
        // moitié du contrat qui vaut dans les deux constructions.
        #[cfg(feature = "local-audio")]
        for local in [None, Some("local")] {
            assert!(
                local_backend_status_value(local, "asio").is_some(),
                "zone locale ({local:?}) : statut absent"
            );
        }
        #[cfg(not(feature = "local-audio"))]
        for local in [None, Some("local")] {
            assert!(
                local_backend_status_value(local, "asio").is_none(),
                "zone locale ({local:?}) : statut annoncé sans sortie locale compilée"
            );
        }
        for distant in [
            "dlna",
            "chromecast",
            "bluos",
            "airplay",
            "browser",
            "oaat",
            "squeezebox",
        ] {
            assert!(
                local_backend_status_value(Some(distant), "asio").is_none(),
                "zone « {distant} » : statut de backend LOCAL annoncé à tort"
            );
        }
    }

    /// Le contrat de la charge utile : les cinq champs, nommés, pour que le
    /// client puisse dire « vous avez demandé X, Y tourne, parce que Z ».
    #[cfg(feature = "local-audio")]
    #[test]
    fn le_statut_porte_le_demande_a_cote_de_lactif() {
        let v = local_backend_status_value(Some("local"), "ASIO").expect("zone locale");
        for champ in [
            "active",
            "requested",
            "fell_back",
            "fallback_reason",
            "fallback_detail",
            // #2207 — le PÉRIPHÉRIQUE réellement ouvert, face au demandé. Le
            // champ fait partie du contrat même quand rien n'a encore joué :
            // il vaut alors `null`, ce qui est la réponse honnête. C'est son
            // ABSENCE de la charge utile qui serait la régression — le client
            // n'aurait de nouveau que le journal pour savoir où sort le son.
            "device",
        ] {
            assert!(v.get(champ).is_some(), "champ « {champ} » absent de {v}");
        }
        assert_eq!(
            v["requested"], "asio",
            "le demandé doit être rendu normalisé, pas déduit"
        );
        assert!(
            v["active"].as_str().is_some_and(|s| !s.is_empty()),
            "l'actif doit être nommé : {v}"
        );
    }

    /// Le VERROU de branchement : la fonction peut être parfaite et n'être
    /// appelée nulle part. Les quatre charges utiles qui portent une zone
    /// doivent toutes s'en servir — c'est la leçon de #1864, où quinze
    /// prédicats sur dix-sept n'étaient jamais construits pendant leur test.
    #[test]
    fn les_quatre_charges_utiles_de_zone_appellent_le_contrat() {
        // Source normalisée : on retire tous les blancs, pour que le test
        // survive à un passage de rustfmt qui recasserait les lignes.
        fn sans_blancs(fichier: &str) -> String {
            std::fs::read_to_string(std::path::Path::new(fichier))
                .unwrap_or_else(|e| panic!("{fichier} doit être lisible : {e}"))
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        }

        // Les QUATRE sites, un par charge utile qui décrit une zone. Chacun
        // est nommé par l'appel exact qu'il doit contenir.
        for (fichier, appel, quoi) in [
            (
                "src/routes/zones.rs",
                "local_backend_status_value(z.output_type.as_deref(),&audio_backend_pref",
                "GET /zones",
            ),
            (
                "src/routes/zones.rs",
                "local_backend_status_value(zone.output_type.as_deref(),&audio_backend_pref",
                "GET /zones/{id}",
            ),
            (
                "src/routes/ws.rs",
                "local_backend_status_value(z.output_type.as_deref(),&audio_backend_pref",
                "instantané WebSocket",
            ),
            (
                "src/routes/playback.rs",
                "local_backend_status_value(zone.output_type.as_deref(),&audio_backend_pref",
                "play / next / previous / resume",
            ),
        ] {
            let src = sans_blancs(fichier);
            assert!(
                src.contains(appel),
                "{quoi} ({fichier}) n'appelle plus local_backend_status_value — \
                 la zone repart sans dire quel backend tourne vraiment"
            );
            assert!(
                src.contains("\"audio_backend_status\""),
                "{quoi} ({fichier}) : le champ audio_backend_status a disparu de la charge utile"
            );
        }
    }
}
