//! La lecture d'abord — les propriétés du frein des traitements de fond (#4681).
//!
//! Le cas mesuré : #4567, trois micro-coupures sur Tune Endpoint (OAAT), dont
//! deux tombent à la seconde sur une ligne `replaygain_album … ecrites=15` —
//! une passe qui écrivait ~60 clés SQLite d'affilée en pleine écoute.
//!
//! Ce que ces témoins tiennent :
//!
//! 1. **La passe d'album ne s'exécute pas pendant qu'une zone joue**, et
//!    rattrape son album dès l'arrêt. Retirer la garde de `passe_d_album` fait
//!    rougir le premier témoin : le gain d'album s'écrit pendant la lecture.
//! 2. **Le témoin de lecture est branché sur le point qui écrit l'état des
//!    zones** (`ZoneRepo::save_play_state`) — pas sur un appel que les tests
//!    seraient seuls à faire. Une zone en PAUSE ne compte pas.
//! 3. **Les passes freinées marquent une pause entre deux éléments** tant
//!    qu'une zone joue (enrichissement, images d'artistes — par
//!    `attendre_son_tour` —, scan entre deux lots), et aucune au repos.
//! 4. **Les écritures partent hors du fil de l'exécuteur**.
//! 5. **Le relevé dit ce qui a cédé**, et se vide à l'arrêt.
//!
//! ## Pourquoi une cible `[[test]]` à elle seule
//!
//! `autotests = false` : sans l'entrée du manifeste, ce fichier ne serait
//! JAMAIS compilé. Et un binaire à lui seul : le témoin de lecture est un état
//! de PROCESSUS ; un voisin qui mettrait une zone en lecture dans le même
//! binaire rendrait ces témoins intermittents. Ils se sérialisent entre eux
//! pour la même raison.

use std::sync::Arc;
use std::time::Duration;

use tune_core::audio::replaygain::passe_d_album;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::taches_de_fond::priorite::{
    self, ID_SCAN, PAUSE_EN_LECTURE, ceder_a_la_lecture, hors_du_fil_async, lecture_en_cours,
    noter_etat_de_lecture, oublier_la_lecture_pour_les_essais, oublier_la_zone, releve,
};
use tune_core::taches_de_fond::{Tache, attendre_son_tour, oublier_pour_les_essais};

/// Le témoin de lecture et le masque de pause sont des états de PROCESSUS.
static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn a_neuf() {
    oublier_la_lecture_pour_les_essais();
    oublier_pour_les_essais();
}

/// Un album COMPLET (toutes ses pistes ont un gain de piste) et rien d'autre :
/// exactement ce que la passe d'album prend.
fn base_avec_un_album_pret() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(
        "CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                              duration_ms INTEGER, sample_rate INTEGER, channels INTEGER);
         CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                      value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
    )
    .expect("schéma");
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    for (id, gain) in [(1i64, "-6.00 dB"), (2, "-8.00 dB"), (3, "-7.00 dB")] {
        let chemin = format!("/album/{id}.flac");
        db.execute(
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels) \
             VALUES (?, 1, ?, 300000, 44100, 2)",
            &[&id, &chemin],
        )
        .expect("piste");
        meta.set(id, "rg_track_gain", gain).expect("gain de piste");
        meta.set(id, "rg_track_peak", "0.95").expect("pic de piste");
    }
    backend
}

fn gains_d_album(backend: &Arc<dyn DbBackend>) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM track_metadata WHERE key = 'rg_album_gain'",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// 🔴 LE CAS DE #4567 — la passe d'album écrivait en pleine lecture.
///
/// Zone en lecture : la passe ne prend RIEN, n'écrit RIEN, et se déclare au
/// relevé. Lecture arrêtée : la même passe prend l'album et l'écrit — le
/// retard est rattrapé, rien n'est perdu.
#[tokio::test]
async fn la_passe_d_album_attend_l_arret_de_la_lecture() {
    let _g = VERROU.lock().await;
    a_neuf();
    let backend = base_avec_un_album_pret();

    noter_etat_de_lecture(7, "playing");
    assert!(lecture_en_cours());
    assert_eq!(
        passe_d_album(&backend, false).await,
        0,
        "une zone joue : la passe d'album ne doit rien prendre"
    );
    assert_eq!(
        gains_d_album(&backend),
        0,
        "une zone joue : AUCUN gain d'album ne doit s'écrire (#4567)"
    );
    assert!(
        releve().throttled.contains(&Tache::ReplayGain.id()),
        "la passe s'est effacée : le relevé doit la nommer, sinon une coupure \
         ne peut pas être rapprochée de ce qui tournait"
    );

    noter_etat_de_lecture(7, "stopped");
    assert!(!lecture_en_cours());
    assert!(
        releve().throttled.is_empty(),
        "à l'arrêt, plus rien n'est ralenti"
    );
    assert_eq!(passe_d_album(&backend, false).await, 1);
    assert_eq!(
        gains_d_album(&backend),
        3,
        "lecture arrêtée : l'album est rattrapé, ses trois pistes portent leur gain"
    );
    a_neuf();
}

/// Contre-épreuve : le témoin EN BASE que la boucle ReplayGain lit
/// (`zones.last_play_state`) suffit lui aussi à faire céder la passe, même si
/// le témoin en mémoire n'a rien vu — les deux sources ne se contredisent pas
/// au détriment de la lecture.
#[tokio::test]
async fn le_temoin_en_base_suffit_aussi_a_faire_ceder_la_passe_d_album() {
    let _g = VERROU.lock().await;
    a_neuf();
    let backend = base_avec_un_album_pret();
    assert!(!lecture_en_cours());
    assert_eq!(passe_d_album(&backend, true).await, 0);
    assert_eq!(gains_d_album(&backend), 0);
    a_neuf();
}

/// Contre-épreuve : sans lecture, la passe d'album travaille au premier tour —
/// le frein ne doit pas la retenir quand personne n'écoute.
#[tokio::test]
async fn sans_lecture_la_passe_d_album_travaille_tout_de_suite() {
    let _g = VERROU.lock().await;
    a_neuf();
    let backend = base_avec_un_album_pret();
    assert_eq!(passe_d_album(&backend, false).await, 1);
    assert_eq!(gains_d_album(&backend), 3);
    assert!(releve().throttled.is_empty());
    a_neuf();
}

/// Le branchement RÉEL : c'est `ZoneRepo::save_play_state`, le point où
/// l'orchestrateur et les routes écrivent l'état d'une zone, qui alimente le
/// témoin. Une zone en PAUSE ne compte pas comme une lecture.
#[tokio::test]
async fn le_temoin_suit_l_etat_ecrit_par_le_depot_des_zones() {
    let _g = VERROU.lock().await;
    a_neuf();
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(
        "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT,
                             is_hidden INTEGER DEFAULT 0);
         INSERT INTO zones (id, name, last_play_state) VALUES (1, 'Salon', 'stopped'),
                                                            (2, 'Bureau', 'stopped');",
    )
    .expect("schéma");
    let zones = ZoneRepo::with_backend(Arc::new(db));

    zones.save_play_state(1, "playing").expect("écriture");
    assert!(lecture_en_cours(), "Salon joue");
    zones.save_play_state(2, "playing").expect("écriture");
    zones.save_play_state(1, "paused").expect("écriture");
    assert!(
        lecture_en_cours(),
        "Salon en pause, mais Bureau joue toujours : l'ENSEMBLE des zones compte"
    );
    assert_eq!(releve().playing_zone_ids, vec![2]);
    assert!(releve().since_epoch_s.is_some());

    zones.save_play_state(2, "stopped").expect("écriture");
    assert!(!lecture_en_cours(), "plus aucune zone ne joue");
    assert_eq!(releve().since_epoch_s, None);

    // Une zone supprimée en pleine lecture ne laisse pas les passes freinées.
    zones.save_play_state(1, "playing").expect("écriture");
    zones.delete(1).expect("suppression");
    assert!(!lecture_en_cours(), "zone supprimée : elle ne joue plus");
    a_neuf();
}

/// La même zone repasse par « playing » à chaque piste : l'ensemble ne doit
/// pas compter deux fois, sinon un seul arrêt ne rendrait jamais la main.
#[tokio::test]
async fn la_meme_zone_annoncee_deux_fois_ne_bloque_pas_la_reprise() {
    let _g = VERROU.lock().await;
    a_neuf();
    noter_etat_de_lecture(3, "playing");
    noter_etat_de_lecture(3, "playing");
    noter_etat_de_lecture(3, "stopped");
    assert!(!lecture_en_cours());
    noter_etat_de_lecture(4, "playing");
    oublier_la_zone(4);
    assert!(!lecture_en_cours());
    a_neuf();
}

/// Cadence réduite : pendant la lecture, la frontière d'une passe freinée
/// (enrichissement, images d'artistes) dort `PAUSE_EN_LECTURE` ; au repos, elle
/// ne coûte rien. Horloge virtuelle : ce qu'on mesure est le temps que la
/// passe AURAIT attendu.
#[tokio::test(start_paused = true)]
async fn la_frontiere_d_une_passe_freinee_marque_une_pause_pendant_la_lecture() {
    let _g = VERROU.lock().await;
    a_neuf();

    let t0 = tokio::time::Instant::now();
    attendre_son_tour(Tache::Enrichissement).await;
    assert_eq!(
        t0.elapsed(),
        Duration::ZERO,
        "au repos, la frontière ne doit rien coûter"
    );
    assert!(!ceder_a_la_lecture(Tache::ImagesArtistes.id()).await);

    noter_etat_de_lecture(9, "playing");
    let t1 = tokio::time::Instant::now();
    attendre_son_tour(Tache::Enrichissement).await;
    assert!(
        t1.elapsed() >= PAUSE_EN_LECTURE,
        "une zone joue : l'enrichissement doit marquer sa pause entre deux pistes \
         (attendu ≥ {PAUSE_EN_LECTURE:?}, mesuré {:?})",
        t1.elapsed()
    );
    assert!(ceder_a_la_lecture(Tache::ImagesArtistes.id()).await);
    let ralenties = releve().throttled;
    assert!(ralenties.contains(&Tache::Enrichissement.id()));
    assert!(ralenties.contains(&Tache::ImagesArtistes.id()));

    noter_etat_de_lecture(9, "stopped");
    let t2 = tokio::time::Instant::now();
    attendre_son_tour(Tache::Enrichissement).await;
    assert_eq!(
        t2.elapsed(),
        Duration::ZERO,
        "la lecture s'arrête : la passe repart"
    );
    a_neuf();
}

/// Le scan cède entre deux LOTS pendant la lecture, par la fonction de
/// production. Deux dossiers ⇒ deux lots (le découpage ne sépare jamais un
/// dossier, #3232) ⇒ une pause, et le relevé nomme le scan. Horloge RÉELLE :
/// la pause du scan est bloquante, sur son fil.
#[tokio::test]
async fn le_scan_marque_une_pause_entre_deux_lots_pendant_la_lecture() {
    let _g = VERROU.lock().await;
    a_neuf();
    let dossier = tempfile::tempdir().expect("dossier");
    let mut fichiers = Vec::new();
    for d in ["a", "b"] {
        let sous = dossier.path().join(d);
        std::fs::create_dir_all(&sous).expect("sous-dossier");
        let f = sous.join("1.flac");
        std::fs::write(&f, b"pas du flac").expect("fichier");
        fichiers.push(f);
    }
    let scanner = |fichiers: Vec<std::path::PathBuf>| {
        tokio::task::spawn_blocking(move || {
            let debut = std::time::Instant::now();
            let mut lots = 0usize;
            tune_core::scanner::walker::scan_files_batched(&fichiers, false, 1, |_, _, _| {
                lots += 1;
                tune_core::scanner::walker::EcrituresDuLot::SANS_PERTE
            });
            (lots, debut.elapsed())
        })
    };

    // Contre-épreuve : au repos, aucune pause.
    let (lots, duree) = scanner(fichiers.clone()).await.expect("scan");
    assert_eq!(lots, 2, "témoin : deux dossiers, deux lots");
    assert!(
        duree < PAUSE_EN_LECTURE,
        "au repos, le scan enchaîne ({duree:?})"
    );

    noter_etat_de_lecture(5, "playing");
    let (lots, duree) = scanner(fichiers).await.expect("scan");
    assert_eq!(lots, 2);
    assert!(
        duree >= PAUSE_EN_LECTURE,
        "une zone joue : le scan doit marquer sa pause entre deux lots ({duree:?})"
    );
    assert!(releve().throttled.contains(&ID_SCAN));
    a_neuf();
}

/// Les écritures de fond partent HORS du fil qui exécute la tâche asynchrone.
/// Sur un exécuteur à un seul fil (celui de `#[tokio::test]`), c'est le fil du
/// test lui-même : le travail doit tourner sur un autre.
#[tokio::test]
async fn une_ecriture_de_fond_ne_tourne_pas_sur_le_fil_de_l_executeur() {
    let _g = VERROU.lock().await;
    a_neuf();
    let fil_de_l_executeur = std::thread::current().id();
    let fil_du_travail = hors_du_fil_async(Tache::ReplayGain.id(), || std::thread::current().id())
        .await
        .expect("le travail rend sa valeur");
    assert_ne!(
        fil_du_travail, fil_de_l_executeur,
        "le travail synchrone a tourné sur le fil de l'exécuteur"
    );
    // Un travail qui panique ne fait pas tomber la passe.
    let rien: Option<()> = hors_du_fil_async(Tache::ReplayGain.id(), || panic!("témoin")).await;
    assert!(rien.is_none());
    a_neuf();
}

/// La forme du relevé que sert `GET /system/background-tasks` sous
/// `playback_priority` — ses clés sont un contrat avec le client à venir.
#[tokio::test]
async fn le_releve_a_la_forme_promise() {
    let _g = VERROU.lock().await;
    a_neuf();
    noter_etat_de_lecture(2, "playing");
    priorite::noter_cedee(Tache::Acoustique.id());
    let json = serde_json::to_value(releve()).expect("sérialisable");
    assert_eq!(json["playback_active"], true);
    assert_eq!(json["playing_zone_ids"], serde_json::json!([2]));
    assert!(json["since_epoch_s"].as_u64().is_some());
    assert_eq!(json["throttled"], serde_json::json!(["acoustic"]));
    assert_eq!(
        json["pause_between_items_ms"],
        PAUSE_EN_LECTURE.as_millis() as u64
    );
    a_neuf();
}
