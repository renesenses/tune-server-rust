//! 🔴 #2569 / #2775 — l'instantané de file d'attente sur disque.
//!
//! Pierre M (#2569, Windows 11) : « la file d'attente se limite au titre en
//! cours ». Sandro (#2775) : « seul ce morceau est présent, le reste de la file
//! d'attente a disparu » après un redémarrage du serveur.
//!
//! `queue_persistence::save_queue` lisait la file avec trois
//! `unwrap_or_default()`. Une base qui répond `Err` (« database is locked » —
//! la condition documentée par #1997 : un lot de scan tient la connexion SQLite
//! partagée pendant que l'utilisateur agit sur sa file) devenait une file VIDE,
//! et cette file vide écrasait sur le disque l'instantané d'une file pleine.
//!
//! Ces tests appellent la fonction de PRODUCTION `save_queue`, sur une VRAIE
//! base SQLite posée sur disque, fermée puis rouverte. Ils couvrent les deux
//! sens :
//!
//! * l'écrasement accidentel est REFUSÉ (base illisible) ;
//! * l'effacement délibéré est ACCEPTÉ (base lisible, file réellement vide).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tune_core::db::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::ZoneState;
use tune_core::queue_persistence::{QueueSnapshot, save_queue};

// ---------------------------------------------------------------------------
// Une base qui ne répond plus sur la file — exactement ce que voit le serveur
// quand un lot de scan tient la connexion SQLite partagée (#1997).
// ---------------------------------------------------------------------------

/// Délègue TOUT à une vraie base, sauf les lectures qui touchent
/// `queue_items` : celles-là échouent, comme sous verrou SQLite.
///
/// On ne simule pas la file : la base est réelle et pleine. On simule seulement
/// le VERROU, c'est-à-dire la seule chose que le test ne peut pas provoquer à
/// coup sûr sans devenir intermittent.
struct BaseOccupee {
    interne: Arc<dyn DbBackend>,
}

/// Le message que rusqlite remonte à travers `query_many` sous SQLITE_BUSY.
const VERROU: &str = "query: database is locked";

fn touche_la_file(sql: &str) -> bool {
    sql.contains("queue_items")
}

impl DbBackend for BaseOccupee {
    fn engine(&self) -> Engine {
        self.interne.engine()
    }

    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        self.interne.execute(sql, params)
    }

    fn last_insert_rowid(&self) -> i64 {
        self.interne.last_insert_rowid()
    }

    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        if touche_la_file(sql) {
            return Err(VERROU.to_string());
        }
        self.interne.query_one(sql, params)
    }

    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        if touche_la_file(sql) {
            return Err(VERROU.to_string());
        }
        self.interne.query_many(sql, params)
    }

    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.interne.write_tx(f)
    }

    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.interne.execute_batch(sql)
    }
}

// ---------------------------------------------------------------------------
// Outillage : une vraie base sur disque, et la lecture de l'instantané écrit.
// ---------------------------------------------------------------------------

/// Le chemin où `save_queue` dépose l'instantané, à côté de la base.
fn fichier_instantane(db_path: &str, zone_id: i64) -> PathBuf {
    Path::new(db_path)
        .parent()
        .unwrap()
        .join("queue_state")
        .join(format!("queue_{zone_id}.json"))
}

fn relire_instantane(db_path: &str, zone_id: i64) -> QueueSnapshot {
    let brut = std::fs::read_to_string(fichier_instantane(db_path, zone_id))
        .expect("l'instantané doit exister sur le disque");
    serde_json::from_str(&brut).expect("l'instantané doit rester du JSON valide")
}

/// Ouvre la base POSÉE SUR DISQUE et l'amène au schéma courant.
fn ouvrir(db_path: &str) -> Arc<dyn DbBackend> {
    let sqlite = SqliteDb::open(db_path).expect("ouverture SQLite sur disque");
    sqlite.init_schema().expect("schema");
    tune_core::db::migrations::run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    db.execute(
        "INSERT OR IGNORE INTO zones (id, name, output_type) VALUES (1, 'Salon', 'local')",
        &[],
    )
    .expect("zone");
    db
}

fn album_de(n: i64) -> Vec<QueueInput> {
    (0..n)
        .map(|i| QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: format!("q{i}"),
            title: format!("Titre {i}"),
            artist: "Artiste".into(),
            album: Some("Album".into()),
            cover_url: None,
            duration_ms: 200_000,
            track_number: Some(i + 1),
            disc_number: Some(1),
        })
        .collect()
}

fn etat_zone() -> ZoneState {
    ZoneState {
        zone_id: 1,
        queue_position: 3,
        queue_length: 12,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// 1. L'écrasement accidentel doit être REFUSÉ.
// ---------------------------------------------------------------------------

/// 🔴 LE défaut. Un instantané de douze titres est sur le disque. La base
/// devient illisible. `save_queue` s'exécute — et l'instantané des douze titres
/// doit être TOUJOURS là, intact.
///
/// Avant le correctif, `unwrap_or_default()` rendait une file vide et
/// `save_queue` écrivait par-dessus un instantané à zéro titre. C'est ce que
/// Sandro retrouve au redémarrage suivant.
#[test]
fn une_base_illisible_n_ecrase_pas_l_instantane_i2569() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dossier.path().join("tune.db");
    let db_path = chemin.to_str().unwrap().to_string();

    // Une vraie base, une vraie file de douze titres, un vrai instantané.
    {
        let db = ouvrir(&db_path);
        let repo = PlayQueueRepo::with_backend(db.clone());
        repo.append(1, &album_de(12)).expect("remplir la file");
        save_queue(&db, &db_path, 1, &etat_zone());
    } // la base est FERMÉE ici.

    let avant = relire_instantane(&db_path, 1);
    assert_eq!(avant.items.len(), 12, "l'instantané de départ est complet");

    // Rouverte depuis le disque, puis rendue illisible sur la file — le serveur
    // pendant un lot de scan.
    let db_reelle = ouvrir(&db_path);
    let repo_reel = PlayQueueRepo::with_backend(db_reelle.clone());
    assert_eq!(
        repo_reel.get_ordered(1).expect("relecture").len(),
        12,
        "la file est bien encore de douze titres en base"
    );

    let occupee: Arc<dyn DbBackend> = Arc::new(BaseOccupee {
        interne: db_reelle.clone(),
    });
    assert!(
        PlayQueueRepo::with_backend(occupee.clone())
            .get_ordered(1)
            .is_err(),
        "le montage doit bien rendre la file illisible"
    );

    // La fonction de production, sur une base qui ne répond pas.
    save_queue(&occupee, &db_path, 1, &etat_zone());

    let apres = relire_instantane(&db_path, 1);
    assert_eq!(
        apres.items.len(),
        12,
        "une base illisible ne doit RIEN écraser : la file de douze titres \
         doit survivre (#2569 — sans quoi elle revient à un seul titre)"
    );
    let ids: Vec<Option<&str>> = apres.items.iter().map(|i| i.source_id.as_deref()).collect();
    assert_eq!(
        ids[0],
        Some("q0"),
        "l'instantané conservé doit être l'ancien, à l'identique"
    );
    assert_eq!(ids[11], Some("q11"));
}

// ---------------------------------------------------------------------------
// 2. LE TÉMOIN : l'effacement délibéré doit être ACCEPTÉ.
// ---------------------------------------------------------------------------

/// 🔴 LE TÉMOIN, et le défaut symétrique à ne pas introduire.
///
/// Refuser d'écrire « une file vide » serait plus insidieux que le défaut
/// d'origine : la file que l'utilisateur vient de vider réapparaîtrait au
/// redémarrage suivant. Le critère n'est donc pas « la file est-elle vide »,
/// c'est `Err` contre `Ok(vide)`.
///
/// Ici la base RÉPOND, et elle répond « vide ». L'instantané doit passer à zéro.
#[test]
fn une_file_videe_volontairement_est_bien_persistee_i2569() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dossier.path().join("tune.db");
    let db_path = chemin.to_str().unwrap().to_string();

    {
        let db = ouvrir(&db_path);
        let repo = PlayQueueRepo::with_backend(db.clone());
        repo.append(1, &album_de(12)).expect("remplir la file");
        save_queue(&db, &db_path, 1, &etat_zone());
    } // fermée.

    assert_eq!(relire_instantane(&db_path, 1).items.len(), 12);

    // Rouverte depuis le disque : l'utilisateur vide sa file.
    let db = ouvrir(&db_path);
    let repo = PlayQueueRepo::with_backend(db.clone());
    repo.clear(1).expect("vider la file");
    assert_eq!(repo.get_ordered(1).expect("relecture").len(), 0);

    save_queue(&db, &db_path, 1, &ZoneState::default());

    let apres = relire_instantane(&db_path, 1);
    assert_eq!(
        apres.items.len(),
        0,
        "une file que l'utilisateur a VIDÉE doit s'écrire vide : sinon elle \
         réapparaît au redémarrage"
    );
    assert_eq!(apres.local_track_ids.len(), 0);
    assert_eq!(apres.streaming_tracks.len(), 0);
}

// ---------------------------------------------------------------------------
// 3. Le retrait du dernier titre : la même chose, par le chemin de l'utilisateur.
// ---------------------------------------------------------------------------

/// La route `queue_clear` supprime carrément le fichier ; mais retirer les
/// titres UN À UN passe, lui, par `save_queue`. Le dernier retrait doit donc
/// bien écrire un instantané vide.
#[test]
fn le_retrait_du_dernier_titre_ecrit_bien_une_file_vide_i2569() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dossier.path().join("tune.db");
    let db_path = chemin.to_str().unwrap().to_string();

    let db = ouvrir(&db_path);
    let repo = PlayQueueRepo::with_backend(db.clone());
    repo.append(1, &album_de(2)).expect("remplir la file");
    save_queue(&db, &db_path, 1, &etat_zone());
    assert_eq!(relire_instantane(&db_path, 1).items.len(), 2);

    assert!(
        repo.remove_pos(1, 1).expect("retrait"),
        "second titre retiré"
    );
    save_queue(&db, &db_path, 1, &ZoneState::default());
    assert_eq!(relire_instantane(&db_path, 1).items.len(), 1);

    assert!(
        repo.remove_pos(1, 0).expect("retrait"),
        "dernier titre retiré"
    );
    save_queue(&db, &db_path, 1, &ZoneState::default());
    assert_eq!(
        relire_instantane(&db_path, 1).items.len(),
        0,
        "le dernier retrait doit bien vider l'instantané"
    );
}

// ---------------------------------------------------------------------------
// 4. #2775 : la perte est à l'ÉCRITURE, la relecture est fidèle.
// ---------------------------------------------------------------------------

/// Sandro (#2775) décrit le même symptôme APRÈS un redémarrage. Ce test sépare
/// les deux chemins : il écrit sous une base illisible, ferme la base, la
/// rouvre, et relit.
///
/// Si la relecture était en cause, l'instantané conservé ne reviendrait pas
/// entier. Il revient entier — donc `restore_all_queues` est fidèle, et le seul
/// endroit où douze titres peuvent devenir un est l'ÉCRITURE.
#[test]
fn l_instantane_conserve_survit_a_une_fermeture_et_une_reouverture_i2775() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dossier.path().join("tune.db");
    let db_path = chemin.to_str().unwrap().to_string();

    {
        let db = ouvrir(&db_path);
        let repo = PlayQueueRepo::with_backend(db.clone());
        repo.append(1, &album_de(12)).expect("remplir la file");
        save_queue(&db, &db_path, 1, &etat_zone());

        // La base devient illisible juste avant l'arrêt du serveur.
        let occupee: Arc<dyn DbBackend> = Arc::new(BaseOccupee {
            interne: db.clone(),
        });
        save_queue(&occupee, &db_path, 1, &etat_zone());

        // Puis la file est perdue en base — ce que produit le `clear` de la
        // route de lecture, ou une base repartie de zéro.
        repo.clear(1).expect("vider la file en base");
    } // serveur arrêté, base fermée.

    // Redémarrage : le serveur rouvre la base depuis le DISQUE et restaure.
    let db = ouvrir(&db_path);
    tune_core::queue_persistence::restore_all_queues(&db, &db_path);

    let repo = PlayQueueRepo::with_backend(db.clone());
    let restauree = repo.get_ordered(1).expect("relecture après redémarrage");
    assert_eq!(
        restauree.len(),
        12,
        "la relecture rend fidèlement ce que l'écriture a laissé : #2775 se \
         joue à l'écriture, pas à la restauration"
    );
    assert_eq!(restauree[0].source_id.as_deref(), Some("q0"));
    assert_eq!(restauree[11].source_id.as_deref(), Some("q11"));
}

// ---------------------------------------------------------------------------
// 5. Le DÉSORDRE d'écriture : deux persistances qui se marchent dessus.
// ---------------------------------------------------------------------------

/// 🔴 `persist_queue_async` fait `tokio::spawn` puis `spawn_blocking` : deux
/// persistances lancées coup sur coup n'ont AUCUN ordre garanti, et leurs deux
/// tâches bloquantes écrivaient le MÊME fichier en même temps.
///
/// Mesuré avant le correctif, avec ce montage exact : **19 fichiers illisibles
/// sur 901 lectures** (2,1 %). `std::fs::write` tronque puis écrit ; deux
/// écritures concurrentes de longueurs différentes laissent un JSON déchiré. Au
/// redémarrage, `restore_all_queues` échoue sur `queue_restore_parse_failed` et
/// passe la zone SANS BRUIT : l'instantané est perdu, et l'utilisateur retrouve
/// la file que la base contient — le seul titre en cours.
///
/// L'écriture par renommage rend chaque persistance indivisible. L'assertion
/// est donc DÉTERMINISTE dans le sens vert : un renommage ne peut pas produire
/// un fichier partiel. Ce test ne peut pas devenir intermittemment rouge.
#[test]
fn deux_persistances_concurrentes_ne_dechirent_jamais_l_instantane_i2569() {
    let dossier = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dossier.path().join("tune.db");
    let db_path = chemin.to_str().unwrap().to_string();

    let db = ouvrir(&db_path);
    let repo = PlayQueueRepo::with_backend(db.clone());
    repo.append(1, &album_de(60)).expect("remplir la file");
    // Un premier instantané complet sur le disque : le lecteur a toujours
    // quelque chose à lire, et toute lecture illisible ensuite est une déchirure
    // et rien d'autre.
    save_queue(&db, &db_path, 1, &ZoneState::default());

    let fichier = fichier_instantane(&db_path, 1);
    let dechire = Arc::new(AtomicUsize::new(0));
    let lectures = Arc::new(AtomicUsize::new(0));
    let ecrivains_finis = Arc::new(AtomicUsize::new(0));

    // La forme réelle : un client fait varier la TAILLE de la file (donc la
    // longueur du JSON) pendant que plusieurs routes persistent.
    std::thread::scope(|s| {
        let db_m = db.clone();
        let fin_m = ecrivains_finis.clone();
        s.spawn(move || {
            let repo = PlayQueueRepo::with_backend(db_m);
            for i in 0..150 {
                let _ = repo.clear(1);
                let _ = repo.append(1, &album_de(if i % 2 == 0 { 60 } else { 2 }));
            }
            fin_m.fetch_add(1, Ordering::Release);
        });

        for _ in 0..3 {
            let db_e = db.clone();
            let p = db_path.clone();
            let fin_e = ecrivains_finis.clone();
            s.spawn(move || {
                for _ in 0..150 {
                    save_queue(&db_e, &p, 1, &ZoneState::default());
                }
                fin_e.fetch_add(1, Ordering::Release);
            });
        }

        // Le lecteur suit les écrivains au lieu de compter ses tours : sans ça
        // il finit avant eux et ne mesure rien.
        let f = fichier.clone();
        let d = dechire.clone();
        let l = lectures.clone();
        let fin_l = ecrivains_finis.clone();
        s.spawn(move || {
            while fin_l.load(Ordering::Acquire) < 4 {
                if let Ok(brut) = std::fs::read_to_string(&f) {
                    l.fetch_add(1, Ordering::Relaxed);
                    if serde_json::from_str::<QueueSnapshot>(&brut).is_err() {
                        d.fetch_add(1, Ordering::Relaxed);
                    }
                }
                std::thread::yield_now();
            }
        });
    });

    assert!(
        lectures.load(Ordering::Relaxed) > 0,
        "le montage doit vraiment lire le fichier"
    );
    assert_eq!(
        dechire.load(Ordering::Relaxed),
        0,
        "un instantané déchiré est SILENCIEUSEMENT perdu au redémarrage \
         (queue_restore_parse_failed) : l'écriture doit être indivisible"
    );

    // Et rien ne doit rester derrière : un temporaire abandonné dans
    // `queue_state/` serait ignoré par la restauration, mais s'accumulerait.
    let restes: Vec<_> = std::fs::read_dir(fichier.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
        .collect();
    assert!(
        restes.is_empty(),
        "aucun fichier temporaire ne doit survivre : {restes:?}"
    );
}
