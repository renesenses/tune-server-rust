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
}
