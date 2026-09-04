use super::transcode_source_to_file;
use crate::audio::alac_encoder::encode_alac_m4a;

const TRAMES: usize = 8_000;
const CANAUX: u16 = 2;
const CADENCE: u32 = 96_000;

fn signal(amplitude: i32) -> Vec<i32> {
    let mut v = Vec::with_capacity(TRAMES * CANAUX as usize);
    for i in 0..TRAMES {
        let s = ((i as f64 * 0.05).sin() * amplitude as f64) as i32;
        for _ in 0..CANAUX {
            v.push(s);
        }
    }
    v
}

/// Un dossier par test ET par processus : plusieurs agents travaillent sur
/// la même machine de compilation. Le garde le supprime en sortant, y
/// compris quand le test panique (#3030) — la construction à la main
/// laissait un `tune-i1437-*` de plus à chaque exécution.
fn dossier(nom: &str) -> crate::test_scratch::ScratchDir {
    crate::test_scratch::scratch_dir(&format!("tune-i1437-{nom}"))
}

fn source_alac(dossier: &std::path::Path, profondeur: u16) -> String {
    let amplitude = if profondeur >= 24 { 4_000_000 } else { 20_000 };
    let m4a = encode_alac_m4a(&signal(amplitude), profondeur, CANAUX, CADENCE)
        .expect("encodage ALAC de la source");
    let chemin = dossier.join(format!("source-{profondeur}.m4a"));
    std::fs::write(&chemin, &m4a).expect("écriture de la source");
    chemin.to_string_lossy().to_string()
}

/// `(profondeur annoncée par l'en-tête, octets du chunk data)`.
fn entete_wav(chemin: &str) -> (u16, u32) {
    let w = std::fs::read(chemin).expect("relecture du WAV");
    assert!(w.len() > 44, "WAV tronqué : {} octets", w.len());
    assert_eq!(&w[0..4], b"RIFF");
    (
        u16::from_le_bytes([w[34], w[35]]),
        u32::from_le_bytes([w[40], w[41], w[42], w[43]]),
    )
}

/// Le flux est-il AUDIBLE ? Un `Vec` de zéros passerait toutes les
/// vérifications de format ci-dessus.
fn niveau_non_nul(chemin: &str, profondeur: u16) -> bool {
    let w = std::fs::read(chemin).expect("relecture du WAV");
    let pas = (profondeur / 8) as usize;
    w[44..]
        .chunks_exact(pas)
        .any(|c| c.iter().any(|&o| o != 0 && o != 0xFF))
}

#[tokio::test]
async fn la_cible_est_toujours_une_largeur_que_la_chaine_sait_ecrire() {
    let d = dossier("cible");
    let src = source_alac(&d, 24);

    // Les quatre cas sont joués JUSQU'AU BOUT avant de conclure : le
    // premier `panic!` masquerait les suivants, et les deux défauts que ce
    // test couvre ne tombent pas sur la même cible.
    let mut anomalies: Vec<String> = Vec::new();

    // (profondeur demandée par l'orchestrateur, profondeur attendue)
    for (demande, attendu) in [(16u16, 16u16), (20, 24), (24, 24), (32, 32)] {
        let dest = d.join(format!("sortie-{demande}.wav"));
        let dest_s = dest.to_string_lossy().to_string();
        match transcode_source_to_file(
            src.clone(),
            CADENCE,
            CANAUX,
            demande,
            "wav".to_string(),
            None,
            None,
            None,
            dest_s.clone(),
            None,
        )
        .await
        {
            Err(e) => anomalies.push(format!(
                "cible {demande} bits : le transcodage ÉCHOUE — {e}"
            )),
            Ok((_taille, _pcm, ecrite)) => {
                if ecrite != attendu {
                    anomalies.push(format!(
                        "cible {demande} bits : profondeur rendue {ecrite}, attendu {attendu}"
                    ));
                }
                let (entete, data) = entete_wav(&dest_s);
                if entete != attendu {
                    anomalies.push(format!(
                        "cible {demande} bits : l'en-tête WAV annonce {entete}, attendu {attendu}"
                    ));
                }
                let voulu = TRAMES * CANAUX as usize * (attendu as usize / 8);
                if data as usize != voulu {
                    anomalies.push(format!(
                        "cible {demande} bits : chunk data de {data} octets, attendu {voulu}"
                    ));
                }
                if !niveau_non_nul(&dest_s, entete) {
                    anomalies.push(format!(
                        "cible {demande} bits : le flux écrit est SILENCIEUX"
                    ));
                }
            }
        }
    }
    assert!(
        anomalies.is_empty(),
        "la profondeur annoncée n'est pas celle qui est écrite :\n  {}",
        anomalies.join("\n  ")
    );
}

/// Témoin anti-régression : le seul chemin que les testeurs écoutent
/// aujourd'hui — « Forcer le WAV 16 bits » sur une zone DLNA — ne bouge
/// pas d'un octet, que la source soit 16 ou 24 bits.
#[tokio::test]
async fn le_forcage_wav16_reste_identique() {
    let d = dossier("temoin");
    for (profondeur_source, octets_attendus) in [(16u16, 2usize), (24, 2)] {
        let src = source_alac(&d, profondeur_source);
        let dest = d.join(format!("wav16-{profondeur_source}.wav"));
        let dest_s = dest.to_string_lossy().to_string();
        let (_t, _p, ecrite) = transcode_source_to_file(
            src,
            CADENCE,
            CANAUX,
            16,
            "wav".to_string(),
            None,
            None,
            None,
            dest_s.clone(),
            None,
        )
        .await
        .expect("transcodage WAV 16 bits");
        assert_eq!(ecrite, 16, "source {profondeur_source} bits → WAV 16 bits");
        let (entete, data) = entete_wav(&dest_s);
        assert_eq!(entete, 16);
        assert_eq!(data as usize, TRAMES * CANAUX as usize * octets_attendus);
        assert!(
            niveau_non_nul(&dest_s, 16),
            "source {profondeur_source} bits : le WAV 16 bits est SILENCIEUX"
        );
    }
}
