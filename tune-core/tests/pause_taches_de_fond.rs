//! Mettre les traitements de fond en pause — les trois propriétés qui comptent.
//!
//! Le défaut : sur le .18 de Bertrand, la plage dynamique en est à 57 % de
//! 47 118 pistes et le ReplayGain à 7 % de 2 483. Ces passes décodent des
//! fichiers entiers pendant des heures et mangent le disque et le processeur
//! pendant qu'il écoute. Seul le scan avait un geste (`POST
//! /system/scan/cancel`) ; les cinq autres n'avaient **rien**.
//!
//! Ce témoin tient les trois propriétés du mécanisme, et il faut les trois :
//!
//! 1. **Frontière propre, et rien de perdu** — une passe suspendue EN PLEIN
//!    TRAVAIL s'arrête entre deux pistes, la piste en cours est finie et
//!    écrite, les pistes non prises restent candidates, et la reprise repart
//!    exactement où la pause l'a laissée. Retirer la garde de
//!    `analyze_track_batch` le fait rougir : le lot traverse ses 8 pistes.
//! 2. **Persistance** — une pause survit à un redémarrage du serveur. Retirer
//!    l'écriture en base, ou l'appel à `hydrater` du démarrage, le fait rougir.
//! 3. **La dépendance d'ordre** — mettre le ReplayGain en pause ne doit PAS
//!    faire démarrer la plage dynamique. C'est le piège central : la descente
//!    de la cascade se décidait sur le NOMBRE rendu par le rang précédent, et
//!    un rang suspendu rend `0` comme un rang au repos. Revenir à la forme
//!    `match analyze_track_batch(…) { 0 => … }` le fait rougir.
//!
//! ## Pourquoi une cible `[[test]]` à elle seule
//!
//! `autotests = false` en tête de `tune-core/Cargo.toml` : sans l'entrée du
//! manifeste, ce fichier ne serait JAMAIS compilé et la porte rendrait un vert
//! contre rien.
//!
//! Et un binaire à lui seul : la pause est un état de PROCESSUS (un masque de
//! bits atomique) comme l'avancement ReplayGain. Un voisin qui suspendrait une
//! passe dans le même binaire rendrait ces témoins intermittents. Les tests
//! d'ici se sérialisent d'ailleurs entre eux, pour la même raison.
//!
//! ## Pourquoi PAS `start_paused`
//!
//! La première propriété se joue sur une course RÉELLE : un lot en cours, une
//! pause posée par un autre fil pendant qu'il travaille. L'horloge virtuelle de
//! tokio rendrait les 400 ms de pause entre fichiers gratuites, le lot entier
//! se jouerait avant que la pause n'arrive, et le témoin ne garderait plus
//! rien.

use std::sync::Arc;

use tune_core::audio::replaygain::{
    TourDeCascade, analyze_track_batch, compter_les_candidats_replaygain, progression,
    un_tour_de_cascade,
};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::taches_de_fond::{
    Tache, est_en_pause, hydrater, mettre_en_pause, oublier_pour_les_essais, reprendre,
    tout_reprendre, tout_suspendre,
};

/// Huit pistes : assez pour qu'un arrêt à la première ou à la deuxième laisse
/// une marge visible sous le total, et bien en deçà du lot de 25 — le témoin
/// doit tenir en UN seul appel.
const PISTES: i64 = 8;

/// Les globales de ce mécanisme (le masque de pause, l'avancement ReplayGain,
/// le verrou d'analyse) sont des états de PROCESSUS. Deux témoins qui les
/// touchent en même temps se verraient mutuellement.
static VERROU: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SCHEMA: &str = "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, last_play_state TEXT);
     CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                            updated_at TEXT NOT NULL DEFAULT '');
     CREATE TABLE tracks (id INTEGER PRIMARY KEY, album_id INTEGER, file_path TEXT,
                          duration_ms INTEGER, sample_rate INTEGER, channels INTEGER,
                          audio_fingerprint TEXT, format TEXT);
     CREATE TABLE track_metadata (track_id INTEGER NOT NULL, key TEXT NOT NULL,
                                  value TEXT NOT NULL, PRIMARY KEY (track_id, key));";

/// Une bibliothèque minimale : des pistes dont le fichier RÉPOND et reste
/// indécodable.
///
/// Même montage que le témoin de #4144 et que ceux de #2496 : la piste
/// traverse toute la boucle — résolution du chemin, mesure, témoin
/// `rg_analyzed` — sans qu'aucun décodeur ne travaille. Ce témoin compte des
/// PISTES SORTIES DU BALAYAGE ; ce que la mesure a trouvé ne le regarde pas.
///
/// `format = 'dsf'` : les DSD sont hors du rang des empreintes
/// (`CANDIDATS_EMPREINTE_WHERE`). Le rang 2 est donc vide par construction, ce
/// qui laisse la dépendance d'ordre se jouer entre les seuls rangs 1 et 3 —
/// sans quoi le témoin ne saurait pas lequel des deux a bloqué la descente.
fn bibliotheque(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    peupler(&db, dossier);
    Arc::new(db)
}

fn peupler(db: &SqliteDb, dossier: &std::path::Path) {
    db.execute_batch(SCHEMA).expect("schéma");
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
            "INSERT INTO tracks (id, album_id, file_path, duration_ms, sample_rate, channels, format) \
             VALUES (?, NULL, ?, 300000, 44100, 2, 'dsf')",
            &[&i, &chemin],
        )
        .expect("insertion de piste");
    }
}

/// Combien de pistes portent une clef donnée. C'est la mesure de ce qui est
/// RÉELLEMENT écrit en base — pas ce que la fonction a bien voulu rendre.
fn comptees(backend: &Arc<dyn DbBackend>, clef: &str) -> i64 {
    backend
        .query_one(
            "SELECT COUNT(*) FROM track_metadata WHERE key = ?",
            &[&clef as &dyn tune_core::db::backend::ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// Remettre le mécanisme à neuf entre deux témoins du même binaire.
fn a_neuf() {
    oublier_pour_les_essais();
    progression::reinitialiser_pour_les_essais();
}

// ---------------------------------------------------------------------------
// 1. Frontière propre, rien de perdu, reprise au même point
// ---------------------------------------------------------------------------

/// La pause tombe PENDANT le lot, pas entre deux lots.
///
/// C'est la seule forme qui garde quelque chose : une pause posée avant le
/// départ prouverait seulement qu'une passe qui n'a pas commencé ne commence
/// pas. Ici le lot travaille, la pause arrive par un autre fil, et on vérifie
/// les quatre choses qui comptent : il s'arrête, il s'arrête À UNE FRONTIÈRE
/// (autant de témoins écrits que de pistes annoncées), il ne perd rien (les
/// pistes non prises sont encore candidates), et il reprend où il en était.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_pause_en_plein_lot_s_arrete_a_une_frontiere_et_ne_perd_rien() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque(tmp.path());

    let pour_le_lot = backend.clone();
    let lot = tokio::spawn(async move { analyze_track_batch(&pour_le_lot).await });

    // Attendre qu'UNE piste soit effectivement sortie du balayage : la pause
    // doit tomber sur une passe qui travaille, pas sur une passe qui n'a pas
    // encore démarré.
    let mut tours = 0;
    while comptees(&backend, "rg_analyzed") < 1 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        tours += 1;
        assert!(tours < 2_000, "le lot n'a jamais traité la moindre piste");
    }
    mettre_en_pause(&backend, Tache::ReplayGain).expect("poser la pause");

    let pendant = lot.await.expect("le lot ne doit pas paniquer") as i64;

    // — il s'est arrêté —
    assert!(
        pendant > 0 && pendant < PISTES,
        "🔴 LA PAUSE N'ARRÊTE PAS LE LOT EN COURS. {pendant} pistes traitées \
         sur {PISTES} alors que la pause est tombée après la première. Une \
         pause qui n'agit qu'entre deux lots laisse tourner jusqu'à 25 \
         décodages — plus d'une heure de disque et de processeur après le clic, \
         exactement ce que le bouton doit éviter."
    );

    // — à une FRONTIÈRE : autant d'écritures que de pistes annoncées —
    let ecrites = comptees(&backend, "rg_analyzed");
    assert_eq!(
        ecrites, pendant,
        "🔴 ARRÊT AU MILIEU D'UNE PISTE. Le lot annonce {pendant} pistes \
         traitées et la base en porte {ecrites}. La frontière propre est entre \
         deux pistes : celle qui est commencée doit être finie et écrite, \
         jamais laissée à moitié."
    );
    let relevee = progression::releve();
    assert_eq!(
        relevee.traitees, pendant,
        "🔴 la jauge ne dit pas la même chose que la base : {relevee:?}"
    );

    // — rien n'est perdu : les pistes non prises sont encore candidates —
    let restantes = compter_les_candidats_replaygain(&backend);
    assert_eq!(
        restantes,
        PISTES - pendant,
        "🔴 DU TRAVAIL A ÉTÉ PERDU. {pendant} pistes traitées, {restantes} \
         encore candidates, sur {PISTES}. Une pause ne doit RIEN retirer du \
         balayage qu'elle n'a pas mesuré — une piste estampillée sans avoir été \
         analysée sort du balayage pour toujours."
    );

    // — la reprise repart où elle en était —
    reprendre(&backend, Tache::ReplayGain).expect("lever la pause");
    let apres = analyze_track_batch(&backend).await as i64;
    assert_eq!(
        apres,
        PISTES - pendant,
        "🔴 LA REPRISE NE REPART PAS OÙ LA PAUSE A LAISSÉ LE TRAVAIL. Le second \
         lot a traité {apres} pistes au lieu des {} qui restaient.",
        PISTES - pendant
    );
    assert_eq!(
        comptees(&backend, "rg_analyzed"),
        PISTES,
        "🔴 après la reprise, les {PISTES} pistes doivent toutes être sorties du \
         balayage, et chacune UNE fois."
    );
}

/// La contre-épreuve, et elle est indispensable : sans elle, un mécanisme qui
/// arrêterait le lot pour n'importe quelle raison — ou qui ne le ferait jamais
/// démarrer — passerait le témoin ci-dessus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contre_epreuve_sans_pause_le_lot_traverse_toute_la_bibliotheque() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque(tmp.path());

    let fait = analyze_track_batch(&backend).await as i64;

    assert_eq!(
        fait, PISTES,
        "le montage du témoin est en défaut : sans pause, le lot doit \
         traverser les {PISTES} pistes"
    );
    assert_eq!(comptees(&backend, "rg_analyzed"), PISTES);
    assert_eq!(compter_les_candidats_replaygain(&backend), 0);
}

// ---------------------------------------------------------------------------
// 2. Persistance
// ---------------------------------------------------------------------------

/// Une pause survit à un redémarrage du serveur.
///
/// Le redémarrage est joué à l'identique de ce que fait le serveur : le
/// processus perd son miroir en mémoire ([`oublier_pour_les_essais`]) et
/// `spawn_background_tasks` le reconstruit depuis la base ([`hydrater`]). Et
/// la base est un FICHIER, relu par une SECONDE connexion : une pause qui ne
/// vivrait que dans la connexion d'origine ne prouverait rien.
///
/// Sans cette propriété la pause ne sert à rien : la mise à jour du soir ou un
/// `Restart=always` après un incident relancerait le décodage au milieu de
/// l'écoute, sur une passe de huit heures.
#[tokio::test]
async fn une_pause_survit_au_redemarrage_du_serveur() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let fichier = tmp.path().join("tune.db");

    // ── Session 1 : on suspend ────────────────────────────────────────────
    {
        let db = SqliteDb::open(fichier.to_str().expect("chemin")).expect("base sur disque");
        db.execute_batch(SCHEMA).expect("schéma");
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        mettre_en_pause(&backend, Tache::PlageDynamique).expect("suspendre");
        assert!(est_en_pause(Tache::PlageDynamique));
    }

    // ── Le redémarrage : le processus oublie tout ─────────────────────────
    oublier_pour_les_essais();
    assert!(
        !est_en_pause(Tache::PlageDynamique),
        "le montage du témoin est en défaut : un processus neuf ne connaît \
         aucune pause tant qu'il n'a pas relu la base"
    );

    // ── Session 2 : le serveur redémarre et relit ─────────────────────────
    let db = SqliteDb::open(fichier.to_str().expect("chemin")).expect("réouverture");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    hydrater(&backend);

    assert!(
        est_en_pause(Tache::PlageDynamique),
        "🔴 LA PAUSE N'A PAS SURVÉCU AU REDÉMARRAGE. Le serveur redémarre pour \
         une mise à jour, ou tout seul après un incident (`Restart=always`), et \
         la plage dynamique repart décoder au milieu de l'écoute. Sur une passe \
         de huit heures, une pause qui ne survit pas ne sert à rien."
    );
    assert_eq!(
        SettingsRepo::with_backend(backend.clone())
            .get(Tache::PlageDynamique.cle_reglage())
            .expect("lecture du réglage")
            .as_deref(),
        Some("true"),
        "la pause doit être en BASE, pas seulement en mémoire"
    );

    // ── Contre-épreuve : une reprise survit tout autant ───────────────────
    // Sans elle, un `hydrater` qui poserait la pause sur TOUTE clef présente —
    // ou une écriture qui ne saurait qu'ajouter — passerait le témoin
    // ci-dessus.
    reprendre(&backend, Tache::PlageDynamique).expect("reprendre");
    oublier_pour_les_essais();
    hydrater(&backend);
    assert!(
        !est_en_pause(Tache::PlageDynamique),
        "🔴 UNE REPRISE NE SURVIT PAS AU REDÉMARRAGE : le traitement se \
         retrouverait suspendu pour toujours, sans rien à l'écran pour le \
         lever."
    );
}

/// L'interrupteur général pose et lève la même pause sur les six traitements.
#[tokio::test]
async fn l_interrupteur_general_couvre_tous_les_traitements() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    db.execute_batch(SCHEMA).expect("schéma");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let _ = tmp;

    tout_suspendre(&backend).expect("tout suspendre");
    for tache in Tache::TOUTES {
        assert!(
            est_en_pause(tache),
            "🔴 « Suspendre tous les traitements » a laissé {} dehors — et il \
             continuerait de décoder sous un interrupteur qui dit « tout \
             suspendu »",
            tache.id()
        );
    }
    assert!(tune_core::taches_de_fond::tout_est_suspendu());

    // Reprendre UN seul traitement doit défaire l'état « tout suspendu » :
    // c'est ce qu'un septième drapeau « global » aurait manqué.
    reprendre(&backend, Tache::Acoustique).expect("reprendre un seul");
    assert!(!tune_core::taches_de_fond::tout_est_suspendu());

    tout_reprendre(&backend).expect("tout reprendre");
    for tache in Tache::TOUTES {
        assert!(!est_en_pause(tache), "{} est resté suspendu", tache.id());
    }
}

// ---------------------------------------------------------------------------
// 3. La dépendance d'ordre
// ---------------------------------------------------------------------------

/// Une bibliothèque où le ReplayGain n'a plus rien et la plage dynamique a tout
/// à faire : exactement l'état du .18 de Bertrand.
fn bibliotheque_prete_pour_le_dr(dossier: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().expect("base mémoire");
    peupler(&db, dossier);
    for i in 1..=PISTES {
        db.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_analyzed', '1')",
            &[&i],
        )
        .expect("témoin rg_analyzed");
    }
    Arc::new(db)
}

/// 🔴 LE PIÈGE CENTRAL.
///
/// La descente de la cascade se décidait sur le NOMBRE rendu par le rang
/// précédent :
///
/// ```ignore
/// match analyze_track_batch(&backend).await {
///     0 => match empreinter_un_lot(&backend).await {
///         0 => rattraper_un_lot_de_dr(&backend).await,
/// ```
///
/// Un rang suspendu rend `0` — comme un rang au repos. Avec cette forme-là,
/// suspendre le **ReplayGain** aurait lancé la **plage dynamique** : la plus
/// lourde des trois, démarrée par le geste censé rendre la machine à la
/// musique.
///
/// La condition de rang n'est pas « le précédent a rendu 0 », c'est « le
/// précédent est AU REPOS ».
#[tokio::test]
async fn une_pause_du_replaygain_ne_lance_pas_la_plage_dynamique() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque_prete_pour_le_dr(tmp.path());

    mettre_en_pause(&backend, Tache::ReplayGain).expect("suspendre le ReplayGain");
    let tour = un_tour_de_cascade(&backend).await;

    assert_eq!(
        tour,
        TourDeCascade::Suspendue(Tache::ReplayGain),
        "🔴 la cascade devait s'arrêter sur le rang suspendu, elle a rendu {tour:?}"
    );
    let touchees = comptees(&backend, "dr_track") + comptees(&backend, "dr_indisponible");
    assert_eq!(
        touchees, 0,
        "🔴 METTRE LE REPLAYGAIN EN PAUSE A LANCÉ LA PLAGE DYNAMIQUE. \
         {touchees} pistes ont été décodées par le rang 3 alors que le rang 1 \
         est SUSPENDU. La descente se décide sur « le rang précédent est au \
         REPOS », jamais sur « il a rendu 0 » — un rang suspendu rend 0 lui \
         aussi, et le geste qui devait tout calmer démarrerait la passe la plus \
         lourde des trois."
    );
}

/// La contre-épreuve, et elle est indispensable : sans elle, une cascade qui ne
/// descendrait JAMAIS jusqu'au rang 3 passerait le témoin ci-dessus. Ici le
/// ReplayGain est au repos pour de bon — même bibliothèque, même absence de
/// candidats, pause en moins — et la plage dynamique DOIT travailler.
#[tokio::test]
async fn contre_epreuve_replaygain_au_repos_la_plage_dynamique_travaille() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque_prete_pour_le_dr(tmp.path());

    assert_eq!(
        compter_les_candidats_replaygain(&backend),
        0,
        "le montage du témoin est en défaut : le rang 1 doit être au repos"
    );
    let tour = un_tour_de_cascade(&backend).await;

    assert!(
        matches!(tour, TourDeCascade::Travail(_)),
        "le montage du témoin est en défaut : la cascade devait atteindre le \
         rang 3, elle a rendu {tour:?}"
    );
    let touchees = comptees(&backend, "dr_track") + comptees(&backend, "dr_indisponible");
    assert!(
        touchees > 0,
        "la contre-épreuve ne prouve rien : la plage dynamique n'a touché \
         aucune piste alors que rien ne la retenait"
    );
}

/// Et le symétrique : suspendre la SEULE plage dynamique laisse le ReplayGain
/// travailler. Un mécanisme qui gèlerait tout dès qu'une case est cochée
/// passerait les deux témoins précédents.
#[tokio::test]
async fn suspendre_la_plage_dynamique_laisse_le_replaygain_travailler() {
    let _serialise = VERROU.lock().await;
    a_neuf();
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let backend = bibliotheque(tmp.path());

    mettre_en_pause(&backend, Tache::PlageDynamique).expect("suspendre le DR");
    let tour = un_tour_de_cascade(&backend).await;

    assert_eq!(
        tour,
        TourDeCascade::Travail(PISTES as usize),
        "🔴 suspendre la plage dynamique a arrêté le ReplayGain : {tour:?}"
    );
    assert_eq!(comptees(&backend, "rg_analyzed"), PISTES);
    assert_eq!(
        comptees(&backend, "dr_track") + comptees(&backend, "dr_indisponible"),
        0,
        "🔴 la plage dynamique est suspendue et a pourtant décodé"
    );
}
