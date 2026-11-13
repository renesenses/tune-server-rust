//! #8271 — distinguer les mixes d'un même titre Qobuz (Peter Gabriel, « i/o » :
//! Bright-Side Mix et Dark-Side Mix). Qobuz range le nom du mix dans
//! `version`, à côté du titre ; Tune le jetait, et les deux pistes sortaient
//! sous le même nom. Réponses Qobuz réduites à leurs champs utiles, contenu
//! fictif : aucun appel à Qobuz.

use super::*;
use serde_json::json;

fn piste(id: u64, version: Option<&str>) -> serde_json::Value {
    json!({
        "id": id,
        "title": "Panopticom",
        "version": version,
        "duration": 330,
        "performer": {"name": "Peter Gabriel"},
        "album": {
            "id": "io-bright",
            "title": "i/o",
            "version": "Bright-Side Mixes",
            "artist": {"name": "Peter Gabriel", "id": 1}
        }
    })
}

#[test]
fn deux_mixes_d_un_meme_titre_ne_portent_plus_le_meme_nom_8271() {
    let claire = QobuzService::map_track(&piste(1, Some("Bright-Side Mix")));
    let sombre = QobuzService::map_track(&piste(2, Some("Dark-Side Mix")));
    assert_eq!(claire.title, "Panopticom (Bright-Side Mix)");
    assert_eq!(sombre.title, "Panopticom (Dark-Side Mix)");
    assert_ne!(claire.title, sombre.title);
    assert_eq!(
        claire.album.as_deref(),
        Some("i/o (Bright-Side Mixes)"),
        "l'album imbriqué porte aussi sa version"
    );
}

#[test]
fn sans_version_le_titre_ne_change_pas_8271() {
    for version in [None, Some(""), Some("   ")] {
        let t = QobuzService::map_track(&piste(3, version));
        assert_eq!(t.title, "Panopticom", "version {version:?}");
    }
}

#[test]
fn une_version_deja_dans_le_titre_n_est_pas_doublee_8271() {
    let mut brut = piste(4, Some("Bright-Side Mix"));
    brut["title"] = json!("Panopticom (bright-side mix)");
    assert_eq!(
        QobuzService::map_track(&brut).title,
        "Panopticom (bright-side mix)"
    );
}

#[test]
fn l_album_porte_sa_version_dans_les_listes_et_les_favoris_8271() {
    let brut = json!({
        "id": "io-dark",
        "title": "i/o",
        "version": "Dark-Side Mixes",
        "artist": {"name": "Peter Gabriel", "id": 1},
        "tracks_count": 12,
        "duration": 4100
    });
    assert_eq!(
        QobuzService::map_album(&brut).title,
        "i/o (Dark-Side Mixes)"
    );
    assert_eq!(
        QobuzService::favori_date(&brut, "albums")["title"],
        json!("i/o (Dark-Side Mixes)")
    );
    assert_eq!(
        QobuzService::favori_date(&piste(5, Some("Dark-Side Mix")), "tracks")["title"],
        json!("Panopticom (Dark-Side Mix)")
    );
}
