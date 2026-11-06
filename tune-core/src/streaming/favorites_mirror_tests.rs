//! Témoins du miroir des favoris de service (#5997), contre un service
//! SIMULÉ : aucun appel réseau, l'état du « compte » est une table en mémoire
//! que le test lit et modifie comme le ferait l'application du service.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::db::profile_repo::ProfileRepo;
use crate::db::sqlite::SqliteDb;
use crate::error::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
};

/// #6399 — 7 000 favoris, trois profils : une confirmation ne doit pas
/// parcourir les 21 000 lignes. Le compteur SQLite mesure le travail réel de
/// la requête de production, indépendamment de la charge de la machine.
#[test]
fn confirmer_un_favori_ne_balaie_pas_tout_le_miroir_6399() {
    use rusqlite::{StatementStatus, params};

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "WITH RECURSIVE numeros(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM numeros WHERE n < 7000),
         profils(p) AS (VALUES (1), (2), (3))
         INSERT INTO streaming_favorites(profile_id, item_type, service, service_id, miroir_etat)
         SELECT p, 'track', 'qobuz', CAST(n AS TEXT), 'ajout_en_attente' FROM numeros CROSS JOIN profils;
         INSERT INTO streaming_favorites(profile_id, item_type, service, service_id, miroir_etat)
         VALUES (1, 'track', 'tidal', '3500', 'ajout_en_attente'),
                (1, 'album', 'qobuz', '3500', 'ajout_en_attente');",
    )
    .unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    let sql = sql_poser_etat(&backend);
    let conn = db.connection().lock().unwrap();
    let mut stmt = conn.prepare(&sql).unwrap();
    assert_eq!(
        stmt.execute(params![
            ETAT_SYNCHRO,
            Option::<String>::None,
            "qobuz",
            "track",
            "3500"
        ])
        .unwrap(),
        3,
        "la confirmation doit atteindre chaque profil du même favori"
    );
    let balayages = stmt.get_status(StatementStatus::FullscanStep);
    assert_eq!(
        balayages, 0,
        "confirmer un favori ne doit pas balayer tout le miroir (#6399)"
    );
    assert!(
        stmt.get_status(StatementStatus::VmStep) < 500,
        "la confirmation doit rester bornée au favori des trois profils"
    );
    let autres: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM streaming_favorites WHERE service_id = '3500' AND miroir_etat = 'ajout_en_attente'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(autres, 2, "autre service et autre type restent intacts");
    drop(stmt);
    drop(conn);

    // Une base déjà à la dernière version récupère aussi l'index au démarrage.
    db.execute_batch("DROP INDEX idx_streaming_favorites_miroir")
        .unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let conn = db.connection().lock().unwrap();
    let mut stmt = conn.prepare(&sql).unwrap();
    assert_eq!(
        stmt.execute(params![
            ETAT_SYNCHRO,
            Option::<String>::None,
            "qobuz",
            "track",
            "3501"
        ])
        .unwrap(),
        3
    );
    assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
}

/// #6399 — un vrai déclencheur SQLite observe le fil qui écrit. Aucune
/// hypothèse sur la vitesse du processeur ni délai d'ordonnancement : une
/// écriture sur le fil de l'exécuteur est refusée par la base elle-même.
#[tokio::test(flavor = "current_thread")]
async fn la_reconciliation_sql_quitte_le_fil_asynchrone_6399() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("miroir.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    let (arc, compte) = ServiceSimule::nouveau("temoin-fil-6399");
    poser_chez_le_service(&compte, "tracks", "71");

    let fil_asynchrone = std::thread::current().id();
    db.connection()
        .lock()
        .unwrap()
        .create_scalar_function(
            "hors_executeur_6399",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| Ok(i64::from(std::thread::current().id() != fil_asynchrone)),
        )
        .unwrap();
    db.execute_batch(
        "CREATE TRIGGER verifier_fil_miroir_6399 BEFORE INSERT ON streaming_favorites
         BEGIN
           SELECT CASE WHEN hors_executeur_6399() = 0
             THEN RAISE(FAIL, 'le miroir ecrit sur le fil asynchrone (#6399)') END;
         END;",
    )
    .unwrap();

    let bilan = rafraichir(&arc, &backend, 1).await;
    assert_eq!(
        bilan.echecs, 0,
        "la reconciliation SQL doit quitter le fil asynchrone : {:?}",
        bilan.erreur
    );
    assert_eq!(bilan.ajoutes, 1, "le favori a réellement été écrit");
    assert!(
        StreamingFavoritesRepo::with_backend(backend)
            .is_favorite(1, "track", "temoin-fil-6399", "71")
            .unwrap()
    );
}

/// Un favori chez le service : (titre, artiste, album, isrc).
type Favori = (String, String, String, Option<String>);

/// Le compte chez le service : `type pluriel → id → (titre, artiste, album, isrc)`.
#[derive(Default)]
struct Compte {
    favoris: BTreeMap<String, BTreeMap<String, Favori>>,
    /// Panne réseau : toute lecture et toute écriture échouent.
    panne: bool,
    /// Journal des écritures reçues : `"add tracks 71"`, `"remove albums 9"`.
    appels: Vec<String>,
    /// Jamais connecté (#6369) : `auth_status` le dit, `utilisable` suit.
    deconnecte: bool,
    /// #6461 — le service sait-il dire quelles playlists le compte suit ?
    relit_playlists: bool,
    /// La bibliothèque de playlists du compte : `id → editable` (`Some(true)`
    /// possédée, `Some(false)` suivie, `None` auteur inconnu).
    playlists: BTreeMap<String, Option<bool>>,
    /// La lecture des playlists se déclare incomplète (#6461, relecture).
    playlists_tronquees: bool,
    /// #5997 — les favoris (titres, albums, artistes) ne sont servis qu'en
    /// partie : la lecture datée d'affichage rend le PREMIER seulement, comme
    /// un connecteur réel rend la coupe ; la lecture complète le dit (`Err`).
    favoris_tronques: bool,
    /// Identifiants dont le service refuse l'ajout en 401 (radio-qobuz,
    /// 01/11/2026) — en plus des playlists possédées, que Qobuz refuse
    /// toujours à l'abonnement.
    refusees: std::collections::HashSet<String>,
}

struct ServiceSimule {
    nom: &'static str,
    compte: Arc<Mutex<Compte>>,
}

impl ServiceSimule {
    fn nouveau(nom: &'static str) -> (ServiceArc, Arc<Mutex<Compte>>) {
        let compte = Arc::new(Mutex::new(Compte::default()));
        let svc: Box<dyn StreamingService> = Box::new(ServiceSimule {
            nom,
            compte: compte.clone(),
        });
        (Arc::new(RwLock::new(svc)), compte)
    }
}

fn poser_chez_le_service(compte: &Arc<Mutex<Compte>>, fav_type: &str, id: &str) {
    compte
        .lock()
        .unwrap()
        .favoris
        .entry(fav_type.into())
        .or_default()
        .insert(
            id.into(),
            (
                format!("titre {id}"),
                "artiste".into(),
                "album".into(),
                None,
            ),
        );
}

fn ids_chez_le_service(compte: &Arc<Mutex<Compte>>, fav_type: &str) -> Vec<String> {
    compte
        .lock()
        .unwrap()
        .favoris
        .get(fav_type)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn panne(compte: &Arc<Mutex<Compte>>, oui: bool) {
    compte.lock().unwrap().panne = oui;
}

#[async_trait]
impl StreamingService for ServiceSimule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    fn favoris_miroir(&self) -> bool {
        true
    }
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(Default::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: !self.compte.lock().unwrap().deconnecte,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn playlists_de_la_bibliotheque(&self) -> Option<Result<Vec<StreamPlaylist>, TuneError>> {
        let (relit, tronquee) = {
            let c = self.compte.lock().unwrap();
            (c.relit_playlists, c.playlists_tronquees)
        };
        if !relit {
            return None;
        }
        if tronquee {
            return Some(Err("lecture incomplète simulée".into()));
        }
        Some(self.get_user_playlists().await)
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        let c = self.compte.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        Ok(c.playlists
            .iter()
            .map(|(id, editable)| StreamPlaylist {
                id: id.clone(),
                name: format!("playlist {id}"),
                description: None,
                cover_path: None,
                track_count: 0,
                owner: None,
                editable: *editable,
                covers: Vec::new(),
            })
            .collect())
    }
    /// La projection sérialisée qu'un vrai connecteur rend (`source_id`,
    /// `artist_name`, `album_title`) — celle que la reprise sait lire.
    async fn get_user_favorites_dated(
        &self,
        fav_type: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, TuneError> {
        let c = self.compte.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        let coupe = if c.favoris_tronques { 1 } else { usize::MAX };
        Ok(Some(
            c.favoris
                .get(fav_type)
                .map(|m| {
                    m.iter()
                        .take(coupe)
                        .map(|(id, (t, a, al, isrc))| match fav_type {
                            "artists" => json!({"id": id, "name": t}),
                            // #6482 — l'année et la qualité, comme la
                            // projection `StreamAlbum` d'un vrai connecteur.
                            "albums" => json!({"source_id": id, "title": t, "artist_name": a,
                                               "year": 1959, "quality": {"codec": "FLAC",
                                               "sample_rate": 192000, "bit_depth": 24}}),
                            _ => json!({"source_id": id, "title": t, "artist_name": a,
                                        "album_title": al, "isrc": isrc}),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ))
    }
    /// #5997 — la lecture que réconcilie le miroir : `Err` quand le compte
    /// n'est servi qu'en partie, la liste entière sinon.
    async fn favoris_dates_complets(
        &self,
        fav_type: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, TuneError> {
        if self.compte.lock().unwrap().favoris_tronques {
            return Err("lecture des favoris incomplète simulée".into());
        }
        self.get_user_favorites_dated(fav_type).await
    }
    // Le repli typé n'est atteint qu'en panne (la lecture datée a échoué).
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Err("panne réseau simulée".into())
    }
    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let mut c = self.compte.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        c.appels.push(format!("add {fav_type} {item_id}"));
        // Le refus réel relevé sur le .18 : s'abonner à SA playlist.
        if c.refusees.contains(item_id)
            || (fav_type == "playlists" && c.playlists.get(item_id) == Some(&Some(true)))
        {
            return Err(format!(
                "simule /playlist/subscribe: 401 {{\"status\":\"error\",\"code\":401,\"message\":\"Current authenticated user is not authorized to perform this request\"}}"
            )
            .into());
        }
        if fav_type == "playlists" {
            c.playlists.insert(item_id.into(), Some(false));
        }
        c.favoris.entry(fav_type.into()).or_default().insert(
            item_id.into(),
            (
                format!("titre {item_id}"),
                "artiste".into(),
                "album".into(),
                None,
            ),
        );
        Ok(())
    }
    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let mut c = self.compte.lock().unwrap();
        if c.panne {
            return Err("panne réseau simulée".into());
        }
        c.appels.push(format!("remove {fav_type} {item_id}"));
        if fav_type == "playlists" && c.playlists.get(item_id) == Some(&Some(false)) {
            c.playlists.remove(item_id);
        }
        if let Some(m) = c.favoris.get_mut(fav_type) {
            m.remove(item_id);
        }
        Ok(())
    }
}

/// Une base SUR DISQUE (pas `:memory:`, dont le pool de lecture ne voit pas
/// les écritures), avec ses profils.
fn base(profils: usize) -> (Arc<dyn DbBackend>, Vec<i64>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ProfileRepo::with_backend(backend.clone());
    // `init_schema` peut déjà poser un profil par défaut : on part de ce qui
    // existe et on complète jusqu'au nombre voulu.
    let mut ids: Vec<i64> = repo
        .list()
        .unwrap()
        .into_iter()
        .filter_map(|p| p.id)
        .collect();
    let mut n = 0;
    while ids.len() < profils {
        n += 1;
        ids.push(repo.create(&format!("profil-{n}"), None, None).unwrap());
    }
    ids.truncate(profils.max(1));
    (backend, ids, dir)
}

fn favori(item_type: &str, id: &str) -> FavoriMiroir {
    FavoriMiroir {
        item_type: item_type.into(),
        service_id: id.into(),
        title: Some(format!("titre {id}")),
        artist: Some("artiste".into()),
        album: Some("album".into()),
        ..Default::default()
    }
}

fn liste(backend: &Arc<dyn DbBackend>, pid: i64) -> Vec<(String, String, Option<String>)> {
    StreamingFavoritesRepo::with_backend(backend.clone())
        .list(pid, None)
        .unwrap()
        .into_iter()
        .map(|f| (f.service, f.service_id, f.miroir_etat))
        .collect()
}

fn ids(backend: &Arc<dyn DbBackend>, pid: i64) -> Vec<String> {
    let mut v: Vec<String> = liste(backend, pid)
        .into_iter()
        .map(|(_, id, _)| id)
        .collect();
    v.sort();
    v
}

/// Témoin 1 — un cœur posé dans Tune est un ajout CHEZ le service ; un
/// retrait dans Tune est un retrait chez le service.
#[tokio::test]
async fn ajout_puis_retrait_dans_tune_sont_propages_au_service_5997() {
    let (backend, profils, _dir) = base(1);
    let (arc, compte) = ServiceSimule::nouveau("miroir-t1");

    let p = ajouter(&arc, &backend, profils[0], &favori("track", "71"))
        .await
        .unwrap();
    assert_eq!(p, Propagation::Propage);
    assert_eq!(
        ids_chez_le_service(&compte, "tracks"),
        vec!["71"],
        "l'ajout n'est pas arrivé chez le service"
    );
    assert_eq!(
        liste(&backend, profils[0]),
        vec![("miroir-t1".into(), "71".into(), Some(ETAT_SYNCHRO.into()))]
    );

    let p = retirer(&arc, &backend, "track", "71").await.unwrap();
    assert_eq!(p, Propagation::Propage);
    assert!(
        ids_chez_le_service(&compte, "tracks").is_empty(),
        "le retrait n'est pas arrivé chez le service"
    );
    assert!(liste(&backend, profils[0]).is_empty());
    assert_eq!(
        compte.lock().unwrap().appels,
        vec!["add tracks 71", "remove tracks 71"]
    );
}

/// Témoin 1 bis — une playlist passe par `add_favorite("playlists")`, que le
/// connecteur Qobuz route vers `/playlist/subscribe` (#2370), et un
/// rafraîchissement ne la retire pas : les playlists ne sont pas relues.
#[tokio::test]
async fn une_playlist_est_poussee_et_jamais_retiree_par_le_rafraichissement_5997() {
    let (backend, profils, _dir) = base(1);
    let (arc, compte) = ServiceSimule::nouveau("miroir-t1b");
    ajouter(&arc, &backend, profils[0], &favori("playlist", "15732665"))
        .await
        .unwrap();
    assert_eq!(
        compte.lock().unwrap().appels,
        vec!["add playlists 15732665"]
    );
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 0);
    assert_eq!(ids(&backend, profils[0]), vec!["15732665"]);
}

/// Témoin 2 — un favori retiré dans l'application du service disparaît de
/// Tune au rafraîchissement ; un favori posé là-bas y apparaît.
#[tokio::test]
async fn un_favori_retire_chez_le_service_disparait_au_rafraichissement_5997() {
    let (backend, profils, _dir) = base(1);
    let (arc, compte) = ServiceSimule::nouveau("miroir-t2");
    poser_chez_le_service(&compte, "tracks", "71");
    poser_chez_le_service(&compte, "albums", "kob");

    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.ajoutes, 2, "{bilan:?}");
    assert_eq!(ids(&backend, profils[0]), vec!["71", "kob"]);

    // Fabien retire la piste dans l'application Qobuz, en pose une autre.
    compte
        .lock()
        .unwrap()
        .favoris
        .get_mut("tracks")
        .unwrap()
        .remove("71");
    poser_chez_le_service(&compte, "tracks", "72");

    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 1, "{bilan:?}");
    assert_eq!(
        ids(&backend, profils[0]),
        vec!["72", "kob"],
        "le favori retiré chez le service est resté dans Tune"
    );
    assert_eq!(etat("miroir-t2").statut, "ok");
}

/// Témoin 3 — les favoris en miroir sont COMMUNS à tous les profils : posé
/// par l'un, vu par l'autre ; retiré par l'autre, disparu pour l'un ; un
/// profil créé après coup les reçoit à sa première lecture.
#[tokio::test]
async fn les_favoris_en_miroir_sont_partages_entre_profils_5997() {
    let (backend, profils, _dir) = base(2);
    let (a, b) = (profils[0], profils[1]);
    let (arc, compte) = ServiceSimule::nouveau("miroir-t3");

    ajouter(&arc, &backend, a, &favori("album", "kob"))
        .await
        .unwrap();
    assert_eq!(
        ids(&backend, b),
        vec!["kob"],
        "le profil B ne voit pas le favori posé par A"
    );

    poser_chez_le_service(&compte, "tracks", "71");
    rafraichir(&arc, &backend, b).await;
    assert_eq!(
        ids(&backend, a),
        vec!["71", "kob"],
        "le favori venu du service n'atteint pas A"
    );

    retirer(&arc, &backend, "album", "kob").await.unwrap();
    assert_eq!(ids(&backend, a), vec!["71"]);
    assert_eq!(ids(&backend, b), vec!["71"]);

    let c = ProfileRepo::with_backend(backend.clone())
        .create("arrive-apres", None, None)
        .unwrap();
    assert!(ids(&backend, c).is_empty());
    aligner_profil(&backend, c, &["miroir-t3".to_string()]).unwrap();
    assert_eq!(
        ids(&backend, c),
        vec!["71"],
        "un profil créé après coup ne reçoit pas les favoris communs"
    );
}

/// Témoin 4 — les favoris LOCAUX et ceux d'un service HORS miroir restent
/// propres à chaque profil, quoi que fasse le miroir.
#[tokio::test]
async fn les_favoris_locaux_et_hors_miroir_restent_intacts_5997() {
    let (backend, profils, _dir) = base(2);
    let (a, b) = (profils[0], profils[1]);
    let profils_repo = ProfileRepo::with_backend(backend.clone());
    profils_repo.add_favorite(a, "track", 1).unwrap();
    profils_repo.add_favorite(b, "album", 2).unwrap();
    let sf = StreamingFavoritesRepo::with_backend(backend.clone());
    sf.add(
        a,
        "track",
        "bandcamp",
        "https://x.bandcamp.com/track/y",
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let (arc, compte) = ServiceSimule::nouveau("miroir-t4");
    poser_chez_le_service(&compte, "tracks", "71");
    ajouter(&arc, &backend, a, &favori("album", "kob"))
        .await
        .unwrap();
    rafraichir(&arc, &backend, a).await;
    retirer(&arc, &backend, "album", "kob").await.unwrap();

    let locaux = |pid| {
        profils_repo
            .list_favorites(pid, None)
            .unwrap()
            .into_iter()
            .map(|f| (f.item_type, f.item_id))
            .collect::<Vec<_>>()
    };
    assert_eq!(locaux(a), vec![("track".to_string(), 1)]);
    assert_eq!(locaux(b), vec![("album".to_string(), 2)]);
    let services = |pid| {
        let mut v: Vec<String> = liste(&backend, pid)
            .into_iter()
            .map(|(s, id, _)| format!("{s}:{id}"))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        services(a),
        vec!["bandcamp:https://x.bandcamp.com/track/y", "miroir-t4:71"]
    );
    assert_eq!(
        services(b),
        vec!["miroir-t4:71"],
        "le favori Bandcamp de A a fui vers B"
    );
    // Même si un appelant nommait Bandcamp parmi les miroirs, une ligne HORS
    // miroir (`miroir_etat` NULL) n'est jamais recopiée à un autre profil.
    aligner_profil(
        &backend,
        b,
        &["miroir-t4".to_string(), "bandcamp".to_string()],
    )
    .unwrap();
    assert_eq!(
        services(b),
        vec!["miroir-t4:71"],
        "le favori Bandcamp de A a fui vers B par l'alignement"
    );
}

/// Témoin 5 — une panne du service ne perd RIEN et le DIT : l'ajout reste en
/// attente avec son motif, un retrait en attente est masqué, une lecture
/// ratée ne retire aucun favori, et le retour du service rattrape tout.
#[tokio::test]
async fn une_panne_du_service_ne_perd_rien_et_le_dit_5997() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (arc, compte) = ServiceSimule::nouveau("miroir-t5");
    poser_chez_le_service(&compte, "tracks", "70");
    rafraichir(&arc, &backend, pid).await;
    assert_eq!(ids(&backend, pid), vec!["70"]);

    panne(&compte, true);
    let p = ajouter(&arc, &backend, pid, &favori("track", "71"))
        .await
        .unwrap();
    let Propagation::EnAttente(motif) = p else {
        panic!("un échec du service annoncé comme propagé");
    };
    assert!(motif.contains("panne"), "motif perdu : {motif}");
    assert_eq!(
        Propagation::EnAttente(motif.clone()).en_json("miroir-t5")["statut"],
        "en_attente"
    );
    let fav = StreamingFavoritesRepo::with_backend(backend.clone())
        .list(pid, None)
        .unwrap()
        .into_iter()
        .find(|f| f.service_id == "71")
        .expect("le cœur posé pendant la panne a été perdu");
    assert_eq!(fav.miroir_etat.as_deref(), Some(ETAT_AJOUT_EN_ATTENTE));
    assert!(fav.miroir_erreur.unwrap_or_default().contains("panne"));

    let p = retirer(&arc, &backend, "track", "70").await.unwrap();
    assert!(matches!(p, Propagation::EnAttente(_)));
    assert_eq!(
        ids(&backend, pid),
        vec!["71"],
        "le retrait en attente doit être masqué"
    );

    // Rafraîchissement pendant la panne : rien n'est retiré, l'échec est dit.
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.echecs, 3, "{bilan:?}");
    assert_eq!(bilan.retires, 0);
    assert_eq!(bilan.en_attente, 2);
    let e = etat("miroir-t5");
    assert_eq!(e.statut, "echec");
    assert_eq!(e.en_attente, 2);
    assert!(e.erreur.unwrap_or_default().contains("panne"));
    assert_eq!(ids(&backend, pid), vec!["71"]);

    // Le service revient : l'ajout et le retrait en attente sont poussés.
    panne(&compte, false);
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.pousses, 2, "ajout ET retrait en attente : {bilan:?}");
    assert_eq!(bilan.en_attente, 0);
    assert_eq!(ids_chez_le_service(&compte, "tracks"), vec!["71"]);
    assert_eq!(
        liste(&backend, pid),
        vec![("miroir-t5".into(), "71".into(), Some(ETAT_SYNCHRO.into()))]
    );
}

/// Témoin 6 — une ligne d'avant la rc4 (`miroir_etat` NULL) que le service ne
/// connaît pas n'est PAS effacée : elle est poussée au service.
#[tokio::test]
async fn une_ligne_d_avant_la_rc4_est_adoptee_et_poussee_pas_effacee_5997() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    StreamingFavoritesRepo::with_backend(backend.clone())
        .add(
            pid,
            "album",
            "miroir-t6",
            "ancien",
            Some("Ancien"),
            None,
            None,
            None,
        )
        .unwrap();
    let (arc, compte) = ServiceSimule::nouveau("miroir-t6");
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert_eq!(ids_chez_le_service(&compte, "albums"), vec!["ancien"]);
    assert_eq!(
        liste(&backend, pid),
        vec![(
            "miroir-t6".into(),
            "ancien".into(),
            Some(ETAT_SYNCHRO.into())
        )]
    );
}

/// Le TTL : périmé jamais vu, frais après un passage, périmé après
/// `invalider` (écriture directe chez le service).
#[tokio::test]
async fn le_cache_court_se_perime_et_s_invalide_5997() {
    let (backend, profils, _dir) = base(1);
    let (arc, _compte) = ServiceSimule::nouveau("miroir-t7");
    let ttl = Duration::from_secs(60);
    assert!(est_perime("miroir-t7", ttl));
    assert!(
        rafraichir_si_perime(&arc, &backend, profils[0], false)
            .await
            .is_some()
    );
    assert!(!est_perime("miroir-t7", ttl));
    assert!(
        rafraichir_si_perime(&arc, &backend, profils[0], false)
            .await
            .is_none()
    );
    invalider("miroir-t7");
    assert!(est_perime("miroir-t7", ttl));
}

/// Le même parcours sur PostgreSQL (#5997) : écriture pour tous les profils,
/// alignement d'un profil (INSERT … SELECT … ON CONFLICT), retrait au
/// rafraîchissement, panne. Prouve le SQL du miroir et les colonnes posées par
/// `ENSURE_TABLES` / `ENSURE_COLUMNS` sur le vrai moteur.
///
/// Exige `TUNE_TEST_PG_URL` (étape dédiée de `test-postgres.yml`). Une
/// variable posée dont la connexion échoue FAIT TOMBER l'épreuve.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_5997_favoris_miroir_sur_postgresql() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!(
            "SAUT : TUNE_TEST_PG_URL non posée — pg_5997 rend la main sans toucher aucune base."
        );
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap_or_else(|e| {
        panic!("TUNE_TEST_PG_URL est POSÉE ({url}) mais la connexion échoue : {e}")
    });
    let backend: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = backend.execute(sql, &[]);
    }
    backend
        .execute(
            "DELETE FROM streaming_favorites WHERE service = 'miroir-pg'",
            &[],
        )
        .unwrap();
    // La colonne `profile_id` telle que la laisse une base de bascule (TEXT,
    // `PG_FULL_SCHEMA`) : c'est le cas qui tombait en CI (« text = bigint »).
    // Posé ICI, l'épreuve ne dépend plus de l'ordre des étapes ; le type
    // d'origine est remis à la fin.
    let type_avant = backend
        .query_one(
            "SELECT data_type FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name = 'streaming_favorites' AND column_name = 'profile_id'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap_or_default();
    backend
        .execute_batch(
            "ALTER TABLE streaming_favorites ALTER COLUMN profile_id DROP DEFAULT; \
             ALTER TABLE streaming_favorites ALTER COLUMN profile_id TYPE TEXT USING profile_id::text",
        )
        .unwrap();
    let profils_repo = ProfileRepo::with_backend(backend.clone());
    let suffixe = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let a = profils_repo
        .create(&format!("pg5997-a-{suffixe}"), None, None)
        .unwrap();
    let b = profils_repo
        .create(&format!("pg5997-b-{suffixe}"), None, None)
        .unwrap();

    let (arc, compte) = ServiceSimule::nouveau("miroir-pg");
    assert_eq!(
        ajouter(&arc, &backend, a, &favori("album", "kob"))
            .await
            .unwrap(),
        Propagation::Propage
    );
    assert_eq!(
        ids(&backend, b),
        vec!["kob"],
        "PG : B ne voit pas le favori posé par A"
    );

    poser_chez_le_service(&compte, "tracks", "71");
    let bilan = rafraichir(&arc, &backend, a).await;
    assert_eq!(bilan.ajoutes, 1, "{bilan:?}");
    compte
        .lock()
        .unwrap()
        .favoris
        .get_mut("albums")
        .unwrap()
        .remove("kob");
    let bilan = rafraichir(&arc, &backend, a).await;
    assert_eq!(bilan.retires, 1, "{bilan:?}");
    assert_eq!(ids(&backend, a), vec!["71"]);

    // Un profil sans ses lignes les reçoit par l'alignement.
    backend
        .execute(
            "DELETE FROM streaming_favorites WHERE service = 'miroir-pg' AND profile_id::text = ($1)::text",
            &[&b],
        )
        .unwrap();
    assert_eq!(
        aligner_profil(&backend, b, &["miroir-pg".to_string()]).unwrap(),
        1
    );
    assert_eq!(ids(&backend, b), vec!["71"]);

    panne(&compte, true);
    assert!(matches!(
        ajouter(&arc, &backend, b, &favori("track", "72"))
            .await
            .unwrap(),
        Propagation::EnAttente(_)
    ));
    let bilan = rafraichir(&arc, &backend, b).await;
    assert_eq!(bilan.retires, 0);
    assert_eq!(bilan.en_attente, 1);
    assert_eq!(ids(&backend, a), vec!["71", "72"]);
    panne(&compte, false);
    let bilan = rafraichir(&arc, &backend, b).await;
    assert_eq!(bilan.en_attente, 0, "{bilan:?}");
    assert_eq!(ids_chez_le_service(&compte, "tracks"), vec!["71", "72"]);

    backend
        .execute(
            "DELETE FROM streaming_favorites WHERE service = 'miroir-pg'",
            &[],
        )
        .unwrap();
    let _ = profils_repo.delete(a);
    let _ = profils_repo.delete(b);
    if type_avant == "bigint" {
        backend
            .execute_batch(
                "ALTER TABLE streaming_favorites ALTER COLUMN profile_id TYPE BIGINT USING profile_id::bigint; \
                 ALTER TABLE streaming_favorites ALTER COLUMN profile_id SET DEFAULT 1",
            )
            .unwrap();
    }
}

/// #6369 — un service en miroir jamais connecté (Tidal, toujours au
/// registre) n'est pas un échec : Qobuz rafraîchi sans erreur et Tidal non
/// connecté publient `ok`, pas `echec`. Fil 2279 : le bandeau « le dernier
/// rafraîchissement a échoué » restait affiché en permanence.
#[tokio::test]
async fn un_service_non_connecte_n_est_pas_un_echec_6369() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (qobuz, _) = ServiceSimule::nouveau("miroir-6369-qobuz");
    let (tidal, compte_tidal) = ServiceSimule::nouveau("miroir-6369-tidal");
    compte_tidal.lock().unwrap().deconnecte = true;

    let bilan = rafraichir(&qobuz, &backend, pid).await;
    assert_eq!(bilan.echecs, 0, "{bilan:?}");
    let bilan = rafraichir(&tidal, &backend, pid).await;
    assert_eq!(bilan.echecs, 0, "rien n'a été tenté : {bilan:?}");
    assert_eq!(etat("miroir-6369-tidal").statut, STATUT_NON_CONNECTE);

    let noms = vec![
        "miroir-6369-qobuz".to_string(),
        "miroir-6369-tidal".to_string(),
    ];
    assert_eq!(
        statut_global(&noms),
        "ok",
        "Tidal non connecté fait publier un échec alors que Qobuz s'est rafraîchi"
    );
    // Seul le service non connecté : « aucun service en miroir n'est connecté ».
    assert_eq!(statut_global(&noms[1..]), "aucun");
}

/// #6369 — un service CONNECTÉ en panne reste un échec, même à côté d'un
/// service non connecté.
#[tokio::test]
async fn une_panne_d_un_service_connecte_reste_un_echec_6369() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (qobuz, compte_qobuz) = ServiceSimule::nouveau("miroir-6369b-qobuz");
    let (tidal, compte_tidal) = ServiceSimule::nouveau("miroir-6369b-tidal");
    compte_tidal.lock().unwrap().deconnecte = true;
    panne(&compte_qobuz, true);
    rafraichir(&qobuz, &backend, pid).await;
    rafraichir(&tidal, &backend, pid).await;
    let noms = vec![
        "miroir-6369b-qobuz".to_string(),
        "miroir-6369b-tidal".to_string(),
    ];
    assert_eq!(statut_global(&noms), "echec");
}

// ---------------------------------------------------------------------------
// #6461 — playlists suivies relues (FabienM, fil 2302, point 1)
// ---------------------------------------------------------------------------

fn bibliotheque_de_playlists(compte: &Arc<Mutex<Compte>>, playlists: &[(&str, Option<bool>)]) {
    let mut c = compte.lock().unwrap();
    c.relit_playlists = true;
    for (id, editable) in playlists {
        c.playlists.insert((*id).into(), *editable);
    }
}

/// Témoin 6461-1 — une playlist éditoriale suivie chez le service est un
/// favori de Tune ; une playlist possédée par le compte n'en est pas un.
#[tokio::test]
async fn une_playlist_suivie_chez_le_service_est_un_favori_la_sienne_non_6461() {
    let (backend, profils, _dir) = base(1);
    let (arc, compte) = ServiceSimule::nouveau("miroir-6461a");
    bibliotheque_de_playlists(&compte, &[("edito", Some(false)), ("perso", Some(true))]);

    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.echecs, 0, "{bilan:?}");
    assert_eq!(
        liste(&backend, profils[0]),
        vec![(
            "miroir-6461a".into(),
            "edito".into(),
            Some(ETAT_SYNCHRO.into())
        )],
        "la playlist suivie doit porter le cœur, la playlist possédée non"
    );
}

/// Témoin 6461-2 — retirer le cœur d'une playlist éditoriale dans Tune la
/// sort de la bibliothèque du service ; le rafraîchissement suivant ne la
/// fait pas revenir. Et une playlist que l'auditeur cesse de suivre dans
/// l'application du service perd son cœur dans Tune.
#[tokio::test]
async fn retirer_le_coeur_d_une_playlist_editoriale_la_sort_de_la_bibliotheque_6461() {
    let (backend, profils, _dir) = base(1);
    let (arc, compte) = ServiceSimule::nouveau("miroir-6461b");
    bibliotheque_de_playlists(&compte, &[("edito", Some(false)), ("autre", Some(false))]);
    rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(ids(&backend, profils[0]), vec!["autre", "edito"]);

    let p = retirer(&arc, &backend, "playlist", "edito").await.unwrap();
    assert_eq!(p, Propagation::Propage);
    assert!(
        !compte.lock().unwrap().playlists.contains_key("edito"),
        "la playlist est restée dans la bibliothèque du service"
    );
    rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(ids(&backend, profils[0]), vec!["autre"]);

    // Retirée dans l'application du service.
    compte.lock().unwrap().playlists.remove("autre");
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 1, "{bilan:?}");
    assert!(
        ids(&backend, profils[0]).is_empty(),
        "une playlist qui n'est plus suivie chez le service garde son cœur dans Tune"
    );
}

/// Témoin 6461-3 — les prudences : une playlist possédée qui porte un cœur
/// posé dans Tune n'est pas retirée ; une ligne d'avant la rc4 n'est pas
/// poussée (aucun abonnement sans geste) ; une lecture dont un auteur manque
/// ne retire rien.
#[tokio::test]
async fn les_playlists_relues_ne_retirent_ni_n_abonnent_sans_certitude_6461() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (arc, compte) = ServiceSimule::nouveau("miroir-6461c");
    bibliotheque_de_playlists(&compte, &[("perso", Some(true)), ("suivie", Some(false))]);
    ajouter(&arc, &backend, pid, &favori("playlist", "perso"))
        .await
        .unwrap();
    // `ajouter` a « suivi » la playlist possédée chez le simulé : on rétablit
    // ce que le service rend vraiment, une playlist à soi.
    compte
        .lock()
        .unwrap()
        .playlists
        .insert("perso".into(), Some(true));
    StreamingFavoritesRepo::with_backend(backend.clone())
        .add(
            pid,
            "playlist",
            "miroir-6461c",
            "ancienne",
            None,
            None,
            None,
            None,
        )
        .unwrap();
    compte.lock().unwrap().appels.clear();

    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert_eq!(ids(&backend, pid), vec!["ancienne", "perso", "suivie"]);
    assert!(
        compte.lock().unwrap().appels.is_empty(),
        "aucune écriture n'est due : {:?}",
        compte.lock().unwrap().appels
    );

    // Auteur inconnu : la lecture est incertaine, rien n'est retiré même
    // si « suivie » a disparu.
    {
        let mut c = compte.lock().unwrap();
        c.playlists.remove("suivie");
        c.playlists.insert("mystere".into(), None);
    }
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert_eq!(bilan.echecs, 1, "{bilan:?}");
    assert_eq!(ids(&backend, pid), vec!["ancienne", "perso", "suivie"]);
}

/// Témoin 6461-4 (relecture de #6616) — une lecture des playlists que le
/// service déclare incomplète ne retire RIEN : sans cette garde, une liste
/// tronquée retirerait le cœur de tout ce qui dépasse la coupe.
#[tokio::test]
async fn une_lecture_incomplete_des_playlists_ne_retire_rien_6461() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (arc, compte) = ServiceSimule::nouveau("miroir-6461d");
    bibliotheque_de_playlists(&compte, &[("a", Some(false)), ("b", Some(false))]);
    rafraichir(&arc, &backend, pid).await;
    assert_eq!(ids(&backend, pid), vec!["a", "b"]);

    {
        let mut c = compte.lock().unwrap();
        c.playlists.remove("b");
        c.playlists_tronquees = true;
    }
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert_eq!(bilan.echecs, 1, "{bilan:?}");
    assert_eq!(
        ids(&backend, pid),
        vec!["a", "b"],
        "une lecture incomplète a retiré des cœurs"
    );
}

/// Témoin 6461-5 (relecture de #6616), de bout en bout : le VRAI connecteur
/// Qobuz contre un Qobuz factice qui annonce 7 playlists et n'en sert que 3 —
/// la coupe de `detail_pagine`. Aucune ligne `synchro` ne disparaît.
#[tokio::test]
async fn qobuz_tronque_aucune_playlist_synchro_ne_disparait_6461() {
    use axum::{Json, Router};
    let app = Router::new().fallback(|uri: axum::http::Uri| async move {
        if uri.path().ends_with("/playlist/getUserPlaylists") {
            Json(json!({
                "user": {"id": 1234},
                "playlists": {"offset": 0, "limit": 500, "total": 7, "items": [
                    {"id": 11, "name": "Ma playlist", "owner": {"id": 1234, "name": "fabien"}},
                    {"id": 22, "name": "Jazz Essentials", "owner": {"id": 2, "name": "Qobuz"}},
                    {"id": 33, "name": "Classique", "owner": {"id": 2, "name": "Qobuz"}}
                ]}
            }))
        } else {
            Json(json!({}))
        }
    });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let svc: Box<dyn StreamingService> = Box::new(
        crate::streaming::qobuz::QobuzService::connecte_a_base_forcee(format!("http://{adresse}")),
    );
    let arc: ServiceArc = Arc::new(RwLock::new(svc));

    let (backend, profils, _dir) = base(2);
    // « 44 » est suivie chez Qobuz mais au-delà de la coupe : la réponse
    // tronquée ne la nomme pas.
    for id in ["22", "33", "44"] {
        ecrire_partout(
            &backend,
            &profils,
            "qobuz",
            &favori("playlist", id),
            ETAT_SYNCHRO,
            None,
        )
        .unwrap();
    }
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    for &pid in &profils {
        assert_eq!(
            ids(&backend, pid),
            vec!["22", "33", "44"],
            "une lecture tronquée de Qobuz a retiré des playlists synchro"
        );
    }
}

/// Témoin #5997 (lot H de la rc8) — une lecture PARTIELLE des favoris chez le
/// service ne retire AUCUN favori local, pour aucun profil, et le dit.
///
/// Le connecteur sert la coupe à l'affichage (`get_user_favorites_dated`) et
/// déclare la lecture incomplète à la réconciliation
/// (`favoris_dates_complets`). Réconcilier la coupe retirerait de Tune, à
/// chaque rafraîchissement, tout favori confirmé qu'elle ne nomme pas.
#[tokio::test]
async fn une_lecture_partielle_des_favoris_ne_retire_aucun_favori_5997() {
    let (backend, profils, _dir) = base(2);
    let (arc, compte) = ServiceSimule::nouveau("miroir-5997p");
    for id in ["1", "2", "3"] {
        poser_chez_le_service(&compte, "tracks", id);
        poser_chez_le_service(&compte, "albums", &format!("a{id}"));
    }
    rafraichir(&arc, &backend, profils[0]).await;
    let attendu = vec!["1", "2", "3", "a1", "a2", "a3"];
    for &pid in &profils {
        assert_eq!(ids(&backend, pid), attendu);
    }

    compte.lock().unwrap().favoris_tronques = true;
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert!(
        bilan.echecs >= 2,
        "la lecture partielle doit se dire : {bilan:?}"
    );
    for &pid in &profils {
        assert_eq!(
            ids(&backend, pid),
            attendu,
            "une lecture partielle du service a retiré des favoris locaux"
        );
    }
    assert_eq!(etat("miroir-5997p").statut, "echec");

    // Le service sert de nouveau tout : rien n'a été perdu entre-temps.
    compte.lock().unwrap().favoris_tronques = false;
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    assert_eq!(ids(&backend, profils[1]), attendu);
}

/// Témoin #5997, de bout en bout : le VRAI connecteur Qobuz contre un Qobuz
/// factice qui annonce 3 titres et n'en sert qu'un, et rend les albums sans
/// leur clé (réponse d'une autre forme). Aucun favori `synchro` ne disparaît.
#[tokio::test]
async fn qobuz_favoris_tronques_aucun_favori_synchro_ne_disparait_5997() {
    use axum::extract::Query;
    use axum::{Json, Router};
    use std::collections::HashMap as Carte;
    let app = Router::new().fallback(
        |uri: axum::http::Uri, Query(q): Query<Carte<String, String>>| async move {
            if !uri.path().ends_with("/favorite/getUserFavorites") {
                return Json(json!({}));
            }
            match q.get("type").map(String::as_str) {
                Some("tracks") => Json(json!({"tracks": {"offset": 0, "limit": 50, "total": 3,
                    "items": [{"id": 11, "title": "Summertime", "duration": 200}]}})),
                Some("artists") => Json(json!({"artists": {"offset": 0, "limit": 50,
                    "total": 0, "items": []}})),
                _ => Json(json!({"user": {"id": 1234}})),
            }
        },
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let svc: Box<dyn StreamingService> = Box::new(
        crate::streaming::qobuz::QobuzService::connecte_a_base_forcee(format!("http://{adresse}")),
    );
    let arc: ServiceArc = Arc::new(RwLock::new(svc));

    let (backend, profils, _dir) = base(2);
    for (t, id) in [
        ("track", "11"),
        ("track", "22"),
        ("track", "33"),
        ("album", "a1"),
    ] {
        ecrire_partout(
            &backend,
            &profils,
            "qobuz",
            &favori(t, id),
            ETAT_SYNCHRO,
            None,
        )
        .unwrap();
    }
    let bilan = rafraichir(&arc, &backend, profils[0]).await;
    assert_eq!(bilan.retires, 0, "{bilan:?}");
    for &pid in &profils {
        assert_eq!(
            ids(&backend, pid),
            vec!["11", "22", "33", "a1"],
            "une lecture Qobuz tronquée a retiré des favoris locaux"
        );
    }
    // L'écran du service, lui, garde la coupe : rien n'y devient une erreur.
    let affichage = arc.read().await.get_user_tracks().await.expect("affichage");
    assert_eq!(affichage.len(), 1);
}

/// #6482 — le rafraîchissement du miroir (le chemin de Qobuz) range l'année et
/// la qualité d'un album favori, pour chaque profil : une collection
/// intelligente les lit pour classer les albums favoris comme les locaux.
#[tokio::test]
async fn le_miroir_range_l_annee_et_la_qualite_des_albums_6482() {
    let (backend, profils, _dir) = base(2);
    let (arc, compte) = ServiceSimule::nouveau("miroir-6482");
    poser_chez_le_service(&compte, "albums", "9");
    rafraichir(&arc, &backend, profils[0]).await;
    let lignes: Vec<(i64, String, String, String)> = backend
        .query_many(
            "SELECT profile_id, year, sample_rate, bit_depth FROM streaming_favorites \
             WHERE service = 'miroir-6482' AND service_id = '9' ORDER BY profile_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            let t = |i: usize| r.get(i).and_then(|v| v.as_string()).unwrap_or_default();
            (r[0].as_i64().unwrap_or(0), t(1), t(2), t(3))
        })
        .collect();
    let attendu: Vec<(i64, String, String, String)> = profils
        .iter()
        .map(|&p| (p, "1959".into(), "192000".into(), "24".into()))
        .collect();
    assert_eq!(
        lignes, attendu,
        "l'année et la qualité de l'album favori doivent être rangées pour chaque profil"
    );
}

// ---------------------------------------------------------------------------
// Radio → Qobuz (01/11/2026) — jamais d'abonnement à sa propre playlist,
// jamais de 401 en boucle.
// ---------------------------------------------------------------------------

/// Les lignes d'un favori, tous profils : `(profile_id, miroir_etat)`.
fn etats_de(backend: &Arc<dyn DbBackend>, service: &str, id: &str) -> Vec<Option<String>> {
    backend
        .query_many_strong(
            "SELECT miroir_etat FROM streaming_favorites WHERE service = ?1 AND service_id = ?2 ORDER BY profile_id",
            &[&service, &id],
        )
        .unwrap()
        .into_iter()
        .map(|r| r.first().and_then(|v| v.as_string()))
        .collect()
}

fn appels(compte: &Arc<Mutex<Compte>>) -> Vec<String> {
    compte.lock().unwrap().appels.clone()
}

#[test]
fn un_refus_401_ou_403_se_reconnait_dans_le_motif_radio_qobuz() {
    assert!(est_un_refus_du_service(
        r#"qobuz /playlist/subscribe: 401 {"status":"error","code":401}"#
    ));
    assert!(est_un_refus_du_service("tidal favorites: 403 Forbidden"));
    // Contre-épreuves : une panne, un 404, un identifiant qui CONTIENT 401.
    assert!(!est_un_refus_du_service("panne réseau simulée"));
    assert!(!est_un_refus_du_service(
        "qobuz /playlist/subscribe: 404 not found"
    ));
    assert!(!est_un_refus_du_service(
        "qobuz /favorite/create: 500 playlist 58698401x"
    ));
    assert!(!est_un_refus_du_service(
        "qobuz /favorite/create: 500 id 4013"
    ));
}

/// Témoin 1 — un cœur posé dans Tune sur SA playlist ne s'abonne pas : elle
/// est `synchro` d'emblée, aucun appel d'écriture. Contre-épreuve : une
/// playlist suivie (d'un autre) est bien abonnée.
#[tokio::test]
async fn un_coeur_sur_sa_propre_playlist_ne_s_abonne_pas_radio_qobuz() {
    let (backend, profils, _dir) = base(2);
    let (arc, compte) = ServiceSimule::nouveau("miroir-rq1");
    bibliotheque_de_playlists(&compte, &[("58698591", Some(true))]);

    let p = ajouter(&arc, &backend, profils[0], &favori("playlist", "58698591"))
        .await
        .unwrap();
    assert_eq!(p, Propagation::Propage);
    assert!(
        appels(&compte).is_empty(),
        "abonnement tenté : {:?}",
        appels(&compte)
    );
    assert_eq!(
        etats_de(&backend, "miroir-rq1", "58698591"),
        vec![Some(ETAT_SYNCHRO.to_string()); 2]
    );

    // Contre-épreuve : la playlist d'un autre, elle, est abonnée.
    let p = ajouter(&arc, &backend, profils[0], &favori("playlist", "edito"))
        .await
        .unwrap();
    assert_eq!(p, Propagation::Propage);
    assert_eq!(appels(&compte), vec!["add playlists edito".to_string()]);

    // Retirer le cœur de SA playlist : local, aucun désabonnement.
    compte.lock().unwrap().appels.clear();
    retirer(&arc, &backend, "playlist", "58698591")
        .await
        .unwrap();
    assert!(appels(&compte).is_empty(), "{:?}", appels(&compte));
    assert!(etats_de(&backend, "miroir-rq1", "58698591").is_empty());
}

/// Témoin 2 — l'état exact relevé sur le .18 : trois playlists du compte en
/// `ajout_en_attente`. Le rafraîchissement les règle SANS appel ; les
/// suivants n'appellent rien non plus. Contre-épreuve : sans la règle, le
/// simulé répondait 401 à chaque passage (le journal en compte 12 360).
#[tokio::test]
async fn les_playlists_possedees_en_attente_ne_bouclent_plus_en_401_radio_qobuz() {
    let (backend, profils, _dir) = base(2);
    let (arc, compte) = ServiceSimule::nouveau("miroir-rq2");
    bibliotheque_de_playlists(
        &compte,
        &[
            ("58698591", Some(true)),
            ("58698592", Some(true)),
            ("58698595", Some(true)),
        ],
    );
    let repo = StreamingFavoritesRepo::with_backend(backend.clone());
    for id in ["58698591", "58698592", "58698595"] {
        for &pid in &profils {
            repo.add(pid, "playlist", "miroir-rq2", id, None, None, None, None)
                .unwrap();
        }
        poser_etat(
            &backend,
            "miroir-rq2",
            "playlist",
            id,
            ETAT_AJOUT_EN_ATTENTE,
            Some("qobuz /playlist/subscribe: 401"),
        )
        .unwrap();
    }
    assert_eq!(compter_en_attente(&backend, "miroir-rq2"), 3);

    for passage in 0..3 {
        let bilan = rafraichir(&arc, &backend, profils[0]).await;
        assert_eq!(bilan.echecs, 0, "passage {passage} : {bilan:?}");
        assert_eq!(bilan.en_attente, 0, "passage {passage} : {bilan:?}");
        assert!(
            appels(&compte).is_empty(),
            "passage {passage} : abonnement à sa propre playlist : {:?}",
            appels(&compte)
        );
    }
    for id in ["58698591", "58698592", "58698595"] {
        assert_eq!(
            etats_de(&backend, "miroir-rq2", id),
            vec![Some(ETAT_SYNCHRO.to_string()); 2],
            "{id} doit rester un cœur, confirmé"
        );
    }
}

/// Témoin 3 — un 401 sur un identifiant que le compte ne possède pas, la
/// session venant de lire le compte sans erreur : UN appel, la ligne passe
/// `refuse_par_le_service`, plus aucun appel ensuite. Contre-épreuve : si
/// la lecture a échoué (session douteuse), le 401 reste en attente et est
/// retenté — on ne condamne pas un favori sur une session expirée.
#[tokio::test]
async fn un_refus_401_est_memorise_et_n_est_plus_retente_radio_qobuz() {
    let (backend, profils, _dir) = base(1);
    let pid = profils[0];
    let (arc, compte) = ServiceSimule::nouveau("miroir-rq3");
    compte.lock().unwrap().refusees.insert("privee".into());
    bibliotheque_de_playlists(&compte, &[]);

    let p = ajouter(&arc, &backend, pid, &favori("playlist", "privee"))
        .await
        .unwrap();
    assert!(matches!(p, Propagation::EnAttente(_)), "{p:?}");
    compte.lock().unwrap().appels.clear();

    let bilan = rafraichir(&arc, &backend, pid).await;
    assert_eq!(bilan.refuses, 1, "{bilan:?}");
    assert_eq!(bilan.en_attente, 0, "{bilan:?}");
    assert_eq!(appels(&compte), vec!["add playlists privee".to_string()]);
    assert_eq!(
        etats_de(&backend, "miroir-rq3", "privee"),
        vec![Some(ETAT_REFUSE.to_string())]
    );

    for _ in 0..3 {
        rafraichir(&arc, &backend, pid).await;
    }
    assert_eq!(
        appels(&compte),
        vec!["add playlists privee".to_string()],
        "le refus a été retenté"
    );

    // Retirer le cœur d'un ajout refusé : local, aucun appel.
    retirer(&arc, &backend, "playlist", "privee").await.unwrap();
    assert_eq!(appels(&compte).len(), 1);
    assert!(etats_de(&backend, "miroir-rq3", "privee").is_empty());

    // Contre-épreuve : lecture du compte en échec → le 401 reste en attente.
    let (arc, compte) = ServiceSimule::nouveau("miroir-rq3b");
    compte.lock().unwrap().refusees.insert("71".into());
    ajouter(&arc, &backend, pid, &favori("track", "71"))
        .await
        .unwrap();
    compte.lock().unwrap().favoris_tronques = true;
    let bilan = rafraichir(&arc, &backend, pid).await;
    assert!(bilan.echecs > 0, "{bilan:?}");
    assert_eq!(bilan.refuses, 0, "{bilan:?}");
    assert_eq!(bilan.en_attente, 1, "{bilan:?}");
    assert_eq!(
        etats_de(&backend, "miroir-rq3b", "71"),
        vec![Some(ETAT_AJOUT_EN_ATTENTE.to_string())]
    );
}
