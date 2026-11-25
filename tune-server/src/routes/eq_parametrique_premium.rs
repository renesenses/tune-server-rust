//! rc14 — la règle Free/Premium de l'égaliseur PARAMÉTRIQUE sur les routes
//! historiques de l'égaliseur.
//!
//! Décision de Bertrand : TESS est disponible en Free et montre tous les
//! greffons ; les greffons Premium ne sont pas accessibles en Free.
//! L'égaliseur paramétrique est Premium ([`Feature::ParametricEq`], T19) ;
//! le graphique et le casque (AutoEQ, OPRA) restent gratuits
//! ([`Feature::DspEq`]).
//!
//! `PUT /zones/{id}/tess` applique déjà cette règle. Les trois routes qui
//! écrivent `zone_{id}_eq_profile` sans passer par TESS l'appliquent ici,
//! avec le même refus 402 que les autres routes ([`require_premium_localise`]) :
//! `PUT /zones/{id}/dsp`, `POST /zones/{id}/eq` et
//! `POST /eq/presets/{id}/activate`.
//!
//! La façade d'un profil est celle que lui donnerait la chaîne TESS
//! ([`facade_d_un_profil_ecrit`]) : graphique si la courbe tient sur la grille
//! ISO, casque si la zone porte déjà un égaliseur casque, correction de pièce
//! si ce sont les bandes du profil de pièce, paramétrique sinon. Des bandes
//! identiques à celles d'un préréglage importé d'AutoEQ ou d'OPRA sont un
//! égaliseur casque.
//!
//! Comme pour Crossfeed Pro et `PUT /zones/{id}/tess`, rien d'enregistré
//! n'est réécrit ni refusé quand le droit manque : une zone qui porte déjà un
//! paramétrique (licence expirée, migration) démarre et joue comme avant, et
//! ses bandes INCHANGÉES ne demandent aucun droit (réserve, extinction,
//! crossfeed dans le même corps). Seul un nouveau réglage paramétrique, ou le
//! rallumage du paramétrique enregistré, demande le Premium.
//!
//! [`require_premium_localise`]: crate::premium_guard::require_premium_localise
//! [`facade_d_un_profil_ecrit`]: tune_core::tess::migration::facade_d_un_profil_ecrit

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;
use tune_core::audio::eq::{EqBandSpec, EqProfile};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::tess::migration::{
    ID_CORRECTION_PIECE, ID_EQ_PARAMETRIQUE, ORIGINE_ESTIMATION, facade_d_un_profil_ecrit,
    memes_bandes, origine_de_la_piece,
};

use crate::state::AppState;

/// Les sources d'un préréglage importé qui en font un égaliseur casque
/// (`eq_pro::enregistrer_profil_autoeq`).
const SOURCES_CASQUE: [&str; 2] = ["autoeq", "opra"];

/// Les bandes des préréglages casque enregistrés (`eq_presets`).
fn bandes_des_prereglages_casque(settings: &SettingsRepo) -> Vec<Vec<EqBandSpec>> {
    let presets: Vec<Value> = settings
        .get("eq_presets")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    presets
        .iter()
        .filter(|p| {
            p["source"]
                .as_str()
                .is_some_and(|s| SOURCES_CASQUE.contains(&s))
        })
        .map(|p| {
            p["bands"]
                .as_array()
                .map(|b| {
                    b.iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// `nouveau` porte-t-il les bandes d'un profil de pièce CALCULÉ SANS MICRO
/// de la zone (D1, Free) ? Toute autre correction de pièce est mesurée, donc
/// Premium (même règle que `origines_par_le_serveur`).
fn correction_estimee_de_la_zone(settings: &SettingsRepo, zone_id: i64, nouveau: &EqProfile) -> bool {
    let Some(brut) = settings
        .get(&format!("room_profile_{zone_id}"))
        .ok()
        .flatten()
    else {
        return false;
    };
    let (Ok(v), Ok(piece)) = (
        serde_json::from_str::<Value>(&brut),
        serde_json::from_str::<tune_core::room_correction::RoomProfile>(&brut),
    ) else {
        return false;
    };
    origine_de_la_piece(&v) == ORIGINE_ESTIMATION
        && memes_bandes(
            &tune_core::room_correction::profil_eq_de_correction(&piece.filters).bands,
            &nouveau.bands,
        )
}

/// Le droit Premium qu'écrire `nouveau` dans `zone_{id}_eq_profile` demande,
/// ou `None`.
///
/// Rien si les bandes enregistrées restent les mêmes sans que le profil soit
/// rallumé (une zone qui avait déjà un paramétrique ou une correction n'est
/// bloquée dans aucun autre geste), ni pour une courbe graphique ou casque.
/// [`Feature::ParametricEq`] pour un paramétrique ; [`Feature::RoomCorrection`]
/// pour une correction de pièce MESURÉE (posée ou RALLUMÉE : sans cela, un
/// compte gratuit rallumait par `POST /zones/{id}/eq` une correction mesurée
/// éteinte). Une correction calculée sans micro reste libre (D1).
pub(crate) fn droit_demande(
    settings: &SettingsRepo,
    zone_id: i64,
    nouveau: &EqProfile,
) -> Option<Feature> {
    if nouveau.bands.is_empty() {
        return None;
    }
    let enregistre: Option<EqProfile> = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    if let Some(e) = &enregistre
        && memes_bandes(&e.bands, &nouveau.bands)
        && (e.enabled || !nouveau.enabled)
    {
        return None;
    }
    match facade_d_un_profil_ecrit(settings, zone_id, nouveau) {
        ID_EQ_PARAMETRIQUE => (!bandes_des_prereglages_casque(settings)
            .iter()
            .any(|b| memes_bandes(b, &nouveau.bands)))
        .then_some(Feature::ParametricEq),
        ID_CORRECTION_PIECE => (!correction_estimee_de_la_zone(settings, zone_id, nouveau))
            .then_some(Feature::RoomCorrection),
        _ => None,
    }
}

/// Le refus 402 du droit que `nouveau` demande ([`droit_demande`] :
/// [`Feature::ParametricEq`] ou [`Feature::RoomCorrection`]) quand le compte
/// ne l'a pas. Même corps que les autres routes
/// (`premium_guard`), message dans la langue de la requête.
pub(crate) async fn exiger_le_droit(
    state: &AppState,
    zone_id: i64,
    nouveau: &EqProfile,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let Some(droit) = droit_demande(&settings, zone_id, nouveau) else {
        return Ok(());
    };
    if state.license.check_feature(droit).await {
        return Ok(());
    }
    crate::premium_guard::require_premium_localise(&state.license, droit, headers).await
}
