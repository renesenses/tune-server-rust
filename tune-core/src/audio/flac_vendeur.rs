//! L'ENCODEUR qui a écrit un FLAC — la chaîne « vendeur » de son bloc
//! VORBIS_COMMENT — et la règle qui en découle pour le passthrough (#4350).
//!
//! ## Le fait
//!
//! .18, 17/09/2026 : zone Eversolo DMP-A8 (DLNA) en mode PURE, les pistes de
//! l'enregistreur restaient MUETTES. Le renderer lisait exactement les
//! en-têtes FLAC (70 788 octets : STREAMINFO, tags, pochette) puis calait en
//! `TRANSITIONING`, sans jamais demander la suite. Hors PURE, Tune décode et
//! ré-encode, et les mêmes pistes jouent.
//!
//! | fichier | vendeur | MD5 | PURE sur le DMP-A8 |
//! |---|---|---|---|
//! | Dakota Jim 3 (enregistreur) | `Lavf60.16.100` | nul | muet |
//! | Elbow (enregistreur) | `Lavf60.16.100` | nul | muet |
//! | Extreme Ways | `reference libFLAC 1.2.1` | réel | joue |
//!
//! Le blocksize (4608 des deux côtés) et la taille de la pochette (251 Ko sur
//! le fichier qui joue) sont écartés. Ce qui fait caler le renderer dans le
//! flux ffmpeg n'est PAS établi ; ce qui l'est : un FLAC écrit par ffmpeg,
//! servi tel quel, ne joue pas sur ce renderer.
//!
//! ## La règle — la combinaison MESURÉE, et elle seule
//!
//! Un FLAC dont le vendeur commence par `Lavf` (libavformat, donc ffmpeg) **ET**
//! dont le MD5 de STREAMINFO est nul ne part PAS en passthrough vers une sortie
//! réseau : il est décodé et ré-encodé par Tune. C'est sans perte — les
//! échantillons sont identiques, seul le conteneur est réécrit — et c'est ce
//! que Tune faisait déjà hors PURE, là où le renderer jouait.
//!
//! Pourquoi les DEUX conditions :
//! - `Lavf` seul est trop large : le FLAC de référence du dépôt
//!   (`tests/fixtures/test.flac`) est écrit par `Lavf62.12.101` avec un MD5
//!   réel, et le témoin `un_flac_part_tel_quel_vers_un_renderer_dlna_qui_ne_refuse_rien`
//!   exige qu'il parte tel quel. Rien ne montre qu'un tel fichier cale.
//! - le MD5 nul seul aussi : d'autres encodeurs légitimes le laissent à zéro
//!   (encodage en flux), sans preuve de calage.
//!
//! Les fichiers muets du .18 portent les deux.
//!
//! ## La lecture
//!
//! On parcourt les blocs de métadonnées EN SAUTANT leur contenu (`seek`) : une
//! pochette de plusieurs mégaoctets placée avant VORBIS_COMMENT n'est jamais
//! lue. On s'arrête au bloc vendeur, au dernier bloc, ou après
//! [`BLOCS_MAX`] blocs. Toute erreur — fichier illisible, marqueur absent,
//! bloc tronqué — rend `None` : dans le doute, la décision de lecture ne
//! change pas.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Borne du parcours : un FLAC réel porte une poignée de blocs ; au-delà, le
/// fichier est anormal et on n'insiste pas.
const BLOCS_MAX: usize = 64;

/// Taille maximale lue pour la chaîne vendeur (elle fait quelques dizaines
/// d'octets ; une valeur absurde signale un fichier corrompu).
const VENDEUR_MAX: u32 = 4096;

/// Type de bloc VORBIS_COMMENT dans l'en-tête FLAC.
const TYPE_VORBIS_COMMENT: u8 = 4;

/// Ce que l'en-tête d'un FLAC dit de son écriture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnteteFlac {
    /// La chaîne vendeur du VORBIS_COMMENT, si le bloc existe.
    pub vendeur: Option<String>,
    /// Le MD5 des échantillons de STREAMINFO vaut-il zéro (non calculé) ?
    pub md5_nul: bool,
}

/// L'en-tête d'un flux FLAC : MD5 de STREAMINFO (toujours le premier bloc) et
/// vendeur. `None` si ce n'est pas un FLAC ou si STREAMINFO est illisible.
pub fn entete_flac<R: Read + Seek>(lecteur: &mut R) -> Option<EnteteFlac> {
    let mut marqueur = [0u8; 4];
    lecteur.read_exact(&mut marqueur).ok()?;
    if &marqueur != b"fLaC" {
        return None;
    }
    let mut entete = [0u8; 4];
    lecteur.read_exact(&mut entete).ok()?;
    let longueur = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]);
    if entete[0] & 0x7f != 0 || longueur != 34 {
        return None;
    }
    let mut streaminfo = [0u8; 34];
    lecteur.read_exact(&mut streaminfo).ok()?;
    let md5_nul = streaminfo[18..34].iter().all(|&o| o == 0);
    let vendeur = if entete[0] & 0x80 != 0 {
        None
    } else {
        vendeur_apres_streaminfo(lecteur)
    };
    Some(EnteteFlac { vendeur, md5_nul })
}

/// La chaîne vendeur d'un flux FLAC, lue sur n'importe quel lecteur
/// positionnable. `None` si ce n'est pas un FLAC ou si aucun bloc
/// VORBIS_COMMENT lisible n'est trouvé.
pub fn vendeur_flac<R: Read + Seek>(lecteur: &mut R) -> Option<String> {
    entete_flac(lecteur)?.vendeur
}

/// Parcourt les blocs qui suivent STREAMINFO jusqu'au VORBIS_COMMENT.
fn vendeur_apres_streaminfo<R: Read + Seek>(lecteur: &mut R) -> Option<String> {
    for _ in 0..BLOCS_MAX {
        let mut entete = [0u8; 4];
        lecteur.read_exact(&mut entete).ok()?;
        let dernier = entete[0] & 0x80 != 0;
        let type_bloc = entete[0] & 0x7f;
        let longueur = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]);
        if type_bloc == TYPE_VORBIS_COMMENT {
            // Le vendeur est en tête du bloc : longueur sur 32 bits PETIT-
            // boutiste (convention Vorbis, contrairement à l'en-tête FLAC).
            if longueur < 4 {
                return None;
            }
            let mut taille = [0u8; 4];
            lecteur.read_exact(&mut taille).ok()?;
            let taille = u32::from_le_bytes(taille);
            if taille > VENDEUR_MAX || taille > longueur - 4 {
                return None;
            }
            let mut octets = vec![0u8; taille as usize];
            lecteur.read_exact(&mut octets).ok()?;
            return Some(String::from_utf8_lossy(&octets).into_owned());
        }
        if dernier {
            return None;
        }
        lecteur.seek(SeekFrom::Current(i64::from(longueur))).ok()?;
    }
    None
}

/// Le vendeur désigne-t-il ffmpeg (libavformat) ?
pub fn vendeur_est_ffmpeg(vendeur: &str) -> bool {
    vendeur.trim_start().starts_with("Lavf")
}

/// La combinaison qui cale le renderer (voir l'en-tête du module).
pub fn entete_a_eviter_en_passthrough(e: &EnteteFlac) -> bool {
    e.md5_nul && e.vendeur.as_deref().is_some_and(vendeur_est_ffmpeg)
}

/// Ce fichier FLAC est-il écrit par ffmpeg SANS MD5 ? `false` dans le doute
/// (fichier illisible, pas un FLAC, pas de vendeur).
pub fn flac_ecrit_par_ffmpeg(chemin: &Path) -> bool {
    std::fs::File::open(chemin)
        .ok()
        .and_then(|f| entete_flac(&mut std::io::BufReader::new(f)))
        .is_some_and(|e| entete_a_eviter_en_passthrough(&e))
}

/// Longueur de l'en-tête neuf : `fLaC` (4) + en-tête de bloc (4) + STREAMINFO
/// (34) + en-tête de bloc (4) + VORBIS_COMMENT vide (8).
pub const ENTETE_NEUF_OCTETS: usize = 4 + 4 + 34 + 4 + 8;

/// #4800 — le conteneur NEUF sous lequel servir ce FLAC tel quel, sans le
/// décoder : `fLaC`, son STREAMINFO recopié à l'octet près, un VORBIS_COMMENT
/// vide — puis ses trames, copiées depuis le fichier à partir de
/// `body_src_start`.
///
/// C'est, à l'octet près, la forme que [`crate::audio::decode::remux_flac_dash_stream`]
/// donne aux trames Tidal (DASH) pour les renderers DLNA, l'Eversolo DMP-A8
/// compris — et les fichiers de l'enregistreur portent ces mêmes trames,
/// recopiées par `ffmpeg -c copy` sous un en-tête `Lavf`. Réécrire l'en-tête
/// sans toucher aux trames tient donc la promesse de #4350 (conteneur neuf,
/// échantillons intacts) sans le décodage-ré-encodage complet qui retardait
/// le premier son de 2 à 4,6 s sur le .18 (mesure du 23/09, cause 5 de #4800).
///
/// `None` dans le doute — pas un FLAC, STREAMINFO illisible ou sans cadence,
/// aucune trame après les métadonnées, ou premier octet qui n'est pas un code
/// de synchronisation de trame : la décision garde alors le transcodage.
pub fn conteneur_neuf<R: Read + Seek>(
    lecteur: &mut R,
    taille_fichier: u64,
) -> Option<crate::audio::faststart::FaststartMap> {
    let mut marqueur = [0u8; 4];
    lecteur.read_exact(&mut marqueur).ok()?;
    if &marqueur != b"fLaC" {
        return None;
    }
    let mut entete = [0u8; 4];
    lecteur.read_exact(&mut entete).ok()?;
    let longueur = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]);
    if entete[0] & 0x7f != 0 || longueur != 34 {
        return None;
    }
    let mut streaminfo = [0u8; 34];
    lecteur.read_exact(&mut streaminfo).ok()?;
    // Cadence sur 20 bits en tête du quatrième mot : nulle, le STREAMINFO est
    // un gabarit jamais rempli, pas un en-tête.
    let cadence = u32::from_be_bytes([
        streaminfo[10],
        streaminfo[11],
        streaminfo[12],
        streaminfo[13],
    ]) >> 12;
    if cadence == 0 {
        return None;
    }
    let mut dernier = entete[0] & 0x80 != 0;
    let mut blocs = 0usize;
    while !dernier {
        blocs += 1;
        if blocs > BLOCS_MAX {
            return None;
        }
        let mut e = [0u8; 4];
        lecteur.read_exact(&mut e).ok()?;
        dernier = e[0] & 0x80 != 0;
        let l = u32::from_be_bytes([0, e[1], e[2], e[3]]);
        lecteur.seek(SeekFrom::Current(i64::from(l))).ok()?;
    }
    let debut_des_trames = lecteur.stream_position().ok()?;
    if debut_des_trames >= taille_fichier {
        return None;
    }
    // Code de synchronisation d'une trame FLAC : 0xFFF8 (taille de bloc
    // fixe) ou 0xFFF9 (variable), les deux bits réservés à zéro.
    let mut sync = [0u8; 2];
    lecteur.read_exact(&mut sync).ok()?;
    if sync[0] != 0xFF || sync[1] & 0xFC != 0xF8 {
        return None;
    }
    let mut header = Vec::with_capacity(ENTETE_NEUF_OCTETS);
    header.extend_from_slice(b"fLaC");
    header.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]);
    header.extend_from_slice(&streaminfo);
    header.extend_from_slice(&[0x84, 0x00, 0x00, 0x08]);
    header.extend_from_slice(&[0u8; 8]);
    let body_len = taille_fichier - debut_des_trames;
    Some(crate::audio::faststart::FaststartMap {
        total: header.len() as u64 + body_len,
        header,
        body_src_start: debut_des_trames,
        body_len,
    })
}

/// [`conteneur_neuf`] sur un fichier du disque. Seules les métadonnées sont
/// lues (une pochette est sautée, jamais chargée) ; `None` dans le doute.
pub fn conteneur_neuf_pour_passthrough(
    chemin: &Path,
) -> Option<crate::audio::faststart::FaststartMap> {
    let f = std::fs::File::open(chemin).ok()?;
    let taille = f.metadata().ok()?.len();
    conteneur_neuf(&mut std::io::BufReader::new(f), taille)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Un en-tête de bloc FLAC : dernier bloc ?, type, longueur sur 24 bits.
    fn bloc(dernier: bool, type_bloc: u8, corps: &[u8]) -> Vec<u8> {
        let l = corps.len() as u32;
        let mut v = vec![
            (if dernier { 0x80 } else { 0 }) | type_bloc,
            (l >> 16) as u8,
            (l >> 8) as u8,
            l as u8,
        ];
        v.extend_from_slice(corps);
        v
    }

    fn vorbis(vendeur: &str) -> Vec<u8> {
        let mut c = (vendeur.len() as u32).to_le_bytes().to_vec();
        c.extend_from_slice(vendeur.as_bytes());
        c.extend_from_slice(&0u32.to_le_bytes()); // aucun commentaire
        c
    }

    /// Un STREAMINFO de 34 octets, MD5 nul ou non.
    fn streaminfo(md5_nul: bool) -> [u8; 34] {
        let mut s = [0u8; 34];
        if !md5_nul {
            s[18..34].copy_from_slice(&[0x5a; 16]);
        }
        s
    }

    /// La forme exacte des fichiers de l'enregistreur mesurés sur le .18 :
    /// STREAMINFO (34, MD5 nul), VORBIS_COMMENT `Lavf60.16.100`, pochette.
    fn flac_enregistreur() -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo(true)));
        f.extend(bloc(false, 4, &vorbis("Lavf60.16.100")));
        f.extend(bloc(true, 6, &vec![0xAB; 70_488]));
        f.extend_from_slice(&[0xFF, 0xF8, 0x69, 0x18]); // début de trame
        f
    }

    #[test]
    fn le_fichier_de_l_enregistreur_est_reconnu() {
        let e = entete_flac(&mut Cursor::new(flac_enregistreur())).unwrap();
        assert_eq!(e.vendeur.as_deref(), Some("Lavf60.16.100"));
        assert!(e.md5_nul);
        assert!(entete_a_eviter_en_passthrough(&e));
    }

    /// `Lavf` AVEC un MD5 réel — la forme du FLAC de référence du dépôt — part
    /// tel quel : rien ne montre qu'il cale.
    #[test]
    fn lavf_avec_md5_reel_reste_en_passthrough() {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo(false)));
        f.extend(bloc(true, 4, &vorbis("Lavf62.12.101")));
        let e = entete_flac(&mut Cursor::new(f)).unwrap();
        assert!(!e.md5_nul);
        assert!(!entete_a_eviter_en_passthrough(&e));
    }

    #[test]
    fn le_flac_de_reference_du_depot_reste_en_passthrough() {
        let chemin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
        let e = entete_flac(&mut std::io::BufReader::new(
            std::fs::File::open(&chemin).unwrap(),
        ))
        .unwrap();
        assert!(
            e.vendeur.as_deref().is_some_and(vendeur_est_ffmpeg),
            "{e:?}"
        );
        assert!(!flac_ecrit_par_ffmpeg(&chemin));
    }

    #[test]
    fn un_md5_nul_sans_ffmpeg_reste_en_passthrough() {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo(true)));
        f.extend(bloc(true, 4, &vorbis("reference libFLAC 1.4.3")));
        assert!(!entete_a_eviter_en_passthrough(
            &entete_flac(&mut Cursor::new(f)).unwrap()
        ));
    }

    #[test]
    fn libflac_n_est_pas_ffmpeg() {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo(false)));
        f.extend(bloc(true, 4, &vorbis("reference libFLAC 1.2.1 20071117")));
        let v = vendeur_flac(&mut Cursor::new(f)).unwrap();
        assert!(!vendeur_est_ffmpeg(&v));
    }

    #[test]
    fn une_pochette_avant_les_tags_est_sautee_sans_etre_lue() {
        // Pochette de 5 Mo AVANT VORBIS_COMMENT : le parcours la saute.
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo(false)));
        f.extend(bloc(false, 6, &vec![0u8; 5 * 1024 * 1024]));
        f.extend(bloc(true, 4, &vorbis("Lavf61.1.100")));
        assert_eq!(
            vendeur_flac(&mut Cursor::new(f)).as_deref(),
            Some("Lavf61.1.100")
        );
    }

    #[test]
    fn dans_le_doute_rien_ne_change() {
        // Pas un FLAC.
        assert_eq!(
            vendeur_flac(&mut Cursor::new(b"RIFF....WAVE".to_vec())),
            None
        );
        // FLAC sans VORBIS_COMMENT.
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(true, 0, &streaminfo(false)));
        assert_eq!(vendeur_flac(&mut Cursor::new(f)), None);
        // Bloc tronqué.
        let mut t = b"fLaC".to_vec();
        t.extend(bloc(false, 0, &streaminfo(false)));
        t.extend_from_slice(&[4, 0, 0, 40, 200, 0, 0]);
        assert_eq!(vendeur_flac(&mut Cursor::new(t)), None);
        // Chemin inexistant.
        assert!(!flac_ecrit_par_ffmpeg(Path::new("/nexiste/pas.flac")));
    }

    #[test]
    fn un_vide_initial_ne_trompe_pas_la_reconnaissance() {
        assert!(vendeur_est_ffmpeg(" Lavf58.76.100"));
        assert!(!vendeur_est_ffmpeg("libFLAC"));
        assert!(!vendeur_est_ffmpeg(""));
    }

    // ── #4800 — le conteneur neuf ─────────────────────────────────────────

    /// Un STREAMINFO rempli comme celui mesuré sur le .18 le 23/09
    /// (`10 - Stickle Bricks.flac`, Qobuz, enregistreur) : blocs 4608,
    /// trames 857..20 596, 96 kHz, 2 canaux, 24 bits, 10 369 280
    /// échantillons, MD5 nul.
    fn streaminfo_du_18() -> [u8; 34] {
        let mut s = [0u8; 34];
        s[0..4].copy_from_slice(&[0x12, 0x00, 0x12, 0x00]);
        s[4..10].copy_from_slice(&[0x00, 0x03, 0x59, 0x00, 0x50, 0x74]);
        s[10..14].copy_from_slice(&[0x17, 0x70, 0x03, 0x70]);
        s[14..18].copy_from_slice(&[0x00, 0x9e, 0x39, 0x00]);
        s
    }

    /// La forme exacte du fichier du .18 : STREAMINFO, PADDING (42 509),
    /// VORBIS_COMMENT `Lavf60.16.100` avec ses tags, PICTURE (154 508) en
    /// dernier, puis les trames. Rend le fichier et l'offset des trames.
    fn fichier_du_18(trames: &[u8]) -> (Vec<u8>, u64) {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &streaminfo_du_18()));
        f.extend(bloc(false, 1, &vec![0u8; 42_509]));
        let mut tags = vorbis("Lavf60.16.100");
        // Le compteur de commentaires est le dernier mot de `vorbis` : on le
        // remplace par deux tags, comme sur le fichier mesuré.
        tags.truncate(tags.len() - 4);
        tags.extend_from_slice(&2u32.to_le_bytes());
        for t in ["TITLE=Stickle Bricks", "ARTIST=Guess What"] {
            tags.extend_from_slice(&(t.len() as u32).to_le_bytes());
            tags.extend_from_slice(t.as_bytes());
        }
        f.extend(bloc(false, 4, &tags));
        f.extend(bloc(true, 6, &vec![0xAB; 154_508]));
        let debut = f.len() as u64;
        f.extend_from_slice(trames);
        (f, debut)
    }

    #[test]
    fn le_conteneur_neuf_recopie_streaminfo_et_pointe_sur_les_trames_4800() {
        let trames = [0xFF, 0xF8, 0x5B, 0x1C, 0x00, 0xE9, 0x48, 0xFF, 0xFF, 0xFF];
        let (f, debut) = fichier_du_18(&trames);
        let taille = f.len() as u64;
        let m = conteneur_neuf(&mut Cursor::new(f), taille).expect("conteneur neuf");
        assert_eq!(m.header.len(), ENTETE_NEUF_OCTETS);
        assert_eq!(&m.header[..8], b"fLaC\x00\x00\x00\x22");
        assert_eq!(
            &m.header[8..42],
            &streaminfo_du_18(),
            "STREAMINFO à l'octet près"
        );
        assert_eq!(
            &m.header[42..],
            &[0x84, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0],
            "VORBIS_COMMENT vide, dernier bloc — celui de `remux_flac_dash_stream`"
        );
        assert_eq!(
            m.body_src_start, debut,
            "les trames commencent après la pochette"
        );
        assert_eq!(m.body_len, trames.len() as u64);
        assert_eq!(m.total, ENTETE_NEUF_OCTETS as u64 + trames.len() as u64);
        // Ce qui est SERVI, reconstitué : en-tête neuf puis trames intactes.
        let (f, _) = fichier_du_18(&trames);
        let servi = [&m.header[..], &f[m.body_src_start as usize..]].concat();
        assert_eq!(servi.len() as u64, m.total);
        assert_eq!(&servi[ENTETE_NEUF_OCTETS..], &trames);
    }

    /// Le fichier de référence du dépôt (Lavf, MD5 réel) se remuxe aussi :
    /// la fonction ne juge pas le vendeur, c'est la règle qui décide quand
    /// l'appeler.
    #[test]
    fn le_flac_de_reference_du_depot_a_un_conteneur_neuf() {
        let chemin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
        let taille = std::fs::metadata(&chemin).unwrap().len();
        let m = conteneur_neuf_pour_passthrough(&chemin).expect("conteneur neuf");
        let d = std::fs::read(&chemin).unwrap();
        assert_eq!(&m.header[8..42], &d[8..42]);
        assert_eq!(
            m.total - ENTETE_NEUF_OCTETS as u64,
            taille - m.body_src_start
        );
        assert_eq!(d[m.body_src_start as usize], 0xFF);
        assert_eq!(d[m.body_src_start as usize + 1] & 0xFC, 0xF8);
    }

    /// Dans le doute, `None` : la décision garde alors le transcodage.
    #[test]
    fn sans_trame_ou_sans_cadence_pas_de_conteneur_neuf() {
        // Aucune trame après les métadonnées.
        let (f, _) = fichier_du_18(&[]);
        let taille = f.len() as u64;
        assert!(conteneur_neuf(&mut Cursor::new(f), taille).is_none());
        // Un premier octet qui n'est pas un code de synchronisation.
        let (f, _) = fichier_du_18(&[0x00, 0x00, 0x00, 0x00]);
        let taille = f.len() as u64;
        assert!(conteneur_neuf(&mut Cursor::new(f), taille).is_none());
        // STREAMINFO gabarit (cadence nulle), comme les témoins du panneau.
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(true, 0, &streaminfo(true)));
        f.extend_from_slice(&[0xFF, 0xF8, 0x69, 0x18]);
        let taille = f.len() as u64;
        assert!(conteneur_neuf(&mut Cursor::new(f), taille).is_none());
        // Pas un FLAC ; chemin inexistant.
        assert!(conteneur_neuf(&mut Cursor::new(b"RIFF....WAVE".to_vec()), 12).is_none());
        assert!(conteneur_neuf_pour_passthrough(Path::new("/nexiste/pas.flac")).is_none());
    }
}
