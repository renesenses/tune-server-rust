//! #4283 — le curseur de file ne bouge que vers une entrée qui EXISTE.
//!
//! `play_from_queue` (route `POST /{id}/queue/jump`, sondeur, `next`,
//! `previous`) et `advance_queue_metadata` (avance gapless) écrivaient
//! `set_current_pos(position)` AVANT de lire `get_at(position)`. Or
//! `set_current_pos` efface `is_current` sur TOUTE la zone puis le pose sur la
//! ligne à `position` : quand cette ligne n'existe pas, la zone perd son
//! curseur — plus aucune ligne courante — et l'erreur arrive après la mutation.
//!
//! Les témoins relisent la table : c'est l'ORDRE lecture-puis-écriture qui est
//! en jeu, et il se mesure sur ce que la base garde après un refus.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

use super::PlaybackOrchestrator;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Une zone et une file de `n` pistes locales dont la première est courante
/// (c'est ce que `set_queue` pose).
fn zone_avec_une_file(n: usize) -> (PlaybackOrchestrator, i64) {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone 4283", Some("local"), None)
        .unwrap();
    let pistes = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
    let mut ids = Vec::new();
    for i in 1..=n {
        let mut piste = crate::db::models::Track::new(format!("Piste {i}"));
        piste.file_path = Some(format!("/aucun/chemin/4283/piste{i}.flac"));
        piste.track_number = i as i32;
        piste.duration_ms = 180_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    if !ids.is_empty() {
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_queue(zone_id, &ids)
            .unwrap();
    }
    (orch, zone_id)
}

/// La position de la ligne courante en base, `None` quand AUCUNE ligne ne
/// porte `is_current` — c'est exactement l'état que le défaut laissait.
fn curseur_en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> Option<i64> {
    PlayQueueRepo::with_backend(orch.db.clone())
        .get_ordered(zone_id)
        .unwrap()
        .into_iter()
        .find(|e| e.is_current)
        .map(|e| e.position)
}

/// Le refus DOIT porter la sentinelle, la position demandée et la longueur de
/// la file : c'est ce que la route traduit en réponse nommant le champ.
fn verifier_le_refus(erreur: &str, position: i64, longueur: i64) {
    assert!(
        erreur.starts_with(PlaybackOrchestrator::QUEUE_POSITION_OUT_OF_RANGE),
        "le refus doit porter la sentinelle `{}` : {erreur}",
        PlaybackOrchestrator::QUEUE_POSITION_OUT_OF_RANGE
    );
    assert_eq!(
        erreur,
        format!(
            "{}{position}:{longueur}",
            PlaybackOrchestrator::QUEUE_POSITION_OUT_OF_RANGE
        ),
        "le refus doit nommer la position demandée et la longueur de la file"
    );
}

/// Le fait de l'issue : un saut hors bornes est refusé en nommant la position
/// et la longueur, ET le curseur n'a pas bougé — la piste 0 est toujours la
/// ligne courante. Négatif compris.
#[tokio::test]
async fn un_saut_hors_bornes_est_refuse_et_laisse_le_curseur_en_place() {
    let (orch, zone_id) = zone_avec_une_file(3);
    assert_eq!(curseur_en_base(&orch, zone_id), Some(0));

    for position in [3_i64, 4, 1_000, -1, i64::MIN, i64::MAX] {
        let erreur = orch
            .play_from_queue(zone_id, position)
            .await
            .err()
            .unwrap_or_else(|| {
                panic!("la position {position} n'existe pas : le saut doit échouer")
            });
        verifier_le_refus(&erreur, position, 3);
        assert_eq!(
            curseur_en_base(&orch, zone_id),
            Some(0),
            "après un saut refusé vers {position} le curseur a été DÉPLACÉ : \
             `set_current_pos` a été écrit avant `get_at` (#4283)"
        );
    }
}

/// File VIDE : refus nommant une longueur de 0, pas de panique, et la table
/// reste vide (rien à déplacer, rien d'écrit).
#[tokio::test]
async fn un_saut_sur_une_file_vide_est_refuse_sans_paniquer() {
    let (orch, zone_id) = zone_avec_une_file(0);
    assert_eq!(curseur_en_base(&orch, zone_id), None);

    for position in [0_i64, 1, -1] {
        let erreur = orch
            .play_from_queue(zone_id, position)
            .await
            .err()
            .unwrap_or_else(|| panic!("une file vide n'a pas de position {position}"));
        verifier_le_refus(&erreur, position, 0);
    }
    assert_eq!(curseur_en_base(&orch, zone_id), None);
}

/// CONTRE-ÉPREUVE du garde-fou : un saut VALIDE déplace bien le curseur et
/// engage la lecture. Sans elle, une garde qui refuserait tout resterait
/// verte et la file serait morte.
///
/// La lecture elle-même échoue ici (aucun fichier sous `/aucun/chemin`), mais
/// elle a été ENGAGÉE : `update_queue_info` a publié la position dans l'état
/// de la zone avant `play()`, et le motif rendu n'est pas celui du refus.
#[tokio::test]
async fn un_saut_valide_deplace_le_curseur_et_engage_la_lecture() {
    let (orch, zone_id) = zone_avec_une_file(3);

    let resultat = orch.play_from_queue(zone_id, 2).await;
    if let Err(erreur) = &resultat {
        assert!(
            !erreur.starts_with(PlaybackOrchestrator::QUEUE_POSITION_OUT_OF_RANGE),
            "la position 2 existe dans une file de 3 : elle ne doit pas être refusée — {erreur}"
        );
    }
    assert_eq!(
        curseur_en_base(&orch, zone_id),
        Some(2),
        "un saut valide doit déplacer le curseur"
    );
    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(
        etat.queue_position, 2,
        "la position doit être publiée à la zone"
    );
    assert_eq!(etat.queue_length, 3);
}

/// La JUMELLE : `advance_queue_metadata` écrivait le curseur de la même
/// façon. Hors bornes ⇒ refus nommé, curseur intact ; valide ⇒ `Ok` et le
/// curseur suit.
#[tokio::test]
async fn l_avance_gapless_lit_l_entree_avant_d_ecrire_le_curseur() {
    let (orch, zone_id) = zone_avec_une_file(2);

    for position in [2_i64, -1] {
        let erreur = orch
            .advance_queue_metadata(zone_id, position)
            .await
            .expect_err("la position n'existe pas : l'avance doit échouer");
        verifier_le_refus(&erreur, position, 2);
        assert_eq!(
            curseur_en_base(&orch, zone_id),
            Some(0),
            "après une avance refusée vers {position} le curseur a été DÉPLACÉ (#4283)"
        );
    }

    orch.advance_queue_metadata(zone_id, 1)
        .await
        .expect("la position 1 existe : l'avance doit aboutir");
    assert_eq!(curseur_en_base(&orch, zone_id), Some(1));
}
