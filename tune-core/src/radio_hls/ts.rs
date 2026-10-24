//! Des segments HLS au flux élémentaire : démultiplexage MPEG-TS
//! (ISO 13818-1), audio « empaqueté » (`.aac`, `.mp3` précédés d'un ID3) et
//! titres ID3.
//!
//! La sortie est le flux audio ÉLÉMENTAIRE, bout à bout : des trames ADTS
//! (AAC) ou MP3. Les deux se synchronisent seules ; le lecteur radio les
//! pousse dans symphonia exactement comme le corps d'une station Icecast
//! `.aac` ou `.mp3`. Les PTS ne servent pas : des segments consécutifs se
//! suivent sans trou (mesuré sur la BBC, étude #6363 §3.2).

/// Ce que portent les segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charge {
    /// AAC en ADTS (TS type `0x0F`, ou `.aac` empaqueté).
    Adts,
    /// MP3 (TS types `0x03`/`0x04`, ou `.mp3` empaqueté).
    Mp3,
}

/// Un format de segment que Tune ne lit pas : le texte nomme lequel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NonPrisEnCharge(pub(crate) String);

const TAILLE_PAQUET: usize = 188;
const SYNCHRO: u8 = 0x47;

/// Les types de flux du PMT qui comptent ici.
const TYPE_MP3_1: u8 = 0x03;
const TYPE_MP3_2: u8 = 0x04;
const TYPE_ADTS: u8 = 0x0F;
const TYPE_LATM: u8 = 0x11;
const TYPE_ID3: u8 = 0x15;
const TYPE_AC3: u8 = 0x81;
const TYPE_EAC3: u8 = 0x87;

/// Le démultiplexeur d'UN segment TS (ou d'une suite de segments sans
/// discontinuité : l'état — PID, compteurs — survit d'un appel à l'autre).
#[derive(Debug, Default)]
pub(crate) struct Demux {
    pid_pmt: Option<u16>,
    pid_audio: Option<u16>,
    pid_id3: Option<u16>,
    charge: Option<Charge>,
    /// Le dernier compteur de continuité vu, par PID suivi.
    cc_audio: Option<u8>,
    /// Les ruptures de continuité vues sur le PID audio.
    pub(crate) ruptures: u32,
    /// Octets resynchronisés (perte du `0x47`).
    pub(crate) resynchros: u32,
    /// Le PES ID3 en cours de réception.
    id3_pes: Vec<u8>,
    /// Le dernier titre ID3 lu.
    pub(crate) titre_id3: Option<super::liste::TitreListe>,
    /// Le refus nommé, quand le PMT ne porte qu'un son illisible.
    refus: Option<String>,
}

impl Demux {
    /// Ce que le PMT a annoncé, une fois lu.
    pub(crate) fn charge(&self) -> Option<Charge> {
        self.charge
    }

    /// Démultiplexe `octets` (un segment entier) et ajoute le flux
    /// élémentaire audio à `sortie`.
    pub(crate) fn pousser(
        &mut self,
        octets: &[u8],
        sortie: &mut Vec<u8>,
    ) -> Result<(), NonPrisEnCharge> {
        // Le compteur de continuité se juge DANS un segment : d'un segment
        // à l'autre, un codeur relancé ou une découpe différente le fait
        // repartir d'ailleurs, et y voir un « paquet dupliqué » jetterait le
        // premier paquet légitime du segment.
        self.cc_audio = None;
        let mut i = 0usize;
        while i + TAILLE_PAQUET <= octets.len() {
            if octets[i] != SYNCHRO {
                // Synchro perdue : on cherche le prochain `0x47` suivi d'un
                // autre 188 octets plus loin (ou de la fin du segment).
                self.resynchros += 1;
                i += 1;
                while i + TAILLE_PAQUET <= octets.len()
                    && !(octets[i] == SYNCHRO
                        && (i + TAILLE_PAQUET == octets.len()
                            || octets.get(i + TAILLE_PAQUET) == Some(&SYNCHRO)))
                {
                    i += 1;
                }
                continue;
            }
            self.paquet(&octets[i..i + TAILLE_PAQUET], sortie);
            i += TAILLE_PAQUET;
        }
        if let Some(refus) = self.refus.take() {
            return Err(NonPrisEnCharge(refus));
        }
        Ok(())
    }

    fn paquet(&mut self, p: &[u8], sortie: &mut Vec<u8>) {
        let erreur_transport = p[1] & 0x80 != 0;
        let debut = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let controle = (p[3] >> 4) & 0x03;
        let cc = p[3] & 0x0F;
        if erreur_transport {
            return;
        }
        let mut charge_utile = match controle {
            0b01 => &p[4..],
            0b11 => {
                let longueur = usize::from(p[4]);
                match p.get(5 + longueur..) {
                    Some(reste) => reste,
                    None => return,
                }
            }
            // 0b10 : champ d'adaptation seul ; 0b00 : réservé.
            _ => return,
        };
        if pid == 0 {
            self.lire_pat(debut, charge_utile);
        } else if Some(pid) == self.pid_pmt {
            self.lire_pmt(debut, charge_utile);
        } else if Some(pid) == self.pid_audio {
            if let Some(avant) = self.cc_audio
                && cc != (avant + 1) & 0x0F
                && cc != avant
            {
                self.ruptures += 1;
                tracing::debug!(
                    attendu = (avant + 1) & 0x0F,
                    recu = cc,
                    "hls_ts_rupture_de_continuite"
                );
            }
            if self.cc_audio == Some(cc) {
                // Paquet dupliqué (ISO 13818-1 §2.4.3.3) : on l'écarte.
                return;
            }
            self.cc_audio = Some(cc);
            if debut {
                match entete_pes(charge_utile) {
                    Some(apres) => charge_utile = apres,
                    None => return,
                }
            }
            sortie.extend_from_slice(charge_utile);
        } else if Some(pid) == self.pid_id3 {
            if debut {
                self.finir_id3();
                if let Some(apres) = entete_pes(charge_utile) {
                    self.id3_pes.extend_from_slice(apres);
                }
            } else if !self.id3_pes.is_empty() {
                self.id3_pes.extend_from_slice(charge_utile);
            }
        }
    }

    /// Un PES ID3 se termine au début du suivant, ou à la fin du segment.
    pub(crate) fn finir_id3(&mut self) {
        if !self.id3_pes.is_empty() {
            if let Some(titre) = titre_id3(&self.id3_pes) {
                self.titre_id3 = Some(titre);
            }
            self.id3_pes.clear();
        }
    }

    fn section<'a>(debut: bool, charge_utile: &'a [u8], table: u8) -> Option<&'a [u8]> {
        if !debut {
            // Une section sur plusieurs paquets : PAT et PMT d'une radio
            // tiennent toujours dans un seul ; on attend le prochain.
            return None;
        }
        let pointeur = usize::from(*charge_utile.first()?);
        let s = charge_utile.get(1 + pointeur..)?;
        if *s.first()? != table {
            return None;
        }
        let longueur = (usize::from(s.get(1)? & 0x0F) << 8) | usize::from(*s.get(2)?);
        // Sans le CRC final (4 octets).
        s.get(3..(3 + longueur).checked_sub(4)?)
    }

    fn lire_pat(&mut self, debut: bool, charge_utile: &[u8]) {
        let Some(s) = Self::section(debut, charge_utile, 0x00) else {
            return;
        };
        // 5 octets d'en-tête de section étendue, puis des entrées de 4.
        let Some(entrees) = s.get(5..) else { return };
        for e in entrees.chunks_exact(4) {
            let programme = u16::from_be_bytes([e[0], e[1]]);
            let pid = (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]);
            if programme != 0 {
                self.pid_pmt = Some(pid);
                return;
            }
        }
    }

    fn lire_pmt(&mut self, debut: bool, charge_utile: &[u8]) {
        let Some(s) = Self::section(debut, charge_utile, 0x02) else {
            return;
        };
        // program_number(2) version(1) section(1) dernière(1) PCR_PID(2)
        // program_info_length(2).
        let Some(info) = s.get(7..9) else { return };
        let longueur_info = (usize::from(info[0] & 0x0F) << 8) | usize::from(info[1]);
        let Some(mut flux) = s.get(9 + longueur_info..) else {
            return;
        };
        let mut audio: Option<(u16, Charge)> = None;
        let mut illisible: Option<&'static str> = None;
        let mut id3 = None;
        while flux.len() >= 5 {
            let type_flux = flux[0];
            let pid = (u16::from(flux[1] & 0x1F) << 8) | u16::from(flux[2]);
            let longueur_es = (usize::from(flux[3] & 0x0F) << 8) | usize::from(flux[4]);
            match type_flux {
                TYPE_ADTS if audio.is_none() => audio = Some((pid, Charge::Adts)),
                TYPE_MP3_1 | TYPE_MP3_2 if audio.is_none() => audio = Some((pid, Charge::Mp3)),
                TYPE_LATM => illisible = illisible.or(Some("AAC en LATM/LOAS")),
                TYPE_AC3 => illisible = illisible.or(Some("AC-3 (Dolby Digital)")),
                TYPE_EAC3 => illisible = illisible.or(Some("E-AC-3 (Dolby Digital Plus)")),
                TYPE_ID3 => id3 = Some(pid),
                _ => {}
            }
            flux = flux.get(5 + longueur_es..).unwrap_or(&[]);
        }
        self.pid_id3 = id3;
        match audio {
            Some((pid, charge)) => {
                if self.pid_audio != Some(pid) {
                    self.cc_audio = None;
                }
                self.pid_audio = Some(pid);
                self.charge = Some(charge);
            }
            None => {
                self.refus = Some(match illisible {
                    Some(codec) => format!("le segment porte de l'audio {codec}"),
                    None => "le segment ne porte aucun flux audio AAC ou MP3".to_string(),
                });
            }
        }
    }
}

/// Saute l'en-tête d'un PES (ISO 13818-1 §2.4.3.6) et rend sa charge utile.
fn entete_pes(p: &[u8]) -> Option<&[u8]> {
    if p.len() < 9 || p[0..3] != [0, 0, 1] {
        return None;
    }
    let longueur_entete = usize::from(p[8]);
    p.get(9 + longueur_entete..)
}

/// La longueur d'une étiquette ID3v2 en tête de `octets`, en-tête compris.
pub(crate) fn longueur_id3(octets: &[u8]) -> Option<usize> {
    if octets.len() < 10 || &octets[0..3] != b"ID3" {
        return None;
    }
    let taille = octets[6..10]
        .iter()
        .fold(0usize, |acc, b| (acc << 7) | usize::from(b & 0x7F));
    let pied = if octets[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + taille + pied)
}

/// Un texte de trame ID3 (`TIT2`, `TPE1`) : octet d'encodage, puis le texte.
fn texte_id3(donnees: &[u8]) -> Option<String> {
    let (&encodage, texte) = donnees.split_first()?;
    let texte = match encodage {
        0 => texte.iter().map(|&b| char::from(b)).collect::<String>(),
        3 => String::from_utf8_lossy(texte).into_owned(),
        1 | 2 => {
            let mut unites: Vec<u16> = texte
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            let petit_boutiste = encodage == 1 && unites.first() == Some(&0xFFFE);
            if petit_boutiste {
                unites = unites.iter().map(|u| u.swap_bytes()).collect();
            }
            if unites.first() == Some(&0xFEFF) {
                unites.remove(0);
            }
            String::from_utf16_lossy(&unites)
        }
        _ => return None,
    };
    let texte = texte.trim_matches(char::from(0)).trim().to_string();
    (!texte.is_empty()).then_some(texte)
}

/// Le titre (et l'artiste) d'une étiquette ID3v2.3/2.4 : `TIT2`, `TPE1`.
pub(crate) fn titre_id3(octets: &[u8]) -> Option<super::liste::TitreListe> {
    let fin = longueur_id3(octets)?.min(octets.len());
    let version = octets[3];
    let mut i = 10usize;
    if octets[5] & 0x40 != 0 {
        // En-tête étendu : sa taille est en tête.
        let t = octets.get(10..14)?;
        let taille = if version >= 4 {
            t.iter()
                .fold(0usize, |a, b| (a << 7) | usize::from(b & 0x7F))
        } else {
            u32::from_be_bytes([t[0], t[1], t[2], t[3]]) as usize + 4
        };
        i += taille;
    }
    let (mut titre, mut artiste) = (None, None);
    while i + 10 <= fin {
        let id = &octets[i..i + 4];
        if id[0] == 0 {
            break;
        }
        let t = &octets[i + 4..i + 8];
        let taille = if version >= 4 {
            t.iter()
                .fold(0usize, |a, b| (a << 7) | usize::from(b & 0x7F))
        } else {
            u32::from_be_bytes([t[0], t[1], t[2], t[3]]) as usize
        };
        let donnees = octets.get(i + 10..(i + 10 + taille).min(fin))?;
        match id {
            b"TIT2" => titre = texte_id3(donnees),
            b"TPE1" => artiste = texte_id3(donnees),
            _ => {}
        }
        i += 10 + taille;
    }
    Some(super::liste::TitreListe {
        titre: titre?,
        artiste,
    })
}

/// Le flux élémentaire d'un segment « empaqueté » (RFC 8216 §3.4) : un
/// `.aac` ou un `.mp3` brut, précédé d'un ID3 qui porte l'horodatage.
pub(crate) fn empaquete(
    octets: &[u8],
) -> Result<(Charge, &[u8], Option<super::liste::TitreListe>), NonPrisEnCharge> {
    let (reste, titre) = match longueur_id3(octets) {
        Some(n) => (octets.get(n..).unwrap_or(&[]), titre_id3(octets)),
        None => (octets, None),
    };
    match reste {
        [0xFF, b, ..] if b & 0xF6 == 0xF0 => Ok((Charge::Adts, reste, titre)),
        [0xFF, b, ..] if b & 0xE0 == 0xE0 => Ok((Charge::Mp3, reste, titre)),
        _ => Err(NonPrisEnCharge(
            "le segment n'est ni du MPEG-TS, ni de l'AAC ou du MP3 brut".into(),
        )),
    }
}

/// Le segment est-il du fMP4/CMAF (`ftyp`, `styp`, `moof`, `sidx`) ?
pub(crate) fn est_du_fmp4(octets: &[u8]) -> bool {
    octets.len() >= 8 && matches!(&octets[4..8], b"ftyp" | b"styp" | b"moof" | b"sidx")
}

/// Le débit des trames ADTS de `octets`, en kbit/s : la somme de leurs
/// tailles sur leur durée (1 024 échantillons du cœur par trame). C'est le
/// débit du CODEC, mesuré, sans le surcoût TS que compte `BANDWIDTH`.
pub(crate) fn debit_adts_kbps(octets: &[u8]) -> Option<u32> {
    let (mut i, mut trames, mut total, mut cadence) = (0usize, 0u64, 0u64, 0u32);
    while i + 7 <= octets.len() {
        if octets[i] != 0xFF || octets[i + 1] & 0xF6 != 0xF0 {
            i += 1;
            continue;
        }
        let indice = usize::from((octets[i + 2] >> 2) & 0x0F);
        let longueur = (usize::from(octets[i + 3] & 0x03) << 11)
            | (usize::from(octets[i + 4]) << 3)
            | usize::from(octets[i + 5] >> 5);
        if longueur < 7 {
            i += 1;
            continue;
        }
        cadence = *[
            96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
        ]
        .get(indice)
        .unwrap_or(&0);
        trames += 1 + u64::from(octets[i + 6] & 0x03);
        total += longueur as u64;
        i += longueur;
    }
    if trames == 0 || cadence == 0 {
        return None;
    }
    let secondes = trames as f64 * 1024.0 / f64::from(cadence);
    Some((total as f64 * 8.0 / secondes / 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::super::fabrique_tests as fab;
    use super::*;

    #[test]
    fn le_ts_rend_l_adts_bout_a_bout_et_saute_les_paquets_dupliques() {
        let adts = fab::trames_adts_lc(48_000, 2, 10);
        let ts = fab::segment_ts(&adts, 0, true);
        let mut demux = Demux::default();
        let mut sortie = Vec::new();
        demux.pousser(&ts, &mut sortie).unwrap();
        assert_eq!(demux.charge(), Some(Charge::Adts));
        assert_eq!(sortie, adts, "le flux élémentaire ressort intact");
        assert_eq!(demux.ruptures, 0);
    }

    #[test]
    fn la_synchro_perdue_est_retrouvee() {
        let adts = fab::trames_adts_lc(48_000, 2, 10);
        let mut ts = fab::segment_ts(&adts, 0, true);
        // Trois octets parasites après le PMT.
        ts.splice(2 * 188..2 * 188, [1u8, 2, 3]);
        let mut demux = Demux::default();
        let mut sortie = Vec::new();
        demux.pousser(&ts, &mut sortie).unwrap();
        assert_eq!(sortie, adts);
        assert!(demux.resynchros > 0);
    }

    #[test]
    fn un_pmt_ac3_est_refuse_en_le_nommant() {
        let ts = fab::segment_ts_type(&[0u8; 400], 0, 0x81);
        let erreur = Demux::default().pousser(&ts, &mut Vec::new()).unwrap_err();
        assert!(erreur.0.contains("AC-3"), "{}", erreur.0);
    }

    #[test]
    fn le_titre_id3_est_lu() {
        let id3 = fab::etiquette_id3("So What", Some("Miles Davis"));
        let titre = titre_id3(&id3).unwrap();
        assert_eq!(titre.titre, "So What");
        assert_eq!(titre.artiste.as_deref(), Some("Miles Davis"));
        let mut paquet = id3.clone();
        paquet.extend(fab::trames_adts_lc(48_000, 2, 2));
        let (charge, reste, titre) = empaquete(&paquet).unwrap();
        assert_eq!(charge, Charge::Adts);
        assert_eq!(reste.len(), paquet.len() - id3.len());
        assert_eq!(titre.unwrap().titre, "So What");
    }

    #[test]
    fn le_debit_adts_est_mesure() {
        // 10 trames de 128 octets à 24 kHz : 1 280 octets en 10 × 1024 / 24 000 s.
        let adts = fab::trames_adts_taille(24_000, 2, 10, 128);
        assert_eq!(debit_adts_kbps(&adts), Some(24));
    }
}
