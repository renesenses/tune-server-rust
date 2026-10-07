//! #5439 — un FLAC servi tel quel par un serveur multimédia se décode au fil
//! de l'eau et sonne pendant le téléchargement, comme un fichier local.
//!
//! Le témoin joue ce que `play_url` fait d'un flux non-WAV : le lecteur HTTP
//! de production (`LecteurHttpAnnulable`) lit le premier bloc, le reconnaît,
//! passe en décodage continu, lit l'en-tête WAV du flux décodé, puis la boucle
//! producteur du chemin PCM (`BoucleProducteur::tourner`, `EtageDeConversion`)
//! pousse vers un `CaptureOutput`. Le serveur multimédia factice sert
//! LENTEMENT : la piste entière met [`TRANSFERT_COMPLET_MS`] à arriver.

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::decodage_en_continu::{
    EnteteDecodee, extension_decodable_en_continu, lire_l_entete_decodee,
};
use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::*;
use crate::outputs::traits::{CaptureOutput, FormatOuvert};

pub(super) const CADENCE: u32 = 48_000;
pub(super) const CANAUX: u16 = 2;
const DUREE_S: u32 = 8;
/// Le transfert complet de la piste, imposé par le serveur factice.
const TRANSFERT_COMPLET_MS: u64 = 4_000;
/// Le pré-remplissage du chemin PCM avant `stream.play()` : ~500 ms d'audio
/// (`min_buf_ms` de `play_url`). C'est lui qui fixe le premier son.
const PRE_REMPLISSAGE_MS: u64 = 500;
/// Premier son exigé en moins de 1 500 ms.
///
/// L'ancienne branche ne peut RIEN jouer avant la fin du transfert
/// ([`TRANSFERT_COMPLET_MS`], 4 s par construction) : elle lit tout le corps
/// avant de décoder. En continu, 500 ms d'audio sur 8 s arrivent après
/// ~1/16 du transfert (~250 ms), plus la sonde du conteneur. 1 500 ms laisse
/// six fois cette marge à un Shrek chargé tout en restant sous 40 % du
/// transfert complet : les deux comportements ne peuvent pas se confondre.
const PREMIER_SON_MAX_MS: u64 = 1_500;

/// Un FLAC 48 kHz / 16 bits stéréo de [`DUREE_S`] s, fabriqué par l'encodeur
/// du dépôt : deux sinus et un bruit déterministe, pour que chaque trame
/// compte.
fn flac_48k() -> Vec<u8> {
    let trames = (CADENCE * DUREE_S) as usize;
    let mut pcm = Vec::with_capacity(trames * CANAUX as usize * 2);
    let mut graine: u32 = 0x5439;
    for i in 0..trames {
        let t = i as f64 / CADENCE as f64;
        for voie in 0..CANAUX {
            graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bruit = ((graine >> 20) as f64 / 4096.0 - 0.5) * 0.02;
            let f = if voie == 0 { 440.0 } else { 660.0 };
            let v = (2.0 * std::f64::consts::PI * f * t).sin() * 0.5 + bruit;
            pcm.extend_from_slice(&((v * i16::MAX as f64) as i16).to_le_bytes());
        }
    }
    let mut encodeur = crate::audio::encoder::AudioEncoder::new("flac", CADENCE, 16, CANAUX as u32);
    encodeur.start_sync().expect("début FLAC");
    encodeur.write_sync(&pcm).expect("écriture FLAC");
    encodeur.finish_sync().expect("fin FLAC")
}

/// Un serveur multimédia factice qui sert `corps` en [`TRANSFERT_COMPLET_MS`].
pub(super) struct ServeurLent {
    pub(super) url: String,
    fil: Option<JoinHandle<()>>,
}

impl ServeurLent {
    pub(super) fn servir(corps: Arc<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/files/Freebox/Musiques/Io%20Capitano.flac",
            listener.local_addr().unwrap()
        );
        let fil = std::thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut requete = Vec::new();
            let mut bloc = [0u8; 1024];
            while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut bloc) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => requete.extend_from_slice(&bloc[..n]),
                }
            }
            let entete = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\n\
                 transferMode.dlna.org: Streaming\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                corps.len()
            );
            if socket.write_all(entete.as_bytes()).is_err() {
                return;
            }
            const MORCEAU: usize = 4096;
            let morceaux = corps.len().div_ceil(MORCEAU) as u64;
            let pas = Duration::from_micros(TRANSFERT_COMPLET_MS * 1000 / morceaux.max(1));
            for morceau in corps.chunks(MORCEAU) {
                if socket.write_all(morceau).is_err() {
                    return;
                }
                std::thread::sleep(pas);
            }
        });
        Self {
            url,
            fil: Some(fil),
        }
    }
}

impl Drop for ServeurLent {
    fn drop(&mut self) {
        if let Some(fil) = self.fil.take() {
            let _ = fil.join();
        }
    }
}

/// Ce que `play_url` fait d'un flux non-WAV, jusqu'à l'en-tête décodé.
pub(super) fn ouvrir_comme_play_url(
    url: &str,
    arret: &Arc<AtomicBool>,
) -> (LecteurHttpAnnulable, Vec<u8>, (u16, u32, u16, usize)) {
    let mut lecteur = LecteurHttpAnnulable::ouvrir(url, arret.clone()).expect("ouverture HTTP");
    let mut entete = vec![0u8; 4096];
    let lus = loop {
        match lecteur.read(&mut entete) {
            Ok(n) => break n,
            Err(ref e) if header_read_should_retry(e.kind()) => continue,
            Err(e) => panic!("lecture du premier bloc : {e}"),
        }
    };
    entete.truncate(lus);
    assert!(
        parse_wav_header(&entete).is_none(),
        "un FLAC n'est pas un WAV"
    );
    let extension = extension_decodable_en_continu(&entete).expect("un FLAC se décode en continu");
    assert_eq!(extension, "flac");
    let mut lecteur = lecteur.decoder_en_continu(entete, extension);
    match lire_l_entete_decodee(&mut lecteur, arret) {
        EnteteDecodee::Pret { octets, format } => (lecteur, octets, format),
        EnteteDecodee::Interrompu => panic!("en-tête interrompu"),
        EnteteDecodee::Echec => panic!("décodage : {:?}", lecteur.echec_du_decodage()),
    }
}

struct Jeu {
    puits: CaptureOutput,
    premier_son_ms: Option<u64>,
    fin: FinDeBoucle,
}

/// La boucle producteur du chemin PCM, de `amont` au puits.
fn jouer(
    amont: &mut dyn Read,
    amorce: Vec<u8>,
    sortie_sr: u32,
    skip_bytes: u64,
    seek_offset: u64,
    arret: &AtomicBool,
    debut: Instant,
) -> Jeu {
    let disparu = AtomicBool::new(false);
    let position = AtomicU64::new(0);
    let erreur = std::sync::Mutex::new(None);
    let (_tx, rx) = mpsc::channel();
    let producteur = BoucleProducteur {
        role: RoleDeLaBoucle::PisteInitiale,
        backend: "capture",
        device_name: "5439",
        cle_de_flux: None,
        stop_rx: &rx,
        force_silent: arret,
        device_gone: &disparu,
        position_ms: &position,
        open_failure: &erreur,
        debut_du_flux: debut,
        duree_de_la_piste_ms: &DUREE_DE_PISTE_INCONNUE,
        cretes_de_sortie: None,
    };
    let dsp = DspAuRepos::neuf();
    let mut conversion = etage(&dsp, amorce, CADENCE, CANAUX, 32, sortie_sr, CANAUX);
    if sortie_sr != CADENCE {
        conversion.resampler = Some(
            crate::audio::resample::new_streaming_resampler(CADENCE, sortie_sr, CANAUX)
                .expect("rééchantillonneur en flux"),
        );
    }
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(sortie_sr, CANAUX));
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset,
        skip_bytes,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    let seuil = CADENCE as u64 * PRE_REMPLISSAGE_MS / 1000;
    let mut premier_son_ms = None;
    let mut tampon = vec![0u8; 16_384];
    let fin = producteur.tourner(
        amont,
        &mut tampon,
        &mut conversion,
        &mut puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |c| {
            if premier_son_ms.is_none() && c.total_frames_fed >= seuil {
                premier_son_ms = Some(debut.elapsed().as_millis() as u64);
            }
            true
        },
    );
    Jeu {
        puits,
        premier_son_ms,
        fin,
    }
}

/// Le décodeur des FICHIERS LOCAUX sur le même FLAC posé sur disque : la
/// référence octet pour octet (en-tête WAV compris).
pub(super) fn reference_fichier_local(flac: &[u8]) -> Vec<u8> {
    let mut fichier = tempfile::Builder::new()
        .suffix(".flac")
        .tempfile()
        .expect("fichier temporaire");
    fichier.write_all(flac).unwrap();
    let chemin = fichier.path().to_string_lossy().into_owned();
    let moteur = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
    let decodeur = std::thread::spawn(move || {
        let moteur = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _contexte = moteur.enter();
        let (niveaux, _) = tokio::sync::mpsc::unbounded_channel();
        crate::audio::decode::decode_to_pcm_streaming_seeked(
            &chemin,
            None,
            None,
            Some(32),
            tx,
            32_768,
            Arc::new(tokio::sync::Notify::new()),
            niveaux,
            0.0,
        )
        .expect("décodage du fichier local");
    });
    let mut octets = Vec::new();
    while let Some(bloc) = moteur.block_on(rx.recv()) {
        octets.extend_from_slice(&bloc);
    }
    decodeur.join().unwrap();
    drop(fichier);
    octets
}

#[test]
fn chemin_compresse_5439_le_premier_son_n_attend_pas_la_fin_du_transfert() {
    let flac = Arc::new(flac_48k());
    for sortie_sr in [CADENCE, 44_100] {
        let serveur = ServeurLent::servir(flac.clone());
        let arret = Arc::new(AtomicBool::new(false));
        let debut = Instant::now();
        let (mut lecteur, octets, format) = ouvrir_comme_play_url(&serveur.url, &arret);
        assert_eq!((format.0, format.1, format.2), (CANAUX, CADENCE, 32));
        let amorce = octets[format.3..].to_vec();
        let jeu = jouer(&mut lecteur, amorce, sortie_sr, 0, 0, &arret, debut);
        let fin_ms = debut.elapsed().as_millis() as u64;
        let premier_son_ms = jeu.premier_son_ms.expect("500 ms d'audio ont été poussés");

        eprintln!(
            "5439 sortie {sortie_sr} Hz : premier son (pré-remplissage) à {premier_son_ms} ms, \
             fin du flux à {fin_ms} ms"
        );
        assert!(matches!(jeu.fin, FinDeBoucle::FinDeFlux));
        assert!(
            premier_son_ms < PREMIER_SON_MAX_MS,
            "sortie {sortie_sr} Hz : les {PRE_REMPLISSAGE_MS} ms du pré-remplissage ne sont \
             prêts qu'après {premier_son_ms} ms, sur un transfert de {TRANSFERT_COMPLET_MS} ms \
             (fin à {fin_ms} ms) — le flux compressé attend encore la fin du téléchargement \
             avant le premier son (#5439)"
        );
        assert!(
            fin_ms >= TRANSFERT_COMPLET_MS * 9 / 10,
            "le serveur a bien servi lentement ({fin_ms} ms)"
        );
        let attendu = if sortie_sr == CADENCE {
            (CADENCE * DUREE_S) as u64
        } else {
            (44_100 * DUREE_S) as u64
        };
        // Le rééchantillonneur en flux garde son délai de groupe (1 % de marge).
        let ecart = jeu.puits.trames().abs_diff(attendu);
        assert!(
            ecart <= attendu / 100,
            "sortie {sortie_sr} Hz : {} trames pour {attendu} attendues",
            jeu.puits.trames()
        );
    }
}

#[test]
fn chemin_compresse_5439_le_flux_decode_est_celui_d_un_fichier_local_a_l_octet_pres() {
    let flac = Arc::new(flac_48k());
    let reference = reference_fichier_local(&flac);
    let serveur = ServeurLent::servir(flac.clone());
    let arret = Arc::new(AtomicBool::new(false));
    let (mut lecteur, mut octets, _) = ouvrir_comme_play_url(&serveur.url, &arret);
    lecteur
        .read_to_end(&mut octets)
        .expect("lecture du flux décodé");
    assert_eq!(
        octets.len(),
        reference.len(),
        "même longueur, en-tête compris"
    );
    assert!(
        octets == reference,
        "le flux décodé du serveur multimédia diffère du décodage du même fichier local"
    );
    assert_eq!(
        (octets.len() - 44) as u64,
        (CADENCE * DUREE_S) as u64 * CANAUX as u64 * 4,
        "toute la piste, en 32 bits"
    );
}

/// Seek : le flux décodé repart du début de la piste, `play_url` baisse
/// `pre_seeked` et le chemin PCM saute les octets — comme un WAV non
/// pré-positionné. Le puits reçoit exactement la suite du fichier local.
#[test]
fn chemin_compresse_5439_seek_par_saut_d_octets_identique_au_fichier_local() {
    let flac = Arc::new(flac_48k());
    let reference = reference_fichier_local(&flac);
    let seek_offset: u64 = 3_000;
    // La formule de `play_url` (`skip_bytes`), 32 bits = 4 octets.
    let skip_frames = (seek_offset as f64 / 1000.0 * CADENCE as f64) as u64;
    let skip_bytes = skip_frames * CANAUX as u64 * 4;

    let serveur = ServeurLent::servir(flac.clone());
    let arret = Arc::new(AtomicBool::new(false));
    let debut = Instant::now();
    let (mut lecteur, octets, format) = ouvrir_comme_play_url(&serveur.url, &arret);
    let jeu = jouer(
        &mut lecteur,
        octets[format.3..].to_vec(),
        CADENCE,
        skip_bytes,
        seek_offset,
        &arret,
        debut,
    );

    let arret_ref = AtomicBool::new(false);
    let mut amont_ref = Cursor::new(reference[44..].to_vec());
    let jeu_ref = jouer(
        &mut amont_ref,
        Vec::new(),
        CADENCE,
        skip_bytes,
        seek_offset,
        &arret_ref,
        Instant::now(),
    );
    assert_eq!(
        jeu.puits.trames(),
        (CADENCE * DUREE_S) as u64 - skip_frames,
        "la piste à partir de 3 s"
    );
    assert_eq!(jeu.puits.trames(), jeu_ref.puits.trames());
    assert_eq!(
        jeu.puits.empreinte(),
        jeu_ref.puits.empreinte(),
        "seek à 3 s : le puits ne reçoit pas la même suite que pour le fichier local"
    );
}

/// Arrêt en plein transfert : la lecture rend `Interrompue`, et détruire le
/// lecteur rejoint le fil de décodage sans attendre la fin du serveur.
#[test]
fn chemin_compresse_5439_l_arret_interrompt_et_rejoint_le_decodeur() {
    let flac = Arc::new(flac_48k());
    let serveur = ServeurLent::servir(flac.clone());
    let arret = Arc::new(AtomicBool::new(false));
    let (mut lecteur, _, _) = ouvrir_comme_play_url(&serveur.url, &arret);
    let mut bloc = [0u8; 4096];
    let lus = lecteur.read(&mut bloc).expect("du PCM arrive");
    assert!(lus > 0, "du PCM arrive avant l'arrêt");
    arret.store(true, std::sync::atomic::Ordering::SeqCst);
    let erreur = lecteur
        .read(&mut bloc)
        .expect_err("l'arrêt interrompt la lecture");
    assert_eq!(erreur.kind(), std::io::ErrorKind::ConnectionAborted);
    let depuis = Instant::now();
    drop(lecteur);
    let ms = depuis.elapsed().as_millis() as u64;
    assert!(
        ms < 1_000,
        "le fil de décodage a mis {ms} ms à rendre la main après l'arrêt"
    );
}

/// Les formats hors du décodage continu gardent la branche d'avant.
#[test]
fn chemin_compresse_5439_formats_decodables_en_continu() {
    assert_eq!(
        extension_decodable_en_continu(b"fLaC\0\0\0\x22"),
        Some("flac")
    );
    assert_eq!(extension_decodable_en_continu(b"ID3\x04\0"), Some("mp3"));
    assert_eq!(
        extension_decodable_en_continu(&[0xFF, 0xFB, 0x90, 0x64]),
        Some("mp3")
    );
    // ADTS (AAC) : couche 00.
    assert_eq!(
        extension_decodable_en_continu(&[0xFF, 0xF1, 0x50, 0x80]),
        None
    );
    assert_eq!(extension_decodable_en_continu(b"\0\0\0\x20ftypM4A "), None);
    assert_eq!(extension_decodable_en_continu(b"OggS\0\x02"), None);
    assert_eq!(extension_decodable_en_continu(b""), None);
}

/// Garde de site : `play_url` branche le décodage continu AVANT la branche
/// compressée d'avant, et baisse `pre_seeked` pour le flux décodé.
#[test]
fn chemin_compresse_5439_play_url_branche_le_decodage_continu() {
    const TOUT: &str = include_str!("../local.rs");
    let fin = TOUT
        .find("mod relache_peripherique_i3575")
        .expect("module d'épreuves renommé : la découpe ne protège plus rien");
    let production: String = TOUT[..fin].chars().filter(|c| !c.is_whitespace()).collect();
    let branchement = [
        "reader=reader.decoder_en",
        "_continu(std::mem::take(&mutheader_buf),extension);",
    ]
    .concat();
    let saut = [branchement.as_str(), "pre_seeked", "=false;"].concat();
    assert_eq!(
        production.matches(&saut).count(),
        1,
        "`play_url` ne décode plus en continu un flux compressé, ou ne baisse plus \
         `pre_seeked` (le seek sauterait alors à tort au début de la piste)"
    );
    let pos_continu = production.find(&branchement).unwrap();
    let pos_ancienne = production
        .find("decode_compressed_stream(&all_data,&force_silent)")
        .expect("la branche d'avant reste pour les autres formats");
    assert!(
        pos_continu < pos_ancienne,
        "le décodage continu doit être essayé avant la branche qui charge tout"
    );
}
