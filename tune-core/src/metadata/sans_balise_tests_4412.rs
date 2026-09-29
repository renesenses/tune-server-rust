//! #4412 (Marco Polo, fil 1836) — un Ogg Vorbis sans aucune balise.
//!
//! L'en-tête de commentaires d'un Vorbis est obligatoire : même vide, il porte
//! la chaîne du vendeur, que lofty rend comme une balise. Le fichier passait
//! donc pour balisé et ne recevait pas l'artiste tiré de l'arborescence,
//! contrairement à un WAV rangé au même endroit.
//!
//! Le Vorbis est FABRIQUÉ ici : la fixture synthétique du dépôt
//! (`tests/fixtures/test_vorbis.ogg`, du silence encodé) dont lofty réécrit le
//! bloc de commentaires — vide, avec le vendeur des fichiers du testeur, ou
//! balisé. Aucun fichier sous droits, aucun encodeur externe.

use super::*;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::ogg::VorbisComments;
use lofty::tag::TagExt;

fn scratch() -> crate::test_scratch::ScratchDir {
    crate::test_scratch::scratch_dir_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        "sans-balise-4412",
    )
}

/// Pose un Vorbis à `rel` sous `racine`, avec ces commentaires (aucun ⇒ bloc
/// vide, seul le vendeur reste).
fn vorbis(racine: &Path, rel: &str, balises: &[(&str, &str)]) -> std::path::PathBuf {
    let piste = racine.join(rel);
    std::fs::create_dir_all(piste.parent().unwrap()).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_vorbis.ogg"),
        &piste,
    )
    .unwrap();
    let mut vc = VorbisComments::default();
    vc.set_vendor("Xiph.Org libVorbis I 20040629".to_string());
    for (k, v) in balises {
        vc.insert(k.to_string(), v.to_string());
    }
    vc.save_to_path(&piste, WriteOptions::default()).unwrap();
    piste
}

#[test]
fn vorbis_sans_balise_recoit_l_artiste_du_chemin() {
    let dir = scratch();
    let piste = vorbis(&dir, "Tatiana Nikolayeva/Preludes and Fugues/01.ogg", &[]);
    // Le fichier est bien lu comme un Vorbis vide par lofty, pas en échec.
    assert!(
        lofty::probe::Probe::open(&piste)
            .unwrap()
            .read()
            .is_ok_and(|f| f.first_tag().is_some()),
        "le Vorbis fabriqué doit porter un bloc de commentaires (vide)"
    );
    let m = try_read_metadata(&piste).unwrap();
    assert_eq!(m.artist.as_deref(), Some("Tatiana Nikolayeva"));
    assert_eq!(m.album.as_deref(), Some("Preludes and Fugues"));
    assert!(m.artist_from_path, "repli sans balise : il se dénonce");
    assert_eq!(m.album_artist, None);
    assert_eq!(m.format.as_deref(), Some("ogg"));
    assert!(m.duration_ms.is_some_and(|d| d > 0), "propriétés gardées");
}

#[test]
fn vorbis_sans_balise_sous_un_dossier_generique_n_a_pas_d_artiste_musique() {
    let dir = scratch();
    let piste = vorbis(&dir, "bib/Musique/Album/01.ogg", &[]);
    let m = try_read_metadata(&piste).unwrap();
    assert_eq!(m.album.as_deref(), Some("Album"));
    assert_ne!(m.artist.as_deref(), Some("Musique"));
    // On remonte : « bib » est le premier nom qui n'est pas muet.
    assert_eq!(m.artist.as_deref(), Some("bib"));
}

#[test]
fn vorbis_balise_garde_ses_balises() {
    let dir = scratch();
    let piste = vorbis(
        &dir,
        "Dossier Artiste/Dossier Album/01.ogg",
        &[
            ("TITLE", "Prelude"),
            ("ARTIST", "Chostakovitch"),
            ("ALBUM", "24 Preludes"),
        ],
    );
    let m = try_read_metadata(&piste).unwrap();
    assert_eq!(m.title.as_deref(), Some("Prelude"));
    assert_eq!(m.artist.as_deref(), Some("Chostakovitch"));
    assert_eq!(m.album.as_deref(), Some("24 Preludes"));
    assert!(!m.artist_from_path);
}

/// Le chemin réel du testeur, au format Windows : l'artiste reste VIDE.
/// Découpé comme une chaîne, il se lit pareil sur la CI Linux.
#[test]
fn chemin_windows_de_marco_polo_ne_donne_aucun_artiste() {
    let album = r"Z:\Musique\750GB\Musique\Shostakovich, Dimitri - 24 Preludes and Fugues, op 87 - Tatiana Nikolayeva";
    assert_eq!(artiste_au_dessus_de(Path::new(album)), None);
    // Même partage vu par Tune en UNC (journaux du 17/09).
    let unc = r"\\synonas\share\Musique\750GB\Musique\Shostakovich - Nikolayeva";
    assert_eq!(artiste_au_dessus_de(Path::new(unc)), None);
    // Témoin : le découpage Windows fonctionne bien, un vrai nom remonte.
    assert_eq!(
        artiste_au_dessus_de(Path::new(r"Z:\Musique\750GB\Bach\Goldberg")).as_deref(),
        Some("Bach")
    );
    assert_eq!(
        artiste_au_dessus_de(Path::new("/Volumes/NAS/Music/Miles Davis/Kind of Blue")).as_deref(),
        Some("Miles Davis")
    );
    // Racine atteinte : jamais la racine elle-même.
    assert_eq!(artiste_au_dessus_de(Path::new("/Musique/Album")), None);
}

#[test]
fn noms_muets_generiques_et_volumes() {
    for muet in [
        "Musique",
        "MUSIC",
        "Audio",
        "Musik",
        "Música",
        "Downloads",
        "Téléchargements",
        "750GB",
        "2 To",
        "1.5tb",
        "500G",
        "NAS",
        "share",
        "Public",
        "Z",
        "z:",
        "Volumes",
        "mnt",
        "media",
    ] {
        assert!(
            dossier_muet_pour_l_artiste(muet),
            "{muet} devrait être muet"
        );
    }
    for parlant in [
        "Miles Davis",
        "Tango",
        "750 Grammes",
        "Musique de chambre",
        "Nas Band",
        "1.5",
        "GB",
    ] {
        assert!(
            !dossier_muet_pour_l_artiste(parlant),
            "{parlant} ne devrait pas être muet"
        );
    }
}

/// Seule l'identification de l'encodeur (ffmpeg écrit `ENCODER=`) : toujours
/// « sans balise utile ». Un genre seul, lui, garde le chemin nominal.
#[test]
fn encodeur_seul_est_sans_balise_mais_un_genre_seul_reste_balise() {
    let dir = scratch();
    let piste = vorbis(
        &dir,
        "Nikolayeva/Fugues/02.ogg",
        &[("ENCODER", "Lavc62.28.102 libvorbis")],
    );
    let m = try_read_metadata(&piste).unwrap();
    assert_eq!(m.artist.as_deref(), Some("Nikolayeva"));
    assert!(m.artist_from_path);

    let piste = vorbis(&dir, "Nikolayeva/Fugues/03.ogg", &[("GENRE", "Classical")]);
    let m = try_read_metadata(&piste).unwrap();
    assert_eq!(m.genre.as_deref(), Some("Classical"));
    assert!(!m.artist_from_path);
}
