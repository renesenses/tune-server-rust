//! TESS T17 — préréglages de chaîne : enregistrement, chargement par zone,
//! préréglage actif, mode d'application (maquette A2).
//!
//! - **Liste globale** sous [`CLE_PREREGLAGES`] (`tess_presets`) : une
//!   [`PreReglages`] JSON. Clé absente = la liste par défaut (« Pure — no
//!   processing », mode `immediately`). Clé illisible = refus explicite
//!   ([`ErreurStockage::Illisible`]), jamais une liste vide inventée.
//! - **Préréglage actif par zone** sous [`cle_prereglage_actif`]
//!   (`zone_{id}_tess_active_preset`). Il n'est PUBLIÉ
//!   ([`prereglage_actif`]) que si la chaîne de la zone est encore
//!   exactement celle du préréglage : dès qu'un réglage bouge (écran TESS,
//!   ancienne route, client mobile), « ACTIVE » s'éteint. Le chemin ne ment
//!   jamais, la pastille non plus.
//! - **Charger** ([`charger`]) valide la chaîne du préréglage contre le
//!   catalogue et les droits AU CHARGEMENT (402/409, contrat § 5), puis
//!   l'écrit par T02 ([`super::migration::ecrire_chaine`] : chaîne, anciennes
//!   clés et ombre, sous le verrou de la zone). Le geste sur le son (pause
//!   puis reprise, ou au prochain démarrage) est celui de l'orchestrateur
//!   (`orchestrator::relance_tess_t17`).
//! - **Préréglages par greffon** (`eq_presets`, `crossfeed_presets`…) : pas
//!   touchés. Ils vivent dans les réglages de chaque greffon.

use super::chaine::{ChaineTess, ContexteValidation, ErreurChaine};
use super::greffon::Catalogue;
use super::migration::{ErreurMigration, ecrire_chaine};
use super::prereglages::{
    ErreurPreReglage, ID_PREREGLAGE_PURE, ModeApplication, PreReglage, PreReglages,
};
use crate::db::settings_repo::SettingsRepo;

/// La liste globale des préréglages.
pub const CLE_PREREGLAGES: &str = "tess_presets";

/// Le préréglage chargé sur une zone.
pub fn cle_prereglage_actif(zone_id: i64) -> String {
    format!("zone_{zone_id}_tess_active_preset")
}

/// Pourquoi une opération sur les préréglages échoue.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ErreurStockage {
    #[error("unreadable presets: {0}")]
    Illisible(String),
    #[error("storage error: {0}")]
    Stockage(String),
    #[error("unknown preset {0}")]
    Inconnu(String),
    #[error("the preset name is empty")]
    NomVide,
    #[error("{0}")]
    Liste(#[from] ErreurPreReglage),
    #[error("{0}")]
    Chaine(ErreurChaine),
    #[error("{0}")]
    Migration(String),
}

impl ErreurStockage {
    /// Le statut HTTP du refus (contrat § 7 : 400, 402, 409 ; 404 inconnu).
    pub fn statut_http(&self) -> u16 {
        match self {
            Self::Inconnu(_) => 404,
            Self::NomVide => 400,
            Self::Liste(_) => 409,
            Self::Chaine(e) => e.statut_http(),
            Self::Illisible(_) | Self::Stockage(_) | Self::Migration(_) => 500,
        }
    }

    /// Code stable du refus.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Inconnu(_) => "unknown_preset",
            Self::NomVide => "empty_name",
            Self::Liste(ErreurPreReglage::PureModifie) => "pure_preset_read_only",
            Self::Liste(_) => "invalid_presets",
            Self::Chaine(e) => e.code(),
            Self::Illisible(_) => "presets_unreadable",
            Self::Stockage(_) => "storage_error",
            Self::Migration(_) => "chain_write_failed",
        }
    }

    pub fn plugin_id(&self) -> Option<&str> {
        match self {
            Self::Chaine(e) => e.greffon(),
            _ => None,
        }
    }
}

fn stockage<T>(r: Result<T, String>) -> Result<T, ErreurStockage> {
    r.map_err(ErreurStockage::Stockage)
}

/// La liste des préréglages (défaut si la clé est absente).
pub fn lire_prereglages(settings: &SettingsRepo) -> Result<PreReglages, ErreurStockage> {
    match stockage(settings.get(CLE_PREREGLAGES))? {
        None => Ok(PreReglages::default()),
        Some(brut) => {
            serde_json::from_str(&brut).map_err(|e| ErreurStockage::Illisible(e.to_string()))
        }
    }
}

/// Enregistre la liste après [`PreReglages::verifier`].
pub fn ecrire_prereglages(settings: &SettingsRepo, l: &PreReglages) -> Result<(), ErreurStockage> {
    l.verifier()?;
    let texte = serde_json::to_string(l).map_err(|e| ErreurStockage::Stockage(e.to_string()))?;
    stockage(settings.set(CLE_PREREGLAGES, &texte))
}

/// L'identifiant tiré d'un nom : minuscules ASCII, chiffres et tirets
/// (règle de [`super::greffon::id_valide`]), 64 caractères au plus.
pub fn id_depuis_le_nom(nom: &str) -> String {
    let mut id = String::new();
    for c in nom.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
        } else if !id.ends_with('-') && !id.is_empty() {
            id.push('-');
        }
    }
    let mut id: String = id.trim_matches('-').chars().take(64).collect();
    while id.ends_with('-') {
        id.pop();
    }
    if id.is_empty() { "preset".into() } else { id }
}

/// « Name the current chain » + « Save » : enregistre `chaine` sous `nom`.
///
/// Un préréglage utilisateur du MÊME nom est remplacé (même id) ; sinon un
/// nouvel id unique est tiré du nom. Le préréglage Pure (fourni par le
/// serveur) n'est jamais écrasé : un homonyme reçoit un autre id.
pub fn enregistrer_la_chaine(
    settings: &SettingsRepo,
    nom: &str,
    chaine: &ChaineTess,
) -> Result<PreReglage, ErreurStockage> {
    let nom = nom.trim();
    if nom.is_empty() {
        return Err(ErreurStockage::NomVide);
    }
    let mut l = lire_prereglages(settings)?;
    let chaine = chaine.clone().mettre_a_niveau();
    let p = if let Some(p) = l.presets.iter_mut().find(|p| !p.builtin && p.name == nom) {
        p.chain = chaine;
        p.clone()
    } else {
        let base = id_depuis_le_nom(nom);
        let mut id = base.clone();
        let mut n = 2;
        while id == ID_PREREGLAGE_PURE || l.get(&id).is_some() {
            let suffixe = format!("-{n}");
            let tete: String = base.chars().take(64 - suffixe.len()).collect();
            id = format!("{}{suffixe}", tete.trim_end_matches('-'));
            n += 1;
        }
        let p = PreReglage {
            id,
            name: nom.to_owned(),
            builtin: false,
            chain: chaine,
        };
        l.presets.push(p.clone());
        p
    };
    ecrire_prereglages(settings, &l)?;
    Ok(p)
}

/// Supprime un préréglage (la croix de A2 ; Pure compris).
pub fn supprimer(settings: &SettingsRepo, id: &str) -> Result<(), ErreurStockage> {
    let mut l = lire_prereglages(settings)?;
    let avant = l.presets.len();
    l.presets.retain(|p| p.id != id);
    if l.presets.len() == avant {
        return Err(ErreurStockage::Inconnu(id.to_owned()));
    }
    ecrire_prereglages(settings, &l)
}

/// Bascule « Immediately » / « At next playback ».
pub fn definir_le_mode(
    settings: &SettingsRepo,
    mode: ModeApplication,
) -> Result<PreReglages, ErreurStockage> {
    let mut l = lire_prereglages(settings)?;
    l.apply_mode = mode;
    ecrire_prereglages(settings, &l)?;
    Ok(l)
}

/// Le mode qui s'applique : une valeur inconnue vaut `at_next_playback`
/// (jamais d'interruption non voulue, contrat § 5).
pub fn mode_effectif(mode: ModeApplication) -> ModeApplication {
    match mode {
        ModeApplication::Immediately => ModeApplication::Immediately,
        _ => ModeApplication::AtNextPlayback,
    }
}

/// Charge le préréglage `id` sur la zone : validation (droits au
/// chargement), écriture par T02, préréglage actif. Rend la chaîne écrite.
pub fn charger(
    settings: &SettingsRepo,
    zone_id: i64,
    id: &str,
    catalogue: &Catalogue,
    premium: bool,
) -> Result<ChaineTess, ErreurStockage> {
    let l = lire_prereglages(settings)?;
    let p = l
        .get(id)
        .ok_or_else(|| ErreurStockage::Inconnu(id.to_owned()))?;
    p.chain
        .valider(catalogue, ContexteValidation { premium })
        .map_err(ErreurStockage::Chaine)?;
    let chaine = p.chain.clone().mettre_a_niveau();
    ecrire_chaine(settings, zone_id, &chaine)
        .map_err(|e: ErreurMigration| ErreurStockage::Migration(e.to_string()))?;
    stockage(settings.set(&cle_prereglage_actif(zone_id), id))?;
    Ok(chaine)
}

/// Le préréglage « ACTIVE » de la zone : celui chargé en dernier, SI la
/// chaîne de la zone lui est encore identique. `chaine_courante` est la
/// chaîne lue par T02 (`lire_zone`).
pub fn prereglage_actif(
    settings: &SettingsRepo,
    zone_id: i64,
    chaine_courante: &ChaineTess,
) -> Option<String> {
    let id = settings
        .get(&cle_prereglage_actif(zone_id))
        .ok()
        .flatten()?;
    let l = lire_prereglages(settings).ok()?;
    let p = l.get(&id)?;
    (p.chain.clone().mettre_a_niveau() == *chaine_courante).then_some(id)
}

#[cfg(test)]
#[path = "prereglages_stockage_tests.rs"]
mod tests;
