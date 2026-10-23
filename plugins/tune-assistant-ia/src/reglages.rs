//! Les réglages de l'assistant, dans la table `settings`.
//!
//! Chaque nom est préfixé `assistant_ia_`. La clé d'API, SCELLÉE par le
//! [`crate::coffre::Coffre`], vit sous [`CLE_API_CHIFFREE`] : son nom porte
//! `api_key`, et [`tune_core::secrets::est_secret`] la retire donc de
//! `/system/config`, des exports et de la sauvegarde dans le nuage.

use std::sync::Arc;

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::coffre::{Coffre, fin_de_cle};
use crate::couts::{self, Consommation, Tarif};
use crate::fournisseur::{Acces, Famille};

pub const CLE_FOURNISSEUR: &str = "assistant_ia_fournisseur";
pub const CLE_MODELE: &str = "assistant_ia_modele";
pub const CLE_MODELE_CLASSIQUE: &str = "assistant_ia_modele_classique";
pub const CLE_URL: &str = "assistant_ia_url_base";
pub const CLE_PLAFOND: &str = "assistant_ia_plafond_eur";
pub const CLE_PRIX_ENTREE: &str = "assistant_ia_prix_entree_usd_mtok";
pub const CLE_PRIX_SORTIE: &str = "assistant_ia_prix_sortie_usd_mtok";
pub const CLE_TAUX: &str = "assistant_ia_taux_usd_eur";
/// La clé d'API scellée. Le nom DOIT porter un fragment que
/// [`tune_core::secrets::est_secret`] reconnaît — gardé par un essai.
pub const CLE_API_CHIFFREE: &str = "assistant_ia_api_key_chiffree";
pub const CLE_CONSOMMATION: &str = "assistant_ia_consommation";

/// Le plafond mensuel par défaut, en euros.
pub const PLAFOND_PAR_DEFAUT_EUR: &str = "2";

/// Les réglages lus.
#[derive(Clone, Debug)]
pub struct Reglages {
    pub famille: Option<Famille>,
    pub modele: String,
    pub modele_classique: String,
    pub url_base: String,
    pub plafond_micro_eur: u64,
    pub prix_perso: Option<Tarif>,
    pub taux_usd_eur: f64,
    /// La clé est-elle posée (scellée) ?
    pub cle_posee: bool,
}

fn lire(s: &SettingsRepo, cle: &str) -> String {
    s.get(cle).ok().flatten().unwrap_or_default()
}

fn nombre(texte: &str) -> Option<f64> {
    let v: f64 = texte.trim().replace(',', ".").parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

impl Reglages {
    pub fn charger(db: &Arc<dyn DbBackend>) -> Self {
        let s = SettingsRepo::with_backend(db.clone());
        let famille = Famille::depuis(&lire(&s, CLE_FOURNISSEUR));
        let plafond = lire(&s, CLE_PLAFOND);
        let plafond = if plafond.trim().is_empty() {
            PLAFOND_PAR_DEFAUT_EUR.to_string()
        } else {
            plafond
        };
        let prix_perso = match (
            nombre(&lire(&s, CLE_PRIX_ENTREE)),
            nombre(&lire(&s, CLE_PRIX_SORTIE)),
        ) {
            (Some(e), Some(so)) => Some(Tarif {
                entree_usd_mtok: e,
                sortie_usd_mtok: so,
            }),
            _ => None,
        };
        Self {
            famille,
            modele: lire(&s, CLE_MODELE).trim().to_string(),
            modele_classique: lire(&s, CLE_MODELE_CLASSIQUE).trim().to_string(),
            url_base: lire(&s, CLE_URL).trim().to_string(),
            // Un plafond illisible vaut ZÉRO, jamais « illimité ».
            plafond_micro_eur: couts::euros_en_micro(&plafond).unwrap_or(0),
            prix_perso,
            taux_usd_eur: nombre(&lire(&s, CLE_TAUX))
                .filter(|t| *t > 0.0)
                .unwrap_or(1.0),
            cle_posee: !lire(&s, CLE_API_CHIFFREE).trim().is_empty(),
        }
    }

    /// Le modèle effectif : celui choisi, ou celui de la famille par défaut.
    pub fn modele_effectif(&self, classique: bool) -> String {
        if classique && !self.modele_classique.is_empty() {
            return self.modele_classique.clone();
        }
        if !self.modele.is_empty() {
            return self.modele.clone();
        }
        self.famille
            .map(|f| f.modele_par_defaut().to_string())
            .unwrap_or_default()
    }

    /// Le tarif du modèle. `Some(0)` pour Ollama ; `None` si inconnu.
    pub fn tarif(&self, modele: &str) -> Option<Tarif> {
        match self.famille? {
            Famille::Ollama => Some(Tarif {
                entree_usd_mtok: 0.0,
                sortie_usd_mtok: 0.0,
            }),
            // Un prix saisi par l'utilisateur prime : il connaît son contrat.
            Famille::Anthropic => self.prix_perso.or_else(|| couts::tarif_anthropic(modele)),
            Famille::Gemini => self.prix_perso.or_else(|| couts::tarif_gemini(modele)),
            Famille::CompatibleOpenai => self.prix_perso,
        }
    }
}

/// La clé en clair, si elle est posée et s'ouvre.
pub enum EtatCle {
    Absente,
    Illisible,
    Ouverte(String),
}

pub fn cle_en_clair(db: &Arc<dyn DbBackend>, coffre: &Coffre) -> EtatCle {
    let s = SettingsRepo::with_backend(db.clone());
    let scelle = lire(&s, CLE_API_CHIFFREE);
    if scelle.trim().is_empty() {
        return EtatCle::Absente;
    }
    match coffre.ouvrir(&scelle) {
        Ok(c) => EtatCle::Ouverte(c),
        Err(_) => EtatCle::Illisible,
    }
}

/// L'accès au fournisseur configuré, avec sa clé.
pub fn acces(r: &Reglages, cle: Option<String>) -> Option<Acces> {
    Some(Acces {
        famille: r.famille?,
        url_base: r.url_base.clone(),
        cle,
    })
}

pub fn charger_consommation(db: &Arc<dyn DbBackend>) -> Consommation {
    let s = SettingsRepo::with_backend(db.clone());
    let brut = lire(&s, CLE_CONSOMMATION);
    serde_json::from_str::<Consommation>(&brut)
        .unwrap_or_default()
        .pour_le_mois(&couts::mois_courant())
}

pub fn ranger_consommation(db: &Arc<dyn DbBackend>, c: &Consommation) -> Result<(), String> {
    let s = SettingsRepo::with_backend(db.clone());
    s.set(
        CLE_CONSOMMATION,
        &serde_json::to_string(c).map_err(|e| e.to_string())?,
    )
}

/// Les réglages tels que l'écran les voit. JAMAIS la clé : ses quatre
/// derniers caractères seulement.
pub fn vue(db: &Arc<dyn DbBackend>, coffre: &Coffre) -> Value {
    let r = Reglages::charger(db);
    let c = charger_consommation(db);
    let (cle_definie, cle_illisible, fin) = match cle_en_clair(db, coffre) {
        EtatCle::Absente => (false, false, None),
        EtatCle::Illisible => (true, true, None),
        EtatCle::Ouverte(k) => (true, false, Some(fin_de_cle(&k))),
    };
    let modele = r.modele_effectif(false);
    let tarif = r.tarif(&modele);
    let par_usage: serde_json::Map<String, Value> = c
        .par_usage
        .iter()
        .map(|(usage, u)| {
            (
                usage.clone(),
                json!({
                    "cout_eur": couts::micro_en_euros(u.micro_eur),
                    "appels": u.appels,
                    "jetons_entree": u.jetons_entree,
                    "jetons_sortie": u.jetons_sortie,
                    "depuis_cache": u.depuis_cache,
                }),
            )
        })
        .collect();
    let plafond = crate::appel::etat_plafond_de(&c, r.plafond_micro_eur);
    json!({
        "fournisseur": r.famille.map(Famille::nom),
        "modele": r.modele,
        "modele_effectif": modele,
        "modele_classique": r.modele_classique,
        "url_base": r.url_base,
        "url_par_defaut": r.famille.map(Famille::url_par_defaut),
        "plafond_eur": couts::micro_en_euros(r.plafond_micro_eur),
        "prix_entree_usd_mtok": r.prix_perso.map(|t| t.entree_usd_mtok),
        "prix_sortie_usd_mtok": r.prix_perso.map(|t| t.sortie_usd_mtok),
        "tarif_connu": tarif.is_some(),
        "tarif": vue_tarif(&r, &modele, &couts::jour_courant()),
        "taux_usd_eur": r.taux_usd_eur,
        "cle_definie": cle_definie,
        "cle_illisible": cle_illisible,
        "cle_fin": fin,
        "modeles_anthropic": couts::TARIFS_ANTHROPIC.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
        "modeles_gemini": couts::TARIFS_GEMINI.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
        "modele_gemini_par_defaut": couts::MODELE_GEMINI_PAR_DEFAUT,
        "modele_anthropic_par_defaut": couts::MODELE_ANTHROPIC_PAR_DEFAUT,
        "modele_anthropic_classique": couts::MODELE_ANTHROPIC_CLASSIQUE,
        "transcription": crate::transcription::vue(
            db,
            coffre,
            std::env::var(crate::transcription::VARIABLE_HERITEE).ok().as_deref(),
        ),
        "consommation": {
            "mois": c.mois,
            "cout_eur": couts::micro_en_euros(c.micro_eur),
            "appels": c.appels,
            "jetons_entree": c.jetons_entree,
            "jetons_sortie": c.jetons_sortie,
            "depuis_cache": c.depuis_cache,
            "plafond_atteint": c.micro_eur >= r.plafond_micro_eur,
            "alerte": plafond["alerte"],
            "part_du_plafond": plafond["part"],
            "seuil_alerte": couts::SEUIL_ALERTE_POURCENT,
            "par_usage": par_usage,
        },
    })
}

/// Étape 3 : le tarif appliqué au modèle effectif, et d'où il vient, daté.
///
/// `origine` : `table` (relevé daté, avec sa source), `saisi` (les prix de
/// l'utilisateur, qui priment), `local` (Ollama, gratuit), ou `null` (aucun
/// tarif : l'appel sera refusé, `tarif_inconnu`). `a_relever` : le
/// fournisseur a annoncé un changement de prix et la date est passée — la
/// table de ce binaire ne le suit plus.
pub fn vue_tarif(r: &Reglages, modele: &str, aujourd_hui: &str) -> Value {
    let Some(famille) = r.famille else {
        return Value::Null;
    };
    let (origine, releve) = match famille {
        Famille::Ollama => ("local", None),
        _ if r.prix_perso.is_some() => ("saisi", None),
        Famille::Anthropic if couts::tarif_anthropic(modele).is_some() => {
            ("table", Some(couts::RELEVE_ANTHROPIC))
        }
        Famille::Gemini if couts::tarif_gemini(modele).is_some() => {
            ("table", Some(couts::RELEVE_GEMINI))
        }
        _ => return json!({ "origine": null }),
    };
    let t = r.tarif(modele);
    json!({
        "origine": origine,
        "entree_usd_mtok": t.map(|t| t.entree_usd_mtok),
        "sortie_usd_mtok": t.map(|t| t.sortie_usd_mtok),
        "releve_le": releve.map(|x| x.releve_le),
        "source_url": releve.and_then(|x| x.source_url),
        "a_relever_avant": releve.and_then(|x| x.a_relever_avant),
        "a_relever": releve.is_some_and(|x| x.a_relever(aujourd_hui)),
    })
}

/// Ce que l'écran peut changer. Un champ absent n'est pas touché.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Modification {
    pub fournisseur: Option<String>,
    pub modele: Option<String>,
    pub modele_classique: Option<String>,
    pub url_base: Option<String>,
    pub plafond_eur: Option<Value>,
    pub prix_entree_usd_mtok: Option<Value>,
    pub prix_sortie_usd_mtok: Option<Value>,
    pub taux_usd_eur: Option<Value>,
    /// La clé en clair. `""` l'efface.
    pub cle: Option<String>,
    /// La clé de TRANSCRIPTION (Whisper, OpenAI) en clair, scellée au coffre.
    /// `""` l'efface. Voir [`crate::transcription`].
    #[serde(default)]
    pub cle_transcription: Option<String>,
}

fn texte_de(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        autre => autre.to_string(),
    }
}

/// Applique une modification. Rend un message d'erreur lisible si une valeur
/// est refusée ; rien n'est alors écrit.
pub fn appliquer(db: &Arc<dyn DbBackend>, coffre: &Coffre, m: &Modification) -> Result<(), String> {
    // 1. Tout valider avant d'écrire quoi que ce soit.
    if let Some(f) = &m.fournisseur
        && !f.trim().is_empty()
        && Famille::depuis(f).is_none()
    {
        return Err(format!("fournisseur inconnu : {f}"));
    }
    if let Some(v) = &m.plafond_eur
        && couts::euros_en_micro(&texte_de(v)).is_none()
    {
        return Err("plafond illisible".into());
    }
    for (nom, v) in [
        ("prix d'entrée", &m.prix_entree_usd_mtok),
        ("prix de sortie", &m.prix_sortie_usd_mtok),
        ("taux", &m.taux_usd_eur),
    ] {
        if let Some(v) = v {
            let t = texte_de(v);
            if !t.trim().is_empty() && nombre(&t).is_none() {
                return Err(format!("{nom} illisible"));
            }
        }
    }
    if let Some(u) = &m.url_base {
        let u = u.trim();
        if !u.is_empty() && !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err("l'adresse doit commencer par http:// ou https://".into());
        }
    }
    // 2. Écrire.
    let s = SettingsRepo::with_backend(db.clone());
    let poser = |cle: &str, v: &str| s.set(cle, v.trim());
    if let Some(f) = &m.fournisseur {
        poser(
            CLE_FOURNISSEUR,
            Famille::depuis(f).map(Famille::nom).unwrap_or(""),
        )?;
    }
    if let Some(v) = &m.modele {
        poser(CLE_MODELE, v)?;
    }
    if let Some(v) = &m.modele_classique {
        poser(CLE_MODELE_CLASSIQUE, v)?;
    }
    if let Some(v) = &m.url_base {
        poser(CLE_URL, v)?;
    }
    if let Some(v) = &m.plafond_eur {
        poser(CLE_PLAFOND, &texte_de(v))?;
    }
    if let Some(v) = &m.prix_entree_usd_mtok {
        poser(CLE_PRIX_ENTREE, &texte_de(v))?;
    }
    if let Some(v) = &m.prix_sortie_usd_mtok {
        poser(CLE_PRIX_SORTIE, &texte_de(v))?;
    }
    if let Some(v) = &m.taux_usd_eur {
        poser(CLE_TAUX, &texte_de(v))?;
    }
    if let Some(cle) = &m.cle {
        let cle = cle.trim();
        if cle.is_empty() {
            s.delete(CLE_API_CHIFFREE)?;
        } else {
            s.set(CLE_API_CHIFFREE, &coffre.sceller(cle)?)?;
        }
    }
    if let Some(cle) = &m.cle_transcription {
        crate::transcription::poser_cle(db, coffre, cle)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reglages(famille: Famille, modele: &str, prix: Option<Tarif>) -> Reglages {
        Reglages {
            famille: Some(famille),
            modele: modele.into(),
            modele_classique: String::new(),
            url_base: String::new(),
            plafond_micro_eur: 2_000_000,
            prix_perso: prix,
            taux_usd_eur: 1.0,
            cle_posee: true,
        }
    }

    #[test]
    fn le_tarif_de_la_table_est_date_et_dit_quand_le_relever() {
        let r = reglages(Famille::Gemini, "", None);
        let m = r.modele_effectif(false);
        let v = vue_tarif(&r, &m, "2026-10-13");
        assert_eq!(v["origine"], "table");
        assert_eq!(v["releve_le"], "2026-10-12");
        assert_eq!(v["a_relever_avant"], "2027-01-01");
        assert_eq!(
            v["source_url"],
            "https://ai.google.dev/gemini-api/docs/pricing"
        );
        assert_eq!(v["entree_usd_mtok"], 0.30);
        assert_eq!(v["a_relever"], false);
        assert_eq!(vue_tarif(&r, &m, "2027-01-01")["a_relever"], true);

        let a = reglages(Famille::Anthropic, "", None);
        let v = vue_tarif(&a, &a.modele_effectif(false), "2027-06-01");
        assert_eq!(v["origine"], "table");
        assert_eq!(v["a_relever"], false, "aucun changement annoncé");
    }

    #[test]
    fn un_prix_saisi_ollama_et_un_modele_inconnu_ne_sont_pas_dates() {
        let saisi = Some(Tarif {
            entree_usd_mtok: 1.0,
            sortie_usd_mtok: 2.0,
        });
        let r = reglages(Famille::CompatibleOpenai, "mon-modele", saisi);
        let v = vue_tarif(&r, "mon-modele", "2026-10-13");
        assert_eq!(v["origine"], "saisi");
        assert_eq!(v["releve_le"], Value::Null);
        assert_eq!(v["sortie_usd_mtok"], 2.0);

        let o = reglages(Famille::Ollama, "llama", None);
        assert_eq!(vue_tarif(&o, "llama", "2026-10-13")["origine"], "local");

        let inconnu = reglages(Famille::Anthropic, "claude-inconnu", None);
        assert_eq!(
            vue_tarif(&inconnu, "claude-inconnu", "2026-10-13")["origine"],
            Value::Null
        );
    }
}
