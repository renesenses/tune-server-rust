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

/// Le compte chez le service : `type pluriel → id → (titre, artiste, album, isrc)`.
#[derive(Default)]
struct Compte {
    favoris: BTreeMap<String, BTreeMap<String, (String, String, String, Option<String>)>>,
    /// Panne réseau : toute lecture et toute écriture échouent.
    panne: bool,
    /// Journal des écritures reçues : `"add tracks 71"`, `"remove albums 9"`.
    appels: Vec<String>,
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
            authenticated: true,
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
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
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
        Ok(Some(
            c.favoris
                .get(fav_type)
                .map(|m| {
                    m.iter()
                        .map(|(id, (t, a, al, isrc))| match fav_type {
                            "artists" => json!({"id": id, "name": t}),
                            "albums" => json!({"source_id": id, "title": t, "artist_name": a}),
                            _ => json!({"source_id": id, "title": t, "artist_name": a,
                                        "album_title": al, "isrc": isrc}),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ))
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
            "DELETE FROM streaming_favorites WHERE service = 'miroir-pg' AND profile_id = $1",
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
}
