//! #6737 (Gerard, fil 2374, rc7, Linux) — un « artiste » portait le nom du
//! partage réseau de la bibliothèque (`192.168.x.x_Musique`).
//!
//! Tune monte un partage SMB sous `/mnt/<hôte>_<partage>`
//! (`mount_smb_share`). Une piste sans balise artiste reçoit l'artiste de
//! l'arborescence, au-dessus du dossier d'album : pour un album rangé juste
//! sous le partage, c'était le nom du point de montage.
use super::*;

#[test]
fn le_point_de_montage_d_un_partage_n_est_jamais_un_artiste_6737() {
    // Le cas du signalement : l'album juste sous le partage monté par Tune.
    let (album, artiste, _) = album_artiste_du_chemin(Path::new(
        "/mnt/192.168.1.20_Musique/Mon Album/01 - Titre.flac",
    ));
    assert_eq!(album.as_deref(), Some("Mon Album"));
    assert_eq!(
        artiste, None,
        "#6737 : le point de montage du partage est devenu l'artiste de l'album"
    );

    // Même borne pour un nom de machine du réseau local, et pour le repli
    // macOS : rien au-dessus du point de montage (`Tune`, `montages`) ne doit
    // être pris à sa place.
    let (_, artiste, _) = album_artiste_du_chemin(Path::new(
        "/Users/gerard/Library/Application Support/Tune/montages/nas.local_Musique/Album/01.flac",
    ));
    assert_eq!(
        artiste, None,
        "#6737 : un dossier au-dessus du montage est devenu l'artiste"
    );

    // Un dossier d'artiste SOUS le partage reste l'artiste.
    let (_, artiste, _) = album_artiste_du_chemin(Path::new(
        "/mnt/192.168.1.20_Musique/Miles Davis/Kind of Blue/01.flac",
    ));
    assert_eq!(artiste.as_deref(), Some("Miles Davis"));

    // Un disque intercalé ne change rien : la borne vaut toujours.
    let (album, artiste, disque) =
        album_artiste_du_chemin(Path::new("/mnt/192.168.1.20_Musique/Coffret/CD2/01.flac"));
    assert_eq!(album.as_deref(), Some("Coffret"));
    assert_eq!(disque, Some(2));
    assert_eq!(artiste, None);
}

#[test]
fn seuls_les_noms_de_montage_sont_reconnus_6737() {
    for montage in [
        "192.168.1.20_Musique",
        "10.0.0.5_music",
        "nas.local_Musique",
        "Synology.lan_Audio",
        "freebox.home_Disque dur",
    ] {
        assert!(
            point_de_montage_de_partage(montage),
            "{montage} est un point de montage"
        );
    }
    for parlant in [
        "Guns_N_Roses",
        "DS918_music",
        "1.2.3_Album",
        "192.168.1.20",
        "192.168.1.300_Musique",
        "192.168.1.20_",
        "Miles Davis",
    ] {
        assert!(
            !point_de_montage_de_partage(parlant),
            "{parlant} n'est pas un point de montage"
        );
    }
}
