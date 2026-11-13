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

/// #8088 — le code stable du refus d'un contenu explicite, et la sentinelle
/// que rend la lecture refusée (`orchestrator::play`).
pub const CODE_REFUS_EXPLICITE: &str = "parental_explicit_refused";

/// #7913, #8611 — le code stable du refus d'un contenu dont l'artiste est
/// bloqué : fiche ouverte directement (403) et lecture refusée (sentinelle de
/// `orchestrator::play`).
pub const CODE_REFUS_CONTENU_BLOQUE: &str = "parental_content_blocked";

/// #8611 — le code stable d'une lecture refusée parce que la politique
/// ENREGISTRÉE du profil ne se relit pas (ligne corrompue). Ce refus n'est
/// PAS propre à la piste : il arrête la file au lieu de la faire sauter
/// piste après piste (`source_url::arret_de_la_file`).
pub const CODE_POLITIQUE_ILLISIBLE: &str = "parental_policy_unreadable";

/// Tête de l'erreur de [`lire`] quand la ligne existe mais que son JSON ne
/// se relit pas.
const TETE_POLITIQUE_CORROMPUE: &str = "politique_parentale illisible";

/// #8611 — l'erreur de [`lire`] dit-elle que la politique est enregistrée
/// mais corrompue (et non que la base n'a pas répondu) ?
pub fn politique_corrompue(erreur: &str) -> bool {
    erreur.starts_with(TETE_POLITIQUE_CORROMPUE)
}

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
    /// #8088 — masquer et refuser les contenus explicites : titres et albums
    /// que le service (Qobuz `parental_warning`, TIDAL et Spotify
    /// `explicit`) ou la balise du fichier (`ITUNESADVISORY` = 1, MP4
    /// `rtng`) marque explicites. Une piste sans marquage n'est pas
    /// explicite. `false` (défaut) : rien ne change.
    #[serde(default)]
    pub masquer_explicite: bool,
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
        .map_err(|e| format!("{TETE_POLITIQUE_CORROMPUE}: {e}"))?;
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
///
/// Ne lit que le superviseur, jamais le contenu : une politique illisible
/// (contenu corrompu) reste retirable ou remplaçable par qui la gère.
pub fn peut_gerer(db: &Arc<dyn DbBackend>, appelant: i64, cible: i64) -> Result<bool, String> {
    if appelant == cible || superviseur(db, appelant)?.is_some() {
        return Ok(false);
    }
    let admin = est_administrateur(db, appelant)?;
    Ok(match superviseur(db, cible)? {
        Some(s) => admin || s == appelant,
        None => admin,
    })
}

/// Le superviseur de la politique d'un profil, ou `None` : profil non
/// supervisé. Ne lit pas le contenu de la politique.
pub fn superviseur(db: &Arc<dyn DbBackend>, profile_id: i64) -> Result<Option<i64>, String> {
    Ok(db
        .query_one_strong(
            "SELECT superviseur_id FROM politiques_parentales WHERE profile_id = ?",
            &[&profile_id as &dyn ToSqlValue],
        )?
        .map(|row| row.first().and_then(|v| v.as_i64()).unwrap_or(0)))
}

/// Le profil a le rôle administrateur. Un tel profil ne se place pas sous
/// supervision : [`poser`] est refusé par les routes pour lui.
pub fn est_administrateur(db: &Arc<dyn DbBackend>, profile_id: i64) -> Result<bool, String> {
    Ok(db
        .query_one_strong(
            "SELECT is_admin FROM profiles WHERE id = ?",
            &[&profile_id as &dyn ToSqlValue],
        )?
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .is_some_and(|n| n != 0))
}

/// L'erreur dit que la table `politiques_parentales` n'existe pas (création
/// échouée au démarrage) : aucune politique ne peut alors être posée, et les
/// listes passent telles quelles. Toute autre erreur reste une erreur.
pub fn table_absente(erreur: &str) -> bool {
    let e = erreur.to_ascii_lowercase();
    e.contains("politiques_parentales")
        && (e.contains("no such table") || e.contains("does not exist"))
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
    /// #8611 — résultats et favoris des services : les éléments d'un artiste
    /// bloqué, reconnu par son NOM seulement. L'identifiant d'un service ne
    /// désigne pas un artiste de la bibliothèque : le comparer aux
    /// identifiants bloqués retirerait des éléments au hasard.
    Services,
}

/// #7913 — la fiche ouverte directement, selon la route qui la sert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fiche {
    Album,
    Artiste,
    Piste,
    Radio,
    Podcast,
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
const CLES_DE_RADIO: &[&str] = &[
    "id",
    "stream_url",
    "url",
    "radio_id",
    "uuid",
    "stationuuid",
    "external_id",
    "my_radio_id",
];
const CLES_DE_PODCAST: &[&str] = &["feed_url", "feedUrl", "url"];
/// Les tableaux d'une réponse de radios qui portent des stations : `items`
/// (« Mes radios », recherche, annuaire) et `annuaire` (recherche). Les autres
/// (pays, genres, flux d'une station) ne sont pas fouillés.
const TABLEAUX_DE_RADIOS: &[&str] = &["items", "annuaire"];
/// Les tableaux d'une réponse de podcasts qui portent des podcasts : `items`
/// (recherche, top, catalogue), `curated` et `top` (découverte).
const TABLEAUX_DE_PODCASTS: &[&str] = &["items", "curated", "top"];

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
    pub(crate) fn est_locale(o: &serde_json::Map<String, Value>) -> bool {
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
            Liste::Services if artistes => {
                self.nom_bloque(o.get("name")) || self.nom_bloque(o.get("artist_name"))
            }
            Liste::Services => {
                CLES_NOM_D_ARTISTE
                    .iter()
                    .any(|k| self.nom_bloque(o.get(*k)))
                    // Un favori de service de type artiste porte son nom
                    // dans `title`.
                    || (o.get("item_type").and_then(Value::as_str) == Some("artist")
                        && self.nom_bloque(o.get("title")))
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

    /// #7913 — la politique bloque-t-elle des artistes ? Sans artiste bloqué,
    /// les listes de la bibliothèque passent telles quelles.
    pub fn bloque_des_artistes(&self) -> bool {
        !self.ids_bloques.is_empty() || !self.noms_bloques.is_empty()
    }

    /// #8611 — l'artiste (identifiant de bibliothèque, nom) est-il bloqué ?
    pub fn artiste_bloque(&self, id: Option<i64>, nom: Option<&str>) -> bool {
        id.is_some_and(|id| self.ids_bloques.contains(&id))
            || nom.is_some_and(|n| self.noms_bloques.contains(&normaliser(n)))
    }

    /// #7913 — la FICHE ouverte directement (album, artiste, piste, radio,
    /// podcast) est-elle bloquée pour ce profil ? Le filtre des listes ne
    /// retire que des éléments de tableaux : la racine d'une fiche lui
    /// échappait.
    pub fn fiche_bloquee(&self, corps: &Value, fiche: Fiche) -> bool {
        let Value::Object(o) = corps else {
            return false;
        };
        match fiche {
            Fiche::Artiste => self.retire(o, Liste::Artistes, true),
            Fiche::Album | Fiche::Piste => self.retire(o, Liste::Bibliotheque, false),
            Fiche::Radio => self.retire(o, Liste::Radios, false),
            Fiche::Podcast => self.retire(o, Liste::Podcasts, false),
        }
    }

    /// #8611 — la fiche d'un album, d'une piste (`artiste` faux) ou d'un
    /// artiste (`artiste` vrai) d'un SERVICE est-elle d'un artiste bloqué ?
    /// Par le nom seul.
    pub fn fiche_bloquee_de_service(&self, corps: &Value, artiste: bool) -> bool {
        match corps {
            Value::Object(o) => self.retire(o, Liste::Services, artiste),
            _ => false,
        }
    }

    /// #7913 — combien d'éléments du tableau `items` de la racine (ou de la
    /// racine elle-même, si c'est un tableau) sont bloqués. Sert au total
    /// exact d'une liste paginée : seuls les éléments de premier rang
    /// comptent, pas les pistes retirées DANS un album gardé.
    pub fn bloques_au_premier_rang(&self, corps: &Value, liste: Liste) -> usize {
        let elements = match corps {
            Value::Array(a) => a,
            Value::Object(o) => match o.get("items") {
                Some(Value::Array(a)) => a,
                _ => return 0,
            },
            _ => return 0,
        };
        let artistes = liste == Liste::Artistes;
        elements
            .iter()
            .filter(|e| match e {
                Value::Object(o) => self.retire(o, liste, artistes),
                _ => false,
            })
            .count()
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
                if matches!(
                    liste,
                    Liste::Bibliotheque | Liste::Artistes | Liste::Services
                ) {
                    for e in elements.iter_mut() {
                        retires += self.parcourir_l_objet(e, liste);
                    }
                }
                retires
            }
            // Radios et podcasts : seuls les tableaux connus de la racine.
            Value::Object(o) if matches!(liste, Liste::Radios | Liste::Podcasts) => {
                let cles = if liste == Liste::Radios {
                    TABLEAUX_DE_RADIOS
                } else {
                    TABLEAUX_DE_PODCASTS
                };
                o.iter_mut()
                    .filter(|(cle, enfant)| cles.contains(&cle.as_str()) && enfant.is_array())
                    .map(|(_, enfant)| self.parcourir(enfant, liste, false))
                    .sum()
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

/// #8088 — l'objet porte-t-il le marquage explicite d'un service ou d'une
/// balise ? `explicit` (pistes et albums des services, pistes locales) ou
/// `parental_warning` (Qobuz brut), vrai ou 1. Une absence n'est pas un
/// marquage.
pub fn est_marque_explicite(o: &serde_json::Map<String, Value>) -> bool {
    ["explicit", "parental_warning"]
        .iter()
        .any(|k| match o.get(*k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_i64() == Some(1),
            Some(Value::String(s)) => {
                matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true")
            }
            _ => false,
        })
}

/// #8088 — les pistes locales marquées explicites par leur balise, et les
/// albums locaux qui en contiennent au moins une.
#[derive(Debug, Clone, Default)]
pub struct MarquesLocales {
    pub pistes: HashSet<i64>,
    pub albums: HashSet<i64>,
}

/// #8088 — lit les marques explicites de la bibliothèque locale
/// (`tracks.explicit = 1`, index partiel `idx_tracks_explicit`).
pub fn marques_explicites_locales(db: &Arc<dyn DbBackend>) -> Result<MarquesLocales, String> {
    let lignes = db.query_many("SELECT id, album_id FROM tracks WHERE explicit = 1", &[])?;
    let mut m = MarquesLocales::default();
    for l in lignes {
        if let Some(id) = l.first().and_then(|v| v.as_i64()) {
            m.pistes.insert(id);
        }
        if let Some(album) = l.get(1).and_then(|v| v.as_i64()) {
            m.albums.insert(album);
        }
    }
    Ok(m)
}

/// #8088 — la piste locale `track_id` est-elle marquée explicite ?
pub fn piste_locale_explicite(db: &Arc<dyn DbBackend>, track_id: i64) -> Result<bool, String> {
    Ok(db
        .query_one_strong(
            "SELECT explicit FROM tracks WHERE id = ?",
            &[&track_id as &dyn ToSqlValue],
        )?
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        == Some(1))
}

impl MarquesLocales {
    /// L'objet est-il un contenu explicite : marqué par son service ou sa
    /// balise, ou piste ou album local que la base marque ? `cle` : la clé du
    /// tableau qui le porte (« albums » désigne des albums).
    pub fn objet_explicite(&self, o: &serde_json::Map<String, Value>, cle: Option<&str>) -> bool {
        if est_marque_explicite(o) {
            return true;
        }
        if !Filtre::est_locale(o) {
            return false;
        }
        if o.get("track_id")
            .and_then(Value::as_i64)
            .is_some_and(|t| self.pistes.contains(&t))
        {
            return true;
        }
        let Some(id) = o.get("id").and_then(Value::as_i64) else {
            return false;
        };
        let piste = ["album_id", "duration_ms", "file_path"]
            .iter()
            .any(|k| o.contains_key(*k));
        if piste {
            self.pistes.contains(&id)
        } else if cle == Some("albums") || o.contains_key("track_count") {
            self.albums.contains(&id)
        } else {
            false
        }
    }

    /// Retire de `corps`, à toute profondeur, les contenus explicites, et
    /// rend leur nombre. Un `total` à la racine est diminué d'autant. La
    /// racine elle-même n'est jamais retirée : voir [`Self::racine_explicite`].
    pub fn filtrer(&self, corps: &mut Value) -> usize {
        let retires = self.parcourir(corps, None);
        if retires > 0
            && let Some(total) = corps.get_mut("total")
            && let Some(n) = total.as_u64()
        {
            *total = Value::from(n.saturating_sub(retires as u64));
        }
        retires
    }

    /// La réponse entière est la fiche d'un contenu explicite (page d'un
    /// album, d'une piste) : elle est refusée, pas vidée.
    pub fn racine_explicite(&self, corps: &Value) -> bool {
        corps
            .as_object()
            .is_some_and(|o| self.objet_explicite(o, None))
    }

    fn parcourir(&self, v: &mut Value, cle: Option<&str>) -> usize {
        match v {
            Value::Array(elements) => {
                let avant = elements.len();
                elements.retain(|e| match e {
                    Value::Object(o) => !self.objet_explicite(o, cle),
                    _ => true,
                });
                let mut retires = avant - elements.len();
                for e in elements.iter_mut() {
                    retires += self.parcourir(e, None);
                }
                retires
            }
            Value::Object(o) => o
                .iter_mut()
                .filter(|(_, e)| e.is_array() || e.is_object())
                .map(|(k, e)| self.parcourir(e, Some(k.as_str())))
                .sum(),
            _ => 0,
        }
    }
}

/// #8611 — les artistes d'une piste locale, tels que la bibliothèque les
/// range : l'artiste de la piste, l'artiste de l'album, la balise
/// `ALBUMARTIST`. Le FROM est celui de `TrackRepo` (jointures par clé
/// primaire).
const SQL_ARTISTES_DES_PISTES: &str = "SELECT t.id, t.artist_id, ar.name, al.artist_id, aal.name, t.album_artist \
     FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
     LEFT JOIN artists ar ON t.artist_id = ar.id \
     LEFT JOIN artists aal ON al.artist_id = aal.id WHERE t.id IN ";
const SQL_ARTISTES_DES_ALBUMS: &str = "SELECT al.id, al.artist_id, aal.name \
     FROM albums al LEFT JOIN artists aal ON al.artist_id = aal.id WHERE al.id IN ";
const SQL_NOMS_DES_ARTISTES: &str = "SELECT id, id, name FROM artists WHERE id IN ";

/// Les identifiants d'une requête `IN (…)`, écrits dans le SQL : ce sont des
/// entiers, rien ne s'y injecte, et la même chaîne vaut pour les deux moteurs.
fn liste_d_entiers(ids: &[i64]) -> String {
    let mut s = String::from("(");
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&id.to_string());
    }
    s.push(')');
    s
}

/// Taille d'un lot `IN (…)`.
const LOT_IN: usize = 500;

/// #8611 — parmi `ids` (pistes, albums ou artistes de la bibliothèque, selon
/// `sql`), ceux dont un artiste est bloqué. Lecture ciblée par clé primaire,
/// par lots : la bibliothèque n'est pas relue.
fn bloques_parmi(
    db: &Arc<dyn DbBackend>,
    filtre: &Filtre,
    sql: &str,
    ids: &[i64],
) -> Result<HashSet<i64>, String> {
    let mut bloques = HashSet::new();
    if !filtre.bloque_des_artistes() {
        return Ok(bloques);
    }
    for lot in ids.chunks(LOT_IN) {
        for l in db.query_many(&format!("{sql}{}", liste_d_entiers(lot)), &[])? {
            let Some(id) = l.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let int = |i: usize| l.get(i).and_then(|v| v.as_i64());
            let txt = |i: usize| l.get(i).and_then(|v| v.as_string());
            // Les colonnes vont par paires (identifiant, nom), puis une
            // éventuelle balise de nom seul.
            let mut bloque = false;
            let mut i = 1;
            while i < l.len() {
                if i + 1 < l.len() {
                    bloque |= filtre.artiste_bloque(int(i), txt(i + 1).as_deref());
                    i += 2;
                } else {
                    bloque |= filtre.artiste_bloque(None, txt(i).as_deref());
                    i += 1;
                }
            }
            if bloque {
                bloques.insert(id);
            }
        }
    }
    Ok(bloques)
}

/// #8611 — la piste locale `track_id` est-elle d'un artiste bloqué (artiste
/// de la piste, de l'album, ou balise `ALBUMARTIST`) ?
pub fn piste_locale_bloquee(
    db: &Arc<dyn DbBackend>,
    filtre: &Filtre,
    track_id: i64,
) -> Result<bool, String> {
    Ok(bloques_parmi(db, filtre, SQL_ARTISTES_DES_PISTES, &[track_id])?.contains(&track_id))
}

/// #8611 — parmi des favoris de la bibliothèque (`item_type`, `item_id`),
/// ceux qui désignent un artiste bloqué, un de ses albums ou une de ses
/// pistes. Trois lectures ciblées au plus, par clé primaire.
pub fn favoris_bloques(
    db: &Arc<dyn DbBackend>,
    filtre: &Filtre,
    favoris: &[(String, i64)],
) -> Result<HashSet<(String, i64)>, String> {
    let mut sortie = HashSet::new();
    if !filtre.bloque_des_artistes() {
        return Ok(sortie);
    }
    for (genre, sql) in [
        ("artist", SQL_NOMS_DES_ARTISTES),
        ("album", SQL_ARTISTES_DES_ALBUMS),
        ("track", SQL_ARTISTES_DES_PISTES),
    ] {
        let ids: Vec<i64> = favoris
            .iter()
            .filter(|(t, _)| t == genre)
            .map(|(_, id)| *id)
            .collect();
        if ids.is_empty() {
            continue;
        }
        for id in bloques_parmi(db, filtre, sql, &ids)? {
            sortie.insert((genre.to_string(), id));
        }
    }
    Ok(sortie)
}

#[cfg(test)]
#[path = "politique_parentale_tests_8611.rs"]
mod tests_8611;

#[cfg(test)]
#[path = "politique_parentale_tests_7856.rs"]
mod tests;

#[cfg(test)]
#[path = "politique_parentale_tests_8088.rs"]
mod tests_8088;
