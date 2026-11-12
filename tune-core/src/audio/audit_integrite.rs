//! Audit complet et strict, sans conversion ni décodeur externe. Refs #6286.
//! Mode rapide (#7847) : sommes de contrôle des trames FLAC et des pages Ogg,
//! sans décoder, voir [`verifier_rapide`].
mod crc;
mod priorite;
pub mod rappel;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};
use symphonia::core::{
    codecs::{CodecParameters, audio::AudioDecoderOptions},
    errors::Error,
    formats::probe::Hint,
    formats::{FormatOptions, TrackType},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
};

/// #8105 — fin des données audio d'un FLAC : la taille du fichier moins les
/// étiquettes que des tagueurs ajoutent APRÈS la dernière trame (ID3v1,
/// Lyrics3v2, APEv2, dans n'importe quel ordre). Le démultiplexeur FLAC de
/// symphonia ne les connaît pas : il les colle à la dernière trame, dont le
/// CRC-16 devient faux, la perd, puis rend « unexpected end of file » au
/// dernier multiple de la taille de bloc. Un FLAC intact était ainsi déclaré
/// corrompu, avec un nombre d'échantillons pris pour une position.
fn fin_des_donnees<R: Read + Seek>(source: &mut R, taille: u64) -> io::Result<u64> {
    let mut lire = |pos: u64, n: usize| -> io::Result<Vec<u8>> {
        let mut o = vec![0u8; n];
        source.seek(SeekFrom::Start(pos))?;
        source.read_exact(&mut o)?;
        Ok(o)
    };
    let mut fin = taille;
    loop {
        if fin >= 128 && lire(fin - 128, 3)? == b"TAG" {
            fin -= 128;
            continue;
        }
        if fin >= 32 {
            let pied = lire(fin - 32, 32)?;
            if &pied[..8] == b"APETAGEX" {
                // Taille des éléments et du pied ; l'en-tête, s'il est
                // annoncé (bit 31 des drapeaux), s'y ajoute.
                let corps = u64::from(u32::from_le_bytes(pied[12..16].try_into().unwrap()));
                let drapeaux = u32::from_le_bytes(pied[20..24].try_into().unwrap());
                let total = corps + if drapeaux & (1 << 31) != 0 { 32 } else { 0 };
                if total >= 32 && total <= fin {
                    fin -= total;
                    continue;
                }
            }
        }
        if fin >= 15 && lire(fin - 9, 9)? == b"LYRICS200" {
            let chiffres = lire(fin - 15, 6)?;
            if let Some(corps) = std::str::from_utf8(&chiffres)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                && corps + 15 <= fin
            {
                fin -= corps + 15;
                continue;
            }
        }
        return Ok(fin);
    }
}

/// Une source bornée à `[0, fin)` : ce qui suit la dernière trame n'est pas
/// donné au démultiplexeur.
struct Borne<R> {
    source: R,
    fin: u64,
    pos: u64,
}

impl<R: Read> Read for Borne<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let reste = self.fin.saturating_sub(self.pos);
        let n = buf.len().min(usize::try_from(reste).unwrap_or(usize::MAX));
        if n == 0 {
            return Ok(0);
        }
        let lus = self.source.read(&mut buf[..n])?;
        self.pos += lus as u64;
        Ok(lus)
    }
}

impl<R: Seek> Seek for Borne<R> {
    fn seek(&mut self, vers: SeekFrom) -> io::Result<u64> {
        let cible = match vers {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.fin.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "position invalide"))?;
        self.pos = self.source.seek(SeekFrom::Start(cible))?;
        Ok(self.pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for Borne<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.fin)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Sain,
    Corrompu,
    NonVerifiable,
}

/// Mode de l'audit. `Complet` décode tout et vérifie le MD5 ; `Rapide` ne
/// contrôle que les sommes du conteneur (FLAC et Ogg), sans décoder.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Mode {
    #[default]
    #[serde(rename = "full", alias = "complet")]
    Complet,
    #[serde(rename = "quick", alias = "rapide")]
    Rapide,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Unite {
    Trame,
    Page,
}

/// La trame FLAC ou la page Ogg désignée par le mode rapide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Localisation {
    pub unite: Unite,
    /// Rang dans le fichier, à partir de 0.
    pub numero: u64,
    /// Position de son premier octet.
    pub octet: u64,
    /// Son premier échantillon, s'il est connu.
    pub echantillon: Option<u64>,
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
    /// Mode rapide : la première trame ou page fautive, ou celle où le
    /// fichier est coupé.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localisation: Option<Localisation>,
    /// Mode rapide : nombre de trames ou de pages fautives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fautes: Option<u64>,
    /// Mode rapide : trames ou pages contrôlées.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unites_controlees: Option<u64>,
}

impl Resultat {
    pub fn defaut(code: &str, erreur: impl ToString, position: Option<u64>, trames: u64) -> Self {
        Self {
            verdict: Verdict::Corrompu,
            code: code.into(),
            erreur: Some(erreur.to_string()),
            position_trames: position,
            trames_decodees: trames,
            localisation: None,
            fautes: None,
            unites_controlees: None,
        }
    }
    fn non_verifiable(code: &str, trames: u64) -> Self {
        Self {
            verdict: Verdict::NonVerifiable,
            code: code.into(),
            erreur: None,
            position_trames: None,
            trames_decodees: trames,
            localisation: None,
            fautes: None,
            unites_controlees: None,
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

/// L'audit du mode demandé.
pub fn verifier_selon(
    mode: Mode,
    chemin: &Path,
    continuer: impl FnMut() -> bool,
) -> Option<Resultat> {
    match mode {
        Mode::Complet => verifier(chemin, continuer),
        Mode::Rapide => verifier_rapide(chemin, continuer),
    }
}

/// Ouvre un fichier ordinaire, ou rend le verdict « illisible ».
fn ouvrir(chemin: &Path) -> Result<File, Resultat> {
    let ordinaire = || {
        Resultat::defaut(
            "illisible",
            "Ce chemin ne désigne pas un fichier ordinaire",
            None,
            0,
        )
    };
    match std::fs::metadata(chemin) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(ordinaire()),
        Err(e) => return Err(Resultat::defaut("illisible", e, None, 0)),
    }
    let fichier = File::open(chemin).map_err(|e| Resultat::defaut("illisible", e, None, 0))?;
    match fichier.metadata() {
        Ok(m) if m.is_file() => Ok(fichier),
        _ => Err(ordinaire()),
    }
}

/// Mode rapide (#7847) : CRC-8 et CRC-16 de chaque trame FLAC, CRC-32 de
/// chaque page Ogg, sans décoder. Nomme la première trame ou page fautive
/// et compte les fautes ; un fichier coupé est `tronque`, distinct d'une
/// somme fausse. Le MD5 de STREAMINFO n'est pas vérifié : c'est le mode
/// complet. Un autre format est non vérifiable dans ce mode.
pub fn verifier_rapide(chemin: &Path, mut continuer: impl FnMut() -> bool) -> Option<Resultat> {
    let _priorite = priorite::baisser();
    if !continuer() {
        return None;
    }
    let mut fichier = match ouvrir(chemin) {
        Ok(f) => f,
        Err(r) => return Some(r),
    };
    // Les étiquettes finales ne sont pas des trames (#8105).
    let taille = match fichier.metadata() {
        Ok(m) => m.len(),
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    let fin = match fin_des_donnees(&mut fichier, taille)
        .and_then(|fin| fichier.seek(SeekFrom::Start(0)).map(|_| fin))
    {
        Ok(fin) => fin,
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    let constat = match crc::analyser(fichier.take(fin), continuer) {
        Ok(crc::Analyse::Constat(c)) => c,
        Ok(crc::Analyse::NonApplicable) => {
            return Some(Resultat::non_verifiable("mode_rapide_non_applicable", 0));
        }
        Ok(crc::Analyse::Interrompu) => return None,
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    let unite = match constat.conteneur {
        crc::Conteneur::Flac => Unite::Trame,
        crc::Conteneur::Ogg => Unite::Page,
    };
    let localiser = |l: &crc::Lieu| Localisation {
        unite,
        numero: l.numero,
        octet: l.octet,
        echantillon: l.echantillon,
    };
    let mut resultat = Resultat {
        verdict: Verdict::Sain,
        code: "sain".into(),
        erreur: None,
        position_trames: None,
        trames_decodees: 0,
        localisation: None,
        fautes: Some(constat.fautes),
        unites_controlees: Some(constat.unites),
    };
    let nom = match unite {
        Unite::Trame => "trame",
        Unite::Page => "page",
    };
    if let Some(faute) = &constat.premiere_faute {
        resultat.verdict = Verdict::Corrompu;
        resultat.code = faute.code.into();
        resultat.erreur = Some(format!(
            "{} ; première {nom} fautive : n° {}, octet {} ; {} {nom}(s) fautive(s){}",
            faute.detail,
            faute.lieu.numero,
            faute.lieu.octet,
            constat.fautes,
            if constat.tronque.is_some() {
                " ; fichier tronqué"
            } else {
                ""
            }
        ));
        resultat.position_trames = faute.lieu.echantillon;
        resultat.localisation = Some(localiser(&faute.lieu));
    } else if let Some(lieu) = &constat.tronque {
        // Toutes les sommes présentes sont justes : le fichier est coupé,
        // pas abîmé. Il n'est pas sain pour autant.
        resultat.verdict = Verdict::Corrompu;
        resultat.code = "tronque".into();
        resultat.erreur = Some(format!(
            "Fichier tronqué : coupé à la {nom} n° {}, octet {} ; taille réelle : {taille} octets",
            lieu.numero, lieu.octet
        ));
        resultat.position_trames = lieu.echantillon;
        resultat.localisation = Some(localiser(lieu));
    } else if let Some(lieu) = &constat.fin_absente {
        // Ogg aux pages toutes justes sans page de fin : coupé pile entre deux
        // pages ou écrit sans le drapeau. Ni sain ni corrompu.
        resultat.verdict = Verdict::NonVerifiable;
        resultat.code = "fin_de_flux_absente".into();
        resultat.erreur = Some(format!(
            "Pages justes, mais aucune page de fin de flux : fichier coupé après la page n° {} ou écrit sans marque de fin",
            lieu.numero.saturating_sub(1)
        ));
        resultat.position_trames = lieu.echantillon;
        resultat.localisation = Some(localiser(lieu));
    }
    Some(resultat)
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
    let fichier = match File::open(chemin) {
        Ok(f) => f,
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    let taille = match fichier.metadata() {
        Ok(m) if m.is_file() => m.len(),
        _ => {
            return Some(Resultat::defaut(
                "illisible",
                "Ce chemin ne désigne pas un fichier ordinaire",
                None,
                0,
            ));
        }
    };
    verifier_source(
        fichier,
        taille,
        chemin.extension().and_then(|e| e.to_str()),
        || File::open(chemin),
        continuer,
    )
}

/// Le décodage complet d'une source de `taille` octets. `rouvrir` donne une
/// seconde lecture de la même source, pour situer une fin de flux
/// inattendue. Séparé de [`verifier`] pour que les témoins puissent donner
/// une source aux lectures courtes, comme `ReadFile` sous Windows (#8105).
fn verifier_source<R: Read + Seek + Send + Sync + 'static>(
    mut source: R,
    taille: u64,
    extension: Option<&str>,
    rouvrir: impl FnOnce() -> io::Result<R>,
    mut continuer: impl FnMut() -> bool,
) -> Option<Resultat> {
    let flac = super::flac_vendeur::entete_flac(&mut source);
    let fin = match flac {
        Some(_) => fin_des_donnees(&mut source, taille),
        None => Ok(taille),
    };
    let fin = match fin.and_then(|fin| source.seek(SeekFrom::Start(0)).map(|_| fin)) {
        Ok(fin) => fin,
        Err(e) => return Some(Resultat::defaut("illisible", e, None, 0)),
    };
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }
    let mut format = match symphonia::default::get_probe().probe(
        &hint,
        MediaSourceStream::new(
            Box::new(Borne {
                source,
                fin,
                pos: 0,
            }),
            Default::default(),
        ),
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(f) => f,
        Err(Error::Unsupported(e)) => {
            let connu = extension.is_some_and(|s| {
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
            Err(Error::IoError(e))
                if flac.is_some() && e.kind() == io::ErrorKind::UnexpectedEof =>
            {
                return fin_de_flux_inattendue(rouvrir, fin, taille, trames, attendues, continuer);
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
        localisation: None,
        fautes: None,
        unites_controlees: None,
    })
}

/// Le décodeur FLAC a rencontré la fin du flux avant le dernier échantillon
/// annoncé (#8105). « Tronqué » n'est rendu que si le parcours des trames
/// le confirme : le fichier est réellement plus court que ce qu'il annonce.
/// Sinon c'est la dernière trame qui est fautive (abîmée, ou suivie de
/// données inconnues). Le message donne la position et la taille réelle.
fn fin_de_flux_inattendue<R: Read>(
    rouvrir: impl FnOnce() -> io::Result<R>,
    fin: u64,
    taille: u64,
    trames: u64,
    attendues: Option<u64>,
    continuer: impl FnMut() -> bool,
) -> Option<Resultat> {
    let annonce = attendues
        .map(|n| format!(" sur {n} annoncés"))
        .unwrap_or_default();
    let tailles = if fin == taille {
        format!("taille réelle : {taille} octets")
    } else {
        format!("taille réelle : {taille} octets, dont {fin} de données audio")
    };
    let constat = match rouvrir().and_then(|s| crc::analyser(s.take(fin), continuer)) {
        Ok(crc::Analyse::Constat(c)) => Some(c),
        Ok(crc::Analyse::Interrompu) => return None,
        Ok(crc::Analyse::NonApplicable) | Err(_) => None,
    };
    let (code, position, detail) = match constat {
        Some(crc::Constat {
            tronque: Some(lieu),
            ..
        }) => (
            "tronque",
            lieu.echantillon,
            format!(
                "Fichier tronqué : coupé à la trame n° {}, octet {}",
                lieu.numero, lieu.octet
            ),
        ),
        Some(crc::Constat {
            premiere_faute: Some(faute),
            ..
        }) => (
            faute.code,
            faute.lieu.echantillon,
            format!(
                "Fin de flux illisible : {} (trame n° {}, octet {})",
                faute.detail, faute.lieu.numero, faute.lieu.octet
            ),
        ),
        _ => (
            "decodage",
            None,
            "Fin de flux inattendue, toutes les trames lues sont pourtant justes".to_string(),
        ),
    };
    Some(Resultat::defaut(
        code,
        format!("{detail} ; {trames} échantillons décodés{annonce} ; {tailles}"),
        position.or(Some(trames)),
        trames,
    ))
}

#[cfg(test)]
#[path = "audit_integrite_tests_8105.rs"]
mod tests_8105;

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
            matches!(
                r.code.as_str(),
                "tronque" | "decodage" | "longueur_incorrecte"
            ),
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
    fn auditer_vite(octets: &[u8], nom: &str) -> Resultat {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join(nom);
        std::fs::write(&chemin, octets).unwrap();
        verifier_selon(Mode::Rapide, &chemin, || true).unwrap()
    }
    #[test]
    fn rapide_flac_sain_sans_decoder() {
        let r = auditer_vite(&flac(), "temoin.flac");
        assert_eq!(r.verdict, Verdict::Sain, "FLAC sain refusé : {r:?}");
        assert_eq!(r.trames_decodees, 0, "le mode rapide ne décode rien");
        assert_eq!(r.fautes, Some(0));
        assert!(r.unites_controlees.is_some_and(|n| n > 1), "{r:?}");
    }
    #[test]
    fn rapide_octet_audio_altere_nomme_sa_trame() {
        let mut fichier = flac();
        let milieu = fichier.len() / 2;
        fichier[milieu] ^= 1;
        let r = auditer_vite(&fichier, "temoin.flac");
        assert_eq!(r.verdict, Verdict::Corrompu, "octet altéré ignoré : {r:?}");
        assert_eq!(r.code, "crc_trame");
        assert_eq!(r.fautes, Some(1));
        let l = r.localisation.expect("trame fautive non nommée");
        assert_eq!(l.unite, Unite::Trame);
        assert!(l.octet as usize <= milieu, "{l:?}");
        assert_eq!(r.position_trames, l.echantillon);
    }
    #[test]
    fn rapide_tronque_n_est_pas_une_faute_de_somme() {
        let mut fichier = flac();
        fichier.truncate(fichier.len() / 2);
        let r = auditer_vite(&fichier, "temoin.flac");
        assert_eq!(r.code, "tronque", "troncature mal nommée : {r:?}");
        assert_eq!(r.fautes, Some(0));
        assert_eq!(
            r.verdict,
            Verdict::Corrompu,
            "un fichier coupé n'est pas sain"
        );
        assert!(r.localisation.is_some());
    }
    #[test]
    fn rapide_ne_verifie_pas_le_md5_que_garde_le_mode_complet() {
        // Le MD5 de STREAMINFO exige le décodage : le mode rapide ne le voit
        // pas, le mode complet si. C'est la frontière annoncée des deux modes.
        let mut fichier = flac();
        fichier[26] ^= 1;
        assert_eq!(auditer_vite(&fichier, "temoin.flac").verdict, Verdict::Sain);
        assert_eq!(auditer(&fichier).code, "md5_incorrect");
    }
    #[test]
    fn rapide_hors_flac_et_ogg_non_verifiable() {
        let mut encodeur = super::super::encoder::AudioEncoder::new("wav", 44100, 16, 2);
        encodeur.start_sync().unwrap();
        encodeur.write_sync(&[0u8; 4800]).unwrap();
        let r = auditer_vite(&encodeur.finish_sync().unwrap(), "temoin.wav");
        assert_eq!(r.verdict, Verdict::NonVerifiable);
        assert_eq!(r.code, "mode_rapide_non_applicable");
    }
    #[test]
    fn rapide_pause_ne_produit_pas_de_verdict() {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("temoin.flac");
        std::fs::write(&chemin, flac()).unwrap();
        assert!(verifier_rapide(&chemin, || false).is_none());
    }
    /// Mesure, pas témoin : `cargo test -p tune-core --release --lib --
    /// temps_compare --ignored --nocapture`. Un « album » de 10 pistes FLAC
    /// 16/44,1 de 60 s, audité dans chaque mode.
    #[test]
    #[ignore]
    fn temps_compare_des_modes_rapide_et_complet() {
        let dossier = tempfile::tempdir().unwrap();
        let mut graine = 1u32;
        let pistes: Vec<_> = (0..10)
            .map(|n| {
                let pcm: Vec<u8> = (0..44100 * 60 * 2)
                    .flat_map(|i: i32| {
                        graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        let ton = ((i as f32) * 0.0005 * (n + 1) as f32).sin() * 12000.0;
                        ((ton as i32 + (graine >> 22) as i32 - 512) as i16).to_le_bytes()
                    })
                    .collect();
                let mut encodeur = super::super::encoder::AudioEncoder::new("flac", 44100, 16, 2);
                encodeur.start_sync().unwrap();
                encodeur.write_sync(&pcm).unwrap();
                let mut fichier = encodeur.finish_sync().unwrap();
                fichier[26..42].copy_from_slice(&Md5::digest(&pcm));
                let chemin = dossier.path().join(format!("{n:02}.flac"));
                std::fs::write(&chemin, fichier).unwrap();
                chemin
            })
            .collect();
        let octets: u64 = pistes
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        for mode in [Mode::Complet, Mode::Rapide] {
            let debut = std::time::Instant::now();
            for p in &pistes {
                let r = verifier_selon(mode, p, || true).unwrap();
                assert_eq!(r.verdict, Verdict::Sain, "{mode:?} {r:?}");
            }
            println!(
                "{mode:?} : {} pistes, {} Mo, {:?}",
                pistes.len(),
                octets / 1_000_000,
                debut.elapsed()
            );
        }
    }
    #[test]
    fn mode_se_lit_en_anglais_et_en_francais() {
        for (texte, mode) in [
            ("\"quick\"", Mode::Rapide),
            ("\"rapide\"", Mode::Rapide),
            ("\"full\"", Mode::Complet),
            ("\"complet\"", Mode::Complet),
        ] {
            assert_eq!(serde_json::from_str::<Mode>(texte).unwrap(), mode);
        }
        assert_eq!(Mode::default(), Mode::Complet);
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

#[cfg(test)]
#[path = "audit_integrite_tests_8403.rs"]
mod tests_8403;
