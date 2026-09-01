//! « Ces deux albums ne sont PAS des doublons » — l'arbitrage de l'utilisateur
//! sur le rapprochement d'albums (#1276).
//!
//! Megalo, forum-hifi.fr #41831 p.13 : « Tune me trouve des albums doublons
//! alors que ce sont des releases différentes ». Deux pressages, un original
//! et sa réédition remasterisée, un vinyle et son CD : Tune les rapproche, et
//! l'utilisateur n'avait aucun moyen de dire le contraire.
//!
//! # Où le rapprochement se fait, et donc ce que ce marqueur doit atteindre
//!
//! Deux chemins, et deux seulement, rapprochent des ALBUMS :
//!
//! | chemin | ce qu'il fait | risque |
//! |---|---|---|
//! | `GET /library/albums/grouped` | signale les groupes (MBID de release group, puis titre « dévarianté ») | l'alerte |
//! | `POST /library/albums/merge-duplicates` | FUSIONNE par `LOWER(title)` : les pistes changent d'album, la ligne perdante est SUPPRIMÉE | irréversible |
//!
//! L'issue demande les deux : ne plus voir l'alerte, ET ne pas se faire
//! fusionner. Un marqueur qui ne couvrirait que l'affichage laisserait la
//! fusion détruire quand même — c'est le seul des deux qui ne se répare pas.
//!
//! # Pourquoi une table de paires, sans clé étrangère
//!
//! Exactement le raisonnement de `hidden_repo` (#1391), pour la même raison :
//! une ligne `albums` est supprimée en ROUTINE — purge post-scan,
//! `delete_orphans`, fusion de doublons, « vider la bibliothèque ». Une
//! colonne, une clé étrangère ou un simple couple d'ids mourrait au premier
//! déplacement de racine, et l'arbitrage de l'utilisateur avec.
//!
//! On reprend donc la mécanique qui a réparé les favoris puis les albums
//! masqués :
//! 1. **instantané d'identité** (titre + artiste) figé **des deux côtés** à
//!    l'écriture du marqueur ;
//! 2. **réconciliation** aux mêmes cinq ancrages que `hidden_items`
//!    (démarrage, `scan.rs`, `auto_scan.rs`, purge d'orphelines, et la route
//!    de purge de `config.rs`), via le **même** [`find_album_by_identity`] —
//!    pas une seconde règle de rattachement. La PR #2848 a montré ce que coûte
//!    un repli divergent : une chaîne vide traitée autrement qu'un NULL, et un
//!    rattachement à tous les homonymes.
//!
//! # Coût de lecture : nul sur le chemin chaud
//!
//! Les deux consommateurs chargent l'ensemble des paires **en une requête**
//! (`SELECT album_a_id, album_b_id`), puis interrogent un `HashSet` en
//! mémoire. Aucune comparaison `LOWER` n'est ajoutée au rapprochement :
//! #2848 a mesuré ×4000 (19 ms → 83 s) pour un `LOWER` non indexé sur ce
//! chemin. Ici les `LOWER` sont confinés à la réconciliation, bornée par le
//! nombre de marqueurs — quelques dizaines, pas la bibliothèque.
//!
//! # Paire normalisée
//!
//! Une paire est un couple NON ORDONNÉ : « A n'est pas un doublon de B » est
//! la même décision que l'inverse. On la range donc toujours en
//! `(min(a,b), max(a,b))` — à l'écriture comme après un re-rattachement, où
//! les ids renouvelés peuvent inverser l'ordre. Sans cette normalisation, la
//! clé primaire laisserait entrer le même arbitrage deux fois, et la moitié
//! des lectures le manquerait.
//!
//! # Global, pas par profil
//!
//! `profile_id` est ÉCRIT (toujours 1) et jamais LU, même convention que
//! `hidden_items` : aucune route de lecture bibliothèque ne connaît le profil
//! aujourd'hui. La colonne est là pour le jour où elles le connaîtront.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use tracing::info;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::favorites_reconcile::{ReconcileStats, album_live_identity, find_album_by_identity};

/// Marqueur GLOBAL : on écrit le profil pour préparer l'avenir, on ne le lit
/// jamais — même choix que `hidden_items`.
const GLOBAL_PROFILE_ID: i64 = 1;

/// Range une paire d'ids d'albums dans l'ordre canonique.
///
/// « A n'est pas un doublon de B » ne dépend pas de l'ordre : sans cette
/// normalisation le même arbitrage entrerait deux fois dans la table, et une
/// lecture sur deux le manquerait.
fn normaliser(a: i64, b: i64) -> (i64, i64) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Une paire déclarée distincte, telle que la route de révision la rend.
#[derive(Debug, Clone, Serialize)]
pub struct DistinctPair {
    pub album_a_id: i64,
    pub album_b_id: i64,
    /// Titre vivant si l'album existe encore, sinon l'instantané figé à
    /// l'arbitrage — la liste reste lisible pendant qu'une racine est
    /// démontée, donc l'arbitrage reste révocable.
    pub a_title: String,
    pub a_artist: Option<String>,
    pub b_title: String,
    pub b_artist: Option<String>,
    pub created_at: Option<String>,
    /// `false` = au moins un des deux ids ne désigne plus d'album vivant, en
    /// attente de réconciliation.
    pub resolved: bool,
}

/// Toutes les paires distinctes, chargées d'un coup et interrogées en
/// mémoire.
///
/// C'est ce qui garde le rapprochement d'albums au même coût qu'avant :
/// UNE requête par appel de route, puis des lectures `HashSet`. Le détecteur
/// ne fait AUCUNE requête par paire candidate.
#[derive(Debug, Default, Clone)]
pub struct DistinctPairSet {
    paires: HashSet<(i64, i64)>,
}

impl DistinctPairSet {
    /// L'utilisateur a-t-il déclaré ces deux albums distincts ? L'ordre des
    /// arguments est indifférent.
    pub fn contains(&self, a: i64, b: i64) -> bool {
        self.paires.contains(&normaliser(a, b))
    }

    pub fn is_empty(&self) -> bool {
        self.paires.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paires.len()
    }
}

/// Constructeurs SQL agnostiques du moteur.
pub mod sql {
    use super::SqlDialect;

    /// `ON CONFLICT … DO NOTHING` : redire deux fois « ce ne sont pas des
    /// doublons » est un non-événement, pas une erreur — même convention que
    /// `hidden_items` et `favorite_facets`.
    pub fn declarer<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO album_distinct_pairs \
             (profile_id, album_a_id, album_b_id, a_name, a_artist, b_name, b_artist, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, album_a_id, album_b_id) DO NOTHING",
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

    /// La révocation est GLOBALE comme la déclaration : pas de filtre profil.
    pub fn revoquer<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_distinct_pairs WHERE album_a_id = {} AND album_b_id = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn compter_une<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM album_distinct_pairs WHERE album_a_id = {} AND album_b_id = {}",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    /// LEFT JOIN des deux côtés : une paire dont un album est momentanément
    /// mort reste listée, avec son instantané — c'est ce qui permet de la
    /// révoquer quand même.
    pub const LISTER: &str = "SELECT p.album_a_id, p.album_b_id, p.a_name, p.a_artist, \
                                     p.b_name, p.b_artist, p.created_at, \
                                     a.id, a.title, ara.name, \
                                     b.id, b.title, arb.name \
                              FROM album_distinct_pairs p \
                              LEFT JOIN albums a ON a.id = p.album_a_id \
                              LEFT JOIN artists ara ON ara.id = a.artist_id \
                              LEFT JOIN albums b ON b.id = p.album_b_id \
                              LEFT JOIN artists arb ON arb.id = b.artist_id \
                              ORDER BY p.created_at DESC, p.album_a_id ASC, p.album_b_id ASC";
}

pub struct AlbumDistinctRepo {
    db: Arc<dyn DbBackend>,
}

impl AlbumDistinctRepo {
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

    /// Déclare deux albums distincts. `Ok(false)` = paire refusée : id
    /// inconnu, ou les deux ids identiques (un album n'est pas le doublon de
    /// lui-même). Idempotent.
    ///
    /// Les instantanés d'identité sont figés ICI, dans le même INSERT — c'est
    /// eux qui font survivre l'arbitrage au renouvellement des rowids.
    pub fn declarer_distincts(&self, a: i64, b: i64) -> Result<bool, String> {
        if a == b {
            return Ok(false);
        }
        let (bas, haut) = normaliser(a, b);
        let (Some((bas_titre, bas_artiste)), Some((haut_titre, haut_artiste))) = (
            album_live_identity(self.db.as_ref(), bas)?,
            album_live_identity(self.db.as_ref(), haut)?,
        ) else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::declarer, sql::declarer);
        let params: [&dyn ToSqlValue; 7] = [
            &GLOBAL_PROFILE_ID,
            &bas,
            &haut,
            &bas_titre,
            &bas_artiste,
            &haut_titre,
            &haut_artiste,
        ];
        self.db.execute(&sql, &params)?;
        Ok(true)
    }

    /// Révoque l'arbitrage. `Ok(false)` = rien n'était déclaré pour cette
    /// paire.
    pub fn revoquer(&self, a: i64, b: i64) -> Result<bool, String> {
        let (bas, haut) = normaliser(a, b);
        let sql = self.dialect_sql(sql::revoquer, sql::revoquer);
        let params: [&dyn ToSqlValue; 2] = [&bas, &haut];
        Ok(self.db.execute(&sql, &params)? > 0)
    }

    pub fn sont_distincts(&self, a: i64, b: i64) -> Result<bool, String> {
        let (bas, haut) = normaliser(a, b);
        let sql = self.dialect_sql(sql::compter_une, sql::compter_une);
        let params: [&dyn ToSqlValue; 2] = [&bas, &haut];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    /// Charge TOUTES les paires en une requête, pour interrogation en
    /// mémoire par les deux consommateurs. Voir l'en-tête du module : c'est
    /// ce qui évite d'ajouter le moindre coût par candidat au rapprochement.
    pub fn charger_ensemble(&self) -> Result<DistinctPairSet, String> {
        let rows = self.db.query_many(
            "SELECT album_a_id, album_b_id FROM album_distinct_pairs",
            &[],
        )?;
        let mut paires = HashSet::with_capacity(rows.len());
        for r in &rows {
            let (Some(a), Some(b)) = (
                r.first().and_then(|v| v.as_i64()),
                r.get(1).and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            paires.insert(normaliser(a, b));
        }
        Ok(DistinctPairSet { paires })
    }

    /// Toutes les paires déclarées, vivantes comme orphelines.
    pub fn lister(&self) -> Result<Vec<DistinctPair>, String> {
        let rows = self.db.query_many(sql::LISTER, &[])?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let album_a_id = r.first().and_then(|v| v.as_i64())?;
                let album_b_id = r.get(1).and_then(|v| v.as_i64())?;
                let a_snap = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
                let a_snap_art = r.get(3).and_then(|v| v.as_string()).unwrap_or_default();
                let b_snap = r.get(4).and_then(|v| v.as_string()).unwrap_or_default();
                let b_snap_art = r.get(5).and_then(|v| v.as_string()).unwrap_or_default();
                let created_at = r.get(6).and_then(|v| v.as_string());
                let a_vivant = r.get(7).and_then(|v| v.as_i64()).is_some();
                let a_titre = r.get(8).and_then(|v| v.as_string());
                let a_artiste = r.get(9).and_then(|v| v.as_string());
                let b_vivant = r.get(10).and_then(|v| v.as_i64()).is_some();
                let b_titre = r.get(11).and_then(|v| v.as_string());
                let b_artiste = r.get(12).and_then(|v| v.as_string());
                let non_vide = |s: String| if s.is_empty() { None } else { Some(s) };
                Some(DistinctPair {
                    album_a_id,
                    album_b_id,
                    a_title: a_titre.unwrap_or(a_snap),
                    a_artist: a_artiste.or_else(|| non_vide(a_snap_art)),
                    b_title: b_titre.unwrap_or(b_snap),
                    b_artist: b_artiste.or_else(|| non_vide(b_snap_art)),
                    created_at,
                    resolved: a_vivant && b_vivant,
                })
            })
            .collect())
    }

    /// Re-rattache les paires orphelines aux albums vivants retrouvés par
    /// identité — le pendant de `HiddenRepo::reconcile`, appelé aux mêmes
    /// endroits.
    ///
    /// `delete_unresolved` ne doit être vrai qu'après un scan COMPLET et sain
    /// (même règle que les favoris #1943 et les masquages #1391) : c'est la
    /// seule situation où « introuvable » veut dire « n'existe vraiment
    /// plus ». Au démarrage ou sur un scan partiel, une paire orpheline est
    /// CONSERVÉE — un volume pas encore monté peut encore ramener l'album, et
    /// un arbitrage perdu se paie par une fusion destructrice au scan suivant.
    pub fn reconcile(&self, delete_unresolved: bool) -> Result<ReconcileStats, String> {
        let rows = self.db.query_many(
            "SELECT profile_id, album_a_id, album_b_id, a_name, a_artist, b_name, b_artist \
             FROM album_distinct_pairs",
            &[],
        )?;

        let mut stats = ReconcileStats::default();
        for row in &rows {
            let profile_id = row.first().and_then(|v| v.as_i64()).unwrap_or(1);
            let (Some(a_id), Some(b_id)) = (
                row.get(1).and_then(|v| v.as_i64()),
                row.get(2).and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            stats.scanned += 1;
            let a_snap = row.get(3).and_then(|v| v.as_string()).unwrap_or_default();
            let a_snap_art = row.get(4).and_then(|v| v.as_string()).unwrap_or_default();
            let b_snap = row.get(5).and_then(|v| v.as_string()).unwrap_or_default();
            let b_snap_art = row.get(6).and_then(|v| v.as_string()).unwrap_or_default();

            let cote_a = self.resoudre(a_id, &a_snap, &a_snap_art)?;
            let cote_b = self.resoudre(b_id, &b_snap, &b_snap_art)?;

            // Une paire ne vaut que si SES DEUX côtés sont retrouvés : garder
            // un demi-arbitrage laisserait la fusion emporter l'album resté
            // vivant, ce que la paire existait justement pour empêcher.
            let (Some(neuf_a), Some(neuf_b)) = (cote_a, cote_b) else {
                if delete_unresolved {
                    self.supprimer(profile_id, a_id, b_id)?;
                    stats.deleted += 1;
                } else {
                    stats.unresolved += 1;
                }
                continue;
            };

            // Les deux côtés ont convergé vers le MÊME album : la paire est
            // devenue « X n'est pas un doublon de X », qui n'arbitre plus
            // rien. On la retire plutôt que de laisser une ligne dégénérée
            // violer l'invariant a < b.
            if neuf_a == neuf_b {
                self.supprimer(profile_id, a_id, b_id)?;
                info!(
                    ancien_a = a_id,
                    ancien_b = b_id,
                    fusionnes_en = neuf_a,
                    "album_distinct_pair_degeneree_retiree"
                );
                stats.deduplicated += 1;
                continue;
            }

            let (bas, haut) = normaliser(neuf_a, neuf_b);
            if (bas, haut) == (a_id, b_id) {
                // Rien à re-rattacher : au plus un rattrapage d'instantané
                // pour les paires écrites avant qu'il existe.
                if a_snap.is_empty() || b_snap.is_empty() {
                    let (at, aa) = album_live_identity(self.db.as_ref(), bas)?.unwrap_or_default();
                    let (bt, ba) = album_live_identity(self.db.as_ref(), haut)?.unwrap_or_default();
                    let params: [&dyn ToSqlValue; 7] =
                        [&at, &aa, &bt, &ba, &profile_id, &bas, &haut];
                    self.db.execute(
                        "UPDATE album_distinct_pairs \
                         SET a_name = ?, a_artist = ?, b_name = ?, b_artist = ? \
                         WHERE profile_id = ? AND album_a_id = ? AND album_b_id = ?",
                        &params,
                    )?;
                    stats.snapshots_backfilled += 1;
                }
                continue;
            }

            if self.sont_distincts(bas, haut)? {
                // La paire cible est déjà déclarée : celle-ci en est le
                // doublon, on la retire au lieu de violer la clé primaire.
                self.supprimer(profile_id, a_id, b_id)?;
                stats.deduplicated += 1;
                continue;
            }

            // Ré-instantané depuis les albums vivants : la casse du titre ou
            // l'artiste ont pu changer au re-scan.
            let (at, aa) = album_live_identity(self.db.as_ref(), bas)?.unwrap_or_default();
            let (bt, ba) = album_live_identity(self.db.as_ref(), haut)?.unwrap_or_default();
            let params: [&dyn ToSqlValue; 9] =
                [&bas, &haut, &at, &aa, &bt, &ba, &profile_id, &a_id, &b_id];
            self.db.execute(
                "UPDATE album_distinct_pairs \
                 SET album_a_id = ?, album_b_id = ?, a_name = ?, a_artist = ?, \
                     b_name = ?, b_artist = ? \
                 WHERE profile_id = ? AND album_a_id = ? AND album_b_id = ?",
                &params,
            )?;
            info!(
                ancien_a = a_id,
                ancien_b = b_id,
                nouveau_a = bas,
                nouveau_b = haut,
                "album_distinct_pair_relinked"
            );
            stats.relinked += 1;
        }
        Ok(stats)
    }

    /// L'id vivant d'un côté de la paire : lui-même s'il désigne encore un
    /// album, sinon celui retrouvé par identité — **la même règle que les
    /// favoris et les masquages**, jamais une variante locale (#2848).
    fn resoudre(&self, id: i64, nom: &str, artiste: &str) -> Result<Option<i64>, String> {
        if album_live_identity(self.db.as_ref(), id)?.is_some() {
            return Ok(Some(id));
        }
        if nom.is_empty() {
            return Ok(None);
        }
        find_album_by_identity(self.db.as_ref(), nom, artiste)
    }

    fn supprimer(&self, profile_id: i64, a: i64, b: i64) -> Result<(), String> {
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &a, &b];
        self.db.execute(
            "DELETE FROM album_distinct_pairs \
             WHERE profile_id = ? AND album_a_id = ? AND album_b_id = ?",
            &params,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn test_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn insert_artist(db: &Arc<dyn DbBackend>, name: &str) -> i64 {
        let params: [&dyn ToSqlValue; 1] = [&name];
        db.execute("INSERT INTO artists (name) VALUES (?)", &params)
            .unwrap();
        db.last_insert_rowid()
    }

    fn insert_album(db: &Arc<dyn DbBackend>, title: &str, artist_id: i64) -> i64 {
        let params: [&dyn ToSqlValue; 2] = [&title, &artist_id];
        db.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, 10)",
            &params,
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn declarer_revoquer_lister() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Pink Floyd");
        let a = insert_album(&db, "The Dark Side of the Moon", ar);
        let b = insert_album(&db, "The Dark Side Of The Moon", ar);

        assert!(!repo.sont_distincts(a, b).unwrap());
        assert!(repo.declarer_distincts(a, b).unwrap());
        // Idempotent, et insensible à l'ordre des arguments.
        assert!(repo.declarer_distincts(b, a).unwrap());
        assert!(repo.sont_distincts(a, b).unwrap());
        assert!(repo.sont_distincts(b, a).unwrap());

        let listed = repo.lister().unwrap();
        assert_eq!(listed.len(), 1, "une seule ligne malgré l'ordre inversé");
        assert_eq!((listed[0].album_a_id, listed[0].album_b_id), (a, b));
        assert_eq!(listed[0].a_artist.as_deref(), Some("Pink Floyd"));
        assert!(listed[0].resolved);

        assert!(repo.revoquer(b, a).unwrap());
        assert!(!repo.sont_distincts(a, b).unwrap());
        assert!(!repo.revoquer(a, b).unwrap(), "plus rien à révoquer");
    }

    #[test]
    fn un_album_n_est_pas_distinct_de_lui_meme_ni_d_un_fantome() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Air");
        let a = insert_album(&db, "Moon Safari", ar);

        assert!(!repo.declarer_distincts(a, a).unwrap());
        assert!(!repo.declarer_distincts(a, 4242).unwrap());
        assert!(!repo.declarer_distincts(4242, a).unwrap());
        assert!(repo.lister().unwrap().is_empty());
    }

    #[test]
    fn l_ensemble_charge_repond_dans_les_deux_sens() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Portishead");
        let a = insert_album(&db, "Dummy", ar);
        let b = insert_album(&db, "Dummy", ar);
        let c = insert_album(&db, "Third", ar);
        repo.declarer_distincts(b, a).unwrap();

        let ens = repo.charger_ensemble().unwrap();
        assert_eq!(ens.len(), 1);
        assert!(ens.contains(a, b));
        assert!(ens.contains(b, a));
        assert!(!ens.contains(a, c), "une autre paire n'est pas affectée");
    }

    /// LE cas pour lequel la table existe : l'album meurt (racine déplacée,
    /// « vider la bibliothèque ») puis renaît sous un NOUVEL id — l'arbitrage
    /// doit suivre, sans quoi la fusion l'emporterait au scan suivant.
    #[test]
    fn reconcile_suit_le_renouvellement_d_id() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Talvin Singh");
        let a = insert_album(&db, "OK", ar);
        let b = insert_album(&db, "OK (Remaster)", ar);
        assert!(repo.declarer_distincts(a, b).unwrap());

        // Rescan destructeur simulé : les deux lignes meurent, les albums
        // renaissent ailleurs.
        db.execute("DELETE FROM albums", &[]).unwrap();
        let neuf_a = insert_album(&db, "OK", ar);
        let neuf_b = insert_album(&db, "OK (Remaster)", ar);
        assert_ne!((a, b), (neuf_a, neuf_b));

        // Avant réconciliation : paire orpheline, listée mais non résolue.
        let listed = repo.lister().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].resolved);
        assert_eq!(
            listed[0].a_title, "OK",
            "l'instantané garde la liste lisible"
        );

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.relinked, 1);
        assert!(
            repo.sont_distincts(neuf_a, neuf_b).unwrap(),
            "l'arbitrage doit suivre les nouveaux ids"
        );
        assert!(!repo.sont_distincts(a, b).unwrap());
    }

    /// Contre-épreuve du re-rattachement : une paire dont un seul côté meurt
    /// et renaît suit quand même.
    #[test]
    fn reconcile_suit_un_seul_cote_renouvele() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Björk");
        let a = insert_album(&db, "Homogenic", ar);
        let b = insert_album(&db, "Homogenic Live", ar);
        repo.declarer_distincts(a, b).unwrap();

        let params: [&dyn ToSqlValue; 1] = [&b];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();
        let neuf_b = insert_album(&db, "Homogenic Live", ar);

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.relinked, 1);
        assert!(repo.sont_distincts(a, neuf_b).unwrap());
    }

    /// L'ordre canonique tient même quand le re-rattachement l'inverse : le
    /// côté « bas » renaît avec un id PLUS GRAND que l'autre.
    #[test]
    fn reconcile_renormalise_la_paire_inversee() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Massive Attack");
        let a = insert_album(&db, "Mezzanine", ar); // id bas
        let b = insert_album(&db, "Mezzanine (2019)", ar); // id haut
        repo.declarer_distincts(a, b).unwrap();

        // Seul le côté BAS meurt : il renaîtra avec l'id le plus grand.
        let params: [&dyn ToSqlValue; 1] = [&a];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();
        let neuf_a = insert_album(&db, "Mezzanine", ar);
        assert!(neuf_a > b, "le nouvel id doit dépasser l'autre côté");

        repo.reconcile(false).unwrap();
        let listed = repo.lister().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            listed[0].album_a_id < listed[0].album_b_id,
            "la paire reste rangée en (min, max)"
        );
        assert!(repo.sont_distincts(neuf_a, b).unwrap());
        assert!(repo.sont_distincts(b, neuf_a).unwrap());
    }

    /// Un orphelin introuvable n'est supprimé QUE sur un scan complet sain —
    /// jamais au démarrage : un NAS pas encore monté peut le ramener, et
    /// perdre l'arbitrage rouvre la fusion destructrice.
    #[test]
    fn reconcile_ne_supprime_l_introuvable_que_sur_scan_complet() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "daoud");
        let a = insert_album(&db, "ok", ar);
        let b = insert_album(&db, "ok (single)", ar);
        repo.declarer_distincts(a, b).unwrap();
        db.execute("DELETE FROM albums", &[]).unwrap();

        let stats = repo.reconcile(false).unwrap();
        assert_eq!((stats.deleted, stats.unresolved), (0, 1));
        assert_eq!(repo.lister().unwrap().len(), 1);

        let stats = repo.reconcile(true).unwrap();
        assert_eq!(stats.deleted, 1);
        assert!(repo.lister().unwrap().is_empty());
    }

    /// Deux paires qui convergent vers la même : la doublonne est retirée au
    /// lieu de violer la clé primaire.
    #[test]
    fn reconcile_dedoublonne_vers_la_meme_paire() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Boards of Canada");
        let a = insert_album(&db, "Geogaddi", ar);
        let b = insert_album(&db, "Geogaddi", ar);
        let c = insert_album(&db, "Music Has the Right to Children", ar);
        repo.declarer_distincts(a, c).unwrap();
        repo.declarer_distincts(b, c).unwrap();
        assert_eq!(repo.lister().unwrap().len(), 2);

        // La fusion de doublons supprime la ligne perdante : la paire (a, c)
        // se retrouve rattachée à (b, c), déjà déclarée.
        let params: [&dyn ToSqlValue; 1] = [&a];
        db.execute("DELETE FROM albums WHERE id = ?", &params)
            .unwrap();

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.deduplicated, 1);
        let listed = repo.lister().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].album_a_id, listed[0].album_b_id),
            normaliser(b, c)
        );
    }

    /// Les deux côtés retombent sur le MÊME album : la paire n'arbitre plus
    /// rien et disparaît, au lieu de laisser une ligne « X vs X ».
    #[test]
    fn reconcile_retire_une_paire_devenue_degeneree() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Air");
        let a = insert_album(&db, "Moon Safari", ar);
        let b = insert_album(&db, "Moon Safari", ar);
        repo.declarer_distincts(a, b).unwrap();

        // Les deux meurent, un seul renaît : les deux instantanés (même titre,
        // même artiste) retrouvent le même id vivant.
        db.execute("DELETE FROM albums", &[]).unwrap();
        insert_album(&db, "Moon Safari", ar);

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.deduplicated, 1);
        assert!(repo.lister().unwrap().is_empty());
    }

    /// Un rescan ORDINAIRE (l'album garde son rowid, ses colonnes sont
    /// réécrites) ne doit rien changer du tout.
    #[test]
    fn un_rescan_ordinaire_ne_touche_pas_l_arbitrage() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Air");
        let a = insert_album(&db, "Moon Safari", ar);
        let b = insert_album(&db, "Moon Safari (Remaster)", ar);
        repo.declarer_distincts(a, b).unwrap();

        let mut rafraichi = album_repo.get(a).unwrap().unwrap();
        rafraichi.year = Some(1998);
        rafraichi.genre = Some("Downtempo".into());
        album_repo.update(&rafraichi).unwrap();

        assert!(repo.sont_distincts(a, b).unwrap());
        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.changed(), 0, "rien à réparer après un simple update");
        assert!(repo.sont_distincts(a, b).unwrap());
    }

    /// Rattrapage d'instantané : une paire écrite sans identité figée (base
    /// remontée à la main, import) la reçoit à la première réconciliation,
    /// pour survivre au PROCHAIN renouvellement d'ids.
    #[test]
    fn reconcile_rattrape_un_instantane_manquant() {
        let db = test_db();
        let repo = AlbumDistinctRepo::with_backend(db.clone());
        let ar = insert_artist(&db, "Miles Davis");
        let a = insert_album(&db, "Kind of Blue", ar);
        let b = insert_album(&db, "Kind of Blue (Mono)", ar);
        let (bas, haut) = normaliser(a, b);
        let params: [&dyn ToSqlValue; 2] = [&bas, &haut];
        db.execute(
            "INSERT INTO album_distinct_pairs (profile_id, album_a_id, album_b_id) \
             VALUES (1, ?, ?)",
            &params,
        )
        .unwrap();

        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.snapshots_backfilled, 1);
        let listed = repo.lister().unwrap();
        assert_eq!(listed[0].a_title, "Kind of Blue");

        // Contre-épreuve : l'instantané rattrapé fait bien son travail au
        // renouvellement d'ids suivant.
        db.execute("DELETE FROM albums", &[]).unwrap();
        let neuf_a = insert_album(&db, "Kind of Blue", ar);
        let neuf_b = insert_album(&db, "Kind of Blue (Mono)", ar);
        let stats = repo.reconcile(false).unwrap();
        assert_eq!(stats.relinked, 1);
        assert!(repo.sont_distincts(neuf_a, neuf_b).unwrap());
    }
}
