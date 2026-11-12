//! « Pas pour moi » — les suggestions d'accueil écartées, PAR PROFIL
//! (renesenses/tune-web-client#916, FabienM, fil forum 1527).
//!
//! FabienM, 23/08/2026 : « Une découverte — une seule proposition, grande
//! pochette, avec les deux boutons "Écouter" / "Pas pour moi" ». Go de
//! Bertrand le 12/10/2026.
//!
//! # Ce qu'on écarte
//!
//! Trois sortes d'objets, et rien d'autre :
//!
//! | `item_type` | `source`              | `item_id`                       |
//! |-------------|-----------------------|---------------------------------|
//! | `album`     | `local`               | l'`albums.id` de la bibliothèque |
//! | `album`     | un service (`qobuz`…) | son `source_id` chez le service  |
//! | `playlist`  | un service            | son `source_id` chez le service  |
//! | `artist`    | `nom`                 | le NOM normalisé ([`nom_normalise`]) |
//!
//! Un artiste se désigne par son NOM, et non par un identifiant : les rangées
//! qui le proposent (« Nouveautés de vos artistes », recommandations) n'en
//! portent pas d'autre, et un artiste écarté doit l'être chez TOUS les
//! services à la fois — « Miles Davis, pas pour moi » ne veut pas dire « pas
//! sur Qobuz ».
//!
//! Une playlist LOCALE ne s'écarte pas : aucune rangée de suggestion n'en
//! propose ; c'est l'auditeur qui les a faites.
//!
//! # Par profil, écrit ET lu
//!
//! Le profil est celui de l'action (`X-Profile-Id`). Les rangées de
//! recommandation le lisent pour se filtrer : un autre profil de la maison
//! voit toujours l'élément. C'était l'objection n° 2 du dossier d'instruction
//! de web#916 (27/08) : un « Pas pour moi » global masquerait la proposition
//! pour toute la maisonnée.
//!
//! # Ce n'est NI un masquage NI un bannissement
//!
//! `hidden_items` retire un album de la BIBLIOTHÈQUE ; `streaming_hidden_items`
//! empêche un TITRE d'être joué automatiquement. Ici, rien ne disparaît
//! ailleurs que dans les rangées de SUGGESTION de l'accueil : l'album reste
//! dans la bibliothèque, se cherche, se joue, s'ouvre. D'où une table à part
//! plutôt qu'un `item_type` de plus dans `hidden_items` — le jour où un
//! filtre de bibliothèque lirait `hidden_items` sans regarder le type, un
//! « pas pour moi » ferait disparaître un disque qu'on possède.
//!
//! # L'instantané d'affichage
//!
//! `title`, `subtitle` et `cover_url` sont figés à l'écart : la liste des
//! Réglages (« Suggestions écartées ») doit pouvoir dire QUOI a été écarté
//! sans réinterroger un service, ni retrouver un album dont l'id aurait
//! changé.
//!
//! # Un album local survit au rescan (go du 13/10/2026)
//!
//! Un album local se désigne par son `albums.id`, et un id meurt quand la
//! racine est déplacée ou la bibliothèque vidée puis rescannée. Même
//! mécanique que `hidden_items` ([`super::hidden_repo::HiddenRepo::reconcile`]),
//! et SANS colonne de plus : l'instantané `title` / `subtitle` EST l'identité
//! (titre de l'album, nom de son artiste). C'est pourquoi l'écart d'un album
//! local vivant fige le titre et l'artiste LUS EN BASE, et non ce que le
//! client a affiché sous la vignette : une ligne de raison n'est pas un nom
//! d'artiste. [`SuggestionsEcarteesRepo::reconcile`] re-rattache ensuite par
//! [`super::favorites_reconcile::find_album_by_identity`], aux mêmes ancrages
//! que les albums masqués (démarrage, fin de scan, scan auto, purge).

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::info;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::favorites_reconcile::{ReconcileStats, album_live_identity, find_album_by_identity};

/// Un album, local ou de service.
pub const TYPE_ALBUM: &str = "album";
/// Un artiste, désigné par son nom normalisé.
pub const TYPE_ARTISTE: &str = "artist";
/// Une playlist de SERVICE.
pub const TYPE_PLAYLIST: &str = "playlist";

/// La source d'un album de la bibliothèque.
pub const SOURCE_LOCALE: &str = "local";
/// La « source » d'un artiste : il est désigné par son nom, chez tous les
/// services à la fois.
pub const SOURCE_NOM: &str = "nom";

/// Le nom d'un artiste tel qu'on le compare : minuscules, espaces réduits.
///
/// Volontairement modeste. Chaque règle ajoutée ici élargit ce qu'un seul
/// clic retire de l'accueil ; retirer à tort « The Doors » parce qu'on a
/// écarté « Doors » serait pire que de laisser passer une graphie voisine.
pub fn nom_normalise(nom: &str) -> String {
    nom.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Une suggestion écartée — ce que la route reçoit et ce qu'elle rend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionEcartee {
    pub item_type: String,
    /// `local`, un service en minuscules, ou `nom` pour un artiste. Le client
    /// peut l'omettre pour un artiste.
    #[serde(default)]
    pub source: String,
    /// Identifiant dans l'espace de `source` ; pour un artiste, son nom.
    pub item_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Posé par la base ; ignoré à l'écriture.
    #[serde(default, skip_deserializing)]
    pub created_at: Option<String>,
}

/// Valide et met en forme une désignation. `None` = désignation refusée
/// (type inconnu, identifiant vide, playlist locale, album local dont l'id
/// n'est pas un entier) : la route rend 400, rien n'est écrit.
pub fn normaliser(s: &SuggestionEcartee) -> Option<SuggestionEcartee> {
    let item_type = s.item_type.trim().to_lowercase();
    let source = s.source.trim().to_lowercase();
    let brut = s.item_id.trim();
    if brut.is_empty() {
        return None;
    }
    let (source, item_id) = match item_type.as_str() {
        TYPE_ARTISTE => {
            let nom = nom_normalise(brut);
            if nom.is_empty() {
                return None;
            }
            (SOURCE_NOM.to_string(), nom)
        }
        TYPE_ALBUM if source.is_empty() || source == SOURCE_LOCALE => {
            // Un album local se désigne par un entier positif — l'id que lit
            // la rangée. Une chaîne qui n'en est pas un ne désignerait rien.
            let id: i64 = brut.parse().ok().filter(|n| *n > 0)?;
            (SOURCE_LOCALE.to_string(), id.to_string())
        }
        TYPE_ALBUM => (source, brut.to_string()),
        TYPE_PLAYLIST if source.is_empty() || source == SOURCE_LOCALE || source == SOURCE_NOM => {
            return None;
        }
        TYPE_PLAYLIST => (source, brut.to_string()),
        _ => return None,
    };
    if source == SOURCE_NOM && item_type != TYPE_ARTISTE {
        return None;
    }
    let propre = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Some(SuggestionEcartee {
        item_type,
        source,
        item_id,
        title: propre(&s.title).or_else(|| {
            (s.item_type.trim().eq_ignore_ascii_case(TYPE_ARTISTE)).then(|| brut.to_string())
        }),
        subtitle: propre(&s.subtitle),
        cover_url: propre(&s.cover_url),
        created_at: None,
    })
}

/// Tout ce qu'un profil a écarté, en mémoire, pour filtrer une rangée sans
/// une requête par élément. La liste d'un profil se compte en dizaines.
#[derive(Debug, Default, Clone)]
pub struct Ecartees {
    albums_locaux: HashSet<i64>,
    /// `(item_type, source, item_id)` des objets de service.
    distants: HashSet<(String, String, String)>,
    artistes: HashSet<String>,
}

impl Ecartees {
    pub fn est_vide(&self) -> bool {
        self.albums_locaux.is_empty() && self.distants.is_empty() && self.artistes.is_empty()
    }

    /// Combien d'objets sont écartés — la marge qu'une rangée demande en plus
    /// à sa requête pour rester pleine après filtrage.
    pub fn len(&self) -> usize {
        self.albums_locaux.len() + self.distants.len() + self.artistes.len()
    }

    pub fn album_local(&self, id: i64) -> bool {
        self.albums_locaux.contains(&id)
    }

    pub fn album_de_service(&self, source: &str, source_id: &str) -> bool {
        self.distant(TYPE_ALBUM, source, source_id)
    }

    pub fn playlist_de_service(&self, source: &str, source_id: &str) -> bool {
        self.distant(TYPE_PLAYLIST, source, source_id)
    }

    pub fn artiste(&self, nom: &str) -> bool {
        !self.artistes.is_empty() && self.artistes.contains(&nom_normalise(nom))
    }

    fn distant(&self, item_type: &str, source: &str, source_id: &str) -> bool {
        !self.distants.is_empty()
            && self.distants.contains(&(
                item_type.to_string(),
                source.trim().to_lowercase(),
                source_id.trim().to_string(),
            ))
    }

    /// Ajoute une désignation rangée (déjà normalisée) à l'ensemble.
    pub fn ajouter(&mut self, s: &SuggestionEcartee) {
        match (s.item_type.as_str(), s.source.as_str()) {
            (TYPE_ARTISTE, _) => {
                self.artistes.insert(nom_normalise(&s.item_id));
            }
            (TYPE_ALBUM, SOURCE_LOCALE) => {
                if let Ok(id) = s.item_id.parse() {
                    self.albums_locaux.insert(id);
                }
            }
            _ => {
                self.distants
                    .insert((s.item_type.clone(), s.source.clone(), s.item_id.clone()));
            }
        }
    }
}

mod sql {
    use super::SqlDialect;

    /// Réécarter rafraîchit l'instantané sans toucher à la date du premier
    /// écart.
    pub fn ecarter<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO suggestions_ecartees \
             (profile_id, item_type, source, item_id, title, subtitle, cover_url, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, source, item_id) DO UPDATE SET \
             title = COALESCE(excluded.title, suggestions_ecartees.title), \
             subtitle = COALESCE(excluded.subtitle, suggestions_ecartees.subtitle), \
             cover_url = COALESCE(excluded.cover_url, suggestions_ecartees.cover_url)",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.now_iso8601(),
        )
    }

    pub fn retablir<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM suggestions_ecartees \
             WHERE profile_id = {} AND item_type = {} AND source = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    pub fn lister<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_type, source, item_id, title, subtitle, cover_url, created_at \
             FROM suggestions_ecartees WHERE profile_id = {} \
             ORDER BY created_at DESC, item_type ASC, source ASC, item_id ASC",
            d.placeholder(1),
        )
    }
}

pub struct SuggestionsEcarteesRepo {
    db: Arc<dyn DbBackend>,
}

impl SuggestionsEcarteesRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    /// Écarte une suggestion pour ce profil. `Ok(None)` = désignation refusée
    /// (voir [`normaliser`]) : rien n'est écrit. Idempotent.
    pub fn ecarter(
        &self,
        profile_id: i64,
        s: &SuggestionEcartee,
    ) -> Result<Option<SuggestionEcartee>, String> {
        let Some(mut n) = normaliser(s) else {
            return Ok(None);
        };
        // L'identité d'un album local vivant, lue en base : c'est elle que la
        // réconciliation suivra si l'id meurt au rescan.
        if n.item_type == TYPE_ALBUM && n.source == SOURCE_LOCALE {
            if let Ok(id) = n.item_id.parse::<i64>() {
                if let Some((titre, artiste)) = album_live_identity(self.db.as_ref(), id)? {
                    if !titre.trim().is_empty() {
                        n.title = Some(titre);
                        n.subtitle = (!artiste.trim().is_empty()).then_some(artiste);
                    }
                }
            }
        }
        let sql = self.dialect_sql(sql::ecarter, sql::ecarter);
        let params: [&dyn ToSqlValue; 7] = [
            &profile_id,
            &n.item_type,
            &n.source,
            &n.item_id,
            &n.title,
            &n.subtitle,
            &n.cover_url,
        ];
        self.db.execute(&sql, &params)?;
        info!(profile_id, item_type = %n.item_type, source = %n.source, item_id = %n.item_id, "suggestion_ecartee");
        Ok(Some(n))
    }

    /// Rétablit une suggestion. `Ok(false)` = rien n'était écarté (ou
    /// désignation refusée). Le geste « Annuler » du message et le bouton des
    /// Réglages passent tous deux ici.
    pub fn retablir(&self, profile_id: i64, s: &SuggestionEcartee) -> Result<bool, String> {
        let Some(n) = normaliser(s) else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::retablir, sql::retablir);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &n.item_type, &n.source, &n.item_id];
        let lignes = self.db.execute(&sql, &params)?;
        if lignes > 0 {
            info!(profile_id, item_type = %n.item_type, source = %n.source, item_id = %n.item_id, "suggestion_retablie");
        }
        Ok(lignes > 0)
    }

    /// Tout ce que ce profil a écarté, le plus récent d'abord.
    pub fn lister(&self, profile_id: i64) -> Result<Vec<SuggestionEcartee>, String> {
        let sql = self.dialect_sql(sql::lister, sql::lister);
        let params: [&dyn ToSqlValue; 1] = [&profile_id];
        Ok(self
            .db
            .query_many(&sql, &params)?
            .iter()
            .filter_map(|r| {
                let texte = |i: usize| r.get(i).and_then(|v| v.as_string());
                Some(SuggestionEcartee {
                    item_type: texte(0)?,
                    source: texte(1)?,
                    item_id: texte(2)?,
                    title: texte(3),
                    subtitle: texte(4),
                    cover_url: texte(5),
                    created_at: texte(6),
                })
            })
            .collect())
    }

    /// Re-rattache les albums LOCAUX écartés dont l'id a disparu, par leur
    /// instantané (`title` = titre, `subtitle` = artiste) — la mécanique de
    /// [`super::hidden_repo::HiddenRepo::reconcile`], pour la même raison.
    ///
    /// `delete_unresolved` n'est vrai qu'après un scan COMPLET et sain : c'est
    /// la seule situation où « introuvable » veut dire « n'existe plus ». Au
    /// démarrage ou sur un scan partiel, l'écart orphelin est CONSERVÉ — un
    /// volume pas encore monté peut ramener l'album, et un album écarté qui
    /// revient dans les suggestions est précisément ce que l'auditeur a
    /// refusé.
    ///
    /// Ne touche ni aux artistes (désignés par leur nom) ni aux objets de
    /// service (leur `source_id` ne dépend pas du scan).
    pub fn reconcile(&self, delete_unresolved: bool) -> Result<ReconcileStats, String> {
        let params: [&dyn ToSqlValue; 2] = [&TYPE_ALBUM, &SOURCE_LOCALE];
        let rows = self.db.query_many(
            "SELECT profile_id, item_id, title, subtitle FROM suggestions_ecartees \
             WHERE item_type = ? AND source = ?",
            &params,
        )?;
        let mut stats = ReconcileStats::default();
        for row in &rows {
            let Some(profile_id) = row.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(item_id) = row.get(1).and_then(|v| v.as_string()) else {
                continue;
            };
            let Ok(album_id) = item_id.parse::<i64>() else {
                continue;
            };
            stats.scanned += 1;
            let titre = row.get(2).and_then(|v| v.as_string()).unwrap_or_default();
            let artiste = row.get(3).and_then(|v| v.as_string()).unwrap_or_default();

            if let Some((t, a)) = album_live_identity(self.db.as_ref(), album_id)? {
                // Vivant : au plus un instantané à rattraper (écart posé
                // avant que l'identité ne soit figée en base).
                if titre.trim().is_empty() && !t.trim().is_empty() {
                    let a = (!a.trim().is_empty()).then_some(a);
                    let params: [&dyn ToSqlValue; 6] =
                        [&t, &a, &profile_id, &TYPE_ALBUM, &SOURCE_LOCALE, &item_id];
                    self.db.execute(
                        "UPDATE suggestions_ecartees SET title = ?, subtitle = ? \
                         WHERE profile_id = ? AND item_type = ? AND source = ? AND item_id = ?",
                        &params,
                    )?;
                    stats.snapshots_backfilled += 1;
                }
                continue;
            }

            let cible = if titre.trim().is_empty() {
                None
            } else {
                find_album_by_identity(self.db.as_ref(), titre.trim(), artiste.trim())?
            };
            let supprimer = |stats_champ: &mut u64| -> Result<(), String> {
                let params: [&dyn ToSqlValue; 4] =
                    [&profile_id, &TYPE_ALBUM, &SOURCE_LOCALE, &item_id];
                self.db.execute(
                    "DELETE FROM suggestions_ecartees \
                     WHERE profile_id = ? AND item_type = ? AND source = ? AND item_id = ?",
                    &params,
                )?;
                *stats_champ += 1;
                Ok(())
            };
            match cible {
                Some(nouvel_id) if nouvel_id != album_id => {
                    let nouvel = nouvel_id.to_string();
                    let params: [&dyn ToSqlValue; 4] =
                        [&profile_id, &TYPE_ALBUM, &SOURCE_LOCALE, &nouvel];
                    let deja = self
                        .db
                        .query_one(
                            "SELECT COUNT(*) FROM suggestions_ecartees \
                             WHERE profile_id = ? AND item_type = ? AND source = ? AND item_id = ?",
                            &params,
                        )?
                        .and_then(|c| c.first().and_then(|v| v.as_i64()))
                        .unwrap_or(0)
                        > 0;
                    if deja {
                        // Le profil a déjà écarté l'album vivant : l'orphelin
                        // est un doublon.
                        supprimer(&mut stats.deduplicated)?;
                    } else {
                        let (t, a) = album_live_identity(self.db.as_ref(), nouvel_id)?
                            .unwrap_or((titre.clone(), artiste.clone()));
                        let a = (!a.trim().is_empty()).then_some(a);
                        let params: [&dyn ToSqlValue; 7] = [
                            &nouvel,
                            &t,
                            &a,
                            &profile_id,
                            &TYPE_ALBUM,
                            &SOURCE_LOCALE,
                            &item_id,
                        ];
                        self.db.execute(
                            "UPDATE suggestions_ecartees SET item_id = ?, title = ?, subtitle = ? \
                             WHERE profile_id = ? AND item_type = ? AND source = ? AND item_id = ?",
                            &params,
                        )?;
                        info!(
                            profile_id,
                            old_id = album_id,
                            new_id = nouvel_id,
                            "suggestion_ecartee_rerattachee"
                        );
                        stats.relinked += 1;
                    }
                }
                _ if delete_unresolved => supprimer(&mut stats.deleted)?,
                _ => stats.unresolved += 1,
            }
        }
        Ok(stats)
    }

    /// L'ensemble en mémoire que lisent les rangées. Une erreur de lecture
    /// rend un ensemble VIDE : une rangée qui ne filtre pas vaut mieux qu'un
    /// accueil vide.
    pub fn ensemble(&self, profile_id: i64) -> Ecartees {
        let mut e = Ecartees::default();
        match self.lister(profile_id) {
            Ok(liste) => liste.iter().for_each(|s| e.ajouter(s)),
            Err(err) => tracing::warn!(profile_id, error = %err, "suggestions_ecartees_illisibles"),
        }
        e
    }
}

/// La réconciliation telle que l'appellent les ancrages du serveur
/// (démarrage, fin de scan, scan auto, purge) : une ligne de journal si
/// quelque chose a bougé ou reste orphelin, un avertissement sur erreur.
/// `contexte` nomme l'ancrage dans le journal.
pub fn reconcilier_journalise(
    db: Arc<dyn DbBackend>,
    delete_unresolved: bool,
    contexte: &str,
) -> ReconcileStats {
    match SuggestionsEcarteesRepo::with_backend(db).reconcile(delete_unresolved) {
        Ok(s) => {
            if s.changed() > 0 || s.unresolved > 0 {
                info!(
                    contexte,
                    scanned = s.scanned,
                    snapshots = s.snapshots_backfilled,
                    relinked = s.relinked,
                    deduplicated = s.deduplicated,
                    deleted = s.deleted,
                    unresolved = s.unresolved,
                    "suggestions_ecartees_reconciliees"
                );
            }
            s
        }
        Err(e) => {
            tracing::warn!(contexte, error = %e, "suggestions_ecartees_reconcile_failed");
            ReconcileStats::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;

    fn repo() -> SuggestionsEcarteesRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        SuggestionsEcarteesRepo::with_backend(Arc::new(db))
    }

    fn s(item_type: &str, source: &str, item_id: &str) -> SuggestionEcartee {
        SuggestionEcartee {
            item_type: item_type.into(),
            source: source.into(),
            item_id: item_id.into(),
            title: Some("Titre".into()),
            subtitle: None,
            cover_url: None,
            created_at: None,
        }
    }

    #[test]
    fn les_designations_absurdes_sont_refusees() {
        assert!(normaliser(&s("album", "local", "abc")).is_none());
        assert!(normaliser(&s("album", "local", "0")).is_none());
        assert!(normaliser(&s("album", "", "  ")).is_none());
        assert!(normaliser(&s("playlist", "local", "3")).is_none());
        assert!(normaliser(&s("playlist", "", "3")).is_none());
        assert!(normaliser(&s("track", "qobuz", "3")).is_none());
        assert!(normaliser(&s("album", "nom", "x")).is_none());
        let a = normaliser(&s("artist", "", "  Miles   DAVIS ")).unwrap();
        assert_eq!(
            (a.source.as_str(), a.item_id.as_str()),
            ("nom", "miles davis")
        );
        let q = normaliser(&s("album", " Qobuz", " 0060253 ")).unwrap();
        assert_eq!(
            (q.source.as_str(), q.item_id.as_str()),
            ("qobuz", "0060253")
        );
    }

    #[test]
    fn ecarter_lister_retablir_par_profil() {
        let r = repo();
        assert!(r.ecarter(1, &s("album", "local", "42")).unwrap().is_some());
        // Deux fois : pas de doublon.
        assert!(r.ecarter(1, &s("album", "local", "42")).unwrap().is_some());
        assert!(
            r.ecarter(1, &s("artist", "", "Miles Davis"))
                .unwrap()
                .is_some()
        );
        assert!(
            r.ecarter(1, &s("playlist", "qobuz", "pl-9"))
                .unwrap()
                .is_some()
        );
        assert_eq!(r.lister(1).unwrap().len(), 3);
        assert!(
            r.lister(2).unwrap().is_empty(),
            "le profil 2 n'a rien écarté"
        );

        let e = r.ensemble(1);
        assert!(e.album_local(42) && !e.album_local(43));
        assert!(e.artiste("MILES  davis"));
        assert!(e.playlist_de_service("Qobuz", "pl-9"));
        assert!(
            !e.album_de_service("qobuz", "pl-9"),
            "une playlist n'est pas un album"
        );
        assert!(r.ensemble(2).est_vide());

        assert!(r.retablir(1, &s("album", "local", "42")).unwrap());
        assert!(!r.retablir(1, &s("album", "local", "42")).unwrap());
        assert!(!r.ensemble(1).album_local(42));
    }

    fn sql(r: &SuggestionsEcarteesRepo, q: &str) {
        r.db.execute(q, &[]).expect(q);
    }

    /// Miles Davis, « Kind of Blue » sous l'id 1 ; un homonyme de titre chez
    /// un autre artiste sous l'id 2.
    fn bibliotheque(r: &SuggestionsEcarteesRepo) {
        sql(
            r,
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'), (2, 'Autre')",
        );
        sql(
            r,
            "INSERT INTO albums (id, title, artist_id, track_count) VALUES \
             (1, 'Kind of Blue', 1, 5), (2, 'Kind of Blue', 2, 5)",
        );
    }

    /// Le rescan : l'album 1 meurt et renaît sous l'id 7.
    fn rescan(r: &SuggestionsEcarteesRepo) {
        sql(r, "DELETE FROM albums WHERE id = 1");
        sql(
            r,
            "INSERT INTO albums (id, title, artist_id, track_count) VALUES (7, 'Kind of Blue', 1, 5)",
        );
    }

    #[test]
    fn l_ecart_d_un_album_local_fige_son_identite_lue_en_base() {
        let r = repo();
        bibliotheque(&r);
        let mut d = s("album", "local", "1");
        // Ce que la vignette affichait sous le titre : une raison, pas l'artiste.
        d.subtitle = Some("Un genre que vous écoutez souvent : Jazz".into());
        d.title = Some("Kind of Blue (vignette)".into());
        let n = r.ecarter(1, &d).unwrap().unwrap();
        assert_eq!(n.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(n.subtitle.as_deref(), Some("Miles Davis"));
        assert_eq!(
            r.lister(1).unwrap()[0].subtitle.as_deref(),
            Some("Miles Davis")
        );
    }

    #[test]
    fn l_ecart_suit_l_album_renouvele_au_rescan_et_pas_son_homonyme() {
        let r = repo();
        bibliotheque(&r);
        r.ecarter(1, &s("album", "local", "1")).unwrap();
        r.ecarter(2, &s("album", "local", "1")).unwrap();
        rescan(&r);
        assert!(!r.ensemble(1).album_local(7), "avant : l'écart est perdu");

        let st = r.reconcile(false).unwrap();
        assert_eq!(
            (st.scanned, st.relinked, st.unresolved),
            (2, 2, 0),
            "{st:?}"
        );
        for profil in [1, 2] {
            let e = r.ensemble(profil);
            assert!(e.album_local(7), "profil {profil} : suivi vers 7");
            assert!(!e.album_local(2), "jamais l'homonyme d'un autre artiste");
            assert!(!e.album_local(1));
        }
        // Idempotent.
        assert_eq!(r.reconcile(false).unwrap().changed(), 0);
    }

    #[test]
    fn l_orphelin_n_est_purge_que_sur_scan_complet_et_le_doublon_est_retire() {
        let r = repo();
        bibliotheque(&r);
        r.ecarter(1, &s("album", "local", "1")).unwrap();
        sql(&r, "DELETE FROM albums WHERE id = 1");
        let st = r.reconcile(false).unwrap();
        assert_eq!((st.unresolved, st.deleted), (1, 0));
        assert_eq!(
            r.lister(1).unwrap().len(),
            1,
            "gardé : un volume peut revenir"
        );
        let st = r.reconcile(true).unwrap();
        assert_eq!(st.deleted, 1);
        assert!(r.lister(1).unwrap().is_empty());

        // Doublon : le profil a écarté l'ancien ET le nouvel id.
        let r = repo();
        bibliotheque(&r);
        r.ecarter(1, &s("album", "local", "1")).unwrap();
        rescan(&r);
        r.ecarter(1, &s("album", "local", "7")).unwrap();
        let st = r.reconcile(false).unwrap();
        assert_eq!(st.deduplicated, 1, "{st:?}");
        let l = r.lister(1).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].item_id, "7");
    }

    #[test]
    fn la_reconciliation_ne_touche_ni_artistes_ni_objets_de_service() {
        let r = repo();
        r.ecarter(1, &s("artist", "", "Miles Davis")).unwrap();
        r.ecarter(1, &s("album", "qobuz", "0060253")).unwrap();
        let st = r.reconcile(true).unwrap();
        assert_eq!(st.scanned, 0);
        assert_eq!(r.lister(1).unwrap().len(), 2);
    }
}
