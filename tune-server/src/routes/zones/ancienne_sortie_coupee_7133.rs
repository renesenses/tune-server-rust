//! #7133 — le son sort à la fois sur le DAC et sur les haut-parleurs du PC.
//!
//! Terrain (Lulu, fil 2467, 1.0.0-rc8, Windows, WASAPI) : « Double sortie à
//! l'écoute, celle du Dac sur les enceintes et celle du HP du PC, voire (une
//! fois), un morceau différent sur le HP du Pc et sur la sortie Dac ».
//!
//! La cause trouvée dans le code : changer l'APPAREIL d'une zone qui joue ne
//! coupait pas l'appareil qu'elle quittait. Le client web change la sortie
//! d'une zone par `PATCH /zones/{id}` (`changeZoneOutput`, écran Zones) ; la
//! route écrivait le nouvel `output_device_id` en base et s'arrêtait là.
//! L'ancienne sortie — une `LocalOutput` WASAPI, avec son fil de lecture et
//! son flux cpal — restait au registre et continuait de jouer : rien ne lui
//! envoyait d'ordre, puisque toutes les commandes de la zone (lecture, pause,
//! arrêt, suivante) visent désormais la NOUVELLE sortie, résolue en base.
//!
//! D'où les deux symptômes du fil, avec un seul défaut :
//!
//! - le même morceau des deux côtés : l'ancienne sortie finit sa piste, et la
//!   suivante qu'elle avait déjà en réserve pour l'enchaînement sans blanc
//!   (`set_next_media`), pendant que la zone relance sur la nouvelle ;
//! - un morceau différent : la zone avance (ou est relancée) sur la nouvelle
//!   sortie pendant que l'ancienne joue encore la piste d'avant.
//!
//! `POST /zone-manager/zones/{id}/hot-swap` arrêtait déjà l'ancienne sortie
//! d'une zone qui joue ; `PATCH /zones/{id}` et l'activation d'un profil de
//! zones, les deux autres chemins par lesquels l'utilisateur change l'appareil
//! d'une zone (#6774 les recense), ne le faisaient pas.
//!
//! Le geste retenu est l'ARRÊT de la zone, pas un transfert de la lecture sur
//! le nouvel appareil : c'est ce que fait déjà le hot-swap, et relancer la
//! piste ailleurs serait un comportement nouveau. Une zone en PAUSE est
//! arrêtée aussi : sa sortie garde son flux ouvert (et, en exclusif, le
//! périphérique), et une reprise demandée par la zone qui hériterait de cet
//! appareil rouvrirait l'ancienne lecture.

use super::*;

/// La zone `zone_id` vient de passer de `ancienne` à `nouvelle` : l'ancienne
/// sortie doit se taire.
///
/// Rend `true` quand la zone jouait (ou était en pause) et a été arrêtée sur
/// son ancienne sortie, `false` quand il n'y avait rien à couper : pas
/// d'ancienne sortie (zone navigateur, zone neuve), même appareil, ou zone à
/// l'arrêt.
///
/// L'arrêt passe par l'orchestrateur, avec l'ancienne sortie NOMMÉE : sans elle,
/// `stop` relirait la zone en base et arrêterait la nouvelle, qui ne joue pas.
pub(crate) async fn couper_l_ancienne_sortie(
    state: &AppState,
    zone_id: i64,
    ancienne: Option<&str>,
    nouvelle: Option<&str>,
) -> bool {
    let Some(ancienne) = ancienne.map(str::trim).filter(|d| !d.is_empty()) else {
        return false;
    };
    if nouvelle.map(str::trim) == Some(ancienne) {
        return false;
    }
    let etat = state.playback.get_state(zone_id).await.state;
    if !matches!(etat, PlayState::Playing | PlayState::Paused) {
        return false;
    }
    info!(
        zone_id,
        ancienne,
        nouvelle = nouvelle.unwrap_or("<aucune>"),
        "zone_output_changed_old_output_stopped"
    );
    state.orchestrator.stop(zone_id, Some(ancienne)).await;
    true
}
