//! La mémoire du greffon : la collection lue et ce qui a été appris de
//! chaque disque, dans UN fichier JSON du dossier du greffon.
//!
//! Pourquoi un fichier et pas une table : tout ce qui est ici se RELIT chez
//! Discogs, MusicBrainz et les services ; c'est un cache, pas une donnée de
//! l'utilisateur. Une table imposerait la double migration SQLite /
//! PostgreSQL pour rien. Seul le nom d'utilisateur Discogs, qui est un
//! réglage, vit dans `settings` (`crate::REGLAGE_NOM`).
//!
//! Ce que la mémoire garde, et combien de temps :
//!
//! * la LISTE des disques : remplacée en entier à chaque synchronisation —
//!   c'est ainsi qu'un disque vendu sort de la liste ;
//! * le CODE-BARRE d'une fiche : pour toujours (un pressage ne change pas) ;
//! * les liens MUSICBRAINZ : trouvés, pour toujours ; « aucun lien »,
//!   [`REVOIR_APRES_JOURS`] jours (quelqu'un a pu les ajouter depuis). Un
//!   disque sans entrée ici est un disque que l'affinage n'a pas encore lu :
//!   c'est ainsi qu'il reprend après un redémarrage ;
//! * le STREAMING : [`REVOIR_APRES_JOURS`] jours, ou dès que la liste des
//!   services connectés change. N'y sont notés que les services qui ont
//!   RÉPONDU : un service muet laisse le disque à revoir.
//!
//! L'écriture passe par un fichier temporaire renommé : un arrêt brutal
//! laisse l'ancienne mémoire, jamais une moitié.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::correspondance::LiensMusicBrainz;
use crate::discogs::Disque;

pub const FICHIER: &str = "collection.json";
pub const VERSION: u32 = 1;
pub const REVOIR_APRES_JOURS: i64 = 30;
/// La forme des disques de la liste. 2 : avec la grande image
/// (`Disque::pochette`, cache de pochettes du 01/11/2026). Une liste d'une
/// forme plus ancienne se relit en ENTIER à la synchronisation suivante
/// (sans rien refaire d'autre) ; une liste à jour se relit par le haut
/// seulement (`crate::synchro`, relecture partielle).
pub const VERSION_LISTE: u32 = 2;
/// La liste se relit en entier au moins tous les tant de jours, même quand
/// la relecture partielle suffit : c'est ainsi qu'une fiche corrigée chez
/// Discogs (titre, label) finit par arriver.
pub const RELIRE_TOUTE_LA_LISTE_JOURS: i64 = 7;

/// Ce que la fiche Discogs d'un disque a appris.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fiche {
    pub codes_barres: Vec<String>,
    pub lue_le: i64,
}

/// Les liens MusicBrainz d'un disque (pressage par la release Discogs, album
/// par le master).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Liens {
    pub liens: LiensMusicBrainz,
    pub lus_le: i64,
}

/// Un album trouvé chez un service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurUnService {
    pub service: String,
    pub id: String,
    pub titre: String,
    pub artiste: String,
    /// `code_barre` ou `artiste_titre`.
    pub par: String,
}

/// Ce que les services ont répondu pour un disque.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streaming {
    pub trouves: Vec<SurUnService>,
    /// Les services qui ont RÉPONDU : si ce n'est pas la liste des services
    /// connectés (l'un s'est tu, ou la liste a changé), la réponse est à
    /// refaire, et le disque n'est pas « nulle part ».
    pub services: Vec<String>,
    pub lu_le: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memoire {
    pub version: u32,
    /// Le compte dont la liste a été lue. Une liste d'un autre compte n'est
    /// jamais montrée.
    pub nom_utilisateur: Option<String>,
    pub disques: Vec<Disque>,
    /// La liste a été coupée à `discogs::PAGES_MAX` pages.
    pub tronquee: bool,
    pub liste_lue_le: Option<i64>,
    /// Par release Discogs.
    pub fiches: BTreeMap<u64, Fiche>,
    pub liens_mb: BTreeMap<u64, Liens>,
    pub streaming: BTreeMap<u64, Streaming>,
    /// Fin de la dernière synchronisation complète.
    pub derniere_synchro: Option<i64>,
    /// Le code de la dernière erreur, effacé par une synchronisation réussie.
    /// Jamais MusicBrainz : son affinage ne fait pas d'erreur.
    pub derniere_erreur: Option<String>,
    /// La dernière relecture COMPLÈTE de la liste (toutes les pages).
    /// `serde(default)` : une mémoire d'avant le 01/11/2026 se relit.
    #[serde(default)]
    pub liste_complete_le: Option<i64>,
    /// [`VERSION_LISTE`] de la liste rangée ; 0 pour une mémoire d'avant.
    #[serde(default)]
    pub version_liste: u32,
}

impl Memoire {
    pub fn chemin(dossier: &Path) -> PathBuf {
        dossier.join(FICHIER)
    }

    /// La mémoire rangée, ou une mémoire vide si le fichier manque ou ne se
    /// lit pas (d'une autre version) : tout se relit.
    pub fn lire(dossier: &Path) -> Memoire {
        let Ok(brut) = std::fs::read(Self::chemin(dossier)) else {
            return Memoire::default();
        };
        match serde_json::from_slice::<Memoire>(&brut) {
            Ok(m) if m.version == VERSION => m,
            Ok(_) | Err(_) => {
                tracing::warn!(target: "plugin", "collection_physique_memoire_illisible_relue");
                Memoire::default()
            }
        }
    }

    pub fn ecrire(&self, dossier: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dossier).map_err(|e| e.to_string())?;
        let mut m = self.clone();
        m.version = VERSION;
        let octets = serde_json::to_vec(&m).map_err(|e| e.to_string())?;
        let provisoire = dossier.join(format!("{FICHIER}.tmp"));
        std::fs::write(&provisoire, octets).map_err(|e| e.to_string())?;
        std::fs::rename(&provisoire, Self::chemin(dossier)).map_err(|e| e.to_string())
    }

    /// Oublie tout ce qui vient d'un compte : la liste, et l'avancement.
    /// Les fiches et liens MusicBrainz, propres à un disque et pas à un
    /// compte, restent.
    pub fn changer_de_compte(&mut self, nom: Option<String>) {
        if self.nom_utilisateur != nom {
            self.nom_utilisateur = nom;
            self.disques.clear();
            self.tronquee = false;
            self.liste_lue_le = None;
            self.derniere_synchro = None;
            self.derniere_erreur = None;
            self.liste_complete_le = None;
            self.version_liste = 0;
        }
    }

    /// La liste rangée peut-elle être complétée par le haut (relecture
    /// partielle) plutôt que relue en entier ?
    pub fn liste_partielle_possible(&self, nom: &str, maintenant: i64) -> bool {
        self.nom_utilisateur.as_deref() == Some(nom)
            && self.version_liste == VERSION_LISTE
            && !self.tronquee
            && !self.disques.is_empty()
            && self
                .liste_complete_le
                .is_some_and(|t| maintenant - t < RELIRE_TOUTE_LA_LISTE_JOURS * 86_400)
    }

    pub fn liens_a_revoir(&self, release: u64, maintenant: i64) -> bool {
        match self.liens_mb.get(&release) {
            None => true,
            Some(l) if l.liens.releases.is_empty() && l.liens.groupes.is_empty() => {
                maintenant - l.lus_le > REVOIR_APRES_JOURS * 86_400
            }
            Some(_) => false,
        }
    }

    pub fn streaming_a_revoir(&self, release: u64, services: &[String], maintenant: i64) -> bool {
        match self.streaming.get(&release) {
            None => true,
            Some(s) => s.services != services || maintenant - s.lu_le > REVOIR_APRES_JOURS * 86_400,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_memoire_se_relit_telle_qu_elle_a_ete_ecrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = Memoire {
            nom_utilisateur: Some("x".into()),
            ..Default::default()
        };
        m.fiches.insert(
            7,
            Fiche {
                codes_barres: vec!["123456".into()],
                lue_le: 3,
            },
        );
        m.ecrire(dir.path()).unwrap();
        let relue = Memoire::lire(dir.path());
        assert_eq!(relue.fiches, m.fiches);
        assert_eq!(relue.nom_utilisateur.as_deref(), Some("x"));
        assert!(!dir.path().join("collection.json.tmp").exists());
    }

    #[test]
    fn un_fichier_abime_vaut_une_memoire_vide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FICHIER), b"{pas du json").unwrap();
        assert_eq!(Memoire::lire(dir.path()), Memoire::default());
    }

    #[test]
    fn changer_de_compte_oublie_la_liste_mais_garde_les_fiches() {
        let mut m = Memoire {
            nom_utilisateur: Some("a".into()),
            liste_lue_le: Some(1),
            ..Default::default()
        };
        m.fiches.insert(1, Fiche::default());
        m.changer_de_compte(Some("b".into()));
        assert!(m.disques.is_empty());
        assert_eq!(m.liste_lue_le, None);
        assert_eq!(m.fiches.len(), 1);
    }

    #[test]
    fn un_lien_absent_se_revoit_apres_trente_jours_un_lien_trouve_jamais() {
        let mut m = Memoire::default();
        m.liens_mb.insert(
            1,
            Liens {
                liens: LiensMusicBrainz::default(),
                lus_le: 0,
            },
        );
        m.liens_mb.insert(
            2,
            Liens {
                liens: LiensMusicBrainz {
                    releases: vec!["r".into()],
                    groupes: vec![],
                },
                lus_le: 0,
            },
        );
        let plus_tard = 31 * 86_400;
        assert!(m.liens_a_revoir(1, plus_tard));
        assert!(!m.liens_a_revoir(1, 86_400));
        assert!(!m.liens_a_revoir(2, plus_tard));
        assert!(m.liens_a_revoir(3, 0));
    }
}
