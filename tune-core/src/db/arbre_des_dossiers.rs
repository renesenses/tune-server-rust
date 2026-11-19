//! #9116 (fil 2935, Thierry C.) — l'ARBRE DES DOSSIERS de la bibliothèque,
//! calculé une fois à partir de l'index des chemins et gardé en mémoire tant
//! que l'ensemble des chemins de `tracks` n'a pas changé.
//!
//! # Pourquoi
//!
//! La vue Répertoires (`GET /library/browse/dir`) relisait, à CHAQUE dossier
//! ouvert, toutes les pistes situées sous lui : une fois pour compter ses
//! sous-dossiers ([`super::track_repo::compter_pistes_par_sous_dossier`]), une
//! seconde pour trouver ses pistes directes
//! ([`super::track_repo::sql_pistes_du_dossier`], plage récursive filtrée
//! ensuite). Sur 544 147 pistes, descendre « music2 › 1 Classic › Beethoven ›
//! Piano › Bellucci » relisait ainsi cinq fois le gros de la bibliothèque —
//! « la descente coûte plus de 2 minutes ». Le cache de #9122, gardé par la
//! révision de la bibliothèque, ne servait presque jamais : cette révision
//! bouge à toute écriture dans quatorze tables (`track_metadata`, `settings`…),
//! donc sans cesse quand une tâche de fond tourne.
//!
//! Ici : UN parcours de l'index des chemins par état de la bibliothèque, qui
//! rend pour chaque dossier ses comptes (récursifs) et les identifiants de ses
//! pistes directes. Descendre d'un niveau ne lit plus la base que pour les
//! pistes directes du dossier ouvert (par identifiant), et pas du tout pour un
//! dossier qui n'en a pas.
//!
//! # Exactitude
//!
//! L'arbre est gardé sous la [révision des chemins](sql_sqlite) : un compteur
//! tenu par des déclencheurs sur `tracks` qui ne bouge qu'à un ajout, une
//! suppression, ou un changement de `id`, `file_path` ou `cue_media_path`.
//! Rien d'autre n'entre dans l'arbre, donc un arbre gardé sous la révision `r`
//! est exactement celui que la base rendrait. Sans ce compteur (déclencheurs
//! absents), la révision de la bibliothèque ([`super::library_revision`]) en
//! tient lieu (plus souvent invalidée, jamais fausse) ; sans l'une ni l'autre,
//! [`lire`] rend `None` et la route retombe sur ses requêtes SQL.
//!
//! Les comptes sont ceux des requêtes qu'ils remplacent, moteur par moteur :
//!
//! * un chemin compte sous `D` s'il commence par `D<séparateur>` (le motif
//!   `LIKE` de [`super::track_repo::folder_like_pattern`]) ;
//! * sur SQLite, `LIKE` ignore la casse ASCII : les segments y sont repliés en
//!   minuscules ASCII (comme [`super::track_repo::ComptesParSousDossier`]) ;
//! * le compte d'une RACINE ne voit que `file_path` (une tranche CUE n'y entre
//!   pas, [`super::track_repo::compter_pistes_sous_dossier`]), celui d'un
//!   sous-dossier voit `COALESCE(file_path, cue_media_path)` (#4625) : l'arbre
//!   tient les deux.
//!
//! Une seule différence, voulue : les chemins sont repliés en NFC à la
//! construction. Le scanner écrit déjà du NFC ; une ligne ancienne restée en
//! NFD, que le `LIKE` (motif NFC) manquait, est désormais comptée.
//!
//! # Mémoire
//!
//! Un nœud par DOSSIER (pas par piste) et un identifiant (8 octets) par piste.
//! [`ArbreDesDossiers::octets`] l'estime ; le banc (`banc_repertoires_9116`)
//! le mesure sur 544 147 pistes et le journal l'écrit à chaque construction.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::{IsNormalized, is_nfc_quick};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::Engine;

/// Un dossier : ses sous-dossiers, ses comptes récursifs, ses pistes directes.
#[derive(Debug, Default)]
struct Noeud {
    enfants: HashMap<Box<str>, u32>,
    /// Pistes sous ce dossier, récursivement, par `COALESCE(file_path,
    /// cue_media_path)`.
    pistes: u32,
    /// Les mêmes, par `file_path` seul (le compte d'une racine).
    fichiers: u32,
    /// Identifiants des pistes posées directement dans ce dossier.
    directes: Vec<i64>,
}

/// L'arbre des dossiers d'une bibliothèque. Voir la doc du module.
#[derive(Debug)]
pub struct ArbreDesDossiers {
    /// `noeuds[0]` est le sommet virtuel, au-dessus du premier segment.
    noeuds: Vec<Noeud>,
    separateur: char,
    replier_la_casse: bool,
    chemins: usize,
}

/// Un dossier lu dans l'arbre. Un dossier absent de l'arbre (aucune piste
/// dessous) se lit comme un dossier vide : `0` partout, aucune piste directe.
pub struct VueDossier<'a> {
    arbre: &'a ArbreDesDossiers,
    noeud: Option<&'a Noeud>,
}

impl VueDossier<'_> {
    /// Pistes sous le sous-dossier direct `nom`, récursivement — la valeur
    /// de [`super::track_repo::ComptesParSousDossier::get`]. `nom` vient du
    /// disque (`read_dir`), donc peut-être en NFD : il est replié.
    pub fn pistes_du_sous_dossier(&self, nom: &str) -> i64 {
        let Some(n) = self.noeud else { return 0 };
        let nfc: String = nom.nfc().collect();
        let clef = self.arbre.clef(&nfc);
        n.enfants
            .get(clef.as_ref())
            .map_or(0, |&i| i64::from(self.arbre.noeuds[i as usize].pistes))
    }

    /// Identifiants des pistes posées directement dans le dossier (à filtrer
    /// encore par la route : sur SQLite, la casse y est repliée).
    pub fn pistes_directes(&self) -> &[i64] {
        self.noeud.map_or(&[], |n| n.directes.as_slice())
    }

    /// Nombre de sous-dossiers peuplés.
    pub fn nb_sous_dossiers(&self) -> usize {
        self.noeud.map_or(0, |n| n.enfants.len())
    }
}

impl ArbreDesDossiers {
    /// Un arbre vide. `separateur` est celui de `tracks.file_path`
    /// (`std::path::MAIN_SEPARATOR` en service) ; `replier_la_casse` vaut
    /// `true` sur SQLite (voir la doc du module).
    pub fn nouveau(separateur: char, replier_la_casse: bool) -> Self {
        Self {
            noeuds: vec![Noeud::default()],
            separateur,
            replier_la_casse,
            chemins: 0,
        }
    }

    fn clef<'s>(&self, segment: &'s str) -> Cow<'s, str> {
        if self.replier_la_casse && segment.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(segment.to_ascii_lowercase())
        } else {
            Cow::Borrowed(segment)
        }
    }

    /// Ajoute la piste `id` de chemin `chemin`. `fichier` : le chemin vient
    /// de `file_path` (et non de `cue_media_path`). Un chemin sans séparateur
    /// ne commence par aucun `<dossier><séparateur>` : il n'est compté nulle
    /// part, comme avec le `LIKE`.
    pub fn ajouter(&mut self, id: i64, chemin: &str, fichier: bool) {
        let chemin: Cow<str> = if is_nfc_quick(chemin.chars()) == IsNormalized::Yes {
            Cow::Borrowed(chemin)
        } else {
            Cow::Owned(chemin.nfc().collect())
        };
        let Some(fin) = chemin.rfind(self.separateur) else {
            return;
        };
        self.chemins += 1;
        let mut n = 0usize;
        for segment in chemin[..fin].split(self.separateur) {
            let clef = self.clef(segment);
            n = match self.noeuds[n].enfants.get(clef.as_ref()) {
                Some(&i) => i as usize,
                None => {
                    let i = self.noeuds.len();
                    self.noeuds.push(Noeud::default());
                    self.noeuds[n]
                        .enfants
                        .insert(clef.into_owned().into_boxed_str(), i as u32);
                    i
                }
            };
            let noeud = &mut self.noeuds[n];
            noeud.pistes += 1;
            if fichier {
                noeud.fichiers += 1;
            }
        }
        self.noeuds[n].directes.push(id);
    }

    /// Le nœud du dossier `dossier` (séparateurs finaux rognés, NFC, casse
    /// repliée sur SQLite — le repli de `folder_like_pattern`).
    fn noeud(&self, dossier: &str) -> Option<&Noeud> {
        let base: String = dossier.trim_end_matches(['/', '\\']).nfc().collect();
        let mut n = 0usize;
        for segment in base.split(self.separateur) {
            n = *self.noeuds[n].enfants.get(self.clef(segment).as_ref())? as usize;
        }
        Some(&self.noeuds[n])
    }

    /// Le dossier `dossier`, pour la vue Répertoires.
    pub fn dossier(&self, dossier: &str) -> VueDossier<'_> {
        VueDossier {
            arbre: self,
            noeud: self.noeud(dossier),
        }
    }

    /// Pistes sous `dossier` par `file_path` seul : le compte d'une racine
    /// ([`super::track_repo::compter_pistes_sous_dossier`]).
    pub fn fichiers_sous(&self, dossier: &str) -> i64 {
        self.noeud(dossier).map_or(0, |n| i64::from(n.fichiers))
    }

    /// Pistes sous `dossier` par `COALESCE(file_path, cue_media_path)`.
    pub fn pistes_sous(&self, dossier: &str) -> i64 {
        self.noeud(dossier).map_or(0, |n| i64::from(n.pistes))
    }

    /// Nombre de dossiers (nœuds) de l'arbre, sommet virtuel exclu.
    pub fn nb_dossiers(&self) -> usize {
        self.noeuds.len() - 1
    }

    /// Nombre de chemins rangés.
    pub fn nb_chemins(&self) -> usize {
        self.chemins
    }

    /// Estimation de la mémoire tenue : nœuds, tables d'enfants (clés
    /// comprises, plus un octet de contrôle par case), identifiants.
    pub fn octets(&self) -> usize {
        use std::mem::size_of;
        let case = size_of::<(Box<str>, u32)>() + 1;
        self.noeuds.capacity() * size_of::<Noeud>()
            + self
                .noeuds
                .iter()
                .map(|n| {
                    n.enfants.capacity() * case
                        + n.enfants.keys().map(|k| k.len()).sum::<usize>()
                        + n.directes.capacity() * size_of::<i64>()
                })
                .sum::<usize>()
    }

    fn ajuster(&mut self) {
        self.noeuds.shrink_to_fit();
        for n in &mut self.noeuds {
            n.enfants.shrink_to_fit();
            n.directes.shrink_to_fit();
        }
    }
}

/// Taille d'une page de lecture : la connexion est rendue au pool entre deux
/// pages, et le transitoire reste borné (une page, pas 544 147 lignes).
const PAGE: i64 = 50_000;

/// Construit l'arbre de `db` : `file_path` page par page dans l'ordre de son
/// index (`file_path` est `UNIQUE`, la reprise par `> dernier` est exacte, et
/// la lecture ne touche que l'index), puis les tranches CUE (`file_path`
/// NULL) par identifiant.
pub fn construire(db: &dyn DbBackend) -> Result<ArbreDesDossiers, String> {
    let sqlite = db.engine() == Engine::Sqlite;
    let mut arbre = ArbreDesDossiers::nouveau(std::path::MAIN_SEPARATOR, sqlite);
    let (p1, p2) = if sqlite { ("?1", "?2") } else { ("$1", "$2") };
    let sql_fichiers = format!(
        "SELECT id, file_path FROM tracks WHERE file_path > {p1} ORDER BY file_path LIMIT {p2}"
    );
    let mut dernier = String::new();
    loop {
        let lignes = db.query_many(&sql_fichiers, &[&dernier as &dyn ToSqlValue, &PAGE])?;
        let n = lignes.len();
        for l in lignes {
            let (Some(id), Some(chemin)) = (
                l.first().and_then(|v| v.as_i64()),
                l.get(1).and_then(|v| v.as_string()),
            ) else {
                continue;
            };
            arbre.ajouter(id, &chemin, true);
            dernier = chemin;
        }
        if (n as i64) < PAGE {
            break;
        }
    }
    let sql_cue = format!(
        "SELECT id, cue_media_path FROM tracks WHERE file_path IS NULL \
         AND cue_media_path IS NOT NULL AND id > {p1} ORDER BY id LIMIT {p2}"
    );
    let mut dernier_id: i64 = i64::MIN;
    loop {
        let lignes = db.query_many(&sql_cue, &[&dernier_id as &dyn ToSqlValue, &PAGE])?;
        let n = lignes.len();
        for l in lignes {
            let Some(id) = l.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            if let Some(chemin) = l.get(1).and_then(|v| v.as_string()) {
                arbre.ajouter(id, &chemin, false);
            }
            dernier_id = id;
        }
        if (n as i64) < PAGE {
            break;
        }
    }
    arbre.ajuster();
    Ok(arbre)
}

/// La clé d'un arbre : la révision des chemins, à défaut celle de la
/// bibliothèque (le premier champ dit laquelle).
type Clef = (u8, i64, i64);

fn clef(db: &dyn DbBackend) -> Option<Clef> {
    if let Some((i, r)) = lire_revision_des_chemins(db) {
        return Some((0, i, r));
    }
    super::library_revision::lire(db).map(|(i, r)| (1, i, r))
}

/// Un arbre par BASE (l'instance de la révision) : un serveur n'en a qu'une ;
/// les tests, qui ouvrent plusieurs bases dans un même processus, ne
/// s'évincent pas l'un l'autre.
struct Cache {
    courants: Mutex<HashMap<i64, (Clef, Arc<ArbreDesDossiers>)>>,
    /// Une construction à la fois : cinq clics pendant la première ne lancent
    /// pas cinq parcours.
    construction: Mutex<()>,
}

/// Au-delà, la table est vidée (voir [`Cache`]).
const BASES_EN_CACHE_MAX: usize = 8;

impl Cache {
    fn servi(&self, clef: Clef) -> Option<Arc<ArbreDesDossiers>> {
        let courants = self.courants.lock().ok()?;
        courants
            .get(&clef.1)
            .filter(|(c, _)| *c == clef)
            .map(|(_, a)| a.clone())
    }

    fn garder(&self, clef: Clef, arbre: Arc<ArbreDesDossiers>) {
        if let Ok(mut courants) = self.courants.lock() {
            if courants.len() >= BASES_EN_CACHE_MAX && !courants.contains_key(&clef.1) {
                courants.clear();
            }
            // L'arbre d'une révision passée est remplacé : un seul par base.
            courants.insert(clef.1, (clef, arbre));
        }
    }
}

static CACHE: LazyLock<Cache> = LazyLock::new(|| Cache {
    courants: Mutex::new(HashMap::new()),
    construction: Mutex::new(()),
});

/// L'arbre des dossiers de `db`, construit au besoin, ou `None` si aucune
/// révision n'est lisible (rien ne peut alors le garder exact) ou si la
/// construction échoue — l'appelant retombe sur ses requêtes SQL.
///
/// La révision est lue AVANT la construction : une écriture validée pendant
/// celle-ci change la révision suivante, et cet arbre n'est plus servi.
pub fn lire(db: &dyn DbBackend) -> Option<Arc<ArbreDesDossiers>> {
    let clef = clef(db)?;
    if let Some(a) = CACHE.servi(clef) {
        return Some(a);
    }
    let _une_a_la_fois = CACHE.construction.lock().ok()?;
    if let Some(a) = CACHE.servi(clef) {
        return Some(a);
    }
    let debut = std::time::Instant::now();
    let arbre = match construire(db) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            tracing::warn!(error = %e, "arbre_des_dossiers_echec");
            return None;
        }
    };
    tracing::info!(
        chemins = arbre.nb_chemins(),
        dossiers = arbre.nb_dossiers(),
        octets = arbre.octets(),
        ms = debut.elapsed().as_millis() as u64,
        revision_des_chemins = clef.0 == 0,
        "arbre_des_dossiers_construit"
    );
    CACHE.garder(clef, arbre.clone());
    Some(arbre)
}

// ---------------------------------------------------------------------------
// La révision des chemins
// ---------------------------------------------------------------------------

/// La table, ses déclencheurs et sa ligne, pour SQLite, en UNE transaction
/// (comme [`super::library_revision::sql_sqlite`]) : un déclencheur qui
/// manque laisse tout absent, et [`lire`] retombe sur la révision de la
/// bibliothèque. Idempotent ; la ligne n'est créée qu'une fois.
///
/// La mise à jour ne compte que si `id`, `file_path` ou `cue_media_path`
/// changent VRAIMENT (`IS NOT`) : les passes de fond qui écrivent d'autres
/// colonnes de `tracks` (ReplayGain, empreintes…) ne l'invalident pas.
pub fn sql_sqlite() -> String {
    let bump = "UPDATE library_paths_revision SET revision = revision + 1 WHERE id = 1;";
    format!(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS library_paths_revision (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             revision INTEGER NOT NULL,
             instance INTEGER NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS library_paths_revision_ins AFTER INSERT ON tracks
         BEGIN {bump} END;
         CREATE TRIGGER IF NOT EXISTS library_paths_revision_del AFTER DELETE ON tracks
         BEGIN {bump} END;
         CREATE TRIGGER IF NOT EXISTS library_paths_revision_upd
         AFTER UPDATE OF id, file_path, cue_media_path ON tracks
         WHEN OLD.id IS NOT NEW.id OR OLD.file_path IS NOT NEW.file_path
              OR OLD.cue_media_path IS NOT NEW.cue_media_path
         BEGIN {bump} END;
         INSERT OR IGNORE INTO library_paths_revision (id, revision, instance)
         VALUES (1, 0, abs(random()));
         COMMIT;"
    )
}

/// La même chose pour PostgreSQL, en un bloc `DO` : par instruction pour
/// l'ajout et la suppression, par ligne (avec `WHEN`) pour la mise à jour.
pub fn sql_postgres() -> String {
    "DO $chemins$ BEGIN
     CREATE TABLE IF NOT EXISTS library_paths_revision (
         id BIGINT PRIMARY KEY CHECK (id = 1),
         revision BIGINT NOT NULL,
         instance BIGINT NOT NULL
     );
     CREATE OR REPLACE FUNCTION library_paths_revision_bump() RETURNS trigger AS $f$
     BEGIN
         UPDATE library_paths_revision SET revision = revision + 1 WHERE id = 1;
         RETURN NULL;
     END $f$ LANGUAGE plpgsql;
     IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'library_paths_revision_insdel'
                    AND tgrelid = 'tracks'::regclass) THEN
         CREATE TRIGGER library_paths_revision_insdel AFTER INSERT OR DELETE ON tracks
         FOR EACH STATEMENT EXECUTE FUNCTION library_paths_revision_bump();
     END IF;
     IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'library_paths_revision_upd'
                    AND tgrelid = 'tracks'::regclass) THEN
         CREATE TRIGGER library_paths_revision_upd
         AFTER UPDATE OF id, file_path, cue_media_path ON tracks FOR EACH ROW
         WHEN (OLD.id IS DISTINCT FROM NEW.id OR OLD.file_path IS DISTINCT FROM NEW.file_path
               OR OLD.cue_media_path IS DISTINCT FROM NEW.cue_media_path)
         EXECUTE FUNCTION library_paths_revision_bump();
     END IF;
     INSERT INTO library_paths_revision (id, revision, instance)
     VALUES (1, 0, floor(random() * 9007199254740991)::bigint)
     ON CONFLICT (id) DO NOTHING;
     END $chemins$;"
        .to_string()
}

/// `(instance, revision)` des chemins, ou `None` si le compteur est absent.
pub fn lire_revision_des_chemins(db: &dyn DbBackend) -> Option<(i64, i64)> {
    let ligne = db
        .query_one(
            "SELECT instance, revision FROM library_paths_revision WHERE id = 1",
            &[],
        )
        .ok()??;
    Some((ligne.first()?.as_i64()?, ligne.get(1)?.as_i64()?))
}

#[cfg(test)]
#[path = "arbre_des_dossiers_tests.rs"]
mod tests;
