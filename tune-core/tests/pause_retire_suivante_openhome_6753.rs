//! #6753 — Linn OpenHome : la file Tune est en pause, le testeur écoute avec
//! Kazoo, et quelques minutes plus tard la suite de la file Tune se met à
//! jouer.
//!
//! Tune n'envoie rien pendant la pause : c'est le renderer qui avance seul
//! dans sa liste partagée, où `set_next_media` avait rangé la suivante de
//! Tune (`Playlist Insert`) sans jamais l'en retirer. Kazoo insère ses pistes
//! après la piste en cours ; quand elles se terminent, le Linn enchaîne sur
//! l'entrée de Tune.
//!
//! Témoins COMPORTEMENTAUX : une vraie `OpenHomeOutput` contre un faux
//! renderer SOAP sur `127.0.0.1`, qui note chaque action reçue. Aucun son.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tune_core::outputs::openhome::OpenHomeOutput;
use tune_core::outputs::traits::{OutputTarget, PlayMedia};

/// Ce que le faux renderer a reçu : `(action, corps SOAP)`, dans l'ordre.
type Journal = Arc<Mutex<Vec<(String, String)>>>;

/// Un faux renderer OpenHome : `Insert` rend des `NewId` croissants à partir
/// de 1, `Id` rend la valeur de `courante` (l'entrée « en lecture »), toute
/// autre action répond une enveloppe vide.
async fn renderer(courante: Arc<AtomicU32>) -> (u16, Journal) {
    let journal: Journal = Arc::new(Mutex::new(Vec::new()));
    let prochain_id = Arc::new(AtomicU32::new(1));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let j = journal.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let journal = j.clone();
            let courante = courante.clone();
            let prochain_id = prochain_id.clone();
            tokio::spawn(async move {
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
                let texte = String::from_utf8_lossy(&brut).to_string();
                let action = texte
                    .lines()
                    .find_map(|l| {
                        let (nom, valeur) = l.split_once(':')?;
                        nom.eq_ignore_ascii_case("soapaction").then(|| {
                            valeur
                                .trim()
                                .trim_matches('"')
                                .rsplit('#')
                                .next()
                                .unwrap_or("")
                                .to_string()
                        })
                    })
                    .unwrap_or_default();
                let corps = match action.as_str() {
                    "Insert" => {
                        let id = prochain_id.fetch_add(1, Ordering::SeqCst);
                        format!("<u:InsertResponse><NewId>{id}</NewId></u:InsertResponse>")
                    }
                    "Id" => format!(
                        "<u:IdResponse><Value>{}</Value></u:IdResponse>",
                        courante.load(Ordering::SeqCst)
                    ),
                    autre => format!("<u:{autre}Response></u:{autre}Response>"),
                };
                journal.lock().await.push((action, texte));
                let corps = format!("<s:Envelope><s:Body>{corps}</s:Body></s:Envelope>");
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (port, journal)
}

fn sortie(port: u16) -> OpenHomeOutput {
    let mut services = HashMap::new();
    services.insert("playlist".to_string(), "/Playlist".to_string());
    services.insert("transport".to_string(), "/Transport".to_string());
    OpenHomeOutput::new(
        "Linn Klimax".into(),
        "uuid:linn-6753".into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )
}

fn media(url: &'static str) -> PlayMedia<'static> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        ..Default::default()
    }
}

/// Les `Value` des `DeleteId` reçus, dans l'ordre.
async fn delete_ids(journal: &Journal) -> Vec<String> {
    journal
        .lock()
        .await
        .iter()
        .filter(|(action, _)| action == "DeleteId")
        .filter_map(|(_, corps)| {
            let debut = corps.find("<Value>")? + "<Value>".len();
            let fin = corps[debut..].find("</Value>")? + debut;
            Some(corps[debut..fin].to_string())
        })
        .collect()
}

/// 🔴 Le témoin de #6753. Rouge avant : la pause n'envoyait que
/// `Transport Pause`, la suivante (id 2) restait dans la liste du Linn.
#[tokio::test]
async fn la_pause_retire_la_suivante_armee_de_la_liste_du_renderer() {
    let courante = Arc::new(AtomicU32::new(1));
    let (port, journal) = renderer(courante).await;
    let oh = sortie(port);

    oh.play_media(&media("http://tune/piste-1")).await.unwrap();
    oh.set_next_media(&media("http://tune/piste-2"))
        .await
        .unwrap();
    oh.pause().await.unwrap();

    assert_eq!(
        delete_ids(&journal).await,
        vec!["2".to_string()],
        "après la pause, la suivante armée par Tune (id 2) doit être retirée de la \
         liste du renderer, sinon il l'enchaîne après les pistes de Kazoo"
    );
}

/// Le renderer est déjà passé sur la suivante (gapless) : c'est la piste en
/// cours, elle ne doit pas disparaître sous l'auditeur.
#[tokio::test]
async fn la_pause_ne_retire_pas_la_suivante_devenue_piste_en_cours() {
    let courante = Arc::new(AtomicU32::new(1));
    let (port, journal) = renderer(courante.clone()).await;
    let oh = sortie(port);

    oh.play_media(&media("http://tune/piste-1")).await.unwrap();
    oh.set_next_media(&media("http://tune/piste-2"))
        .await
        .unwrap();
    courante.store(2, Ordering::SeqCst);
    oh.pause().await.unwrap();

    assert!(
        delete_ids(&journal).await.is_empty(),
        "l'entrée en cours (id 2) ne doit pas être retirée"
    );
}

/// L'arrêt laisse le même risque que la pause : la suivante est retirée, et
/// une seconde pause n'a plus rien à retirer.
#[tokio::test]
async fn l_arret_retire_aussi_la_suivante_et_une_seule_fois() {
    let courante = Arc::new(AtomicU32::new(1));
    let (port, journal) = renderer(courante).await;
    let oh = sortie(port);

    oh.play_media(&media("http://tune/piste-1")).await.unwrap();
    oh.set_next_media(&media("http://tune/piste-2"))
        .await
        .unwrap();
    oh.stop().await.unwrap();
    assert_eq!(
        delete_ids(&journal).await,
        vec!["2".to_string()],
        "après l'arrêt, la suivante armée par Tune (id 2) doit être retirée"
    );

    oh.pause().await.unwrap();
    assert_eq!(
        delete_ids(&journal).await,
        vec!["2".to_string()],
        "une entrée déjà retirée ne doit pas l'être une seconde fois"
    );
}

/// Après la reprise, une nouvelle suivante armée est de nouveau suivie et
/// retirée à la pause suivante.
#[tokio::test]
async fn une_suivante_rearmee_apres_la_reprise_est_retiree_a_la_pause_suivante() {
    let courante = Arc::new(AtomicU32::new(1));
    let (port, journal) = renderer(courante).await;
    let oh = sortie(port);

    oh.play_media(&media("http://tune/piste-1")).await.unwrap();
    oh.set_next_media(&media("http://tune/piste-2"))
        .await
        .unwrap();
    oh.pause().await.unwrap();
    oh.resume().await.unwrap();
    oh.set_next_media(&media("http://tune/piste-2"))
        .await
        .unwrap();
    oh.pause().await.unwrap();

    assert_eq!(
        delete_ids(&journal).await,
        vec!["2".to_string(), "3".to_string()]
    );
}
