//! Contrôle parental, étape 1 (#7856) : la politique d'un profil supervisé et
//! le filtrage des listes qu'il reçoit.
//!
//! Fonction gratuite et sans effet tant qu'aucune politique n'est posée : un
//! profil sans ligne dans `politiques_parentales` reçoit ses listes telles
//! quelles. Les plages horaires et les budgets sont enregistrés et validés
//! ici ; leur application à la lecture est l'étape 2.
//!
//! Table sans numéro de migration, sur le modèle des favoris en miroir
//! (#5997) : créée à chaque démarrage par la passe finale de
//! `run_migrations` (SQLite) et par `ENSURE_TABLES` (PostgreSQL), déclarée
//! dans `PG_FULL_SCHEMA` et copiée à la bascule.

use super::backend::{DbBackend, ToSqlValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/politiques_parentales.sql");

/// Plafonds de taille d'une politique : une liste plus longue est refusée.
pub const ELEMENTS_MAX: usize = 5000;
pub const PLAGES_MAX: usize = 64;

/// Un artiste bloqué : son identifiant de bibliothèque quand il est connu, et
/// son nom, qui couvre aussi les résultats des services et de la recherche.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtisteBloque {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub nom: Option<String>,
}

/// Une plage d'écoute permise, dans le fuseau du serveur. `jours` : 1 = lundi
/// … 7 = dimanche ; `debut` et `fin` en `HH:MM`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlageHoraire {
    pub jours: Vec<u8>,
    pub debut: String,
    pub fin: String,
}

/// La politique d'un profil supervisé.
///
/// Listes blanches : `None` ne restreint rien ; `Some(liste)` ne laisse
/// paraître que les éléments listés (`Some([])` n'en laisse aucun).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolitiqueParentale {
    #[serde(default)]
    pub artistes_bloques: Vec<ArtisteBloque>,
    /// Identifiant de « Mes radios », adresse du flux ou identifiant d'annuaire.
    #[serde(default)]
    pub radios_autorisees: Option<Vec<String>>,
    /// Adresse du flux RSS.
    #[serde(default)]
    pub podcasts_autorises: Option<Vec<String>>,
    #[serde(default)]
    pub plages_horaires: Vec<PlageHoraire>,
    #[serde(default)]
    pub budget_quotidien_minutes: Option<u32>,
    #[serde(default)]
    pub budget_hebdomadaire_minutes: Option<u32>,
}

impl PolitiqueParentale {
    /// Le premier défaut trouvé, sous forme de code stable.
    pub fn valider(&self) -> Result<(), &'static str> {
        if self.artistes_bloques.len() > ELEMENTS_MAX
            || self
                .radios_autorisees
                .as_ref()
                .is_some_and(|l| l.len() > ELEMENTS_MAX)
            || self
                .podcasts_autorises
                .as_ref()
                .is_some_and(|l| l.len() > ELEMENTS_MAX)
        {
            return Err("politique_trop_longue");
        }
        if self.artistes_bloques.iter().any(|a| {
            a.id.is_none()
                && a.nom
                    .as_deref()
                    .map(normaliser)
                    .unwrap_or_default()
                    .is_empty()
        }) {
            return Err("artiste_bloque_sans_id_ni_nom");
        }
        if self.plages_horaires.len() > PLAGES_MAX {
            return Err("trop_de_plages_horaires");
        }
        for p in &self.plages_horaires {
            if p.jours.is_empty() || p.jours.iter().any(|j| !(1..=7).contains(j)) {
                return Err("plage_horaire_jour_invalide");
            }
            let lire = |h: &str| chrono::NaiveTime::parse_from_str(h, "%H:%M").ok();
            match (lire(&p.debut), lire(&p.fin)) {
                (Some(d), Some(f)) if d < f => {}
                (Some(_), Some(_)) => return Err("plage_horaire_vide"),
                _ => return Err("plage_horaire_heure_invalide"),
            }
        }
        if self.budget_quotidien_minutes.is_some_and(|m| m > 24 * 60) {
            return Err("budget_quotidien_invalide");
        }
        if self
            .budget_hebdomadaire_minutes
            .is_some_and(|m| m > 7 * 24 * 60)
        {
            return Err("budget_hebdomadaire_invalide");
        }
        Ok(())
    }
}

/// Une politique telle qu'enregistrée.
#[derive(Debug, Clone, Serialize)]
pub struct PolitiqueEnregistree {
    pub profile_id: i64,
    pub superviseur_id: i64,
    pub revision: i64,
    pub politique: PolitiqueParentale,
    pub maj_le: String,
}

/// Issue d'une écriture.
#[derive(Debug, Clone, PartialEq)]
pub enum Ecriture {
    /// Écrite ; la nouvelle révision.
    Faite(i64),
    /// La révision attendue n'est plus celle de la base (ou la politique a été
    /// créée ou retirée entre-temps).
    Conflit,
}

/// La politique d'un profil, ou `None` : profil non supervisé.
pub fn lire(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
) -> Result<Option<PolitiqueEnregistree>, String> {
    let Some(row) = db.query_one_strong(
        "SELECT profile_id, superviseur_id, revision, politique_json, maj_le \
         FROM politiques_parentales WHERE profile_id = ?",
        &[&profile_id as &dyn ToSqlValue],
    )?
    else {
        return Ok(None);
    };
    let politique = serde_json::from_str(&row[3].as_string().unwrap_or_default())
        .map_err(|e| format!("politique_parentale illisible: {e}"))?;
    Ok(Some(PolitiqueEnregistree {
        profile_id: row[0].as_i64().unwrap_or(profile_id),
        superviseur_id: row[1].as_i64().unwrap_or(0),
        revision: row[2].as_i64().unwrap_or(0),
        politique,
        maj_le: row[4].as_string().unwrap_or_default(),
    }))
}

/// Pose la politique d'un profil. `revision_attendue` vaut 0 pour une
/// création, la révision lue sinon : toute autre valeur rend
/// [`Ecriture::Conflit`] sans rien écrire. Le superviseur d'une politique
/// existante ne change pas.
pub fn poser(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    superviseur_id: i64,
    revision_attendue: i64,
    politique: &PolitiqueParentale,
) -> Result<Ecriture, String> {
    let json = serde_json::to_string(politique).map_err(|e| e.to_string())?;
    let maintenant = chrono::Utc::now().to_rfc3339();
    let lignes = if revision_attendue == 0 {
        db.execute(
            "INSERT INTO politiques_parentales \
             (profile_id, superviseur_id, revision, politique_json, maj_le) \
             VALUES (?, ?, 1, ?, ?) ON CONFLICT (profile_id) DO NOTHING",
            &[
                &profile_id as &dyn ToSqlValue,
                &superviseur_id,
                &json,
                &maintenant,
            ],
        )?
    } else {
        db.execute(
            "UPDATE politiques_parentales \
             SET revision = revision + 1, politique_json = ?, maj_le = ? \
             WHERE profile_id = ? AND revision = ?",
            &[
                &json as &dyn ToSqlValue,
                &maintenant,
                &profile_id,
                &revision_attendue,
            ],
        )?
    };
    Ok(if lignes == 0 {
        Ecriture::Conflit
    } else {
        Ecriture::Faite(revision_attendue + 1)
    })
}

/// Retire la politique d'un profil, à la révision attendue seulement.
pub fn retirer(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    revision_attendue: i64,
) -> Result<Ecriture, String> {
    let lignes = db.execute(
        "DELETE FROM politiques_parentales WHERE profile_id = ? AND revision = ?",
        &[&profile_id as &dyn ToSqlValue, &revision_attendue],
    )?;
    Ok(if lignes == 0 {
        Ecriture::Conflit
    } else {
        Ecriture::Faite(0)
    })
}

/// Qui peut lire et écrire la politique d'un profil.
///
/// Jamais le profil lui-même, ni un profil lui-même supervisé. Une politique
/// existante appartient à son superviseur et aux administrateurs ; la
/// première se pose par un administrateur, qui en devient le superviseur.
pub fn peut_gerer(db: &Arc<dyn DbBackend>, appelant: i64, cible: i64) -> Result<bool, String> {
    if appelant == cible || lire(db, appelant)?.is_some() {
        return Ok(false);
    }
    let admin = db
        .query_one_strong(
            "SELECT is_admin FROM profiles WHERE id = ?",
            &[&appelant as &dyn ToSqlValue],
        )?
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .is_some_and(|n| n != 0);
    Ok(match lire(db, cible)? {
        Some(p) => admin || p.superviseur_id == appelant,
        None => admin,
    })
}

/// Minuscules, espaces repliés : « The  Doors » et « the doors » se valent.
pub fn normaliser(nom: &str) -> String {
    nom.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Ce que filtre une réponse, selon la route qui l'a produite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liste {
    /// Albums, pistes, recherche : les éléments d'un artiste bloqué.
    Bibliotheque,
    /// La liste des artistes : ses éléments SONT des artistes.
    Artistes,
    /// Radios : ce qui n'est pas dans la liste blanche.
    Radios,
    /// Podcasts : ce qui n'est pas dans la liste blanche.
    Podcasts,
}

/// La politique prête à filtrer : ensembles normalisés une fois par requête.
pub struct Filtre {
    ids_bloques: HashSet<i64>,
    noms_bloques: HashSet<String>,
    radios: Option<HashSet<String>>,
    podcasts: Option<HashSet<String>>,
}

const CLES_NOM_D_ARTISTE: &[&str] = &["artist_name", "artist", "album_artist", "album_artist_name"];
const CLES_ID_D_ARTISTE: &[&str] = &["artist_id", "album_artist_id"];
const CLES_DE_RADIO: &[&str] = &["id", "stream_url", "url", "radio_id", "uuid", "stationuuid"];
const CLES_DE_PODCAST: &[&str] = &["feed_url", "feedUrl", "url"];

fn texte(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Filtre {
    pub fn depuis(p: &PolitiqueParentale) -> Self {
        let ensemble = |l: &Option<Vec<String>>| {
            l.as_ref().map(|l| {
                l.iter()
                    .map(|s| s.trim().to_string())
                    .collect::<HashSet<_>>()
            })
        };
        Filtre {
            ids_bloques: p.artistes_bloques.iter().filter_map(|a| a.id).collect(),
            noms_bloques: p
                .artistes_bloques
                .iter()
                .filter_map(|a| a.nom.as_deref().map(normaliser))
                .filter(|n| !n.is_empty())
                .collect(),
            radios: ensemble(&p.radios_autorisees),
            podcasts: ensemble(&p.podcasts_autorises),
        }
    }

    /// Une entrée de la bibliothèque locale (pas d'un service) : son `id`
    /// est un identifiant de bibliothèque.
    fn est_locale(o: &serde_json::Map<String, Value>) -> bool {
        match o.get("source").and_then(Value::as_str) {
            None => true,
            Some(s) => s == "local" || s == "library",
        }
    }

    fn nom_bloque(&self, v: Option<&Value>) -> bool {
        v.and_then(Value::as_str)
            .is_some_and(|n| self.noms_bloques.contains(&normaliser(n)))
    }

    /// Vrai si l'objet est un élément à retirer de la liste où il paraît.
    fn retire(&self, o: &serde_json::Map<String, Value>, liste: Liste, artistes: bool) -> bool {
        match liste {
            Liste::Bibliotheque | Liste::Artistes if artistes => {
                (Self::est_locale(o)
                    && o.get("id")
                        .and_then(Value::as_i64)
                        .is_some_and(|id| self.ids_bloques.contains(&id)))
                    || self.nom_bloque(o.get("name"))
                    || self.nom_bloque(o.get("artist_name"))
            }
            Liste::Bibliotheque | Liste::Artistes => {
                CLES_ID_D_ARTISTE.iter().any(|k| {
                    Self::est_locale(o)
                        && o.get(*k)
                            .and_then(Value::as_i64)
                            .is_some_and(|id| self.ids_bloques.contains(&id))
                }) || CLES_NOM_D_ARTISTE
                    .iter()
                    .any(|k| self.nom_bloque(o.get(*k)))
            }
            Liste::Radios => self.hors_liste(&self.radios, o, CLES_DE_RADIO),
            Liste::Podcasts => self.hors_liste(&self.podcasts, o, CLES_DE_PODCAST),
        }
    }

    /// Un objet qui porte une clé d'identité et dont aucune valeur n'est
    /// autorisée. Un objet sans clé d'identité n'est pas un élément.
    fn hors_liste(
        &self,
        liste: &Option<HashSet<String>>,
        o: &serde_json::Map<String, Value>,
        cles: &[&str],
    ) -> bool {
        let Some(autorises) = liste else {
            return false;
        };
        let valeurs: Vec<String> = cles
            .iter()
            .filter_map(|k| o.get(*k).and_then(texte))
            .collect();
        !valeurs.is_empty() && !valeurs.iter().any(|v| autorises.contains(v))
    }

    /// Retire de `corps` les éléments bloqués, à toute profondeur, et rend
    /// leur nombre. Un `total` à la racine est diminué d'autant.
    pub fn filtrer(&self, corps: &mut Value, liste: Liste) -> usize {
        let retires = self.parcourir(corps, liste, liste == Liste::Artistes);
        if retires > 0
            && let Some(total) = corps.get_mut("total")
            && let Some(n) = total.as_u64()
        {
            *total = Value::from(n.saturating_sub(retires as u64));
        }
        retires
    }

    fn parcourir(&self, v: &mut Value, liste: Liste, artistes: bool) -> usize {
        match v {
            Value::Array(elements) => {
                let avant = elements.len();
                elements.retain(|e| match e {
                    Value::Object(o) => !self.retire(o, liste, artistes),
                    _ => true,
                });
                let mut retires = avant - elements.len();
                // Les éléments gardés ne sont pas fouillés : les flux d'une
                // radio autorisée ou les pistes d'un album gardé restent.
                if matches!(liste, Liste::Bibliotheque | Liste::Artistes) {
                    for e in elements.iter_mut() {
                        retires += self.parcourir_l_objet(e, liste);
                    }
                }
                retires
            }
            Value::Object(_) => self.parcourir_l_objet(v, liste),
            _ => 0,
        }
    }

    /// Les tableaux portés par un objet : « artists » contient des artistes,
    /// « items » ceux de la liste des artistes, les autres des éléments.
    fn parcourir_l_objet(&self, v: &mut Value, liste: Liste) -> usize {
        let Value::Object(o) = v else {
            return 0;
        };
        let mut retires = 0;
        for (cle, enfant) in o.iter_mut() {
            if enfant.is_array() {
                let artistes = cle == "artists" || (liste == Liste::Artistes && cle == "items");
                retires += self.parcourir(enfant, liste, artistes);
            } else if enfant.is_object() {
                retires += self.parcourir_l_objet(enfant, liste);
            }
        }
        retires
    }
}

#[cfg(test)]
#[path = "politique_parentale_tests_7856.rs"]
mod tests;
