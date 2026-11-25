//! rc14 (rc13.91) — l'étape « Upsampling » de TESS dit POURQUOI elle ne
//! tourne pas.
//!
//! Relevé de Xavier (Windows, WASAPI partagé, 25/11) : suréchantillonnage
//! activé et enregistré, aucun changement au DAC. La sortie locale refusait
//! la piste (`rates_not_measured`) et le disait au chemin du signal
//! historique (`upsampling.reason`), mais TESS ne le voyait pas :
//!
//! - l'écran TESS (`GET /zones/{id}/tess`) ne publie que la RÉSOLUTION de la
//!   chaîne : l'étape restait effective (« PCM · Max · filtre ») ;
//! - le bloc `tess` du chemin du signal la rapprochait bien du réel, mais
//!   avec le seul motif générique `not_in_service`.
//!
//! Les deux reçoivent ici la raison de la sortie : `reason = not_in_service`,
//! `detail` = le code, et dans `params` le code (`refusal`) et sa phrase dans
//! la langue de la requête (`refusal_text`).

use serde_json::{Map, Value, json};
use tune_core::tess::resolution::{CodeEtape, EtapeResolue, MotifEtape};

/// Pose la raison du refus sur l'étape Upsampling demandée qui tourne (selon
/// la résolution) ou que le rapprochement a déjà éteinte (`not_in_service`).
/// Une étape éteinte pour une autre raison (PURE, licence, transport…) garde
/// la sienne. `refus` : le code publié par la sortie, `None` = rien à dire.
pub(crate) fn avec_le_refus_du_surechantillonneur<'a>(
    etapes: impl IntoIterator<Item = &'a mut EtapeResolue>,
    refus: Option<&str>,
) {
    let Some(raison) = refus.filter(|r| !r.is_empty()) else {
        return;
    };
    for e in etapes {
        let concernee = e.code == CodeEtape::Upsampling
            && e.requested
            && (e.effective || e.reason == Some(MotifEtape::NotInService));
        if !concernee {
            continue;
        }
        e.effective = false;
        e.reason = Some(MotifEtape::NotInService);
        e.detail = Some(raison.to_owned());
        let mut params: Map<String, Value> = e
            .params
            .take()
            .and_then(|p| p.as_object().cloned())
            .unwrap_or_default();
        params.insert("refusal".into(), json!(raison));
        params.insert(
            "refusal_text".into(),
            json!(super::description_du_refus_surechantillonnage(raison)),
        );
        e.params = Some(Value::Object(params));
    }
}

/// Le refus publié par le chemin du signal historique (`upsampling.reason`
/// quand `upsampling.active` est faux).
pub(crate) fn refus_du_chemin_historique(sp: &Value) -> Option<&str> {
    let up = sp.get("upsampling")?;
    if up.get("active").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    up.get("reason").and_then(Value::as_str)
}

/// Le refus que la sortie LOCALE a déclaré pour la piste en cours.
pub(crate) fn refus_de_la_sortie_locale(
    output_type: Option<&str>,
    etat: &tune_core::playback::ZoneState,
) -> Option<&'static str> {
    if output_type.unwrap_or("local") != "local" {
        return None;
    }
    etat.now_playing.as_ref()?;
    etat.transformations_reelles
        .as_ref()
        .and_then(|t| t.refus_surechantillonnage())
}
