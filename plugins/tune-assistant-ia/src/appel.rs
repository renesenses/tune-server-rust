//! Le chemin unique de tout appel : cache, plafond, fournisseur, comptage.
//!
//! L'ordre est la garantie :
//!
//! 1. le cache d'abord — une question déjà posée ne coûte rien, même plafond
//!    atteint ;
//! 2. le plafond ensuite, sous verrou — deux appels simultanés ne peuvent pas
//!    franchir ensemble un plafond que chacun aurait vu ouvert ;
//! 3. l'appel, puis le comptage de SES jetons, au tarif de la table.

use std::sync::{Arc, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;

use crate::Etat;
use crate::cache::{self, Cache};
use crate::coffre::Coffre;
use crate::couts;
use crate::fournisseur::{Demande, ErreurFournisseur, Fabrique};
use crate::reglages::{self, EtatCle, Reglages};

/// LE verrou de la consommation, unique pour tout le processus.
///
/// Étape 2 : le greffon n'est plus seul à dépenser — l'ancien `/ai/query` et
/// la traduction des ambiances passent par [`crate::passerelle::Passerelle`],
/// que l'hôte construit de son côté. Deux verrous distincts laisseraient deux
/// appels franchir ensemble un plafond que chacun aurait vu ouvert.
pub fn verrou_de_consommation() -> Arc<tokio::sync::Mutex<()>> {
    static VERROU: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    VERROU
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Ce dont un appel a besoin. Le greffon passe son [`Etat`] ; la passerelle
/// de l'hôte, la même base, le même coffre, le même verrou — sans cache.
pub struct Moyens<'a> {
    pub backend: &'a Arc<dyn DbBackend>,
    pub coffre: &'a Coffre,
    pub fabrique: &'a Fabrique,
    pub verrou: &'a tokio::sync::Mutex<()>,
    pub cache: Option<&'a Cache>,
}

impl<'a> Moyens<'a> {
    pub fn de(etat: &'a Etat) -> Self {
        Self {
            backend: &etat.backend,
            coffre: &etat.coffre,
            fabrique: &etat.fabrique,
            verrou: &etat.verrou_conso,
            cache: Some(&etat.cache),
        }
    }
}

/// Version des consignes : la changer invalide le cache.
pub const VERSION_CONSIGNES: &str = "v1";

/// Pourquoi un appel n'a pas eu lieu, ou a échoué.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErreurAppel {
    NonConfigure,
    CleIllisible,
    TarifInconnu,
    PlafondAtteint,
    Fournisseur(ErreurFournisseur),
}

impl ErreurAppel {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonConfigure => "non_configure",
            Self::CleIllisible => "cle_illisible",
            Self::TarifInconnu => "tarif_inconnu",
            Self::PlafondAtteint => "plafond_atteint",
            Self::Fournisseur(e) => e.code(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NonConfigure => {
                "Assistant IA non configuré : choisissez un fournisseur et saisissez votre clé dans Réglages ▸ Assistant IA.".into()
            }
            Self::CleIllisible => {
                "La clé enregistrée ne s'ouvre plus sur ce serveur : saisissez-la de nouveau.".into()
            }
            Self::TarifInconnu => {
                "Tarif inconnu pour ce modèle : renseignez ses prix dans Réglages ▸ Assistant IA.".into()
            }
            Self::PlafondAtteint => {
                "Plafond mensuel de dépenses atteint : plus aucun appel ce mois-ci. Relevez-le dans Réglages ▸ Assistant IA.".into()
            }
            Self::Fournisseur(e) => e.message(),
        }
    }

    /// Jamais 401 : le client web y lit une session expirée et déconnecte.
    pub fn statut(&self) -> StatusCode {
        match self {
            Self::NonConfigure | Self::CleIllisible | Self::TarifInconnu => StatusCode::BAD_REQUEST,
            Self::PlafondAtteint => StatusCode::CONFLICT,
            Self::Fournisseur(ErreurFournisseur::CleRefusee) => StatusCode::BAD_REQUEST,
            Self::Fournisseur(ErreurFournisseur::CleSansEspaceDeTravail) => StatusCode::BAD_REQUEST,
            Self::Fournisseur(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn corps(&self) -> Value {
        let mut corps = json!({ "ok": false, "error": self.code(), "message": self.message() });
        // #6730 — le motif du fournisseur, à part : l'écran le montre sous son
        // message traduit. Absent quand le fournisseur n'en a pas donné.
        if let Self::Fournisseur(e) = self
            && let Some(m) = e.motif()
        {
            corps["motif"] = json!(m);
        }
        corps
    }
}

/// Quel modèle pour un appel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choix {
    /// Le modèle habituel (Haiku 5.5 par défaut chez Anthropic).
    Defaut,
    /// Le modèle « classique » s'il est choisi, sinon l'habituel.
    Classique,
    /// Le second avis demandé par l'utilisateur : Sonnet 5.5 chez Anthropic
    /// ([`Reglages::modele_fort`]).
    Fort,
}

impl Choix {
    pub fn depuis_classique(classique: bool) -> Self {
        if classique {
            Self::Classique
        } else {
            Self::Defaut
        }
    }
}

/// Une réponse, et d'où elle vient.
#[derive(Clone, Debug)]
pub struct Resultat {
    pub texte: String,
    pub depuis_cache: bool,
    pub cout_micro_eur: u64,
}

/// Un appel complet. `entree_cache` identifie la question (déjà normalisée) ;
/// `None` = ne pas utiliser le cache (« Tester la clé »).
pub async fn appeler(
    etat: &Etat,
    usage: &str,
    classique: bool,
    systeme: &str,
    message: &str,
    max_jetons: u32,
    entree_cache: Option<&str>,
) -> Result<Resultat, ErreurAppel> {
    appeler_avec(
        &Moyens::de(etat),
        usage,
        classique,
        systeme,
        message,
        max_jetons,
        entree_cache,
        true,
    )
    .await
}

/// [`appeler`], avec des moyens explicites. Sans cache (`m.cache = None`),
/// `entree_cache` est ignorée. `json` : demander au fournisseur une réponse
/// JSON quand il sait la garantir (faux pour une traduction en texte brut).
#[allow(clippy::too_many_arguments)]
pub async fn appeler_avec(
    m: &Moyens<'_>,
    usage: &str,
    classique: bool,
    systeme: &str,
    message: &str,
    max_jetons: u32,
    entree_cache: Option<&str>,
    json: bool,
) -> Result<Resultat, ErreurAppel> {
    appeler_choix(
        m,
        usage,
        Choix::depuis_classique(classique),
        systeme,
        message,
        max_jetons,
        entree_cache,
        json,
    )
    .await
}

/// [`appeler_avec`], avec le modèle désigné par un [`Choix`].
#[allow(clippy::too_many_arguments)]
pub async fn appeler_choix(
    m: &Moyens<'_>,
    usage: &str,
    choix: Choix,
    systeme: &str,
    message: &str,
    max_jetons: u32,
    entree_cache: Option<&str>,
    json: bool,
) -> Result<Resultat, ErreurAppel> {
    let r = Reglages::charger(m.backend);
    let famille = r.famille.ok_or(ErreurAppel::NonConfigure)?;
    let modele = r.modele_choisi(choix);
    if modele.is_empty() {
        return Err(ErreurAppel::NonConfigure);
    }
    let cle_cache = entree_cache
        .filter(|_| m.cache.is_some())
        .map(|e| cache::cle(&[VERSION_CONSIGNES, usage, famille.nom(), &modele, e]));

    // 1. Le cache.
    if let (Some(k), Some(cache)) = (&cle_cache, m.cache)
        && let Some(texte) = cache.lire(k)
    {
        let _verrou = m.verrou.lock().await;
        let mut c = reglages::charger_consommation(m.backend);
        c.ajouter_cache(usage);
        let _ = reglages::ranger_consommation(m.backend, &c);
        info!(usage, "assistant_ia_reponse_du_cache");
        return Ok(Resultat {
            texte,
            depuis_cache: true,
            cout_micro_eur: 0,
        });
    }

    // 2. La clé et le tarif.
    let (acces, tarif) = acces_et_tarif(m.backend, m.coffre, &r, &modele)?;

    // 3. Le plafond, sous verrou jusqu'au comptage.
    let _verrou = m.verrou.lock().await;
    let mut c = reglages::charger_consommation(m.backend);
    if c.micro_eur >= r.plafond_micro_eur {
        info!(usage, "assistant_ia_plafond_atteint");
        return Err(ErreurAppel::PlafondAtteint);
    }

    let fournisseur = (m.fabrique)(acces);
    let demande = Demande {
        modele: modele.clone(),
        systeme: systeme.to_string(),
        message: message.to_string(),
        max_jetons,
        json,
        // Décision du 23/10/2026 : `effort: low` pour les usages courts.
        effort: couts::usage_court(usage).then_some(couts::EFFORT_COURT),
    };
    let rep = fournisseur
        .completer(&demande)
        .await
        .map_err(ErreurAppel::Fournisseur)?;

    let cout = compter(
        m.backend,
        &mut c,
        &r,
        tarif,
        usage,
        &modele,
        rep.jetons_entree,
        rep.jetons_sortie,
    );
    if let (Some(k), Some(cache)) = (&cle_cache, m.cache) {
        cache.ecrire(k, &rep.texte);
    }
    Ok(Resultat {
        texte: rep.texte,
        depuis_cache: false,
        cout_micro_eur: cout,
    })
}

/// La clé ouverte et le tarif du modèle, ou pourquoi l'appel ne peut partir.
pub(crate) fn acces_et_tarif(
    backend: &Arc<dyn DbBackend>,
    coffre: &Coffre,
    r: &Reglages,
    modele: &str,
) -> Result<(crate::fournisseur::Acces, couts::Tarif), ErreurAppel> {
    let famille = r.famille.ok_or(ErreurAppel::NonConfigure)?;
    let cle = match reglages::cle_en_clair(backend, coffre) {
        EtatCle::Ouverte(k) => Some(k),
        EtatCle::Illisible => return Err(ErreurAppel::CleIllisible),
        EtatCle::Absente if famille.exige_une_cle() => return Err(ErreurAppel::NonConfigure),
        EtatCle::Absente => None,
    };
    let tarif = r.tarif(modele).ok_or(ErreurAppel::TarifInconnu)?;
    let acces = reglages::acces(r, cle).ok_or(ErreurAppel::NonConfigure)?;
    Ok((acces, tarif))
}

/// Ajoute un appel à la consommation du mois et la range. À appeler SOUS le
/// verrou de consommation, avec la consommation lue sous ce même verrou.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compter(
    backend: &Arc<dyn DbBackend>,
    c: &mut couts::Consommation,
    r: &Reglages,
    tarif: couts::Tarif,
    usage: &str,
    modele: &str,
    jetons_entree: u64,
    jetons_sortie: u64,
) -> u64 {
    let cout = couts::cout_micro_eur(tarif, jetons_entree, jetons_sortie, r.taux_usd_eur);
    let avant = c.micro_eur;
    c.ajouter_appel(usage, cout, jetons_entree, jetons_sortie);
    let _ = reglages::ranger_consommation(backend, c);
    // Étape 3 : le passage du seuil d'alerte se journalise UNE fois, à
    // l'appel qui le franchit (montants seulement, jamais la clé).
    if !couts::alerte(avant, r.plafond_micro_eur) && couts::alerte(c.micro_eur, r.plafond_micro_eur)
    {
        warn!(
            usage,
            depense_micro_eur = c.micro_eur,
            plafond_micro_eur = r.plafond_micro_eur,
            "assistant_ia_alerte_80_pour_cent"
        );
    }
    info!(
        usage,
        fournisseur = r.famille.map(|f| f.nom()).unwrap_or(""),
        modele = %modele,
        jetons_entree,
        jetons_sortie,
        cout_micro_eur = cout,
        "assistant_ia_appel"
    );
    cout
}

/// Étape 3 : où en est la dépense du mois face au plafond, pour l'écran.
/// Joint à chaque réponse d'un usage (`plafond`), et aux réglages.
pub fn etat_plafond(backend: &Arc<dyn DbBackend>) -> Value {
    let r = Reglages::charger(backend);
    let c = reglages::charger_consommation(backend);
    etat_plafond_de(&c, r.plafond_micro_eur)
}

pub(crate) fn etat_plafond_de(c: &couts::Consommation, plafond_micro_eur: u64) -> Value {
    let part = if plafond_micro_eur == 0 {
        1.0
    } else {
        c.micro_eur as f64 / plafond_micro_eur as f64
    };
    json!({
        "depense_eur": couts::micro_en_euros(c.micro_eur),
        "plafond_eur": couts::micro_en_euros(plafond_micro_eur),
        "part": part,
        "seuil_alerte": couts::SEUIL_ALERTE_POURCENT,
        "alerte": couts::alerte(c.micro_eur, plafond_micro_eur),
        "atteint": c.micro_eur >= plafond_micro_eur,
    })
}

/// Retire d'une réponse ce qui entoure l'objet JSON (bloc ```json, phrase
/// d'introduction) et le lit.
pub fn objet_json(texte: &str) -> Option<Value> {
    let debut = texte.find('{')?;
    let fin = texte.rfind('}')?;
    if fin < debut {
        return None;
    }
    serde_json::from_str::<Value>(&texte[debut..=fin])
        .ok()
        .filter(Value::is_object)
}

/// Le message HTTP d'une erreur d'appel.
pub fn reponse_erreur(e: &ErreurAppel) -> axum::response::Response {
    use axum::response::IntoResponse;
    (e.statut(), axum::Json(e.corps())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_objet_json_se_retrouve_dans_un_bloc_de_code() {
        let v = objet_json("Voici :\n```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        assert!(objet_json("pas de json").is_none());
        assert!(objet_json("[1, 2]").is_none());
    }

    #[test]
    fn aucune_erreur_ne_rend_401() {
        for e in [
            ErreurAppel::NonConfigure,
            ErreurAppel::CleIllisible,
            ErreurAppel::TarifInconnu,
            ErreurAppel::PlafondAtteint,
            ErreurAppel::Fournisseur(ErreurFournisseur::CleRefusee),
            ErreurAppel::Fournisseur(ErreurFournisseur::Quota),
            ErreurAppel::Fournisseur(ErreurFournisseur::CleSansEspaceDeTravail),
        ] {
            assert_ne!(e.statut(), StatusCode::UNAUTHORIZED, "{e:?}");
        }
    }

    /// #6730 — l'écran reçoit le motif du fournisseur à part, et le message.
    #[test]
    fn le_corps_d_erreur_porte_le_motif_du_fournisseur_6730() {
        let e = ErreurAppel::Fournisseur(crate::fournisseur::erreur_de_statut(
            crate::fournisseur::Famille::CompatibleOpenai,
            400,
            r#"{"error":{"message":"Invalid model parameter"}}"#,
        ));
        let c = e.corps();
        assert_eq!(c["error"], "erreur_fournisseur");
        assert_eq!(
            c["motif"], "Invalid model parameter",
            "#6730 : le corps rendu à l'écran perd le motif du fournisseur : {c}"
        );
        let sans = ErreurAppel::Fournisseur(ErreurFournisseur::Quota).corps();
        assert!(sans.get("motif").is_none(), "{sans}");
    }
}
