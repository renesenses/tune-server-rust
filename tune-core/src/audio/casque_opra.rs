//! rc14 — les réglages du greffon TESS « Casque » (`equalizer-headphone`)
//! quand ils viennent d'une correction OPRA ([`super::opra_base`]).
//!
//! Le maillon garde les bandes que le moteur joue (`bands`, même moteur que
//! les autres égaliseurs) ET la référence de la correction choisie, sous
//! [`CHAMP_OPRA`] :
//!
//! ```json
//! { "listening": "headphones", "bands": [ … ], "headroom_mode": "realistic",
//!   "opra": { "id": "sennheiser:hd650::oratory1990_harman_target",
//!             "product_id": "sennheiser::hd650", "vendor": "Sennheiser",
//!             "product": "HD650", "author": "oratory1990",
//!             "details": "Harman Target", "license": "CC BY-SA 4.0",
//!             "level": 100, "bands_digest": "…" } }
//! ```
//!
//! - **Profil reconnu** ([`profil_reconnu`]) : la référence ne vaut que si
//!   les bandes sont encore celles que le serveur a calculées pour elle
//!   (`bands_digest`). Des bandes retouchées ailleurs (ancien écran, `PUT`
//!   à la main) gardent leur son, mais ne se disent plus « HD650 d'après
//!   oratory1990 ». C'est l'appui de la règle rc15 « un casque = un profil
//!   reconnu » ; les bandes libres restent acceptées en rc14.
//! - **Préampli automatique** : `headroom_mode: realistic` — le moteur
//!   réserve le maximum réel de la courbe (plus sa marge) et ne peut pas
//!   écrêter ; [`preampli_auto_db`] dit combien.
//! - **Niveau d'effet** (`level`, 0 à 100 %) : les gains des cloches et des
//!   plateaux sont multipliés par `level / 100` ; fréquences, Q, passe-haut
//!   et passe-bas ne bougent pas. 100 % = la correction publiée.
//! - Le gain global d'OPRA (`gain_db`) n'est pas appliqué, comme le
//!   `Preamp` d'AutoEq : c'est la réserve du moteur qui le remplace.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::eq::{EqBandSpec, EqProfile, HeadroomMode, ListeningMode};
use super::opra_base::{CorrectionOpra, LICENCE_OPRA};

/// La clé des réglages qui porte la référence OPRA.
pub const CHAMP_OPRA: &str = "opra";
/// Le niveau d'effet par défaut : la correction telle que publiée.
pub const NIVEAU_PLEIN: u8 = 100;
/// Le débit auquel la réserve et la courbe sont calculées pour l'affichage.
const DEBIT_D_AFFICHAGE: f64 = 48_000.0;

/// La référence d'une correction OPRA, telle que le maillon la garde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceOpra {
    /// L'identifiant OPRA de la correction.
    pub id: String,
    /// L'identifiant OPRA du produit.
    pub product_id: String,
    pub vendor: String,
    pub product: String,
    pub author: String,
    pub details: String,
    /// La licence du jeu de données (attribution).
    pub license: String,
    /// Le niveau d'effet, en % (0 à 100).
    pub level: u8,
    /// L'empreinte des bandes calculées pour cette référence.
    pub bands_digest: String,
}

/// Les bandes au niveau d'effet `niveau` (0 à 100, borné).
pub fn bandes_au_niveau(bandes: &[EqBandSpec], niveau: u8) -> Vec<EqBandSpec> {
    let facteur = f64::from(niveau.min(NIVEAU_PLEIN)) / f64::from(NIVEAU_PLEIN);
    bandes
        .iter()
        .map(|b| {
            let mut b = b.clone();
            if matches!(b.band_type.as_str(), "peak" | "low_shelf" | "high_shelf") {
                // Au centième de dB : ce que l'écran montre est ce qui joue.
                b.gain = (b.gain * facteur * 100.0).round() / 100.0;
            }
            b
        })
        .collect()
}

/// L'empreinte d'une liste de bandes (SHA-256 tronqué). Les nombres y sont
/// écrits à 10⁻⁴ près : un aller-retour par le JSON enregistré (dont la
/// lecture des flottants n'est pas exacte au dernier bit) ne la change pas,
/// une retouche d'un centième de dB si.
pub fn empreinte(bandes: &[EqBandSpec]) -> String {
    let mut texte = String::new();
    for b in bandes {
        texte.push_str(&format!(
            "{}|{:.4}|{:.4}|{:.4}|{:?};",
            b.band_type, b.freq, b.gain, b.q, b.channel
        ));
    }
    Sha256::digest(texte.as_bytes())
        .iter()
        .take(16)
        .map(|o| format!("{o:02x}"))
        .collect()
}

/// Les réglages du maillon `equalizer-headphone` pour la correction `c` au
/// niveau `niveau`. Les champs inconnus de `precedents` (un client plus
/// récent) sont gardés ; ceux que la correction décide sont écrasés.
pub fn reglages_du_casque(c: &CorrectionOpra, niveau: u8, precedents: Option<&Value>) -> Value {
    let niveau = niveau.min(NIVEAU_PLEIN);
    let bandes = bandes_au_niveau(&c.profil.bandes, niveau);
    let mut base: Map<String, Value> = match serde_json::to_value(EqProfile::default()) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if let Some(Value::Object(p)) = precedents {
        for (k, v) in p {
            base.insert(k.clone(), v.clone());
        }
    }
    // Dans la chaîne, c'est le maillon qui porte `enabled`.
    base.remove("enabled");
    base.insert(
        "listening".into(),
        serde_json::to_value(ListeningMode::Headphones).unwrap_or(Value::Null),
    );
    for cle in ["bass_gain_db", "mid_gain_db", "treble_gain_db"] {
        base.insert(cle.into(), Value::from(0.0));
    }
    base.insert(
        "headroom_mode".into(),
        Value::String(HeadroomMode::Realistic.code().into()),
    );
    base.insert(
        "bands".into(),
        serde_json::to_value(&bandes).unwrap_or(Value::Array(Vec::new())),
    );
    let reference = ReferenceOpra {
        id: c.variante.id.clone(),
        product_id: c.produit.id.clone(),
        vendor: c.produit.vendor.clone(),
        product: c.produit.name.clone(),
        author: c.variante.author.clone(),
        details: c.variante.details.clone(),
        license: LICENCE_OPRA.into(),
        level: niveau,
        bands_digest: empreinte(&bandes),
    };
    base.insert(
        CHAMP_OPRA.into(),
        serde_json::to_value(reference).unwrap_or(Value::Null),
    );
    Value::Object(base)
}

/// La référence OPRA des réglages d'un maillon, SI leurs bandes sont encore
/// celles calculées pour elle. `None` : bandes libres (ou retouchées).
pub fn profil_reconnu(reglages: &Value) -> Option<ReferenceOpra> {
    let reference: ReferenceOpra =
        serde_json::from_value(reglages.get(CHAMP_OPRA)?.clone()).ok()?;
    let bandes: Vec<EqBandSpec> = serde_json::from_value(reglages.get("bands")?.clone()).ok()?;
    (!bandes.is_empty() && empreinte(&bandes) == reference.bands_digest).then_some(reference)
}

/// Le profil du moteur que ces réglages décrivent, allumé.
fn profil_moteur(reglages: &Value) -> Option<EqProfile> {
    let mut v = reglages.clone();
    v.as_object_mut()?
        .insert("enabled".into(), Value::Bool(true));
    serde_json::from_value(v).ok()
}

/// Le préampli automatique (dB, ≤ 0) que le moteur posera : la réserve du
/// mode des réglages, au pire des deux canaux.
pub fn preampli_auto_db(reglages: &Value) -> f64 {
    profil_moteur(reglages).map_or(0.0, |p| {
        p.reserve_db_at(0, DEBIT_D_AFFICHAGE)
            .min(p.reserve_db_at(1, DEBIT_D_AFFICHAGE))
    })
}

/// Les fréquences de la courbe affichée : `n` points en échelle log de 20 Hz
/// à 20 kHz.
pub fn frequences_d_affichage(n: usize) -> Vec<f64> {
    let n = n.max(2);
    (0..n)
        .map(|i| 20.0 * 1000f64.powf(i as f64 / (n - 1) as f64))
        .collect()
}

/// La courbe de correction (dB, sans le préampli) aux fréquences données.
pub fn courbe_db(reglages: &Value, frequences: &[f64]) -> Vec<f64> {
    match profil_moteur(reglages) {
        Some(p) => {
            let [gauche, _] = crate::tess::migration::courbe_db(&p, frequences);
            gauche
        }
        None => vec![0.0; frequences.len()],
    }
}

#[cfg(test)]
#[path = "casque_opra_tests.rs"]
mod tests;
