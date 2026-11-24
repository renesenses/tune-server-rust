//! TESS T12 — un faux serveur `/stream` pour les bancs de bout en bout.
//!
//! Le vrai serveur HTTP des flux vit dans `tune-server` : dans `tune-core`, une
//! sortie RÉELLE (OAAT, AirPlay 1, Devialet AIR) qui va chercher l'URL que
//! l'orchestrateur lui remet ne trouverait personne. Ce banc lie un port sur
//! la boucle locale, crée l'[`AudioStreamer`] qui publie ses URL sur ce port,
//! et sert à chaque `GET /stream/{id}.{ext}` les octets de la session : le
//! fichier d'une session de fichier, ou tout ce que le canal d'une session
//! progressive produit (relais de la chaîne compris).
//!
//! Un canal ne se lit qu'une fois : il est lu en entier à la première
//! requête, puis la même copie est servie à toutes les connexions (une sortie
//! qui sonde avant de lire reçoit le même corps). Pas de `Range`, réponse
//! `200` complète : le banc ne mesure que les OCTETS, jamais le transport.
//! Aucun son, rien hors de `127.0.0.1`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::http::streamer::AudioStreamer;

type Copies = Arc<Mutex<HashMap<String, Arc<(String, Vec<u8>)>>>>;

/// Démarre le faux serveur ; rend le streamer à donner à l'orchestrateur
/// (avec `advertised_ip = 127.0.0.1`).
pub(crate) async fn demarrer() -> Arc<AudioStreamer> {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    let streamer = Arc::new(AudioStreamer::new(port));
    let copies: Copies = Arc::new(Mutex::new(HashMap::new()));
    let s = streamer.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = ecoute.accept().await {
            tokio::spawn(servir(sock, s.clone(), copies.clone()));
        }
    });
    streamer
}

async fn servir(mut sock: tokio::net::TcpStream, streamer: Arc<AudioStreamer>, copies: Copies) {
    let mut requete = Vec::new();
    let mut octet = [0u8; 1];
    while !requete.ends_with(b"\r\n\r\n") {
        if sock.read(&mut octet).await.unwrap_or(0) == 0 {
            return;
        }
        requete.push(octet[0]);
    }
    let texte = String::from_utf8_lossy(&requete).into_owned();
    let mut premiere = texte.lines().next().unwrap_or("").split_whitespace();
    let methode = premiere.next().unwrap_or("").to_owned();
    let chemin = premiere.next().unwrap_or("");
    let id = chemin
        .trim_start_matches("/stream/")
        .split(['.', '?'])
        .next()
        .unwrap_or("")
        .to_owned();
    let Some(copie) = octets_de_la_session(&streamer, &id, &copies).await else {
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let (mime, corps) = (&copie.0, &copie.1);
    let entetes = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        corps.len()
    );
    if sock.write_all(entetes.as_bytes()).await.is_err() {
        return;
    }
    if methode != "HEAD" {
        let _ = sock.write_all(corps).await;
    }
    let _ = sock.shutdown().await;
}

/// Les octets de la session `id` (lus une fois, puis gardés).
async fn octets_de_la_session(
    streamer: &AudioStreamer,
    id: &str,
    copies: &Copies,
) -> Option<Arc<(String, Vec<u8>)>> {
    let mut copies = copies.lock().await;
    if let Some(c) = copies.get(id) {
        return Some(c.clone());
    }
    let session = streamer.sessions_state().lock().await.get(id).cloned()?;
    let fichier = session.file_path.lock().await.clone();
    let octets = match fichier {
        Some(f) => tokio::fs::read(f).await.ok()?,
        None => {
            // Fin du flux : le canal se ferme, ou plus rien n'arrive pendant
            // 3 s une fois des octets reçus (un relais peut garder son
            // émetteur ouvert) ; 20 s au plus pour le premier.
            let mut octets = Vec::new();
            loop {
                let attente = if octets.is_empty() {
                    Duration::from_secs(20)
                } else {
                    Duration::from_secs(3)
                };
                match tokio::time::timeout(attente, session.recv_chunk()).await {
                    Ok(Some(c)) => octets.extend_from_slice(&c),
                    _ => break,
                }
            }
            octets
        }
    };
    let copie = Arc::new((session.info.mime_type.clone(), octets));
    copies.insert(id.to_owned(), copie.clone());
    Some(copie)
}

/// Un WAV PCM 16 bits stéréo de `trames` trames à `cadence` Hz : un sinus de
/// 1 kHz à −6 dBFS sur la voie GAUCHE, la voie DROITE muette. Rend le fichier
/// et les échantillons `[gauche, droite]`.
///
/// La voie droite muette est la sonde : tout étage qui croise les voies
/// (crossfeed, greffon de la famille) l'allume, et un chemin bit-perfect la
/// rend nulle à l'échantillon près.
pub(crate) fn wav_gauche_seule(
    etiquette: &str,
    cadence: u32,
    trames: usize,
) -> (crate::test_scratch::ScratchFile, Vec<[i16; 2]>) {
    let f = crate::test_scratch::scratch_file(etiquette, ".wav");
    let echantillons: Vec<[i16; 2]> = (0..trames)
        .map(|i| {
            let t = i as f64 / f64::from(cadence);
            let g = (16_384.0 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()).round() as i16;
            [g, 0]
        })
        .collect();
    let mut donnees = Vec::with_capacity(trames * 4);
    for [g, d] in &echantillons {
        donnees.extend_from_slice(&g.to_le_bytes());
        donnees.extend_from_slice(&d.to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + donnees.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + donnees.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&cadence.to_le_bytes());
    wav.extend_from_slice(&(cadence * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
    wav.extend_from_slice(&donnees);
    std::fs::write(f.path(), wav).unwrap();
    (f, echantillons)
}

/// Inscrit la piste `fichier` (WAV 16 bits stéréo) en bibliothèque ; rend son
/// identifiant.
pub(crate) fn inscrire_la_piste(
    db: &Arc<dyn crate::db::backend::DbBackend>,
    fichier: &str,
    cadence: u32,
    duree_ms: i64,
) -> i64 {
    let mut t = crate::db::models::Track::new("Piste du banc TESS T12".into());
    t.duration_ms = duree_ms;
    t.file_path = Some(fichier.into());
    t.format = Some("wav".into());
    t.sample_rate = Some(cadence as i32);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(fichier).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .create(&t)
        .unwrap()
}

/// Le crossfeed intégré, coché sur la zone (−∞ → voie droite allumée).
pub(crate) fn armer_le_crossfeed(orch: &super::PlaybackOrchestrator, zone_id: i64) {
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_crossfeed"),
            r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
        )
        .unwrap();
}

/// Le mode Pure de la zone.
pub(crate) fn armer_pure(orch: &super::PlaybackOrchestrator, zone_id: i64) {
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
}

/// Ce que montre une capture de `[gauche, droite]` (entiers à la profondeur
/// reçue) face à la source 16 bits.
#[derive(Debug)]
pub(crate) struct Constat {
    pub trames: usize,
    /// |droite| maximale reçue, ramenée à l'échelle 16 bits.
    pub droite_max: i64,
    /// La gauche reçue (ramenée à 16 bits) est-elle la source à l'échantillon
    /// près, et la droite nulle, sur toutes les trames comparables ?
    pub identique: bool,
    /// Écart maximal de la gauche à `facteur × source` (16 bits).
    pub ecart_gauche: f64,
}

/// Compare une capture `recu` (entiers sur `bits` bits) à la `source`,
/// à partir de la première trame non nulle de chacune (le transport peut
/// faire précéder la piste de silence), sur `source.len()` trames au plus.
pub(crate) fn constater(
    recu: &[[i32; 2]],
    bits: u32,
    source: &[[i16; 2]],
    facteur: f64,
) -> Constat {
    let decalage = bits.saturating_sub(16);
    let ramener = |v: i32| -> i64 { i64::from(v) >> decalage };
    let debut_recu = recu.iter().position(|f| f[0] != 0).unwrap_or(recu.len());
    let debut_source = source.iter().position(|f| f[0] != 0).unwrap_or(0);
    let recu = &recu[debut_recu..];
    let source = &source[debut_source..];
    let n = recu.len().min(source.len());
    let mut identique = n > source.len() / 2;
    let mut droite_max = 0i64;
    let mut ecart_gauche = 0f64;
    for (r, s) in recu.iter().zip(source.iter()).take(n) {
        let (g, d) = (ramener(r[0]), ramener(r[1]));
        droite_max = droite_max.max(d.abs());
        ecart_gauche = ecart_gauche.max((g as f64 - facteur * f64::from(s[0])).abs());
        if g != i64::from(s[0]) || d != 0 {
            identique = false;
        }
    }
    // La droite au-delà de la fenêtre compte aussi (queue d'un étage).
    for r in recu.iter().skip(n) {
        droite_max = droite_max.max(ramener(r[1]).abs());
    }
    Constat {
        trames: n,
        droite_max,
        identique,
        ecart_gauche,
    }
}
