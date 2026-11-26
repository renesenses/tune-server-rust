//! rc14 — les profils d'égaliseur et les préréglages à l'import de
//! configuration et à la restauration d'une sauvegarde.
//!
//! Le profil d'égaliseur d'une zone (`zone_{id}_eq_profile`, et sa copie
//! d'avant TESS), les préréglages (`eq_presets`) et la chaîne TESS ne
//! s'écrivent en temps normal que par leurs routes dédiées, qui valident leur
//! contenu et le droit qu'il demande (`PUT /zones/{id}/tess`,
//! `PUT /zones/{id}/dsp`, `POST /zones/{id}/eq`, `/eq/presets`…). L'écriture
//! générique les refuse ([`crate::tess::migration::cle_tenue_par_le_serveur`]).
//!
//! Un fichier importé ou une sauvegarde restaurée les rapportent. Ils passent
//! ici, avec la même règle que ces routes :
//!
//! - avec le Premium, tout revient tel quel ;
//! - sans le Premium, un égaliseur graphique, casque (AutoEQ, OPRA) ou une
//!   courbe déjà en service sur la zone d'arrivée reviennent tels quels. Un
//!   égaliseur paramétrique ou une correction de pièce mesurée revient en
//!   égaliseur graphique au plus près (décision P2, comme à la lecture d'une
//!   zone), et le rapport le dit. Rien n'est refusé : le reste de la
//!   restauration continue.
//!
//! Un préréglage ne porte la marque d'un import AutoEQ ou OPRA (`source`)
//! que si ses bandes sont celles qu'un tel import produit
//! ([`bandes_d_un_import_casque`]) ; sinon la marque est retirée et le
//! préréglage suit la règle du paramétrique.

use serde_json::Value;

use crate::audio::eq::{EqBandSpec, EqProfile};
use crate::db::settings_repo::SettingsRepo;
use crate::tess::ChaineTess;
use crate::tess::migration::{
    CHAMP_ORIGINE, ID_CORRECTION_PIECE, ID_EQ_CASQUE, ID_EQ_GRAPHIQUE, ID_EQ_PARAMETRIQUE,
    est_courbe_graphique, grille_reglee, lire_zone, memes_bandes, profil_graphique_au_plus_pres,
};

/// La liste des préréglages d'égaliseur.
pub const CLE_PREREGLAGES_EQ: &str = "eq_presets";

/// Les sources d'un préréglage importé qui en font un égaliseur casque
/// (`eq_pro::enregistrer_profil_autoeq`).
const SOURCES_CASQUE: [&str; 2] = ["autoeq", "opra"];

/// Le nombre de bandes qu'accepte l'import d'un fichier d'égaliseur
/// (`eq_pro::MAX_BANDS`).
const BANDES_MAX_D_UN_IMPORT: usize = 31;

/// Les types de bande que l'analyse d'un fichier AutoEQ, Equalizer APO ou
/// OPRA produit ([`crate::audio::autoeq`]).
const TYPES_D_UN_IMPORT: [&str; 6] = [
    "peak",
    "low_shelf",
    "high_shelf",
    "low_pass",
    "high_pass",
    "notch",
];

/// Des bandes qu'un import de fichier d'égaliseur (AutoEQ, Equalizer APO,
/// OPRA) peut produire : 31 au plus, types connus, et les bornes que
/// [`crate::audio::autoeq`] impose (fréquence, gain, Q).
pub fn bandes_d_un_import_casque(bandes: &[EqBandSpec]) -> bool {
    use crate::audio::autoeq::{FREQ_MAX_HZ, GAIN_MAX_DB, Q_MAX, Q_MIN};
    !bandes.is_empty()
        && bandes.len() <= BANDES_MAX_D_UN_IMPORT
        && bandes.iter().all(|b| {
            TYPES_D_UN_IMPORT.contains(&b.band_type.as_str())
                && b.freq.is_finite()
                && (1.0..=FREQ_MAX_HZ).contains(&b.freq)
                && b.gain.is_finite()
                && (-GAIN_MAX_DB..=GAIN_MAX_DB).contains(&b.gain)
                && b.q.is_finite()
                && (Q_MIN..=Q_MAX).contains(&b.q)
                && b.channel.is_none_or(|c| c <= 1)
        })
}

fn bandes_du_prereglage(p: &Value) -> Option<Vec<EqBandSpec>> {
    p.get("bands")
        .and_then(Value::as_array)?
        .iter()
        .map(|b| serde_json::from_value(b.clone()).ok())
        .collect()
}

fn est_casque(p: &Value) -> bool {
    p.get("source")
        .and_then(Value::as_str)
        .is_some_and(|s| SOURCES_CASQUE.contains(&s))
}

/// La liste de préréglages `texte` telle qu'elle peut être écrite.
///
/// Avec le Premium, telle quelle. Sans, un préréglage marqué AutoEQ/OPRA
/// dont les bandes ne sont pas celles d'un import ([`bandes_d_un_import_casque`])
/// perd sa marque (avertissement). Une liste illisible est rendue telle
/// quelle : aucune route ne la lit alors comme une liste de préréglages.
pub fn prereglages_restaurables(texte: &str, premium: bool, avert: &mut Vec<String>) -> String {
    if premium {
        return texte.to_string();
    }
    let Ok(Value::Array(mut liste)) = serde_json::from_str::<Value>(texte) else {
        return texte.to_string();
    };
    let mut retires = Vec::new();
    for p in &mut liste {
        if !est_casque(p) || bandes_du_prereglage(p).is_some_and(|b| bandes_d_un_import_casque(&b))
        {
            continue;
        }
        if let Some(o) = p.as_object_mut() {
            o.remove("source");
            retires.push(
                o.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            );
        }
    }
    if retires.is_empty() {
        return texte.to_string();
    }
    avert.push(format!(
        "equalizer presets: {} restored as parametric presets (their bands are not those of a headphone profile import): {}",
        retires.len(),
        retires.join(", ")
    ));
    Value::Array(liste).to_string()
}

/// Ce que le compte couvre et les courbes casque connues, pour une
/// restauration.
pub struct Contexte {
    premium: bool,
    grille: u32,
    casques: Vec<Vec<EqBandSpec>>,
}

impl Contexte {
    /// `prereglages` : la liste qui existera après la restauration (celle du
    /// fichier, déjà passée par [`prereglages_restaurables`], sinon celle de
    /// la machine).
    pub fn nouveau(settings: &SettingsRepo, premium: bool, prereglages: Option<&str>) -> Self {
        let actuels = settings.get(CLE_PREREGLAGES_EQ).ok().flatten();
        let liste: Vec<Value> = prereglages
            .or(actuels.as_deref())
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();
        let casques = liste
            .iter()
            .filter(|p| est_casque(p))
            .filter_map(bandes_du_prereglage)
            .filter(|b| bandes_d_un_import_casque(b))
            .collect();
        Self {
            premium,
            grille: grille_reglee(settings),
            casques,
        }
    }

    /// Ces bandes demandent-elles le Premium ? Non pour une courbe vide ou
    /// graphique, ni pour celle d'un préréglage casque.
    fn demande_le_premium(&self, bandes: &[EqBandSpec]) -> bool {
        !(bandes.is_empty()
            || est_courbe_graphique(bandes)
            || self.casques.iter().any(|c| memes_bandes(c, bandes)))
    }
}

fn profil_enregistre(settings: &SettingsRepo, cle: &str) -> Option<EqProfile> {
    settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Le profil d'égaliseur `texte` tel qu'il peut être écrit sous `cle_arrivee`
/// (`zone_{id}_eq_profile` ou sa copie d'avant TESS, numéro d'arrivée ;
/// `None` pour une zone créée par la restauration).
///
/// Sans le Premium, un paramétrique ou une correction de pièce revient en
/// égaliseur graphique au plus près, sauf si la zone d'arrivée porte déjà les
/// mêmes bandes (rien de nouveau). Un profil illisible est rendu tel quel :
/// le moteur ne le lit pas non plus.
pub fn profil_restaurable(
    ctx: &Contexte,
    settings: &SettingsRepo,
    cle_arrivee: Option<&str>,
    texte: &str,
    zone: &str,
    avert: &mut Vec<String>,
) -> String {
    if ctx.premium {
        return texte.to_string();
    }
    let Ok(profil) = serde_json::from_str::<EqProfile>(texte) else {
        return texte.to_string();
    };
    if !ctx.demande_le_premium(&profil.bands) {
        return texte.to_string();
    }
    if cle_arrivee
        .and_then(|c| profil_enregistre(settings, c))
        .is_some_and(|e| memes_bandes(&e.bands, &profil.bands) && (e.enabled || !profil.enabled))
    {
        return texte.to_string();
    }
    avert.push(format!(
        "zone '{zone}': the parametric equalizer requires Premium; restored as a graphic equalizer"
    ));
    let converti = profil_graphique_au_plus_pres(&profil, ctx.grille);
    serde_json::to_string(&converti).unwrap_or_else(|_| texte.to_string())
}

fn bandes_du_maillon(settings: &Value) -> Vec<EqBandSpec> {
    settings
        .get("bands")
        .and_then(|b| serde_json::from_value(b.clone()).ok())
        .unwrap_or_default()
}

/// La chaîne TESS `chaine` d'un fichier telle qu'elle peut être écrite pour
/// la zone `arrivee` (`None` : zone créée par la restauration).
///
/// Sans le Premium, chaque égaliseur coché qui demande le droit — le
/// paramétrique, la correction de pièce (restaurée MESURÉE, D1), un casque
/// dont les bandes ne sont pas celles d'un import — devient l'égaliseur
/// graphique au plus près (P2), ou est décoché si la chaîne en porte déjà un.
/// Un maillon identique à celui de la zone d'arrivée est gardé.
pub fn chaine_restaurable(
    ctx: &Contexte,
    settings: &SettingsRepo,
    arrivee: Option<i64>,
    chaine: &mut ChaineTess,
    zone: &str,
    avert: &mut Vec<String>,
) {
    if ctx.premium {
        return;
    }
    let actuelle = arrivee
        .and_then(|id| lire_zone(settings, id, true).ok())
        .map(|l| l.chaine);
    let mut graphique = chaine.plugins.iter().any(|m| m.id == ID_EQ_GRAPHIQUE);
    let mut retenus = 0usize;
    for m in &mut chaine.plugins {
        if !m.enabled {
            continue;
        }
        let bandes = bandes_du_maillon(&m.settings);
        let premium_requis = match m.id.as_str() {
            // Le greffon paramétrique et la correction de pièce demandent le
            // droit quelles que soient leurs bandes (`PUT /zones/{id}/tess`).
            ID_EQ_PARAMETRIQUE | ID_CORRECTION_PIECE => true,
            ID_EQ_CASQUE => !bandes.is_empty() && !bandes_d_un_import_casque(&bandes),
            _ => false,
        };
        if !premium_requis {
            continue;
        }
        let inchange = actuelle.as_ref().is_some_and(|c| {
            c.plugins.iter().any(|a| {
                a.id == m.id && a.enabled && memes_bandes(&bandes_du_maillon(&a.settings), &bandes)
            })
        });
        if inchange {
            continue;
        }
        retenus += 1;
        if graphique {
            m.enabled = false;
            continue;
        }
        let mut reglages = m.settings.clone();
        if let Value::Object(o) = &mut reglages {
            o.remove(CHAMP_ORIGINE);
            o.insert("enabled".into(), Value::Bool(true));
        }
        m.id = ID_EQ_GRAPHIQUE.to_owned();
        graphique = true;
        if let Ok(p) = serde_json::from_value::<EqProfile>(reglages) {
            let converti = profil_graphique_au_plus_pres(&p, ctx.grille);
            if let Ok(Value::Object(mut o)) = serde_json::to_value(&converti) {
                o.remove("enabled");
                m.settings = Value::Object(o);
            }
        } else if let Value::Object(o) = &mut m.settings {
            o.remove(CHAMP_ORIGINE);
        }
    }
    if retenus > 0 {
        avert.push(format!(
            "zone '{zone}': the parametric equalizer and the measured room correction require Premium; restored as a graphic equalizer"
        ));
    }
}

/// Applique la règle de ce module à un plan d'import ([`super::planifier`]) :
/// la liste des préréglages, le profil d'égaliseur de chaque zone (et sa
/// copie d'avant TESS) et la chaîne TESS de chaque zone. Les avertissements
/// rejoignent ceux du plan.
pub(super) fn retenir(settings: &SettingsRepo, plan: &mut super::Plan, premium: bool) {
    if premium {
        return;
    }
    let mut avert = Vec::new();
    let mut prereglages = None;
    for r in plan
        .reglages
        .iter_mut()
        .filter(|r| r.cle == CLE_PREREGLAGES_EQ)
    {
        let texte = prereglages_restaurables(&super::texte_json(&r.valeur), false, &mut avert);
        r.valeur = Value::String(texte.clone());
        prereglages = Some(texte);
    }
    let ctx = Contexte::nouveau(settings, false, prereglages.as_deref());
    // L'ancien format à plat : les réglages de zone gardent leur numéro.
    for r in plan.reglages.iter_mut().filter(|r| {
        r.cle != CLE_PREREGLAGES_EQ && crate::tess::migration::cle_de_profil_d_egaliseur(&r.cle)
    }) {
        let texte = profil_restaurable(
            &ctx,
            settings,
            Some(&r.cle),
            &super::texte_json(&r.valeur),
            &r.cle,
            &mut avert,
        );
        r.valeur = Value::String(texte);
    }
    for z in &mut plan.zones {
        let arrivee = match z.cible {
            super::Cible::Existante(id) => Some(id),
            super::Cible::Nouvelle(_) => None,
        };
        if let Some(r) = z.donnees.get_mut("settings").and_then(Value::as_object_mut) {
            for (gabarit, v) in r.iter_mut() {
                if !crate::tess::migration::cle_de_profil_d_egaliseur(&gabarit.replace("{id}", "0"))
                {
                    continue;
                }
                let cle = arrivee.map(|id| gabarit.replace("{id}", &id.to_string()));
                let texte = profil_restaurable(
                    &ctx,
                    settings,
                    cle.as_deref(),
                    &super::texte_json(v),
                    &z.nom,
                    &mut avert,
                );
                *v = Value::String(texte);
            }
        }
        if let Some(c) = &mut z.chaine {
            chaine_restaurable(&ctx, settings, arrivee, c, &z.nom, &mut avert);
        }
    }
    for a in avert {
        if !plan.avertissements.contains(&a) {
            plan.avertissements.push(a);
        }
    }
}
