//! Essais purs de l'annuaire v2 (#6516) : aucun réseau, aucune base.

use super::*;

const EXTRAIT: &str = include_str!("../../tests/fixtures/annuaire_v2_extrait_20261019.json");

fn fiches() -> Vec<StationAnnuaire> {
    let corps: Value = serde_json::from_str(EXTRAIT).unwrap();
    annuaire_depuis_json(&radios_du_corps(corps))
}

fn station(id: i64, nom: &str, url: &str) -> RadioStation {
    RadioStation {
        id: Some(id),
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

#[test]
fn une_fiche_refusee_ou_retiree_n_entre_pas_dans_l_annuaire() {
    let f = fiches();
    let noms: Vec<&str> = f.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(f.len(), 4, "{noms:?}");
    assert!(!noms.contains(&"FIP Latino"), "refusée : {noms:?}");
    assert!(!noms.contains(&"FIP Tout nouveau"), "retirée : {noms:?}");
}

#[test]
fn la_fiche_v2_garde_cle_logo_absolu_et_tous_ses_flux() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    assert_eq!(fip.external_id.as_deref(), Some("S000973"));
    assert_eq!(fip.radio_metadata_key.as_deref(), Some("fip"));
    assert_eq!(
        fip.logo_url.as_deref(),
        Some("https://mozaiklabs.fr/storage/radio-logos/01KV2K6H8392ZH6JA8NNK4904E.png")
    );
    assert!(
        fip.stream_urls
            .contains(&"http://radio-metadata.fr:8000/fiproon".to_string())
    );
    assert_eq!(
        fip.stream_urls[0], fip.stream_url,
        "le flux principal d'abord"
    );
    assert_eq!(
        (fip.codec.as_deref(), fip.bitrate),
        (Some("AAC"), Some(192))
    );
    assert_eq!(fip.categories, vec!["eclectic".to_string()]);
}

#[test]
fn une_entree_v1_se_lit_toujours() {
    let v1 = serde_json::json!([{"name": "X", "stream_url": "https://x.example/s", "logo_url": "/storage/x.png"}]);
    let f = annuaire_depuis_json(v1.as_array().unwrap());
    assert_eq!(f.len(), 1);
    assert_eq!(
        f[0].logo_url.as_deref(),
        Some("https://mozaiklabs.fr/storage/x.png")
    );
    assert_eq!(f[0].stream_urls, vec!["https://x.example/s".to_string()]);
}

#[test]
fn mes_radios_se_rapprochent_par_flux_puis_par_nom() {
    let f = fiches();
    let index = IndexAnnuaire::new(&f);
    // Le semis : même adresse, autre schéma et casse.
    let fip = index
        .fiche_de("FIP", "HTTPS://icecast.radiofrance.fr/fip-hifi.aac/")
        .unwrap();
    assert_eq!(fip.external_id.as_deref(), Some("S000973"));
    // Le semis de FIP Nouveautés n'a pas le `?id=radiofrance` de l'annuaire :
    // c'est le nom replié qui le rapproche.
    let nouv = index
        .fiche_de(
            "FIP Nouveautés",
            "https://icecast.radiofrance.fr/fipnouveautes-hifi.aac",
        )
        .unwrap();
    assert_eq!(nouv.external_id.as_deref(), Some("S000981"));
    // Une station que l'annuaire a refusée ne se rapproche de rien.
    assert!(
        index
            .fiche_de(
                "FIP Latino",
                "https://icecast.radiofrance.fr/fiplatino-hifi.aac"
            )
            .is_none()
    );
}

#[test]
fn l_annuaire_filtre_par_recherche_pays_et_genre_et_signale_mes_radios() {
    let f = fiches();
    let mes = vec![
        station(7, "FIP", "https://icecast.radiofrance.fr/fip-hifi.aac"),
        station(
            9,
            "FIP Latino",
            "https://icecast.radiofrance.fr/fiplatino-hifi.aac",
        ),
    ];
    let etat = EtatAnnuaire {
        origine: "reseau",
        ..Default::default()
    };
    let tout = corps_annuaire(&f, &mes, &FiltresAnnuaire::default(), &etat);
    assert_eq!(tout["total"], 4);
    assert_eq!(
        tout["attribution"]["support_url"],
        "https://ko-fi.com/aleceiffel"
    );
    let fip = tout["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert_eq!(fip["my_radio_id"], 7);
    assert_eq!(fip["now_playing"], true);

    let jazz = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            q: Some("jazz".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(jazz["total"], 1, "{jazz}");
    assert_eq!(jazz["items"][0]["external_id"], "S000978");

    let us = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            country: Some("us".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(us["total"], 1, "{us}");
    assert_eq!(us["items"][0]["name"], "Radio Paradise: Main Mix");
    // Les genres proposés sont ceux du pays choisi.
    assert_eq!(us["genres"].as_array().unwrap().len(), 1);

    let latino = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            q: Some("latino".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(
        latino["total"], 0,
        "une station refusée n'est pas dans l'annuaire"
    );
}

#[test]
fn la_pagination_reste_dans_ses_bornes() {
    let f = fiches();
    let etat = EtatAnnuaire::default();
    let page = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire {
            offset: Some(3),
            limit: Some(2),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(
        (page["total"].as_u64(), page["count"].as_u64()),
        (Some(4), Some(1))
    );
    let loin = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire {
            offset: Some(99),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(loin["count"], 0);
}

#[test]
fn une_station_creee_depuis_l_annuaire_porte_son_flux_principal() {
    let f = fiches();
    let rp = f
        .iter()
        .find(|s| s.external_id.as_deref() == Some("S002479"))
        .unwrap();
    let st = station_depuis_fiche(rp);
    assert_eq!(st.url, rp.stream_url);
    assert_eq!(st.logo_url, rp.logo_url);
    assert_eq!(st.country.as_deref(), Some("US"));
    assert!(!st.is_favorite);
}

#[test]
fn une_fiche_rapprochee_par_le_nom_seul_ne_prete_pas_son_logo() {
    // Fil du 19/10 : le logo d'une station ne vient que de la fiche qui lui
    // est liée (`external_id`) ou qui partage son adresse de flux.
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let ailleurs = "https://ailleurs.example/fip";
    let par_nom = rattachement(fip, Lien::Nom, ailleurs);
    assert!(par_nom["logo_url"].is_null(), "{par_nom}");
    assert_eq!(par_nom["rapproche_par"], "nom");
    let par_flux = rattachement(fip, Lien::Flux, &fip.stream_url);
    assert_eq!(par_flux["logo_url"], serde_json::json!(fip.logo_url));
    assert_eq!(par_flux["flux_change"], false);
    let par_id = rattachement(fip, Lien::Id, ailleurs);
    assert_eq!(par_id["logo_url"], serde_json::json!(fip.logo_url));
    assert_eq!(par_id["flux_change"], true);
    assert_eq!(
        par_id["stream_url_annuaire"],
        serde_json::json!(fip.stream_url)
    );
    // Le nom suffit toujours au rapprochement (pas de doublon à l'ajout).
    let index = IndexAnnuaire::new(&f);
    assert_eq!(
        index.lien(None, "FIP", ailleurs).map(|(_, l)| l),
        Some(Lien::Nom)
    );
    assert_eq!(
        index.lien(Some("S000973"), "x", ailleurs).map(|(_, l)| l),
        Some(Lien::Id)
    );
    // Lié à une fiche sortie de l'annuaire : aucun repli par l'adresse.
    assert_eq!(index.lien(Some("S999001"), "FIP", &fip.stream_url), None);
}

// ── #7177 (web#2443) — les flux d'une fiche, et le flux préféré ─────────────

const FIP_MIDFI: &str = "https://icecast.radiofrance.fr/fip-midfi.mp3?id=radiofrance";

#[test]
fn les_flux_proposes_d_une_fiche_v2_portent_format_et_debit_le_principal_d_abord() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let flux = fip.flux_proposes();
    assert_eq!(flux.len(), 4, "{flux:?}");
    assert_eq!(flux[0].url, fip.stream_url);
    assert!(flux[0].primary);
    assert_eq!(flux.iter().filter(|x| x.primary).count(), 1);
    let mp3 = flux.iter().find(|x| x.url == FIP_MIDFI).unwrap();
    assert_eq!(
        (mp3.codec.as_deref(), mp3.bitrate, mp3.sample_rate, mp3.hls),
        (Some("MP3"), Some(128), Some(48000), false)
    );
}

#[test]
fn une_entree_v1_propose_son_seul_flux_sans_format() {
    let v1 = serde_json::json!([{"name": "X", "stream_url": "https://x.example/s"}]);
    let f = annuaire_depuis_json(v1.as_array().unwrap());
    let flux = f[0].flux_proposes();
    assert_eq!(flux.len(), 1);
    assert_eq!(flux[0].url, "https://x.example/s");
    assert!(flux[0].primary);
    assert_eq!((flux[0].codec.as_deref(), flux[0].bitrate), (None, None));
}

#[test]
fn le_flux_prefere_se_choisit_parmi_ceux_de_la_fiche_avec_son_format() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let mut st = station(1, "FIP", &fip.stream_url);
    assert!(poser_le_flux_prefere(fip, &mut st, FIP_MIDFI));
    assert_eq!(st.url, FIP_MIDFI);
    assert_eq!((st.codec.as_deref(), st.bitrate), (Some("mp3"), Some(128)));
    // Le rattrapage de fond (#7043) le garde : la fiche le publie toujours.
    assert!(!crate::routes::radios::flux_a_poser(
        &f,
        &mut st,
        Some("S000973")
    ));
    assert_eq!(st.url, FIP_MIDFI);
}

#[test]
fn contre_epreuve_une_adresse_qui_n_est_pas_de_la_fiche_ne_change_rien() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let mut st = station(1, "FIP", &fip.stream_url);
    assert!(!poser_le_flux_prefere(
        fip,
        &mut st,
        "https://ailleurs.example/fip.mp3"
    ));
    assert_eq!(st.url, fip.stream_url);
    assert_eq!((st.codec.as_deref(), st.bitrate), (None, None));
}

/// #7194 (web#2400) — la carte des radios fait ses réseaux des mots-clés de
/// la fiche (le diffuseur) : `/radios/directory` doit les rendre.
#[test]
fn l_annuaire_rend_les_mots_cles_de_chaque_fiche_pour_les_reseaux_de_la_carte() {
    let f = fiches();
    let etat = EtatAnnuaire::default();
    let corps = corps_annuaire(&f, &[], &FiltresAnnuaire::default(), &etat);
    let items = corps["items"].as_array().unwrap();
    let fip = items
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert_eq!(
        fip["keywords"],
        serde_json::json!(["Radio France"]),
        "le diffuseur de FIP doit être servi : {fip}"
    );
    let paradise = items
        .iter()
        .find(|i| i["name"] == "Radio Paradise: Main Mix")
        .unwrap();
    assert_eq!(
        paradise["keywords"],
        serde_json::json!([]),
        "aucun mot-clé : liste vide"
    );
}

/// #7215 — trois fiches au format du site du 27/10/2026 (`genres`) et une
/// fiche d'un site plus ancien (`genre` seul).
fn fiches_genres_7215() -> Vec<StationAnnuaire> {
    let brut = serde_json::json!([
        {"external_id": "S100001", "name": "OuiFM Blues & Rock", "stream_url": "https://ouifm.example/bluesrock",
         "country": "FR", "genre": "blues", "genres": ["blues", "rock"]},
        {"external_id": "S100002", "name": "FIP Metal", "stream_url": "https://fip.example/metal",
         "country": "FR", "genre": "rock", "genres": ["rock", "metal", "Rock", " "]},
        {"external_id": "S100003", "name": "Radio Tout", "stream_url": "https://tout.example/live",
         "country": "BE", "genre": "eclectic", "genres": ["eclectic"]},
        {"external_id": "S100004", "name": "Vieux Jazz", "stream_url": "https://jazz.example/live",
         "country": "FR", "genre": "jazz"}
    ]);
    annuaire_depuis_json(brut.as_array().unwrap())
}

fn ids_du_genre_7215(f: &[StationAnnuaire], genre: &str) -> Vec<String> {
    let corps = corps_annuaire(
        f,
        &[],
        &FiltresAnnuaire {
            genre: Some(genre.into()),
            ..Default::default()
        },
        &EtatAnnuaire::default(),
    );
    corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["external_id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn temoin_7215_ouifm_blues_et_rock_sort_sous_chacun_de_ses_genres() {
    let f = fiches_genres_7215();
    assert_eq!(
        ids_du_genre_7215(&f, "blues"),
        vec!["S100001"],
        "OuiFM Blues & Rock doit sortir sous blues"
    );
    assert_eq!(
        ids_du_genre_7215(&f, "rock"),
        vec!["S100001", "S100002"],
        "OuiFM Blues & Rock doit AUSSI sortir sous rock (son 2e genre), FIP Metal sous rock"
    );
    assert_eq!(ids_du_genre_7215(&f, "METAL"), vec!["S100002"]);
}

#[test]
fn les_genres_7215_sont_lus_sans_doublon_et_rendus_par_l_annuaire() {
    let f = fiches_genres_7215();
    let metal = f.iter().find(|s| s.name == "FIP Metal").unwrap();
    assert_eq!(metal.genre.as_deref(), Some("rock"));
    assert_eq!(metal.genres, vec!["rock".to_string(), "metal".to_string()]);

    let corps = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire::default(),
        &EtatAnnuaire::default(),
    );
    let ouifm = corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["external_id"] == "S100001")
        .unwrap();
    assert_eq!(ouifm["genre"], "blues", "le genre v1 reste le premier");
    assert_eq!(ouifm["genres"], serde_json::json!(["blues", "rock"]));
}

#[test]
fn les_compteurs_7215_comptent_une_fiche_dans_chacun_de_ses_genres() {
    let f = fiches_genres_7215();
    let corps = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire::default(),
        &EtatAnnuaire::default(),
    );
    let comptes: std::collections::BTreeMap<String, u64> = corps["genres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["key"].as_str().unwrap().to_string(),
                g["count"].as_u64().unwrap(),
            )
        })
        .collect();
    let attendu: std::collections::BTreeMap<String, u64> = [
        ("rock", 2),
        ("blues", 1),
        ("metal", 1),
        ("eclectic", 1),
        ("jazz", 1),
    ]
    .into_iter()
    .map(|(k, n)| (k.to_string(), n))
    .collect();
    assert_eq!(comptes, attendu, "{}", corps["genres"]);

    // Les genres proposés suivent le pays choisi.
    let be = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire {
            country: Some("BE".into()),
            ..Default::default()
        },
        &EtatAnnuaire::default(),
    );
    assert_eq!(
        be["genres"],
        serde_json::json!([{"key": "eclectic", "count": 1}])
    );
}

#[test]
fn ancien_format_7215_sans_genres_se_replie_sur_genre() {
    let f = fiches_genres_7215();
    let vieux = f.iter().find(|s| s.name == "Vieux Jazz").unwrap();
    assert_eq!(vieux.genres, vec!["jazz".to_string()]);
    assert_eq!(ids_du_genre_7215(&f, "jazz"), vec!["S100004"]);

    // Une fiche construite sans `genres` (aucun site) : repli sur `genre`.
    let construite = StationAnnuaire {
        name: "Construite".into(),
        stream_url: "https://c.example/s".into(),
        external_id: Some("C000001".into()),
        genre: Some("folk".into()),
        ..Default::default()
    };
    assert_eq!(construite.genres_ou_genre(), vec!["folk"]);
    assert_eq!(ids_du_genre_7215(&[construite], "folk"), vec!["C000001"]);
}

#[test]
fn contre_epreuve_7215_un_genre_que_la_fiche_n_a_pas_ne_la_trouve_pas() {
    let f = fiches_genres_7215();
    assert!(
        ids_du_genre_7215(&f, "metal")
            .iter()
            .all(|id| id != "S100001")
    );
    assert!(
        ids_du_genre_7215(&f, "jazz")
            .iter()
            .all(|id| id != "S100001")
    );
    assert!(ids_du_genre_7215(&f, "funk").is_empty());
}
