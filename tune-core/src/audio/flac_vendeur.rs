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
//! ## La règle
//!
//! Un FLAC dont le vendeur commence par `Lavf` (libavformat, donc ffmpeg) ne
//! part PAS en passthrough vers une sortie réseau : il est décodé et ré-encodé
//! par Tune. C'est sans perte — les échantillons sont identiques, seul le
//! conteneur est réécrit — et c'est ce que Tune faisait déjà hors PURE, là où
//! le renderer jouait.
//!
//! La règle vise le seul vendeur `Lavf`, pas le MD5 nul : d'autres encodeurs
//! légitimes laissent le MD5 à zéro (encodage en flux), et rien ne montre
//! qu'ils calent. Élargir priverait de passthrough des fichiers qui jouent.
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

/// La chaîne vendeur d'un flux FLAC, lue sur n'importe quel lecteur
/// positionnable. `None` si ce n'est pas un FLAC ou si aucun bloc
/// VORBIS_COMMENT lisible n'est trouvé.
pub fn vendeur_flac<R: Read + Seek>(lecteur: &mut R) -> Option<String> {
    let mut marqueur = [0u8; 4];
    lecteur.read_exact(&mut marqueur).ok()?;
    if &marqueur != b"fLaC" {
        return None;
    }
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

/// Ce fichier FLAC a-t-il été écrit par ffmpeg ? `false` dans le doute
/// (fichier illisible, pas un FLAC, pas de vendeur).
pub fn flac_ecrit_par_ffmpeg(chemin: &Path) -> bool {
    std::fs::File::open(chemin)
        .ok()
        .and_then(|f| vendeur_flac(&mut std::io::BufReader::new(f)))
        .is_some_and(|v| vendeur_est_ffmpeg(&v))
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

    /// La forme exacte des fichiers de l'enregistreur mesurés sur le .18 :
    /// STREAMINFO (34), VORBIS_COMMENT `Lavf60.16.100`, puis une pochette.
    fn flac_enregistreur() -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &[0u8; 34]));
        f.extend(bloc(false, 4, &vorbis("Lavf60.16.100")));
        f.extend(bloc(true, 6, &vec![0xAB; 70_488]));
        f.extend_from_slice(&[0xFF, 0xF8, 0x69, 0x18]); // début de trame
        f
    }

    #[test]
    fn le_fichier_de_l_enregistreur_est_reconnu() {
        let v = vendeur_flac(&mut Cursor::new(flac_enregistreur()));
        assert_eq!(v.as_deref(), Some("Lavf60.16.100"));
        assert!(vendeur_est_ffmpeg(v.as_deref().unwrap()));
    }

    #[test]
    fn libflac_n_est_pas_ffmpeg() {
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &[0u8; 34]));
        f.extend(bloc(true, 4, &vorbis("reference libFLAC 1.2.1 20071117")));
        let v = vendeur_flac(&mut Cursor::new(f)).unwrap();
        assert!(!vendeur_est_ffmpeg(&v));
    }

    #[test]
    fn une_pochette_avant_les_tags_est_sautee_sans_etre_lue() {
        // Pochette de 5 Mo AVANT VORBIS_COMMENT : le parcours la saute.
        let mut f = b"fLaC".to_vec();
        f.extend(bloc(false, 0, &[0u8; 34]));
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
        f.extend(bloc(true, 0, &[0u8; 34]));
        assert_eq!(vendeur_flac(&mut Cursor::new(f)), None);
        // Bloc tronqué.
        let mut t = b"fLaC".to_vec();
        t.extend(bloc(false, 0, &[0u8; 34]));
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
