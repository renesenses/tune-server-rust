//! Le parcours d'une source distante, REPRENABLE après une coupure (#8189).
//!
//! Sur un téléphone, une coupure est l'ordinaire : l'application passe en
//! arrière-plan, le Wi-Fi tombe, le NAS s'endort. Le parcours retient donc,
//! dossier par dossier, ce qu'il a déjà listé ([`PointDeReprise`]) : relancé
//! après une coupure, il repart de là au lieu de tout relister.
//!
//! Deux règles empruntées au parcours local :
//! - un parcours INTERROMPU rend une erreur, jamais une liste partielle : une
//!   liste tronquée ressemblerait à une bibliothèque dont des fichiers ont
//!   disparu, et la purge la prendrait au mot (`list_audio_files_avec_arret`) ;
//! - la reprise ne vaut que pour UN parcours : terminé, il l'efface, et le
//!   parcours suivant relit tout (c'est lui qui voit les fichiers changés).
//!
//! La lecture des étiquettes, elle, reprend d'elle-même : le scan incrémental
//! saute les fichiers déjà importés dont la taille et la date n'ont pas bougé.

use std::collections::BTreeMap;
use std::io;

use super::systeme::{Entree, SystemeDistant};

/// Profondeur maximale d'une arborescence distante.
pub const PROFONDEUR_MAX: usize = 64;
/// Nombre maximal d'entrées d'un parcours.
pub const ENTREES_MAX: usize = 1_000_000;

/// Ce qu'un parcours interrompu a déjà fait.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PointDeReprise {
    /// Le dossier de départ du parcours repris : une autre racine repart de zéro.
    pub racine: String,
    /// Les dossiers déjà listés, avec leur contenu.
    pub dossiers_finis: BTreeMap<String, Vec<Entree>>,
}

impl PointDeReprise {
    pub fn est_vide(&self) -> bool {
        self.dossiers_finis.is_empty()
    }
}

/// Un fichier trouvé : son chemin interne et sa description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FichierTrouve {
    pub chemin: String,
    pub entree: Entree,
}

/// Le résultat d'un parcours terminé.
#[derive(Debug, Default)]
pub struct Parcours {
    pub fichiers: Vec<FichierTrouve>,
    /// Dossiers listés pendant CE passage.
    pub dossiers_listes: usize,
    /// Dossiers repris du point de reprise, sans nouvelle demande.
    pub dossiers_repris: usize,
}

/// Les réglages d'un parcours.
pub struct Reglages<'a> {
    /// Garder ce fichier ? (`nom` de l'entrée ; par exemple, son extension).
    pub garder: &'a dyn Fn(&str) -> bool,
    /// Demande d'arrêt, consultée avant chaque dossier.
    pub arret: &'a dyn Fn() -> bool,
    /// Tentatives par dossier avant d'abandonner (au moins 1).
    pub tentatives: u32,
    /// Attente entre deux tentatives (doublée à chaque fois).
    pub attente: std::time::Duration,
}

fn joindre(dossier: &str, nom: &str) -> String {
    if dossier.is_empty() {
        nom.to_string()
    } else {
        format!("{dossier}/{nom}")
    }
}

fn lister_avec_reprises(
    systeme: &dyn SystemeDistant,
    dossier: &str,
    reglages: &Reglages<'_>,
) -> io::Result<Vec<Entree>> {
    let mut attente = reglages.attente;
    let mut derniere = None;
    for essai in 0..reglages.tentatives.max(1) {
        match systeme.lister(dossier) {
            Ok(e) => return Ok(e),
            // Un dossier absent ou refusé ne se règle pas en réessayant.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                return Err(e);
            }
            Err(e) => {
                tracing::debug!(dossier, essai, error = %e, "source_distante_liste_reessai");
                derniere = Some(e);
                if essai + 1 < reglages.tentatives && !attente.is_zero() {
                    std::thread::sleep(attente);
                    attente *= 2;
                }
            }
        }
    }
    Err(derniere.unwrap_or_else(|| io::Error::other("liste impossible")))
}

/// Parcourt `racine` (chemin interne) en profondeur, dans l'ordre des noms.
///
/// `reprise` est lu au départ et tenu à jour dossier par dossier : en cas
/// d'erreur, il porte tout ce qui a été listé, et le passage suivant le
/// reprend ; en cas de succès, il est vidé.
pub fn parcourir(
    systeme: &dyn SystemeDistant,
    racine: &str,
    reprise: &mut PointDeReprise,
    reglages: &Reglages<'_>,
) -> io::Result<Parcours> {
    let racine = racine.trim_matches('/').to_string();
    if reprise.racine != racine {
        *reprise = PointDeReprise {
            racine: racine.clone(),
            dossiers_finis: BTreeMap::new(),
        };
    }
    let mut parcours = Parcours::default();
    let mut pile: Vec<(String, usize)> = vec![(racine.clone(), 0)];
    let mut entrees = 0usize;
    while let Some((dossier, profondeur)) = pile.pop() {
        if (reglages.arret)() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "parcours interrompu",
            ));
        }
        let mut contenu = match reprise.dossiers_finis.get(&dossier) {
            Some(c) => {
                parcours.dossiers_repris += 1;
                c.clone()
            }
            None => {
                let c = lister_avec_reprises(systeme, &dossier, reglages)?;
                reprise.dossiers_finis.insert(dossier.clone(), c.clone());
                parcours.dossiers_listes += 1;
                c
            }
        };
        contenu.sort_by(|a, b| a.nom.cmp(&b.nom));
        let mut sous = Vec::new();
        for e in contenu {
            entrees += 1;
            if entrees > ENTREES_MAX {
                return Err(io::Error::other("trop d'entrées dans la source"));
            }
            if e.nom.is_empty() || e.nom == "." || e.nom == ".." || e.nom.contains('/') {
                continue;
            }
            if e.dossier {
                if e.nom.starts_with('.') || crate::scanner::dossier_systeme_ignore(&e.nom) {
                    continue;
                }
                if profondeur + 1 < PROFONDEUR_MAX {
                    sous.push(joindre(&dossier, &e.nom));
                }
            } else if !e.nom.starts_with("._") && (reglages.garder)(&e.nom) {
                parcours.fichiers.push(FichierTrouve {
                    chemin: joindre(&dossier, &e.nom),
                    entree: e,
                });
            }
        }
        // Dans l'ordre des noms : la pile se dépile à l'envers.
        for s in sous.into_iter().rev() {
            pile.push((s, profondeur + 1));
        }
    }
    reprise.dossiers_finis.clear();
    Ok(parcours)
}
