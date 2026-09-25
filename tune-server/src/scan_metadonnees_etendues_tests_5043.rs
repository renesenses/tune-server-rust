//! #5043 — le scan ne rangeait les métadonnées étendues que pour les fichiers
//! de son LOT DE TRAVAIL. Un fichier inchangé n'y entre pas : une
//! bibliothèque constituée avant l'ajout de ce bloc restait à zéro, et les
//! sept champs « Crédits » des Réglages ne se remplissaient jamais.
//!
//! Arbitrage : **le scan complet rattrape** — et lui seul, et seulement ce qui
//! manque.
//!
//! Ces épreuves exécutent le VRAI scan manuel (`spawn_library_scan`, le bouton
//! « Scanner » et « Scan complet ») sur de vrais FLAC posés sous une racine de
//! musique, et lisent `track_metadata` en base.
use super::surveillant_retouche_tests_4896::{baliser, flac_8_canaux};
use crate::state::AppState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;

const COMPOSITEUR: &str = "Ludwig van Beethoven";

/// Une valeur que le FICHIER ne porte pas : si elle survit au scan, c'est que
/// le fichier n'a pas été rouvert ; si elle disparaît, c'est qu'il l'a été.
const TEMOIN_NON_ROUVERT: &str = "TÉMOIN — ce fichier ne doit pas être rouvert";

/// Une piste locale sur le disque, datée d'hier, avec une balise COMPOSER.
/// Racine sous le dossier courant : `is_tune_temp_file` écarte tout ce qui vit
/// sous le dossier temporaire du système.
fn piste_sur_disque(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, PathBuf) {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("scan-5043-{epreuve}"),
    );
    let dossier = racine.join("Beethoven").join("Symphonie no 9");
    std::fs::create_dir_all(&dossier).unwrap();
    let piste = dossier.join("01 - Allegro.flac");
    std::fs::write(&piste, flac_8_canaux()).unwrap();
    baliser(
        &piste,
        &[
            ("TITLE", "Allegro"),
            ("ARTIST", "Berliner Philharmoniker"),
            ("ALBUM", "Symphonie no 9"),
            ("TRACKNUMBER", "1"),
            ("COMPOSER", COMPOSITEUR),
        ],
        SystemTime::now() - Duration::from_secs(86_400),
    );
    (racine, piste)
}

/// La base est un FICHIER, pas `:memory:`. Sous SQLite en mémoire, les
/// connexions de lecture sont des clones de la connexion d'écriture : une
/// lecture faite pendant la transaction d'un lot voit ce que ce lot vient
/// d'écrire. Sur une base de fichier — ce que tout le monde fait tourner — le
/// pool de lecture est composé de connexions SÉPARÉES. Les deux ne se
/// comportent pas pareil, et c'est la seconde qu'il faut éprouver.
fn etat(racine: &Path) -> AppState {
    let base = racine.join("tune-epreuve-5043.db");
    let etat = AppState::new(&base.to_string_lossy(), 0, Default::default())
        .expect("AppState sur base de fichier");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    etat
}

/// Le scan MANUEL, jusqu'à sa fin annoncée. `complet` = le bouton « Scan
/// complet » (`?full=true`, qui vaut `force` côté serveur). Le droit de
/// scanner est global au processus : un autre essai peut le tenir, on attend
/// qu'il revienne.
async fn scan_manuel(etat: &AppState, complet: bool) {
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), complet, None).await {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan manuel n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => return,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus d'événements fermé : {e}"),
        }
    }
}

fn id_de_la_piste(db: &Arc<dyn DbBackend>) -> i64 {
    let lignes = db
        .query_many("SELECT id FROM tracks WHERE source = 'local'", &[])
        .expect("lecture tracks");
    assert_eq!(lignes.len(), 1, "une seule piste locale attendue");
    lignes[0][0].as_i64().expect("id")
}

fn cles(db: &Arc<dyn DbBackend>, id: i64) -> Vec<(String, String)> {
    db.query_many(
        &format!("SELECT key, value FROM track_metadata WHERE track_id = {id} ORDER BY key"),
        &[],
    )
    .expect("lecture track_metadata")
    .into_iter()
    .filter_map(|c| Some((c.first()?.as_string()?, c.get(1)?.as_string()?)))
    .collect()
}

fn valeur(db: &Arc<dyn DbBackend>, id: i64, cle: &str) -> Option<String> {
    cles(db, id)
        .into_iter()
        .find(|(k, _)| k == cle)
        .map(|(_, v)| v)
}

/// Simule une bibliothèque constituée AVANT l'existence du bloc de
/// métadonnées étendues : la piste est en base, `track_metadata` est vide.
fn vider_les_metadonnees(db: &Arc<dyn DbBackend>) {
    db.execute("DELETE FROM track_metadata", &[])
        .expect("purge track_metadata");
}

fn poser(db: &Arc<dyn DbBackend>, id: i64, lignes: &[(&str, &str)]) {
    let champs: HashMap<String, String> = lignes
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    TrackMetadataRepo::with_backend(db.clone())
        .set_batch_multi(&[(id, champs)])
        .expect("pose des lignes témoins");
}

/// Le premier scan pose la piste en base, puis on efface ses métadonnées
/// étendues : c'est l'état d'une bibliothèque d'avant le bloc.
async fn bibliotheque_dantan(
    epreuve: &str,
) -> (tune_core::test_scratch::ScratchDir, AppState, i64) {
    let (racine, _piste) = piste_sur_disque(epreuve);
    let etat = etat(racine.path());
    scan_manuel(&etat, false).await;
    let id = id_de_la_piste(&etat.backend);
    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(COMPOSITEUR),
        "le premier scan (fichier NEUF) doit poser le compositeur"
    );
    vider_les_metadonnees(&etat.backend);
    assert!(cles(&etat.backend, id).is_empty());
    (racine, etat, id)
}

// ---------------------------------------------------------------------------
// 0. LA CAUSE : sur une base de FICHIER, le scan ne rangeait AUCUNE métadonnée
//    étendue — pas même pour un fichier neuf. Le bloc tourne dans la
//    transaction du lot et cherchait le `tracks.id` par le pool de lecture,
//    des connexions séparées qui ne voient pas cette transaction.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_range_les_metadonnees_etendues_sur_une_base_de_fichier() {
    let (racine, _piste) = piste_sur_disque("base-fichier");
    let etat = etat(racine.path());

    scan_manuel(&etat, false).await;

    let id = id_de_la_piste(&etat.backend);
    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(COMPOSITEUR),
        "le scan doit ranger les métadonnées étendues d'un fichier NEUF sur une base de \
         FICHIER : le `tracks.id` se lit dans la transaction du lot, donc par une lecture forte. \
         Par le pool de lecture, la piste n'existe pas encore et tout le bloc est sauté en \
         silence (#5043). Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
}

// ---------------------------------------------------------------------------
// 1. Le scan COMPLET rattrape un fichier inchangé SANS métadonnées étendues.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_complet_rattrape_un_fichier_inchange_sans_metadonnees_etendues() {
    let (_racine, etat, id) = bibliotheque_dantan("rattrapage").await;

    scan_manuel(&etat, true).await;

    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(COMPOSITEUR),
        "le scan COMPLET doit rouvrir un fichier inchangé qui n'a AUCUNE métadonnée étendue : \
         sans cela les sept champs « Crédits » des Réglages restent vides à vie (#5043). \
         Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
}

// ---------------------------------------------------------------------------
// 2. LE PIÈGE : des lignes `rg_*` / `dr_*` / `upnp_*` ne sont PAS des
//    métadonnées étendues. 15 151 pistes du .18 n'ont que celles-là.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn une_piste_qui_na_que_des_lignes_replaygain_dr_upnp_reste_a_rattraper() {
    let (_racine, etat, id) = bibliotheque_dantan("piege-rg-dr-upnp").await;

    // L'état EXACT des 15 151 pistes du .18 : des lignes en base, posées par
    // l'analyse ReplayGain, la mesure DR et l'indexation UPnP — et pas une
    // seule métadonnée étendue.
    poser(
        &etat.backend,
        id,
        &[
            ("rg_track_gain", "-7.32 dB"),
            ("rg_track_peak", "0.988"),
            ("dr_track", "12"),
            ("dr_source", "tag"),
            ("upnp_object_id", "1$4$7"),
        ],
    );

    scan_manuel(&etat, true).await;

    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(COMPOSITEUR),
        "compter les LIGNES de `track_metadata` déclarerait cette piste « déjà faite » et la \
         sauterait à vie : `rg_*`, `dr_*` et `upnp_*` ont leurs propres écrivains, qui n'ouvrent \
         jamais le fichier pour ses crédits (#5043). Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
    assert_eq!(
        valeur(&etat.backend, id, "rg_track_gain").as_deref(),
        Some("-7.32 dB"),
        "le rattrapage n'efface pas ce que les autres écrivains ont posé"
    );
}

// ---------------------------------------------------------------------------
// 3. Un fichier inchangé qui a DÉJÀ ses métadonnées étendues n'est pas rouvert.
//    C'est la condition qui rend le surcoût UNIQUE et non permanent.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_complet_ne_rouvre_pas_un_fichier_qui_a_deja_ses_metadonnees_etendues() {
    let (_racine, etat, id) = bibliotheque_dantan("deja-pourvu").await;

    // Une métadonnée étendue en base, d'une valeur que le fichier ne porte
    // pas : si le scan rouvre le fichier, il l'écrase par `Ludwig van
    // Beethoven`.
    poser(&etat.backend, id, &[("composer", TEMOIN_NON_ROUVERT)]);

    scan_manuel(&etat, true).await;

    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(TEMOIN_NON_ROUVERT),
        "un fichier INCHANGÉ qui a déjà ses métadonnées étendues ne doit pas être rouvert : \
         sinon le second passage sur toute la bibliothèque (47 079 pistes sur le .18) se \
         repaierait à CHAQUE scan complet au lieu d'une seule fois (#5043). Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
}

// ---------------------------------------------------------------------------
// 4. Un fichier MODIFIÉ, lui, se relit toujours — même s'il a déjà ses
//    métadonnées étendues. Sans quoi la borne du point 3 gèlerait les crédits.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn un_fichier_retouche_relit_ses_metadonnees_etendues_meme_sil_en_avait() {
    let (_racine, piste) = piste_sur_disque("retouche");
    let etat = etat(_racine.path());
    scan_manuel(&etat, false).await;
    let id = id_de_la_piste(&etat.backend);
    poser(&etat.backend, id, &[("composer", TEMOIN_NON_ROUVERT)]);

    // Mp3tag repasse sur le fichier : la date de modification avance.
    baliser(
        &piste,
        &[
            ("TITLE", "Allegro"),
            ("ARTIST", "Berliner Philharmoniker"),
            ("ALBUM", "Symphonie no 9"),
            ("TRACKNUMBER", "1"),
            ("COMPOSER", COMPOSITEUR),
        ],
        SystemTime::now(),
    );

    scan_manuel(&etat, false).await;

    assert_eq!(
        valeur(&etat.backend, id, "composer").as_deref(),
        Some(COMPOSITEUR),
        "un fichier RETOUCHÉ se relit toujours : la borne du rattrapage ne porte que sur les \
         fichiers inchangés. Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
}

// ---------------------------------------------------------------------------
// 5. Un scan INCRÉMENTAL ne rouvre rien. Le rattrapage ne doit pas devenir une
//    régression de performance à chaque scan automatique.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn un_scan_incremental_ne_rouvre_aucun_fichier_inchange() {
    let (_racine, etat, id) = bibliotheque_dantan("incremental").await;

    scan_manuel(&etat, false).await;

    assert!(
        cles(&etat.backend, id).is_empty(),
        "un scan INCRÉMENTAL ne doit rouvrir aucun fichier inchangé : rouvrir toute la \
         bibliothèque à chaque scan automatique serait une régression de performance (#5043). \
         Lignes lues : {:?}",
        cles(&etat.backend, id)
    );
}
