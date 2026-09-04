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

mod lecture;
pub use lecture::*;

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

mod ecriture;
pub use ecriture::*;

mod peripheriques;
pub use peripheriques::*;

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
