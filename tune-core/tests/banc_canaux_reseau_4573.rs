//! Le banc partagé de #4573 : un faux renderer SOAP qui répond un Sink donné,
//! une base avec une zone DLNA et une piste multicanale. Inclus par
//! `#[path]` dans `canaux_reseau_branchement_4573.rs` et
//! `canaux_origine_journal_6599.rs` ; il n'est pas une cible à lui seul.
#![allow(dead_code)]

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

pub const UDN: &str = "uuid:denon-x1600h-4573";

/// Un ConnectionManager qui répond toujours le même Sink.
///
/// Une tâche par connexion, et `Connection: close` : la résolution enchaîne
/// DEUX sondes SOAP (`supports_mime` pour la négociation FLAC, puis les
/// canaux), et `reqwest` réemploie sa connexion. Un serveur qui n'en sert
/// qu'une à la fois rendait la seconde sonde INCONCLUANTE — donc `None`,
/// donc aucune réduction : un faux rouge qui ressemblait trait pour trait au
/// défaut qu'on corrige.
pub async fn renderer_qui_annonce(sink: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // Lire la requête jusqu'au bout de ses en-têtes : une pile
                // SOAP qui répond avant d'avoir lu fait échouer l'écriture
                // du client.
                let mut brut = Vec::new();
                let mut tampon = [0u8; 4096];
                while let Ok(n) = sock.read(&mut tampon).await {
                    if n == 0 {
                        break;
                    }
                    brut.extend_from_slice(&tampon[..n]);
                    let texte = String::from_utf8_lossy(&brut);
                    let Some(fin) = texte.find("\r\n\r\n") else {
                        continue;
                    };
                    let attendu: usize = texte
                        .lines()
                        .find_map(|l| {
                            let (nom, valeur) = l.split_once(':')?;
                            nom.eq_ignore_ascii_case("content-length")
                                .then(|| valeur.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if brut.len() >= fin + 4 + attendu {
                        break;
                    }
                }
                let corps = format!(
                    concat!(
                        r#"<?xml version="1.0"?>"#,
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                        "<s:Body><u:GetProtocolInfoResponse><Sink>{}</Sink>",
                        "</u:GetProtocolInfoResponse></s:Body></s:Envelope>"
                    ),
                    sink
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://127.0.0.1:{port}"), handle)
}

/// Une base avec une zone DLNA pointée sur `UDN` et une piste déclarée à
/// `canaux` voies. Le FICHIER est la stéréo de la caisse : la décision se
/// prend sur la ligne `tracks`, pas sur les octets.
pub fn base_avec_piste(canaux: i32) -> (Arc<dyn DbBackend>, i64, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let zone_id = zones
        .create("Home Theater", Some("dlna"), Some(UDN))
        .unwrap();

    let chemin = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");
    let mut t = tune_core::db::models::Track::new("Piste 5.1".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(48_000);
    t.bit_depth = Some(24);
    t.channels = canaux;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap();

    tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(backend.clone())
        .append(
            zone_id,
            &[tune_core::db::play_queue_repo::QueueInput::Local { track_id }],
        )
        .unwrap();
    (backend, zone_id, track_id)
}

/// Le Sink RÉEL du Denon AVR-X1600H de Xavier : du LPCM en 1 et 2 canaux, et
/// du FLAC sans aucun `channels=`. Le maximum déclaré est donc DEUX.
pub const SINK_DENON: &str = "http-get:*:audio/L16;rate=44100;channels=1:*,\
                          http-get:*:audio/L16;rate=44100;channels=2:*,\
                          http-get:*:audio/L16;rate=48000;channels=2:*,\
                          http-get:*:audio/flac:*,\
                          http-get:*:audio/wav:*";

/// Un renderer qui ne dit RIEN de ses canaux — le cas le plus fréquent, et
/// celui où Tune ne doit surtout pas décider à la place de l'auditeur.
pub const SINK_MUET_SUR_LES_CANAUX: &str = "http-get:*:audio/flac:*,\
                                        http-get:*:audio/wav:*,\
                                        http-get:*:audio/mpeg:*";

/// Annonce synthétique : contrairement au relevé Denon, le FLAC lui-même
/// déclare une limite stéréo. Elle éprouve le repli et son journal.
pub const SINK_FLAC_STEREO: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,\
http-get:*:audio/flac;channels=2:*";
