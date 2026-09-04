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
    output_device_id: Option<&str>,
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
    // #3254 — …et ce que ce réglage VAUT sur cette zone-ci. Le champ ci-dessus
    // était accepté et relu pour n'importe quelle zone, alors que les trois
    // seuls sites qui poussent le repli exigent une sortie `local:` et un
    // `LocalOutput` : sur une zone réseau, accepté, persisté, relu… et sans
    // effet. Le chemin du signal disait déjà la vérité (`zone_mono_downmix_step`
    // rend `None` hors sortie locale) ; c'est le RÉGLAGE qui se taisait.
    //
    // Strictement ADDITIF : `mono_downmix` reste publié tel quel, à sa valeur
    // persistée. Un client qui ne lit pas ce statut voit le même écran qu'avant.
    // Même vocabulaire que `local_exclusive_mode_status` (#3192) : `reason`
    // stable pour la machine, `detail` en clair pour un écran sans table de
    // traduction.
    obj.insert(
        "mono_downmix_status".into(),
        json!(tune_core::audio::mono_downmix::mono_downmix_status(
            mono_downmix,
            tune_core::audio::mono_downmix::mono_downmix_runs_on_output(output_device_id),
            tune_core::audio::audiophile::zone_enabled(backend, zone_id),
        )),
    );
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
    // …et ce que ce réglage VAUT sur CETTE zone (#2742). Additif : l'objet
    // `crossfeed` ci-dessus est publié tel quel, un client qui ignore ce
    // champ voit le même écran qu'avant.
    let crossfeed_status = crossfeed_status_de_zone(
        &state.backend,
        id,
        crossfeed["enabled"].as_bool().unwrap_or(false),
    );

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
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

/// Ce que le crossfeed VAUT sur cette zone-ci, à côté de ce que le réglage
/// demande — #2742.
///
/// Le crossfeed n'est installé qu'à trois endroits, tous derrière la même
/// double garde `device_id.starts_with("local:")` +
/// `downcast_ref::<LocalOutput>()` (`orchestrator.rs` : chemin de lecture,
/// `refresh_zone_crossfeed`, `refresh_zone_pure_dsp`). Une zone réseau n'a donc
/// aucun chemin de code — pendant que cette route-ci offrait le réglage, le
/// persistait, et le relisait sans un mot. Tades : « Crossfeed n'a aucune
/// action ».
///
/// La règle elle-même vit dans `tune_core::audio::crossfeed` et ne lit aucune
/// base : ici on ne fait que lui passer les deux faits qu'elle attend — la
/// sortie de la zone et son mode PURE. Une seule règle, donc pas de dérive
/// possible entre cet écran et le son.
fn crossfeed_status_de_zone(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    requested: bool,
) -> tune_core::audio::crossfeed::CrossfeedStatus {
    let device = ZoneRepo::with_backend(backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    tune_core::audio::crossfeed::crossfeed_status(
        requested,
        tune_core::audio::crossfeed::crossfeed_runs_on_output(device.as_deref()),
        tune_core::audio::audiophile::zone_enabled(backend, zone_id),
    )
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
    // #2742 — publié dès que le corps porte un `crossfeed`, pour que la réponse
    // au CLIC dise déjà si le réglage aura le moindre effet.
    let mut crossfeed_status: Option<tune_core::audio::crossfeed::CrossfeedStatus> = None;
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
        // #2742 — et si la zone ne peut PAS faire tourner de crossfeed, le
        // serveur le dit au lieu d'enregistrer en silence. Journalisé au
        // moment du CLIC, pas à la lecture : c'est ici que l'utilisateur
        // croit avoir obtenu quelque chose.
        let statut = crossfeed_status_de_zone(&state.backend, id, enabled);
        if statut.unavailable {
            warn!(
                zone_id = id,
                requested = enabled,
                reason = statut
                    .reason
                    .map(tune_core::audio::crossfeed::CrossfeedConstraint::code),
                "zone_crossfeed_sans_effet"
            );
        }
        crossfeed_status = Some(statut);
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
        // #2742 — la moitié qui manquait : ce que ce réglage VAUT sur cette
        // zone. `null` quand le corps ne portait pas de `crossfeed` (rien n'a
        // été demandé, il n'y a rien à répondre). `unavailable: true` doit
        // VERROUILLER le contrôle côté client, `detail` l'expliquer.
        "crossfeed_status": crossfeed_status,
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
mod debit_de_zone_tests;

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

/// La sortie enregistrée pour une zone ne sait-elle mettre en attente qu'un
/// FICHIER local ? (`prefers_local_file_gapless`, OAAT en DSD natif / PCM direct)
///
/// `false` pour une sortie inconnue, comme pour l'immense majorité des sorties :
/// c'est le comportement par défaut du trait.
pub(crate) async fn output_prefers_local_file_gapless(
    state: &AppState,
    output_device_id: Option<&str>,
) -> bool {
    let Some(device_id) = output_device_id else {
        return false;
    };
    let Some(output) = ({ state.outputs.lock().await.get(device_id) }) else {
        return false;
    };
    output.lock().await.prefers_local_file_gapless()
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

/// Le nom PRÉSENTABLE d'un transport que le `match` de `build_signal_path` ne
/// nomme pas par un bras.
///
/// Le bras par défaut rendait le second membre du tuple — la chaîne BRUTE de
/// la colonne `zones.output_type` — comme nom de transport. Le panneau d'Alex
/// Campbell affichait donc « hqplayer » en minuscules là où toutes les autres
/// sorties affichent « DLNA/UPnP », « BluOS » ou « CoreAudio » (#2189).
///
/// Les types INCONNUS gardent leur chaîne : un greffon hors dépôt enregistre
/// le nom qu'il veut, et aucune règle de mise en forme ne saurait deviner sa
/// capitalisation. Inventer un libellé serait pire que de rendre le sien.
fn libelle_de_transport(output_type: &str) -> &str {
    match output_type {
        "hqplayer" => "HQPlayer",
        "diretta" => "Diretta",
        autre => autre,
    }
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
    //
    // ⚠️ Cette liste ÉTAIT recopiée ici. `orchestrator.rs` porte pourtant, en
    // toutes lettres, « l'unique exemplaire de cette liste » — et cette
    // quatrième copie avait déjà dérivé : cinq types au lieu de six,
    // `slimproto` manquant. Une zone Slimproto était donc « réseau » pour le
    // chemin audio (qui lui applique les forçages WAV/LPCM et le plafond
    // 16 bits) et « inconnue » pour le panneau, qui la déclarait non
    // bit-perfect sans jamais lire ces réglages. Le miroir suit désormais la
    // décision, par la MÊME fonction (#2189, même faute que #3183).
    let is_network_output = tune_core::orchestrator::is_network_output_type(Some(output_type));
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
        // AirPlay 1 comme AirPlay 2 : le protocole impose de l'ALAC 44,1/16.
        // La conversion a lieu POUR DE VRAI, le verdict `false` est donc juste
        // — c'est le LIBELLÉ qui manquait : sans ce bras, une zone AirPlay 2
        // (créée par `discovery_setup.rs`, `(Some(Box::new(ap2)), "airplay2")`)
        // tombait dans le fourre-tout et affichait « airplay2 » en minuscules
        // comme nom de transport (#2189).
        "airplay" => (false, "AirPlay", "ALAC"),
        "airplay2" => (false, "AirPlay 2", "ALAC"),
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
        // `slimproto` EST le protocole Squeezebox, et l'orchestrateur les
        // traite déjà à l'identique (`is_network_output_type` les liste tous
        // les deux). Le panneau, lui, ne nommait que `squeezebox` : une zone
        // créée par le serveur Slimproto (`tune-core/src/slimproto/mod.rs`,
        // `get_or_create(&player_name, Some("slimproto"), …)`) tombait dans le
        // fourre-tout et sortait « non bit-perfect » quoi qu'il arrive (#2189).
        "squeezebox" | "slimproto" => {
            let transport = if output_type == "slimproto" {
                "Slimproto"
            } else {
                "Squeezebox"
            };
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, transport, target.display_name())
            } else {
                (true, transport, format_name)
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
        // Tout le reste est une sortie PULL : elle va CHERCHER le flux
        // elle-même et reçoit nos octets TELS QUELS — `hqplayer`, `diretta`,
        // et tout greffon hors dépôt. Ce bras rendait `false`
        // INCONDITIONNELLEMENT, et son second membre — la chaîne brute de la
        // base — servait de nom de transport.
        //
        // Alex Campbell (Tune 0.9.98, Linux, sortie HQPlayer, fil 1524) :
        // « When playing local **or streaming** music files to HQPlayer, Tune
        // is reporting that it is transcoding. » Le « local OU streaming » est
        // le fait qui tranche : le symptôme est inconditionnel, ce qu'aucune
        // règle dépendant du format ne produirait. Une zone HQPlayer était
        // déclarée non bit-perfect sur un FLAC 44,1/16 servi octet pour octet,
        // sans EQ ni ReplayGain, sans qu'aucun transcodage n'ait lieu (#2189).
        //
        // Le verdict n'est plus écrit ici : il est LU du chemin audio, par la
        // fonction que celui-ci utilise pour décider
        // (`orchestrator::is_pull_dsp_output_type`, extraite de
        // `pull_output_needs_dsp_transcode`). Sur ces sorties le transport ne
        // touche aucun échantillon ; le seul traitement possible est celui que
        // cette même fonction force — EQ, correction de pièce, ReplayGain — et
        // il est déjà compté plus bas par `dsp_applique` et `replaygain_step`.
        // Le verdict global retombe donc à `false` dès qu'un égaliseur est
        // armé, exactement là où le transcodage a réellement lieu.
        other => (
            tune_core::orchestrator::is_pull_dsp_output_type(Some(other)),
            libelle_de_transport(other),
            format_name,
        ),
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
        // AirPlay 2 encode en ALAC 44,1/16 comme AirPlay 1 : l'étape est la
        // même, et elle manquait ici aussi (#2189).
        || matches!(output_type, "airplay" | "airplay2")
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
            inject_device_identity(
                obj,
                &state.backend,
                zone_id,
                z.output_device_id.as_deref(),
                detected_dev,
            );
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
                inject_device_identity(
                    obj,
                    &state.backend,
                    id,
                    zone.output_device_id.as_deref(),
                    detected_dev,
                );
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
        // #3254 — dire au JOURNAL, au moment du clic, que ce clic n'obtiendra
        // rien. La réponse porte déjà `mono_downmix_status` (la route rend la
        // fiche complète via `get_zone`), mais c'est ici que l'utilisateur croit
        // avoir obtenu quelque chose.
        //
        // ⚠️ On ne se sert PAS de la valeur rendue par `refresh_zone_mono_downmix`
        // comme signal de disponibilité : elle vaut `false` aussi bien parce que
        // la zone n'est pas locale que parce qu'aucune sortie n'est ouverte — la
        // même ambiguïté que `crossfeed_applied_live`. La règle, elle, ne dépend
        // que de la zone.
        let statut = tune_core::audio::mono_downmix::mono_downmix_status(
            enabled,
            tune_core::audio::mono_downmix::mono_downmix_runs_on_output(
                // La zone RELUE, pas `zone_before` : le même PATCH a pu changer
                // `output_device_id` quelques lignes plus haut, et c'est la
                // sortie d'APRÈS qui décide si le repli agira.
                repo.get(id)
                    .ok()
                    .flatten()
                    .and_then(|z| z.output_device_id)
                    .as_deref(),
            ),
            tune_core::audio::audiophile::zone_enabled(&state.backend, id),
        );
        if statut.unavailable {
            warn!(
                zone_id = id,
                requested = enabled,
                reason = statut.reason.map(|r| r.code()).unwrap_or_default(),
                "zone_mono_downmix_sans_effet — le réglage est enregistré mais rien ne l'applique sur cette zone"
            );
        }
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
mod signal_path_tests;

/// #1499 — une zone qui « joue » sans destination doit le dire.
///
/// Deux situations produisent le même symptôme (file remplie, position qui
/// avance, aucun son) et ne se distinguaient que dans les journaux : la zone
/// sans sortie associée, et la zone navigateur dont aucun onglet ne tire le
/// flux. Bilou a ouvert deux fils forum sur un défaut BluOS inexistant faute
/// de ce signal.
#[cfg(test)]
mod output_reach_tests;

#[cfg(test)]
mod patch_zone_deserialize_tests;

#[cfg(test)]
mod zone_group_tests;

#[cfg(test)]
mod aac_passthrough_tests;

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
mod patch_zone_error_guard;

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
mod charge_utile_zone_guard;

#[cfg(test)]
mod contrat_des_retours_anticipes;

/// #1395 — le backend de sortie locale réellement actif, et le motif du repli,
/// doivent arriver jusqu'au client.
#[cfg(test)]
mod backend_local_annonce_tests;
