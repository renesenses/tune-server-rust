//! Les correspondances de SERVICE choisies ou retirées à la main
//! (tune-server-rust#9166, go de Bertrand du 19/11/2026, demande d'un
//! testeur).
//!
//! La synchronisation rapproche un disque d'un album de service (Qobuz,
//! TIDAL) ; quand elle se trompe, l'utilisateur relance « Chercher sur les
//! services » (#7928) et, pour CE service :
//!
//! * **remplace** la correspondance par un autre résultat (`PUT
//!   /disques/{release}/streaming`) ;
//! * ou la **retire** (`DELETE /disques/{release}/streaming/{service}`).
//!
//! Les choix sont rangés dans les réglages de la base (une clé,
//! [`REGLAGE`]), PAS dans la mémoire du greffon (`collection.json`) : comme
//! les associations à un album local (#8672), « Effacer le cache » et les
//! synchronisations Discogs ne les touchent pas. La synchronisation continue
//! d'écrire ce qu'elle trouve ; le choix est appliqué à l'AFFICHAGE
//! ([`appliquer`]), par-dessus. Une correspondance retirée à la main reste
//! donc retirée au passage suivant du rapprochement automatique : c'est le
//! marqueur « refusé par l'utilisateur » (`null` pour ce service).
//!
//! « Revenir à l'automatique » (`DELETE
//! /disques/{release}/streaming/{service}/choix`) efface le choix ou le
//! retrait de ce service : le rapprochement automatique reprend la main.
//!
//! La forme rangée : `{"<release>": {"<service>": {id, titre, artiste} | null}}`.
//! Elle reste bornée : un service n'y entre que s'il est connu du serveur
//! (liste blanche, voir `lib.rs`), et les disques sortis de la collection en
//! sont purgés ([`purger`]). Un réglage illisible n'est jamais écrasé en
//! silence : il est journalisé et copié sous [`REGLAGE_ILLISIBLE`] avant la
//! première écriture.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::memoire::SurUnService;

/// La clé du réglage.
pub const REGLAGE: &str = "collection_physique_choix_streaming";

/// La copie d'un réglage illisible, gardée avant qu'une écriture ne le
/// remplace.
pub const REGLAGE_ILLISIBLE: &str = "collection_physique_choix_streaming_illisible";

/// `par` d'une correspondance choisie à la main.
pub const PAR_MANUEL: &str = "manuel";

/// Les longueurs admises (en caractères).
const SERVICE_MAX: usize = 32;
const ID_MAX: usize = 256;
const TEXTE_MAX: usize = 1024;

/// L'album de service choisi à la main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choisi {
    pub id: String,
    pub titre: String,
    pub artiste: String,
}

/// Pour une release : service → choisi (`Some`) ou retiré (`None`).
pub type ChoixDuDisque = BTreeMap<String, Option<Choisi>>;

/// Tous les choix : release → choix du disque.
pub type Choix = BTreeMap<u64, ChoixDuDisque>;

/// Les lectures-écritures du réglage se suivent une à une.
static ECRITURE: Mutex<()> = Mutex::new(());

/// Un nom de service plausible : minuscules ASCII, chiffres, `-`, `_`.
pub fn service_valide(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= SERVICE_MAX
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Un choix recevable : identifiant non vide, textes bornés.
pub fn choisi_valide(c: &Choisi) -> bool {
    let id = c.id.trim();
    !id.is_empty()
        && id.chars().count() <= ID_MAX
        && c.titre.chars().count() <= TEXTE_MAX
        && c.artiste.chars().count() <= TEXTE_MAX
}

/// Le réglage tel que rangé : les choix, ou le texte brut s'il est
/// illisible.
fn lire_brut(db: &Arc<dyn DbBackend>) -> Result<Result<Choix, String>, String> {
    let brut = SettingsRepo::with_backend(db.clone()).get(REGLAGE)?;
    Ok(match brut {
        None => Ok(Choix::new()),
        Some(s) => serde_json::from_str(&s).map_err(|e| {
            tracing::warn!(target: "plugin", erreur = %e, "collection_physique_choix_streaming_illisibles");
            s
        }),
    })
}

/// Les choix rangés. Un réglage illisible vaut « aucun choix » (et se dit
/// au journal) : l'écran ne tombe pas pour autant, et rien n'est écrit.
pub fn lire(db: &Arc<dyn DbBackend>) -> Result<Choix, String> {
    Ok(lire_brut(db)?.unwrap_or_default())
}

/// Lit, modifie, range. `f` dit s'il a changé quelque chose : sinon rien
/// n'est écrit. Un réglage illisible est d'abord copié sous
/// [`REGLAGE_ILLISIBLE`] (s'il n'y en a pas déjà une copie), puis remplacé.
fn modifier(db: &Arc<dyn DbBackend>, f: impl FnOnce(&mut Choix) -> bool) -> Result<bool, String> {
    let _g = ECRITURE.lock().unwrap_or_else(|e| e.into_inner());
    let repo = SettingsRepo::with_backend(db.clone());
    let (mut choix, illisible) = match lire_brut(db)? {
        Ok(c) => (c, None),
        Err(brut) => (Choix::new(), Some(brut)),
    };
    if !f(&mut choix) {
        return Ok(false);
    }
    if let Some(brut) = illisible {
        if repo.get(REGLAGE_ILLISIBLE)?.is_none() {
            repo.set(REGLAGE_ILLISIBLE, &brut)?;
        }
        tracing::warn!(target: "plugin", copie = REGLAGE_ILLISIBLE, "collection_physique_choix_streaming_illisibles_remplaces");
    }
    choix.retain(|_, c| !c.is_empty());
    if choix.is_empty() {
        repo.delete(REGLAGE)?;
    } else {
        let s = serde_json::to_string(&choix).map_err(|e| e.to_string())?;
        repo.set(REGLAGE, &s)?;
    }
    Ok(true)
}

/// Remplace la correspondance de `service` pour la release par `choisi`.
pub fn choisir(
    db: &Arc<dyn DbBackend>,
    release: u64,
    service: &str,
    choisi: Choisi,
) -> Result<(), String> {
    modifier(db, |c| {
        c.entry(release)
            .or_default()
            .insert(service.to_string(), Some(choisi));
        true
    })
    .map(|_| ())
}

/// Retire la correspondance de `service` pour la release : refusée par
/// l'utilisateur, elle ne revient pas avec le rapprochement automatique.
pub fn retirer(db: &Arc<dyn DbBackend>, release: u64, service: &str) -> Result<(), String> {
    modifier(db, |c| {
        c.entry(release)
            .or_default()
            .insert(service.to_string(), None);
        true
    })
    .map(|_| ())
}

/// « Revenir à l'automatique » : oublie le choix ou le retrait de `service`
/// pour la release. `true` s'il y en avait un.
pub fn oublier(db: &Arc<dyn DbBackend>, release: u64, service: &str) -> Result<bool, String> {
    modifier(db, |c| {
        c.get_mut(&release)
            .is_some_and(|d| d.remove(service).is_some())
    })
}

/// Oublie les choix des disques qui ne sont plus dans la collection. Rend
/// le nombre de disques purgés ; n'écrit rien s'il n'y en a aucun.
pub fn purger(db: &Arc<dyn DbBackend>, garder: impl Fn(u64) -> bool) -> Result<usize, String> {
    let mut n = 0;
    modifier(db, |c| {
        let avant = c.len();
        c.retain(|r, _| garder(*r));
        n = avant - c.len();
        n > 0
    })?;
    Ok(n)
}

/// Ce que l'écran montre : les correspondances du rapprochement automatique
/// (`auto`), corrigées des choix du disque. Rend aussi les services dont la
/// correspondance a été retirée à la main, dans l'ordre des noms.
pub fn appliquer(
    auto: &[SurUnService],
    choix: Option<&ChoixDuDisque>,
) -> (Vec<SurUnService>, Vec<String>) {
    let Some(choix) = choix else {
        return (auto.to_vec(), Vec::new());
    };
    let mut rendus: Vec<SurUnService> = auto
        .iter()
        .filter(|s| !choix.contains_key(&s.service))
        .cloned()
        .collect();
    let mut retires = Vec::new();
    for (service, c) in choix {
        match c {
            Some(c) => rendus.push(SurUnService {
                service: service.clone(),
                id: c.id.clone(),
                titre: c.titre.clone(),
                artiste: c.artiste.clone(),
                par: PAR_MANUEL.to_string(),
            }),
            None => retires.push(service.clone()),
        }
    }
    (rendus, retires)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sur(service: &str, id: &str) -> SurUnService {
        SurUnService {
            service: service.into(),
            id: id.into(),
            titre: "T".into(),
            artiste: "A".into(),
            par: "artiste_titre".into(),
        }
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn sans_choix_l_automatique_passe_tel_quel_9166() {
        let auto = vec![sur("qobuz", "Q1"), sur("tidal", "T1")];
        let (r, retires) = appliquer(&auto, None);
        assert_eq!(r, auto);
        assert!(retires.is_empty());
    }

    #[test]
    fn un_choix_remplace_le_service_et_un_retrait_l_efface_9166() {
        let auto = vec![sur("qobuz", "Q1"), sur("tidal", "T1")];
        let mut choix = ChoixDuDisque::new();
        choix.insert(
            "qobuz".into(),
            Some(Choisi {
                id: "Q2".into(),
                titre: "Bon".into(),
                artiste: "Lui".into(),
            }),
        );
        choix.insert("tidal".into(), None);
        let (r, retires) = appliquer(&auto, Some(&choix));
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].service, "qobuz");
        assert_eq!(r[0].id, "Q2");
        assert_eq!(r[0].par, PAR_MANUEL);
        assert_eq!(retires, vec!["tidal".to_string()]);
    }

    #[test]
    fn les_choix_se_rangent_et_se_relisent_9166() {
        let db = base();
        assert!(lire(&db).unwrap().is_empty());
        retirer(&db, 7, "tidal").unwrap();
        choisir(
            &db,
            7,
            "qobuz",
            Choisi {
                id: "Q9".into(),
                titre: "X".into(),
                artiste: "Y".into(),
            },
        )
        .unwrap();
        let c = lire(&db).unwrap();
        assert_eq!(c[&7]["tidal"], None);
        assert_eq!(c[&7]["qobuz"].as_ref().unwrap().id, "Q9");
        // Retirer après avoir choisi : le choix devient un refus.
        retirer(&db, 7, "qobuz").unwrap();
        assert_eq!(lire(&db).unwrap()[&7]["qobuz"], None);
    }

    #[test]
    fn oublier_et_purger_9166() {
        let db = base();
        retirer(&db, 7, "tidal").unwrap();
        retirer(&db, 8, "qobuz").unwrap();
        assert!(oublier(&db, 7, "tidal").unwrap(), "il y avait un retrait");
        assert!(!oublier(&db, 7, "tidal").unwrap(), "plus rien");
        assert!(!lire(&db).unwrap().contains_key(&7), "disque vide retiré");
        assert_eq!(purger(&db, |r| r != 8).unwrap(), 1);
        assert!(lire(&db).unwrap().is_empty());
        assert_eq!(purger(&db, |_| false).unwrap(), 0);
    }

    #[test]
    fn un_reglage_illisible_est_copie_avant_d_etre_remplace_9166() {
        let db = base();
        let repo = SettingsRepo::with_backend(db.clone());
        repo.set(REGLAGE, "{pas du json").unwrap();
        assert!(lire(&db).unwrap().is_empty(), "illisible : aucun choix");
        assert_eq!(
            repo.get(REGLAGE).unwrap().as_deref(),
            Some("{pas du json"),
            "une lecture n'écrit rien"
        );
        retirer(&db, 7, "tidal").unwrap();
        assert_eq!(
            repo.get(REGLAGE_ILLISIBLE).unwrap().as_deref(),
            Some("{pas du json"),
            "copie gardée"
        );
        assert_eq!(lire(&db).unwrap()[&7]["tidal"], None);
    }

    #[test]
    fn noms_de_service_et_choix_bornes_9166() {
        assert!(service_valide("qobuz"));
        assert!(service_valide("apple-music"));
        assert!(!service_valide(""));
        assert!(!service_valide("Qobuz"));
        assert!(!service_valide("../x"));
        assert!(!service_valide(&"a".repeat(33)));
        let c = |id: &str| Choisi {
            id: id.into(),
            titre: String::new(),
            artiste: String::new(),
        };
        assert!(choisi_valide(&c("123")));
        assert!(!choisi_valide(&c("  ")));
        assert!(!choisi_valide(&c(&"9".repeat(257))));
    }
}
