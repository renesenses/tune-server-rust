//! **#8052 — le DSD d'un prêt de sorties arrive natif au serveur qui
//! possède la sortie**, et c'est lui qui décide du format, comme en local.
//!
//! # Le défaut
//!
//! Signalé sur le fil ASR (06/11, DAC Gustard X16) : un `.dsf` joué en local
//! sur le serveur qui a le DAC partait en DoP ; lancé depuis le serveur
//! principal sur la même sortie (« Play on another Tune server's outputs »),
//! il arrivait converti en PCM 176,4 kHz.
//!
//! Le prêt de sorties passe par la façade UPnP du prêteur : le principal la
//! voit comme un renderer DLNA et la sonde. Trois trous :
//!
//! 1. la façade annonçait un `Sink` FIXE, sans un seul type DSD. Le principal
//!    en concluait « pas de DSD » (`parse_dsd_capability`, sondage concluant)
//!    et décimait lui-même en PCM, zone en `auto` ;
//! 2. le prêteur qui recevait un DSD par la façade le remettait tel quel à
//!    `LocalOutput` (bras générique de `resolve_direct`) : ni DoP ni natif,
//!    quel que soit le réglage de la zone — et `LocalOutput` ne décode pas un
//!    `.dsf` brut : la zone restait muette. Le prêteur joue désormais le
//!    fichier reçu comme la lecture locale (`servir_la_piste_locale`) : natif,
//!    DoP vérifié, ou PCM converti par lui quand le DoP est écarté (#7265) ;
//! 3. il ne le reconnaissait même pas comme DSD : l'adresse `/stream/<id>.dsf`
//!    n'a pas de DIDL côté façade, et l'extension `.dsf` tombait sur le
//!    défaut `audio/mpeg` de `mime_depuis_l_extension`.
//!
//! Sabotages qui font rougir les témoins :
//! - `zone_pretee_accepte_le_dsd` qui rend toujours `false` : rougit
//!   `la_facade_d_une_sortie_locale_en_dop_ou_natif_annonce_le_dsd` et
//!   `le_principal_n_envoie_plus_de_pcm_a_une_facade_qui_annonce_le_dsd` ;
//! - retirer le bras `est_dsd_brut(mime_type) && is_local_output` de
//!   `resolve_direct` : rougit
//!   `le_preteur_sert_en_dop_un_dsd_recu_par_la_facade` ;
//! - faire rendre au bras l'adresse brute quand la lecture locale ne sert
//!   pas de DoP (le correctif de la revue retiré) : rougissent
//!   `le_preteur_convertit_en_pcm_quand_sa_sortie_ecarte_le_dop` et
//!   `le_preteur_en_auto_ou_pcm_convertit_en_pcm_un_dsd_recu` ;
//! - retirer `.dsf`/`.dff` de `mime_depuis_l_extension` : rougit
//!   `l_adresse_dsf_du_principal_est_reconnue_comme_du_dsd` et, par la porte
//!   publique, `le_preteur_sert_en_dop_un_dsd_recu_par_la_facade`.

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use crate::upnp_server::{
    build_renderer_connection_manager_response_pour, zone_pretee_accepte_le_dsd,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const SORTIE_LOCALE: &str = "local:Gustard X16";
const SOAP_GET_PROTOCOL_INFO: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body>
</s:Envelope>"#;

/// Le `Sink` de la réponse GetProtocolInfo, découpé comme le fait le client
/// DLNA (une entrée par virgule).
fn sink(reponse: &str) -> Vec<String> {
    let debut = reponse.find("<Sink>").expect("un Sink") + "<Sink>".len();
    let fin = reponse.find("</Sink>").expect("un Sink fermé");
    reponse[debut..fin]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Ce que la façade d'une zone répond au GetProtocolInfo du principal.
fn sink_de_la_facade(output_device_id: Option<&str>, dsd_mode: &str) -> Vec<String> {
    let accepte = zone_pretee_accepte_le_dsd(output_device_id, dsd_mode);
    sink(&build_renderer_connection_manager_response_pour(
        SOAP_GET_PROTOCOL_INFO,
        accepte,
    ))
}

/// **Le défaut côté annonce.** Une zone prêtée dont la sortie locale joue le
/// DSD en DoP (`dop`) ou en natif (`native`, natif ASIO ou DoP selon le
/// pilote) l'annonce dans son `Sink`, avec `application/x-dsd` en tête.
#[test]
fn la_facade_d_une_sortie_locale_en_dop_ou_natif_annonce_le_dsd() {
    for dsd_mode in ["dop", "native"] {
        let sink = sink_de_la_facade(Some(SORTIE_LOCALE), dsd_mode);
        let cap = crate::outputs::dlna::parse_dsd_capability(&sink);
        assert!(
            cap.supports_dsf,
            "[{dsd_mode}] la façade d'une sortie locale qui joue le DSD doit l'annoncer : \
             sans cela le serveur principal décime en PCM 176,4 kHz (#8052). Sink : {sink:?}"
        );
        assert!(cap.supports_dff, "[{dsd_mode}] le DFF aussi");
        assert_eq!(
            cap.dsf_mime.as_deref(),
            Some("application/x-dsd"),
            "[{dsd_mode}] le principal reprend la première orthographe DSD du Sink"
        );
        // Les formats d'avant restent annoncés.
        assert!(sink.iter().any(|p| p == "http-get:*:audio/flac:*"));
    }
}

/// **Non-régression.** En `auto` et en `pcm`, la sortie locale joue du PCM
/// de toute façon ; une zone réseau prêtée relève de #8053. Ces zones gardent
/// le `Sink` d'avant, sans DSD : le principal décime, comme avant.
#[test]
fn la_facade_garde_son_sink_sans_dsd_quand_la_sortie_jouerait_du_pcm() {
    for (sortie, dsd_mode) in [
        (Some(SORTIE_LOCALE), "auto"),
        (Some(SORTIE_LOCALE), "pcm"),
        (Some("dlna:uuid-eversolo"), "dop"),
        (Some("dlna:uuid-eversolo"), "native"),
        (None, "dop"),
    ] {
        let sink = sink_de_la_facade(sortie, dsd_mode);
        let cap = crate::outputs::dlna::parse_dsd_capability(&sink);
        assert!(
            !cap.supports_dsf && !cap.supports_dff,
            "[{sortie:?} / {dsd_mode}] pas de DSD annoncé : {sink:?}"
        );
        assert_eq!(
            sink.join(","),
            crate::upnp_server::renderer_sink_protocol_info(false),
            "[{sortie:?} / {dsd_mode}] le Sink d'avant, à l'identique"
        );
    }
}

/// **Le défaut côté principal, bout à bout.** La zone du principal qui vise
/// la façade est en `auto` (réglage par défaut). Avec l'annonce de la façade,
/// le principal envoie le `.dsf` tel quel — il ne le décime plus.
#[test]
fn le_principal_n_envoie_plus_de_pcm_a_une_facade_qui_annonce_le_dsd() {
    for dsd_mode_du_preteur in ["dop", "native"] {
        let sink = sink_de_la_facade(Some(SORTIE_LOCALE), dsd_mode_du_preteur);
        let annonce = Some(crate::outputs::dlna::parse_dsd_capability(&sink).supports_dsf);
        assert!(
            PlaybackOrchestrator::decider_passthrough_dsd("auto", annonce),
            "[{dsd_mode_du_preteur}] le principal en `auto` doit envoyer le DSD natif \
             à la façade, pas du PCM 176,4 kHz (#8052)"
        );
    }
    // Et une façade sans DSD continue de recevoir du PCM.
    let sink = sink_de_la_facade(Some(SORTIE_LOCALE), "auto");
    let annonce = Some(crate::outputs::dlna::parse_dsd_capability(&sink).supports_dsf);
    assert!(!PlaybackOrchestrator::decider_passthrough_dsd(
        "auto", annonce
    ));
}

/// L'adresse que le principal remet à la façade en passthrough est un
/// `/stream/<id>.dsf` sans DIDL exploitable : c'est son extension qui dit DSD.
#[test]
fn l_adresse_dsf_du_principal_est_reconnue_comme_du_dsd() {
    for url in [
        "http://192.168.1.15:8888/stream/7c1e2b9a-8052.dsf",
        "http://192.168.1.15:8888/stream/7c1e2b9a-8052.DFF?t=1",
    ] {
        let mime = crate::orchestrator::mime_depuis_l_extension(url);
        assert_eq!(
            mime,
            Some("application/x-dsd"),
            "{url} doit être reconnu comme du DSD, pas étiqueté MP3 (#8052)"
        );
    }
}

/// Un DSF petit mais valide, stéréo DSD64, seize blocs par canal.
fn dsf_minuscule() -> Vec<u8> {
    let channels: u32 = 2;
    let block_size: u32 = 4096;
    let sample_rate: u32 = 2_822_400;
    let super_blocs: usize = 16;
    let total_samples: u64 = block_size as u64 * 8 * super_blocs as u64;
    let mut data = Vec::new();
    for _ in 0..super_blocs {
        for _ in 0..channels {
            data.extend(std::iter::repeat_n(0x69u8, block_size as usize));
        }
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&block_size.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    buf
}

/// Le serveur principal : il sert le `.dsf` brut à l'adresse `/stream/<id>.dsf`
/// que le passthrough remet à la façade. Compte les GET reçus.
struct ServeurPrincipal {
    url: String,
    requetes: Arc<AtomicUsize>,
    _tache: tokio::task::JoinHandle<()>,
}

impl Drop for ServeurPrincipal {
    fn drop(&mut self) {
        self._tache.abort();
    }
}

async fn serveur_principal() -> ServeurPrincipal {
    serveur_principal_qui_sert(dsf_minuscule()).await
}

/// Le serveur principal, qui sert `corps` à l'adresse `/stream/<id>.dsf`.
async fn serveur_principal_qui_sert(corps: Vec<u8>) -> ServeurPrincipal {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/stream/7c1e2b9a-8052.dsf",
        listener.local_addr().unwrap()
    );
    let requetes = Arc::new(AtomicUsize::new(0));
    let compteur = requetes.clone();
    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut requete = Vec::new();
            let mut octet = [0u8; 1];
            while !requete.ends_with(b"\r\n\r\n") {
                match socket.read_exact(&mut octet).await {
                    Ok(_) => requete.push(octet[0]),
                    Err(_) => break,
                }
                if requete.len() > 16_384 {
                    break;
                }
            }
            compteur.fetch_add(1, Ordering::SeqCst);
            let entete = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-dsd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                corps.len()
            );
            let _ = socket.write_all(entete.as_bytes()).await;
            let _ = socket.write_all(&corps).await;
            let _ = socket.shutdown().await;
        }
    });
    ServeurPrincipal {
        url,
        requetes,
        _tache: tache,
    }
}

/// Le serveur prêteur : une zone LOCALE au `dsd_mode` demandé.
fn preteur(dsd_mode: &str) -> (PlaybackOrchestrator, i64) {
    preteur_sur(SORTIE_LOCALE, dsd_mode)
}

/// Le serveur prêteur, sa zone sur la sortie locale `sortie`.
fn preteur_sur(sortie: &str, dsd_mode: &str) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(db.clone());
    let premier_id = zones
        .create("Gustard X16", Some("local"), Some(sortie))
        .unwrap();
    // Un identifiant propre à chaque prêteur : la raison #7265 vit dans un
    // registre GLOBAL par zone (`dop_ecarte_7265::par_zone`), partagé par
    // tous les tests du binaire. Chaque base en mémoire numérote sa première
    // zone 1 ; une résolution DSD locale d'un autre test sur « la zone 1 »
    // appelait `oublier(1)` entre la pose de la raison et sa lecture, et
    // `le_preteur_convertit_en_pcm_quand_sa_sortie_ecarte_le_dop` rougissait
    // par moments en CI (« la garde #7265 a bien écarté le DoP »).
    static PROCHAIN_ID: AtomicI64 = AtomicI64::new(8_052_000_001);
    let zone_id = PROCHAIN_ID.fetch_add(1, Ordering::SeqCst);
    db.execute(
        "UPDATE zones SET id = ? WHERE id = ?",
        &[&zone_id as &dyn ToSqlValue, &premier_id as &dyn ToSqlValue],
    )
    .unwrap();
    zones.update_dsd_mode(zone_id, dsd_mode).unwrap();
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    (orch, zone_id)
}

/// La demande que la façade bâtit sur `SetAVTransportURI` puis `Play`
/// (`routes/upnp_media_renderer.rs`) : source `upnp`, l'adresse reçue, AUCUNE
/// ligne en base.
fn demande_de_la_facade(zone_id: i64, url: &str) -> PlayRequest {
    demande_de_la_facade_sur(SORTIE_LOCALE, zone_id, url)
}

fn demande_de_la_facade_sur(sortie: &str, zone_id: i64, url: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(sortie.into()),
        track_id: None,
        source: Some("upnp".into()),
        source_id: Some(url.to_string()),
        title: Some("Abacab".into()),
        artist_name: Some("Genesis".into()),
        ..Default::default()
    }
}

/// **Le défaut côté prêteur.** Zone locale en `dop` : le DSD reçu par la
/// façade part en DoP vers la sortie locale — trames PCM 24 bits à 176,4 kHz
/// pour un DSD64, le fil du `.dsf` de la bibliothèque — et non plus en adresse
/// brute remise telle quelle à `LocalOutput`. En `native`, hors ASIO (ici,
/// Linux sans pilote), la sortie locale reçoit aussi le DoP, comme en local.
#[tokio::test]
async fn le_preteur_sert_en_dop_un_dsd_recu_par_la_facade() {
    for dsd_mode in ["dop", "native"] {
        let principal = serveur_principal().await;
        let (orch, zone_id) = preteur(dsd_mode);
        let resolu = orch
            .resolve_stream(&demande_de_la_facade(zone_id, &principal.url))
            .await
            .unwrap();

        assert_ne!(
            resolu.url, principal.url,
            "[{dsd_mode}] le DSD reçu par la façade ne doit pas partir en adresse brute \
             vers la sortie locale : ni DoP ni natif n'en sortiraient (#8052)"
        );
        assert!(
            resolu.url.contains("/stream/") && resolu.url.ends_with(".wav"),
            "[{dsd_mode}] la sortie locale tire le DoP d'une session de Tune : {}",
            resolu.url
        );
        assert_eq!(resolu.mime_type, "audio/wav", "[{dsd_mode}]");
        assert_eq!(
            resolu.sample_rate,
            Some(176_400),
            "[{dsd_mode}] la cadence DoP d'un DSD64 est 176,4 kHz"
        );
        assert_eq!(resolu.bit_depth, Some(24), "[{dsd_mode}]");
        assert_eq!(resolu.source, "upnp", "[{dsd_mode}]");
        assert_eq!(
            resolu.origin_url.as_deref(),
            Some(principal.url.as_str()),
            "[{dsd_mode}] l'origine reste connue"
        );
        assert_eq!(
            principal.requetes.load(Ordering::SeqCst),
            1,
            "[{dsd_mode}] le prêteur tire le fichier une fois"
        );
    }
}

/// Ce qu'une résolution du prêteur doit être quand la sortie locale ne
/// reçoit pas le DSD : du PCM qu'il a converti lui-même et qu'il sert depuis
/// une session de Tune — jamais l'adresse `.dsf` du principal, que
/// `LocalOutput` ne sait pas lire (zone muette).
fn assert_pcm_servi_par_le_preteur(
    cas: &str,
    resolu: &crate::orchestrator::ResolvedStream,
    principal: &ServeurPrincipal,
) {
    assert_ne!(
        resolu.url, principal.url,
        "[{cas}] le `.dsf` brut du principal ne doit pas partir vers la sortie locale : \
         `LocalOutput` ne le décode pas et la zone resterait muette (#8052)"
    );
    assert!(
        !resolu.url.contains(".dsf") && !resolu.url.contains(".dff"),
        "[{cas}] aucune adresse DSD brute : {}",
        resolu.url
    );
    assert!(
        resolu.stream_id.is_some() && resolu.url.contains("/stream/"),
        "[{cas}] le PCM est servi par une session de Tune : {}",
        resolu.url
    );
    assert_eq!(resolu.mime_type, "audio/wav", "[{cas}] du PCM en WAV");
    assert_ne!(
        resolu.bit_depth,
        Some(24),
        "[{cas}] pas de DoP (trames 24 bits) : la sortie ne le reçoit pas"
    );
    assert_eq!(resolu.source, "upnp", "[{cas}]");
    assert_eq!(
        resolu.origin_url.as_deref(),
        Some(principal.url.as_str()),
        "[{cas}] l'origine reste connue"
    );
}

/// **Le repli, sortie qui écarte le DoP (#7265).** Zone en `dop` (ou
/// `native`) dont la façade a donc annoncé le DSD, mais dont la sortie WASAPI
/// ne peut pas porter le DoP : mixeur partagé à 48 kHz, ou pilote exclusif
/// qui refuse le 24 bits. En lecture locale, Tune convertit alors en PCM ; le
/// prêteur doit faire de même avec le fichier reçu, et non rendre l'adresse
/// brute.
#[tokio::test]
async fn le_preteur_convertit_en_pcm_quand_sa_sortie_ecarte_le_dop() {
    use crate::audio::dop_ecarte_7265::{AccueilDuDop, ecart_de_la_zone};
    for (cas, sortie, dsd_mode, accueil) in [
        (
            "mixeur partagé à 48 kHz, dop",
            "local:Gustard X16 (WASAPI partagé 8052)",
            "dop",
            AccueilDuDop::MixeurPartage {
                cadence: 48_000,
                voies: 2,
            },
        ),
        (
            "pilote exclusif sans 24 bits, native",
            "local:Gustard X16 (WASAPI exclusif 8052)",
            "native",
            AccueilDuDop::ExclusifRefuse {
                detail: "aucun format 24 bits à 176,4 kHz".into(),
            },
        ),
    ] {
        super::resolve_local::forcer_l_accueil_du_dop_pour_test(sortie, accueil);
        assert!(
            zone_pretee_accepte_le_dsd(Some(sortie), dsd_mode),
            "[{cas}] prémisse : la façade de cette zone annonce le DSD"
        );
        let principal = serveur_principal().await;
        let (orch, zone_id) = preteur_sur(sortie, dsd_mode);
        let resolu = orch
            .resolve_stream(&demande_de_la_facade_sur(sortie, zone_id, &principal.url))
            .await
            .unwrap_or_else(|e| panic!("[{cas}] le prêteur doit jouer la piste : {e}"));

        assert!(
            ecart_de_la_zone(zone_id).is_some(),
            "[{cas}] prémisse : la garde #7265 a bien écarté le DoP"
        );
        assert_pcm_servi_par_le_preteur(cas, &resolu, &principal);
        assert_eq!(
            principal.requetes.load(Ordering::SeqCst),
            1,
            "[{cas}] le fichier téléchargé sert à la conversion : pas de second transfert"
        );
    }
}

/// **Le repli, en-tête illisible.** Un `.dsf` dont l'en-tête ne se lit pas
/// ne part pas non plus en adresse brute : le prêteur le traite comme la
/// lecture locale traiterait ce fichier, par une session de Tune.
#[tokio::test]
async fn le_preteur_ne_rend_pas_l_adresse_brute_d_un_dsd_a_l_entete_illisible() {
    let mut abime = dsf_minuscule();
    abime[0..4].copy_from_slice(b"XXXX");
    let principal = serveur_principal_qui_sert(abime).await;
    let (orch, zone_id) = preteur("dop");
    let resolu = orch
        .resolve_stream(&demande_de_la_facade(zone_id, &principal.url))
        .await
        .unwrap_or_else(|e| panic!("le prêteur doit résoudre la piste : {e}"));
    assert_ne!(
        resolu.url, principal.url,
        "un DSD à l'en-tête illisible ne doit pas partir en adresse brute vers la \
         sortie locale, qui ne le lirait pas (#8052)"
    );
    assert!(
        resolu.stream_id.is_some()
            && resolu.url.contains("/stream/")
            && !resolu.url.contains(".dsf"),
        "le fichier reçu est servi par une session de Tune : {}",
        resolu.url
    );
    assert_eq!(resolu.mime_type, "audio/wav");
    assert_eq!(principal.requetes.load(Ordering::SeqCst), 1);
}

/// **Non-régression et filet.** Zone locale en `auto` ou `pcm` : sa façade
/// n'annonce pas le DSD, le principal lui envoie donc du PCM, comme avant.
/// Mais un principal plus ancien, ou dont le sondage date d'un autre réglage
/// de la zone, peut lui envoyer un `.dsf` : le prêteur le convertit en PCM,
/// comme la lecture locale du même fichier, au lieu de rendre l'adresse brute
/// que la sortie locale ne lirait pas.
#[tokio::test]
async fn le_preteur_en_auto_ou_pcm_convertit_en_pcm_un_dsd_recu() {
    for dsd_mode in ["auto", "pcm"] {
        let principal = serveur_principal().await;
        let (orch, zone_id) = preteur(dsd_mode);
        let resolu = orch
            .resolve_stream(&demande_de_la_facade(zone_id, &principal.url))
            .await
            .unwrap_or_else(|e| panic!("[{dsd_mode}] le prêteur doit jouer la piste : {e}"));
        assert_pcm_servi_par_le_preteur(dsd_mode, &resolu, &principal);
    }
}
