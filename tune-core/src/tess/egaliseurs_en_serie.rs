//! rc14 (retours de Rémi, GO de Bertrand du 25/11) — les égaliseurs de la
//! chaîne TESS sont **combinables** et s'appliquent **en série**, « Bypass
//! all » coupe **tout**, et le gain d'une bande va jusqu'à −30 dB.
//!
//! # Égaliseurs en série
//!
//! Graphique, paramétrique, casque et correction de pièce peuvent être
//! cochés ensemble. Le moteur les applique tous, dans l'ordre de la chaîne :
//! chaque façade cochée garde SES bandes, qui deviennent autant de biquads
//! (chacun avec son propre état) mis bout à bout dans un seul processeur
//! ([`combiner_en_serie`]). Une cascade de biquads EST une mise en série :
//! la réponse combinée est le produit des réponses de chaque égaliseur.
//!
//! La marge anti-écrêtage est calculée UNE fois, sur la réponse combinée
//! (norme L1 de toute la cascade, ou maximum réel en mode « Réaliste ») :
//! jamais la somme des réserves de chaque égaliseur, qui baisserait le
//! niveau pour rien. Avec un headroom MANUEL dans la chaîne, c'est lui, et
//! lui seul, qui fait la marge ([`EqProfile::reserve_imposee_db`]).
//!
//! Compatibilité : `zone_{id}_eq_profile` reste l'égaliseur PRINCIPAL (la
//! première façade cochée), que lisent et écrivent les anciennes routes
//! (`/zones/{id}/eq`, `/zones/{id}/dsp`). Le moteur ne lit la chaîne
//! ([`egaliseurs_de_zone`]) que lorsqu'elle porte au moins deux façades, ou
//! un headroom manuel : sinon, rien ne change, au bit près.
//!
//! # Bypass all
//!
//! [`bypass_all_de_zone`] est lu par `audio::audiophile::traitements_contournes`,
//! la garde que tous les chargeurs DSP consultent déjà (PURE, mesure de
//! pièce) : greffons, égaliseurs, convolution, crossfeed, natifs tiers, repli
//! mono, réaffectation, suréchantillonnage, cadence fixe, ReplayGain et
//! fondus. Restent le volume de la zone (et son trim) et la quantification
//! qu'il exige, comme le dit le contrat (§ 3).

use serde_json::Value;

use super::chaine::{ChaineTess, ErreurChaine, Headroom, MaillonGreffon, VERSION_CHAINE};
use super::migration::{
    CHAMP_ORIGINE, FACADES_EQ, GAIN_GRAPHIQUE_MAX_DB, ID_EQ_GRAPHIQUE, MARQUE_REMPLACE, lire_zone,
};
use crate::audio::eq::{EqBandSpec, EqProfile, HeadroomMode};
use crate::db::settings_repo::SettingsRepo;

/// Plage d'usage du gain d'une bande du paramétrique, du casque et de la
/// correction de pièce, en dB (publiée dans le catalogue, vérifiée au `PUT`).
pub const GAIN_PARAMETRIQUE_MIN_DB: f64 = -30.0;
pub const GAIN_PARAMETRIQUE_MAX_DB: f64 = 15.0;

/// Plafond du casque : des corrections OPRA montent jusqu'à +20 dB (197
/// profils au-dessus de +15 dB, #9519). Sous le plafond du moteur (+24 dB).
pub const GAIN_CASQUE_MAX_DB: f64 = 20.0;

/// Les bornes de gain d'une bande pour cette façade : ±12 dB pour le
/// graphique (curseurs, conversion P2), −30 / +20 dB pour le casque,
/// −30 / +15 dB pour le paramétrique et la correction de pièce.
pub fn bornes_de_gain(id: &str) -> Option<(f64, f64)> {
    if id == ID_EQ_GRAPHIQUE {
        Some((-GAIN_GRAPHIQUE_MAX_DB, GAIN_GRAPHIQUE_MAX_DB))
    } else if id == super::migration::ID_EQ_CASQUE {
        Some((GAIN_PARAMETRIQUE_MIN_DB, GAIN_CASQUE_MAX_DB))
    } else if FACADES_EQ.contains(&id) {
        Some((GAIN_PARAMETRIQUE_MIN_DB, GAIN_PARAMETRIQUE_MAX_DB))
    } else {
        None
    }
}

/// Publie les bornes de gain dans le schéma de réglages d'une façade, au
/// chemin `properties.bands.items.properties.gain.minimum` / `.maximum`
/// (la référence `$ref` vers `EqBandSpec` est résolue sur place).
pub fn borner_le_schema(id: &str, schema: &mut Value) {
    let Some((min, max)) = bornes_de_gain(id) else {
        return;
    };
    let bande = schema
        .pointer("/$defs/EqBandSpec")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
    let Some(items) = schema
        .pointer_mut("/properties/bands")
        .and_then(Value::as_object_mut)
        .map(|b| b.entry("items").or_insert(Value::Null))
    else {
        return;
    };
    let resolu = match items {
        Value::Object(o) if !o.contains_key("$ref") => Value::Object(o.clone()),
        _ => bande,
    };
    *items = resolu;
    if let Some(props) = items
        .as_object_mut()
        .map(|o| {
            o.entry("properties")
                .or_insert_with(|| serde_json::json!({}))
        })
        .and_then(Value::as_object_mut)
    {
        let gain = props
            .entry("gain")
            .or_insert_with(|| serde_json::json!({ "type": "number" }));
        if let Some(g) = gain.as_object_mut() {
            g.insert("minimum".into(), serde_json::json!(min));
            g.insert("maximum".into(), serde_json::json!(max));
        }
    }
}

/// Une bande à gain (cloche, plateaux) : les autres ignorent le gain.
fn bande_a_gain(b: &Value) -> bool {
    matches!(
        b.get("type").and_then(Value::as_str).unwrap_or("peak"),
        "peak" | "low_shelf" | "high_shelf"
    )
}

/// `PUT` : le gain de chaque bande d'une façade dont les réglages CHANGENT
/// reste dans [`bornes_de_gain`] (409 `invalid_settings` sinon). Un maillon
/// inchangé n'est pas revérifié : un profil enregistré par une ancienne route
/// (jusqu'à ±24 dB) se garde tel quel.
pub fn verifier_les_gains(
    chaine: &ChaineTess,
    enregistree: &ChaineTess,
) -> Result<(), ErreurChaine> {
    for m in &chaine.plugins {
        let Some((min, max)) = bornes_de_gain(&m.id) else {
            continue;
        };
        let inchange = enregistree
            .plugins
            .iter()
            .any(|e| e.id == m.id && e.settings == m.settings);
        if inchange {
            continue;
        }
        let Some(bandes) = m.settings.get("bands").and_then(Value::as_array) else {
            continue;
        };
        for (i, b) in bandes.iter().enumerate() {
            let Some(gain) = b.get("gain") else {
                continue;
            };
            let g = gain.as_f64().unwrap_or(f64::NAN);
            if !g.is_finite() || (bande_a_gain(b) && !(min..=max).contains(&g)) {
                return Err(ErreurChaine::ReglagesInvalides {
                    id: m.id.clone(),
                    raison: format!("band {i}: gain {gain} dB outside [{min}, {max}] dB"),
                });
            }
        }
    }
    Ok(())
}

/// Le profil que porte une façade de la chaîne (sans l'origine D1 ni la
/// marque de groupe), activé ; `None` si ses réglages ne sont pas un profil.
pub fn profil_du_maillon(m: &MaillonGreffon) -> Option<EqProfile> {
    let Value::Object(s) = &m.settings else {
        return None;
    };
    let mut s = s.clone();
    s.remove(CHAMP_ORIGINE);
    s.remove(MARQUE_REMPLACE);
    s.insert("enabled".into(), Value::Bool(true));
    serde_json::from_value(Value::Object(s)).ok()
}

/// Les bandes qu'un profil fait jouer, dans l'ordre : ses bandes expertes, ou
/// la cascade des trois curseurs (grave 80 Hz, médium 2 kHz, aigu 10 kHz),
/// exactement celle du moteur quand le profil n'a pas de bandes.
pub fn bandes_jouees(p: &EqProfile) -> Vec<EqBandSpec> {
    if !p.bands.is_empty() {
        return p.bands.clone();
    }
    let (grave, medium, aigu) = p.effective_gains();
    if grave.abs() <= 0.01 && medium.abs() <= 0.01 && aigu.abs() <= 0.01 {
        return Vec::new();
    }
    let bande = |freq: f64, gain: f64, q: f64, t: &str| EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        channel: None,
    };
    vec![
        bande(80.0, grave, 0.707, "low_shelf"),
        bande(2000.0, medium, 1.0, "peak"),
        bande(10000.0, aigu, 0.707, "high_shelf"),
    ]
}

/// Les égaliseurs `profils`, en série et dans cet ordre, en un seul profil :
/// les bandes de chacun mises bout à bout. Un seul égaliseur est rendu tel
/// quel. Réserve : « Réaliste » seulement si tous la demandent, sinon
/// « Sûre » (la norme L1 de la cascade ENTIÈRE).
pub fn combiner_en_serie(profils: &[EqProfile]) -> Option<EqProfile> {
    let (premier, autres) = profils.split_first()?;
    let mut p = premier.clone();
    p.enabled = true;
    if autres.is_empty() {
        return Some(p);
    }
    let bandes: Vec<EqBandSpec> = profils.iter().flat_map(bandes_jouees).collect();
    if !bandes.is_empty() {
        p.bands = bandes;
    }
    p.headroom_mode = if profils
        .iter()
        .all(|x| x.headroom_mode == HeadroomMode::Realistic)
    {
        HeadroomMode::Realistic
    } else {
        HeadroomMode::Safe
    };
    Some(p)
}

/// rc14 — la clé que rend [`super::migration::ecrire_chaine`] quand l'ordre
/// des greffons cochés change : la route qui écrit pose alors le nouvel
/// ordre sur la sortie locale vivante. Ce n'est pas un réglage enregistré.
pub fn cle_ordre_tess(zone_id: i64) -> String {
    format!("{}#order", super::cle_chaine_zone(zone_id))
}

/// L'ordre des FAMILLES d'étages de la sortie locale que donnent les
/// greffons cochés (0 égaliseurs, 1 convolution, 2 crossfeed et natifs), les
/// familles non nommées à leur rang historique — la même règle que
/// `outputs::local::executeur_tess::OrdreLocal::depuis_ids`. Cocher ou
/// décocher sans rien déplacer ne le change pas.
pub fn ordre_des_coches(chaine: &ChaineTess) -> Vec<u8> {
    let catalogue = super::catalogue_de_reference();
    let mut ordre: Vec<u8> = Vec::with_capacity(3);
    for m in chaine.plugins.iter().filter(|m| m.enabled) {
        let id = m.id.as_str();
        let famille = if FACADES_EQ.contains(&id) {
            0
        } else if id == "convolution" {
            1
        } else if id == "crossfeed"
            || id == crate::audio::crossfeed::ID_CROSSTALK
            || id.starts_with(super::greffon::PREFIXE_FAMILLE_CROSSFEED)
            || catalogue.get(id).is_none()
        {
            2
        } else {
            continue;
        };
        if !ordre.contains(&famille) {
            ordre.push(famille);
        }
    }
    for f in 0..3 {
        if !ordre.contains(&f) {
            ordre.push(f);
        }
    }
    ordre
}

/// La chaîne ENREGISTRÉE de la zone, sans la relire (rapide) ; la chaîne par
/// défaut (vide) si elle est absente ou illisible.
pub fn chaine_de_zone(settings: &SettingsRepo, zone_id: i64) -> ChaineTess {
    chaine_brute(settings, zone_id).unwrap_or_default()
}

/// La chaîne ENREGISTRÉE de la zone, sans la relire (rapide).
fn chaine_brute(settings: &SettingsRepo, zone_id: i64) -> Option<ChaineTess> {
    let brut = settings
        .get(&super::cle_chaine_zone(zone_id))
        .ok()
        .flatten()?;
    serde_json::from_str(&brut).ok()
}

/// « Bypass all » est-il coché sur la chaîne de la zone ?
pub fn bypass_all_de_zone(settings: &SettingsRepo, zone_id: i64) -> bool {
    settings
        .get(&super::cle_chaine_zone(zone_id))
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .and_then(|v| v.get("bypass_all").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Les égaliseurs cochés d'une chaîne, dans l'ordre, et la marge manuelle.
/// `principal` sert à une façade sans réglages (`null`) : c'est
/// `zone_{id}_eq_profile`.
pub fn profil_de_la_chaine(
    chaine: &ChaineTess,
    principal: Option<&EqProfile>,
) -> Option<EqProfile> {
    let mut profils = Vec::new();
    for m in chaine
        .plugins
        .iter()
        .filter(|m| m.enabled && FACADES_EQ.contains(&m.id.as_str()))
    {
        match profil_du_maillon(m) {
            Some(p) => profils.push(p),
            None if m.settings.is_null() && profils.is_empty() => {
                if let Some(p) = principal {
                    profils.push(p.clone());
                }
            }
            None => {}
        }
    }
    let mut p = combiner_en_serie(&profils)?;
    if let Headroom::Manual { db } = chaine.fixed.headroom
        && db.is_finite()
    {
        p.reserve_imposee_db = Some(db.min(0.0));
    }
    Some(p)
}

/// L'égaliseur que le MOTEUR applique à la zone, tiré de la chaîne TESS :
/// - `None` : la chaîne ne porte pas plus d'une façade ni de headroom manuel
///   (ou elle est absente, illisible, d'une version plus récente) ; le moteur
///   lit `zone_{id}_eq_profile` comme avant, au bit près ;
/// - `Some(None)` : aucun égaliseur coché ;
/// - `Some(Some(p))` : les égaliseurs cochés, en série ([`profil_de_la_chaine`]).
///
/// La chaîne est relue par [`lire_zone`] : un réglage écrit depuis par une
/// ancienne route y est déjà réappliqué. Le mode PURE et « Bypass all » ne
/// sont pas regardés ici (`traitements_contournes`).
pub fn egaliseurs_de_zone(settings: &SettingsRepo, zone_id: i64) -> Option<Option<EqProfile>> {
    let brute = chaine_brute(settings, zone_id)?;
    let facades = brute
        .plugins
        .iter()
        .filter(|m| FACADES_EQ.contains(&m.id.as_str()))
        .count();
    let manuel = matches!(brute.fixed.headroom, Headroom::Manual { .. });
    if brute.version > VERSION_CHAINE || (facades < 2 && !manuel) {
        return None;
    }
    let chaine = lire_zone(settings, zone_id, true).ok()?.chaine;
    let principal: Option<EqProfile> = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    Some(profil_de_la_chaine(&chaine, principal.as_ref()))
}

/// Ce qui, dans une chaîne, change l'égaliseur du moteur : les façades
/// cochées (ordre et réglages) et le headroom.
pub fn signature(chaine: &ChaineTess) -> (Vec<(String, Value)>, Option<f64>) {
    let facades = chaine
        .plugins
        .iter()
        .filter(|m| m.enabled && FACADES_EQ.contains(&m.id.as_str()))
        .map(|m| (m.id.clone(), m.settings.clone()))
        .collect();
    let manuel = match chaine.fixed.headroom {
        Headroom::Manual { db } => Some(db),
        _ => None,
    };
    (facades, manuel)
}

#[cfg(test)]
#[path = "egaliseurs_en_serie_tests.rs"]
mod tests;
