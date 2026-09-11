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
    "dts", "ac3", "eac3", // conteneurs plutôt vidéo/multicanal
    // Matroska. `mka` (piste audio seule) y était déjà ; `mkv` ne l'était
    // NULLE PART — ni ici, ni au catalogue, ni chez le décodeur. Il
    // retombait donc sur `NotAudio`, c'est-à-dire le `continue` muet de
    // `walker.rs` : aucune piste, aucun compteur, aucune ligne de rapport.
    // C'est le défaut de #2060 pour `.oga`, reproduit à l'identique sur
    // l'extension que Didier apporte (#3633, fil 1717).
    //
    // Il reste dans les NON LUS, pas au catalogue : symphonia démuxe bien
    // le Matroska (feature `mkv` de `Cargo.toml`), mais un MKV de concert
    // porte presque toujours de l'AC-3/E-AC-3/TrueHD, et symphonia 0.6 ne
    // fournit AUCUN de ces codecs. Le cataloguer promettrait une lecture
    // que le binaire ne sait pas tenir.
    "mka", "mkv", // Matroska : conteneur démuxé, contenu non décodé (#3633)
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

/// Le motif NOMMÉ qui interdit de lire un fichier téléversé, ou `None`.
///
/// #3270 (point 4) — `upload_audio_file` et `resolve_uploaded_file` acceptaient
/// n'importe quoi : l'extension n'était lue que pour NOMMER le fichier écrit,
/// et le `None` de `AudioFormat::from_extension` était immédiatement absorbé par
/// un `unwrap_or("audio/wav")`. Un `.wma`, un `.iso`, un `.pdf` traversaient
/// toute la résolution, obtenaient une session de flux annoncée `audio/wav`, et
/// la zone se taisait sans qu'un mot soit dit. Même famille que #3234, dont le
/// « refus nommé » ne connaît que `.iso`.
///
/// La frontière est celle du décodeur livré ([`NATIVE_DECODE_EXTENSIONS`]), pas
/// celle du catalogue : un téléversement n'entre pas dans la bibliothèque, il
/// est joué. `.aac` brut est donc accepté ici alors que le catalogue le refuse.
///
/// **Une extension ABSENTE n'est pas un refus.** Le chemin d'aujourd'hui nomme
/// ces fichiers `.wav` par défaut (`unwrap_or("wav")`) et les joue quand ils en
/// sont vraiment ; refuser ici retirerait un cas qui marche pour couvrir une
/// supposition. Ce qu'on refuse, c'est une extension PRÉSENTE dont on sait
/// qu'aucun décodeur livré ne la lit — un fait, pas une présomption.
pub fn refus_de_televersement_par_extension(nom: &str) -> Option<String> {
    let ext = extension(Path::new(nom))?;
    if NATIVE_DECODE_EXTENSIONS.contains(&ext.as_str()) {
        // Le contenu peut encore démentir l'extension (DSDIFF compressé en
        // DST). C'est `refus_de_televersement` qui tranche, fichier en main.
        return None;
    }
    Some(motif_de_refus(&ext, None))
}

/// Même refus, FICHIER EN MAIN : ajoute l'inspection de contenu que
/// l'extension seule ne peut pas faire (un `.dff` compressé en DST).
///
/// C'est cette variante que le chemin de lecture appelle : à ce moment le
/// fichier existe, et promettre un décodage sans l'avoir vérifié est
/// exactement ce que [`native_decoder_supports_file`] existe pour éviter.
pub fn refus_de_televersement(path: &Path) -> Option<String> {
    let ext = extension(path)?;
    if !NATIVE_DECODE_EXTENSIONS.contains(&ext.as_str()) {
        return Some(motif_de_refus(&ext, None));
    }
    decoder_rejection(path).map(|refus| motif_de_refus(&ext, Some(refus.reason)))
}

/// La phrase rendue à l'auditeur. Elle nomme ce qui est refusé ET ce qui est
/// accepté : un refus qui n'indique pas la sortie est un cul-de-sac.
fn motif_de_refus(ext: &str, precision: Option<&'static str>) -> String {
    let cause = match precision {
        Some(p) => p.to_string(),
        None => format!("aucun décodeur livré ne lit « .{ext} »"),
    };
    format!(
        "Ce fichier ne peut pas être lu : {cause}. Formats acceptés : \
         FLAC, WAV, AIFF, MP3, M4A/ALAC, AAC, OGG/Opus, DSF, DFF (DSD non \
         compressé), WavPack, APE."
    )
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
    // -----------------------------------------------------------------
    // #3270 (point 4) — le refus NOMMÉ d'un fichier téléversé.
    // -----------------------------------------------------------------

    /// TÉMOIN — une extension qu'aucun décodeur livré ne lit est refusée, et
    /// le motif NOMME l'extension refusée ET les formats acceptés.
    #[test]
    fn un_televersement_illisible_est_refuse_en_nommant_le_format() {
        for nom in [
            "concert.wma",
            "album.iso",
            "notice.pdf",
            "installeur.exe",
            "decoupe.cue",
        ] {
            let motif = super::refus_de_televersement_par_extension(nom)
                .unwrap_or_else(|| panic!("« {nom} » doit être refusé"));
            let ext = nom.rsplit('.').next().unwrap();
            assert!(
                motif.contains(ext),
                "le motif doit NOMMER ce qui est refusé : {motif}"
            );
            assert!(
                motif.contains("FLAC") && motif.contains("WAV"),
                "un refus qui n'indique pas la sortie est un cul-de-sac : {motif}"
            );
        }
    }

    /// CONTRE-ÉPREUVE — tout ce que le décodeur livré sait lire passe. Sans
    /// elle, un refus trop large serait vert : il suffirait de tout refuser.
    #[test]
    fn tout_ce_que_le_decodeur_sait_lire_passe() {
        for ext in super::NATIVE_DECODE_EXTENSIONS {
            let nom = format!("piste.{ext}");
            assert_eq!(
                super::refus_de_televersement_par_extension(&nom),
                None,
                "« {nom} » est dans NATIVE_DECODE_EXTENSIONS : le refuser                  retirerait un cas qui marche"
            );
        }
        // Et la casse ne décide de rien.
        assert_eq!(
            super::refus_de_televersement_par_extension("Piste.FLAC"),
            None
        );
    }

    /// Une extension ABSENTE n'est pas un refus : le chemin d'aujourd'hui
    /// nomme ces fichiers `.wav` et les joue quand ils en sont. Refuser ici
    /// couvrirait une supposition, pas un fait.
    #[test]
    fn sans_extension_rien_n_est_refuse() {
        assert_eq!(
            super::refus_de_televersement_par_extension("enregistrement"),
            None
        );
        assert_eq!(super::refus_de_televersement_par_extension(""), None);
    }

    /// Le CONTENU peut démentir l'extension : un `.dff` compressé en DST porte
    /// une extension que le décodeur connaît, et pourtant aucun décodeur DST
    /// n'est livré. C'est ce que la variante « fichier en main » attrape et que
    /// la variante « extension seule » ne peut pas voir — les deux moitiés du
    /// même contrat.
    #[test]
    fn un_dff_compresse_en_dst_est_refuse_par_le_contenu() {
        // #3030 — `test_scratch` et rien d'autre : un chemin composé à la main
        // survit au test qui ÉCHOUE, et c'est le geste qui a laissé 3 204
        // entrées dans /tmp.
        let dossier = crate::test_scratch::scratch_dir("3270-dst");
        let chemin = dossier.join("image.dff");
        std::fs::write(&chemin, super::dff_dst_minimal_fixture()).unwrap();

        assert_eq!(
            super::refus_de_televersement_par_extension("image.dff"),
            None,
            "l'extension seule ne peut pas voir la compression DST"
        );
        let motif = super::refus_de_televersement(&chemin)
            .expect("fichier en main, le DST doit être refusé");
        assert!(
            motif.contains("DST"),
            "le motif doit nommer la compression en cause : {motif}"
        );

        // CONTRE-ÉPREUVE de la même variante : elle ne refuse pas tout. Un
        // vrai FLAC, fichier en main, passe — sinon ce test resterait vert
        // contre une fonction qui rendrait `Some` pour n'importe quoi.
        let flac = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test.flac"
        ));
        assert!(flac.exists(), "la fixture FLAC doit exister : {flac:?}");
        assert_eq!(
            super::refus_de_televersement(flac),
            None,
            "un FLAC se lit : le refuser serait une régression"
        );
    }

    use super::*;

    #[test]
    fn tout_format_catalogue_est_decode_ou_extrait() {
        for ext in LIBRARY_AUDIO_EXTENSIONS {
            if *ext == "iso" {
                // `iso` reste la seule exception au contrat de décodage — mais
                // une exception, pas un angle mort. Ce test se contentait ici
                // d'un `continue` : il ACTAIT qu'un ISO puisse être catalogué
                // sans décodeur ET sans que rien ne le dise, c'est-à-dire le
                // défaut même de #3234. Il n'est pas supprimé, il est resserré :
                // l'exemption ne vaut plus que si la demande de lecture rend un
                // motif nommé à la place du silence.
                assert!(
                    crate::audio::iso_sacd::refus_de_lecture(Path::new("album.iso")).is_some(),
                    ".iso est le seul format catalogué sans décodeur natif : la \
                     demande de lecture doit alors rendre un motif, sans quoi la \
                     zone reste muette sans un mot (#3234)"
                );
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

    /// #3633 — un `.mkv` est COMPTÉ, pas perdu.
    ///
    /// Le jumeau `.mka` est le témoin : même conteneur Matroska, même liste,
    /// même appel. S'il tombait avec `.mkv`, ce test mesurerait la fonction et
    /// non le défaut.
    #[test]
    fn un_mkv_est_declare_non_lu_comme_son_jumeau_mka() {
        for nom in ["concert.mka", "concert.mkv", "Concert.MKV"] {
            let LibraryAudioSupport::Unsupported(refus) =
                library_audio_support_by_extension(Path::new(nom))
            else {
                panic!(
                    "« {nom} » doit être DÉCLARÉ non lu : `NotAudio` est un \
                     `continue` muet du parcours — ni compteur, ni ligne de \
                     rapport, le fichier disparaît sans trace (#3633)"
                );
            };
            assert_eq!(
                refus.report_key,
                nom.rsplit('.').next().unwrap().to_lowercase()
            );
        }
        // CONTRE-ÉPREUVE : la liste ne s'est pas mise à tout avaler. Un format
        // catalogué reste catalogué, et une pochette reste muette.
        assert!(matches!(
            library_audio_support_by_extension(Path::new("album.flac")),
            LibraryAudioSupport::Supported
        ));
        assert!(matches!(
            library_audio_support_by_extension(Path::new("cover.jpg")),
            LibraryAudioSupport::NotAudio
        ));
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
