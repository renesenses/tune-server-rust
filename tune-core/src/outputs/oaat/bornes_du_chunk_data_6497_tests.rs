//! #6497 (fil 2332) — un WAV lu en entier par la lecture directe OAAT ne doit
//! envoyer que les échantillons de son chunk `data`.
//!
//! Le signalement : du bruit fort qui commence à la fin de chaque piste WAV,
//! au moment de l'enchaînement, jamais en FLAC. Le journal joint montre, sur
//! chacune des deux pistes, environ 62 Ko de plus que la durée de la piste :
//! le chunk d'étiquettes rangé après `data`, joué comme des échantillons.

use super::*;

const MOTIF_QUEUE: u8 = 0xEE;

/// Échantillons reconnaissables : l'octet `i` vaut `0x10 + i % 64`, jamais
/// `MOTIF_QUEUE`.
fn echantillons(n: usize) -> Vec<u8> {
    (0..n).map(|i| 0x10 + (i % 64) as u8).collect()
}

/// Un chunk RIFF, octet de bourrage compris quand le corps est impair.
fn chunk(id: &[u8; 4], corps: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&(corps.len() as u32).to_le_bytes());
    v.extend_from_slice(corps);
    if !corps.len().is_multiple_of(2) {
        v.push(0);
    }
    v
}

/// Un chunk `data` dont la taille DÉCLARÉE est imposée.
fn chunk_data_declare(taille: u32, corps: &[u8]) -> Vec<u8> {
    let mut v = b"data".to_vec();
    v.extend_from_slice(&taille.to_le_bytes());
    v.extend_from_slice(corps);
    if !corps.len().is_multiple_of(2) {
        v.push(0);
    }
    v
}

fn fmt_pcm(canaux: u16, frequence: u32, bits: u16) -> Vec<u8> {
    let alignement = canaux * bits / 8;
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&canaux.to_le_bytes());
    f.extend_from_slice(&frequence.to_le_bytes());
    f.extend_from_slice(&(frequence * alignement as u32).to_le_bytes());
    f.extend_from_slice(&alignement.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    chunk(b"fmt ", &f)
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let corps: Vec<u8> = chunks.concat();
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(4 + corps.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(&corps);
    v
}

/// Un chunk `LIST`/INFO suivi d'un `id3 ` portant une « pochette » : ce que
/// les logiciels d'étiquetage rangent après `data`.
fn queue_d_etiquettes(taille_pochette: usize) -> Vec<u8> {
    let mut info = b"INFO".to_vec();
    info.extend_from_slice(&chunk(b"INAM", b"That's Our Sun\0"));
    let mut v = chunk(b"LIST", &info);
    v.extend_from_slice(&chunk(b"id3 ", &vec![MOTIF_QUEUE; taille_pochette]));
    v
}

fn lire_fichier(octets: &[u8]) -> (Option<StreamInfo>, Vec<u8>) {
    let mut buf = octets.to_vec();
    let si = detect_and_parse_fichier(&mut buf);
    (si, buf)
}

/// Le cas du signalement : 16 bits, 44,1 kHz, stéréo, étiquettes et pochette
/// derrière `data`. Pas un octet de la queue ne doit sortir.
#[test]
fn les_etiquettes_rangees_apres_data_ne_sont_jamais_lues() {
    let donnees = echantillons(4 * 4410);
    let fichier = riff(&[
        fmt_pcm(2, 44_100, 16),
        chunk(b"data", &donnees),
        queue_d_etiquettes(62_000),
    ]);
    let (si, pcm) = lire_fichier(&fichier);
    let si = si.expect("WAV lisible");
    assert_eq!(
        (si.channels, si.sample_rate, si.bits_per_sample),
        (2, 44_100, 16)
    );
    assert_eq!(si.data_offset, 44);
    assert_eq!(si.duration_ms, 100);
    assert_eq!(pcm.len(), donnees.len(), "rien après la fin de `data`");
    assert_eq!(pcm, donnees);
    assert!(!pcm.contains(&MOTIF_QUEUE));
}

/// Le chemin de l'enchaînement (piste suivante préparée en tâche de fond)
/// passe par le même découpage, depuis un vrai fichier.
#[test]
fn la_piste_suivante_preparee_s_arrete_a_la_fin_de_data() {
    let donnees = echantillons(6 * 2000);
    let fichier = riff(&[
        fmt_pcm(2, 48_000, 24),
        chunk(b"data", &donnees),
        queue_d_etiquettes(9_001),
    ]);
    let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    std::fs::write(f.path(), &fichier).unwrap();
    let piste = stage_direct_track(
        f.path().to_str().unwrap(),
        "That's Our Sun".into(),
        "Benjamin Wallfisch".into(),
        "Alien: Romulus".into(),
        None,
        0,
    )
    .expect("WAV préparable");
    assert_eq!(piste.pcm, donnees);
    assert_eq!(piste.duration_ms, 2000 * 1000 / 48_000);
}

/// Un WAV propre (en-tête canonique, rien après `data`) ressort intact.
#[test]
fn un_wav_propre_est_inchange() {
    let donnees = echantillons(4 * 1000);
    let fichier = riff(&[fmt_pcm(2, 44_100, 16), chunk(b"data", &donnees)]);
    let (si, pcm) = lire_fichier(&fichier);
    assert!(si.is_some());
    assert_eq!(pcm, donnees);
    // Et le parseur des flux n'a pas bougé.
    let mut flux = fichier.clone();
    let si = detect_and_parse(&mut flux).unwrap();
    assert_eq!(si.data_offset, 44);
    assert_eq!(flux, donnees);
}

/// Taille de `data` impaire : l'octet de bourrage n'est pas un échantillon,
/// et la trame incomplète n'est pas envoyée.
#[test]
fn une_taille_impaire_exclut_le_bourrage_et_la_trame_incomplete() {
    let donnees = echantillons(4 * 100 + 1);
    let fichier = riff(&[
        fmt_pcm(2, 44_100, 16),
        chunk(b"data", &donnees),
        queue_d_etiquettes(300),
    ]);
    let (si, pcm) = lire_fichier(&fichier);
    assert!(si.is_some());
    assert_eq!(pcm, donnees[..400].to_vec());
}

/// Un chunk de taille impaire AVANT `data` : son octet de bourrage décalait le
/// parcours, `data` n'était plus trouvé et l'en-tête partait comme du son.
#[test]
fn un_chunk_impair_avant_data_ne_fait_pas_jouer_l_en_tete() {
    let donnees = echantillons(4 * 50);
    let fichier = riff(&[
        fmt_pcm(2, 44_100, 16),
        chunk(b"LIST", b"INFOINAM\x03\0\0\0ab\0"), // 15 octets : impair
        chunk(b"data", &donnees),
    ]);
    let (si, pcm) = lire_fichier(&fichier);
    let si = si.expect("WAV lisible");
    assert_eq!(si.data_offset, 12 + 24 + 8 + 16 + 8);
    assert_eq!(pcm, donnees);
}

/// Un chunk `JUNK` avant `fmt ` (courant en diffusion) : le format se lit dans
/// le chunk `fmt `, pas à des positions fixes.
#[test]
fn le_format_se_lit_dans_le_chunk_fmt_ou_qu_il_soit() {
    let donnees = echantillons(6 * 10);
    let fichier = riff(&[
        chunk(b"JUNK", &[0u8; 28]),
        fmt_pcm(2, 96_000, 24),
        chunk(b"data", &donnees),
    ]);
    let (si, pcm) = lire_fichier(&fichier);
    let si = si.expect("WAV lisible");
    assert_eq!(
        (si.channels, si.sample_rate, si.bits_per_sample),
        (2, 96_000, 24)
    );
    assert_eq!(pcm, donnees);
}

/// WAV écrit en flux : taille de `data` à 0 ou 0xFFFF_FFFF. Borné à la fin du
/// fichier, chunk de queue reconnu exclu.
#[test]
fn une_taille_inconnue_borne_au_chunk_de_queue_reconnu() {
    let donnees = echantillons(4 * 300);
    for taille in [0u32, 0xFFFF_FFFF, 0x7FFF_FFFF] {
        let avec_queue = riff(&[
            fmt_pcm(2, 44_100, 16),
            chunk_data_declare(taille, &donnees),
            queue_d_etiquettes(1_234),
        ]);
        let (si, pcm) = lire_fichier(&avec_queue);
        assert!(si.is_some(), "taille {taille:#x}");
        assert_eq!(pcm, donnees, "taille {taille:#x} : queue exclue");

        let sans_queue = riff(&[fmt_pcm(2, 44_100, 16), chunk_data_declare(taille, &donnees)]);
        let (_, pcm) = lire_fichier(&sans_queue);
        assert_eq!(
            pcm, donnees,
            "taille {taille:#x} : jusqu'à la fin du fichier"
        );
    }
}

/// Une taille déclarée plus grande que le fichier (fichier tronqué) borne à la
/// fin réelle, ramenée à une trame entière.
#[test]
fn un_fichier_tronque_borne_a_sa_fin_reelle() {
    let donnees = echantillons(4 * 20 + 2);
    let fichier = riff(&[
        fmt_pcm(2, 44_100, 16),
        chunk_data_declare(1_000_000, &donnees),
    ]);
    let (si, pcm) = lire_fichier(&fichier);
    assert!(si.is_some());
    assert_eq!(pcm, donnees[..80].to_vec());
}

/// Un WAV sans chunk `data` n'est pas joué en direct : l'appelant repasse par
/// le flux HTTP.
#[test]
fn un_wav_sans_chunk_data_est_refuse() {
    let fichier = riff(&[fmt_pcm(2, 44_100, 16), chunk(b"LIST", &[0u8; 64])]);
    let (si, _) = lire_fichier(&fichier);
    assert!(si.is_none());
}

/// Le flux HTTP servi aux renderers (décodage symphonia, `decode_to_pcm_streaming`)
/// est lui aussi borné au chunk `data` : la queue d'étiquettes n'y entre pas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_flux_http_decode_est_borne_au_chunk_data() {
    let trames = 4410usize;
    let donnees = echantillons(4 * trames);
    let fichier = riff(&[
        fmt_pcm(2, 44_100, 16),
        chunk(b"data", &donnees),
        queue_d_etiquettes(62_000),
    ]);
    let f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    std::fs::write(f.path(), &fichier).unwrap();
    let chemin = f.path().to_string_lossy().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let decodeur = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming(&chemin, None, None, tx, 1_024)
    });
    let mut morceaux = Vec::new();
    while let Some(m) = rx.recv().await {
        morceaux.push(m);
    }
    let (bits, frequence) = decodeur.await.unwrap().unwrap();
    assert_eq!(frequence, 44_100);
    let mut tout: Vec<u8> = morceaux.concat();
    if tout.starts_with(b"RIFF") {
        tout.drain(..44);
    }
    let charge = tout;
    let octets_par_trame = 2 * (bits as usize / 8);
    assert_eq!(
        charge.len(),
        trames * octets_par_trame,
        "ni plus ni moins que `data`"
    );
    if bits == 16 {
        assert_eq!(charge, donnees);
    }
}
