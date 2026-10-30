//! Audit complet et strict, sans conversion ni décodeur externe. Refs #6286.
mod priorite;
pub mod rappel;
use serde::{Deserialize, Serialize};
use std::{fs::File, path::Path};
use symphonia::core::{
    codecs::{CodecParameters, audio::AudioDecoderOptions},
    errors::Error,
    formats::probe::Hint,
    formats::{FormatOptions, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Sain,
    Corrompu,
    NonVerifiable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resultat {
    pub verdict: Verdict,
    pub code: String,
    pub erreur: Option<String>,
    /// Position du premier défaut, en trames PCM depuis le début du fichier.
    /// Un MD5 final ne permet pas de localiser l'octet fautif : position inconnue.
    pub position_trames: Option<u64>,
    pub trames_decodees: u64,
}

impl Resultat {
    pub fn defaut(code: &str, erreur: impl ToString, position: Option<u64>, trames: u64) -> Self {
        Self {
            verdict: Verdict::Corrompu,
            code: code.into(),
            erreur: Some(erreur.to_string()),
            position_trames: position,
            trames_decodees: trames,
        }
    }
    fn non_verifiable(code: &str, trames: u64) -> Self {
        Self {
            verdict: Verdict::NonVerifiable,
            code: code.into(),
            erreur: None,
            position_trames: None,
            trames_decodees: trames,
        }
    }
}

fn longueur_annoncee(track: &symphonia::core::formats::Track) -> Option<u64> {
    use symphonia::core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
    // Le démultiplexeur MPEG estime parfois cette valeur à partir des 17
    // premières trames. Une estimation de durée n'est pas une preuve de
    // troncature d'un VBR valide : contrôler ses paquets, pas cette estimation.
    match &track.codec_params {
        Some(CodecParameters::Audio(p))
            if matches!(p.codec, CODEC_ID_MP1 | CODEC_ID_MP2 | CODEC_ID_MP3) =>
        {
            None
        }
        _ => track.num_frames,
    }
}

/// `continuer` est appelé entre les paquets : pause et priorité à la lecture
/// restent réactives même pour une image de disque de plusieurs heures.
/// Une interruption ne produit AUCUN verdict et le fichier sera repris à zéro.
pub fn verifier(chemin: &Path, mut continuer: impl FnMut() -> bool) -> Option<Resultat> {
    let _priorite = priorite::baisser();
    if !continuer() {
        return None;
    }
    match std::fs::metadata(chemin) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            return Some(Resultat::defaut(
                "illisible",
                "Ce chemin ne désigne pas un fichier ordinaire",
                None,
                0,
            ));
        }
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    }
    let mut fichier = match File::open(chemin) {
        Ok(f) => f,
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    match fichier.metadata() {
        Ok(m) if m.is_file() => {}
        _ => {
            return Some(Resultat::defaut(
                "illisible",
                "Ce chemin ne désigne pas un fichier ordinaire",
                None,
                0,
            ));
        }
    }
    let flac = super::flac_vendeur::entete_flac(&mut fichier);
    use std::io::{Seek, SeekFrom};
    if let Err(e) = fichier.seek(SeekFrom::Start(0)) {
        return Some(Resultat::defaut("illisible", e, None, 0));
    }
    let mut hint = Hint::new();
    if let Some(extension) = chemin.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = match symphonia::default::get_probe().probe(
        &hint,
        MediaSourceStream::new(Box::new(fichier), Default::default()),
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(f) => f,
        Err(Error::Unsupported(e)) => {
            let connu = chemin
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| {
                    matches!(
                        s.to_ascii_lowercase().as_str(),
                        "flac" | "wav" | "mp3" | "ogg" | "oga" | "m4a" | "mp4" | "aac"
                    )
                });
            return Some(if connu {
                Resultat::defaut("entete_illisible", e, Some(0), 0)
            } else {
                Resultat::non_verifiable("format_non_pris_en_charge", 0)
            });
        }
        Err(e) => return Some(Resultat::defaut("entete_illisible", e, Some(0), 0)),
    };
    let Some(track) = format.default_track(TrackType::Audio) else {
        return Some(Resultat::non_verifiable("aucune_piste_audio", 0));
    };
    let Some(CodecParameters::Audio(params)) = &track.codec_params else {
        return Some(Resultat::non_verifiable("codec_non_pris_en_charge", 0));
    };
    let mut id = track.id;
    let mut attendues = flac
        .as_ref()
        .map(|f| f.total_samples)
        .filter(|n| *n > 0)
        .or_else(|| longueur_annoncee(track));
    let mut decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default().verify(true))
    {
        Ok(d) => d,
        Err(Error::Unsupported(_)) => {
            return Some(Resultat::non_verifiable("codec_non_pris_en_charge", 0));
        }
        Err(e) => return Some(Resultat::defaut("decodeur", e, Some(0), 0)),
    };
    let mut trames = 0u64;
    let mut trames_lien = 0u64;
    loop {
        if !continuer() {
            return None;
        }
        let paquet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::ResetRequired) => {
                // Un Ogg chaîné est un fichier valide : vérifier chaque lien
                // puis poursuivre avec les paramètres du suivant.
                if attendues.is_some_and(|n| n != trames_lien)
                    || decoder.finalize().verify_ok == Some(false)
                {
                    return Some(Resultat::defaut(
                        "lien_incomplet",
                        "Lien Ogg incomplet ou invalide",
                        Some(trames),
                        trames,
                    ));
                }
                let Some(track) = format.default_track(TrackType::Audio) else {
                    return Some(Resultat::non_verifiable("aucune_piste_audio", trames));
                };
                let Some(CodecParameters::Audio(params)) = &track.codec_params else {
                    return Some(Resultat::non_verifiable("codec_non_pris_en_charge", trames));
                };
                id = track.id;
                attendues = longueur_annoncee(track);
                decoder = match symphonia::default::get_codecs()
                    .make_audio_decoder(params, &AudioDecoderOptions::default().verify(true))
                {
                    Ok(d) => d,
                    Err(Error::Unsupported(_)) => {
                        return Some(Resultat::non_verifiable("codec_non_pris_en_charge", trames));
                    }
                    Err(e) => return Some(Resultat::defaut("decodeur", e, Some(trames), trames)),
                };
                trames_lien = 0;
                continue;
            }
            Err(e) => return Some(Resultat::defaut("decodage", e, Some(trames), trames)),
        };
        if paquet.track_id != id {
            continue;
        }
        if flac.is_some() && paquet.pts.get() != trames as i64 {
            return Some(Resultat::defaut(
                "trames_manquantes",
                "Discontinuité des trames FLAC",
                Some(trames),
                trames,
            ));
        }
        match decoder.decode(&paquet) {
            Ok(pcm) => {
                trames += pcm.frames() as u64;
                trames_lien += pcm.frames() as u64;
            }
            Err(Error::Unsupported(_)) => {
                return Some(Resultat::non_verifiable("codec_non_pris_en_charge", trames));
            }
            Err(e) => return Some(Resultat::defaut("decodage", e, Some(trames), trames)),
        }
    }
    if trames == 0 || attendues.is_some_and(|n| n != trames_lien) {
        return Some(Resultat::defaut(
            "longueur_incorrecte",
            format!("{trames} trames décodées ; attendues : {attendues:?}"),
            Some(trames),
            trames,
        ));
    }
    let verification = decoder.finalize().verify_ok;
    if let Some(entete) = flac {
        if entete.md5_nul {
            return Some(Resultat::non_verifiable("md5_nul", trames));
        }
        if verification != Some(true) {
            return Some(Resultat::defaut(
                "md5_incorrect",
                "Le MD5 du PCM décodé diffère de STREAMINFO",
                None,
                trames,
            ));
        }
    } else if verification == Some(false) {
        return Some(Resultat::defaut(
            "somme_incorrecte",
            "Échec de vérification du décodeur",
            None,
            trames,
        ));
    }
    Some(Resultat {
        verdict: Verdict::Sain,
        code: "sain".into(),
        erreur: None,
        position_trames: None,
        trames_decodees: trames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};

    fn flac() -> Vec<u8> {
        let pcm: Vec<u8> = (0..24000i32)
            .flat_map(|n| ((n * 97) as i16).to_le_bytes())
            .collect();
        let mut encodeur = super::super::encoder::AudioEncoder::new("flac", 44100, 16, 2);
        encodeur.start_sync().unwrap();
        encodeur.write_sync(&pcm).unwrap();
        let mut fichier = encodeur.finish_sync().unwrap();
        // L'encodeur embarqué laisse ce champ nul. La fixture lui donne le
        // MD5 connu du PCM d'origine, indépendamment du décodeur testé.
        fichier[26..42].copy_from_slice(&Md5::digest(&pcm));
        fichier
    }
    fn auditer(octets: &[u8]) -> Resultat {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("temoin.flac");
        std::fs::write(&chemin, octets).unwrap();
        verifier(&chemin, || true).unwrap()
    }
    #[test]
    fn flac_sain_decode_complet_et_md5_verifie() {
        let r = auditer(&flac());
        assert_eq!(r.verdict, Verdict::Sain, "FLAC sain refusé : {r:?}");
        assert_eq!(
            r.trames_decodees, 12000,
            "le fichier entier doit être décodé"
        );
    }
    #[test]
    fn flac_octet_modifie_detecte_par_md5() {
        let mut fichier = flac();
        // Un seul octet de la signature change : le décodage reste valide,
        // un contrôle limité aux CRC de trames laisserait passer le défaut.
        fichier[26] ^= 1;
        let r = auditer(&fichier);
        assert_eq!(r.code, "md5_incorrect", "MD5 STREAMINFO ignoré : {r:?}");
        assert_eq!(r.verdict, Verdict::Corrompu);
    }
    #[test]
    fn flac_tronque_n_est_jamais_sain() {
        let mut fichier = flac();
        fichier.truncate(fichier.len() / 2);
        let r = auditer(&fichier);
        assert_eq!(r.verdict, Verdict::Corrompu, "troncature ignorée : {r:?}");
        assert!(
            matches!(r.code.as_str(), "decodage" | "longueur_incorrecte"),
            "troncature non décodée : {r:?}"
        );
        assert!(r.position_trames.is_some());
    }
    #[test]
    fn flac_octet_audio_altere_n_est_jamais_sain() {
        let mut fichier = flac();
        let milieu = fichier.len() / 2;
        fichier[milieu] ^= 1;
        let r = auditer(&fichier);
        assert_eq!(
            r.verdict,
            Verdict::Corrompu,
            "octet audio altéré ignoré : {r:?}"
        );
        assert!(r.trames_decodees < 12000 || r.code == "md5_incorrect");
    }
    #[test]
    fn fichier_illisible_est_signale() {
        let dossier = tempfile::tempdir().unwrap();
        let r = verifier(&dossier.path().join("absent.flac"), || true).unwrap();
        assert_eq!(r.code, "illisible");
        assert_eq!(r.verdict, Verdict::Corrompu);
    }
    #[test]
    fn flac_md5_nul_decode_mais_non_verifiable() {
        let mut fichier = flac();
        fichier[26..42].fill(0);
        let r = auditer(&fichier);
        assert_eq!(
            r.verdict,
            Verdict::NonVerifiable,
            "MD5 nul mal classé : {r:?}"
        );
        assert_eq!(r.code, "md5_nul");
        assert_eq!(
            r.trames_decodees, 12000,
            "MD5 nul ne dispense pas de décoder"
        );
    }
    #[test]
    fn md5_nul_ne_masque_pas_une_troncature() {
        let mut fichier = flac();
        fichier[26..42].fill(0);
        fichier.truncate(fichier.len() / 2);
        assert_eq!(auditer(&fichier).verdict, Verdict::Corrompu);
    }
    #[test]
    fn wav_sain_et_wav_tronque_sont_distingues() {
        let pcm = vec![0u8; 48000];
        let mut encodeur = super::super::encoder::AudioEncoder::new("wav", 44100, 16, 2);
        encodeur.start_sync().unwrap();
        encodeur.write_sync(&pcm).unwrap();
        let mut fichier = encodeur.finish_sync().unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("temoin.wav");
        std::fs::write(&chemin, &fichier).unwrap();
        let r = verifier(&chemin, || true).unwrap();
        assert_eq!(r.verdict, Verdict::Sain, "WAV valide refusé : {r:?}");
        assert_eq!(r.trames_decodees, 12000);
        fichier.truncate(fichier.len() / 2);
        std::fs::write(&chemin, &fichier).unwrap();
        let r = verifier(&chemin, || true).unwrap();
        assert_eq!(r.verdict, Verdict::Corrompu, "WAV tronqué accepté : {r:?}");
    }
    #[test]
    fn pause_ne_produit_pas_de_verdict() {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("temoin.flac");
        std::fs::write(&chemin, flac()).unwrap();
        assert!(verifier(&chemin, || false).is_none());
    }
    #[test]
    fn mp3_vbr_sans_xing_n_est_pas_corrompu_par_une_duree_estimee() {
        // Trames MPEG-1 Layer III silencieuses (side information et données
        // nulles), sans Xing : 17 à 128 kb/s puis 17 à 320 kb/s. La durée
        // estimée sur les premières trames surestime donc la longueur réelle.
        let mut fichier = Vec::new();
        for (bitrate, taille) in [(0x90, 417), (0xe0, 1044)] {
            for _ in 0..17 {
                let debut = fichier.len();
                fichier.extend_from_slice(&[0xff, 0xfb, bitrate, 0x00]);
                fichier.resize(debut + taille, 0);
            }
        }
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("vbr.mp3");
        std::fs::write(&chemin, fichier).unwrap();
        let r = verifier(&chemin, || true).unwrap();
        assert_eq!(
            r.verdict,
            Verdict::Sain,
            "durée MPEG estimée prise pour une troncature : {r:?}"
        );
        assert_eq!(r.trames_decodees, 34 * 1152);
    }
}
