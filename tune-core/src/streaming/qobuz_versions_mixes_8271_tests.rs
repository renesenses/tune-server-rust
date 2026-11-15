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

#[test]
fn une_version_qui_n_est_qu_une_sous_chaine_du_titre_est_ajoutee_8271() {
    let mut brut = piste(6, Some("Live"));
    brut["title"] = json!("Live Forever");
    assert_eq!(QobuzService::map_track(&brut).title, "Live Forever (Live)");
}

#[test]
fn une_seule_mention_de_reedition_8271() {
    let mut brut = piste(7, Some("2011 Remaster"));
    brut["title"] = json!("Panopticom (Remastered 2011)");
    assert_eq!(
        QobuzService::map_track(&brut).title,
        "Panopticom (Remastered 2011)"
    );
}

/// Relecture de #8639 : le titre versionné sert aussi au rapprochement
/// (favoris radio #1235, transferts de playlists, Tune Circle). Une requête
/// sans le mix doit toujours trouver la piste ; une requête qui nomme le mix
/// doit choisir le bon.
#[test]
fn le_rapprochement_ignore_une_version_que_la_requete_ne_nomme_pas_8271() {
    use crate::streaming::matching::best_stream_match_scored;
    let claire = QobuzService::map_track(&piste(1, Some("Bright-Side Mix")));
    let sombre = QobuzService::map_track(&piste(2, Some("Dark-Side Mix")));

    let (trouvee, score) = best_stream_match_scored(
        "Panopticom",
        "Peter Gabriel",
        "",
        0,
        std::slice::from_ref(&claire),
    )
    .expect("une requête sans le mix trouve la piste versionnée");
    assert_eq!(trouvee.id, "1");
    assert!(
        score >= crate::streaming::matching::MATCH_ACCEPT_SCORE,
        "score {score}"
    );

    let candidats = [claire, sombre];
    let (choisie, _) = best_stream_match_scored(
        "Panopticom (Dark-Side Mix)",
        "Peter Gabriel",
        "",
        0,
        &candidats,
    )
    .expect("le mix nommé est trouvé");
    assert_eq!(choisie.id, "2", "le mix nommé départage");
}

#[test]
fn titre_a_rapprocher_ne_coupe_que_la_parenthese_finale_absente_8271() {
    use crate::streaming::matching::titre_a_rapprocher;
    assert_eq!(
        titre_a_rapprocher("Panopticom (Bright-Side Mix)", "panopticom"),
        "Panopticom"
    );
    assert_eq!(
        titre_a_rapprocher(
            "Panopticom (Bright-Side Mix)",
            "Panopticom (bright-side mix)"
        ),
        "Panopticom (Bright-Side Mix)"
    );
    assert_eq!(titre_a_rapprocher("Panopticom", "x"), "Panopticom");
    assert_eq!(titre_a_rapprocher("(Untitled)", "x"), "(Untitled)");
}
