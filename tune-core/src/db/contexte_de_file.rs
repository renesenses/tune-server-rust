//! Le CONTENEUR de chaque élément de file (#6466, FabienM, fil 2302 points
//! 10, 11, 15 ; migration SQLite 128 / PG 092).
//!
//! Jusqu'ici, l'historique écrivait le contexte de SESSION de la zone : celui
//! du dernier « Lire ». Tout ce qui entrait dans la file autrement — « Lire
//! ensuite », « Ajouter à la file », un transfert de zone — héritait de ce
//! contexte, à tort :
//!
//! * une playlist ajoutée à la file pendant un album s'écrivait DANS l'album ;
//! * un titre seul ajouté à la file aussi, au lieu du premier niveau ;
//! * un transfert vers une autre zone sortait les titres du conteneur, la
//!   zone cible n'ayant aucun contexte de session.
//!
//! Chaque ligne de `queue_items` porte donc désormais le conteneur dont elle a
//! été enfilée (`context_type` = `album`, `playlist`, `artist`, `label`, ou
//! `track` pour un titre ajouté seul). NULL = la ligne suit le contexte de
//! session, comme avant : c'est le cas de tout ce que pose `POST
//! /zones/{id}/play`, dont la session EST le conteneur.
//!
//! Le conteneur n'entre dans l'historique qu'à l'écoute d'un de ses titres :
//! l'historique s'écrit par écoute, et une file vidée avant d'y arriver n'a
//! rien écrit.

use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Le conteneur d'un élément de file, dans le vocabulaire de
/// `listen_history.context_*`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContexteDeFile {
    pub nature: String,
    pub id: Option<String>,
    pub service: Option<String>,
    pub titre: Option<String>,
    pub pochette: Option<String>,
}

impl ContexteDeFile {
    /// Un titre ajouté SEUL : premier niveau dans l'historique.
    pub fn titre_seul() -> Self {
        Self {
            nature: "track".to_string(),
            ..Default::default()
        }
    }
}

/// L'élément à une position, avec ce qui permet de vérifier que c'est bien la
/// piste jouée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementContextualise {
    pub contexte: Option<ContexteDeFile>,
    pub track_id: Option<i64>,
    pub source_id: Option<String>,
}

fn ph(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    }
}

fn texte(v: Option<&SqlValue>) -> Option<String> {
    v.and_then(|v| v.as_string()).filter(|s| !s.is_empty())
}

fn contexte_de_la_ligne(r: &[SqlValue], depuis: usize) -> Option<ContexteDeFile> {
    Some(ContexteDeFile {
        nature: texte(r.get(depuis))?,
        id: texte(r.get(depuis + 1)),
        service: texte(r.get(depuis + 2)),
        titre: texte(r.get(depuis + 3)),
        pochette: texte(r.get(depuis + 4)),
    })
}

/// Le plus grand identifiant de ligne de la zone (0 si la file est vide) :
/// tout ce qu'un ajout insère ensuite a un identifiant plus grand.
pub fn dernier_identifiant(db: &Arc<dyn DbBackend>, zone_id: i64) -> i64 {
    db.query_one(
        &format!(
            "SELECT COALESCE(MAX(id), 0) FROM queue_items WHERE zone_id = {}",
            ph(db.engine(), 1)
        ),
        &[&zone_id as &dyn ToSqlValue],
    )
    .ok()
    .flatten()
    .and_then(|r| r.first().and_then(|v| v.as_i64()))
    .unwrap_or(0)
}

/// Pose `contexte` sur les lignes de la zone d'identifiant > `apres_id` —
/// celles qu'un ajout vient d'insérer. Rend le nombre de lignes posées.
pub fn poser_apres(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    apres_id: i64,
    contexte: &ContexteDeFile,
) -> Result<usize, String> {
    let e = db.engine();
    let sql = format!(
        "UPDATE queue_items SET context_type = {}, context_id = {}, context_source = {}, \
         context_title = {}, context_cover = {} WHERE zone_id = {} AND id > {}",
        ph(e, 1),
        ph(e, 2),
        ph(e, 3),
        ph(e, 4),
        ph(e, 5),
        ph(e, 6),
        ph(e, 7)
    );
    db.execute(
        &sql,
        &[
            &contexte.nature as &dyn ToSqlValue,
            &contexte.id,
            &contexte.service,
            &contexte.titre,
            &contexte.pochette,
            &zone_id,
            &apres_id,
        ],
    )
}

/// L'élément à `position` dans la file de la zone — même départage que
/// `get_at` (`ORDER BY position, id`).
pub fn element_a_la_position(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    position: i64,
) -> Option<ElementContextualise> {
    let e = db.engine();
    let r = db
        .query_one(
            &format!(
                "SELECT track_id, source_id, context_type, context_id, context_source, \
                 context_title, context_cover FROM queue_items \
                 WHERE zone_id = {} AND position = {} ORDER BY position, id LIMIT 1",
                ph(e, 1),
                ph(e, 2)
            ),
            &[&zone_id as &dyn ToSqlValue, &position],
        )
        .ok()
        .flatten()?;
    Some(ElementContextualise {
        track_id: r.first().and_then(|v| v.as_i64()),
        source_id: texte(r.get(1)),
        contexte: contexte_de_la_ligne(&r, 2),
    })
}

/// Le conteneur de la piste jouée à `position`, si la ligne en porte un ET
/// désigne bien cette piste (`track_id`, sinon `source_id`). Une ligne qui ne
/// correspond pas — file réécrite entre-temps, curseur en retard — ne prête
/// pas son conteneur : l'appelant garde le contexte de session.
pub fn contexte_de_la_piste_jouee(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    position: i64,
    track_id: Option<i64>,
    source_id: Option<&str>,
) -> Option<ContexteDeFile> {
    let el = element_a_la_position(db, zone_id, position)?;
    let meme_piste = match (track_id, el.track_id) {
        (Some(a), Some(b)) => a == b,
        (None, None) => source_id.is_some() && source_id == el.source_id.as_deref(),
        _ => false,
    };
    if !meme_piste {
        return None;
    }
    el.contexte
}

/// Clé d'appariement d'une ligne d'une zone à l'autre.
type Cle = (Option<i64>, Option<String>);

fn lignes(db: &Arc<dyn DbBackend>, zone_id: i64) -> Vec<(i64, Cle, Option<ContexteDeFile>)> {
    db.query_many(
        &format!(
            "SELECT id, track_id, source_id, context_type, context_id, context_source, \
             context_title, context_cover FROM queue_items WHERE zone_id = {} \
             ORDER BY position, id",
            ph(db.engine(), 1)
        ),
        &[&zone_id as &dyn ToSqlValue],
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|r| {
        let id = r.first().and_then(|v| v.as_i64())?;
        let cle = (r.get(1).and_then(|v| v.as_i64()), texte(r.get(2)));
        Some((id, cle, contexte_de_la_ligne(r, 3)))
    })
    .collect()
}

/// Les conteneurs de la file de la zone, dans l'ordre de la file, avec la
/// piste de chaque ligne — de quoi les reposer ailleurs ([`poser_par_appariement`]).
pub fn contextes_de_la_file(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
) -> Vec<(Option<i64>, Option<String>, Option<ContexteDeFile>)> {
    lignes(db, zone_id)
        .into_iter()
        .map(|(_, (track_id, source_id), ctx)| (track_id, source_id, ctx))
        .collect()
}

/// Repose des conteneurs sur la file de `zone_id`, ligne par ligne, en
/// appariant par piste (`track_id` ou `source_id`) dans l'ordre de la file :
/// une piste présente deux fois garde ses deux conteneurs, une piste absente
/// (sautée à la copie) ne décale rien. Rend le nombre de lignes posées.
pub fn poser_par_appariement(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    contextes: Vec<(Option<i64>, Option<String>, Option<ContexteDeFile>)>,
) -> usize {
    use std::collections::{HashMap, VecDeque};
    let mut par_cle: HashMap<Cle, VecDeque<Option<ContexteDeFile>>> = HashMap::new();
    for (track_id, source_id, ctx) in contextes {
        par_cle
            .entry((track_id, source_id))
            .or_default()
            .push_back(ctx);
    }
    let e = db.engine();
    let sql = format!(
        "UPDATE queue_items SET context_type = {}, context_id = {}, context_source = {}, \
         context_title = {}, context_cover = {} WHERE id = {}",
        ph(e, 1),
        ph(e, 2),
        ph(e, 3),
        ph(e, 4),
        ph(e, 5),
        ph(e, 6)
    );
    let mut posees = 0;
    for (id, cle, _) in lignes(db, zone_id) {
        let Some(Some(ctx)) = par_cle.get_mut(&cle).and_then(|q| q.pop_front()) else {
            continue;
        };
        if db
            .execute(
                &sql,
                &[
                    &ctx.nature as &dyn ToSqlValue,
                    &ctx.id,
                    &ctx.service,
                    &ctx.titre,
                    &ctx.pochette,
                    &id,
                ],
            )
            .is_ok()
        {
            posees += 1;
        }
    }
    posees
}

/// Transfert de zone (#6466, point 15) : recopie le conteneur de chaque ligne
/// de `depuis` sur la ligne correspondante de `vers`, dont la file vient
/// d'être copiée. Rend le nombre de lignes posées.
pub fn copier_les_contextes(db: &Arc<dyn DbBackend>, depuis: i64, vers: i64) -> usize {
    poser_par_appariement(db, vers, contextes_de_la_file(db, depuis))
}

// Témoins : `orchestrator/historique_conteneurs_tests_6466.rs`.
