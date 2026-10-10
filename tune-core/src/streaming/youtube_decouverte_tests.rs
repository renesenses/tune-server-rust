//! Analyseurs de la découverte YouTube Music, sur des réponses RÉELLES
//! d'InnerTube enregistrées le 07/10/2026 (client `WEB_REMIX`, sans compte),
//! réduites à trois éléments par rayon. Aucun réseau.
use super::*;

fn fixture(nom: &str) -> Value {
    let chemin = format!(
        "{}/tests/fixtures/youtube/{nom}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&chemin).expect(&chemin)).expect("json")
}

#[test]
fn accueil_rend_ses_rayons_avec_titres_pochettes_et_types() {
    let rayons = parser_rayons(&fixture("home"));
    let titres: Vec<&str> = rayons.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titres, ["Quick picks", "Today's biggest hits", "Throwback"]);

    // « Quick picks » : des TITRES (lignes `musicResponsiveListItemRenderer`),
    // que `parse_home_sections` jetait faute de `musicTwoRowItemRenderer`.
    let piste = &rayons[0].items[0];
    assert_eq!(piste.kind, TypeElement::Track);
    assert_eq!(piste.id, "V-uIp-WuD60");
    assert_eq!(piste.title, "Patient Zero");
    // « Taylor Swift • 19M plays » : seul le nom reste, le client en fait un
    // lien vers l'artiste.
    assert_eq!(piste.subtitle, "Taylor Swift");
    assert!(piste.cover_path.as_deref().unwrap().starts_with("https://"));

    let playlist = &rayons[1].items[0];
    assert_eq!(playlist.kind, TypeElement::Playlist);
    assert_eq!(playlist.id, "VLRDCLAK5uy_lBNUteBRencHzKelu5iDHwLF6mYqjL-JU");
    assert!(playlist.cover_path.is_some());
    assert!(rayons.iter().all(|r| r.items.len() == 3));
}

#[test]
fn tendances_rendent_les_classements_et_les_artistes_du_pays() {
    let rayons = parser_rayons(&fixture("charts_fr"));
    let titres: Vec<&str> = rayons.iter().map(|r| r.title.as_str()).collect();
    // Le sélecteur de pays (un `musicShelfRenderer` sans contenu) n'est pas un
    // rayon : il est écarté.
    assert_eq!(titres, ["Video charts", "Top artists"]);
    let classement = &rayons[0].items[0];
    assert_eq!(classement.kind, TypeElement::Playlist);
    assert_eq!(classement.title, "Trending 20 France");
    assert_eq!(classement.id, "VLOLAK5uy_mnRyLDuByhBA_r8-L9ugjllTxfVytwAp0");
    let artiste = &rayons[1].items[0];
    assert_eq!(artiste.kind, TypeElement::Artist);
    assert_eq!(artiste.title, "Jul");
    assert_eq!(artiste.id, "UCYO-8CIkoBoUG2nOWz57Q9g");
}

#[test]
fn ambiances_rendent_les_deux_groupes_et_leurs_params() {
    let groupes = parser_ambiances(&fixture("moods"));
    let titres: Vec<&str> = groupes.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(titres, ["Moods & moments", "Genres"]);
    assert_eq!(groupes[0].items.len(), 14);
    assert_eq!(groupes[1].items.len(), 26);
    assert_eq!(
        groupes[0].items[0],
        Ambiance {
            title: "Autumn".into(),
            params: "ggMPOg1uX3JBUDJTM2ZUUVJM".into()
        }
    );
    assert_eq!(groupes[1].items[0].title, "African");
}

#[test]
fn contenu_d_une_ambiance_rend_des_playlists_ouvrables() {
    let rayons = parser_rayons(&fixture("mood_category"));
    assert_eq!(rayons.len(), 4);
    assert_eq!(rayons[0].title, "Coffee shop blends");
    let p = &rayons[0].items[0];
    assert_eq!(p.kind, TypeElement::Playlist);
    assert_eq!(p.title, "Coffee Shop Blend");
    assert_eq!(p.id, "VLRDCLAK5uy_nBE4bLuBHUXWZrF59ZrkPEToKt8M_I3Vc");
    assert!(p.cover_path.is_some());
}

#[test]
fn entete_de_playlist_lu_dans_la_nouvelle_disposition() {
    // Page RÉELLE de la playlist ouverte depuis l'ambiance ci-dessus.
    let e = entete_playlist(&fixture("playlist"));
    assert_eq!(e.title.as_deref(), Some("Coffee Shop Blend"));
    assert_eq!(
        e.description.as_deref(),
        Some("A mellow brew of everything from Americana to indie.")
    );
    assert!(e.cover_path.as_deref().unwrap().starts_with("https://"));
}

#[test]
fn entete_de_playlist_garde_l_ancienne_disposition() {
    let data = serde_json::json!({"header": {"musicDetailHeaderRenderer": {
        "title": {"runs": [{"text": "Ancienne"}]},
        "description": {"runs": [{"text": "d"}]},
        "thumbnail": {"croppedSquareThumbnailRenderer": {"thumbnail": {"thumbnails": [
            {"url": "https://petite"}, {"url": "https://grande"}]}}}
    }}});
    let e = entete_playlist(&data);
    assert_eq!(e.title.as_deref(), Some("Ancienne"));
    assert_eq!(e.cover_path.as_deref(), Some("https://grande"));
}

#[test]
fn une_page_inconnue_ne_fabrique_rien() {
    let vide = serde_json::json!({"contents": {}});
    assert!(parser_rayons(&vide).is_empty());
    assert!(parser_ambiances(&vide).is_empty());
    assert_eq!(entete_playlist(&vide), EntetePlaylist::default());
}

#[test]
fn code_pays_refuse_ce_qui_n_est_pas_deux_lettres() {
    assert_eq!(code_pays("fr").as_deref(), Some("FR"));
    assert_eq!(code_pays(" ZZ ").as_deref(), Some("ZZ"));
    for mauvais in ["", "F", "FRA", "F1", "é1", "../"] {
        assert_eq!(code_pays(mauvais), None, "{mauvais}");
    }
}

#[test]
fn le_reglage_du_pays_l_emporte_sur_la_langue_du_navigateur() {
    assert_eq!(
        choisir_pays(Some("de"), Some("FR")),
        ("DE".to_string(), OriginePays::Reglage)
    );
    assert_eq!(
        choisir_pays(None, Some("FR")),
        ("FR".to_string(), OriginePays::Requete)
    );
    // Réglage vide = automatique.
    assert_eq!(
        choisir_pays(Some(""), Some("FR")),
        ("FR".to_string(), OriginePays::Requete)
    );
    assert_eq!(
        choisir_pays(None, None),
        ("ZZ".to_string(), OriginePays::Monde)
    );
    // Un réglage illisible n'écrase pas la requête.
    assert_eq!(
        choisir_pays(Some("xyz"), None),
        ("ZZ".to_string(), OriginePays::Monde)
    );
}
