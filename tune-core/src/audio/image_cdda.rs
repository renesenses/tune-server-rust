//! Images de CD audio brutes : `.bin` et `.img` découpés par une feuille CUE
//! (#5298).
//!
//! Un `FILE "album.bin" BINARY` n'a AUCUN en-tête : c'est le flux du disque tel
//! que le lecteur l'a lu, 2352 octets par secteur, 75 secteurs par seconde. En
//! mode `AUDIO`, chaque secteur porte 588 trames de PCM 16 bits little-endian,
//! 44 100 Hz, stéréo — exactement la charge utile d'un WAV. Ce module ne décode
//! donc rien : il présente une FENÊTRE du fichier brut au décodeur comme un WAV
//! (44 octets d'en-tête, puis les octets du disque, sans copie), et le chemin
//! PCM existant — symphonia, le relais de niveaux, le rééchantillonnage, la
//! profondeur de sortie — fait le reste. Aucun outil externe.
//!
//! ## L'exactitude à l'échantillon
//!
//! La bibliothèque range les bornes d'une tranche CUE en MILLISECONDES. Une
//! frame CD vaut 13,33 ms : `INDEX 01 00:02:37` devient 2493 ms, soit 109 941
//! trames, quand le disque dit 187 × 588 = 109 956. Quinze échantillons de la
//! piste précédente en tête de celle-ci, quinze de moins à sa fin : un clic
//! possible, et une piste qui n'est plus celle du disque.
//!
//! [`trame_de`] rend l'exactitude : un instant qui tombe moins d'une
//! milliseconde AVANT une frontière de secteur est cette frontière — c'est la
//! seule façon dont `parse_cue_time` arrondit. Tout autre instant (un
//! déplacement de l'auditeur au milieu d'une piste) garde sa position à
//! l'échantillon près.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::io::MediaSource;

/// Octets d'un secteur de CD audio brut.
pub const OCTETS_PAR_SECTEUR: u64 = 2352;
/// Trames stéréo par secteur : 2352 / 4.
pub const TRAMES_PAR_SECTEUR: u64 = 588;
/// Secteurs (frames CD) par seconde.
pub const SECTEURS_PAR_SECONDE: u64 = 75;
/// Cadence du CD audio.
pub const CADENCE: u32 = 44_100;
/// Canaux du CD audio.
pub const CANAUX: u16 = 2;
/// Profondeur du CD audio.
pub const PROFONDEUR: u16 = 16;

const OCTETS_PAR_TRAME: u64 = 4;
const EN_TETE: u64 = 44;

/// Un instant tombant au plus à cette distance AVANT une frontière de secteur
/// est cette frontière : c'est l'arrondi des millisecondes de la feuille.
const TOLERANCE_S: f64 = 0.001 + 1e-9;

/// Ce fichier est-il une image de CD audio brute ?
///
/// Par l'extension seule, comme le reste du décodeur. La feuille, elle, a déjà
/// vérifié le type `BINARY` avant qu'une piste n'existe en base : un `.bin`
/// n'entre jamais seul dans la bibliothèque.
pub fn est_image_cdda(chemin: &Path) -> bool {
    chemin
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("img"))
}

/// La trame (échantillon stéréo) qui correspond à un instant de l'image.
///
/// Voir l'en-tête du module : un instant venu d'un temps de feuille est
/// ramené sur SON secteur, exactement ; tout autre instant garde sa position.
pub fn trame_de(secondes: f64) -> u64 {
    if !secondes.is_finite() || secondes <= 0.0 {
        return 0;
    }
    let secteur = (secondes * SECTEURS_PAR_SECONDE as f64).ceil();
    if secteur / SECTEURS_PAR_SECONDE as f64 - secondes < TOLERANCE_S {
        return secteur as u64 * TRAMES_PAR_SECTEUR;
    }
    (secondes * CADENCE as f64).round() as u64
}

/// Nombre de trames entières d'une image de `octets` octets.
pub fn trames_de_l_image(octets: u64) -> u64 {
    octets / OCTETS_PAR_TRAME
}

/// Durée d'une image, en millisecondes, lue sur sa seule taille.
///
/// Une image brute n'a pas d'en-tête que lofty sache lire : sans cette
/// mesure, la DERNIÈRE piste d'une feuille entrerait en base avec
/// `duration_ms = 0`.
pub fn duree_ms(chemin: &Path) -> Option<i64> {
    let octets = std::fs::metadata(chemin).ok()?.len();
    let ms = trames_de_l_image(octets) * 1000 / CADENCE as u64;
    (ms > 0).then_some(ms as i64)
}

/// Une fenêtre `[debut, fin)` d'une image brute, vue comme un WAV.
///
/// Les octets de l'image ne sont jamais copiés : une lecture dans la charge
/// utile est une lecture dans le fichier, décalée du début de la fenêtre.
pub struct SourceWavCdda {
    fichier: File,
    en_tete: [u8; EN_TETE as usize],
    /// Premier octet de la fenêtre DANS le fichier.
    debut_octets: u64,
    /// Longueur de la charge utile.
    donnees: u64,
    /// Position dans le WAV virtuel.
    position: u64,
    /// Position réelle du curseur du fichier, pour ne chercher qu'au besoin.
    curseur_fichier: Option<u64>,
}

impl SourceWavCdda {
    /// Ouvre la fenêtre qui commence à `debut_s` et dure `duree_s` — jusqu'au
    /// bout de l'image si `duree_s` est absente ou nulle.
    pub fn ouvrir(chemin: &Path, debut_s: f64, duree_s: Option<f64>) -> io::Result<Self> {
        let fichier = File::open(chemin)?;
        let total = trames_de_l_image(fichier.metadata()?.len());
        let debut = trame_de(debut_s).min(total);
        let fin = duree_s
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| trame_de(debut_s + d))
            .unwrap_or(total)
            .clamp(debut, total);
        Ok(Self::sur_fenetre(fichier, debut, fin))
    }

    /// La fenêtre en trames, bornes déjà résolues.
    fn sur_fenetre(fichier: File, debut: u64, fin: u64) -> Self {
        let donnees = (fin - debut) * OCTETS_PAR_TRAME;
        let en_tete = super::wav::build_wav_header_with_data_size(
            CANAUX,
            CADENCE,
            PROFONDEUR,
            u32::try_from(donnees).unwrap_or(u32::MAX),
        );
        Self {
            fichier,
            en_tete,
            debut_octets: debut * OCTETS_PAR_TRAME,
            donnees,
            position: 0,
            curseur_fichier: None,
        }
    }

    fn longueur(&self) -> u64 {
        EN_TETE + self.donnees
    }
}

impl Read for SourceWavCdda {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.longueur() {
            return Ok(0);
        }
        if self.position < EN_TETE {
            let depuis = self.position as usize;
            let n = (EN_TETE as usize - depuis).min(buf.len());
            buf[..n].copy_from_slice(&self.en_tete[depuis..depuis + n]);
            self.position += n as u64;
            return Ok(n);
        }
        let dans_les_donnees = self.position - EN_TETE;
        let cible = self.debut_octets + dans_les_donnees;
        if self.curseur_fichier != Some(cible) {
            self.fichier.seek(SeekFrom::Start(cible))?;
        }
        let reste = (self.donnees - dans_les_donnees).min(buf.len() as u64) as usize;
        let n = self.fichier.read(&mut buf[..reste])?;
        self.position += n as u64;
        self.curseur_fichier = Some(cible + n as u64);
        Ok(n)
    }
}

impl Seek for SourceWavCdda {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let cible = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.longueur().checked_add_signed(d),
            SeekFrom::Current(d) => self.position.checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek avant le début"))?;
        self.position = cible;
        Ok(cible)
    }
}

impl MediaSource for SourceWavCdda {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.longueur())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_temps_de_feuille_retombe_sur_son_secteur() {
        // `INDEX 01 00:02:37` : 187 secteurs, que la feuille range à 2493 ms.
        assert_eq!(trame_de(2.493), 187 * TRAMES_PAR_SECTEUR);
        // Une frame : 13 ms en base, 588 trames sur le disque.
        assert_eq!(trame_de(0.013), TRAMES_PAR_SECTEUR);
        // Frontière exacte.
        assert_eq!(trame_de(1.0), 44_100);
        assert_eq!(trame_de(0.0), 0);
    }

    #[test]
    fn un_deplacement_libre_garde_sa_position() {
        // 2,5 s n'est à moins d'une milliseconde d'aucun secteur.
        assert_eq!(trame_de(2.5), 110_250);
    }

    #[test]
    fn la_fenetre_se_lit_comme_un_wav() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("x.bin");
        let octets: Vec<u8> = (0..(3 * OCTETS_PAR_SECTEUR)).map(|i| i as u8).collect();
        std::fs::write(&p, &octets).unwrap();
        let mut s = SourceWavCdda::ouvrir(&p, 1.0 / 75.0, Some(1.0 / 75.0)).unwrap();
        assert_eq!(s.byte_len(), Some(44 + OCTETS_PAR_SECTEUR));
        let mut lu = Vec::new();
        s.read_to_end(&mut lu).unwrap();
        assert_eq!(&lu[..4], b"RIFF");
        assert_eq!(
            &lu[44..],
            &octets[OCTETS_PAR_SECTEUR as usize..2 * OCTETS_PAR_SECTEUR as usize]
        );
        // Retour arrière : le curseur du fichier suit.
        s.seek(SeekFrom::Start(44)).unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(b, octets[2352..2356]);
    }
}
