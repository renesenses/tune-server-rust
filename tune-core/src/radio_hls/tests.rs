//! Témoins du chargeur HLS, contre un faux serveur sur 127.0.0.1.
//!
//! Chaque segment fabriqué porte une MARQUE (l'octet qui remplit ses trames
//! ADTS) : le flux rendu par `HlsSource` se relit trame par trame, et l'on
//! voit exactement quels segments sont sortis, dans quel ordre, et combien
//! de fois. Aucun son, aucun décodeur : c'est le transport qui est jugé ici.

use super::fabrique_tests::{self as fab, ServeurHls};
use super::{HlsSource, Reglages};
use crate::orchestrator::{RADIO_HLS_INTERDIT, RADIO_HLS_UNSUPPORTED};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Des réglages de témoin : relecture vingt fois plus rapide, essais courts.
fn rapides() -> Reglages {
    Reglages {
        facteur_relecture: 0.05,
        attente_essai: Duration::from_millis(20),
        delai_requete: Duration::from_secs(5),
        // Deux secondes réelles sans segment neuf avant de déclarer le direct figé.
        cibles_immobiles: 40.0,
        ..Reglages::default()
    }
}

const TAILLE_TRAME: usize = 64;
const TRAMES_PAR_SEGMENT: usize = 4;

/// Le segment `n` : quatre trames ADTS marquées `n`, en MPEG-TS.
fn segment(n: u64) -> Vec<u8> {
    fab::segment_ts(
        &fab::trames_marquees(48_000, 2, TRAMES_PAR_SEGMENT, TAILLE_TRAME, n as u8),
        (n % 16) as u8,
        true,
    )
}

/// Lit `segments` segments de la source (borné dans le temps) et rend leurs
/// marques, une par TRAME.
fn marques(source: &mut HlsSource, segments: usize) -> Vec<u8> {
    let voulu = segments * TRAMES_PAR_SEGMENT * TAILLE_TRAME;
    let mut flux = Vec::new();
    let limite = Instant::now() + Duration::from_secs(20);
    let mut tampon = [0u8; 512];
    while flux.len() < voulu {
        assert!(
            Instant::now() < limite,
            "la source n'a rendu que {} octets en 20 s",
            flux.len()
        );
        match source.read(&mut tampon) {
            Ok(0) => break,
            Ok(n) => flux.extend_from_slice(&tampon[..n]),
            Err(e) => panic!("la source a rendu une erreur : {e}"),
        }
    }
    flux.chunks(TAILLE_TRAME).map(|t| t[7]).collect()
}

/// Les marques attendues pour les segments `de..de + n`.
fn attendues(de: u64, n: usize) -> Vec<u8> {
    (de..de + n as u64)
        .flat_map(|s| std::iter::repeat_n(s as u8, TRAMES_PAR_SEGMENT))
        .collect()
}

/// Un direct à fenêtre glissante : à la k-ième lecture de la liste, elle
/// montre les segments `k..k + fenetre` (séquence 1000 + k). Le chargeur
/// relit plus vite que la fenêtre ne glisse : chaque segment doit sortir
/// UNE fois, dans l'ordre, sans trou.
#[test]
fn la_fenetre_glissante_sort_chaque_segment_une_fois_dans_l_ordre() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, rang: usize| {
        if chemin == "/live/media.m3u8" {
            // La fenêtre glisse d'un segment toutes les deux lectures.
            let k = (rang / 2) as u64;
            let segs: Vec<(String, String, bool)> = (k..k + 5)
                .map(|s| (format!("seg-{}.ts", 1000 + s), String::new(), false))
                .collect();
            return fab::ok(fab::TYPE_LISTE, fab::liste_media(1, 1000 + k, &segs, false));
        }
        if let Some(n) = chemin
            .strip_prefix("/live/seg-")
            .and_then(|r| r.strip_suffix(".ts"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            return fab::ok(fab::TYPE_TS, segment(n - 1000));
        }
        fab::introuvable()
    });
    let serveur = ServeurHls::lancer(routeur);
    let url = format!("{}/live/media.m3u8", serveur.base);
    let mut source = HlsSource::ouvrir(&url, None, rapides()).expect("ouverture");
    // Départ à trois segments de la fin : 5 - 3 = 2.
    let vues = marques(&mut source, 8);
    assert_eq!(
        vues,
        attendues(2, 8),
        "un trou ou un doublon dans le flux rendu"
    );
    for s in 2..10u64 {
        assert_eq!(
            serveur.demandes(&format!("/live/seg-{}.ts", 1000 + s)),
            1,
            "le segment {s} doit être téléchargé une fois, et une seule"
        );
    }
    assert!(
        serveur.demandes("/live/media.m3u8") > 2,
        "la liste doit être relue"
    );
}

/// La fenêtre glisse PLUS vite que le chargeur ne relit : des segments sont
/// sortis de la liste avant d'être chargés. Le trou est assumé (journalisé
/// `hls_segment_perdu`), la reprise se fait au plus ancien disponible, et
/// rien n'est rejoué.
#[test]
fn un_trou_dans_la_fenetre_reprend_au_plus_ancien_sans_rejouer() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, rang: usize| {
        if chemin == "/live/media.m3u8" {
            // 1re lecture : 0..3 ; 2e : 10..13 ; ensuite immobile.
            let k = if rang == 0 { 0 } else { 10 };
            let segs: Vec<(String, String, bool)> = (k..k + 3)
                .map(|s| (format!("seg-{s}.ts"), String::new(), false))
                .collect();
            return fab::ok(fab::TYPE_LISTE, fab::liste_media(1, k, &segs, false));
        }
        if let Some(n) = chemin
            .strip_prefix("/live/seg-")
            .and_then(|r| r.strip_suffix(".ts"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            return fab::ok(fab::TYPE_TS, segment(n));
        }
        fab::introuvable()
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source = HlsSource::ouvrir(
        &format!("{}/live/media.m3u8", serveur.base),
        None,
        rapides(),
    )
    .unwrap();
    let vues = marques(&mut source, 6);
    let mut voulu = attendues(0, 3);
    voulu.extend(attendues(10, 3));
    assert_eq!(vues, voulu);
}

/// Liste maîtresse et liste média en chemins RELATIFS (`../`, absolus sans
/// hôte, simples noms) : le serveur ne répond qu'aux chemins correctement
/// résolus.
#[test]
fn les_chemins_relatifs_sont_resolus_contre_la_liste_qui_les_porte() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, _rang: usize| {
        match chemin {
        "/radio/maitre/master.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=101760,CODECS=\"mp4a.40.5\"\n../variantes/audio=96000.m3u8\n".to_vec(),
        ),
        "/radio/variantes/audio=96000.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(
                1,
                0,
                &[
                    ("s0.ts".into(), String::new(), false),
                    ("/absolu/s1.ts".into(), String::new(), false),
                    ("../segments/s2.ts".into(), String::new(), false),
                ],
                true,
            ),
        ),
        "/radio/variantes/s0.ts" => fab::ok(fab::TYPE_TS, segment(0)),
        "/absolu/s1.ts" => fab::ok(fab::TYPE_TS, segment(1)),
        "/radio/segments/s2.ts" => fab::ok(fab::TYPE_TS, segment(2)),
        _ => fab::introuvable(),
    }
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source = HlsSource::ouvrir(
        &format!("{}/radio/maitre/master.m3u8", serveur.base),
        None,
        rapides(),
    )
    .expect("ouverture");
    assert_eq!(source.infos.codecs.as_deref(), Some("mp4a.40.5"));
    // Liste close : lue depuis le début, puis fin de flux (Ok(0)).
    let vues = marques(&mut source, 4);
    assert_eq!(vues, attendues(0, 3));
}

/// Une discontinuité (nouveau codeur : PID et compteurs neufs, même
/// format) passe sans couture ; une discontinuité qui CHANGE le format
/// (48 kHz stéréo → 24 kHz mono) clôt le flux pour qu'une session neuve
/// reparte avec le bon en-tête WAV.
#[test]
fn la_discontinuite_passe_et_un_changement_de_format_clot_le_flux() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, _rang: usize| match chemin {
        "/d/media.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(
                1,
                0,
                &[
                    ("0.ts".into(), String::new(), false),
                    ("1.ts".into(), String::new(), true),
                    ("2.ts".into(), String::new(), true),
                ],
                true,
            ),
        ),
        "/d/0.ts" => fab::ok(fab::TYPE_TS, segment(0)),
        // Même format, compteur de continuité qui repart d'ailleurs.
        "/d/1.ts" => fab::ok(
            fab::TYPE_TS,
            fab::segment_ts(
                &fab::trames_marquees(48_000, 2, 4, TAILLE_TRAME, 1),
                9,
                true,
            ),
        ),
        // Format différent.
        "/d/2.ts" => fab::ok(
            fab::TYPE_TS,
            fab::segment_ts(
                &fab::trames_marquees(24_000, 1, 4, TAILLE_TRAME, 2),
                0,
                true,
            ),
        ),
        _ => fab::introuvable(),
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source =
        HlsSource::ouvrir(&format!("{}/d/media.m3u8", serveur.base), None, rapides()).unwrap();
    let vues = marques(&mut source, 3);
    assert_eq!(
        vues,
        attendues(0, 2),
        "le segment au format changé ne doit PAS être servi dans la même session"
    );
}

/// Trois variantes : la plus haute répond 403 (géobloquée), la suivante est
/// retenue, la plus basse n'est jamais demandée.
#[test]
fn la_variante_la_plus_haute_qui_repond_est_retenue() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, _rang: usize| match chemin {
        "/v/master.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            b"#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=56000,CODECS=\"mp4a.40.2\"\nlc48.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=340000,CODECS=\"mp4a.40.2\"\nlc320.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"ac-3\"\nac3.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=101760,CODECS=\"mp4a.40.5\"\nhe96.m3u8\n"
                .to_vec(),
        ),
        "/v/lc320.m3u8" => (403, "text/plain", b"geo".to_vec()),
        "/v/he96.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(1, 0, &[("he.ts".into(), String::new(), false)], true),
        ),
        "/v/he.ts" => fab::ok(fab::TYPE_TS, segment(7)),
        _ => fab::introuvable(),
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source =
        HlsSource::ouvrir(&format!("{}/v/master.m3u8", serveur.base), None, rapides()).unwrap();
    assert_eq!(source.infos.codecs.as_deref(), Some("mp4a.40.5"));
    assert_eq!(marques(&mut source, 2), attendues(7, 1));
    assert_eq!(
        serveur.demandes("/v/lc320.m3u8"),
        1,
        "la plus haute est essayée d'abord"
    );
    assert_eq!(
        serveur.demandes("/v/ac3.m3u8"),
        0,
        "AC-3 n'est jamais essayé"
    );
    assert_eq!(
        serveur.demandes("/v/lc48.m3u8"),
        0,
        "la plus basse n'est pas nécessaire"
    );
}

/// Erreurs réseau : un segment répond 500 deux fois puis 200 (repris, pas
/// de trou) ; la liste échoue deux fois (absorbé) ; un segment est perdu
/// pour de bon (sauté, et le direct continue après lui).
#[test]
fn les_erreurs_passageres_sont_reprises_et_un_segment_perdu_est_saute() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, rang: usize| {
        if chemin == "/e/media.m3u8" {
            // Lectures 2 et 3 en échec ; la fenêtre avance d'un segment par
            // lecture réussie.
            if rang == 2 || rang == 3 {
                return (503, "text/plain", Vec::new());
            }
            let k = rang.saturating_sub(2).max(rang.min(1)) as u64;
            let segs: Vec<(String, String, bool)> = (k..k + 4)
                .map(|s| (format!("{s}.ts"), String::new(), false))
                .collect();
            return fab::ok(fab::TYPE_LISTE, fab::liste_media(1, k, &segs, false));
        }
        let n: u64 = match chemin
            .strip_prefix("/e/")
            .and_then(|r| r.strip_suffix(".ts"))
            .and_then(|n| n.parse().ok())
        {
            Some(n) => n,
            None => return fab::introuvable(),
        };
        match n {
            3 if rang < 2 => (500, "text/plain", Vec::new()),
            5 => (500, "text/plain", Vec::new()),
            _ => fab::ok(fab::TYPE_TS, segment(n)),
        }
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source =
        HlsSource::ouvrir(&format!("{}/e/media.m3u8", serveur.base), None, rapides()).unwrap();
    let vues = marques(&mut source, 5);
    // Départ : 4 - 3 = segment 1. Le 5 est perdu (3 essais), le 3 est repris.
    assert_eq!(
        vues,
        [attendues(1, 4), attendues(6, 1)].concat(),
        "le segment 3 doit être repris après deux échecs, le 5 sauté, aucun rejoué"
    );
    assert_eq!(
        serveur.demandes("/e/3.ts"),
        3,
        "deux échecs puis la réussite"
    );
    assert_eq!(serveur.demandes("/e/5.ts"), 3, "trois essais, puis sauté");
}

/// Une panne DURABLE de la liste (toutes les relectures échouent) rend la
/// main au lecteur par une erreur de `Read`, au lieu de bloquer.
#[test]
fn une_panne_durable_de_la_liste_rend_une_erreur_de_lecture() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, rang: usize| match chemin {
        "/p/media.m3u8" if rang == 0 => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(1, 0, &[("0.ts".into(), String::new(), false)], false),
        ),
        "/p/media.m3u8" => (503, "text/plain", Vec::new()),
        "/p/0.ts" => fab::ok(fab::TYPE_TS, segment(0)),
        _ => fab::introuvable(),
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source =
        HlsSource::ouvrir(&format!("{}/p/media.m3u8", serveur.base), None, rapides()).unwrap();
    let mut tampon = vec![0u8; 4096];
    let limite = Instant::now() + Duration::from_secs(20);
    let erreur = loop {
        assert!(
            Instant::now() < limite,
            "la source doit rendre la main en moins de 20 s"
        );
        match source.read(&mut tampon) {
            Ok(0) => panic!("une panne n'est pas une fin de liste"),
            Ok(_) => continue,
            Err(e) => break e.to_string(),
        }
    };
    assert!(erreur.contains("503"), "{erreur}");
}

fn ouvrir_seul(
    chemin_liste: &'static str,
    liste: Vec<u8>,
    segment: Vec<u8>,
) -> Result<HlsSource, String> {
    let routeur: Arc<fab::Routeur> = Arc::new(move |chemin: &str, _rang: usize| {
        if chemin == chemin_liste {
            fab::ok(fab::TYPE_LISTE, liste.clone())
        } else if chemin.ends_with(".ts") || chemin.ends_with(".m4s") {
            fab::ok(fab::TYPE_TS, segment.clone())
        } else {
            fab::introuvable()
        }
    });
    let serveur = ServeurHls::lancer(routeur);
    HlsSource::ouvrir(&format!("{}{chemin_liste}", serveur.base), None, rapides())
}

/// Ce que Tune ne lit pas est refusé À L'OUVERTURE, avec le préfixe
/// définitif et la raison nommée : fMP4/CMAF, DRM, AC-3, et un 403.
#[test]
fn les_formats_non_pris_en_charge_sont_refuses_en_les_nommant() {
    let fmp4 =
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\n0.m4s\n"
            .to_vec();
    let e = ouvrir_seul("/f/media.m3u8", fmp4, Vec::new())
        .err()
        .unwrap();
    assert!(
        e.starts_with(RADIO_HLS_UNSUPPORTED) && e.contains("fMP4"),
        "{e}"
    );

    let drm = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\"\n#EXTINF:4,\n0.ts\n".to_vec();
    let e = ouvrir_seul("/k/media.m3u8", drm, segment(0)).err().unwrap();
    assert!(
        e.starts_with(RADIO_HLS_UNSUPPORTED) && e.contains("DRM"),
        "{e}"
    );

    let simple = fab::liste_media(1, 0, &[("0.ts".into(), String::new(), false)], true);
    let e = ouvrir_seul(
        "/a/media.m3u8",
        simple.clone(),
        fab::segment_ts_type(&[0u8; 300], 0, 0x81),
    )
    .err()
    .unwrap();
    assert!(
        e.starts_with(RADIO_HLS_UNSUPPORTED) && e.contains("AC-3"),
        "{e}"
    );

    let e = ouvrir_seul(
        "/m/media.m3u8",
        simple,
        b"\x00\x00\x00\x18ftypiso6".to_vec(),
    )
    .err()
    .unwrap();
    assert!(
        e.starts_with(RADIO_HLS_UNSUPPORTED) && e.contains("fMP4"),
        "{e}"
    );

    let routeur: Arc<fab::Routeur> = Arc::new(|_: &str, _: usize| (403, "text/plain", Vec::new()));
    let serveur = ServeurHls::lancer(routeur);
    let e = HlsSource::ouvrir(&format!("{}/g/master.m3u8", serveur.base), None, rapides())
        .err()
        .unwrap();
    assert!(e.starts_with(RADIO_HLS_INTERDIT), "{e}");
}

/// Le titre que porte `EXTINF` devient le titre en cours de la station
/// quand son segment commence à être lu ; il disparaît avec la source.
#[test]
fn le_titre_extinf_devient_le_titre_en_cours() {
    let routeur: Arc<fab::Routeur> = Arc::new(|chemin: &str, _rang: usize| match chemin {
        "/t/media.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            fab::liste_media(
                1,
                0,
                &[
                    ("0.ts".into(), "Miles Davis - So What".into(), false),
                    (
                        "1.ts".into(),
                        "title=\"Blue in Green\",artist=\"Bill Evans\"".into(),
                        false,
                    ),
                ],
                true,
            ),
        ),
        "/t/0.ts" => fab::ok(fab::TYPE_TS, segment(0)),
        "/t/1.ts" => fab::ok(fab::TYPE_TS, segment(1)),
        _ => fab::introuvable(),
    });
    let serveur = ServeurHls::lancer(routeur);
    let url = format!("{}/t/media.m3u8", serveur.base);
    let mut source = HlsSource::ouvrir(&url, None, rapides()).unwrap();
    let titre = super::titre_en_cours(&url).expect("titre du premier segment");
    assert_eq!(
        (titre.titre.as_str(), titre.artiste.as_deref()),
        ("So What", Some("Miles Davis"))
    );
    marques(&mut source, 2);
    let titre = super::titre_en_cours(&url).expect("titre du second segment");
    assert_eq!(titre.titre, "Blue in Green");
    drop(source);
    assert!(super::titre_en_cours(&url).is_none());
}

/// AES-128 (clé servie en clair, IV implicite = numéro de séquence) : le
/// segment est déchiffré avant d'être démultiplexé.
#[test]
fn un_segment_aes_128_est_dechiffre() {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    let cle = [7u8; 16];
    let clair = segment(4);
    let iv = 42u128.to_be_bytes();
    let mut tampon = clair.clone();
    tampon.extend([0u8; 16]);
    let chiffre = cbc::Encryptor::<aes::Aes128>::new((&cle).into(), (&iv).into())
        .encrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut tampon, clair.len())
        .expect("chiffrement")
        .to_vec();
    let routeur: Arc<fab::Routeur> = Arc::new(move |chemin: &str, _rang: usize| {
        match chemin {
        "/c/media.m3u8" => fab::ok(
            fab::TYPE_LISTE,
            b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-KEY:METHOD=AES-128,URI=\"cle.bin\"\n#EXTINF:1,\n42.ts\n#EXT-X-ENDLIST\n".to_vec(),
        ),
        "/c/cle.bin" => fab::ok("application/octet-stream", cle.to_vec()),
        "/c/42.ts" => fab::ok(fab::TYPE_TS, chiffre.clone()),
        _ => fab::introuvable(),
    }
    });
    let serveur = ServeurHls::lancer(routeur);
    let mut source =
        HlsSource::ouvrir(&format!("{}/c/media.m3u8", serveur.base), None, rapides()).unwrap();
    assert_eq!(marques(&mut source, 2), attendues(4, 1));
}
