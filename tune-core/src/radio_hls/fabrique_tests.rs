//! Fabrique des témoins HLS : trames ADTS, HE-AAC, segments MPEG-TS, ID3,
//! et un faux serveur HLS sur 127.0.0.1.
//!
//! Tout est FABRIQUÉ ici, en Rust : aucun enregistrement de station n'entre
//! dans le dépôt (droits), et aucun témoin ne touche un vrai flux.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// Un en-tête ADTS de 7 octets (sans CRC) pour une trame de `longueur`
/// octets au total, objet AAC-LC.
fn entete_adts(cadence: u32, canaux: u16, longueur: usize) -> Vec<u8> {
    rusty_aac::write_adts_header(&rusty_aac::AdtsHeader {
        object_type: 2,
        sample_rate: cadence,
        channels: canaux,
        frame_length: longueur,
        header_len: 7,
    })
}

/// `n` trames ADTS de `taille` octets, dont la charge utile est remplie de
/// `marque` : indécodables, mais reconnaissables une à une. Pour les
/// témoins de TRANSPORT (le décodeur n'y passe pas).
pub(crate) fn trames_marquees(
    cadence: u32,
    canaux: u16,
    n: usize,
    taille: usize,
    marque: u8,
) -> Vec<u8> {
    let mut sortie = Vec::new();
    for _ in 0..n {
        sortie.extend(entete_adts(cadence, canaux, taille));
        sortie.extend(std::iter::repeat_n(marque, taille - 7));
    }
    sortie
}

pub(crate) fn trames_adts_taille(cadence: u32, canaux: u16, n: usize, taille: usize) -> Vec<u8> {
    trames_marquees(cadence, canaux, n, taille, 0)
}

/// `n` trames AAC-LC SILENCIEUSES, valides : un élément par canal, aucun
/// coefficient (`max_sfb = 0`), puis `ID_END`.
pub(crate) fn trames_adts_lc(cadence: u32, canaux: u16, n: usize) -> Vec<u8> {
    let mut w = rusty_aac::encode::BitWriter::new();
    let ics = |w: &mut rusty_aac::encode::BitWriter| {
        w.write(100, 8); // global_gain
        w.write(0, 1); // ics_reserved_bit
        w.write(0, 2); // ONLY_LONG_SEQUENCE
        w.write(0, 1); // window_shape
        w.write(0, 6); // max_sfb = 0
        w.write(0, 1); // predictor_data_present
        w.write(0, 3); // pulse, tns, gain_control
    };
    if canaux == 1 {
        w.write(0, 3); // ID_SCE
        w.write(0, 4);
        ics(&mut w);
    } else {
        w.write(1, 3); // ID_CPE
        w.write(0, 4);
        w.write(0, 1); // common_window
        ics(&mut w);
        ics(&mut w);
    }
    w.write(7, 3); // ID_END
    let bloc = w.into_bytes();
    let mut sortie = Vec::new();
    for _ in 0..n {
        sortie.extend(entete_adts(cadence, canaux, 7 + bloc.len()));
        sortie.extend(&bloc);
    }
    sortie
}

/// Un son pur de `hz` Hz, `secondes` secondes, mono.
fn son_pur(cadence: u32, hz: f32, secondes: f32) -> Vec<f32> {
    let n = (cadence as f32 * secondes) as usize;
    (0..n)
        .map(|i| (i as f32 * hz * std::f32::consts::TAU / cadence as f32).sin() * 0.4)
        .collect()
}

/// Les unités d'accès AAC-LC d'un son pur, par l'encodeur de `rusty_aac`.
fn encoder_lc(cadence: u32, hz: f32, secondes: f32) -> Vec<Vec<u8>> {
    let mut e = rusty_aac::AacEncoder::new(rusty_aac::AacEncoderConfig {
        bitrate_bps: 48_000,
        ..Default::default()
    });
    e.push_pcm(&son_pur(cadence, hz, secondes), 1, cadence)
        .expect("encodage AAC-LC");
    e.finish();
    let mut unites = Vec::new();
    while let Ok(p) = e.next_packet() {
        unites.push(p.data);
    }
    unites
}

/// La charge SBR (`sbr_extension_data`, ISO 14496-3 §4.4.2.8) d'un élément
/// mono, en-tête compris, pour un cœur à 24 kHz (sortie 48 kHz) :
///
/// * `bs_start_freq = 15`, `bs_stop_freq = 14` : k0 = 31, k2 = 2·k0 = 62,
///   soit la bande 11,6 – 23,2 kHz, toute AU-DESSUS de ce que le cœur à
///   24 kHz peut porter (12 kHz) ;
/// * `bs_freq_scale = 0`, `bs_alter_scale = 0` : table maîtresse linéaire de
///   30 bandes, `bs_xover_band = 0` → 30 bandes hautes, 15 basses ;
/// * `bs_noise_bands = 1` : N_Q = 1·log2(62/31) = 1 bande de bruit ;
/// * une enveloppe FIXFIX, résolution basse : une valeur de départ puis 14
///   deltas nuls (le mot `00` du livre « env 1,5 dB, fréquence »).
fn charge_sbr_mono(enveloppe: u32, bruit: u32) -> rusty_aac::encode::BitWriter {
    let mut w = rusty_aac::encode::BitWriter::new();
    w.write(13, 4); // extension_type = EXT_SBR_DATA
    w.write(1, 1); // bs_header_flag
    // sbr_header()
    w.write(0, 1); // bs_amp_res
    w.write(15, 4); // bs_start_freq
    w.write(14, 4); // bs_stop_freq
    w.write(0, 3); // bs_xover_band
    w.write(0, 2); // bs_reserved
    w.write(1, 1); // bs_header_extra_1
    w.write(0, 1); // bs_header_extra_2
    w.write(0, 2); // bs_freq_scale
    w.write(0, 1); // bs_alter_scale
    w.write(1, 2); // bs_noise_bands
    // sbr_single_channel_element()
    w.write(0, 1); // bs_data_extra
    w.write(0, 2); // sbr_grid : FIXFIX
    w.write(0, 2); // une enveloppe
    w.write(0, 1); // bs_freq_res : basse (15 bandes)
    w.write(0, 1); // bs_df_env : en fréquence
    w.write(0, 1); // bs_df_noise : en fréquence
    w.write(0, 2); // bs_invf_mode
    w.write(enveloppe, 7); // bs_env_start_value (1,5 dB, une enveloppe)
    for _ in 1..15 {
        w.write(0b00, 2); // delta 0
    }
    w.write(bruit, 5); // bs_noise_start_value
    w.write(0, 1); // bs_add_harmonic_flag
    w.write(0, 1); // bs_extended_data
    w
}

/// Retire `ID_END` et le bourrage de la fin d'une unité d'accès : rend le
/// nombre de bits utiles. Le bourrage est nul et `ID_END` vaut `111` : le
/// dernier bit à 1 est celui qui clôt `ID_END`.
fn bits_avant_id_end(au: &[u8]) -> usize {
    let dernier = au
        .iter()
        .rposition(|&b| b != 0)
        .expect("unité d'accès vide");
    let bit = 7 - au[dernier].trailing_zeros() as usize;
    dernier * 8 + bit + 1 - 3
}

/// Des trames ADTS HE-AAC (SBR implicite) : l'en-tête ADTS dit AAC-LC à
/// 24 kHz mono, comme la BBC hors Royaume-Uni (étude #6363 §3.2), et chaque
/// trame porte un `ID_FIL` / `EXT_SBR_DATA` juste avant `ID_END`.
///
/// Le cœur est un son pur à 6 kHz : il tombe dans la zone que le SBR
/// recopie vers le haut (bande QMF 16, recopiée en 48). Un décodeur qui fait
/// le SBR sort du 48 kHz avec de l'énergie au-dessus de 12 kHz ; un
/// décodeur AAC-LC seul sort du 24 kHz.
pub(crate) fn trames_he_aac(secondes: f32) -> Vec<u8> {
    let charge = charge_sbr_mono(60, 20);
    let bits_charge = charge.bit_len();
    let octets_charge = charge.into_bytes();
    let cnt = bits_charge.div_ceil(8);
    let mut sortie = Vec::new();
    for au in encoder_lc(24_000, 6_000.0, secondes) {
        let utiles = bits_avant_id_end(&au);
        let mut w = rusty_aac::encode::BitWriter::new();
        for i in 0..utiles {
            w.write(u32::from((au[i / 8] >> (7 - i % 8)) & 1), 1);
        }
        w.write(6, 3); // ID_FIL
        if cnt >= 15 {
            w.write(15, 4);
            w.write((cnt - 14) as u32, 8);
        } else {
            w.write(cnt as u32, 4);
        }
        for octet in &octets_charge {
            w.write(u32::from(*octet), 8);
        }
        w.write(7, 3); // ID_END
        let bloc = w.into_bytes();
        sortie.extend(entete_adts(24_000, 1, 7 + bloc.len()));
        sortie.extend(bloc);
    }
    sortie
}

/// Un PID de PMT et un PID audio, comme un codeur réel.
const PID_PMT: u16 = 0x1000;
pub(crate) const PID_AUDIO: u16 = 0x0100;

fn paquet_ts(pid: u16, debut: bool, cc: u8, charge: &[u8]) -> Vec<u8> {
    assert!(charge.len() <= 184);
    let mut p = vec![
        0x47,
        (if debut { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F),
        pid as u8,
        0,
    ];
    if charge.len() == 184 {
        p[3] = 0x10 | (cc & 0x0F);
    } else {
        // Champ d'adaptation de bourrage pour compléter les 188 octets.
        p[3] = 0x30 | (cc & 0x0F);
        let longueur = 183 - charge.len();
        p.push(longueur as u8);
        if longueur > 0 {
            p.push(0);
            p.extend(std::iter::repeat_n(0xFF, longueur - 1));
        }
    }
    p.extend_from_slice(charge);
    assert_eq!(p.len(), 188);
    p
}

fn section(table: u8, corps: &[u8]) -> Vec<u8> {
    let longueur = corps.len() + 5 + 4;
    let mut s = vec![0u8, table, 0xB0 | ((longueur >> 8) as u8), longueur as u8];
    s.extend([0, 1, 0xC1, 0, 0]);
    s.extend_from_slice(corps);
    s.extend([0, 0, 0, 0]); // CRC (non vérifié par le démultiplexeur)
    s
}

/// Un segment MPEG-TS : PAT, PMT (un flux de type `type_flux` sur
/// [`PID_AUDIO`]), puis le flux élémentaire en PES de 4 096 octets au plus.
/// `cc` : compteur de continuité de départ du PID audio.
pub(crate) fn segment_ts_type(elementaire: &[u8], cc: u8, type_flux: u8) -> Vec<u8> {
    let mut ts = Vec::new();
    let pat = section(0x00, &[0, 1, 0xE0 | (PID_PMT >> 8) as u8, PID_PMT as u8]);
    ts.extend(paquet_ts(0, true, 0, &pat));
    let pmt = section(
        0x02,
        &[
            0xE0 | (PID_AUDIO >> 8) as u8,
            PID_AUDIO as u8,
            0xF0,
            0,
            type_flux,
            0xE0 | (PID_AUDIO >> 8) as u8,
            PID_AUDIO as u8,
            0xF0,
            0,
        ],
    );
    ts.extend(paquet_ts(PID_PMT, true, 0, &pmt));
    let mut cc = cc;
    for morceau in elementaire.chunks(4096) {
        let mut pes = vec![0, 0, 1, 0xC0];
        let longueur = morceau.len() + 3;
        pes.extend([(longueur >> 8) as u8, longueur as u8, 0x80, 0, 0]);
        pes.extend_from_slice(morceau);
        for (i, bout) in pes.chunks(184).enumerate() {
            ts.extend(paquet_ts(PID_AUDIO, i == 0, cc, bout));
            cc = (cc + 1) & 0x0F;
        }
    }
    ts
}

/// [`segment_ts_type`] en ADTS (type `0x0F`). `_pat` documente l'usage :
/// tout segment HLS commence par son PAT et son PMT.
pub(crate) fn segment_ts(elementaire: &[u8], cc: u8, _pat: bool) -> Vec<u8> {
    segment_ts_type(elementaire, cc, 0x0F)
}

/// Une étiquette ID3v2.4, `TIT2` et `TPE1` en UTF-8.
pub(crate) fn etiquette_id3(titre: &str, artiste: Option<&str>) -> Vec<u8> {
    let trame = |id: &[u8; 4], texte: &str| {
        let mut t = id.to_vec();
        let n = texte.len() + 1;
        t.extend([
            (n >> 21) as u8 & 0x7F,
            (n >> 14) as u8 & 0x7F,
            (n >> 7) as u8 & 0x7F,
            n as u8 & 0x7F,
        ]);
        t.extend([0, 0, 3]);
        t.extend(texte.as_bytes());
        t
    };
    let mut corps = trame(b"TIT2", titre);
    if let Some(a) = artiste {
        corps.extend(trame(b"TPE1", a));
    }
    let n = corps.len();
    let mut e = b"ID3".to_vec();
    e.extend([4, 0, 0]);
    e.extend([
        (n >> 21) as u8 & 0x7F,
        (n >> 14) as u8 & 0x7F,
        (n >> 7) as u8 & 0x7F,
        n as u8 & 0x7F,
    ]);
    e.extend(corps);
    e
}

/// Une réponse du faux serveur : statut, type, corps.
pub(crate) type Reponse = (u16, &'static str, Vec<u8>);

/// Ce que le faux serveur répond à un chemin, sachant combien de fois CE
/// chemin a déjà été demandé (0 au premier GET).
pub(crate) type Routeur = dyn Fn(&str, usize) -> Reponse + Send + Sync;

/// Un faux serveur HLS sur 127.0.0.1, port éphémère.
pub(crate) struct ServeurHls {
    pub(crate) base: String,
    demandes: Arc<Mutex<HashMap<String, usize>>>,
}

impl ServeurHls {
    pub(crate) fn lancer(routeur: Arc<Routeur>) -> Self {
        let ecoute = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", ecoute.local_addr().expect("addr"));
        let demandes: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let d = demandes.clone();
        std::thread::spawn(move || {
            for flux in ecoute.incoming().flatten() {
                let (routeur, d) = (routeur.clone(), d.clone());
                std::thread::spawn(move || servir(flux, &*routeur, &d));
            }
        });
        Self { base, demandes }
    }

    /// Combien de fois `chemin` a été demandé.
    pub(crate) fn demandes(&self, chemin: &str) -> usize {
        self.demandes
            .lock()
            .unwrap()
            .get(chemin)
            .copied()
            .unwrap_or(0)
    }
}

fn servir(
    mut flux: std::net::TcpStream,
    routeur: &Routeur,
    demandes: &Mutex<HashMap<String, usize>>,
) {
    let mut requete = Vec::new();
    let mut tampon = [0u8; 1024];
    while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
        match flux.read(&mut tampon) {
            Ok(0) | Err(_) => return,
            Ok(n) => requete.extend_from_slice(&tampon[..n]),
        }
    }
    let texte = String::from_utf8_lossy(&requete);
    let chemin = texte.split_whitespace().nth(1).unwrap_or("/").to_string();
    let rang = {
        let mut d = demandes.lock().unwrap();
        let n = d.entry(chemin.clone()).or_insert(0);
        *n += 1;
        *n - 1
    };
    let (statut, type_mime, corps) = routeur(&chemin, rang);
    let _ = write!(
        flux,
        "HTTP/1.1 {statut} X\r\nContent-Type: {type_mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        corps.len()
    );
    let _ = flux.write_all(&corps);
    let _ = flux.flush();
}

/// Une liste média : `sequence` du premier segment, segments `(chemin,
/// titre EXTINF, discontinuité)`.
pub(crate) fn liste_media(
    cible: u32,
    sequence: u64,
    segments: &[(String, String, bool)],
    fin: bool,
) -> Vec<u8> {
    let mut l = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{cible}\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n"
    );
    for (chemin, titre, discontinuite) in segments {
        if *discontinuite {
            l.push_str("#EXT-X-DISCONTINUITY\n");
        }
        l.push_str(&format!("#EXTINF:{cible}.0,{titre}\n{chemin}\n"));
    }
    if fin {
        l.push_str("#EXT-X-ENDLIST\n");
    }
    l.into_bytes()
}

pub(crate) const TYPE_LISTE: &str = "application/vnd.apple.mpegurl";
pub(crate) const TYPE_TS: &str = "video/MP2T";

pub(crate) fn ok(type_mime: &'static str, corps: Vec<u8>) -> Reponse {
    (200, type_mime, corps)
}

pub(crate) fn introuvable() -> Reponse {
    (404, "text/plain", b"not found".to_vec())
}
