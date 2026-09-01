//! Contrat de prise en charge des formats audio dans la bibliothèque.
//!
//! Trois consommateurs avaient chacun leur propre liste d'extensions : le
//! décodeur, le scanner et le repli de métadonnées. Leur dérive a permis à WMA
//! et DST d'entrer dans la bibliothèque alors qu'aucun décodeur livré ne pouvait
//! les lire. Ce module nomme désormais les deux frontières et impose que tout
//! format catalogué (hors ISO, qui est extrait en DSF) possède un décodeur.

use std::path::Path;

/// Extensions que le moteur de lecture sait réellement décoder dans ce binaire.
pub const NATIVE_DECODE_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "wav", "m4a", "aac", "alac", "ogg", "oga", "opus", "aiff", "aif", "dsf", "dff",
    "wv", "ape",
];

/// Extensions admises par le catalogue. `iso` est l'unique exception au
/// contrat de décodage direct : le walker l'extrait d'abord en pistes DSF.
///
/// `oga` est l'extension normalisée d'un flux audio Ogg (Vorbis, FLAC-in-Ogg
/// ou Opus). Elle manquait ici seule, alors que tout le reste de la chaîne la
/// connaît — `AudioFormat::from_extension`, `can_decode_native`,
/// `tag_writer::TagFormat::Vorbis`, la décision de transcodage de
/// `network.rs`. Un `.oga` n'était donc ni catalogué ni déclaré non lu : il
/// retombait sur `NotAudio`, un `continue` muet du parcours, et disparaissait
/// de la bibliothèque sans un compteur ni une ligne de rapport (#2060).
pub const LIBRARY_AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "ogg", "oga", "opus", "wav", "aiff", "aif", "wv", "dsf", "dff", "alac",
    "ape", "iso",
];

/// Formats audio reconnus mais volontairement exclus du catalogue. Cette liste
/// reste ciblée : les pochettes, playlists et journaux ne sont pas des formats
/// audio à signaler dans un rapport de scan.
pub const KNOWN_UNREAD_AUDIO_EXTENSIONS: &[&str] = &[
    "wma", "asf", // aucun décodeur WMA/ASF livré (#2078, #2242)
    "dst", // flux DST autonome sans décodeur (#2242)
    "mpc", "mp+", "mpp", // Musepack (Rhorn, #1763)
    "cue", // feuille de découpe, jamais interprétée
    "tta", "shn", "ofr", "ofs", // sans perte, formats de niche
    "m4b", "m4p", // livres audio, achats protégés
    "dts", "ac3", "eac3", "mka", // conteneurs plutôt vidéo/multicanal
    "aac", // AAC brut : le catalogue exige aujourd'hui un conteneur m4a
    "ra", "rm", "amr", "spx",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedLibraryAudio {
    /// Clé stable utilisée par les compteurs du rapport de scan.
    pub report_key: String,
    /// Motif destiné au rapport utilisateur, pas seulement aux journaux.
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryAudioSupport {
    Supported,
    Unsupported(UnsupportedLibraryAudio),
    NotAudio,
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
}

pub fn native_decoder_supports(path: &Path) -> bool {
    extension(path).is_some_and(|ext| NATIVE_DECODE_EXTENSIONS.contains(&ext.as_str()))
}

/// Retourne le motif précis qui interdit un chemin au décodeur livré.
///
/// Cette frontière est volontairement distincte du catalogue : AAC brut est
/// décodable mais n'est pas indexé aujourd'hui, tandis que WMA/DST ne sont ni
/// catalogables ni décodables. DFF exige une inspection de contenu.
pub fn decoder_rejection(path: &Path) -> Option<UnsupportedLibraryAudio> {
    let ext = extension(path)?;
    match ext.as_str() {
        "wma" | "asf" => Some(UnsupportedLibraryAudio {
            report_key: ext,
            reason: "WMA/ASF : aucun décodeur n'est livré",
        }),
        "dst" => Some(UnsupportedLibraryAudio {
            report_key: ext,
            reason: "DST compressé : aucun décodeur n'est livré",
        }),
        "dff"
            if path
                .to_str()
                .and_then(|path| super::dff::parse_dff(path).ok())
                .is_some_and(|info| info.is_dst()) =>
        {
            Some(UnsupportedLibraryAudio {
                report_key: "dff-dst".into(),
                reason: "DSDIFF compressé en DST : aucun décodeur DST n'est livré",
            })
        }
        _ => None,
    }
}

/// Capacité réelle du décodeur pour ce fichier, y compris le contenu DFF.
pub fn native_decoder_supports_file(path: &Path) -> bool {
    native_decoder_supports(path) && decoder_rejection(path).is_none()
}

/// Classe un chemin par sa seule extension, sans ouvrir le fichier.
///
/// Le parcours initial de la bibliothèque appelle cette variante : sur un NAS,
/// l'énumération doit rester une opération de répertoire et ne jamais ajouter
/// une lecture bloquante par fichier. Un `.dff` est donc admis provisoirement ;
/// son éventuelle compression DST sera vérifiée dans la phase de métadonnées,
/// qui possède déjà un délai maximal.
pub fn library_audio_support_by_extension(path: &Path) -> LibraryAudioSupport {
    let Some(ext) = extension(path) else {
        return LibraryAudioSupport::NotAudio;
    };

    if LIBRARY_AUDIO_EXTENSIONS.contains(&ext.as_str()) {
        return LibraryAudioSupport::Supported;
    }

    let reason = match ext.as_str() {
        "wma" | "asf" => "WMA/ASF : aucun décodeur n'est livré",
        "dst" => "DST compressé : aucun décodeur n'est livré",
        _ if KNOWN_UNREAD_AUDIO_EXTENSIONS.contains(&ext.as_str()) => {
            "format audio reconnu mais non pris en charge"
        }
        _ => return LibraryAudioSupport::NotAudio,
    };

    LibraryAudioSupport::Unsupported(UnsupportedLibraryAudio {
        report_key: ext,
        reason,
    })
}

/// Classe un fichier en inspectant son contenu lorsque l'extension ne suffit
/// pas.
///
/// DSDIFF peut contenir du DSD brut (pris en charge) ou des trames DST
/// compressées (non décodées). Cette variante inspecte donc l'en-tête `.dff` ;
/// le scanner ne l'appelle que derrière le délai maximal du lecteur de
/// métadonnées, jamais pendant l'énumération des dossiers. Les chemins de
/// lecture explicites l'emploient aussi avant de promettre un décodage.
pub fn library_audio_support(path: &Path) -> LibraryAudioSupport {
    let by_extension = library_audio_support_by_extension(path);
    if !matches!(by_extension, LibraryAudioSupport::Supported) {
        return by_extension;
    }

    if let Some(unsupported) = decoder_rejection(path) {
        return LibraryAudioSupport::Unsupported(unsupported);
    }

    by_extension
}

/// DSDIFF minimal, mais structurellement valide, dont le payload est annoncé
/// DST. Gardé octet pour octet comme témoin commun du contrat scanner/décodeur.
#[cfg(test)]
pub(crate) fn dff_dst_minimal_fixture() -> Vec<u8> {
    let mut fver = Vec::new();
    fver.extend_from_slice(b"FVER");
    fver.extend_from_slice(&4u64.to_be_bytes());
    fver.extend_from_slice(&0x0105_0000u32.to_be_bytes());

    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&10u64.to_be_bytes());
    prop.extend_from_slice(&2u16.to_be_bytes());
    prop.extend_from_slice(b"SLFTSRGT");
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DST ");

    let mut dst = Vec::new();
    dst.extend_from_slice(b"FRTE");
    dst.extend_from_slice(&6u64.to_be_bytes());
    dst.extend_from_slice(&75u32.to_be_bytes());
    dst.extend_from_slice(&75u16.to_be_bytes());
    dst.extend_from_slice(b"DSTF");
    dst.extend_from_slice(&4u64.to_be_bytes());
    dst.extend_from_slice(&[0xAA; 4]);

    let frm8_size = 4 + fver.len() + 12 + prop.len() + 12 + dst.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"FRM8");
    bytes.extend_from_slice(&(frm8_size as u64).to_be_bytes());
    bytes.extend_from_slice(b"DSD ");
    bytes.extend_from_slice(&fver);
    bytes.extend_from_slice(b"PROP");
    bytes.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&prop);
    bytes.extend_from_slice(b"DST ");
    bytes.extend_from_slice(&(dst.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&dst);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tout_format_catalogue_est_decode_ou_extrait() {
        for ext in LIBRARY_AUDIO_EXTENSIONS {
            if *ext == "iso" {
                continue;
            }
            assert!(
                NATIVE_DECODE_EXTENSIONS.contains(ext),
                ".{ext} est catalogué sans décodeur natif"
            );
        }
    }

    /// Le contrat vaut dans les DEUX sens (#2060).
    ///
    /// Un format que le decodeur sait lire mais que le catalogue ignore ne
    /// devient pas « non pris en charge » : il devient `NotAudio`, donc un
    /// `continue` muet dans le parcours — pas de piste, pas de compteur, pas
    /// de ligne de rapport. Un ecart doit donc etre DECIDE (`aac`, present
    /// dans la liste des non lus) et jamais subi.
    #[test]
    fn tout_format_decodable_est_catalogue_ou_declare_non_lu() {
        for ext in NATIVE_DECODE_EXTENSIONS {
            assert!(
                LIBRARY_AUDIO_EXTENSIONS.contains(ext)
                    || KNOWN_UNREAD_AUDIO_EXTENSIONS.contains(ext),
                ".{ext} est decodable mais ni catalogue ni declare non lu — il disparaitrait du scan sans une ligne de rapport"
            );
        }
    }

    #[test]
    fn decodeur_et_catalogue_restent_deux_frontieres_distinctes() {
        assert!(native_decoder_supports_file(Path::new("radio.aac")));
        assert!(matches!(
            library_audio_support_by_extension(Path::new("radio.aac")),
            LibraryAudioSupport::Unsupported(_)
        ));
        assert!(!native_decoder_supports_file(Path::new("album.wma")));
    }

    #[test]
    fn wma_asf_et_dst_restent_fail_closed() {
        for (name, expected_reason) in [
            ("album.wma", "WMA/ASF : aucun décodeur n'est livré"),
            ("album.asf", "WMA/ASF : aucun décodeur n'est livré"),
            ("album.dst", "DST compressé : aucun décodeur n'est livré"),
        ] {
            let LibraryAudioSupport::Unsupported(unsupported) =
                library_audio_support(Path::new(name))
            else {
                panic!("{name} ne doit jamais être annoncé comme jouable");
            };
            assert_eq!(unsupported.reason, expected_reason);
            assert!(!native_decoder_supports(Path::new(name)));
        }
    }

    #[test]
    fn dff_compresse_dst_est_detecte_par_son_contenu() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("album.dff");
        std::fs::write(&path, dff_dst_minimal_fixture()).unwrap();

        let LibraryAudioSupport::Unsupported(unsupported) = library_audio_support(&path) else {
            panic!("le DFF compressé DST ne doit pas entrer au catalogue");
        };
        assert_eq!(unsupported.report_key, "dff-dst");
        assert!(unsupported.reason.contains("aucun décodeur DST"));
    }
}
