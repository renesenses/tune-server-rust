use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Une correction que la communaute propose sur une metadonnee de cette
/// instance.
///
/// Local d'abord, comme les signalements : la ligne fait foi. L'utilisateur
/// tranche, l'effet local s'applique tout de suite, et le renvoi de la decision
/// au cloud vient par-dessus. Une decision prise hors ligne n'est pas perdue —
/// elle repart au cycle suivant.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataProposal {
    pub id: i64,
    /// Toujours "album" pour l'instant. Le champ existe pour que l'ouverture
    /// aux pistes ne demande pas une migration.
    pub entity: String,
    /// Identifiant de l'entite canonique cote cloud — la cle de la decision.
    pub cloud_entity_id: i64,
    /// Identifiant de la ligne LOCALE a corriger.
    pub local_id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub field: String,
    pub current_value: Option<String>,
    pub proposed_value: Option<String>,
    /// Combien de serveurs portent la valeur proposee. C'est ce qui donne son
    /// poids a la proposition dans l'interface.
    pub servers_count: i64,
    pub fetched_at: String,
    /// NULL tant que l'utilisateur n'a pas tranche.
    pub decision: Option<String>,
    pub decided_at: Option<String>,
    pub pushed_at: Option<String>,
}

/// Les champs qu'une proposition peut viser.
///
/// Liste fermee, cote client aussi : le cloud a deja la sienne, mais un client
/// qui appliquerait aveuglement ce qu'on lui envoie ferait dependre l'integrite
/// de la bibliotheque d'un serveur distant. Deux verrous valent mieux qu'un.
///
/// `track_count` en est absent volontairement. Mesure du 2026-08-12 : deux
/// propositions sur trois portant sur ce champ visaient un serveur qui avait
/// PLUS de pistes que la majorite — l'edition deluxe contre l'edition standard.
/// Un ecart de pistes n'est pas une faute de metadonnee.
pub const PROPOSABLE_FIELDS: &[&str] = &["year"];

/// Engine-agnostic SQL builders for metadata_proposal_repo.
pub mod sql {
    use super::SqlDialect;

    pub const SELECT_COLS: &str = "id, entity, cloud_entity_id, local_id, title, artist, field, \
                                   current_value, proposed_value, servers_count, fetched_at, \
                                   decision, decided_at, pushed_at";

    /// Insere une proposition, ou rafraichit celle qui existe deja.
    ///
    /// `ON CONFLICT` et non un `DELETE` prealable : ecraser la table a chaque
    /// cycle effacerait les decisions deja prises mais pas encore renvoyees au
    /// cloud. On ne remet donc a jour que ce qui vient du cloud, jamais la
    /// reponse de l'utilisateur.
    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO metadata_proposals \
             (entity, cloud_entity_id, local_id, title, artist, field, current_value, \
              proposed_value, servers_count, fetched_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (entity, cloud_entity_id, field) DO UPDATE SET \
                local_id = excluded.local_id, \
                title = excluded.title, \
                artist = excluded.artist, \
                current_value = excluded.current_value, \
                proposed_value = excluded.proposed_value, \
                servers_count = excluded.servers_count, \
                fetched_at = excluded.fetched_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10)
        )
    }

    /// Les propositions en attente, les plus largement soutenues d'abord.
    pub fn list_pending<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {SELECT_COLS} FROM metadata_proposals WHERE decision IS NULL \
             ORDER BY servers_count DESC, id LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn get<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {SELECT_COLS} FROM metadata_proposals WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn decide<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE metadata_proposals SET decision = {}, decided_at = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    /// Les decisions que le cloud n'a pas encore recues.
    pub fn list_undelivered<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {SELECT_COLS} FROM metadata_proposals \
             WHERE decision IS NOT NULL AND pushed_at IS NULL ORDER BY id LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn mark_pushed<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE metadata_proposals SET pushed_at = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_pending() -> String {
        "SELECT count(*) FROM metadata_proposals WHERE decision IS NULL".to_string()
    }
}

pub struct MetadataProposalRepo {
    db: Arc<dyn DbBackend>,
}

impl MetadataProposalRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

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

    /// Enregistre une proposition venue du cloud.
    ///
    /// Un champ hors de `PROPOSABLE_FIELDS` est refuse ici, avant d'atteindre
    /// la base : c'est le second verrou.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        entity: &str,
        cloud_entity_id: i64,
        local_id: i64,
        title: Option<&str>,
        artist: Option<&str>,
        field: &str,
        current_value: Option<&str>,
        proposed_value: Option<&str>,
        servers_count: i64,
        fetched_at: &str,
    ) -> Result<(), String> {
        if !PROPOSABLE_FIELDS.contains(&field) {
            return Err(format!("champ non proposable : {field}"));
        }

        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 10] = [
            &entity,
            &cloud_entity_id,
            &local_id,
            &title,
            &artist,
            &field,
            &current_value,
            &proposed_value,
            &servers_count,
            &fetched_at,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    fn rows_to_proposals(rows: Vec<Vec<super::backend::SqlValue>>) -> Vec<MetadataProposal> {
        rows.into_iter()
            .filter_map(|cols| {
                Some(MetadataProposal {
                    id: cols.first().and_then(|v| v.as_i64())?,
                    entity: cols.get(1).and_then(|v| v.as_string())?,
                    cloud_entity_id: cols.get(2).and_then(|v| v.as_i64())?,
                    local_id: cols.get(3).and_then(|v| v.as_i64())?,
                    title: cols.get(4).and_then(|v| v.as_string()),
                    artist: cols.get(5).and_then(|v| v.as_string()),
                    field: cols.get(6).and_then(|v| v.as_string())?,
                    current_value: cols.get(7).and_then(|v| v.as_string()),
                    proposed_value: cols.get(8).and_then(|v| v.as_string()),
                    servers_count: cols.get(9).and_then(|v| v.as_i64()).unwrap_or(0),
                    fetched_at: cols.get(10).and_then(|v| v.as_string()).unwrap_or_default(),
                    decision: cols.get(11).and_then(|v| v.as_string()),
                    decided_at: cols.get(12).and_then(|v| v.as_string()),
                    pushed_at: cols.get(13).and_then(|v| v.as_string()),
                })
            })
            .collect()
    }

    /// Les propositions en attente de reponse, les mieux soutenues d'abord.
    pub fn list_pending(&self, limit: i64) -> Result<Vec<MetadataProposal>, String> {
        let sql = self.dialect_sql(sql::list_pending, sql::list_pending);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        Ok(Self::rows_to_proposals(self.db.query_many(&sql, &params)?))
    }

    pub fn get(&self, id: i64) -> Result<Option<MetadataProposal>, String> {
        let sql = self.dialect_sql(sql::get, sql::get);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(Self::rows_to_proposals(self.db.query_many(&sql, &params)?)
            .into_iter()
            .next())
    }

    pub fn decide(&self, id: i64, decision: &str, decided_at: &str) -> Result<(), String> {
        if decision != "accepted" && decision != "refused" {
            return Err(format!("decision inconnue : {decision}"));
        }
        let sql = self.dialect_sql(sql::decide, sql::decide);
        let params: [&dyn ToSqlValue; 3] = [&decision, &decided_at, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Les decisions que le cloud n'a pas encore recues, les plus anciennes
    /// d'abord.
    pub fn list_undelivered(&self, limit: i64) -> Result<Vec<MetadataProposal>, String> {
        let sql = self.dialect_sql(sql::list_undelivered, sql::list_undelivered);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        Ok(Self::rows_to_proposals(self.db.query_many(&sql, &params)?))
    }

    pub fn mark_pushed(&self, id: i64, pushed_at: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::mark_pushed, sql::mark_pushed);
        let params: [&dyn ToSqlValue; 2] = [&pushed_at, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn count_pending(&self) -> i64 {
        self.db
            .query_one(&sql::count_pending(), &[])
            .ok()
            .flatten()
            .and_then(|row| row.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn setup_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    fn repo() -> MetadataProposalRepo {
        MetadataProposalRepo::new(setup_db())
    }

    fn ajoute(repo: &MetadataProposalRepo, cloud_id: i64, servers: i64) {
        repo.upsert(
            "album",
            cloud_id,
            42,
            Some("The Wall"),
            Some("Pink Floyd"),
            "year",
            Some("1980"),
            Some("1979"),
            servers,
            "2026-08-12T10:00:00Z",
        )
        .unwrap();
    }

    #[test]
    fn upsert_puis_liste_en_attente() {
        let repo = repo();
        ajoute(&repo, 1, 3);

        let pending = repo.list_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].field, "year");
        assert_eq!(pending[0].current_value.as_deref(), Some("1980"));
        assert_eq!(pending[0].proposed_value.as_deref(), Some("1979"));
        assert_eq!(pending[0].local_id, 42);
        assert!(pending[0].decision.is_none());
    }

    #[test]
    fn les_mieux_soutenues_en_premier() {
        let repo = repo();
        ajoute(&repo, 1, 3);
        ajoute(&repo, 2, 9);
        ajoute(&repo, 3, 5);

        let pending = repo.list_pending(10).unwrap();
        assert_eq!(pending[0].servers_count, 9);
        assert_eq!(pending[2].servers_count, 3);
    }

    #[test]
    fn un_second_cycle_ne_duplique_pas_et_ne_perd_pas_la_decision() {
        // Le cas qui justifie l'ON CONFLICT : le cloud renvoie la meme
        // proposition au cycle suivant. Si elle ecrasait la ligne, la reponse
        // de l'utilisateur — pas encore remontee — serait perdue.
        let repo = repo();
        ajoute(&repo, 1, 3);
        let id = repo.list_pending(10).unwrap()[0].id;
        repo.decide(id, "refused", "2026-08-12T11:00:00Z").unwrap();

        ajoute(&repo, 1, 7);

        assert!(repo.list_pending(10).unwrap().is_empty());
        let row = repo.get(id).unwrap().unwrap();
        assert_eq!(row.decision.as_deref(), Some("refused"));
        // Le soutien, lui, s'est bien rafraichi.
        assert_eq!(row.servers_count, 7);
    }

    #[test]
    fn une_decision_attend_sa_remontee_puis_ne_revient_plus() {
        let repo = repo();
        ajoute(&repo, 1, 3);
        let id = repo.list_pending(10).unwrap()[0].id;

        repo.decide(id, "accepted", "2026-08-12T11:00:00Z").unwrap();
        let a_remonter = repo.list_undelivered(10).unwrap();
        assert_eq!(a_remonter.len(), 1);
        assert_eq!(a_remonter[0].decision.as_deref(), Some("accepted"));

        repo.mark_pushed(id, "2026-08-12T11:05:00Z").unwrap();
        assert!(repo.list_undelivered(10).unwrap().is_empty());
    }

    #[test]
    fn un_champ_hors_liste_est_refuse_avant_la_base() {
        // Le second verrou : meme si le cloud se met a proposer autre chose,
        // le client ne l'enregistre pas.
        let repo = repo();
        let err = repo
            .upsert(
                "album",
                1,
                42,
                None,
                None,
                "track_count",
                Some("15"),
                Some("12"),
                5,
                "2026-08-12T10:00:00Z",
            )
            .unwrap_err();

        assert!(err.contains("track_count"), "{err}");
        assert_eq!(repo.count_pending(), 0);
    }

    #[test]
    fn une_decision_inconnue_est_refusee() {
        let repo = repo();
        ajoute(&repo, 1, 3);
        let id = repo.list_pending(10).unwrap()[0].id;

        assert!(
            repo.decide(id, "peut-etre", "2026-08-12T11:00:00Z")
                .is_err()
        );
        assert_eq!(repo.count_pending(), 1);
    }
}
