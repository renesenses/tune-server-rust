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

mod dsp;
pub use dsp::*;

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

mod signal_path;
pub use signal_path::*;

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

mod peripheriques;
pub use peripheriques::*;

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

mod groupes;
pub use groupes::*;

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
