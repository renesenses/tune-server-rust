//! #4144 — la passe ReplayGain publie son avancement, à cadence espacée.
//!
//! Le défaut : `tune-core/src/audio/replaygain.rs` ne publiait NI `processed`,
//! NI `total`, NI le moindre évènement. La carte « ReplayGain » de l'écran
//! Santé affichait donc `IDLE` — littéralement — pendant qu'une passe de
//! plusieurs heures tournait sur 50 000 pistes.
//!
//! Ce témoin tient les DEUX bords de la correction, et il faut les deux :
//!
//! 1. **le compteur avance** — retirer `progression::avancer()` de la boucle de
//!    balayage le fait rougir sur `traitees` ;
//! 2. **la cadence tient** — l'annonce ne part PAS à chaque piste. Retirer la
//!    garde de cadence de `progression::avancer` le fait rougir sur le NOMBRE
//!    d'évènements. C'est la moitié qu'on oublie : un compteur juste qui inonde
//!    un bus de 256 entrées fait décrocher les abonnés, et l'écran perd
//!    justement ce qu'on vient de lui donner.
//!
//! ## Pourquoi une cible `[[test]]` à elle seule
//!
//! `autotests = false` en tête de `tune-core/Cargo.toml` : sans l'entrée du
//! manifeste, ce fichier ne serait JAMAIS compilé et la porte rendrait un vert
//! contre rien.
//!
//! Et un binaire à lui seul, pas un `mod` d'`integration_contracts` : l'état
//! d'avancement est un état de PROCESSUS (`audio::replaygain::progression`),
//! comme le compteur d'écrêtage. Un voisin qui ferait tourner la passe dans le
//! même binaire écrirait dans le même état et rendrait ce témoin intermittent.
//!
//! ## Pourquoi `start_paused`
//!
//! La cadence se mesure sur un `std::time::Instant`, que l'horloge virtuelle de
//! tokio ne déplace pas : les 400 ms de pause entre fichiers deviennent
//! gratuites, le lot entier se joue en quelques millisecondes de temps RÉEL, et
//! le compte d'évènements attendu est alors exact au lieu d'être « à peu près ».
//! Un témoin qui doit tolérer une marge sur le nombre d'annonces ne garde plus
//! la cadence.

use std::sync::Arc;
use tune_core::audio::replaygain::{analyze_track_batch, progression};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::event_bus::{EventBus, TuneEvent};

/// Le nom de fil, écrit ici en toutes lettres : c'est le contrat que
/// `TuneHealthV2.svelte` lit dans l'autre dépôt.
const FIL: &str = "library.replaygain.progress";

/// Nombre de pistes du lot. Six : assez pour qu'une émission par piste se
/// distingue sans ambiguïté des deux annonces de bord, et bien en deçà du lot
/// de 25 — le témoin doit tenir en UN seul appel.
const PISTES: i64 = 6;

/// Une bibliothèque minimale : des pistes dont le fichier RÉPOND et reste
/// indécodable.
///
/// C'est délibéré, et c'est le montage déjà employé par les témoins de #2496
/// dans `replaygain.rs` : la piste traverse toute la boucle — résolution du
/// chemin, mesure, témoin `rg_analyzed` — sans qu'aucun décodeur ne travaille.
/// Ce témoin compte des PISTES SORTIES DU BALAYAGE ; ce que la mesure a trouvé
/// ne le regarde pas, et surtout il ne doit toucher à AUCUN échantillon.
fn bibliotheque(dossier: &std::path::Path) -> (SqliteDb, Arc<dyn DbBackend>) {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(
        "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
         CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                                updated_at TEXT NOT NULL DEFAULT '');
         CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                              duration_ms INTEGER, sample_rate INTEGER, channels INTEGER,
                              audio_fingerprint TEXT, format TEXT);
         CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                      value TEXT NOT NULL, PRIMARY KEY (track_id, key));",
    )
    .expect("schéma");
    // L'analyse doit être ARMÉE, sans quoi la boucle sort au premier tour :
    // `replaygain_mode` ABSENT vaut `off`.
    db.execute(
        "INSERT INTO settings (key, value) VALUES ('replaygain_mode', 'track')",
        &[],
    )
    .expect("réglage");
    for i in 1..=PISTES {
        let fichier = dossier.join(format!("{i}.flac"));
        std::fs::write(&fichier, b"pas du flac").expect("fichier temoin");
        let chemin = fichier.to_string_lossy().to_string();
        db.execute(
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels) \
             VALUES (?, NULL, ?, 300000, 44100, 2)",
            &[&i, &chemin],
        )
        .expect("insertion de piste");
    }
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    (db, backend)
}

/// Vide la boîte de réception sans bloquer.
fn recolter(rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>) -> Vec<TuneEvent> {
    let mut vus = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event_type == FIL {
            vus.push(ev);
        }
    }
    vus
}

#[tokio::test(start_paused = true)]
async fn la_passe_replaygain_publie_traitees_sur_total_a_cadence_espacee() {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let (_db, backend) = bibliotheque(tmp.path());

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    progression::brancher_le_bus(bus.clone());
    progression::reinitialiser_pour_les_essais();

    // ── Avant toute passe : rien à dire, et le dire ───────────────────────
    let avant = progression::releve();
    assert!(
        !avant.a_parle() && !avant.actif,
        "#4144 : au démarrage la passe n'a rien annoncé — \
         `a_parle` doit rester faux pour que la carte distingue « je ne sais pas » \
         d'une bibliothèque finie ; relevé = {avant:?}"
    );

    // ── Le lot ────────────────────────────────────────────────────────────
    let traitees_par_le_lot = analyze_track_batch(&backend).await;
    assert_eq!(
        traitees_par_le_lot, PISTES as usize,
        "le montage du témoin est en défaut : le lot devait traverser les \
         {PISTES} pistes"
    );

    // PROPRIÉTÉ 1 — le compteur a avancé.
    let pendant = progression::releve();
    assert_eq!(
        pendant.traitees, PISTES,
        "🔴 #4144 : LA PASSE REPLAYGAIN NE COMPTE PAS CE QU'ELLE TRAITE. \
         {PISTES} pistes viennent de sortir du balayage et l'avancement en \
         annonce {}. C'est exactement ce qui faisait afficher `IDLE` à l'écran \
         Santé pendant des heures de calcul. Relevé = {pendant:?}",
        pendant.traitees
    );
    assert_eq!(
        pendant.total, PISTES,
        "🔴 #4144 : le DÉNOMINATEUR est faux. Sans lui la carte a un compteur \
         qui monte vers rien. Relevé = {pendant:?}"
    );
    assert!(
        pendant.actif && pendant.a_parle(),
        "🔴 #4144 : une campagne est en cours, l'avancement doit se déclarer \
         actif. Relevé = {pendant:?}"
    );

    // PROPRIÉTÉ 2 — la cadence. Une seule annonce jusqu'ici : l'OUVERTURE.
    // Les six pistes se sont jouées en bien moins de deux secondes de temps
    // réel, donc aucune annonce d'avancement n'était due.
    let pendant_les_evts = recolter(&mut rx);
    assert_eq!(
        pendant_les_evts.len(),
        1,
        "🔴 #4144 : LA CADENCE NE TIENT PAS. {} annonces `{FIL}` pour {PISTES} \
         pistes, alors que seule celle de l'ouverture était due. Émettre par \
         piste sur une bibliothèque de 50 000 titres noierait un bus de 256 \
         entrées et ferait décrocher le WebSocket du client — le scan a réglé \
         ce problème avec `CADENCE_PROGRESSION_PARCOURS`, on le copie. \
         Phases reçues : {:?}",
        pendant_les_evts.len(),
        pendant_les_evts
            .iter()
            .map(|e| e.data["phase"].clone())
            .collect::<Vec<_>>()
    );
    let ouverture = &pendant_les_evts[0];
    assert_eq!(ouverture.data["phase"], "started");
    assert_eq!(
        ouverture.data["total"], PISTES,
        "l'annonce d'ouverture porte le total : c'est elle qui donne son \
         dénominateur à la carte avant la première mesure"
    );

    // ── Le second lot : il ne reste rien ──────────────────────────────────
    let reste = analyze_track_batch(&backend).await;
    assert_eq!(
        reste, 0,
        "les {PISTES} pistes portent leur témoin `rg_analyzed`"
    );

    // PROPRIÉTÉ 3 — le bord « fini ». Sans lui, la carte resterait sur le
    // dernier couple annoncé et dirait « en cours » pour toujours.
    let apres = progression::releve();
    assert!(
        !apres.actif,
        "🔴 #4144 : plus aucune piste en attente et l'avancement se dit encore \
         actif — l'écran afficherait un balayage éternel. Relevé = {apres:?}"
    );
    assert_eq!(
        (apres.traitees, apres.total),
        (PISTES, PISTES),
        "🔴 #4144 : le retour au repos ne doit pas remettre le compteur à zéro : \
         « {PISTES} pistes traitées, plus rien en attente » est ce que la carte \
         doit montrer. Un 0/0 se lirait comme « rien n'a jamais tourné ». \
         Relevé = {apres:?}"
    );

    let fin = recolter(&mut rx);
    assert_eq!(
        fin.len(),
        1,
        "🔴 #4144 : le retour au repos doit être annoncé UNE fois, sans attendre \
         la cadence — une passe qui finit juste après une annonce laisserait \
         sinon la jauge figée. Reçu : {fin:?}"
    );
    assert_eq!(fin[0].data["phase"], "idle");
    assert_eq!(fin[0].data["processed"], PISTES);
    assert_eq!(fin[0].data["active"], false);

    // Et le repos ne se réannonce pas à chaque tour de boucle : la passe
    // repasse ici toutes les 15 minutes, et une ligne par tour chasserait
    // l'historique utile du bus.
    let _ = analyze_track_batch(&backend).await;
    let encore = recolter(&mut rx);
    assert!(
        encore.is_empty(),
        "🔴 #4144 : le repos se réannonce à chaque tour de boucle — même faute \
         que l'émission par piste, à l'autre bout. Reçu : {encore:?}"
    );
}
