//! #7466 — témoins des critères, un par critère, chacun avec sa contre-épreuve
//! (la station qui ne doit PAS passer), plus une combinaison.

use super::*;
use crate::routes::radios::FluxAnnuaire;
use crate::routes::radios_annuaire::{EtatAnnuaire, FiltresAnnuaire, corps_annuaire};
use serde_json::json;

fn flux(url: &str, codec: &str, debit: i32, frequence: i32) -> FluxAnnuaire {
    FluxAnnuaire {
        url: url.into(),
        codec: Some(codec.into()),
        bitrate: Some(debit),
        sample_rate: Some(frequence),
        ..Default::default()
    }
}

/// Une fiche FLAC 96 kHz, jazz français, notée 4,5, avec Metadata+.
fn fiche_flac() -> StationAnnuaire {
    StationAnnuaire {
        name: "Jazz Sans Perte".into(),
        stream_url: "https://exemple.invalid/jazz.flac".into(),
        country: Some("FR".into()),
        genre: Some("jazz".into()),
        genres: vec!["jazz".into(), "blues".into()],
        codec: Some("FLAC".into()),
        bitrate: Some(1411),
        flux: vec![flux(
            "https://exemple.invalid/jazz.flac",
            "FLAC",
            1411,
            96_000,
        )],
        external_id: Some("S1".into()),
        radio_metadata_key: Some("jazzsp".into()),
        note_centiemes: Some(450),
        nombre_de_notes: 2,
        ..Default::default()
    }
}

/// Une fiche MP3 128 à 44,1 kHz, info américaine, jamais notée, sans clé.
fn fiche_mp3() -> StationAnnuaire {
    StationAnnuaire {
        name: "News Compresse".into(),
        stream_url: "https://exemple.invalid/news.mp3".into(),
        country: Some("US".into()),
        genre: Some("news".into()),
        codec: Some("MP3".into()),
        bitrate: Some(128),
        flux: vec![flux("https://exemple.invalid/news.mp3", "MP3", 128, 44_100)],
        external_id: Some("S2".into()),
        ..Default::default()
    }
}

/// Les filtres tels qu'axum les lit dans la chaîne de requête.
fn filtres(requete: &str) -> FiltresAnnuaire {
    let uri: axum::http::Uri = format!("/radios/directory?{requete}").parse().unwrap();
    let axum::extract::Query(f) =
        axum::extract::Query::try_from_uri(&uri).expect("requête lisible");
    f
}

fn criteres(requete: &str) -> Criteres {
    filtres(requete).criteres()
}

fn retenues(requete: &str) -> Vec<String> {
    let c = criteres(requete);
    [fiche_flac(), fiche_mp3()]
        .into_iter()
        .filter(|f| c.retient_la_fiche(f, None))
        .map(|f| f.name)
        .collect()
}

#[test]
fn sans_critere_rien_n_est_filtre() {
    assert!(criteres("").est_vide());
    assert!(criteres("q=fip&offset=0&limit=10").est_vide());
    assert_eq!(retenues(""), ["Jazz Sans Perte", "News Compresse"]);
}

#[test]
fn chaque_critere_retient_la_bonne_fiche_et_ecarte_l_autre() {
    for (requete, attendu) in [
        ("genre=jazz", vec!["Jazz Sans Perte"]),
        ("genre=blues", vec!["Jazz Sans Perte"]),
        ("genre=news", vec!["News Compresse"]),
        ("genre=news,jazz", vec!["Jazz Sans Perte", "News Compresse"]),
        ("country=fr", vec!["Jazz Sans Perte"]),
        ("country=US", vec!["News Compresse"]),
        ("codec=flac", vec!["Jazz Sans Perte"]),
        ("codec=mp3", vec!["News Compresse"]),
        ("bitrate_min=320", vec!["Jazz Sans Perte"]),
        ("bitrate_min=128", vec!["Jazz Sans Perte", "News Compresse"]),
        ("lossless=true", vec!["Jazz Sans Perte"]),
        ("lossless=false", vec!["Jazz Sans Perte", "News Compresse"]),
        ("hires=true", vec!["Jazz Sans Perte"]),
        ("rated_min=4", vec!["Jazz Sans Perte"]),
        ("rated_min=4.6", vec![]),
        ("metadata_plus=true", vec!["Jazz Sans Perte"]),
        ("favorites_only=true", vec![]),
    ] {
        assert_eq!(retenues(requete), attendu, "critère {requete:?}");
    }
}

#[test]
fn les_criteres_se_combinent_en_et() {
    assert_eq!(
        retenues("country=FR&lossless=true&hires=true&genre=jazz"),
        ["Jazz Sans Perte"]
    );
    // Contre-épreuve : un seul critère contraire suffit à tout écarter.
    assert!(retenues("country=FR&codec=mp3").is_empty());
    assert!(retenues("genre=news&lossless=true").is_empty());
}

#[test]
fn hires_veut_plus_de_48_khz() {
    let mut f = fiche_flac();
    f.flux[0].sample_rate = Some(48_000);
    assert!(
        !criteres("hires=true").retient_la_fiche(&f, None),
        "48 kHz n'est pas de la haute résolution"
    );
    f.flux[0].sample_rate = Some(88_200);
    assert!(criteres("hires=true").retient_la_fiche(&f, None));
}

#[test]
fn favoris_de_la_fiche_selon_mes_radios() {
    let c = criteres("favorites_only=true");
    assert!(c.retient_la_fiche(&fiche_mp3(), Some(true)));
    assert!(!c.retient_la_fiche(&fiche_mp3(), Some(false)));
    assert!(!c.retient_la_fiche(&fiche_mp3(), None));
}

#[test]
fn la_note_se_lit_dans_la_fiche_v2() {
    assert_eq!(
        note_centiemes(&json!({"avg_rating": 4.7, "rating_count": 1})),
        Some(470)
    );
    assert_eq!(
        note_centiemes(&json!({"avg_rating": "4.5", "rating_count": "2"})),
        Some(450)
    );
    // Personne n'a noté : pas de note, même si `avg_rating` vaut 0.
    assert_eq!(
        note_centiemes(&json!({"avg_rating": 0, "rating_count": 0})),
        None
    );
    assert_eq!(note_centiemes(&json!({"name": "x"})), None);
}

fn station(
    nom: &str,
    url: &str,
    pays: &str,
    genre: &str,
    codec: Option<&str>,
    debit: Option<i32>,
    favori: bool,
) -> RadioStation {
    RadioStation {
        id: None,
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: Some(pays.into()),
        language: None,
        genre: Some(genre.into()),
        codec: codec.map(Into::into),
        bitrate: debit,
        is_favorite: favori,
        last_played: None,
        play_count: 0,
    }
}

fn mes_radios() -> Vec<RadioStation> {
    vec![
        // Écrite en clair par le semis, sans format : la fiche le donne.
        station(
            "Ma Jazz",
            "https://exemple.invalid/jazz.flac",
            "France",
            "Jazz",
            None,
            None,
            true,
        ),
        station(
            "Mon Rock",
            "https://exemple.invalid/rock.mp3",
            "Royaume-Uni",
            "Rock",
            Some("MP3"),
            Some(128),
            false,
        ),
    ]
}

fn locales(requete: &str) -> Vec<String> {
    criteres(requete)
        .filtrer_les_stations(mes_radios(), &[fiche_flac(), fiche_mp3()], &HashMap::new())
        .into_iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn mes_radios_pays_et_genre_en_clair() {
    assert_eq!(locales("country=FR"), ["Ma Jazz"]);
    assert_eq!(locales("country=GB"), ["Mon Rock"]);
    // « Classique »/« Jazz » du semis répondent aux clés de l'annuaire.
    assert_eq!(locales("genre=jazz"), ["Ma Jazz"]);
    assert_eq!(locales("genre=radio.genre.rock"), ["Mon Rock"]);
    assert!(locales("genre=classical").is_empty());
}

#[test]
fn mes_radios_format_frequence_note_et_metadata_par_la_fiche() {
    assert_eq!(locales("codec=flac"), ["Ma Jazz"]);
    assert_eq!(locales("lossless=true"), ["Ma Jazz"]);
    assert_eq!(locales("hires=true"), ["Ma Jazz"]);
    assert_eq!(locales("rated_min=4"), ["Ma Jazz"]);
    assert_eq!(locales("metadata_plus=true"), ["Ma Jazz"]);
    assert_eq!(locales("bitrate_min=200"), ["Ma Jazz"]);
    assert_eq!(locales("codec=mp3"), ["Mon Rock"]);
    assert_eq!(locales("favorites_only=true"), ["Ma Jazz"]);
    assert_eq!(
        locales("favorites_only=true&codec=mp3"),
        Vec::<String>::new()
    );
}

fn noms(corps: &serde_json::Value) -> Vec<&str> {
    corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect()
}

#[test]
fn l_annuaire_filtre_et_compte_selon_les_criteres() {
    let fiches = [fiche_flac(), fiche_mp3()];
    let tout = corps_annuaire(
        &fiches,
        &[],
        &FiltresAnnuaire::default(),
        &EtatAnnuaire::default(),
    );
    assert_eq!(tout["total"], 2);

    let f = filtres("lossless=true");
    let corps = corps_annuaire(&fiches, &[], &f, &EtatAnnuaire::default());
    assert_eq!(noms(&corps), ["Jazz Sans Perte"]);
    assert_eq!(corps["total"], 1);
    // Les facettes se comptent sous les critères : plus de pays US.
    assert_eq!(corps["countries"], json!([{"code": "FR", "count": 1}]));
    assert_eq!(corps["items"][0]["avg_rating"], json!(4.5));
    assert_eq!(corps["items"][0]["rating_count"], json!(2));

    let f = filtres("genre=news,jazz&metadata_plus=true");
    assert_eq!(
        noms(&corps_annuaire(&fiches, &[], &f, &EtatAnnuaire::default())),
        ["Jazz Sans Perte"]
    );

    // Favoris : la fiche dont la station de « Mes radios » est en favori.
    let f = filtres("favorites_only=true");
    assert_eq!(
        noms(&corps_annuaire(
            &fiches,
            &mes_radios(),
            &f,
            &EtatAnnuaire::default()
        )),
        ["Jazz Sans Perte"]
    );
    assert!(noms(&corps_annuaire(&fiches, &[], &f, &EtatAnnuaire::default())).is_empty());
}

// --- #7718 — le niveau de qualité minimal, sans distinction de format -------

/// Une fiche classique dont le seul flux sans perte ne publie ni débit ni
/// fréquence (le cas fréquent des flux FLAC de l'annuaire).
fn fiche_flac_muette() -> StationAnnuaire {
    StationAnnuaire {
        name: "Classique Muette".into(),
        stream_url: "https://exemple.invalid/classique.flac".into(),
        genre: Some("classical".into()),
        codec: Some("FLAC".into()),
        flux: vec![FluxAnnuaire {
            url: "https://exemple.invalid/classique.flac".into(),
            codec: Some("FLAC".into()),
            ..Default::default()
        }],
        external_id: Some("S3".into()),
        ..Default::default()
    }
}

/// Une fiche classique à deux flux avec perte : AAC 64 et MP3 192.
fn fiche_classique_192() -> StationAnnuaire {
    StationAnnuaire {
        name: "Classique 192".into(),
        stream_url: "https://exemple.invalid/classique-64.aac".into(),
        genre: Some("classical".into()),
        codec: Some("AAC".into()),
        bitrate: Some(64),
        flux: vec![
            flux(
                "https://exemple.invalid/classique-64.aac",
                "AAC",
                64,
                44_100,
            ),
            flux(
                "https://exemple.invalid/classique-192.mp3",
                "MP3",
                192,
                44_100,
            ),
        ],
        external_id: Some("S4".into()),
        ..Default::default()
    }
}

fn retenues_parmi_quatre(requete: &str) -> Vec<String> {
    let c = criteres(requete);
    [
        fiche_flac(),
        fiche_mp3(),
        fiche_flac_muette(),
        fiche_classique_192(),
    ]
    .into_iter()
    .filter(|f| c.retient_la_fiche(f, None))
    .map(|f| f.name)
    .collect()
}

#[test]
fn qualite_min_lit_les_niveaux_et_ignore_l_inconnu() {
    assert_eq!(
        NiveauQualite::depuis("128"),
        Some(NiveauQualite::Debit(128))
    );
    assert_eq!(
        NiveauQualite::depuis("FLAC_24_96"),
        Some(NiveauQualite::SansPerte(96_000))
    );
    assert_eq!(
        NiveauQualite::depuis("flac_16_44"),
        Some(NiveauQualite::SansPerte(44_100))
    );
    for rien in ["", "unknown", "0", "-5", "mp3"] {
        assert_eq!(NiveauQualite::depuis(rien), None, "{rien:?}");
    }
    assert!(criteres("quality_min=unknown").est_vide());
    assert!(!criteres("quality_min=48").est_vide());
}

#[test]
fn un_flux_sans_perte_sans_debit_passe_tout_niveau_avec_perte() {
    for requete in ["quality_min=320", "quality_min=48", "bitrate_min=320"] {
        assert!(
            retenues_parmi_quatre(requete).contains(&"Classique Muette".to_string()),
            "{requete:?} : un flux FLAC sans débit publié est au moins aussi bon \
             que 320 kbit/s, il ne doit pas être écarté"
        );
    }
    assert!(criteres("quality_min=flac_16_44").retient_la_fiche(&fiche_flac_muette(), None));
    // Sans fréquence publiée, rien ne dit que c'est de la haute résolution.
    assert!(!criteres("quality_min=flac_24_48").retient_la_fiche(&fiche_flac_muette(), None));
}

#[test]
fn qualite_min_garde_les_stations_dont_un_flux_atteint_le_niveau() {
    for (requete, attendu) in [
        (
            "quality_min=128",
            vec![
                "Jazz Sans Perte",
                "News Compresse",
                "Classique Muette",
                "Classique 192",
            ],
        ),
        (
            "quality_min=192",
            vec!["Jazz Sans Perte", "Classique Muette", "Classique 192"],
        ),
        (
            "quality_min=256",
            vec!["Jazz Sans Perte", "Classique Muette"],
        ),
        (
            "quality_min=flac_16_44",
            vec!["Jazz Sans Perte", "Classique Muette"],
        ),
        ("quality_min=flac_24_96", vec!["Jazz Sans Perte"]),
        ("quality_min=flac_24_192", vec![]),
    ] {
        assert_eq!(
            retenues_parmi_quatre(requete),
            attendu,
            "critère {requete:?}"
        );
    }
}

#[test]
fn qualite_min_se_combine_avec_le_genre() {
    // L'exemple du forum : « Classique » et « au moins 128 ».
    assert_eq!(
        retenues_parmi_quatre("genre=classical&quality_min=128"),
        ["Classique Muette", "Classique 192"]
    );
    assert_eq!(
        retenues_parmi_quatre("genre=classical&quality_min=flac_16_44"),
        ["Classique Muette"]
    );
    assert!(retenues_parmi_quatre("genre=news&quality_min=256").is_empty());
}

#[test]
fn qualite_min_flac_exige_la_frequence_sur_le_meme_flux_sans_perte() {
    // Un flux FLAC à 44,1 kHz et un flux AAC à 96 kHz : aucun flux n'est
    // à la fois sans perte et à 96 kHz.
    let mut f = fiche_flac();
    f.flux = vec![
        flux("https://exemple.invalid/a.flac", "FLAC", 1411, 44_100),
        flux("https://exemple.invalid/b.aac", "AAC", 320, 96_000),
    ];
    assert!(!criteres("quality_min=flac_24_96").retient_la_fiche(&f, None));
    assert!(criteres("quality_min=flac_16_44").retient_la_fiche(&f, None));
}

#[test]
fn mes_radios_qualite_min_par_le_flux_ecoute() {
    assert_eq!(locales("quality_min=320"), ["Ma Jazz"]);
    assert_eq!(locales("quality_min=128"), ["Ma Jazz", "Mon Rock"]);
    assert_eq!(locales("quality_min=flac_24_96"), ["Ma Jazz"]);
    assert!(locales("quality_min=flac_24_192").is_empty());
}

#[test]
fn l_annuaire_compte_ses_facettes_sous_la_qualite_min() {
    let fiches = [fiche_flac(), fiche_mp3(), fiche_classique_192()];
    let f = filtres("quality_min=192");
    let corps = corps_annuaire(&fiches, &[], &f, &EtatAnnuaire::default());
    assert_eq!(noms(&corps), ["Jazz Sans Perte", "Classique 192"]);
    assert_eq!(corps["total"], 2);
    assert_eq!(corps["countries"], json!([{"code": "FR", "count": 1}]));
}

/// Recette rc12 (13/11) — une fiche au seul flux donné, sous un nom.
fn fiche_a_un_flux(nom: &str, codec: Option<&str>, frequence: i32) -> StationAnnuaire {
    let url = format!("https://exemple.invalid/{nom}");
    StationAnnuaire {
        name: nom.into(),
        stream_url: url.clone(),
        codec: codec.map(Into::into),
        flux: vec![FluxAnnuaire {
            url,
            codec: codec.map(Into::into),
            bitrate: Some(320),
            sample_rate: Some(frequence),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn hires_rc12_un_flux_avec_perte_n_est_jamais_hires() {
    let c = criteres("hires=true");
    // Les deux fiches de l'annuaire réel qui passaient à tort le 13/11.
    for (nom, codec, frequence) in [
        ("africa-mp3-192k", Some("MP3"), 192_000),
        ("wrsi-aac-96k", Some("AAC"), 96_000),
        ("aac-plus-96k", Some("AAC+"), 96_000),
        ("opus-96k", Some("opus"), 96_000),
        ("vorbis-96k", Some("Ogg Vorbis"), 96_000),
        ("format-inconnu-96k", None, 96_000),
        ("flac-cd", Some("FLAC"), 44_100),
        ("flac-48k", Some("FLAC"), 48_000),
    ] {
        assert!(
            !c.retient_la_fiche(&fiche_a_un_flux(nom, codec, frequence), None),
            "{nom} : un flux avec perte, de format inconnu ou à 48 kHz au plus n'est pas Hi-Res"
        );
    }
    for (nom, codec, frequence) in [
        ("flac-96k", "FLAC", 96_000),
        ("flac-192k", "flac", 192_000),
        ("alac-88k", "ALAC", 88_200),
        ("ogg-flac-96k", "Ogg FLAC", 96_000),
    ] {
        assert!(
            c.retient_la_fiche(&fiche_a_un_flux(nom, Some(codec), frequence), None),
            "{nom} : un flux sans perte au-delà de 48 kHz est Hi-Res"
        );
    }
}

#[test]
fn hires_rc12_le_flux_sans_perte_doit_etre_celui_de_la_haute_frequence() {
    // Un FLAC 44,1 kHz à côté d'un AAC 96 kHz : aucun flux n'est Hi-Res.
    let mut f = fiche_a_un_flux("melange", Some("FLAC"), 44_100);
    f.flux.push(flux(
        "https://exemple.invalid/melange.aac",
        "AAC",
        320,
        96_000,
    ));
    assert!(!criteres("hires=true").retient_la_fiche(&f, None));
    // Un MP3 principal et un FLAC 192 kHz de secours : la station en a un.
    let mut g = fiche_a_un_flux("secours", Some("MP3"), 44_100);
    g.flux.push(flux(
        "https://exemple.invalid/secours.flac",
        "FLAC",
        9216,
        192_000,
    ));
    assert!(criteres("hires=true").retient_la_fiche(&g, None));
}

#[test]
fn hires_rc12_mes_radios_juge_le_flux_ecoute() {
    // « Mes radios » écoute le MP3 que l'annuaire dit à 192 kHz : pas Hi-Res.
    let fiche = fiche_a_un_flux("africa", Some("MP3"), 192_000);
    let st = station(
        "Africa",
        "https://exemple.invalid/africa",
        "France",
        "Pop",
        None,
        None,
        false,
    );
    assert!(
        criteres("hires=true")
            .filtrer_les_stations(vec![st], &[fiche], &HashMap::new())
            .is_empty(),
        "un MP3 de « Mes radios » ne passe pas le filtre Hi-Res"
    );
}

/// Recette rc12 (14/11, Rémi) — une fiche au seul flux donné : format, débit
/// et fréquence, chacun pouvant manquer.
fn fiche_flux(
    nom: &str,
    codec: &str,
    debit: Option<i32>,
    frequence: Option<i32>,
) -> StationAnnuaire {
    let url = format!("https://exemple.invalid/{nom}");
    StationAnnuaire {
        name: nom.into(),
        stream_url: url.clone(),
        codec: Some(codec.into()),
        bitrate: debit,
        flux: vec![FluxAnnuaire {
            url,
            codec: Some(codec.into()),
            bitrate: debit,
            sample_rate: frequence,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// (sans perte, haute qualité, Hi-Res) d'une fiche, chacun par son bouton.
fn trois_boutons(f: &StationAnnuaire) -> (bool, bool, bool) {
    (
        criteres("sans_perte=true").retient_la_fiche(f, None),
        criteres("haute_qualite=true").retient_la_fiche(f, None),
        criteres("hires=true").retient_la_fiche(f, None),
    )
}

#[test]
fn boutons_rc12_sans_perte_haute_qualite_hires_flux_par_flux() {
    for (f, attendu) in [
        (
            fiche_flux("mp3-320", "MP3", Some(320), Some(44_100)),
            (false, true, false),
        ),
        (
            fiche_flux("aac-256", "AAC", Some(256), Some(48_000)),
            (false, true, false),
        ),
        (
            fiche_flux("mp3-192", "MP3", Some(192), Some(44_100)),
            (false, false, false),
        ),
        (
            fiche_flux("flac-sans-debit", "FLAC", None, None),
            (true, true, false),
        ),
        (
            fiche_flux("flac-96k", "FLAC", Some(4608), Some(96_000)),
            (true, true, true),
        ),
        // Contre-épreuves : juste sous le seuil, débit inconnu d'un flux avec
        // perte, MP3 que l'annuaire dit à 192 kHz.
        (
            fiche_flux("mp3-255", "MP3", Some(255), Some(44_100)),
            (false, false, false),
        ),
        (
            fiche_flux("aac-sans-debit", "AAC+", None, Some(96_000)),
            (false, false, false),
        ),
        (
            fiche_flux("mp3-320-192k", "MP3", Some(320), Some(192_000)),
            (false, true, false),
        ),
        (
            fiche_flux("alac-cd", "ALAC", Some(1411), Some(44_100)),
            (true, true, false),
        ),
        (
            fiche_flux("ogg-flac", "Ogg FLAC", None, Some(48_000)),
            (true, true, false),
        ),
    ] {
        assert_eq!(trois_boutons(&f), attendu, "{}", f.name);
    }
}

#[test]
fn boutons_rc12_sans_perte_est_l_alias_de_lossless() {
    let flac = fiche_flux("flac", "FLAC", None, None);
    let mp3 = fiche_flux("mp3", "MP3", Some(320), None);
    for requete in [
        "sans_perte=true",
        "lossless=true",
        "lossless=true&sans_perte=true",
    ] {
        assert!(criteres(requete).retient_la_fiche(&flac, None), "{requete}");
        assert!(!criteres(requete).retient_la_fiche(&mp3, None), "{requete}");
    }
    for requete in ["sans_perte=false", "haute_qualite=false", ""] {
        assert!(criteres(requete).est_vide(), "{requete}");
    }
    assert!(!criteres("haute_qualite=true").est_vide());
}

#[test]
fn boutons_rc12_une_station_passe_si_l_un_de_ses_flux_passe() {
    // Un MP3 128 principal et un FLAC de secours : sans perte et haute qualité.
    let mut f = fiche_flux("secours", "MP3", Some(128), Some(44_100));
    f.flux.push(flux(
        "https://exemple.invalid/secours.flac",
        "FLAC",
        1411,
        44_100,
    ));
    assert_eq!(trois_boutons(&f), (true, true, false));
    // Deux MP3, 128 et 320 : haute qualité seulement.
    let mut g = fiche_flux("deux-mp3", "MP3", Some(128), Some(44_100));
    g.flux.push(flux(
        "https://exemple.invalid/deux-mp3-hq",
        "MP3",
        320,
        44_100,
    ));
    assert_eq!(trois_boutons(&g), (false, true, false));
}

#[test]
fn boutons_rc12_mes_radios_juge_le_flux_ecoute() {
    // La fiche a un MP3 128 et un FLAC ; « Mes radios » écoute l'un ou l'autre.
    let mut fiche = fiche_flux("double", "MP3", Some(128), Some(44_100));
    fiche.flux.push(flux(
        "https://exemple.invalid/double.flac",
        "FLAC",
        1411,
        96_000,
    ));
    let ecoute = |url: &str, requete: &str| {
        let st = station("Double", url, "France", "Pop", None, None, false);
        !criteres(requete)
            .filtrer_les_stations(vec![st], &[fiche.clone()], &HashMap::new())
            .is_empty()
    };
    let mp3 = "https://exemple.invalid/double";
    let flac = "https://exemple.invalid/double.flac";
    for requete in ["sans_perte=true", "haute_qualite=true", "hires=true"] {
        assert!(
            !ecoute(mp3, requete),
            "le MP3 128 écouté ne passe pas {requete}"
        );
        assert!(
            ecoute(flac, requete),
            "le FLAC 96 kHz écouté passe {requete}"
        );
    }
    // Une station sans fiche : son propre format et son propre débit.
    let seule = |codec: &str, debit: i32, requete: &str| {
        let st = station(
            "Seule",
            "https://exemple.invalid/seule",
            "France",
            "Pop",
            Some(codec),
            Some(debit),
            false,
        );
        !criteres(requete)
            .filtrer_les_stations(vec![st], &[], &HashMap::new())
            .is_empty()
    };
    assert!(seule("MP3", 320, "haute_qualite=true"));
    assert!(!seule("MP3", 320, "sans_perte=true"));
    assert!(!seule("MP3", 192, "haute_qualite=true"));
    assert!(seule("FLAC", 0, "sans_perte=true"));
    assert!(seule("FLAC", 0, "haute_qualite=true"));
}

#[test]
fn boutons_rc12_l_annuaire_filtre_et_compte() {
    let fiches = [
        fiche_flux("mp3-320", "MP3", Some(320), Some(44_100)),
        fiche_flux("mp3-192", "MP3", Some(192), Some(44_100)),
        fiche_flux("flac-96k", "FLAC", None, Some(96_000)),
    ];
    let compte = |requete: &str| {
        let corps = corps_annuaire(&fiches, &[], &filtres(requete), &EtatAnnuaire::default());
        (
            noms(&corps)
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            corps["total"].clone(),
        )
    };
    assert_eq!(
        compte("haute_qualite=true"),
        (vec!["mp3-320".into(), "flac-96k".into()], json!(2))
    );
    assert_eq!(
        compte("sans_perte=true"),
        (vec!["flac-96k".into()], json!(1))
    );
    assert_eq!(
        compte("haute_qualite=true&hires=true"),
        (vec!["flac-96k".into()], json!(1))
    );
    assert_eq!(
        compte(""),
        (
            vec!["mp3-320".into(), "mp3-192".into(), "flac-96k".into()],
            json!(3)
        )
    );
}
