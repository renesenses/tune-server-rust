//! Lecture native des images ISO SACD, sans outil externe (#5297).
//!
//! Jusqu'ici Tune ne savait lire une image SACD qu'en la confiant à
//! `sacd_extract`, un programme que l'utilisateur devait installer lui-même
//! (#3234) : sans lui, l'album n'entrait pas dans la bibliothèque. Ce module
//! lit la structure du disque et en tire le DSD **directement dans l'ISO**,
//! sans extraction ni fichier temporaire.
//!
//! ## Ce qu'il lit
//!
//! - le **Master TOC** (secteur logique 510, signature `SACDMTOC`) : version,
//!   emplacement des zones stéréo et multicanal, date, textes de l'album et du
//!   disque (secteur 511, signature `SACDText`) ;
//! - chaque **Area TOC** (`TWOCHTOC`, `MULCHTOC`) : fréquence, nombre de canaux,
//!   codage (DSD brut ou DST), puis ses secteurs annexes — `SACDTRL1` (secteur
//!   de début et longueur de chaque piste), `SACDTRL2` (code temporel de début
//!   et durée de chaque piste), `SACDTTxt` (titres, interprètes, compositeurs) ;
//! - les **secteurs audio** d'une zone DSD brut : en-tête d'un octet, table
//!   des paquets, codes temporels des trames qui commencent dans le secteur,
//!   puis les paquets eux-mêmes. Les paquets audio mis bout à bout forment les
//!   trames ; une trame dure 1/75 s et porte 4 704 octets par canal.
//!
//! ## Ce qu'il ne lit pas
//!
//! Le **DST** (DSD compressé sans perte). Une zone DST est reconnue et nommée
//! — [`Codage::Dst`] — mais aucune de ses trames n'est décodée : le décodeur
//! relève de #4378 (clause ISO/Philips du code de référence), à trancher par
//! Bertrand. Le parcours de bibliothèque garde pour ces disques le repli
//! `sacd_extract` s'il est installé, et les signale sinon.
//!
//! ## Sources consultées — descriptions du format, aucun code repris
//!
//! Le livre écarlate (Scarlet Book, la spécification SACD de Philips et Sony)
//! n'est pas public. Les décalages ci-dessous ont été établis à la lecture de :
//!
//! - **sacd-ripper**, `libs/libsacd/scarletbook.h` (disposition des
//!   structures : `master_toc_t`, `master_sacd_text_t`, `area_toc_t`,
//!   `area_tracklist_offset_t`, `area_tracklist_t`, `area_text_t`,
//!   `audio_frame_header_t`, `audio_packet_info_t`, `audio_frame_info_t`) et
//!   `scarletbook_read.c` (parcours des secteurs d'une Area TOC par signature,
//!   lecture des textes de piste, découpe des paquets d'un secteur audio). Ce
//!   projet est sous GPL-2 : il a été **lu comme une description du format**,
//!   pas recopié. Le code de ce module est écrit à neuf, dans la structure de
//!   Tune, et ne reprend ni fonction, ni table, ni commentaire de ce projet.
//! - **sacd-ripper**, `libs/libsacd/dsdiff.c` et `dsf.c` : un DSDIFF extrait
//!   d'un SACD reçoit les trames TELLES QUELLES dans son chunk `DSD `, un DSF
//!   les reçoit renversées bit à bit et désentrelacées. D'où la disposition
//!   rendue ici : octets entrelacés par canal, bit de poids fort en premier —
//!   exactement celle d'un `.dff`, que `DsdToPcmStreamer` et `DsdToDoP`
//!   connaissent déjà (`lsb_first = false`).
//! - **Philips, « DSDIFF 1.5 file format specification »** (publique, livrée
//!   avec sacd-ripper dans `docs/`) : fréquence DSD64 = 64 × 44 100 =
//!   2 822 400 Hz, un bit par échantillon, octets entrelacés par canal.
//!
//! Valeurs vérifiées dans ces sources : secteur logique de 2 048 octets,
//! Master TOC au LSN 510, 75 trames par seconde, 588 échantillons de 64 bits
//! par trame et par canal, soit 588 × 64 / 8 = 4 704 octets par trame et par
//! canal (2 822 400 / 8 / 75 = 4 704), paquet de 2 045 octets au plus.
//!
//! ## Ordre des bits des champs de bits
//!
//! `scarletbook.h` déclare ses champs de bits deux fois, pour les machines
//! gros-boutistes et petit-boutistes. Sur disque, l'ordre qui fait foi est
//! celui de la déclaration GROS-BOUTISTE lue du bit de poids fort vers le bit
//! de poids faible. Les entiers de plus d'un octet sont gros-boutistes.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Taille d'un secteur logique (LSN) d'une image SACD, en octets.
pub const TAILLE_SECTEUR: usize = 2048;

/// Secteur logique du Master TOC.
pub const LSN_MASTER_TOC: u32 = 510;

/// Fréquence d'échantillonnage du DSD d'un SACD : 64 × 44 100 Hz.
pub const FREQUENCE_DSD64: u32 = 2_822_400;

/// Trames audio par seconde.
pub const TRAMES_PAR_SECONDE: u32 = 75;

/// Octets de DSD par trame et par canal : 2 822 400 / 8 / 75.
pub const OCTETS_PAR_TRAME_ET_CANAL: usize = 4704;

/// Taille maximale d'une Area TOC, en secteurs (96 dans `scarletbook.h`).
const TAILLE_MAX_AREA_TOC: u32 = 96;

/// Nombre maximal de pistes d'une zone : les tables `SACDTRL1`/`SACDTRL2`
/// ont 255 entrées.
const PISTES_MAX: usize = 255;

/// Code `sample_frequency` de l'Area TOC pour 64 × 44,1 kHz.
const CODE_FREQUENCE_64FS: u8 = 4;

/// Motif rendu à l'utilisateur pour un disque dont l'audio est compressé DST.
///
/// Même vocabulaire que le rapport de parcours : l'utilisateur lit la même
/// phrase dans son rapport de scan et quand il clique sur l'album.
pub const MOTIF_ISO_SACD_DST: &str =
    "ISO SACD en DST (DSD compressé) : non pris en charge par la lecture native";

/// Clé de rapport des ISO SACD compressés DST que ni la lecture native ni
/// `sacd_extract` n'ont pu rendre.
pub const CLE_RAPPORT_ISO_SACD_DST: &str = "iso-sacd-dst";

/// Le codage de l'audio d'une zone (`frame_format` de l'Area TOC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codage {
    /// DSD non compressé, « 3 trames dans 14 secteurs » (`frame_format` 2).
    Dsd3Dans14,
    /// DSD non compressé, « 3 trames dans 16 secteurs » (`frame_format` 3).
    Dsd3Dans16,
    /// DSD compressé sans perte (`frame_format` 0) : non décodé ici (#4378).
    Dst,
    /// Valeur hors spécification : la zone est refusée, jamais devinée.
    Inconnu(u8),
}

impl Codage {
    fn depuis_format_de_trame(code: u8) -> Self {
        match code {
            0 => Codage::Dst,
            2 => Codage::Dsd3Dans14,
            3 => Codage::Dsd3Dans16,
            autre => Codage::Inconnu(autre),
        }
    }

    /// Vrai pour un DSD non compressé, que ce module sait rendre.
    pub fn est_dsd_brut(self) -> bool {
        matches!(self, Codage::Dsd3Dans14 | Codage::Dsd3Dans16)
    }
}

/// Stéréo (`TWOCHTOC`) ou multicanal (`MULCHTOC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDeZone {
    Stereo,
    Multicanal,
}

impl TypeDeZone {
    /// Libellé court, stable, pour le journal.
    pub fn as_str(self) -> &'static str {
        match self {
            TypeDeZone::Stereo => "stereo",
            TypeDeZone::Multicanal => "multicanal",
        }
    }
}

/// Une piste d'une zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteSacd {
    /// Numéro dans la zone, à partir de 1.
    pub numero: u32,
    /// Secteur où commence la première trame de la piste (`SACDTRL1`).
    pub debut_lsn: u32,
    /// Longueur de la piste, en secteurs (`SACDTRL1`).
    pub longueur_lsn: u32,
    /// Code temporel de début, en trames depuis le début de la zone
    /// (`SACDTRL2`). C'est l'horloge que portent aussi les secteurs audio.
    pub debut_trame: u32,
    /// Durée, en trames (`SACDTRL2`).
    pub duree_trames: u32,
    pub titre: Option<String>,
    pub interprete: Option<String>,
    pub auteur: Option<String>,
    pub compositeur: Option<String>,
    pub arrangeur: Option<String>,
}

impl PisteSacd {
    /// Début de la piste sur l'horloge de la zone, en millisecondes.
    pub fn debut_ms(&self) -> u64 {
        trames_en_ms(self.debut_trame)
    }

    /// Fin de la piste sur l'horloge de la zone, en millisecondes.
    pub fn fin_ms(&self) -> u64 {
        trames_en_ms(self.debut_trame.saturating_add(self.duree_trames))
    }
}

/// Une zone audio : stéréo ou multicanal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneSacd {
    pub genre: TypeDeZone,
    /// Premier secteur de l'Area TOC.
    pub toc_lsn: u32,
    pub version: (u8, u8),
    /// Fréquence DSD, en hertz ; `None` si le code n'est pas celui du 64 fs.
    pub frequence: Option<u32>,
    pub canaux: u8,
    pub codage: Codage,
    /// Premier et dernier secteur de l'audio de la zone (`track_start`,
    /// `track_end`), bornes comprises.
    pub audio_debut_lsn: u32,
    pub audio_fin_lsn: u32,
    /// Durée totale annoncée, en trames.
    pub duree_trames: u32,
    pub pistes: Vec<PisteSacd>,
    pub description: Option<String>,
    pub copyright: Option<String>,
}

impl ZoneSacd {
    /// Octets d'une trame, tous canaux compris.
    pub fn octets_par_trame(&self) -> usize {
        OCTETS_PAR_TRAME_ET_CANAL * self.canaux as usize
    }

    /// Cette zone se lit-elle nativement ?
    pub fn est_lisible(&self) -> bool {
        self.codage.est_dsd_brut()
            && self.frequence == Some(FREQUENCE_DSD64)
            && self.canaux > 0
            && !self.pistes.is_empty()
    }

    /// Le secteur où chercher la trame `trame`, estimé par la table des
    /// pistes. Une estimation : le lecteur recule si elle tombe trop loin.
    fn estimer_lsn(&self, trame: u32) -> u32 {
        let piste = self
            .pistes
            .iter()
            .rev()
            .find(|p| p.debut_trame <= trame)
            .or_else(|| self.pistes.first());
        let Some(p) = piste else {
            return self.audio_debut_lsn;
        };
        let ecart = u64::from(trame.saturating_sub(p.debut_trame));
        let duree = u64::from(p.duree_trames.max(1));
        let dans = (ecart * u64::from(p.longueur_lsn) / duree) as u32;
        p.debut_lsn
            .saturating_add(dans)
            .clamp(self.audio_debut_lsn, self.audio_fin_lsn)
    }
}

/// Ce que le Master TOC et ses textes disent du disque.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DisqueSacd {
    pub version: (u8, u8),
    pub album_titre: Option<String>,
    pub album_artiste: Option<String>,
    pub album_editeur: Option<String>,
    pub album_copyright: Option<String>,
    pub disque_titre: Option<String>,
    pub disque_artiste: Option<String>,
    pub catalogue: Option<String>,
    pub annee: Option<u16>,
    /// Nombre de disques de l'album et rang de celui-ci (coffret).
    pub disques_dans_l_album: u16,
    pub rang_dans_l_album: u16,
    pub zones: Vec<ZoneSacd>,
}

impl DisqueSacd {
    /// La zone que Tune lit : la stéréo si elle est en DSD brut, sinon une
    /// multicanal en DSD brut. `None` quand aucune ne l'est (disque DST).
    ///
    /// Le scan et la lecture appellent CETTE fonction : les deux choisissent
    /// donc forcément la même zone, et les bornes écrites en base décrivent
    /// les trames que la lecture rendra.
    pub fn zone_de_lecture(&self) -> Option<&ZoneSacd> {
        let lisible = |genre| {
            self.zones
                .iter()
                .find(move |z| z.genre == genre && z.est_lisible())
        };
        lisible(TypeDeZone::Stereo).or_else(|| lisible(TypeDeZone::Multicanal))
    }

    /// Le disque a-t-il au moins une zone compressée en DST ?
    pub fn porte_du_dst(&self) -> bool {
        self.zones.iter().any(|z| z.codage == Codage::Dst)
    }

    /// Le titre à afficher : celui de l'album, sinon celui du disque.
    pub fn titre(&self) -> Option<&str> {
        self.album_titre.as_deref().or(self.disque_titre.as_deref())
    }

    /// L'artiste à afficher : celui de l'album, sinon celui du disque.
    pub fn artiste(&self) -> Option<&str> {
        self.album_artiste
            .as_deref()
            .or(self.disque_artiste.as_deref())
    }
}

/// Une image SACD lisible nativement, telle que le parcours l'a lue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoSacdLu {
    pub chemin: PathBuf,
    pub disque: DisqueSacd,
}

/// Ce qu'une image `.iso` est, pour le parcours et la lecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictIso {
    /// Une zone au moins se lit nativement.
    Lisible(DisqueSacd),
    /// SACD valide, mais aucune zone en DSD brut : tout est en DST.
    Dst(DisqueSacd),
    /// Pas de signature `SACDMTOC` au LSN 510 : une image de données.
    PasUnSacd,
    /// Signature présente, structure illisible (image tronquée, abîmée).
    Illisible(String),
}

/// Examine une image : lisible, DST, données, ou abîmée.
pub fn examiner(chemin: &Path) -> VerdictIso {
    let Ok(mut fichier) = File::open(chemin) else {
        return VerdictIso::Illisible("ouverture impossible".into());
    };
    examiner_source(&mut fichier)
}

/// [`examiner`] sur une source quelconque.
pub fn examiner_source<R: Read + Seek>(source: &mut R) -> VerdictIso {
    let mut secteur = vec![0u8; TAILLE_SECTEUR];
    if lire_secteurs(source, LSN_MASTER_TOC, &mut secteur).is_err() || &secteur[..8] != b"SACDMTOC"
    {
        return VerdictIso::PasUnSacd;
    }
    match lire_disque_source(source) {
        Ok(disque) if disque.zone_de_lecture().is_some() => VerdictIso::Lisible(disque),
        Ok(disque) if disque.porte_du_dst() => VerdictIso::Dst(disque),
        Ok(_) => VerdictIso::Illisible("aucune zone audio lisible".into()),
        Err(e) => VerdictIso::Illisible(e),
    }
}

/// Lit le Master TOC, ses textes et chaque Area TOC d'une image.
pub fn lire_disque(chemin: &Path) -> Result<DisqueSacd, String> {
    let mut fichier = File::open(chemin).map_err(|e| format!("sacd: ouverture: {e}"))?;
    lire_disque_source(&mut fichier)
}

/// [`lire_disque`] sur une source quelconque.
pub fn lire_disque_source<R: Read + Seek>(source: &mut R) -> Result<DisqueSacd, String> {
    let mut mtoc = vec![0u8; TAILLE_SECTEUR];
    lire_secteurs(source, LSN_MASTER_TOC, &mut mtoc)?;
    if &mtoc[..8] != b"SACDMTOC" {
        return Err("sacd: signature SACDMTOC absente au LSN 510".into());
    }
    // Master TOC — décalages établis d'après `master_toc_t`.
    let mut disque = DisqueSacd {
        version: (mtoc[8], mtoc[9]),
        disques_dans_l_album: be16(&mtoc, 16),
        rang_dans_l_album: be16(&mtoc, 18),
        catalogue: texte_fixe(&mtoc[88..104]),
        annee: Some(be16(&mtoc, 120)).filter(|a| *a > 0),
        ..DisqueSacd::default()
    };
    let zone_1 = (be32(&mtoc, 64), be16(&mtoc, 84));
    let zone_2 = (be32(&mtoc, 72), be16(&mtoc, 86));
    // Jeu de caractères de la première langue : `locales[0]` à l'octet 136,
    // quatre octets (code langue sur deux, jeu de caractères, réservé).
    let jeu_master = mtoc[138];

    // Textes de l'album et du disque : LSN 511, première langue seule (les
    // sept suivantes, quand elles existent, traduisent les mêmes champs).
    let mut texte = vec![0u8; TAILLE_SECTEUR];
    if lire_secteurs(source, LSN_MASTER_TOC + 1, &mut texte).is_ok() && &texte[..8] == b"SACDText" {
        // Positions (u16, relatives au début du secteur) dans l'ordre de
        // `master_sacd_text_t` : titre, artiste, éditeur, copyright de
        // l'album (16..24), leurs phonétiques (24..32), puis les mêmes pour le
        // disque (32..40).
        let champ = |decalage: usize| texte_a_position(&texte, be16(&texte, decalage), jeu_master);
        disque.album_titre = champ(16);
        disque.album_artiste = champ(18);
        disque.album_editeur = champ(20);
        disque.album_copyright = champ(22);
        disque.disque_titre = champ(32);
        disque.disque_artiste = champ(34);
    }

    for (genre, (lsn, taille)) in [
        (TypeDeZone::Stereo, zone_1),
        (TypeDeZone::Multicanal, zone_2),
    ] {
        if lsn == 0 {
            continue;
        }
        disque.zones.push(lire_zone(source, genre, lsn, taille)?);
    }
    Ok(disque)
}

/// Lit une Area TOC et ses secteurs annexes.
fn lire_zone<R: Read + Seek>(
    source: &mut R,
    genre: TypeDeZone,
    lsn: u32,
    taille_annoncee: u16,
) -> Result<ZoneSacd, String> {
    let mut entete = vec![0u8; TAILLE_SECTEUR];
    lire_secteurs(source, lsn, &mut entete)?;
    let signature = &entete[..8];
    let attendue: &[u8] = match genre {
        TypeDeZone::Stereo => b"TWOCHTOC",
        TypeDeZone::Multicanal => b"MULCHTOC",
    };
    if signature != attendue {
        return Err(format!(
            "sacd: zone {} au LSN {lsn} sans signature {}",
            genre.as_str(),
            String::from_utf8_lossy(attendue)
        ));
    }
    // La taille est écrite deux fois : dans le Master TOC et dans l'en-tête
    // de la zone. La plus grande des deux, bornée, fait foi.
    let taille = u32::from(taille_annoncee.max(be16(&entete, 10))).clamp(1, TAILLE_MAX_AREA_TOC);
    let mut toc = vec![0u8; taille as usize * TAILLE_SECTEUR];
    lire_secteurs(source, lsn, &mut toc)?;

    // En-tête — décalages établis d'après `area_toc_t`.
    let code_frequence = toc[20];
    // `frame_format` occupe les quatre bits de POIDS FAIBLE de l'octet 21.
    let codage = Codage::depuis_format_de_trame(toc[21] & 0x0F);
    let canaux = toc[32];
    let duree_trames = code_temporel(&toc[64..67]);
    let nombre_pistes = (toc[69] as usize).min(PISTES_MAX);
    let audio_debut_lsn = be32(&toc, 72);
    let audio_fin_lsn = be32(&toc, 76);
    // `languages[0]` à l'octet 88 : code langue sur deux octets, puis le jeu.
    let jeu = toc[90];
    let texte_d_en_tete = |decalage: usize| texte_a_position(&toc, be16(&toc, decalage), jeu);
    let description = texte_d_en_tete(144);
    let copyright = texte_d_en_tete(146);

    let mut pistes: Vec<PisteSacd> = (0..nombre_pistes)
        .map(|i| PisteSacd {
            numero: i as u32 + 1,
            debut_lsn: 0,
            longueur_lsn: 0,
            debut_trame: 0,
            duree_trames: 0,
            titre: None,
            interprete: None,
            auteur: None,
            compositeur: None,
            arrangeur: None,
        })
        .collect();
    let (mut trl1, mut trl2, mut textes_lus) = (false, false, false);

    // Les secteurs qui suivent l'en-tête se reconnaissent à leur signature.
    // Un secteur inconnu est sauté : la zone ne se refuse pas pour une table
    // que Tune n'emploie pas.
    let mut s = 1usize;
    while s < taille as usize {
        let debut = s * TAILLE_SECTEUR;
        let secteur = &toc[debut..debut + TAILLE_SECTEUR];
        match &secteur[..8] {
            b"SACDTRL1" => {
                // `track_start_lsn[255]` à 8, `track_length_lsn[255]` à 1028.
                for (i, p) in pistes.iter_mut().enumerate() {
                    p.debut_lsn = be32(secteur, 8 + 4 * i);
                    p.longueur_lsn = be32(secteur, 8 + 4 * PISTES_MAX + 4 * i);
                }
                trl1 = true;
                s += 1;
            }
            b"SACDTRL2" => {
                // `start[255]` à 8, `duration[255]` à 1028 : minutes,
                // secondes, trames, puis un octet de drapeaux.
                for (i, p) in pistes.iter_mut().enumerate() {
                    let a = 8 + 4 * i;
                    let b = 8 + 4 * PISTES_MAX + 4 * i;
                    p.debut_trame = code_temporel(&secteur[a..a + 3]);
                    p.duree_trames = code_temporel(&secteur[b..b + 3]);
                }
                trl2 = true;
                s += 1;
            }
            b"SACDTTxt" => {
                // Seule la première langue est lue ; les suivantes répètent
                // les mêmes pistes dans une autre langue.
                if !textes_lus {
                    lire_textes_de_pistes(&toc[debut..], jeu, &mut pistes);
                    textes_lus = true;
                }
                s += 1;
            }
            _ => s += 1,
        }
    }
    if nombre_pistes > 0 && !(trl1 && trl2) {
        return Err(format!(
            "sacd: zone {} sans table des pistes (SACDTRL1/SACDTRL2)",
            genre.as_str()
        ));
    }
    Ok(ZoneSacd {
        genre,
        toc_lsn: lsn,
        version: (toc[8], toc[9]),
        frequence: (code_frequence == CODE_FREQUENCE_64FS).then_some(FREQUENCE_DSD64),
        canaux,
        codage,
        audio_debut_lsn,
        audio_fin_lsn,
        duree_trames,
        pistes,
        description,
        copyright,
    })
}

/// Les textes des pistes (`SACDTTxt`).
///
/// `bloc` commence au secteur `SACDTTxt` et court jusqu'à la fin de l'Area
/// TOC : un texte peut déborder sur les secteurs suivants. Pour chaque piste,
/// une position (u16 à `8 + 2 × i`, relative au début de ce secteur) mène à :
/// un octet « nombre d'entrées », trois octets réservés, puis les entrées —
/// un octet de type, un octet sans emploi, la chaîne terminée par un zéro.
/// Entre deux entrées, des zéros de remplissage.
fn lire_textes_de_pistes(bloc: &[u8], jeu: u8, pistes: &mut [PisteSacd]) {
    for (i, piste) in pistes.iter_mut().enumerate() {
        let position = be16(bloc, 8 + 2 * i) as usize;
        if position == 0 || position >= bloc.len() {
            continue;
        }
        let nombre = bloc[position];
        let mut curseur = position + 4;
        for _ in 0..nombre {
            if curseur + 2 > bloc.len() {
                break;
            }
            let genre = bloc[curseur];
            curseur += 2;
            let fin = bloc[curseur..]
                .iter()
                .position(|&o| o == 0)
                .map_or(bloc.len(), |n| curseur + n);
            let valeur = decoder_texte(&bloc[curseur..fin], jeu);
            match genre {
                0x01 => piste.titre = valeur,
                0x02 => piste.interprete = valeur,
                0x03 => piste.auteur = valeur,
                0x04 => piste.compositeur = valeur,
                0x05 => piste.arrangeur = valeur,
                // Messages et variantes phonétiques (0x06, 0x07, 0x81…) :
                // rien dans la bibliothèque ne les accueille.
                _ => {}
            }
            curseur = fin;
            while curseur < bloc.len() && bloc[curseur] == 0 {
                curseur += 1;
            }
        }
    }
}

/// Une trame, en trames depuis le début de la zone : minutes, secondes,
/// trames (`TIME_FRAMECOUNT`).
fn code_temporel(octets: &[u8]) -> u32 {
    u32::from(octets[0]) * 60 * TRAMES_PAR_SECONDE
        + u32::from(octets[1]) * TRAMES_PAR_SECONDE
        + u32::from(octets[2])
}

/// Trames → millisecondes, arrondi au-dessus.
///
/// Arrondi choisi pour que [`ms_en_trames`] retrouve EXACTEMENT la trame : la
/// base garde des millisecondes (`cue_start_ms`), la lecture repart d'elles,
/// et une trame perdue à l'aller serait une trame de la piste voisine au
/// retour.
pub fn trames_en_ms(trames: u32) -> u64 {
    (u64::from(trames) * 1000).div_ceil(u64::from(TRAMES_PAR_SECONDE))
}

/// Millisecondes → trame qui contient cet instant.
pub fn ms_en_trames(ms: u64) -> u32 {
    (ms * u64::from(TRAMES_PAR_SECONDE) / 1000).min(u64::from(u32::MAX)) as u32
}

fn be16(o: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([o[i], o[i + 1]])
}

fn be32(o: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([o[i], o[i + 1], o[i + 2], o[i + 3]])
}

/// Un champ de longueur fixe, complété de zéros ou d'espaces.
fn texte_fixe(octets: &[u8]) -> Option<String> {
    let fin = octets.iter().position(|&o| o == 0).unwrap_or(octets.len());
    decoder_texte(&octets[..fin], 2)
}

/// La chaîne terminée par un zéro à `position` de `bloc` ; `None` pour une
/// position nulle ou hors du bloc.
fn texte_a_position(bloc: &[u8], position: u16, jeu: u8) -> Option<String> {
    let debut = position as usize;
    if debut == 0 || debut >= bloc.len() {
        return None;
    }
    let fin = bloc[debut..]
        .iter()
        .position(|&o| o == 0)
        .map_or(bloc.len(), |n| debut + n);
    decoder_texte(&bloc[debut..fin], jeu)
}

/// Décode un texte selon le jeu de caractères du disque.
///
/// Codes (`char_set_t`) : 1 ISO 646, 2 ISO 8859-1, 3 « RIS 506 » (Shift-JIS
/// musical), 4 KS C 5601, 5 GB 2312, 6 Big5, 7 ISO 8859-1 avec échappements.
/// Les caractères de contrôle deviennent des espaces : des disques réels en
/// portent au milieu d'un nom de compositeur.
fn decoder_texte(octets: &[u8], jeu: u8) -> Option<String> {
    if octets.is_empty() {
        return None;
    }
    let brut: String = match jeu & 0x07 {
        3 => encoding_rs::SHIFT_JIS.decode(octets).0.into_owned(),
        4 => encoding_rs::EUC_KR.decode(octets).0.into_owned(),
        5 => encoding_rs::GBK.decode(octets).0.into_owned(),
        6 => encoding_rs::BIG5.decode(octets).0.into_owned(),
        // ISO 646 et ISO 8859-1 : chaque octet est son propre point de code.
        _ => octets.iter().map(|&o| o as char).collect(),
    };
    let propre: String = brut
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let propre = propre.trim();
    (!propre.is_empty()).then(|| propre.to_string())
}

/// Lit `tampon.len()` octets à partir du secteur `lsn`.
fn lire_secteurs<R: Read + Seek>(
    source: &mut R,
    lsn: u32,
    tampon: &mut [u8],
) -> Result<(), String> {
    source
        .seek(SeekFrom::Start(u64::from(lsn) * TAILLE_SECTEUR as u64))
        .map_err(|e| format!("sacd: positionnement au LSN {lsn}: {e}"))?;
    source
        .read_exact(tampon)
        .map_err(|e| format!("sacd: lecture au LSN {lsn}: {e}"))
}

// ---------------------------------------------------------------------------
// Secteurs audio
// ---------------------------------------------------------------------------

/// Type de données d'un paquet audio : audio, supplémentaire, bourrage.
const PAQUET_AUDIO: u8 = 2;

/// Un paquet d'un secteur audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Paquet {
    /// Le paquet ouvre-t-il une trame ?
    debut_de_trame: bool,
    genre: u8,
    /// Décalage des données dans le secteur, et longueur.
    decalage: usize,
    longueur: usize,
}

/// Ce que l'en-tête d'un secteur audio annonce.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SecteurAudio {
    dst: bool,
    paquets: Vec<Paquet>,
    /// Code temporel (en trames) de chaque trame qui COMMENCE dans ce
    /// secteur, dans l'ordre des paquets marqués « début de trame ».
    trames: Vec<u32>,
}

/// Découpe un secteur audio.
///
/// Octet 0, du bit de poids fort au bit de poids faible : nombre de paquets
/// (3 bits), nombre de débuts de trame (3 bits), un bit réservé, le drapeau
/// DST. Puis deux octets par paquet : début de trame (1 bit), réservé (1),
/// type (3), longueur (11). Puis, par début de trame, son code temporel —
/// trois octets (minutes, secondes, trames), quatre en DST. Puis les données
/// des paquets, dans l'ordre.
fn analyser_secteur(secteur: &[u8]) -> Result<SecteurAudio, String> {
    let entete = secteur[0];
    let dst = entete & 0x01 != 0;
    let nb_trames = ((entete >> 2) & 0x07) as usize;
    let nb_paquets = ((entete >> 5) & 0x07) as usize;
    let mut curseur = 1usize;
    let mut infos = Vec::with_capacity(nb_paquets);
    for _ in 0..nb_paquets {
        let (a, b) = (secteur[curseur], secteur[curseur + 1]);
        infos.push((
            a & 0x80 != 0,
            (a >> 3) & 0x07,
            ((usize::from(a) & 0x07) << 8) | usize::from(b),
        ));
        curseur += 2;
    }
    let taille_info = if dst { 4 } else { 3 };
    let mut trames = Vec::with_capacity(nb_trames);
    for _ in 0..nb_trames {
        trames.push(code_temporel(&secteur[curseur..curseur + 3]));
        curseur += taille_info;
    }
    let mut paquets = Vec::with_capacity(nb_paquets);
    for (debut_de_trame, genre, longueur) in infos {
        if curseur + longueur > secteur.len() {
            return Err("sacd: paquet audio qui déborde de son secteur".into());
        }
        paquets.push(Paquet {
            debut_de_trame,
            genre,
            decalage: curseur,
            longueur,
        });
        curseur += longueur;
    }
    Ok(SecteurAudio {
        dst,
        paquets,
        trames,
    })
}

/// Marge, en secteurs, prise avant le secteur estimé d'une trame.
const MARGE_DE_RECHERCHE: u32 = 8;

/// Tentatives de recul quand l'estimation est tombée après la trame visée.
const RECULS_MAX: u32 = 12;

/// Le DSD d'une zone, lu directement dans l'image, trame après trame.
///
/// Rend des blocs d'octets entrelacés par canal, bit de poids fort en premier
/// — la disposition d'un `.dff` : les convertisseurs existants s'en servent
/// avec `lsb_first = false`. Chaque bloc est un multiple du nombre de canaux.
///
/// Le lecteur part de la trame demandée — début d'une piste, ou un instant
/// dans la piste pour un déplacement — et s'arrête à la trame de fin donnée,
/// sinon à la fin de la zone.
pub struct LecteurDsdSacd<R: Read + Seek = File> {
    /// Lecture tamponnée : les secteurs se lisent l'un après l'autre, et un
    /// secteur de 2 Kio par appel système coûterait cher sur un partage
    /// réseau. Le positionnement n'est refait que sur un saut.
    source: BufReader<R>,
    /// Secteur sur lequel `source` est positionnée, s'il est connu.
    position: Option<u32>,
    canaux: usize,
    octets_par_trame: usize,
    lsn: u32,
    fin_lsn: u32,
    /// Trame de départ (horloge de la zone) et trame de fin exclue.
    cible: u32,
    fin_trame: Option<u32>,
    /// Trame par laquelle la lecture a réellement commencé.
    trame_atteinte: Option<u32>,
    /// Octets encore à rendre, une fois calé ; `None` : jusqu'à la fin.
    restant: Option<u64>,
    /// Un premier début de trame a-t-il été vu depuis le positionnement ?
    premier_vu: bool,
    tampon: Vec<u8>,
    taille_bloc: usize,
    secteur: Vec<u8>,
    fini: bool,
}

/// Tampon de lecture des secteurs audio : 32 secteurs, un peu moins de
/// sept trames stéréo.
const TAILLE_TAMPON_DE_LECTURE: usize = 32 * TAILLE_SECTEUR;

/// Taille visée d'un bloc rendu : trois trames stéréo, soit 40 ms.
const TAILLE_BLOC: usize = 3 * 2 * OCTETS_PAR_TRAME_ET_CANAL;

impl LecteurDsdSacd<File> {
    /// Ouvre la zone `zone` de l'image `chemin`, de `debut_ms` à `fin_ms`
    /// (horloge de la zone, en millisecondes).
    pub fn ouvrir(
        chemin: &Path,
        zone: &ZoneSacd,
        debut_ms: u64,
        fin_ms: Option<u64>,
    ) -> Result<Self, String> {
        let fichier = File::open(chemin).map_err(|e| format!("sacd: ouverture: {e}"))?;
        Self::depuis_source(
            fichier,
            zone,
            ms_en_trames(debut_ms),
            fin_ms.map(ms_en_trames),
        )
    }
}

impl<R: Read + Seek> LecteurDsdSacd<R> {
    /// Ouvre une zone sur une source quelconque, de la trame `debut` à la
    /// trame `fin` exclue.
    pub fn depuis_source(
        source: R,
        zone: &ZoneSacd,
        debut: u32,
        fin: Option<u32>,
    ) -> Result<Self, String> {
        match zone.codage {
            Codage::Dst => return Err(MOTIF_ISO_SACD_DST.into()),
            Codage::Inconnu(c) => return Err(format!("sacd: codage de zone inconnu ({c})")),
            _ => {}
        }
        if !zone.est_lisible() {
            return Err("sacd: zone sans DSD 64 lisible".into());
        }
        let mut lecteur = LecteurDsdSacd {
            source: BufReader::with_capacity(TAILLE_TAMPON_DE_LECTURE, source),
            position: None,
            canaux: zone.canaux as usize,
            octets_par_trame: zone.octets_par_trame(),
            lsn: zone.audio_debut_lsn,
            fin_lsn: zone.audio_fin_lsn,
            cible: debut,
            fin_trame: fin,
            trame_atteinte: None,
            restant: None,
            premier_vu: false,
            tampon: Vec::new(),
            taille_bloc: TAILLE_BLOC / 2 * zone.canaux as usize,
            secteur: vec![0u8; TAILLE_SECTEUR],
            fini: false,
        };
        lecteur.se_caler(zone)?;
        Ok(lecteur)
    }

    /// La trame par laquelle la lecture commence vraiment (horloge de la
    /// zone). Égale à la trame demandée sauf si celle-ci précède la zone.
    pub fn trame_atteinte(&self) -> Option<u32> {
        self.trame_atteinte
    }

    /// Nombre de canaux des blocs rendus.
    pub fn canaux(&self) -> usize {
        self.canaux
    }

    /// Place le lecteur sur la trame visée.
    ///
    /// L'estimation par la table des pistes est d'ordinaire juste à un
    /// secteur près ; on part un peu avant et on avance jusqu'au début de la
    /// trame visée. Si le premier début de trame rencontré est déjà APRÈS
    /// elle, l'estimation était trop loin : on recule, de plus en plus.
    fn se_caler(&mut self, zone: &ZoneSacd) -> Result<(), String> {
        let mut depart = zone
            .estimer_lsn(self.cible)
            .saturating_sub(MARGE_DE_RECHERCHE)
            .max(zone.audio_debut_lsn);
        let mut recul = 32u32;
        for _ in 0..RECULS_MAX {
            self.lsn = depart;
            self.premier_vu = false;
            self.tampon.clear();
            self.trame_atteinte = None;
            self.restant = None;
            self.fini = false;
            match self.avancer_jusqu_au_calage()? {
                Calage::Cale | Calage::FinDeZone => return Ok(()),
                Calage::TropLoin if depart > zone.audio_debut_lsn => {
                    depart = depart.saturating_sub(recul).max(zone.audio_debut_lsn);
                    recul = recul.saturating_mul(2);
                }
                // Déjà au tout début de la zone : la trame visée précède la
                // première trame du disque, on part de celle-ci. Le contrôle
                // « trop loin » est levé ; le calage prend alors la première
                // trame au moins égale à la cible, soit la toute première.
                Calage::TropLoin => {
                    self.lsn = zone.audio_debut_lsn;
                    self.premier_vu = true;
                    self.tampon.clear();
                    return match self.avancer_jusqu_au_calage()? {
                        Calage::TropLoin => Err("sacd: calage impossible".into()),
                        _ => Ok(()),
                    };
                }
            }
        }
        Err("sacd: trame introuvable dans la zone".into())
    }

    /// Lit des secteurs jusqu'à être calé sur la trame visée.
    fn avancer_jusqu_au_calage(&mut self) -> Result<Calage, String> {
        while self.trame_atteinte.is_none() {
            if self.lsn > self.fin_lsn {
                self.fini = true;
                return Ok(Calage::FinDeZone);
            }
            if let Some(calage) = self.lire_un_secteur()? {
                return Ok(calage);
            }
        }
        Ok(Calage::Cale)
    }

    /// Lit un secteur et range ses octets audio utiles dans le tampon.
    ///
    /// Rend `Some(TropLoin)` quand la première trame vue depuis le
    /// positionnement est déjà après la trame visée.
    fn lire_un_secteur(&mut self) -> Result<Option<Calage>, String> {
        let lsn = self.lsn;
        if self.position != Some(lsn) {
            self.source
                .seek(SeekFrom::Start(u64::from(lsn) * TAILLE_SECTEUR as u64))
                .map_err(|e| format!("sacd: positionnement au LSN {lsn}: {e}"))?;
        }
        self.position = None;
        self.source
            .read_exact(&mut self.secteur)
            .map_err(|e| format!("sacd: lecture au LSN {lsn}: {e}"))?;
        self.position = Some(lsn + 1);
        self.lsn += 1;
        let analyse = analyser_secteur(&self.secteur).map_err(|e| format!("{e} (LSN {lsn})"))?;
        if analyse.dst {
            return Err(MOTIF_ISO_SACD_DST.into());
        }
        let mut rang_trame = 0usize;
        for paquet in &analyse.paquets {
            if paquet.genre != PAQUET_AUDIO {
                continue;
            }
            if paquet.debut_de_trame {
                let code = analyse.trames.get(rang_trame).copied();
                rang_trame += 1;
                if self.trame_atteinte.is_none() {
                    let Some(code) = code else {
                        return Err(format!(
                            "sacd: début de trame sans code temporel (LSN {lsn})"
                        ));
                    };
                    let premier = !self.premier_vu;
                    self.premier_vu = true;
                    if code > self.cible && premier {
                        return Ok(Some(Calage::TropLoin));
                    }
                    if code >= self.cible {
                        self.trame_atteinte = Some(code);
                        self.restant = self.fin_trame.map(|fin| {
                            u64::from(fin.saturating_sub(code)) * self.octets_par_trame as u64
                        });
                    }
                }
            }
            if self.trame_atteinte.is_none() {
                continue;
            }
            let donnees = &self.secteur[paquet.decalage..paquet.decalage + paquet.longueur];
            let utile = match self.restant.as_mut() {
                Some(reste) => {
                    let n = (*reste).min(donnees.len() as u64) as usize;
                    *reste -= n as u64;
                    &donnees[..n]
                }
                None => donnees,
            };
            self.tampon.extend_from_slice(utile);
            if self.restant == Some(0) {
                self.fini = true;
                break;
            }
        }
        Ok(None)
    }

    /// Le bloc suivant d'octets DSD entrelacés, ou `None` à la fin.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        while !self.fini && self.tampon.len() < self.taille_bloc {
            if self.lsn > self.fin_lsn {
                self.fini = true;
                break;
            }
            self.lire_un_secteur()?;
        }
        if self.tampon.is_empty() {
            return Ok(None);
        }
        // Un multiple du nombre de canaux : couper au milieu d'un groupe
        // d'octets L/R déphaserait les canaux pour tout le reste du flux.
        let prendre = if self.fini {
            self.tampon.len() - self.tampon.len() % self.canaux
        } else {
            let n = self.tampon.len().min(self.taille_bloc);
            n - n % self.canaux
        };
        if prendre == 0 {
            self.tampon.clear();
            return Ok(None);
        }
        let reste = self.tampon.split_off(prendre);
        Ok(Some(std::mem::replace(&mut self.tampon, reste)))
    }
}

enum Calage {
    Cale,
    TropLoin,
    FinDeZone,
}

// ---------------------------------------------------------------------------
// Pour les chemins de lecture : un `.iso` joué comme une tranche
// ---------------------------------------------------------------------------

/// Est-ce un chemin d'image `.iso` ?
pub fn est_extension_iso(chemin: &Path) -> bool {
    chemin
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// Fréquence DSD et canaux de la zone que Tune lit dans cette image.
pub fn parametres_de_lecture(chemin: &Path) -> Result<(u32, usize), String> {
    let disque = lire_disque(chemin)?;
    let zone = zone_lisible(&disque)?;
    Ok((FREQUENCE_DSD64, zone.canaux as usize))
}

/// La zone lue, ou le motif du refus.
pub fn zone_lisible(disque: &DisqueSacd) -> Result<&ZoneSacd, String> {
    match disque.zone_de_lecture() {
        Some(z) => Ok(z),
        None if disque.porte_du_dst() => Err(MOTIF_ISO_SACD_DST.into()),
        None => Err("sacd: aucune zone DSD lisible".into()),
    }
}

/// Ouvre la lecture de l'image à `debut_ms` (horloge de la zone), jusqu'à
/// `fin_ms` ou la fin de la zone.
///
/// La piste virtuelle d'un ISO est une TRANCHE, comme celle d'une feuille
/// CUE : `cue_media_path` désigne l'image, `cue_start_ms`/`cue_end_ms` la
/// piste sur l'horloge de la zone. Les chemins de lecture transmettent déjà
/// « début de la tranche + déplacement de l'utilisateur » : c'est ce `debut_ms`.
pub fn ouvrir_lecture(
    chemin: &Path,
    debut_ms: u64,
    fin_ms: Option<u64>,
) -> Result<LecteurDsdSacd<File>, String> {
    let disque = lire_disque(chemin)?;
    let zone = zone_lisible(&disque)?;
    LecteurDsdSacd::ouvrir(chemin, zone, debut_ms, fin_ms)
}

/// Motif qui interdit de DÉCODER cette image, ou `None` si Tune la lit.
///
/// Distinct de [`super::iso_sacd::refus_de_lecture`], qui garde la route des
/// octets BRUTS : celle-là ne doit jamais servir une image disque, même
/// lisible — un renderer recevrait 4 Go d'ISO. Ici, la question est « le
/// décodeur sait-il en tirer du son ? ».
pub fn refus_de_decodage(chemin: &Path) -> Option<&'static str> {
    if !est_extension_iso(chemin) {
        return None;
    }
    match examiner(chemin) {
        VerdictIso::Lisible(_) => None,
        VerdictIso::Dst(_) => Some(MOTIF_ISO_SACD_DST),
        VerdictIso::PasUnSacd => Some(super::iso_sacd::MOTIF_ISO_SANS_ZONE_SACD),
        VerdictIso::Illisible(_) => Some(super::iso_sacd::MOTIF_ISO_SACD_NON_EXTRAIT),
    }
}

// ---------------------------------------------------------------------------
// Fabrique d'images synthétiques, pour les épreuves
// ---------------------------------------------------------------------------

/// Fabrique de petites images SACD, pour les épreuves de ce module et des
/// modules qui s'en servent (parcours, décodage, lecture).
///
/// L'image est écrite d'après la même description du format que le lecteur,
/// mais par un code indépendant : l'écrivain place les paquets, le lecteur
/// doit les retrouver. Les pistes contiennent un DSD CONNU
/// ([`fabrique::octet_de_trame`]), si bien qu'une épreuve compare des octets,
/// pas seulement des longueurs.
#[cfg(test)]
pub(crate) mod fabrique {
    use super::*;
    use std::io::Write;

    /// Une piste fabriquée.
    pub(crate) struct PisteFabriquee {
        pub titre: &'static str,
        pub interprete: &'static str,
        pub compositeur: &'static str,
        pub trames: u32,
    }

    /// Une image fabriquée.
    pub(crate) struct ImageFabriquee {
        pub album: &'static str,
        pub artiste: &'static str,
        pub annee: u16,
        pub pistes: Vec<PisteFabriquee>,
        /// Zone stéréo compressée en DST (aucune trame audio n'est écrite).
        pub stereo_dst: bool,
        /// Ajoute une zone multicanal DST, vide de trames.
        pub multicanal_dst: bool,
        /// Gonfle `track_length_lsn` pour fausser l'estimation de position.
        pub longueurs_faussees: bool,
        /// Code temporel de la première trame de la zone.
        pub premiere_trame: u32,
    }

    impl ImageFabriquee {
        /// L'image de référence : deux pistes stéréo en DSD brut.
        pub(crate) fn deux_pistes() -> Self {
            ImageFabriquee {
                album: "Kind of Blue",
                artiste: "Miles Davis",
                annee: 1959,
                pistes: vec![
                    PisteFabriquee {
                        titre: "So What",
                        interprete: "Miles Davis Sextet",
                        compositeur: "Miles Davis",
                        trames: 3,
                    },
                    PisteFabriquee {
                        titre: "Freddie Freeloader",
                        interprete: "Miles Davis",
                        compositeur: "Miles Davis",
                        trames: 7,
                    },
                ],
                stereo_dst: false,
                multicanal_dst: false,
                longueurs_faussees: false,
                premiere_trame: 0,
            }
        }
    }

    /// L'octet `i` de la trame audio de code temporel `trame`.
    pub(crate) fn octet_de_trame(trame: u32, i: usize) -> u8 {
        let x = (trame as usize).wrapping_mul(2_654_435_761) ^ i.wrapping_mul(40_503);
        (x ^ (x >> 7) ^ (x >> 13)) as u8
    }

    /// Les octets DSD attendus des trames `[debut, fin)`, deux canaux.
    pub(crate) fn octets_attendus(debut: u32, fin: u32) -> Vec<u8> {
        let taille = 2 * OCTETS_PAR_TRAME_ET_CANAL;
        let mut v = Vec::with_capacity((fin - debut) as usize * taille);
        for t in debut..fin {
            v.extend((0..taille).map(|i| octet_de_trame(t, i)));
        }
        v
    }

    const LSN_ZONE_STEREO: u32 = 540;
    const LSN_ZONE_MULTI: u32 = 560;
    const TAILLE_ZONE: u16 = 5;
    const LSN_AUDIO: u32 = 600;

    fn poser16(s: &mut [u8], i: usize, v: u16) {
        s[i..i + 2].copy_from_slice(&v.to_be_bytes());
    }
    fn poser32(s: &mut [u8], i: usize, v: u32) {
        s[i..i + 4].copy_from_slice(&v.to_be_bytes());
    }
    fn poser_tc(s: &mut [u8], i: usize, trames: u32) {
        s[i] = (trames / (60 * 75)) as u8;
        s[i + 1] = (trames / 75 % 60) as u8;
        s[i + 2] = (trames % 75) as u8;
    }

    /// Place les trames `[premiere, premiere + n)` dans des secteurs audio à
    /// partir de `LSN_AUDIO`. Rend les secteurs et, par trame, le secteur où
    /// elle commence et celui où elle finit.
    fn secteurs_audio(premiere: u32, n: u32) -> (Vec<[u8; TAILLE_SECTEUR]>, Vec<(u32, u32)>) {
        let taille_trame = 2 * OCTETS_PAR_TRAME_ET_CANAL;
        let mut secteurs = Vec::new();
        let mut places = Vec::new();
        let (mut trame, mut dans) = (0u32, 0usize);
        let mut debut_courant = 0u32;
        while trame < n {
            let lsn = LSN_AUDIO + secteurs.len() as u32;
            // Paquets (début de trame ?, longueur) et codes des trames.
            let mut paquets: Vec<(bool, usize, u32, usize)> = Vec::new();
            let mut codes: Vec<u32> = Vec::new();
            while trame < n && paquets.len() < 7 {
                let debut = dans == 0;
                if debut && codes.len() == 7 {
                    break;
                }
                let entete = 1 + 2 * (paquets.len() + 1) + 3 * (codes.len() + usize::from(debut));
                let donnees: usize = paquets.iter().map(|p| p.1).sum();
                let libre = TAILLE_SECTEUR as isize - entete as isize - donnees as isize;
                if libre <= 0 {
                    break;
                }
                let longueur = (libre as usize).min(2045).min(taille_trame - dans);
                if debut {
                    codes.push(premiere + trame);
                    debut_courant = lsn;
                }
                paquets.push((debut, longueur, trame, dans));
                dans += longueur;
                if dans == taille_trame {
                    places.push((debut_courant, lsn));
                    trame += 1;
                    dans = 0;
                }
            }
            let mut s = [0u8; TAILLE_SECTEUR];
            s[0] = ((paquets.len() as u8) << 5) | ((codes.len() as u8) << 2);
            let mut c = 1;
            for &(debut, longueur, _, _) in &paquets {
                s[c] = (u8::from(debut) << 7) | (PAQUET_AUDIO << 3) | ((longueur >> 8) as u8);
                s[c + 1] = longueur as u8;
                c += 2;
            }
            for &code in &codes {
                poser_tc(&mut s, c, code);
                c += 3;
            }
            for &(_, longueur, t, depuis) in &paquets {
                for k in 0..longueur {
                    s[c + k] = octet_de_trame(premiere + t, depuis + k);
                }
                c += longueur;
            }
            secteurs.push(s);
        }
        (secteurs, places)
    }

    fn texte_c(s: &mut [u8], position: usize, texte: &str) -> usize {
        s[position..position + texte.len()].copy_from_slice(texte.as_bytes());
        position + texte.len() + 1
    }

    fn area_toc(
        image: &ImageFabriquee,
        signature: &[u8; 8],
        canaux: u8,
        dst: bool,
        pistes: &[(u32, u32, u32, u32)],
        audio: (u32, u32),
    ) -> Vec<u8> {
        let mut toc = vec![0u8; TAILLE_ZONE as usize * TAILLE_SECTEUR];
        toc[..8].copy_from_slice(signature);
        toc[8] = 1;
        toc[9] = 20;
        poser16(&mut toc, 10, TAILLE_ZONE);
        toc[20] = CODE_FREQUENCE_64FS;
        toc[21] = if dst { 0 } else { 2 };
        toc[32] = canaux;
        let total: u32 = image.pistes.iter().map(|p| p.trames).sum();
        poser_tc(&mut toc, 64, total);
        toc[69] = image.pistes.len() as u8;
        poser32(&mut toc, 72, audio.0);
        poser32(&mut toc, 76, audio.1);
        toc[90] = 2; // ISO 8859-1
        // Copyright de la zone, à une position de l'en-tête.
        poser16(&mut toc, 146, 400);
        texte_c(&mut toc, 400, "(P) 1959 Columbia");

        let trl1 = TAILLE_SECTEUR;
        toc[trl1..trl1 + 8].copy_from_slice(b"SACDTRL1");
        let trl2 = 2 * TAILLE_SECTEUR;
        toc[trl2..trl2 + 8].copy_from_slice(b"SACDTRL2");
        for (i, &(lsn, longueur, debut, duree)) in pistes.iter().enumerate() {
            let longueur = if image.longueurs_faussees {
                longueur * 40
            } else {
                longueur
            };
            poser32(&mut toc, trl1 + 8 + 4 * i, lsn);
            poser32(&mut toc, trl1 + 8 + 4 * 255 + 4 * i, longueur);
            poser_tc(&mut toc, trl2 + 8 + 4 * i, debut);
            poser_tc(&mut toc, trl2 + 8 + 4 * 255 + 4 * i, duree);
        }
        let ttxt = 3 * TAILLE_SECTEUR;
        toc[ttxt..ttxt + 8].copy_from_slice(b"SACDTTxt");
        let mut position = 8 + 2 * 255;
        for (i, p) in image.pistes.iter().enumerate() {
            poser16(&mut toc, ttxt + 8 + 2 * i, position as u16);
            let bloc = ttxt + position;
            toc[bloc] = 3; // trois entrées
            let mut c = bloc + 4;
            for (genre, texte) in [
                (0x01u8, p.titre),
                (0x02, p.interprete),
                (0x04, p.compositeur),
            ] {
                toc[c] = genre;
                toc[c + 1] = 0x20;
                c = texte_c(&mut toc, c + 2, texte);
                // Remplissage de zéros entre deux entrées, comme sur disque.
                c += 1;
            }
            position = c - ttxt;
        }
        toc
    }

    /// Écrit l'image dans `chemin`. Rend, par piste, les trames `[début, fin)`
    /// sur l'horloge de la zone.
    pub(crate) fn ecrire(chemin: &Path, image: &ImageFabriquee) -> Vec<(u32, u32)> {
        let total: u32 = image.pistes.iter().map(|p| p.trames).sum();
        let (secteurs, places) = if image.stereo_dst {
            (Vec::new(), Vec::new())
        } else {
            secteurs_audio(image.premiere_trame, total)
        };
        let mut bornes = Vec::new();
        let mut pistes = Vec::new();
        let mut t = 0u32;
        for p in &image.pistes {
            let (debut, fin) = (t, t + p.trames);
            let (lsn, longueur) = match (places.get(debut as usize), places.get(fin as usize - 1)) {
                (Some(a), Some(b)) => (a.0, b.1 - a.0 + 1),
                _ => (LSN_AUDIO, 1),
            };
            pistes.push((lsn, longueur, image.premiere_trame + debut, p.trames));
            bornes.push((image.premiere_trame + debut, image.premiere_trame + fin));
            t = fin;
        }
        let audio = (LSN_AUDIO, LSN_AUDIO + (secteurs.len() as u32).max(1) - 1);

        let mut mtoc = vec![0u8; TAILLE_SECTEUR];
        mtoc[..8].copy_from_slice(b"SACDMTOC");
        mtoc[8] = 1;
        mtoc[9] = 20;
        poser16(&mut mtoc, 16, 1);
        poser16(&mut mtoc, 18, 1);
        poser32(&mut mtoc, 64, LSN_ZONE_STEREO);
        poser16(&mut mtoc, 84, TAILLE_ZONE);
        if image.multicanal_dst {
            poser32(&mut mtoc, 72, LSN_ZONE_MULTI);
            poser16(&mut mtoc, 86, TAILLE_ZONE);
        }
        mtoc[88..96].copy_from_slice(b"CK 64935");
        poser16(&mut mtoc, 120, image.annee);
        mtoc[128] = 1;
        mtoc[136] = b'e';
        mtoc[137] = b'n';
        mtoc[138] = 2;

        let mut texte = vec![0u8; TAILLE_SECTEUR];
        texte[..8].copy_from_slice(b"SACDText");
        poser16(&mut texte, 16, 64);
        let apres = texte_c(&mut texte, 64, image.album);
        poser16(&mut texte, 18, apres as u16);
        let apres = texte_c(&mut texte, apres, image.artiste);
        // Titre du disque, distinct, pour vérifier qu'il n'écrase pas l'album.
        poser16(&mut texte, 32, apres as u16);
        texte_c(&mut texte, apres, "Disque 1");

        let stereo = area_toc(image, b"TWOCHTOC", 2, image.stereo_dst, &pistes, audio);
        let multi = area_toc(image, b"MULCHTOC", 6, true, &pistes, audio);

        let mut f = File::create(chemin).unwrap();
        let mut ecrire_a = |lsn: u32, octets: &[u8]| {
            f.seek(SeekFrom::Start(u64::from(lsn) * TAILLE_SECTEUR as u64))
                .unwrap();
            f.write_all(octets).unwrap();
        };
        ecrire_a(LSN_MASTER_TOC, &mtoc);
        ecrire_a(LSN_MASTER_TOC + 1, &texte);
        ecrire_a(LSN_ZONE_STEREO, &stereo);
        if image.multicanal_dst {
            ecrire_a(LSN_ZONE_MULTI, &multi);
        }
        for (i, s) in secteurs.iter().enumerate() {
            ecrire_a(LSN_AUDIO + i as u32, s);
        }
        drop(ecrire_a);
        // Un secteur de bourrage en fin d'image, comme la zone de sortie
        // d'un disque : la dernière trame ne touche pas la fin du fichier.
        let fin = u64::from(audio.1 + 2) * TAILLE_SECTEUR as u64;
        f.set_len(fin).unwrap();
        f.flush().unwrap();
        bornes
    }
}

#[cfg(test)]
mod tests {
    use super::fabrique::*;
    use super::*;

    fn tout_lire<R: Read + Seek>(lecteur: &mut LecteurDsdSacd<R>) -> Vec<u8> {
        let mut v = Vec::new();
        while let Some(bloc) = lecteur.next_chunk().unwrap() {
            assert_eq!(bloc.len() % 2, 0, "un bloc coupe un groupe L/R");
            v.extend(bloc);
        }
        v
    }

    fn image(image: &ImageFabriquee) -> (tempfile::TempDir, PathBuf, Vec<(u32, u32)>) {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("Kind of Blue.iso");
        let bornes = ecrire(&chemin, image);
        (dossier, chemin, bornes)
    }

    /// Les sommaires : Master TOC, zone stéréo, table des pistes.
    #[test]
    fn les_sommaires_d_une_image_se_lisent() {
        let (_d, chemin, bornes) = image(&ImageFabriquee::deux_pistes());
        let disque = lire_disque(&chemin).expect("image lisible");
        assert_eq!(disque.version, (1, 20));
        assert_eq!(disque.annee, Some(1959));
        assert_eq!(disque.catalogue.as_deref(), Some("CK 64935"));
        assert_eq!(disque.zones.len(), 1);
        let zone = &disque.zones[0];
        assert_eq!(zone.genre, TypeDeZone::Stereo);
        assert_eq!(zone.frequence, Some(FREQUENCE_DSD64));
        assert_eq!(zone.canaux, 2);
        assert_eq!(zone.codage, Codage::Dsd3Dans14);
        assert_eq!(zone.duree_trames, 10);
        assert_eq!(zone.pistes.len(), 2);
        for (piste, (debut, fin)) in zone.pistes.iter().zip(&bornes) {
            assert_eq!(piste.debut_trame, *debut);
            assert_eq!(piste.duree_trames, fin - debut);
            assert!(piste.longueur_lsn > 0);
            assert!(piste.debut_lsn >= zone.audio_debut_lsn);
        }
        assert!(
            zone.pistes[1].debut_lsn <= zone.pistes[0].debut_lsn + zone.pistes[0].longueur_lsn,
            "la piste 2 commence dans le dernier secteur de la piste 1 ou juste après"
        );
        assert_eq!(disque.zone_de_lecture(), Some(zone));
    }

    /// Les textes : album, artiste, et pour chaque piste titre, interprète,
    /// compositeur.
    #[test]
    fn les_textes_de_l_album_et_des_pistes_se_lisent() {
        let (_d, chemin, _) = image(&ImageFabriquee::deux_pistes());
        let disque = lire_disque(&chemin).unwrap();
        assert_eq!(disque.album_titre.as_deref(), Some("Kind of Blue"));
        assert_eq!(disque.album_artiste.as_deref(), Some("Miles Davis"));
        assert_eq!(disque.disque_titre.as_deref(), Some("Disque 1"));
        assert_eq!(disque.titre(), Some("Kind of Blue"));
        let zone = &disque.zones[0];
        assert_eq!(zone.copyright.as_deref(), Some("(P) 1959 Columbia"));
        let p1 = &zone.pistes[0];
        assert_eq!(p1.titre.as_deref(), Some("So What"));
        assert_eq!(p1.interprete.as_deref(), Some("Miles Davis Sextet"));
        assert_eq!(p1.compositeur.as_deref(), Some("Miles Davis"));
        assert_eq!(zone.pistes[1].titre.as_deref(), Some("Freddie Freeloader"));
    }

    /// Les octets DSD de chaque piste, exactement : ni une trame de la piste
    /// voisine, ni une trame perdue au passage d'un secteur partagé.
    #[test]
    fn chaque_piste_rend_exactement_ses_octets_dsd() {
        let (_d, chemin, bornes) = image(&ImageFabriquee::deux_pistes());
        let disque = lire_disque(&chemin).unwrap();
        let zone = disque.zone_de_lecture().unwrap();
        for (piste, &(debut, fin)) in zone.pistes.iter().zip(&bornes) {
            let mut lecteur =
                LecteurDsdSacd::ouvrir(&chemin, zone, piste.debut_ms(), Some(piste.fin_ms()))
                    .unwrap();
            assert_eq!(lecteur.trame_atteinte(), Some(debut));
            let lu = tout_lire(&mut lecteur);
            assert_eq!(
                lu.len(),
                (fin - debut) as usize * 2 * OCTETS_PAR_TRAME_ET_CANAL
            );
            assert!(
                lu == octets_attendus(debut, fin),
                "piste {} : octets DSD différents de ceux écrits",
                piste.numero
            );
        }
        // Sans borne de fin, la lecture court jusqu'au bout de la zone.
        let mut tout = LecteurDsdSacd::ouvrir(&chemin, zone, 0, None).unwrap();
        assert!(tout_lire(&mut tout) == octets_attendus(0, 10));
    }

    /// Le déplacement : partir d'un instant DANS la piste rend la suite de
    /// la piste, à la trame près.
    #[test]
    fn le_deplacement_dans_une_piste_part_de_la_bonne_trame() {
        let (_d, chemin, bornes) = image(&ImageFabriquee::deux_pistes());
        let disque = lire_disque(&chemin).unwrap();
        let zone = disque.zone_de_lecture().unwrap();
        let (debut, fin) = bornes[1];
        for decalage in 1..(fin - debut) {
            let ms = trames_en_ms(debut + decalage);
            let mut lecteur = ouvrir_lecture(&chemin, ms, Some(trames_en_ms(fin))).unwrap();
            assert_eq!(lecteur.trame_atteinte(), Some(debut + decalage));
            assert!(tout_lire(&mut lecteur) == octets_attendus(debut + decalage, fin));
        }
        // Un instant au milieu d'une trame part de cette trame.
        let milieu = trames_en_ms(debut + 2) + 5;
        let lecteur = ouvrir_lecture(&chemin, milieu, None).unwrap();
        assert_eq!(lecteur.trame_atteinte(), Some(debut + 2));
        let _ = zone;
    }

    /// Une table des pistes qui ment sur les longueurs fausse l'estimation :
    /// le lecteur doit reculer, pas rendre une autre trame.
    #[test]
    fn une_estimation_trop_lointaine_est_rattrapee_en_reculant() {
        let mut fausse = ImageFabriquee::deux_pistes();
        fausse.longueurs_faussees = true;
        let (_d, chemin, bornes) = image(&fausse);
        let (debut, fin) = bornes[1];
        let mut lecteur =
            ouvrir_lecture(&chemin, trames_en_ms(debut + 3), Some(trames_en_ms(fin))).unwrap();
        assert_eq!(lecteur.trame_atteinte(), Some(debut + 3));
        assert!(tout_lire(&mut lecteur) == octets_attendus(debut + 3, fin));
    }

    /// Une zone qui ne commence pas à 00:00:00 : l'horloge des pistes est
    /// celle des secteurs, pas un compteur remis à zéro.
    #[test]
    fn l_horloge_de_la_zone_peut_ne_pas_partir_de_zero() {
        let mut decalee = ImageFabriquee::deux_pistes();
        decalee.premiere_trame = 150; // 00:02:00
        let (_d, chemin, bornes) = image(&decalee);
        let disque = lire_disque(&chemin).unwrap();
        let zone = disque.zone_de_lecture().unwrap();
        assert_eq!(zone.pistes[0].debut_trame, 150);
        let (debut, fin) = bornes[1];
        let mut lecteur = LecteurDsdSacd::ouvrir(
            &chemin,
            zone,
            zone.pistes[1].debut_ms(),
            Some(zone.pistes[1].fin_ms()),
        )
        .unwrap();
        assert!(tout_lire(&mut lecteur) == octets_attendus(debut, fin));
    }

    /// Le refus propre d'une zone DST : reconnue, nommée, jamais lue.
    #[test]
    fn une_zone_dst_est_refusee_proprement() {
        let mut dst = ImageFabriquee::deux_pistes();
        dst.stereo_dst = true;
        let (_d, chemin, _) = image(&dst);
        let disque = lire_disque(&chemin).unwrap();
        assert_eq!(disque.zones[0].codage, Codage::Dst);
        assert!(disque.zone_de_lecture().is_none());
        assert!(matches!(examiner(&chemin), VerdictIso::Dst(_)));
        let erreur = LecteurDsdSacd::ouvrir(&chemin, &disque.zones[0], 0, None)
            .err()
            .expect("une zone DST ne s'ouvre pas");
        assert_eq!(erreur, MOTIF_ISO_SACD_DST);
        assert_eq!(
            ouvrir_lecture(&chemin, 0, None).err().as_deref(),
            Some(MOTIF_ISO_SACD_DST)
        );
        assert_eq!(refus_de_decodage(&chemin), Some(MOTIF_ISO_SACD_DST));
    }

    /// Une multicanal DST à côté d'une stéréo DSD : la stéréo est lue, le
    /// disque n'est pas refusé pour sa zone DST.
    #[test]
    fn la_stereo_dsd_est_choisie_devant_une_multicanal_dst() {
        let mut deux = ImageFabriquee::deux_pistes();
        deux.multicanal_dst = true;
        let (_d, chemin, _) = image(&deux);
        let disque = lire_disque(&chemin).unwrap();
        assert_eq!(disque.zones.len(), 2);
        assert_eq!(disque.zones[1].genre, TypeDeZone::Multicanal);
        assert_eq!(disque.zones[1].canaux, 6);
        assert_eq!(disque.zones[1].codage, Codage::Dst);
        assert_eq!(
            disque.zone_de_lecture().map(|z| z.genre),
            Some(TypeDeZone::Stereo)
        );
        assert!(matches!(examiner(&chemin), VerdictIso::Lisible(_)));
        assert_eq!(refus_de_decodage(&chemin), None);
    }

    /// Une image de données n'est pas un SACD ; une image SACD tronquée est
    /// illisible, sans panique.
    #[test]
    fn images_de_donnees_et_tronquees() {
        let dossier = tempfile::tempdir().unwrap();
        let donnees = dossier.path().join("ubuntu.iso");
        std::fs::write(&donnees, vec![0u8; 600 * TAILLE_SECTEUR]).unwrap();
        assert_eq!(examiner(&donnees), VerdictIso::PasUnSacd);

        let (_d, chemin, _) = image(&ImageFabriquee::deux_pistes());
        let tronquee = dossier.path().join("tronquee.iso");
        let octets = std::fs::read(&chemin).unwrap();
        std::fs::write(
            &tronquee,
            &octets[..(LSN_MASTER_TOC as usize + 2) * TAILLE_SECTEUR],
        )
        .unwrap();
        assert!(matches!(examiner(&tronquee), VerdictIso::Illisible(_)));
    }

    /// L'aller-retour trames → ms → trames ne perd aucune trame.
    #[test]
    fn les_millisecondes_retrouvent_leur_trame() {
        for t in 0..100_000u32 {
            assert_eq!(ms_en_trames(trames_en_ms(t)), t, "trame {t}");
        }
    }

    /// La découpe d'un en-tête de secteur, bit à bit.
    #[test]
    fn l_en_tete_d_un_secteur_audio_se_decoupe_bit_a_bit() {
        let mut s = [0u8; TAILLE_SECTEUR];
        // 2 paquets, 1 début de trame, pas de DST.
        s[0] = (2 << 5) | (1 << 2);
        // Paquet 1 : début de trame, audio, 1000 octets.
        s[1] = 0x80 | (2 << 3) | (1000 >> 8) as u8;
        s[2] = (1000 & 0xFF) as u8;
        // Paquet 2 : bourrage, 20 octets.
        s[3] = 7 << 3;
        s[4] = 20;
        // Code temporel 01:02:03.
        s[5] = 1;
        s[6] = 2;
        s[7] = 3;
        let a = analyser_secteur(&s).unwrap();
        assert!(!a.dst);
        assert_eq!(a.trames, vec![60 * 75 + 2 * 75 + 3]);
        assert_eq!(
            a.paquets,
            vec![
                Paquet {
                    debut_de_trame: true,
                    genre: 2,
                    decalage: 8,
                    longueur: 1000
                },
                Paquet {
                    debut_de_trame: false,
                    genre: 7,
                    decalage: 1008,
                    longueur: 20
                },
            ]
        );
        s[0] |= 1;
        assert!(analyser_secteur(&s).unwrap().dst);
    }
}
