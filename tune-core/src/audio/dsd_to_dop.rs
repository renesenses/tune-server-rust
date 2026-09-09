/// DSD over PCM (DoP) encoder.
///
/// Packs raw DSD bitstream data into 24-bit PCM frames with DoP marker
/// bytes in the top 8 bits. This allows DSD playback through PCM-only
/// audio interfaces (WASAPI, ASIO, CoreAudio).
///
/// DoP frame layout (24-bit LE per channel):
///   byte 0: DSD bits `[7:0]`  (low byte of 16 DSD bits)
///   byte 1: DSD bits `[15:8]` (high byte of 16 DSD bits)
///   byte 2: marker (0x05 or 0xFA, alternating per frame)
///
/// Sample rates:
///   DSD64  (2.8224 MHz) → 176.4 kHz DoP
///   DSD128 (5.6448 MHz) → 352.8 kHz DoP
///   DSD256 (11.2896 MHz) → 705.6 kHz DoP

pub struct DsdToDoP {
    channels: usize,
    lsb_first: bool,
    frame_count: u64,
    /// Les octets d'une trame INCOMPLÈTE laissés par le bloc précédent.
    ///
    /// Une trame DoP consomme `2 * channels` octets DSD. `feed` recevait des
    /// blocs dont la taille n'est garantie multiple que de `channels` —
    /// `DffStreamReader::open` le documente ainsi — et rendait donc jusqu'à
    /// `2 * channels - 1` octets à chaque appel, définitivement perdus. Sur un
    /// DFF six canaux, 32 766 = 2 730 x 12 + 6 : un octet DSD par canal jeté
    /// toutes les 2,7 ms de DSD64, sur toute la piste.
    ///
    /// Le report rend l'encodeur indifférent au découpage : la garde
    /// `un_decoupage_hostile_produit_exactement_le_meme_porteur` le mesure en
    /// comparant un appel unique à des appels d'un octet.
    reste: Vec<u8>,
}

impl DsdToDoP {
    pub fn new(channels: usize, lsb_first: bool) -> Self {
        Self {
            channels,
            lsb_first,
            frame_count: 0,
            reste: Vec::new(),
        }
    }

    pub fn dop_rate(dsd_rate: u32) -> u32 {
        dsd_rate / 16
    }

    /// Feed a chunk of byte-interleaved DSD data and return 24-bit LE DoP PCM.
    ///
    /// Input: byte-interleaved DSD (ch0_b0, ch1_b0, ch0_b1, ch1_b1, ...)
    /// Each byte = 8 DSD bits. We need 16 bits (2 bytes) per channel per DoP frame.
    /// So we consume `2 * channels` bytes per DoP frame.
    /// Le découpage des blocs n'a AUCUN effet sur ce qui sort : les octets
    /// d'une trame incomplète sont reportés sur l'appel suivant (voir
    /// [`Self::reste`]).
    pub fn feed(&mut self, dsd_data: &[u8]) -> Vec<u8> {
        if self.channels == 0 {
            return Vec::new();
        }
        if self.reste.is_empty() {
            return self.encoder_les_trames_entieres(dsd_data);
        }
        let mut tampon = std::mem::take(&mut self.reste);
        tampon.extend_from_slice(dsd_data);
        self.encoder_les_trames_entieres(&tampon)
    }

    fn encoder_les_trames_entieres(&mut self, dsd_data: &[u8]) -> Vec<u8> {
        let bytes_per_frame = 2 * self.channels;
        let num_frames = dsd_data.len() / bytes_per_frame;
        // 3 bytes per channel per frame (24-bit)
        let mut out = Vec::with_capacity(num_frames * 3 * self.channels);

        for frame_idx in 0..num_frames {
            let marker = if self.frame_count % 2 == 0 {
                0x05u8
            } else {
                0xFAu8
            };

            for ch in 0..self.channels {
                let offset = frame_idx * bytes_per_frame + ch;
                let b0 = dsd_data[offset]; // first 8 DSD bits
                let b1 = dsd_data[offset + self.channels]; // next 8 DSD bits

                // DoP expects MSB-first DSD. DSF is LSB-first → reverse bits.
                let (d0, d1) = if self.lsb_first {
                    (reverse_bits(b0), reverse_bits(b1))
                } else {
                    (b0, b1)
                };

                // 24-bit LE: [low_dsd, high_dsd, marker]
                out.push(d1);
                out.push(d0);
                out.push(marker);
            }

            self.frame_count += 1;
        }

        self.reste.clear();
        self.reste
            .extend_from_slice(&dsd_data[num_frames * bytes_per_frame..]);
        out
    }
}

fn reverse_bits(b: u8) -> u8 {
    let mut r = 0u8;
    for i in 0..8 {
        r |= ((b >> i) & 1) << (7 - i);
    }
    r
}

/// Ce qui DÉTRUIT un porteur DoP sur un chemin de sortie flottant.
///
/// Le DoP n'est pas du PCM : c'est un train DSD de 1 bit logé dans les 16 bits
/// de poids faible d'un mot de 24, l'octet de poids fort portant un marqueur
/// qui alterne `0x05` / `0xFA` à **chaque trame** (voir [`DsdToDoP::feed`]).
/// Le DAC ne verrouille en DSD que s'il voit cette alternance, échantillon par
/// échantillon, sur tous les canaux d'une même trame.
///
/// Deux traitements de la sortie locale la détruisent, et un seul des deux
/// était nommé quelque part :
///
/// 1. **Le rééchantillonnage.** L'alternance `0x05`/`0xFA` est un carré à
///    `fs/2` — 88 200 Hz pour un DoP DSD64 à 176 400 Hz. Le sinc de
///    [`crate::audio::resample`] coupe à la Nyquist de SORTIE (≈ 22 kHz quand
///    on ouvre à 48 kHz) : il n'en reste rien. Le DAC ne voit plus de
///    marqueur, ne verrouille pas, et ce qui subsiste est du bruit à très bas
///    niveau. L'horloge avance, le silence dure — **#3233, Pierre M, fil
///    1043 : « DSD : le temps défile, pas de son »**.
/// 2. **L'adaptation de canaux.** Le marqueur est le MÊME sur tous les canaux
///    d'une trame ; dupliquer ou mélanger des canaux décale le mot de 24 bits
///    et l'octet de poids fort n'est plus le marqueur — c'est exactement le
///    bruit blanc constaté sur Wiim Pro quand le compte de canaux était pris
///    dans la base plutôt que dans le fichier (#1894).
///
/// La doctrine est déjà écrite dans ce dépôt pour les bras **exclusifs**
/// (`WindowsExclusivePcmError`, `outputs/local.rs`) : « allowing a detected DoP
/// carrier through [a float transport] would knowingly hand corrupted DSD to
/// the DAC […] the only safe behaviour is to fail before a sample reaches the
/// callback ». Le chemin cpal **partagé** ne l'appliquait pas : il détruisait
/// le porteur en silence. Ce type l'y porte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DopRuptureChemin {
    /// Le flux est ouvert à une autre cadence que la source : le sinc annihile
    /// le marqueur.
    Reechantillonnage {
        source_sr: u32,
        cadence_ouverte: u32,
    },
    /// Le nombre de canaux change : le mot de 24 bits est décalé, l'octet de
    /// poids fort n'est plus le marqueur.
    AdaptationDeCanaux { source_ch: u16, canaux_ouverts: u16 },
}

impl DopRuptureChemin {
    /// Un représentant de chaque variante — pour les témoins exhaustifs, sur
    /// le modèle de `LocalRateFallback::ALL`.
    pub const TOUTES: [Self; 2] = [
        Self::Reechantillonnage {
            source_sr: 176_400,
            cadence_ouverte: 48_000,
        },
        Self::AdaptationDeCanaux {
            source_ch: 2,
            canaux_ouverts: 1,
        },
    ];

    /// Code stable, destiné à la machine (journal, charge utile JSON).
    pub fn code(self) -> &'static str {
        match self {
            Self::Reechantillonnage { .. } => "dop_detruit_par_reechantillonnage",
            Self::AdaptationDeCanaux { .. } => "dop_detruit_par_adaptation_de_canaux",
        }
    }

    /// Nom de l'événement de journal — un par cause, pour qu'un relevé de
    /// terrain distingue les deux sans lire les champs.
    pub fn evenement_journal(self) -> &'static str {
        match self {
            Self::Reechantillonnage { .. } => "local_audio_dop_carrier_destroyed_by_resample",
            Self::AdaptationDeCanaux { .. } => "local_audio_dop_carrier_destroyed_by_channel_adapt",
        }
    }

    /// Ce que lit quelqu'un dont la musique ne démarre pas — et ce qu'il peut
    /// CHANGER. Même contrat que `WindowsExclusivePcmError::user_message`.
    pub fn message_utilisateur(self, peripherique: &str) -> String {
        let constat = match self {
            Self::Reechantillonnage {
                source_sr,
                cadence_ouverte,
            } => format!(
                "un flux DoP (DSD) arrive à {source_sr} Hz alors que la sortie partagée est ouverte à {cadence_ouverte} Hz"
            ),
            Self::AdaptationDeCanaux {
                source_ch,
                canaux_ouverts,
            } => format!(
                "un flux DoP (DSD) arrive sur {source_ch} canaux alors que la sortie partagée en ouvre {canaux_ouverts}"
            ),
        };
        format!(
            "Sortie « {peripherique} » : {constat}. La conversion détruit le marqueur DoP, le DAC ne verrouille pas en DSD et rien ne sort ; la lecture a été refusée avant l'envoi au périphérique. Passez le mode DSD de la zone en « pcm », réglez le format partagé du système sur la cadence de la source, ou utilisez une sortie exclusive (WASAPI exclusif, ASIO)"
        )
    }

    /// La ligne qui manquait : sans elle, rien ne disait que le porteur avait
    /// été détruit, ni pourquoi.
    pub fn journaliser(self, peripherique: &str) {
        let (source_sr, cadence_ouverte, source_ch, canaux_ouverts) = match self {
            Self::Reechantillonnage {
                source_sr,
                cadence_ouverte,
            } => (Some(source_sr), Some(cadence_ouverte), None, None),
            Self::AdaptationDeCanaux {
                source_ch,
                canaux_ouverts,
            } => (None, None, Some(source_ch), Some(canaux_ouverts)),
        };
        tracing::warn!(
            device = %peripherique,
            code = self.code(),
            source_sr,
            cadence_ouverte,
            source_ch,
            canaux_ouverts,
            evenement = self.evenement_journal(),
            "le porteur DoP ne survit pas à ce chemin de sortie (#3233)"
        );
    }
}

/// Le porteur DoP survivra-t-il au chemin de sortie qu'on vient d'ouvrir ?
///
/// `None` = rien à refuser. C'est le cas de **tout** flux qui n'est pas du DoP
/// — un PCM 24 bits continue d'être rééchantillonné exactement comme avant,
/// aucune régression — et celui d'un DoP servi tel quel (cadence et canaux
/// inchangés), qui est le seul chemin partagé où le DSD passe vraiment.
///
/// La fonction est **pure** et ne connaît ni cpal ni la plateforme : elle se
/// teste depuis n'importe quelle machine, y compris là où WASAPI ne compile
/// pas. C'est la même règle que [`crate::audio::dsd_to_dop::DopRuptureChemin`]
/// documente ; l'ordre des deux causes n'a d'importance que pour le message :
/// le rééchantillonnage est celui que le terrain rencontre (#3233).
pub fn rupture_du_porteur_dop(
    dop: bool,
    source_sr: u32,
    cadence_ouverte: u32,
    source_ch: u16,
    canaux_ouverts: u16,
) -> Option<DopRuptureChemin> {
    if !dop {
        return None;
    }
    if source_sr != cadence_ouverte {
        return Some(DopRuptureChemin::Reechantillonnage {
            source_sr,
            cadence_ouverte,
        });
    }
    if source_ch != canaux_ouverts {
        return Some(DopRuptureChemin::AdaptationDeCanaux {
            source_ch,
            canaux_ouverts,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dop_rate_dsd64() {
        assert_eq!(DsdToDoP::dop_rate(2_822_400), 176_400);
    }

    #[test]
    fn dop_rate_dsd128() {
        assert_eq!(DsdToDoP::dop_rate(5_644_800), 352_800);
    }

    #[test]
    fn marker_alternates() {
        let mut dop = DsdToDoP::new(1, false);
        // 4 bytes = 2 frames for mono (2 bytes per frame)
        let data = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let out = dop.feed(&data);
        // Frame 0: marker 0x05, frame 1: marker 0xFA
        assert_eq!(out.len(), 6); // 2 frames × 3 bytes
        assert_eq!(out[2], 0x05); // first frame marker
        assert_eq!(out[5], 0xFA); // second frame marker
    }

    #[test]
    fn stereo_output_size() {
        let mut dop = DsdToDoP::new(2, false);
        // 8 bytes = 2 frames for stereo (4 bytes per frame = 2 bytes × 2 channels)
        let data = vec![0; 8];
        let out = dop.feed(&data);
        // 2 frames × 2 channels × 3 bytes = 12 bytes
        assert_eq!(out.len(), 12);
    }

    /// GARDE — le découpage des blocs ne change RIEN aux octets produits.
    ///
    /// C'est la propriété qui manquait : `feed` jetait la trame incomplète de
    /// chaque bloc, donc le résultat dépendait de la taille de lecture du
    /// fichier. Un octet par canal disparaissait à chaque bloc dès que celui-ci
    /// n'était pas un multiple de `2 * canaux` — le cas de tout DFF dont le
    /// nombre de canaux est impair une fois divisé dans 32 768.
    #[test]
    fn un_decoupage_hostile_produit_exactement_le_meme_porteur() {
        for canaux in [1usize, 2, 6] {
            let dsd: Vec<u8> = (0..3_000u32).map(|i| (i % 251 + 1) as u8).collect();
            let entier = DsdToDoP::new(canaux, false).feed(&dsd);
            for tranche in [1usize, 3, 7, 2 * canaux + 1, 997] {
                let mut dop = DsdToDoP::new(canaux, false);
                let mut morceaux = Vec::new();
                for bloc in dsd.chunks(tranche) {
                    morceaux.extend_from_slice(&dop.feed(bloc));
                }
                assert_eq!(
                    morceaux, entier,
                    "{canaux} canaux, blocs de {tranche} octets : le porteur DoP \
                     dépend du découpage — des octets DSD sont perdus en route"
                );
            }
        }
    }

    #[test]
    fn lsb_first_reversal() {
        let mut dop = DsdToDoP::new(1, true); // LSB-first (DSF)
        let data = vec![0b10000000, 0b00000001]; // 1 frame
        let out = dop.feed(&data);
        // 0b10000000 reversed = 0b00000001
        // 0b00000001 reversed = 0b10000000
        assert_eq!(out[1], 0b00000001); // high byte = reversed b0
        assert_eq!(out[0], 0b10000000); // low byte = reversed b1
    }

    // ------------------------------------------------------------------
    // #3233 — ce qu'un porteur DoP ne survit pas
    // ------------------------------------------------------------------

    /// Un flux qui n'est PAS du DoP n'est jamais refusé : le PCM 24 bits
    /// continue d'être rééchantillonné et remixé comme il l'a toujours été.
    /// C'est la garde de non-régression du cas à 99 %.
    #[test]
    fn un_flux_non_dop_ne_declenche_jamais_de_refus() {
        for (sr, ch) in [(176_400u32, 2u16), (44_100, 2), (96_000, 6)] {
            for cadence in [sr, 48_000] {
                for canaux in [ch, 2] {
                    assert_eq!(
                        rupture_du_porteur_dop(false, sr, cadence, ch, canaux),
                        None,
                        "un flux PCM ({sr} Hz, {ch} ch) ouvert à {cadence} Hz / {canaux} ch \
                         a été refusé : la garde #3233 déborde sur le chemin ordinaire"
                    );
                }
            }
        }
    }

    /// Le seul chemin partagé où le DSD passe vraiment : le périphérique est
    /// DÉJÀ à la cadence DoP et au bon nombre de canaux. Rien à refuser.
    #[test]
    fn un_dop_servi_tel_quel_passe() {
        assert_eq!(rupture_du_porteur_dop(true, 176_400, 176_400, 2, 2), None);
        assert_eq!(rupture_du_porteur_dop(true, 352_800, 352_800, 2, 2), None);
    }

    /// Le cas de Pierre M (#3233) : DSD64 → DoP 176 400 Hz, sortie partagée
    /// ouverte à la cadence du mélangeur Windows.
    #[test]
    fn un_dop_reechantillonne_est_refuse() {
        assert_eq!(
            rupture_du_porteur_dop(true, 176_400, 48_000, 2, 2),
            Some(DopRuptureChemin::Reechantillonnage {
                source_sr: 176_400,
                cadence_ouverte: 48_000,
            })
        );
    }

    /// Une adaptation de canaux décale le mot de 24 bits : le marqueur n'est
    /// plus l'octet de poids fort (#1894).
    #[test]
    fn un_dop_remixe_est_refuse() {
        assert_eq!(
            rupture_du_porteur_dop(true, 176_400, 176_400, 2, 1),
            Some(DopRuptureChemin::AdaptationDeCanaux {
                source_ch: 2,
                canaux_ouverts: 1,
            })
        );
    }

    /// Codes et événements stables et distincts : un relevé de terrain doit
    /// pouvoir séparer les deux causes sans lire les champs.
    #[test]
    fn chaque_cause_a_son_code_et_son_evenement() {
        let mut codes = Vec::new();
        let mut evenements = Vec::new();
        for cause in DopRuptureChemin::TOUTES {
            assert!(!cause.code().is_empty());
            assert!(
                cause
                    .evenement_journal()
                    .starts_with("local_audio_dop_carrier_")
            );
            let message = cause.message_utilisateur("Topping D90");
            assert!(
                message.contains("Topping D90"),
                "le message ne nomme pas le périphérique : {message}"
            );
            assert!(
                message.contains("« pcm »"),
                "le message ne dit pas quoi CHANGER : {message}"
            );
            codes.push(cause.code());
            evenements.push(cause.evenement_journal());
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), DopRuptureChemin::TOUTES.len());
        evenements.sort_unstable();
        evenements.dedup();
        assert_eq!(evenements.len(), DopRuptureChemin::TOUTES.len());
    }

    /// Y a-t-il, quelque part dans ce tampon 24 bits LE, une suite de
    /// `frames_min` trames dont l'octet de poids fort alterne 0x05/0xFA sur
    /// tous les canaux ? C'est la question que se pose le DAC — et la même
    /// règle que `is_dop_pcm` applique côté sortie locale.
    fn porte_un_marqueur_dop(octets: &[u8], canaux: usize, frames_min: usize) -> bool {
        let trame = 3 * canaux;
        if trame == 0 || octets.len() < trame * frames_min {
            return false;
        }
        let total = octets.len() / trame;
        for depart in 0..=total.saturating_sub(frames_min) {
            let mut precedent: Option<u8> = None;
            let mut bon = true;
            for f in depart..depart + frames_min {
                let base = f * trame;
                let marqueur = octets[base + 2];
                if marqueur != 0x05 && marqueur != 0xFA {
                    bon = false;
                    break;
                }
                if (1..canaux).any(|c| octets[base + 3 * c + 2] != marqueur) {
                    bon = false;
                    break;
                }
                if precedent == Some(marqueur) {
                    bon = false;
                    break;
                }
                precedent = Some(marqueur);
            }
            if bon {
                return true;
            }
        }
        false
    }

    fn mot24_vers_f32(octets: &[u8]) -> Vec<f32> {
        octets
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| {
                let brut = ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8;
                brut as f32 / 8_388_608.0
            })
            .collect()
    }

    fn f32_vers_mot24(echantillons: &[f32]) -> Vec<u8> {
        let mut octets = Vec::with_capacity(echantillons.len() * 3);
        for &e in echantillons {
            let mot = (e.clamp(-1.0, 1.0) * 8_388_608.0).round() as i32;
            let mot = mot.clamp(-8_388_608, 8_388_607);
            octets.push((mot & 0xFF) as u8);
            octets.push(((mot >> 8) & 0xFF) as u8);
            octets.push(((mot >> 16) & 0xFF) as u8);
        }
        octets
    }

    /// La contre-épreuve PHYSIQUE de #3233, pas une réplique du code : on
    /// fabrique un vrai porteur DoP DSD64, on lui fait subir exactement ce que
    /// la sortie locale partagée lui fait subir — mot de 24 bits → `f32` →
    /// sinc de `audio::resample` vers 48 kHz → retour en mot de 24 bits — et
    /// on constate qu'il n'en reste RIEN : plus un seul marqueur, et une
    /// amplitude effondrée. C'est « le temps défile, pas de son ».
    #[test]
    fn le_reechantillonnage_annihile_le_porteur_dop() {
        const CANAUX: usize = 2;
        // ~0,1 s de DSD64 : 17 640 trames DoP, soit 2 octets DSD par canal.
        let trames = 17_640usize;
        let mut dsd = Vec::with_capacity(trames * 2 * CANAUX);
        let mut etat: u32 = 0x1234_5678;
        for _ in 0..trames * 2 * CANAUX {
            etat = etat.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            dsd.push((etat >> 24) as u8);
        }
        let porteur = DsdToDoP::new(CANAUX, false).feed(&dsd);
        assert_eq!(porteur.len(), trames * CANAUX * 3);

        // Le point de départ : le marqueur EST là, le DAC verrouillerait.
        assert!(
            porte_un_marqueur_dop(&porteur, CANAUX, 32),
            "le porteur fabriqué ne porte pas de marqueur DoP — le témoin ne \
             prouverait rien"
        );

        let entree = mot24_vers_f32(&porteur);
        let pic_entree = entree.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            pic_entree > 0.03,
            "le porteur DoP devrait osciller autour de ±0,04 pleine échelle, \
             pic mesuré {pic_entree}"
        );

        let sortie =
            crate::audio::resample::rubato_resample_track(&entree, 176_400, 48_000, CANAUX as u16);
        assert!(!sortie.is_empty(), "le rééchantillonneur n'a rien rendu");

        let apres = f32_vers_mot24(&sortie);
        assert!(
            !porte_un_marqueur_dop(&apres, CANAUX, 32),
            "un marqueur DoP a survécu au sinc : le mécanisme de #3233 n'est \
             plus celui qu'on croit, relire le diagnostic avant de toucher au \
             correctif"
        );

        // L'alternance à fs/2 = 88,2 kHz est très au-dessus de la coupure du
        // sinc (≈ 22 kHz pour une sortie à 48 kHz) : il n'en reste qu'un
        // résidu. Le seuil est large — c'est un effondrement, pas une mesure.
        let pic_sortie = sortie.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            pic_sortie < pic_entree / 4.0,
            "le porteur DoP a traversé le sinc sans s'effondrer : entrée \
             {pic_entree}, sortie {pic_sortie}"
        );
    }
}
