//! rc14 (rc13.91) — une étape de TESS (suréchantillonnage, convolution)
//! qui ne tourne pas dit POURQUOI.
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
use tune_core::tess::chaine::{ChaineTess, Upsampling};
use tune_core::tess::resolution::{CodeEtape, EtapeResolue, MotifEtape};

/// L'étape est-elle demandée, et active selon la résolution, ou déjà éteinte
/// par le rapprochement (`not_in_service`) ? Une étape éteinte pour une autre
/// raison (PURE, licence, transport…) garde la sienne.
fn eteignable(e: &EtapeResolue) -> bool {
    e.requested && (e.effective || e.reason == Some(MotifEtape::NotInService))
}

/// rc14 (rc13.91) — LA règle, la même pour tout étage ou greffon : ce que la
/// sortie dit ne pas avoir appliqué est publié `effective = false`,
/// `reason = not_in_service`, `detail` = code, et dans `params` le code
/// (`refusal`) et sa phrase (`refusal_text`). Le client affiche
/// `refusal_text` sur toute étape éteinte, sans cas particulier.
pub(crate) fn eteindre_avec_la_raison(e: &mut EtapeResolue, code: &str, texte: &str) {
    e.effective = false;
    e.reason = Some(MotifEtape::NotInService);
    e.detail = Some(code.to_owned());
    let mut params: Map<String, Value> = e
        .params
        .take()
        .and_then(|p| p.as_object().cloned())
        .unwrap_or_default();
    params.insert("refusal".into(), json!(code));
    params.insert("refusal_text".into(), json!(texte));
    e.params = Some(Value::Object(params));
}

/// Pose la raison du refus sur l'étape Upsampling (voir
/// [`eteindre_avec_la_raison`]). `refus` : le code publié par la sortie,
/// `None` = rien à dire.
///
/// Seulement quand la chaîne demande le suréchantillonnage PCM : le refus
/// vient du greffon PCM de la sortie. Vers le DSD (#9518), ce greffon est
/// éteint par projection et la sortie le déclare refusé (`disabled`) alors
/// que le modulateur, lui, tourne : l'étape ne doit pas se dire inactive.
pub(crate) fn avec_le_refus_du_surechantillonneur<'a>(
    etapes: impl IntoIterator<Item = &'a mut EtapeResolue>,
    chaine: &ChaineTess,
    refus: Option<&str>,
) {
    if !matches!(chaine.fixed.upsampling, Upsampling::Pcm { .. }) {
        return;
    }
    let Some(raison) = refus.filter(|r| !r.is_empty() && *r != "disabled") else {
        return;
    };
    let texte = super::description_du_refus_surechantillonnage(raison);
    for e in etapes {
        if e.code == CodeEtape::Upsampling && eteignable(e) {
            eteindre_avec_la_raison(e, raison, texte);
        }
    }
}

/// Le maillon `id` de la chaîne est-il porté par le CONVOLUEUR ?
///
/// `convolution`, toujours. `room-correction` seulement s'il désigne un
/// filtre (`ir_path` ou `filter`) sans bandes : le plus souvent, c'est une
/// façade d'ÉGALISEUR (bandes), que la cadence du filtre de convolution ne
/// concerne pas — elle s'applique, et doit rester « active ».
fn porte_par_le_convolueur(chaine: &ChaineTess, id: &str) -> bool {
    if id == tune_core::tess::migration::ID_CONVOLUTION {
        return true;
    }
    if id != tune_core::tess::migration::ID_CORRECTION_PIECE {
        return false;
    }
    chaine.plugins.iter().filter(|m| m.id == id).any(|m| {
        let filtre = ["ir_path", "filter"].iter().any(|k| {
            m.settings
                .get(*k)
                .and_then(Value::as_str)
                .is_some_and(|f| !f.trim().is_empty())
        });
        filtre && m.settings.get("bands").is_none()
    })
}

/// Rémi (fil 3097) — un préréglage rechargé affichait la Convolution
/// « active » alors que son filtre (384 kHz) ne s'applique pas au flux
/// (48 kHz, rééchantillonnage du filtre refusé par la zone). `raison` :
/// `(code, phrase)` de la correction de pièce non appliquée.
pub(crate) fn avec_la_raison_de_la_convolution<'a>(
    etapes: impl IntoIterator<Item = &'a mut EtapeResolue>,
    chaine: &ChaineTess,
    raison: Option<(&str, &str)>,
) {
    let Some((code, texte)) = raison else {
        return;
    };
    for e in etapes {
        let du_convolueur = e.code == CodeEtape::Plugin
            && e
                .plugin_id
                .as_deref()
                .is_some_and(|id| porte_par_le_convolueur(chaine, id));
        if du_convolueur && eteignable(e) {
            eteindre_avec_la_raison(e, code, texte);
        }
    }
}

/// La correction de pièce par réponse impulsionnelle que le convolueur
/// n'applique PAS au flux, et pourquoi : `(status, description)`. Seuls les
/// refus que TESS ne sait pas déjà (cadence, fichier illisible) ; PURE et la
/// licence ont leur propre motif dans la résolution.
pub(super) fn raison_de_la_correction_de_piece(
    cp: Option<&super::signal_path::CorrectionPiece>,
) -> Option<(&'static str, String)> {
    let cp = cp?;
    (cp.kind == "ir" && !cp.applied && matches!(cp.status, "rate_mismatch" | "unreadable"))
        .then(|| (cp.status, cp.description.clone()))
}

/// La même raison, lue dans l'étape `room_correction` du chemin historique.
pub(crate) fn raison_de_la_convolution_du_chemin(sp: &Value) -> Option<(String, String)> {
    let e = sp
        .get("steps")?
        .as_array()?
        .iter()
        .find(|e| e.get("code").and_then(Value::as_str) == Some("room_correction"))?;
    if e.get("kind").and_then(Value::as_str) != Some("ir")
        || e.get("applied").and_then(Value::as_bool) != Some(false)
    {
        return None;
    }
    let status = e.get("status").and_then(Value::as_str)?;
    if !matches!(status, "rate_mismatch" | "unreadable") {
        return None;
    }
    let texte = e.get("description").and_then(Value::as_str)?;
    Some((status.to_owned(), texte.to_owned()))
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

/// La raison de la correction de pièce non appliquée à la piste EN COURS,
/// pour l'écran TESS : la même règle que le chemin du signal historique
/// (`zone_correction_piece_lang`, cadence du flux servi). Rien ne joue, ou
/// cadence inconnue : `None`.
pub(super) async fn raison_de_la_convolution_en_cours(
    state: &crate::state::AppState,
    zone_id: i64,
    etat: &tune_core::playback::ZoneState,
) -> Option<(&'static str, String)> {
    let sid = etat.now_playing.as_ref()?.stream_id.clone()?;
    let cadence = state
        .streamer
        .stream_output_wire(&sid)
        .await
        .map(|w| w.sample_rate)
        .filter(|r| *r > 0)?;
    let cp = super::signal_path::zone_correction_piece_lang(
        super::textes_signal::langue_courante(),
        &state.backend,
        zone_id,
        Some(cadence),
    );
    raison_de_la_correction_de_piece(cp.as_ref())
}
