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
use tune_core::tess::migration::{ID_EQ_PARAMETRIQUE, facade_d_un_profil_ecrit, memes_bandes};

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

/// Écrire `nouveau` dans `zone_{id}_eq_profile` pose-t-il un NOUVEAU réglage
/// d'égaliseur paramétrique ?
///
/// Non si les bandes enregistrées restent les mêmes sans que le profil soit
/// rallumé (une zone qui avait déjà un paramétrique n'est bloquée dans aucun
/// autre geste), ni si la courbe est graphique, casque ou de pièce.
pub(crate) fn pose_un_parametrique(
    settings: &SettingsRepo,
    zone_id: i64,
    nouveau: &EqProfile,
) -> bool {
    if nouveau.bands.is_empty() {
        return false;
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
        return false;
    }
    if facade_d_un_profil_ecrit(settings, zone_id, nouveau) != ID_EQ_PARAMETRIQUE {
        return false;
    }
    !bandes_des_prereglages_casque(settings)
        .iter()
        .any(|b| memes_bandes(b, &nouveau.bands))
}

/// Le refus 402 de [`Feature::ParametricEq`] quand `nouveau` pose un réglage
/// paramétrique sans ce droit. Même corps que les autres routes
/// (`premium_guard`), message dans la langue de la requête.
pub(crate) async fn exiger_le_droit(
    state: &AppState,
    zone_id: i64,
    nouveau: &EqProfile,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if state.license.check_feature(Feature::ParametricEq).await {
        return Ok(());
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if !pose_un_parametrique(&settings, zone_id, nouveau) {
        return Ok(());
    }
    crate::premium_guard::require_premium_localise(&state.license, Feature::ParametricEq, headers)
        .await
}
