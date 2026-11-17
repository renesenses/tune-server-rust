//! #8987 (fil 2708, Windows, DAC Denafrips) — un appareil déjà lié à une zone.
//!
//! Trois défauts de la même famille : l'utilisateur ne pouvait pas savoir
//! qu'un appareil était déjà tenu par une zone.
//!
//! 1. `POST /zones` sur un appareil qui a déjà une zone visible rendait cette
//!    zone en `200 OK`, nom demandé écarté : refus `409 zone_existe_deja`,
//!    voir `ecriture::nom_de_zone_existante` (décision de Bertrand, 17/11).
//! 2. Changer l'appareil d'une zone (`PATCH /zones/{id}` avec
//!    `output_device_id`) vers un appareil tenu par une zone SUPPRIMÉE
//!    (masquée) heurtait l'index unique partiel `idx_zones_output_device_id` :
//!    `500 zone_field_write_failed — UNIQUE constraint failed`. La zone
//!    supprimée lâche désormais la sortie ([`liberer_la_sortie_demandee`]) ;
//!    une zone ACTIVE qui la tient donne un `409 sortie_tenue_par_une_zone`
//!    qui la nomme. Plus jamais de 500 pour ce cas.
//! 3. Une zone restaurée garde l'identifiant de l'hôte audio sous lequel elle
//!    est née : zone ASIO restaurée pendant que le serveur tourne en WASAPI
//!    (`reassociation_de_zone_locale_refusee … vient de Asio`). La
//!    ré-association automatique est impossible SANS risque : l'identifiant
//!    ASIO EST le nom du pilote (`outputs::identite_de_sortie`), et le même
//!    DAC porte sous WASAPI un AUTRE nom (« Haut-parleurs (…) ») — s'il
//!    portait le même, ce serait la même clé de registre et il n'y aurait
//!    rien à ré-associer. Rapprocher deux noms serait un pari (le cas Apple
//!    TV / Sonos). La restauration le SIGNALE donc
//!    ([`signaler_la_restauration_hors_hote`]) : `avertissement_restauration`
//!    dans la réponse, phrase traduite qui dit de repasser sur l'autre hôte
//!    ou de choisir l'appareil de l'hôte actif.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// Qui tient la sortie qu'une zone demande ?
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OccupationDeLaSortie {
    /// Personne.
    Libre,
    /// La zone elle-même.
    CetteZone,
    /// Une zone supprimée (masquée) : elle lâchera la sortie.
    ZoneSupprimee(i64),
    /// Une zone active : refus nommé.
    ZoneActive { id: i64, nom: String },
}

/// LA règle, pure. `occupante` : `(id, masquée, nom)` de la ligne qui porte
/// la sortie, masquées comprises.
pub(crate) fn occupation(
    zone_id: i64,
    occupante: Option<(i64, bool, String)>,
) -> OccupationDeLaSortie {
    match occupante {
        None => OccupationDeLaSortie::Libre,
        Some((id, _, _)) if id == zone_id => OccupationDeLaSortie::CetteZone,
        Some((id, true, _)) => OccupationDeLaSortie::ZoneSupprimee(id),
        Some((id, false, nom)) => OccupationDeLaSortie::ZoneActive { id, nom },
    }
}

/// L'occupation lue en base (lecture forte : `get_by_device_id` puis l'état
/// de masquage de la ligne trouvée, par son id).
pub(crate) fn constater(repo: &ZoneRepo, zone_id: i64, device_id: &str) -> OccupationDeLaSortie {
    let occupante = repo
        .get_by_device_id(device_id)
        .ok()
        .flatten()
        .and_then(|z| {
            let id = z.id?;
            let masquee = repo
                .etat_de_masquage(id)
                .ok()
                .flatten()
                .is_some_and(|e| e.masquee);
            Some((id, masquee, z.name))
        });
    occupation(zone_id, occupante)
}

/// `409 sortie_tenue_par_une_zone` : la phrase traduite nomme la zone qui
/// tient l'appareil, `holder_zone_id` / `holder_zone_name` permettent de
/// l'ouvrir.
pub(crate) fn refus_sortie_tenue(
    zone_id: i64,
    device_id: &str,
    occupante: i64,
    nom: &str,
) -> Response {
    warn!(
        zone_id,
        device_id,
        zone_occupante = occupante,
        nom_occupante = %nom,
        "zone_patch_sortie_tenue_par_une_autre_zone"
    );
    let mut corps = crate::message_api::corps_refus(
        "sortie_tenue_par_une_zone",
        "api.zones.deviceHeldByZone",
        &[("zone", nom.to_string())],
    );
    corps["zone_id"] = json!(zone_id);
    corps["output_device_id"] = json!(device_id);
    corps["holder_zone_id"] = json!(occupante);
    corps["holder_zone_name"] = json!(nom);
    (StatusCode::CONFLICT, Json(corps)).into_response()
}

/// Avant l'écriture de `output_device_id` : une zone supprimée qui tient la
/// sortie la lâche ; une zone active la garde, et c'est un refus nommé.
pub(crate) fn liberer_la_sortie_demandee(
    repo: &ZoneRepo,
    zone_id: i64,
    device_id: &str,
) -> Result<(), Response> {
    match constater(repo, zone_id, device_id) {
        OccupationDeLaSortie::Libre | OccupationDeLaSortie::CetteZone => Ok(()),
        OccupationDeLaSortie::ZoneActive { id, nom } => {
            Err(refus_sortie_tenue(zone_id, device_id, id, &nom))
        }
        OccupationDeLaSortie::ZoneSupprimee(supprimee) => {
            match repo.liberer_la_sortie_d_une_zone_masquee(supprimee) {
                Ok(n) if n > 0 => {
                    info!(
                        zone_id,
                        device_id,
                        zone_supprimee = supprimee,
                        "sortie_liberee_d_une_zone_supprimee"
                    );
                    Ok(())
                }
                // Restaurée entre la lecture et l'écriture : elle est
                // redevenue active, on relit pour la nommer.
                Ok(_) => match constater(repo, zone_id, device_id) {
                    OccupationDeLaSortie::ZoneActive { id, nom } => {
                        Err(refus_sortie_tenue(zone_id, device_id, id, &nom))
                    }
                    _ => Ok(()),
                },
                Err(e) => Err(super::ecriture::echec_ecriture(
                    zone_id,
                    "output_device_id",
                    device_id,
                    e,
                )),
            }
        }
    }
}

/// L'écriture de `output_device_id` a échoué malgré tout (course entre deux
/// gestes) : si une zone active tient la sortie, c'est un `409` qui la
/// nomme, pas un 500. Sinon, l'échec d'écriture journalisé habituel.
pub(crate) fn refus_apres_echec(
    repo: &ZoneRepo,
    zone_id: i64,
    device_id: &str,
    erreur: String,
) -> Response {
    match constater(repo, zone_id, device_id) {
        OccupationDeLaSortie::ZoneActive { id, nom } => {
            refus_sortie_tenue(zone_id, device_id, id, &nom)
        }
        _ => super::ecriture::echec_ecriture(zone_id, "output_device_id", device_id, erreur),
    }
}

// ── 3. Restauration sous un autre hôte audio ───────────────────────────────

/// La sortie de la zone s'ouvre-t-elle en ASIO ? `Some(true)` / `Some(false)`
/// quand on le sait, `None` sinon.
///
/// Le backend de la sortie ENREGISTRÉE fait foi (`LocalOutput::audio_backend`,
/// rectifié par l'hôte d'origine) ; à défaut — sortie absente du registre —
/// le préfixe de l'identifiant d'endpoint enregistré sur la zone
/// (`asio:<pilote>`, `wasapi:{guid}`, `coreaudio:…`, `alsa:…`).
pub(crate) fn sortie_en_asio(
    backend_enregistre: Option<&str>,
    endpoint_id: Option<&str>,
) -> Option<bool> {
    use tune_core::outputs::identite_de_sortie::{OrigineDeLIdentifiant, origine_de_l_identifiant};
    if let Some(b) = backend_enregistre.map(str::trim).filter(|b| !b.is_empty()) {
        return Some(b.eq_ignore_ascii_case("asio"));
    }
    match origine_de_l_identifiant(endpoint_id?.trim()) {
        OrigineDeLIdentifiant::Asio => Some(true),
        OrigineDeLIdentifiant::Wasapi
        | OrigineDeLIdentifiant::CoreAudio
        | OrigineDeLIdentifiant::Alsa => Some(false),
        OrigineDeLIdentifiant::Inconnue => None,
    }
}

/// La zone restaurée appartient-elle à l'AUTRE hôte que celui choisi ? La
/// seule frontière qui existe est « ASIO ou pas » (`auto` et `wasapi`
/// ouvrent le même hôte, voir `hors_backend_choisi`). Inconnu : rien à dire.
pub(crate) fn restauree_hors_hote_actif(backend_configure: &str, en_asio: Option<bool>) -> bool {
    let configure_asio = backend_configure.trim().eq_ignore_ascii_case("asio");
    en_asio.is_some_and(|a| a != configure_asio)
}

fn nom_d_hote(asio: bool) -> &'static str {
    if asio { "ASIO" } else { "WASAPI" }
}

/// Pose `avertissement_restauration` sur la réponse d'une restauration quand
/// la zone locale restaurée relève de l'autre hôte audio. Rien pour une zone
/// non locale ou dont l'hôte est inconnu.
pub(crate) async fn signaler_la_restauration_hors_hote(
    state: &AppState,
    zone_id: i64,
    reponse: &mut Value,
) {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let Some(zone) = repo.get(zone_id).ok().flatten() else {
        return;
    };
    let Some(device_id) = zone
        .output_device_id
        .as_deref()
        .filter(|d| d.starts_with("local:"))
    else {
        return;
    };
    let backend_enregistre = super::backend_de_la_sortie_de_la_zone(state, Some(device_id)).await;
    let endpoint = repo.endpoint_id_de_la_sortie(device_id);
    let en_asio = sortie_en_asio(backend_enregistre.as_deref(), endpoint.as_deref());
    let configure = state.effective_audio_backend();
    if !restauree_hors_hote_actif(&configure, en_asio) {
        return;
    }
    let hote_de_la_zone = nom_d_hote(en_asio == Some(true));
    let hote_actif = nom_d_hote(configure.trim().eq_ignore_ascii_case("asio"));
    warn!(
        zone_id,
        device_id,
        hote_de_la_zone,
        hote_actif,
        endpoint_id = ?endpoint,
        "zone_restauree_sur_un_autre_hote_audio"
    );
    let avertissement = crate::message_api::avec_message(
        json!({
            "code": "zone_restauree_autre_hote_audio",
            "hote_de_la_zone": hote_de_la_zone,
            "hote_actif": hote_actif,
        }),
        "api.zones.restoredOnOtherAudioHost",
        &[
            ("zone", zone.name.clone()),
            ("zoneHost", hote_de_la_zone.to_string()),
            ("activeHost", hote_actif.to_string()),
        ],
    );
    if let Some(obj) = reponse.as_object_mut() {
        obj.insert("avertissement_restauration".into(), avertissement);
    }
}

#[cfg(test)]
#[path = "zone_appareil_8987_tests.rs"]
mod tests;
