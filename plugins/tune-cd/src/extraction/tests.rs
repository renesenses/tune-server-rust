//! #2466 — l'extraction de bout en bout, par les ROUTES, sur un lecteur
//! simulé (Shrek n'a pas de lecteur) : FLAC et WAV relus par le décodeur de
//! Tune et comparés aux secteurs source, balises relues par le lecteur de
//! métadonnées du scan, annulation, chemins hostiles, accès admin.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_http_types::AuthUser;

use super::accuraterip::{CalculAccurateRip, hex};
use super::routes::{Pochettes, verdict_admin};
use super::{EVT_DEMARREE, EVT_TERMINEE, Extractions, ScanCible};
use crate::ejection::tests::HoteTemoin;
use crate::fournisseur::SOURCE;
use crate::lecteur::{ErreurCd, ErreurEjection, LecteurDisque, Presence};
use crate::musicbrainz::{Consultation, InfosDisque, InfosPiste};
use crate::routes::{EtatRoutes, router};
use crate::simule::{LecteurSimule, contenu_des_secteurs};
use crate::toc::{PisteToc, TRAMES_PAR_SECTEUR, Toc};

/// Trois pistes courtes : 40, 75 et 30 secteurs.
fn petite_toc() -> Toc {
    Toc::nouvelle(
        vec![
            PisteToc {
                numero: 1,
                debut: 0,
                audio: true,
            },
            PisteToc {
                numero: 2,
                debut: 40,
                audio: true,
            },
            PisteToc {
                numero: 3,
                debut: 115,
                audio: true,
            },
        ],
        145,
    )
    .unwrap()
}

const PLAGES: [(u8, u32, u32); 3] = [(1, 0, 40), (2, 40, 115), (3, 115, 145)];

/// Un JPEG minimal (les trois octets de tête suffisent au tri par type).
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1, 0xFF, 0xD9,
];

struct AvecMusicBrainz;
#[async_trait]
impl Consultation for AvecMusicBrainz {
    async fn consulter(&self, _: &str) -> Option<InfosDisque> {
        let piste = |n: u8, titre: &str| {
            (
                n,
                InfosPiste {
                    titre: titre.into(),
                    artiste: None,
                    recording_id: Some(format!("rec-{n}")),
                    piste_id: Some(format!("trk-{n}")),
                    artiste_ids: Vec::new(),
                },
            )
        };
        Some(InfosDisque {
            titre: "Un Album: Live?".into(),
            artiste: "Le Groupe".into(),
            release_id: Some("rel-1".into()),
            pochette: Some("https://example.invalid/front".into()),
            pistes: HashMap::from([
                piste(1, "Premier"),
                piste(2, "Deuxième / bis"),
                piste(3, "Troisième"),
            ]),
            artiste_ids: vec!["art-1".into()],
            date: Some("2001-02-03".into()),
            disque: 1,
            disques: 1,
        })
    }
}

struct SansReseau;
#[async_trait]
impl Consultation for SansReseau {
    async fn consulter(&self, _: &str) -> Option<InfosDisque> {
        None
    }
}

struct PochetteFixe;
#[async_trait]
impl Pochettes for PochetteFixe {
    async fn telecharger(&self, _: &str) -> Option<Vec<u8>> {
        Some(JPEG.to_vec())
    }
}

#[derive(Default)]
struct ScanTemoin {
    dossiers: Mutex<Vec<String>>,
}
#[async_trait]
impl ScanCible for ScanTemoin {
    async fn scanner(&self, dossier: String) -> bool {
        self.dossiers.lock().unwrap().push(dossier);
        true
    }
}

/// Un lecteur qui s'ARRÊTE à la lecture numéro `apres` jusqu'à ce que le
/// témoin le relâche : l'annulation et les refus se prouvent pendant une
/// extraction réellement en cours, sans course.
struct LecteurBarriere {
    interieur: LecteurSimule,
    appels: AtomicU32,
    apres: u32,
    arrive: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    reprise: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl LecteurDisque for LecteurBarriere {
    fn chemin(&self) -> String {
        "simulé".into()
    }
    fn presence(&self) -> Presence {
        self.interieur.presence()
    }
    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        self.interieur.lire_toc()
    }
    fn lire_secteurs(&self, lba: u32, n: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        if self.appels.fetch_add(1, Ordering::SeqCst) + 1 == self.apres {
            if let Some(tx) = self.arrive.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = self.reprise.lock().unwrap().recv();
        }
        self.interieur.lire_secteurs(lba, n, sortie)
    }
    fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
        self.interieur.ejecter_disque()
    }
}

struct Banc {
    bib: tempfile::TempDir,
    backend: Arc<dyn DbBackend>,
    ex: Arc<Extractions>,
    routes: EtatRoutes,
    hote: Arc<HoteTemoin>,
    scan: Arc<ScanTemoin>,
    bus: tune_core::event_bus::EventBus,
}

impl Banc {
    fn new(lecteur: Arc<dyn LecteurDisque>, c: Arc<dyn Consultation>) -> Banc {
        let bib = tempfile::tempdir().unwrap();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        SettingsRepo::with_backend(backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&[bib.path().to_string_lossy()]).unwrap(),
            )
            .unwrap();
        let scan = Arc::new(ScanTemoin::default());
        let bus = tune_core::event_bus::EventBus::new();
        let ex = Arc::new(
            Extractions::new(backend.clone(), Some(bus.clone()), Some(scan.clone()))
                .avec_pochettes(Arc::new(PochetteFixe)),
        );
        let hote = Arc::new(HoteTemoin::default());
        let routes = EtatRoutes {
            lecteur: Some(lecteur),
            hote: hote.clone(),
            consultation: c,
            zones: Arc::default(),
            reveil: Arc::default(),
            extraction: Some(ex.clone()),
        };
        Banc {
            bib,
            backend,
            ex,
            routes,
            hote,
            scan,
            bus,
        }
    }

    fn racine(&self) -> PathBuf {
        self.bib.path().to_path_buf()
    }

    fn r(&self) -> Router<()> {
        router(self.routes.clone())
    }

    async fn appel(&self, methode: &str, uri: &str, corps: Option<Value>) -> (StatusCode, Value) {
        appel_avec(self.r(), methode, uri, corps, None).await
    }

    /// Lance, puis attend la fin ; rend l'état final.
    async fn extraire(&self, corps: Value) -> Value {
        let (code, v) = self.appel("POST", "/extractions", Some(corps)).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{v}");
        self.attendre(v["id"].as_str().unwrap()).await
    }

    async fn attendre(&self, id: &str) -> Value {
        for _ in 0..3_000 {
            let (code, v) = self.appel("GET", &format!("/extractions/{id}"), None).await;
            assert_eq!(code, StatusCode::OK);
            if v["statut"] != "en_cours" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("l'extraction {id} ne finit pas");
    }
}

async fn appel_avec(
    r: Router<()>,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
    user: Option<AuthUser>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(methode).uri(uri);
    let mut req = match corps {
        Some(c) => req
            .header("content-type", "application/json")
            .body(Body::from(c.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    if let Some(u) = user {
        req.extensions_mut().insert(u);
    }
    let rep = r.oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (code, v)
}

fn simule() -> Arc<LecteurSimule> {
    Arc::new(LecteurSimule::new(petite_toc()))
}

/// Le PCM que Tune relit d'un fichier extrait.
fn pcm_relu(chemin: &Path) -> Vec<u8> {
    let d = tune_core::audio::decode::decode_to_pcm(chemin.to_str().unwrap(), None, None, 0.0, 0.0)
        .unwrap();
    assert_eq!((d.sample_rate, d.channels, d.bit_depth), (44_100, 2, 16));
    d.pcm_bytes()
}

fn crc_attendu(n: u8, debut: u32, fin: u32) -> (String, String) {
    let mut c = CalculAccurateRip::new((fin - debut) * TRAMES_PAR_SECTEUR as u32, n == 1, n == 3);
    c.ajouter(&contenu_des_secteurs(debut, fin - debut));
    let (v1, v2) = c.resultat();
    (hex(v1), hex(v2))
}

/// Le témoin principal : un disque simulé entier, en FLAC, avec
/// MusicBrainz. Chaque fichier est relu par le DÉCODEUR de Tune et comparé
/// octet pour octet aux secteurs du disque ; les balises sont relues par le
/// lecteur de métadonnées du SCAN ; la pochette est dans le fichier et dans
/// le dossier ; le scan ciblé vise le dossier de l'album.
#[tokio::test]
async fn un_disque_entier_en_flac_est_bit_exact_balise_et_scanne() {
    let banc = Banc::new(simule(), Arc::new(AvecMusicBrainz));
    let mut rx = banc.bus.subscribe();
    let fin = banc.extraire(json!({})).await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    assert_eq!(fin["format"], "flac");
    assert_eq!(fin["metadonnees"], "musicbrainz");
    assert_eq!(fin["pourcentage"], 100.0);
    assert_eq!(fin["scan"], "lance");

    let album = banc.racine().join("Le Groupe").join("Un Album_ Live_");
    assert_eq!(fin["dossier"], album.to_string_lossy().as_ref());
    assert_eq!(
        *banc.scan.dossiers.lock().unwrap(),
        vec![album.to_string_lossy().to_string()]
    );
    let noms = [
        "01 - Premier.flac",
        "02 - Deuxième _ bis.flac",
        "03 - Troisième.flac",
    ];
    for (i, (n, debut, fin_piste)) in PLAGES.iter().enumerate() {
        let chemin = album.join(noms[i]);
        assert_eq!(
            pcm_relu(&chemin),
            contenu_des_secteurs(*debut, fin_piste - debut),
            "piste {n} : PCM relu ≠ secteurs du disque"
        );
        let p = &fin["pistes"][i];
        assert_eq!(p["statut"], "terminee");
        assert_eq!(p["fichier"], chemin.to_string_lossy().as_ref());
        let (v1, v2) = crc_attendu(*n, *debut, *fin_piste);
        assert_eq!(
            (p["accuraterip_v1"].as_str(), p["accuraterip_v2"].as_str()),
            (Some(&*v1), Some(&*v2))
        );

        let m = tune_core::metadata::read_metadata(&chemin).expect("balises lisibles");
        assert_eq!(m.track_number, Some(*n as u32));
        assert_eq!(m.total_tracks, Some(3));
        assert_eq!(m.disc_number, Some(1));
        assert_eq!(m.artist.as_deref(), Some("Le Groupe"));
        assert_eq!(m.album_artist.as_deref(), Some("Le Groupe"));
        assert_eq!(
            m.album.as_deref(),
            Some("Un Album: Live?"),
            "le titre vrai, pas le nom de dossier"
        );
        assert_eq!(m.musicbrainz_release_id.as_deref(), Some("rel-1"));
        assert_eq!(
            m.musicbrainz_recording_id.as_deref(),
            Some(&*format!("rec-{n}"))
        );
        assert_eq!(m.musicbrainz_artist_id.as_deref(), Some("art-1"));
        assert_eq!(m.musicbrainz_album_artist_id.as_deref(), Some("art-1"));
        let lofty = lofty::read_from_path(&chemin).unwrap();
        use lofty::file::TaggedFileExt;
        let tag = lofty.primary_tag().unwrap();
        assert_eq!(tag.pictures().len(), 1, "pochette dans le fichier");
        assert_eq!(tag.pictures()[0].data(), JPEG);
        assert!(!super::travail::provisoire(&chemin).exists());
    }
    assert_eq!(
        tune_core::metadata::read_metadata(&album.join(noms[1]))
            .unwrap()
            .title
            .as_deref(),
        Some("Deuxième / bis")
    );
    assert_eq!(std::fs::read(album.join("cover.jpg")).unwrap(), JPEG);

    // Les évènements : démarrée, puis terminée avec le même état.
    let mut vus = Vec::new();
    while let Ok(e) = rx.try_recv() {
        vus.push(e);
    }
    assert_eq!(
        vus.first().map(|e| e.event_type.as_str()),
        Some(EVT_DEMARREE)
    );
    let dernier = vus.last().unwrap();
    assert_eq!(dernier.event_type, EVT_TERMINEE);
    assert_eq!(dernier.data["statut"], "terminee");
    assert_eq!(dernier.data["id"], fin["id"]);
}

/// WAV : même exigence octet pour octet, balises ID3 relues par le scan.
#[tokio::test]
async fn en_wav_les_pistes_choisies_sont_bit_exactes_et_balisees() {
    let banc = Banc::new(simule(), Arc::new(AvecMusicBrainz));
    let fin = banc
        .extraire(json!({ "format": "wav", "pistes": [3, 2] }))
        .await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    assert_eq!(fin["pistes"].as_array().unwrap().len(), 2);
    assert_eq!(fin["pistes"][0]["numero"], 2, "dans l'ordre du disque");
    let album = banc.racine().join("Le Groupe").join("Un Album_ Live_");
    let chemin = album.join("03 - Troisième.wav");
    assert_eq!(pcm_relu(&chemin), contenu_des_secteurs(115, 30));
    assert!(!album.join("01 - Premier.wav").exists());
    let m = tune_core::metadata::read_metadata(&chemin).unwrap();
    assert_eq!(m.title.as_deref(), Some("Troisième"));
    assert_eq!(m.track_number, Some(3));
    assert_eq!(m.disc_number, Some(1));
    assert_eq!(m.album.as_deref(), Some("Un Album: Live?"));
    assert_eq!(m.musicbrainz_release_id.as_deref(), Some("rel-1"));
    assert_eq!(m.musicbrainz_recording_id.as_deref(), Some("rec-3"));
    assert_eq!(m.musicbrainz_artist_id.as_deref(), Some("art-1"));
    assert_eq!(m.musicbrainz_album_artist_id.as_deref(), Some("art-1"));
    // `has_cover` du scan écarte une image indécodable (le JPEG témoin n'a
    // pas de pixels) : la présence se lit donc chez lofty, comme en FLAC.
    use lofty::file::TaggedFileExt;
    let lofty = lofty::read_from_path(&chemin).unwrap();
    assert_eq!(
        lofty.primary_tag().unwrap().pictures().len(),
        1,
        "pochette dans le WAV"
    );
    assert_eq!(lofty.primary_tag().unwrap().pictures()[0].data(), JPEG);
}

/// Sans MusicBrainz : « Piste NN », dossiers de repli, numéro en balise.
#[tokio::test]
async fn sans_musicbrainz_les_pistes_s_appellent_piste_nn() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    let fin = banc.extraire(json!({ "pistes": [1] })).await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    assert_eq!(fin["metadonnees"], "repli");
    let chemin = banc
        .racine()
        .join("Artiste inconnu")
        .join("Album inconnu")
        .join("01 - Piste 01.flac");
    assert_eq!(pcm_relu(&chemin), contenu_des_secteurs(0, 40));
    let m = tune_core::metadata::read_metadata(&chemin).unwrap();
    assert_eq!(m.title.as_deref(), Some("Piste 01"));
    assert_eq!(m.track_number, Some(1));
    assert_eq!(m.musicbrainz_release_id, None);
    assert!(
        !banc
            .racine()
            .join("Artiste inconnu/Album inconnu/cover.jpg")
            .exists()
    );
}

/// Les corrections de l'utilisateur l'emportent sur MusicBrainz.
#[tokio::test]
async fn les_corrections_de_l_utilisateur_l_emportent() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    let fin = banc
        .extraire(json!({
            "pistes": [2], "artiste": "Moi", "album": "Mon disque",
            "titres": { "2": "Ma piste" }
        }))
        .await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    let chemin = banc.racine().join("Moi/Mon disque/02 - Ma piste.flac");
    let m = tune_core::metadata::read_metadata(&chemin).unwrap();
    assert_eq!(
        (m.title.as_deref(), m.artist.as_deref()),
        (Some("Ma piste"), Some("Moi"))
    );
}

/// Un secteur relu : le fichier reste bit-exact, et le compte de
/// relectures le dit.
#[tokio::test]
async fn un_secteur_qui_echoue_est_relu_et_le_fichier_reste_exact() {
    let lecteur = simule();
    lecteur.faire_echouer(50, 2);
    lecteur.corrompre(50, 1);
    let banc = Banc::new(lecteur, Arc::new(SansReseau));
    let fin = banc.extraire(json!({ "pistes": [2] })).await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    assert!(
        fin["pistes"][0]["lectures_supplementaires"]
            .as_u64()
            .unwrap()
            >= 3
    );
    assert_eq!(fin["pistes"][0]["secteurs_illisibles"], 0);
    let chemin = banc
        .racine()
        .join("Artiste inconnu/Album inconnu/02 - Piste 02.flac");
    assert_eq!(pcm_relu(&chemin), contenu_des_secteurs(40, 75));
}

/// L'annulation, pendant une extraction RÉELLEMENT en cours (le lecteur est
/// retenu au milieu de la piste 2) : la piste 1 reste, la 2 n'a ni fichier
/// ni fichier provisoire, la 3 n'est pas commencée. Pendant ce temps, la
/// lecture, l'éjection et une seconde extraction sont refusées.
#[tokio::test]
async fn l_annulation_arrete_l_extraction_et_ne_laisse_rien_a_moitie() {
    let (tx_arrive, rx_arrive) = std::sync::mpsc::channel();
    let (tx_reprise, rx_reprise) = std::sync::mpsc::channel();
    // Piste 1 = 2 blocs (40 secteurs), la 4ᵉ lecture est dans la piste 2.
    let lecteur = Arc::new(LecteurBarriere {
        interieur: LecteurSimule::new(petite_toc()),
        appels: AtomicU32::new(0),
        apres: 4,
        arrive: Mutex::new(Some(tx_arrive)),
        reprise: Mutex::new(rx_reprise),
    });
    let banc = Banc::new(lecteur, Arc::new(SansReseau));
    let (code, v) = banc.appel("POST", "/extractions", Some(json!({}))).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    tokio::task::spawn_blocking(move || rx_arrive.recv_timeout(Duration::from_secs(30)))
        .await
        .unwrap()
        .expect("le lecteur atteint la barrière");

    // Une extraction tourne : tout ce qui toucherait le disque est refusé.
    let (code, v) = banc.appel("POST", "/extractions", Some(json!({}))).await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("extraction_en_cours"))
    );
    assert_eq!(v["extraction_id"], id.as_str());
    let (code, v) = banc
        .appel("POST", "/ejecter", Some(json!({ "forcer": true })))
        .await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("extraction_en_cours"))
    );
    let (code, v) = banc
        .appel("POST", "/jouer", Some(json!({ "zone_id": 1 })))
        .await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("extraction_en_cours"))
    );
    let (_, v) = banc.appel("GET", "/lecteurs", None).await;
    assert_eq!(v["lecteurs"][0]["extraction_en_cours"], id.as_str());

    let (code, v) = banc
        .appel("DELETE", &format!("/extractions/{id}"), None)
        .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    tx_reprise.send(()).unwrap();
    let fin = banc.attendre(&id).await;
    assert_eq!(fin["statut"], "annulee", "{fin}");
    let statuts: Vec<&str> = fin["pistes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["statut"].as_str().unwrap())
        .collect();
    assert_eq!(statuts, ["terminee", "annulee", "annulee"]);
    let album = banc.racine().join("Artiste inconnu/Album inconnu");
    assert!(album.join("01 - Piste 01.flac").exists());
    let deux = album.join("02 - Piste 02.flac");
    assert!(!deux.exists() && !super::travail::provisoire(&deux).exists());
    assert!(!album.join("03 - Piste 03.flac").exists());
    // La piste écrite entre quand même dans la bibliothèque.
    assert_eq!(fin["scan"], "lance");

    // Contre-épreuve : une extraction finie ne s'annule plus, et le disque
    // se laisse de nouveau éjecter.
    let (code, v) = banc
        .appel("DELETE", &format!("/extractions/{id}"), None)
        .await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("extraction_terminee"))
    );
    let (code, _) = banc.appel("POST", "/ejecter", None).await;
    assert_eq!(code, StatusCode::OK);
}

/// Un chemin hostile est refusé AVANT toute lecture : rien n'est lancé,
/// rien n'est écrit. Contre-épreuve : la même demande vers un dossier de la
/// bibliothèque est acceptée.
#[tokio::test]
async fn un_chemin_hostile_est_refuse_et_rien_n_est_ecrit() {
    let lecteur = simule();
    let banc = Banc::new(lecteur.clone(), Arc::new(SansReseau));
    let ailleurs = tempfile::tempdir().unwrap();
    let racine = banc.racine().to_string_lossy().to_string();
    let mut cas = vec![
        (format!("{racine}/../evasion"), "destination_invalide"),
        (
            ailleurs.path().to_string_lossy().to_string(),
            "hors_bibliotheque",
        ),
        ("dossier/relatif".to_string(), "destination_relative"),
        (format!("{racine}-voisin"), "hors_bibliotheque"),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(ailleurs.path(), banc.racine().join("lien")).unwrap();
        cas.push((format!("{racine}/lien/x"), "hors_bibliotheque"));
    }
    for (dest, motif) in &cas {
        let (code, v) = banc
            .appel("POST", "/extractions", Some(json!({ "destination": dest })))
            .await;
        assert_eq!(
            (code, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(*motif)),
            "{dest}"
        );
    }
    let (_, v) = banc.appel("GET", "/extractions", None).await;
    assert_eq!(v["extractions"], json!([]));
    assert_eq!(lecteur.appels(), 0, "aucun secteur lu");
    assert!(std::fs::read_dir(ailleurs.path()).unwrap().next().is_none());

    // Le réglage refuse les mêmes chemins.
    let (code, v) = banc
        .appel(
            "PUT",
            "/extraction/reglages",
            Some(json!({ "destination": cas[0].0 })),
        )
        .await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("destination_invalide"))
    );

    // Contre-épreuve.
    let fin = banc
        .extraire(json!({ "destination": format!("{racine}/CD"), "pistes": [1] }))
        .await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
    assert!(
        banc.racine()
            .join("CD/Artiste inconnu/Album inconnu/01 - Piste 01.flac")
            .exists()
    );
}

/// Le corps est validé : champ inconnu, format hors liste, piste absente,
/// texte de contrôle — 400 avec un motif stable.
#[tokio::test]
async fn le_corps_est_valide() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    for (corps, motif) in [
        (json!({ "chemin": "/x" }), "corps_invalide"),
        (json!({ "format": "mp3" }), "format_non_pris_en_charge"),
        (json!({ "verification": "parfois" }), "corps_invalide"),
        (json!({ "pistes": [42] }), "piste_inconnue"),
        (json!({ "pistes": [] }), "pistes_vides"),
        (
            json!({ "pistes": [1], "titres": { "2": "x" } }),
            "piste_inconnue",
        ),
        (json!({ "titres": { "un": "x" } }), "champ_invalide"),
        (json!({ "album": "a\u{0}b" }), "champ_invalide"),
        (json!({ "artiste": "x".repeat(201) }), "champ_invalide"),
    ] {
        let (code, v) = banc
            .appel("POST", "/extractions", Some(corps.clone()))
            .await;
        assert_eq!(
            (code, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some(motif)),
            "{corps}"
        );
    }
}

/// Des fichiers déjà là : refus qui les nomme ; `ecraser` les remplace.
#[tokio::test]
async fn des_fichiers_existants_sont_proteges_sauf_ecraser() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    let fin = banc.extraire(json!({ "pistes": [1] })).await;
    assert_eq!(fin["statut"], "terminee");
    let (code, v) = banc
        .appel("POST", "/extractions", Some(json!({ "pistes": [1] })))
        .await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("fichiers_existants"))
    );
    assert_eq!(v["fichiers"].as_array().unwrap().len(), 1);
    let fin = banc
        .extraire(json!({ "pistes": [1], "ecraser": true, "format": "flac" }))
        .await;
    assert_eq!(fin["statut"], "terminee", "{fin}");
}

/// Une zone joue le disque : l'extraction est refusée et nomme la zone.
#[tokio::test]
async fn un_disque_en_lecture_n_est_pas_extrait() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    banc.hote.sources.lock().await.insert(4, SOURCE.into());
    banc.routes.zones.lock().await.insert(4);
    let (code, v) = banc.appel("POST", "/extractions", Some(json!({}))).await;
    assert_eq!(
        (code, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("lecture_en_cours"))
    );
    assert_eq!(v["zones"], json!([4]));
}

/// Les réglages : défaut sûr (le premier emplacement), puis réglés et relus.
#[tokio::test]
async fn les_reglages_ont_un_defaut_sur_et_se_reglent() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    let racine = banc.racine().to_string_lossy().to_string();
    let (code, v) = banc.appel("GET", "/extraction/reglages", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["format"], "flac");
    assert_eq!(v["destination"], racine.as_str());
    assert_eq!(v["destination_source"], "defaut");
    assert_eq!(v["formats"], json!(["flac", "wav"]));

    let sous = format!("{racine}/Extractions");
    let (code, v) = banc
        .appel(
            "PUT",
            "/extraction/reglages",
            Some(json!({ "format": "wav", "destination": sous })),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        (v["format"].as_str(), v["destination"].as_str()),
        (Some("wav"), Some(&*sous))
    );
    assert_eq!(v["destination_source"], "reglage");
    // Sans précision, une extraction suit le réglage.
    let fin = banc.extraire(json!({ "pistes": [1] })).await;
    assert_eq!(fin["format"], "wav");
    assert!(
        Path::new(&sous)
            .join("Artiste inconnu/Album inconnu/01 - Piste 01.wav")
            .exists()
    );
    // Un emplacement retiré de la bibliothèque : retour au défaut.
    SettingsRepo::with_backend(banc.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&["/nulle/part"]).unwrap(),
        )
        .unwrap();
    let (_, v) = banc.appel("GET", "/extraction/reglages", None).await;
    assert_eq!(v["destination"], "/nulle/part");
    assert_eq!(v["destination_source"], "defaut");
}

/// L'accès : ouvert sans authentification ; avec, administrateur seulement.
#[tokio::test]
async fn les_routes_d_extraction_exigent_un_administrateur() {
    let banc = Banc::new(simule(), Arc::new(SansReseau));
    let admin = || {
        Some(AuthUser {
            user_id: 1,
            role: "admin".into(),
        })
    };
    let simple = || {
        Some(AuthUser {
            user_id: 2,
            role: "user".into(),
        })
    };
    let (code, _) = appel_avec(banc.r(), "GET", "/extractions", None, None).await;
    assert_eq!(code, StatusCode::OK, "authentification inactive : ouvert");

    SettingsRepo::with_backend(banc.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    for (methode, uri, corps) in [
        ("GET", "/lecteurs", None),
        ("GET", "/extraction/reglages", None),
        (
            "PUT",
            "/extraction/reglages",
            Some(json!({ "format": "wav" })),
        ),
        ("GET", "/extractions", None),
        ("POST", "/extractions", Some(json!({}))),
        ("GET", "/extractions/x", None),
        ("DELETE", "/extractions/x", None),
    ] {
        let (code, _) = appel_avec(banc.r(), methode, uri, corps.clone(), None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{methode} {uri}");
        let (code, v) = appel_avec(banc.r(), methode, uri, corps.clone(), simple()).await;
        assert_eq!(
            (code, v["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("admin_requis")),
            "{methode} {uri}"
        );
    }
    let (code, _) = appel_avec(banc.r(), "GET", "/extractions", None, admin()).await;
    assert_eq!(code, StatusCode::OK);
    // La lecture du disque, elle, n'est pas changée.
    let (code, _) = appel_avec(banc.r(), "GET", "/etat", None, simple()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(banc.ex.liste().len(), 0, "rien n'a été lancé sans droit");

    assert!(verdict_admin(false, None).is_ok());
    assert!(verdict_admin(true, admin().as_ref()).is_ok());
}
