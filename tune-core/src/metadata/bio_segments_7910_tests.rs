//! Témoins de #7910 : liens internes et externes des biographies.

use super::*;

fn artiste(id: i64) -> LienInterne {
    LienInterne {
        genre: GenreDeLien::Artiste,
        id,
    }
}

fn album(id: i64) -> LienInterne {
    LienInterne {
        genre: GenreDeLien::Album,
        id,
    }
}

fn index() -> IndexDesNoms {
    IndexDesNoms::construire(
        vec![
            (1, "Radiohead".to_string()),
            (2, "Björk".to_string()),
            (3, "Pink Floyd".to_string()),
            (4, "U2".to_string()),
            (5, "Live".to_string()),
            (6, "Genesis".to_string()),
            (7, "Genesis".to_string()),
            (8, "Various Artists".to_string()),
            (9, "Thom Yorke".to_string()),
            (10, "Daft Punk".to_string()),
        ],
        vec![
            (100, "OK Computer".to_string()),
            (101, "Homogenic".to_string()),
            (102, "Greatest Hits".to_string()),
            (103, "1999".to_string()),
            (104, "Daft Punk".to_string()),
            (105, "Discovery".to_string()),
            (106, "Discovery".to_string()),
        ],
    )
}

fn liens(segments: &[BioSegment]) -> Vec<(String, Option<LienInterne>, Option<String>)> {
    segments
        .iter()
        .filter(|s| s.lien.is_some() || s.url.is_some())
        .map(|s| (s.texte.clone(), s.lien, s.url.clone()))
        .collect()
}

fn texte_recompose(segments: &[BioSegment]) -> String {
    segments.iter().map(|s| s.texte.as_str()).collect()
}

#[test]
fn un_artiste_et_un_album_cites_ouvrent_leur_page_7910() {
    let bio = "Thom Yorke a fondé Radiohead, qui publie OK Computer en 1997.";
    let s = segments_de_la_bio(bio, &index(), &[], &[]);
    assert_eq!(
        liens(&s),
        vec![
            ("Thom Yorke".to_string(), Some(artiste(9)), None),
            ("Radiohead".to_string(), Some(artiste(1)), None),
            ("OK Computer".to_string(), Some(album(100)), None),
        ],
        "les entités citées et connues doivent devenir des liens internes"
    );
    assert_eq!(
        texte_recompose(&s),
        bio,
        "les segments recomposent la bio exacte"
    );
}

#[test]
fn la_casse_et_les_accents_ne_comptent_pas_7910() {
    let bio = "Elle a chanté avec BJORK, puis avec Bjork et Pink floyd.";
    let s = segments_de_la_bio(bio, &index(), &[], &[]);
    assert_eq!(
        liens(&s),
        vec![
            ("BJORK".to_string(), Some(artiste(2)), None),
            ("Pink floyd".to_string(), Some(artiste(3)), None),
        ],
        "« BJORK » doit rejoindre « Björk » (accents et casse ignorés), une seule fois"
    );
}

#[test]
fn le_texte_accentue_rejoint_un_nom_sans_accent_7910() {
    let index = IndexDesNoms::construire(vec![(1, "Beyonce Knowles".to_string())], vec![]);
    let s = segments_de_la_bio("Avec Beyoncé Knowles.", &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![("Beyoncé Knowles".to_string(), Some(artiste(1)), None)]
    );
}

#[test]
fn les_noms_courts_courants_ambigus_ou_generiques_sont_ecartes_7910() {
    let bio = "U2 joue Live à Wembley ; Genesis et Various Artists ; Greatest Hits ; \
               en 1999 ; Discovery.";
    let s = segments_de_la_bio(bio, &index(), &[], &[]);
    assert!(
        liens(&s).is_empty(),
        "aucun lien attendu (nom court, mot courant, homonymes, générique, nombre) : {:?}",
        liens(&s)
    );
}

#[test]
fn un_mot_seul_doit_etre_ecrit_avec_une_majuscule_7910() {
    let s = segments_de_la_bio("un son homogenic et radiohead", &index(), &[], &[]);
    assert!(liens(&s).is_empty(), "{:?}", liens(&s));
}

#[test]
fn un_nom_ne_traverse_pas_une_ponctuation_ni_un_bout_de_mot_7910() {
    let s = segments_de_la_bio("Pink, Floyd. Radioheads", &index(), &[], &[]);
    assert!(liens(&s).is_empty(), "{:?}", liens(&s));
}

#[test]
fn l_artiste_de_la_page_est_exclu_et_un_lien_par_entite_7910() {
    let bio = "Radiohead enregistre avec Thom Yorke. Radiohead revient ; Thom Yorke aussi.";
    let s = segments_de_la_bio(bio, &index(), &[artiste(1)], &[]);
    assert_eq!(
        liens(&s),
        vec![("Thom Yorke".to_string(), Some(artiste(9)), None)],
        "l'artiste de la page ne se lie pas, et Thom Yorke n'est lié qu'une fois"
    );
}

#[test]
fn un_artiste_l_emporte_sur_son_album_eponyme_7910() {
    let s = segments_de_la_bio("Le duo Daft Punk.", &index(), &[], &[]);
    assert_eq!(
        liens(&s),
        vec![("Daft Punk".to_string(), Some(artiste(10)), None)]
    );
}

#[test]
fn les_liens_externes_de_la_source_sont_gardes_7910() {
    let html = r#"Radiohead are an English band. <a href="https://www.last.fm/music/Radiohead">Read more on Last.fm</a>. Voir <a href='https://fr.wikipedia.org/wiki/Radiohead'>Wikipédia</a>."#;
    let externes = liens_depuis_html(html);
    assert_eq!(
        externes,
        vec![
            LienExterne {
                texte: "Read more on Last.fm".into(),
                url: "https://www.last.fm/music/Radiohead".into()
            },
            LienExterne {
                texte: "Wikipédia".into(),
                url: "https://fr.wikipedia.org/wiki/Radiohead".into()
            },
        ]
    );
    let texte = "Radiohead are an English band. Read more on Last.fm. Voir Wikipédia.";
    let s = segments_de_la_bio(texte, &index(), &[artiste(1)], &externes);
    assert_eq!(
        liens(&s),
        vec![
            (
                "Read more on Last.fm".to_string(),
                None,
                Some("https://www.last.fm/music/Radiohead".to_string())
            ),
            (
                "Wikipédia".to_string(),
                None,
                Some("https://fr.wikipedia.org/wiki/Radiohead".to_string())
            ),
        ]
    );
    assert_eq!(texte_recompose(&s), texte);
}

#[test]
fn les_url_dangereuses_ou_etrangeres_sont_refusees_7910() {
    for refusee in [
        "javascript:alert(1)",
        "JavaScript:alert(1)",
        "http://www.last.fm/music/Radiohead",
        "https://evil.example.com/last.fm",
        "https://last.fm.evil.com/music",
        "https://evil-last.fm/music",
        "https://user@www.last.fm/music",
        "https://www.last.fm:8443/music",
        "data:text/html,<script>alert(1)</script>",
        "//www.last.fm/music",
        "https://www.last.fm/mu sic",
    ] {
        assert_eq!(url_autorisee(refusee), None, "{refusee} doit être refusée");
    }
    for permise in [
        "https://www.last.fm/music/Radiohead",
        "https://last.fm/music/Radiohead",
        "https://en.wikipedia.org/wiki/Radiohead",
        "https://www.qobuz.com/fr-fr/interpreter/radiohead/1",
    ] {
        assert!(
            url_autorisee(permise).is_some(),
            "{permise} doit être permise"
        );
    }
}

#[test]
fn un_lien_refuse_redevient_du_texte_7910() {
    let html = r#"<a href="javascript:alert(1)">clic</a> <a href="http://www.last.fm/x">Last.fm</a> <a href="https://evil.example.com/">ailleurs</a>"#;
    assert!(liens_depuis_html(html).is_empty());
    let externes = vec![LienExterne {
        texte: "clic".into(),
        url: "javascript:alert(1)".into(),
    }];
    let s = segments_de_la_bio("clic ici", &index(), &[], &externes);
    assert!(
        liens(&s).is_empty(),
        "un lien rangé mais dangereux est refiltré à la lecture"
    );
}

#[test]
fn les_segments_se_serialisent_comme_l_issue_les_decrit_7910() {
    let s = segments_de_la_bio("Avec Radiohead.", &index(), &[], &[]);
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(
        json,
        serde_json::json!([
            {"texte": "Avec "},
            {"texte": "Radiohead", "lien": {"type": "artiste", "id": 1}},
            {"texte": "."}
        ])
    );
}

#[test]
fn les_liens_ranges_ne_valent_que_pour_le_meme_texte_7910() {
    let base = crate::db::sqlite::SqliteDb::open_in_memory().expect("base en mémoire");
    base.init_schema().expect("schéma");
    crate::db::migrations::run_migrations(&base).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(base);
    let liens = vec![LienExterne {
        texte: "Read more on Last.fm".into(),
        url: "https://www.last.fm/music/Radiohead".into(),
    }];
    ranger_liens_externes(&db, GenreDeLien::Artiste, 1, "texte A", &liens);
    assert_eq!(
        liens_externes_ranges(&db, GenreDeLien::Artiste, 1, "texte A"),
        liens
    );
    assert!(
        liens_externes_ranges(&db, GenreDeLien::Artiste, 1, "texte réécrit").is_empty(),
        "une bio réécrite ne reprend pas les liens de l'ancienne"
    );
    assert!(liens_externes_ranges(&db, GenreDeLien::Album, 1, "texte A").is_empty());
}
