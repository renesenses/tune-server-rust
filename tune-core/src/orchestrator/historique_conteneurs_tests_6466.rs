//! #6466 (FabienM, fil 2302 points 10, 11, 15) — l'historique organisé par
//! conteneurs, par les vraies portes : `play_from_queue` sur une zone DLNA
//! simulée (`MockOutput`), des pistes dont l'URL vient d'un fournisseur
//! factice, et l'historique relu en base. Aucun son, aucun appareil réel.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::backend::DbBackend;
use crate::db::contexte_de_file::{self, ContexteDeFile};
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::PlaybackManager;
use crate::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};
use crate::streaming::ServiceRegistry;

const SALON: &str = "dlna:banc-6466-salon";
const ENFANTS: &str = "dlna:banc-6466-enfants";
const SOURCE: &str = "circle";

struct Fournisseur;

#[async_trait::async_trait]
impl FournisseurDUrl for Fournisseur {
    async fn url(&self, _zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl> {
        Ok(UrlFournie {
            url: format!("http://127.0.0.1:9/stream/6466/{source_id}"),
            media_format: Some("flac".into()),
        })
    }
}

fn piste(reference: &str, titre: &str) -> QueueInput {
    QueueInput::Streaming {
        source: SOURCE.into(),
        source_id: reference.into(),
        title: titre.into(),
        artist: "The Black Heart Procession".into(),
        album: Some("1".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    db: Arc<dyn DbBackend>,
    salon: i64,
    enfants: i64,
}

impl Banc {
    async fn monter() -> Self {
        Self::monter_avec_un_plafond(None).await
    }

    /// `plafond` : la limite de zones d'un compte gratuit, câblée comme en
    /// production (`orch.license`). `None` : pas de licence, pas de plafond.
    async fn monter_avec_un_plafond(plafond: Option<i64>) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let zones = ZoneRepo::with_backend(db.clone());
        let salon = zones.create("Salon", Some("dlna"), Some(SALON)).unwrap();
        let enfants = zones
            .create("Enfants", Some("dlna"), Some(ENFANTS))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        {
            let mut o = outputs.lock().await;
            o.register(Box::new(MockOutput::new(SALON, "Salon").with_type("dlna")));
            o.register(Box::new(
                MockOutput::new(ENFANTS, "Enfants").with_type("dlna"),
            ));
        }
        let mut orch = PlaybackOrchestrator::new(
            db.clone(),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs,
            None,
        );
        if let Some(limite) = plafond {
            orch.license = Some(Arc::new(crate::license::LicenseManager::new_with_limit(
                db.clone(),
                limite,
            )));
        }
        let orch = Arc::new(orch);
        orch.sources_url().inscrire(SOURCE, Arc::new(Fournisseur));
        Self {
            orch,
            db,
            salon,
            enfants,
        }
    }

    /// Les lancements (`play_media`) reçus par l'appareil de la zone cible.
    async fn lancements_sur_enfants(&self) -> usize {
        let outputs = self.orch.outputs.lock().await;
        let sortie = outputs.get(ENFANTS).expect("sortie enregistrée");
        let garde = sortie.lock().await;
        let mock = garde
            .as_any()
            .downcast_ref::<MockOutput>()
            .expect("sortie factice");
        mock.play_call_count().await
    }

    /// « Lire » l'album 1 : la session de la zone EST l'album, les lignes
    /// n'en portent pas (NULL), comme ce que pose `POST /zones/{id}/play`.
    async fn lire_l_album(&self, zone: i64) {
        PlayQueueRepo::with_backend(self.db.clone())
            .append(
                zone,
                &[
                    piste("a:1", "Even Thieves Come Clean"),
                    piste("a:2", "Release My Heart"),
                ],
            )
            .unwrap();
        self.orch
            .playback
            .set_session_context(
                zone,
                Some("album".into()),
                Some("album-1".into()),
                Some(SOURCE.into()),
                None,
                None,
            )
            .await;
    }

    /// « Ajouter à la file » : les lignes ajoutées portent `contexte`, comme
    /// le fait `POST /zones/{id}/queue/add`.
    fn ajouter(&self, zone: i64, lignes: &[QueueInput], contexte: &ContexteDeFile) {
        let avant = contexte_de_file::dernier_identifiant(&self.db, zone);
        PlayQueueRepo::with_backend(self.db.clone())
            .append(zone, lignes)
            .unwrap();
        contexte_de_file::poser_apres(&self.db, zone, avant, contexte).unwrap();
    }

    async fn jouer(&self, zone: i64, position: i64) {
        self.orch
            .play_from_queue(zone, position)
            .await
            .unwrap_or_else(|e| panic!("la position {position} devait partir : {e}"));
    }

    /// `(titre, zone, conteneur, id du conteneur)` de chaque écoute, dans
    /// l'ordre d'écriture.
    fn historique(&self) -> Vec<(String, i64, Option<String>, Option<String>)> {
        self.db
            .query_many(
                "SELECT title, zone_id, context_type, context_id FROM listen_history ORDER BY id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r[0].as_string().unwrap_or_default(),
                    r[1].as_i64().unwrap_or(0),
                    r[2].as_string(),
                    r[3].as_string(),
                )
            })
            .collect()
    }
}

fn playlist() -> ContexteDeFile {
    ContexteDeFile {
        nature: "playlist".into(),
        id: Some("p42".into()),
        service: Some(SOURCE.into()),
        ..Default::default()
    }
}

/// 🔴 Point 15 : « si on ajoute à la file un titre qui n'appartient pas à
/// l'objet conteneur joué alors il doit se retrouver en 1er niveau dans
/// l'historique dès qu'il est joué ». Et l'album reste l'album.
#[tokio::test]
async fn un_titre_ajoute_seul_s_ecrit_au_premier_niveau_6466() {
    let b = Banc::monter().await;
    b.lire_l_album(b.salon).await;
    b.ajouter(
        b.salon,
        &[piste("t:9", "Un titre seul")],
        &ContexteDeFile::titre_seul(),
    );
    for position in 0..3 {
        b.jouer(b.salon, position).await;
    }
    let h = b.historique();
    let conteneurs: Vec<Option<&str>> = h.iter().map(|l| l.2.as_deref()).collect();
    assert_eq!(
        conteneurs,
        vec![Some("album"), Some("album"), Some("track")],
        "le titre ajouté seul doit être au premier niveau (`track`), pas dans l'album — {h:?}"
    );
}

/// 🔴 Point 15 : « si je joue un album et que j'ajoute à la file une playlist
/// alors dans l'historique je devrais voir 2 conteneurs en 1er niveau ».
#[tokio::test]
async fn une_playlist_ajoutee_pendant_un_album_est_son_propre_conteneur_6466() {
    let b = Banc::monter().await;
    b.lire_l_album(b.salon).await;
    b.ajouter(
        b.salon,
        &[piste("p:1", "Playlist 1"), piste("p:2", "Playlist 2")],
        &playlist(),
    );
    for position in 0..4 {
        b.jouer(b.salon, position).await;
    }
    let h = b.historique();
    let conteneurs: Vec<(Option<&str>, Option<&str>)> =
        h.iter().map(|l| (l.2.as_deref(), l.3.as_deref())).collect();
    assert_eq!(
        conteneurs,
        vec![
            (Some("album"), Some("album-1")),
            (Some("album"), Some("album-1")),
            (Some("playlist"), Some("p42")),
            (Some("playlist"), Some("p42")),
        ],
        "la playlist ajoutée à la file est un SECOND conteneur, pas une part de l'album — {h:?}"
    );
}

/// Point 11 : « le conteneur est ajouté à l'historique dès qu'un titre de son
/// contenu est joué, pas avant ». Une playlist enfilée puis jamais atteinte
/// n'écrit rien.
#[tokio::test]
async fn un_conteneur_jamais_atteint_n_entre_pas_dans_l_historique_6466() {
    let b = Banc::monter().await;
    b.lire_l_album(b.salon).await;
    b.ajouter(b.salon, &[piste("p:1", "Playlist 1")], &playlist());
    b.jouer(b.salon, 0).await;
    PlayQueueRepo::with_backend(b.db.clone())
        .clear(b.salon)
        .unwrap();
    let h = b.historique();
    assert!(
        h.iter().all(|l| l.2.as_deref() != Some("playlist")),
        "une playlist jamais jouée ne doit pas apparaître — {h:?}"
    );
}

/// 🔴 Point 15 : le transfert de zone. La piste transférée reste notée avec la
/// zone d'origine, SANS doublon sur la zone cible ; les titres suivants
/// s'écrivent avec la zone cible, DANS leur conteneur ; le titre ajouté seul
/// garde son premier niveau.
#[tokio::test]
async fn le_transfert_garde_les_conteneurs_et_n_ecrit_pas_de_doublon_6466() {
    let b = Banc::monter().await;
    b.lire_l_album(b.salon).await;
    b.ajouter(
        b.salon,
        &[piste("t:9", "Un titre seul")],
        &ContexteDeFile::titre_seul(),
    );
    b.jouer(b.salon, 0).await;
    assert_eq!(b.historique().len(), 1);

    // La copie de `do_transfer` : la file, les conteneurs, la session.
    let lignes: Vec<QueueInput> = vec![
        piste("a:1", "Even Thieves Come Clean"),
        piste("a:2", "Release My Heart"),
        piste("t:9", "Un titre seul"),
    ];
    PlayQueueRepo::with_backend(b.db.clone())
        .append(b.enfants, &lignes)
        .unwrap();
    contexte_de_file::copier_les_contextes(&b.db, b.salon, b.enfants);
    let source = b.orch.playback.get_state(b.salon).await;
    b.orch
        .playback
        .set_session_context(
            b.enfants,
            source.session_context_type.clone(),
            source.session_context_id.clone(),
            source.session_context_source.clone(),
            None,
            None,
        )
        .await;
    let np = source.now_playing.clone().expect("le salon joue");
    b.orch
        .transferer_sur_l_appareil(b.enfants, ENFANTS, 0, &np, 0, false)
        .await
        .expect("le transfert doit aboutir");

    let h = b.historique();
    assert_eq!(
        h.len(),
        1,
        "la piste transférée est déjà notée avec la zone d'origine : pas de doublon \
         avec la zone de destination — {h:?}"
    );
    assert_eq!(h[0].1, b.salon);

    b.jouer(b.enfants, 1).await;
    b.jouer(b.enfants, 2).await;
    let h = b.historique();
    let suite: Vec<(i64, Option<&str>)> = h[1..].iter().map(|l| (l.1, l.2.as_deref())).collect();
    assert_eq!(
        suite,
        vec![(b.enfants, Some("album")), (b.enfants, Some("track"))],
        "après le transfert, la suite de l'album reste dans l'album, avec la zone \
         cible, et le titre seul reste au premier niveau — {h:?}"
    );
}

/// Relecture rc7 de #6637, point 1 — le transfert ne retire QUE l'écriture
/// de l'historique : il passe par les étapes d'entrée de `play()`. Témoin : le
/// plafond de zones d'un compte gratuit. Une cible jamais activée, au-delà du
/// plafond, est refusée sur les DEUX chemins du transfert (la file, et le flux
/// direct d'une radio) : rien n'atteint son appareil, rien n'entre dans
/// l'historique. `play_without_history`, qui saute ces étapes, la lançait.
#[tokio::test]
async fn le_transfert_passe_par_les_etapes_d_entree_de_play_6466() {
    let b = Banc::monter_avec_un_plafond(Some(1)).await;
    b.lire_l_album(b.salon).await;
    b.jouer(b.salon, 0).await;
    // Le salon est LA zone active du compte gratuit (plafond : 1).
    b.db.execute(
        &format!("UPDATE zones SET last_track_id = 1 WHERE id = {}", b.salon),
        &[],
    )
    .unwrap();
    assert_eq!(b.historique().len(), 1);
    PlayQueueRepo::with_backend(b.db.clone())
        .append(
            b.enfants,
            &[
                piste("a:1", "Even Thieves Come Clean"),
                piste("a:2", "Release My Heart"),
            ],
        )
        .unwrap();
    let np = b
        .orch
        .playback
        .get_state(b.salon)
        .await
        .now_playing
        .expect("le salon joue");

    // Chemin de la file.
    b.orch
        .transferer_sur_l_appareil(b.enfants, ENFANTS, 0, &np, 0, false)
        .await
        .expect("un lancement refusé n'est que journalisé");
    // Chemin direct : une radio ne vit pas dans la file.
    let mut radio = np.clone();
    radio.source = "radio".into();
    radio.source_id = Some("r:6466".into());
    b.orch
        .transferer_sur_l_appareil(b.enfants, ENFANTS, 0, &radio, 0, false)
        .await
        .expect("un lancement refusé n'est que journalisé");
    assert_eq!(
        b.lancements_sur_enfants().await,
        0,
        "le plafond de zones de `play()` doit refuser la cible jamais activée, \
         sur les deux chemins du transfert"
    );
    assert_eq!(b.historique().len(), 1);

    // Contrôle : la même cible, une fois activée, reçoit le transfert — le
    // refus venait bien du plafond. Et toujours aucune écoute de plus.
    b.db.execute(
        &format!(
            "UPDATE zones SET last_track_id = 1 WHERE id = {}",
            b.enfants
        ),
        &[],
    )
    .unwrap();
    b.orch
        .transferer_sur_l_appareil(b.enfants, ENFANTS, 0, &np, 0, false)
        .await
        .expect("le transfert doit aboutir");
    assert_eq!(b.lancements_sur_enfants().await, 1);
    let h = b.historique();
    assert_eq!(
        h.len(),
        1,
        "la piste transférée n'est pas réécrite avec la zone cible — {h:?}"
    );
    assert_eq!(h[0].1, b.salon);
}

/// L'appariement de la copie : une piste présente deux fois garde ses deux
/// conteneurs, une piste sautée ne décale rien.
#[tokio::test]
async fn la_copie_apparie_les_lignes_par_piste_et_dans_l_ordre_6466() {
    let b = Banc::monter().await;
    let album = ContexteDeFile {
        nature: "album".into(),
        id: Some("x".into()),
        service: Some(SOURCE.into()),
        ..Default::default()
    };
    b.ajouter(b.salon, &[piste("s:1", "Un")], &album);
    b.ajouter(b.salon, &[piste("s:2", "Deux")], &playlist());
    b.ajouter(
        b.salon,
        &[piste("s:1", "Un, encore")],
        &ContexteDeFile::titre_seul(),
    );
    // La cible n'a pas « Deux » (sautée) et garde l'ordre.
    PlayQueueRepo::with_backend(b.db.clone())
        .append(b.enfants, &[piste("s:1", "Un"), piste("s:1", "Un, encore")])
        .unwrap();
    assert_eq!(
        contexte_de_file::copier_les_contextes(&b.db, b.salon, b.enfants),
        2
    );
    let natures: Vec<Option<String>> = contexte_de_file::contextes_de_la_file(&b.db, b.enfants)
        .into_iter()
        .map(|(_, _, c)| c.map(|c| c.nature))
        .collect();
    assert_eq!(natures, vec![Some("album".into()), Some("track".into())]);
}

/// Une ligne qui ne désigne pas la piste jouée ne prête pas son conteneur.
#[tokio::test]
async fn une_ligne_d_une_autre_piste_ne_prete_pas_son_conteneur_6466() {
    let b = Banc::monter().await;
    b.ajouter(b.salon, &[piste("s:1", "Un")], &playlist());
    assert_eq!(
        contexte_de_file::contexte_de_la_piste_jouee(&b.db, b.salon, 0, None, Some("s:1"))
            .map(|c| c.nature),
        Some("playlist".into())
    );
    assert_eq!(
        contexte_de_file::contexte_de_la_piste_jouee(&b.db, b.salon, 0, None, Some("autre")),
        None
    );
}
