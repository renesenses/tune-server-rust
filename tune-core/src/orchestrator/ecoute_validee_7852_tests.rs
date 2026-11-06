//! #7852 — témoins de l'écoute validée au seuil min(30 s, 50 %).
//!
//! Chaque parcours tourne sur SQLite, et sur PostgreSQL quand
//! `TUNE_TEST_PG_URL` est posée (fonctionnalité `postgres`).
//!
//! Le parcours arme l'écoute comme les trois lanceurs (`record_listen`), fait
//! avancer la zone (`playback.update_position`, ce que fait le sondeur), puis
//! appelle le point de décision (`valider_l_ecoute`) comme le tour du sondeur.
//!
//! CONTRE-ÉPREUVE (mesurée) : remplacer le corps de `ecoute_validee` par
//! `true` (ce qui revient à écrire au démarrage, la conduite d'avant) fait
//! rougir six témoins : `zapping_a_5_s_rien_n_est_ecrit_7852`,
//! `titre_de_40_s_coupe_a_19_s_rien_n_est_ecrit_7852`,
//! `trente_et_une_s_d_un_titre_de_4_min_est_ecrit_7852` (29 s ne comptent
//! pas), `vingt_et_une_s_d_un_titre_de_40_s_est_ecrit_7852` (19 s ne
//! comptent pas), `transfert_de_zone_une_seule_ecoute_7852` et
//! `le_seuil_est_min_30_s_ou_la_moitie_7852`.
use std::sync::Arc;

use tokio::sync::Mutex;

use super::ecoute_validee_7852::{ecoute_validee, seuil_d_ecoute_ms};
use super::{ContexteEcoute, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;

const SALON: i64 = 78521;
const CUISINE: i64 = 78522;
const ALBUM: &str = "album-7852";

fn orchestrateur(db: Arc<dyn DbBackend>) -> PlaybackOrchestrator {
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    for (id, nom) in [(SALON, "Salon"), (CUISINE, "Cuisine")] {
        db.execute(
            "INSERT INTO zones (id, name) VALUES (?, ?)",
            &[&id as &dyn ToSqlValue, &nom],
        )
        .unwrap();
    }
    db
}

fn piste(titre: &str, duree_ms: i64) -> NowPlaying {
    NowPlaying {
        title: titre.into(),
        artist_name: Some("Artiste".into()),
        album_title: Some("Disque".into()),
        duration_ms: duree_ms,
        source: "qobuz".into(),
        source_id: Some(format!("qobuz-{titre}")),
        ..Default::default()
    }
}

/// La piste dans la file de la zone : sans elle, « Reprendre l'écoute » n'a
/// rien à rouvrir et le témoin de la reprise ne prouverait rien.
fn en_file(db: &Arc<dyn DbBackend>, zone_id: i64, np: &NowPlaying) {
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[QueueInput::Streaming {
                source: np.source.clone(),
                source_id: np.source_id.clone().unwrap(),
                title: np.title.clone(),
                artist: "Artiste".into(),
                album: None,
                cover_url: None,
                duration_ms: np.duration_ms,
                track_number: None,
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            }],
        )
        .unwrap();
}

/// Ce que fait un lanceur (`play_inner`, avance sans blanc, zone navigateur) :
/// la zone joue la piste, et l'écoute est armée.
async fn lancer(orch: &PlaybackOrchestrator, zone_id: i64, np: &NowPlaying) {
    en_file(&orch.db, zone_id, np);
    orch.playback.play(zone_id, np.clone()).await;
    orch.record_listen(
        &np.title,
        np.artist_name.as_deref(),
        np.album_title.as_deref(),
        &np.source,
        np.source_id.as_deref(),
        None,
        np.duration_ms,
        zone_id,
        None,
        None,
        ContexteEcoute {
            nature: Some("album"),
            id: Some(ALBUM),
            rang: Some(0),
            service: Some("qobuz"),
            titre: Some("Disque"),
            pochette: None,
        },
        None,
    );
}

/// Un tour du sondeur : la zone a avancé jusqu'à `position_ms`, puis le point
/// de décision est consulté.
async fn avancer(orch: &PlaybackOrchestrator, zone_id: i64, position_ms: i64) -> bool {
    orch.playback.update_position(zone_id, position_ms).await;
    let etat = orch.playback.get_state(zone_id).await;
    orch.valider_l_ecoute(&etat)
}

fn ecoutes(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

fn reprises(db: &Arc<dyn DbBackend>) -> i64 {
    db.query_one("SELECT COUNT(*) FROM listening_resumes", &[])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

// ─── Parcours, communs aux deux moteurs ─────────────────────────────────────

async fn parcours_zapping(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    // Trois titres de l'album zappés à 5 s : rien ne doit rester.
    for titre in ["Un", "Deux", "Trois"] {
        lancer(&orch, SALON, &piste(titre, 240_000)).await;
        avancer(&orch, SALON, 5_000).await;
    }
    orch.playback.stop(SALON).await;
    assert_eq!(
        ecoutes(&db),
        0,
        "#7852 : un titre zappé à 5 s ne doit écrire aucune ligne d'historique \
         (donc aucune écoute comptée)"
    );
    assert_eq!(
        reprises(&db),
        0,
        "#7852 : un album dont tous les titres sont zappés ne doit laisser aucune \
         entrée « Reprendre l'écoute » (conteneur fantôme)"
    );
}

async fn parcours_31_s_sur_4_min(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    lancer(&orch, SALON, &piste("Long", 240_000)).await;
    avancer(&orch, SALON, 29_000).await;
    assert_eq!(
        ecoutes(&db),
        0,
        "#7852 : 29 s d'un titre de 4 min, sous le seuil de 30 s, ne comptent pas d'écoute"
    );
    assert!(
        avancer(&orch, SALON, 31_000).await,
        "31 s sur 4 min : validée"
    );
    assert!(
        !avancer(&orch, SALON, 200_000).await,
        "validée une seule fois"
    );
    assert_eq!(ecoutes(&db), 1, "31 s d'un titre de 4 min : une écoute");
    assert_eq!(
        reprises(&db),
        1,
        "le conteneur entre dans « Reprendre l'écoute » avec son premier titre validé"
    );
    let contexte = db
        .query_one("SELECT context_type, context_id FROM listen_history", &[])
        .unwrap()
        .unwrap();
    assert_eq!(contexte[0].as_string().as_deref(), Some("album"));
    assert_eq!(contexte[1].as_string().as_deref(), Some(ALBUM));
}

async fn parcours_21_s_sur_40_s(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    lancer(&orch, SALON, &piste("Court", 40_000)).await;
    avancer(&orch, SALON, 19_000).await;
    assert_eq!(
        ecoutes(&db),
        0,
        "#7852 : 19 s d'un titre de 40 s, sous la moitié, ne comptent pas d'écoute"
    );
    assert!(avancer(&orch, SALON, 21_000).await);
    assert_eq!(ecoutes(&db), 1, "21 s d'un titre de 40 s : une écoute");
}

async fn parcours_coupe_a_19_s(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    lancer(&orch, SALON, &piste("Court", 40_000)).await;
    avancer(&orch, SALON, 19_000).await;
    // Le titre suivant arme sa propre écoute : celle du titre coupé s'en va.
    lancer(&orch, SALON, &piste("Suivant", 40_000)).await;
    avancer(&orch, SALON, 2_000).await;
    assert_eq!(
        ecoutes(&db),
        0,
        "#7852 : un titre de 40 s coupé à 19 s ne doit écrire aucune écoute"
    );
}

async fn parcours_transfert(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    let np = piste("Voyageur", 240_000);

    // Transfert AVANT le seuil : l'écoute suit le titre sur la cible.
    lancer(&orch, SALON, &np).await;
    avancer(&orch, SALON, 10_000).await;
    en_file(&orch.db, CUISINE, &np);
    orch.playback.play(CUISINE, np.clone()).await;
    orch.transferer_l_ecoute_en_attente(SALON, CUISINE);
    // La source continue un instant (transfert sans arrêt) : elle ne compte plus.
    avancer(&orch, SALON, 40_000).await;
    assert_eq!(
        ecoutes(&db),
        0,
        "#7852 : après le transfert, la zone d'origine ne compte plus le titre transféré"
    );
    assert!(
        avancer(&orch, CUISINE, 31_000).await,
        "#7852 : la zone cible valide l'écoute transférée au seuil"
    );
    avancer(&orch, CUISINE, 90_000).await;
    assert_eq!(
        ecoutes(&db),
        1,
        "#7852 : un titre transféré avant le seuil compte une seule écoute"
    );

    // Transfert APRÈS le seuil : déjà écrite sur l'origine, rien de plus.
    let np2 = piste("Voyageur 2", 240_000);
    lancer(&orch, SALON, &np2).await;
    assert!(avancer(&orch, SALON, 45_000).await);
    orch.playback.play(CUISINE, np2.clone()).await;
    orch.transferer_l_ecoute_en_attente(SALON, CUISINE);
    assert!(!avancer(&orch, CUISINE, 120_000).await);
    assert_eq!(
        ecoutes(&db),
        2,
        "#7852 : un titre transféré après le seuil n'est pas compté une seconde fois"
    );
}

async fn parcours_sans_duree(db: Arc<dyn DbBackend>) {
    let orch = orchestrateur(db.clone());
    // Flux sans durée connue : la règle d'avant, écrit au démarrage.
    lancer(&orch, SALON, &piste("Flux", 0)).await;
    assert_eq!(
        ecoutes(&db),
        1,
        "un flux sans durée garde l'écriture au démarrage"
    );
    assert!(!orch.ecoute_en_attente(SALON));
}

// ─── La règle ───────────────────────────────────────────────────────────────

#[test]
fn le_seuil_est_min_30_s_ou_la_moitie_7852() {
    assert_eq!(seuil_d_ecoute_ms(240_000), Some(30_000));
    assert_eq!(seuil_d_ecoute_ms(40_000), Some(20_000));
    assert_eq!(seuil_d_ecoute_ms(0), None);
    assert!(!ecoute_validee(240_000, 5_000), "zapping à 5 s");
    assert!(ecoute_validee(240_000, 31_000), "31 s sur 4 min");
    assert!(ecoute_validee(40_000, 21_000), "21 s sur 40 s");
    assert!(!ecoute_validee(40_000, 19_000), "19 s sur 40 s");
    assert!(ecoute_validee(0, 0), "durée inconnue : règle d'avant");
}

/// Le sondeur consulte bien le point de décision à chaque tour : hors de la
/// ligne de définition, `valider_l_ecoute(` doit être appelé dans `tick.rs`.
#[test]
fn le_sondeur_consulte_le_point_de_decision_7852() {
    let tick = include_str!("../poller/tick.rs");
    let appels = tick
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("orchestrator.valider_l_ecoute("))
        .count();
    assert_eq!(appels, 1, "tick.rs doit appeler valider_l_ecoute une fois");
}

// ─── SQLite ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn zapping_a_5_s_rien_n_est_ecrit_7852() {
    parcours_zapping(sqlite()).await;
}

#[tokio::test]
async fn trente_et_une_s_d_un_titre_de_4_min_est_ecrit_7852() {
    parcours_31_s_sur_4_min(sqlite()).await;
}

#[tokio::test]
async fn vingt_et_une_s_d_un_titre_de_40_s_est_ecrit_7852() {
    parcours_21_s_sur_40_s(sqlite()).await;
}

#[tokio::test]
async fn titre_de_40_s_coupe_a_19_s_rien_n_est_ecrit_7852() {
    parcours_coupe_a_19_s(sqlite()).await;
}

#[tokio::test]
async fn transfert_de_zone_une_seule_ecoute_7852() {
    parcours_transfert(sqlite()).await;
}

#[tokio::test]
async fn flux_sans_duree_ecrit_au_demarrage_7852() {
    parcours_sans_duree(sqlite()).await;
}

// ─── PostgreSQL ─────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use super::*;

    /// Une connexion unique et des tables TEMPORAIRES calquées sur le schéma
    /// réel : le banc ne touche aucune donnée de la base de test.
    async fn pg() -> Option<(Arc<dyn DbBackend>, sqlx::PgPool)> {
        let url = std::env::var("TUNE_TEST_PG_URL").ok()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée mais connexion refusée : {e}"));
        let db: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        for table in ["listen_history", "listening_resumes", "queue_items"] {
            db.execute(
                &format!("CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL)"),
                &[],
            )
            .unwrap_or_else(|e| panic!("table temporaire {table} : {e}"));
        }
        Some((db, pool))
    }

    macro_rules! sur_pg {
        ($nom:ident, $parcours:ident) => {
            #[tokio::test(flavor = "multi_thread")]
            async fn $nom() {
                let Some((db, pool)) = pg().await else {
                    eprintln!("SAUT : TUNE_TEST_PG_URL absente, PostgreSQL non exercé");
                    return;
                };
                $parcours(db).await;
                pool.close().await;
            }
        };
    }

    sur_pg!(pg_zapping_a_5_s_rien_n_est_ecrit_7852, parcours_zapping);
    sur_pg!(
        pg_trente_et_une_s_d_un_titre_de_4_min_est_ecrit_7852,
        parcours_31_s_sur_4_min
    );
    sur_pg!(
        pg_vingt_et_une_s_d_un_titre_de_40_s_est_ecrit_7852,
        parcours_21_s_sur_40_s
    );
    sur_pg!(
        pg_titre_de_40_s_coupe_a_19_s_rien_n_est_ecrit_7852,
        parcours_coupe_a_19_s
    );
    sur_pg!(
        pg_transfert_de_zone_une_seule_ecoute_7852,
        parcours_transfert
    );
    sur_pg!(
        pg_flux_sans_duree_ecrit_au_demarrage_7852,
        parcours_sans_duree
    );
}
