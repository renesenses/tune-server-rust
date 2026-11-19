//! #8855 (fil 2749) — SÉCURITÉ volume, sortie AirPlay 1 (RAOP).
//!
//! La sortie naissait à `volume: 1.0` et la découverte ne la semait jamais
//! du volume de sa zone. `set_mute(false)` renvoie ce niveau au récepteur :
//! une zone dé-sourdinée avant tout geste de volume repartait à 0 dB, le
//! maximum AirPlay, quel que soit son curseur.
use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::airplay2::volume_de_depart_8402::VOLUME_DE_DEPART_PRUDENT;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const APPAREIL: &str = "airplay:fixture-8855";

fn sortie() -> AirplayOutput {
    AirplayOutput::new(
        "Enceinte 8855".into(),
        APPAREIL.into(),
        "127.0.0.1".into(),
        1,
    )
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un faux récepteur : répond 200 à chaque requête RTSP et rend leur texte.
async fn session_et_recepteur() -> (RtspSession, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let recepteur = tokio::spawn(async move {
        let (mut pair, _) = listener.accept().await.unwrap();
        let mut requetes = Vec::new();
        let mut tampon = vec![0u8; 4096];
        loop {
            let lus = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                pair.read(&mut tampon),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            let texte = String::from_utf8_lossy(&tampon[..lus]).to_string();
            let cseq = texte
                .lines()
                .find_map(|l| l.strip_prefix("CSeq: "))
                .unwrap_or("0")
                .trim()
                .to_string();
            requetes.push(texte);
            pair.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .await
                .unwrap();
        }
        requetes
    });
    let session = RtspSession::connect("127.0.0.1", port).await.unwrap();
    (session, recepteur)
}

/// Sourdine puis dé-sourdinage en session : rend le corps du second
/// `SET_PARAMETER`, celui du dé-sourdinage.
async fn volume_rendu_au_desourdinage(sortie: &AirplayOutput) -> String {
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let requetes = recepteur.await.unwrap();
    assert_eq!(requetes.len(), 2, "{requetes:?}");
    assert!(requetes[0].contains("volume: -144.0"), "{requetes:?}");
    requetes[1].clone()
}

/// Témoin : une sortie neuve, jamais semée ni réglée, dé-sourdinée en
/// session, ne renvoie pas 0 dB au récepteur. Rouge avant le correctif
/// (`volume: 0.0`).
#[tokio::test]
async fn une_sortie_neuve_desourdinee_ne_repart_pas_a_zero_db_8855() {
    let sortie = sortie();
    let rendu = volume_rendu_au_desourdinage(&sortie).await;
    let attendu = format!("volume: {:.1}", linear_to_airplay_db(VOLUME_DE_DEPART_PRUDENT));
    assert!(
        rendu.contains(&attendu),
        "le dé-sourdinage doit rendre le niveau prudent ({attendu}) : {rendu}"
    );
    assert!(!rendu.contains("volume: 0.0"), "{rendu}");
    assert_eq!(
        sortie.get_status().await.unwrap().volume,
        VOLUME_DE_DEPART_PRUDENT
    );
}

/// La graine de la découverte : le volume mémorisé de la zone, borné par sa
/// limite de sécurité ; c'est lui que rend le dé-sourdinage.
#[tokio::test]
async fn la_graine_suit_la_zone_et_sa_limite_de_securite_8855() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Salon 8855", Some("airplay"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 80.0).unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set(&crate::audio::volume_limites::cle_securite(zone_id), "0.5")
        .unwrap();

    let sortie = sortie();
    sortie.ensemencer_depuis_la_base(&db).await;
    assert!((sortie.get_status().await.unwrap().volume - 0.5).abs() < 1e-9);
    let rendu = volume_rendu_au_desourdinage(&sortie).await;
    assert!(
        rendu.contains(&format!("volume: {:.1}", linear_to_airplay_db(0.5))),
        "{rendu}"
    );
}

/// Contre-épreuve de la graine : une zone à 25 % rend 25 %, pas le niveau
/// prudent ; sans zone connue, la graine garde le niveau prudent.
#[tokio::test]
async fn la_graine_rend_le_curseur_de_la_zone_et_sans_zone_le_niveau_prudent_8855() {
    let db = base();
    let sans_zone = sortie();
    sans_zone.ensemencer_depuis_la_base(&db).await;
    assert_eq!(
        sans_zone.get_status().await.unwrap().volume,
        VOLUME_DE_DEPART_PRUDENT
    );

    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Bureau 8855", Some("airplay"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 25.0).unwrap();
    let sortie = sortie();
    sortie.ensemencer_depuis_la_base(&db).await;
    let rendu = volume_rendu_au_desourdinage(&sortie).await;
    assert!(
        rendu.contains(&format!("volume: {:.1}", linear_to_airplay_db(0.25))),
        "{rendu}"
    );
}

/// Une zone mémorisée en sourdine : la sortie semée la repose à l'ouverture
/// de la session (#8151), sans consigne en attente.
#[tokio::test]
async fn une_zone_en_sourdine_semee_part_muette_8855() {
    let db = base();
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Cuisine 8855", Some("airplay"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 40.0).unwrap();
    repo.update_muted(zone_id, true).unwrap();

    let sortie = sortie();
    sortie.ensemencer_depuis_la_base(&db).await;
    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    drop(session);
    let requetes = recepteur.await.unwrap();
    assert_eq!(requetes.len(), 1, "{requetes:?}");
    assert!(requetes[0].contains("volume: -144.0"), "{requetes:?}");
}
