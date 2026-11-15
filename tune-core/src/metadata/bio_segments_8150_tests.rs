//! Témoins de #8150 : les exemplaires d'un même album, les titres sans
//! mentions d'édition et les labels dans les biographies.

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

/// Un album local en qualité CD, de l'artiste donné.
fn exemplaire(id: i64, titre: &str, artist_id: i64) -> AlbumDeLIndex {
    AlbumDeLIndex {
        id,
        titre: titre.to_string(),
        artist_id: Some(artist_id),
        local: true,
        sample_rate: Some(44_100),
        bit_depth: Some(16),
    }
}

fn artistes() -> Vec<(i64, String)> {
    vec![
        (2739, "Pink Floyd".to_string()),
        (50, "Roger Waters".to_string()),
        (60, "Neil Young".to_string()),
    ]
}

fn index(albums: Vec<AlbumDeLIndex>, labels: &[&str]) -> IndexDesNoms {
    IndexDesNoms::construire_avec(artistes(), albums, labels.iter().map(|l| l.to_string()))
}

/// (texte, lien interne, label) de chaque segment lié.
fn liens(segments: &[BioSegment]) -> Vec<(String, Option<LienInterne>, Option<String>)> {
    segments
        .iter()
        .filter(|s| s.lien.is_some() || s.label.is_some())
        .map(|s| (s.texte.clone(), s.lien, s.label.clone()))
        .collect()
}

fn lie(texte: &str, lien: LienInterne) -> (String, Option<LienInterne>, Option<String>) {
    (texte.to_string(), Some(lien), None)
}

const BIO_FLOYD: &str =
    "Pink Floyd publie The Dark Side of the Moon en 1973, puis The Wall en 1979.";

#[test]
fn les_exemplaires_d_un_meme_album_du_meme_artiste_sont_lies_8150() {
    // Le relevé de l'issue : deux « The Wall » (10328, 11288) et deux « The
    // Dark Side of the Moon » (10332, 19922), tous de Pink Floyd.
    let index = index(
        vec![
            exemplaire(10328, "The Wall", 2739),
            exemplaire(11288, "The Wall", 2739),
            exemplaire(10332, "The Dark Side of the Moon", 2739),
            exemplaire(19922, "The Dark Side of the Moon", 2739),
        ],
        &[],
    );
    let s = segments_de_la_bio(BIO_FLOYD, &index, &[artiste(2739)], &[]);
    assert_eq!(
        liens(&s),
        vec![
            lie("The Dark Side of the Moon", album(10332)),
            lie("The Wall", album(10328)),
        ],
        "un titre porté par plusieurs exemplaires du même artiste doit être lié \
         (à qualité égale, le plus ancien exemplaire)"
    );
}

#[test]
fn le_lien_mene_au_meilleur_exemplaire_8150() {
    let mut hi_res = exemplaire(11288, "The Wall", 2739);
    hi_res.sample_rate = Some(96_000);
    hi_res.bit_depth = Some(24);
    let mut service = exemplaire(9000, "The Wall", 2739);
    service.local = false;
    service.sample_rate = Some(192_000);
    service.bit_depth = Some(24);
    let index = index(
        vec![service, exemplaire(10328, "The Wall", 2739), hi_res],
        &[],
    );
    assert_eq!(
        index.lien_du_nom("The Wall"),
        Some(album(11288)),
        "local avant service, puis la meilleure résolution"
    );
}

#[test]
fn un_meme_titre_de_deux_artistes_differents_n_est_pas_lie_8150() {
    let index = index(
        vec![
            exemplaire(10328, "The Wall", 2739),
            exemplaire(500, "The Wall", 50),
        ],
        &[],
    );
    let s = segments_de_la_bio(BIO_FLOYD, &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![lie("Pink Floyd", artiste(2739))],
        "« The Wall » de deux artistes reste ambigu"
    );
}

#[test]
fn deux_albums_sans_artiste_connu_restent_ambigus_8150() {
    let mut a = exemplaire(1, "The Wall", 2739);
    a.artist_id = None;
    let mut b = exemplaire(2, "The Wall", 2739);
    b.artist_id = None;
    assert_eq!(index(vec![a, b], &[]).lien_du_nom("The Wall"), None);
}

#[test]
fn le_titre_sans_mention_d_edition_rejoint_l_album_8150() {
    let index = index(
        vec![exemplaire(
            11288,
            "The Wall (Remastered 2011 Version)",
            2739,
        )],
        &[],
    );
    let s = segments_de_la_bio(BIO_FLOYD, &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![
            lie("Pink Floyd", artiste(2739)),
            lie("The Wall", album(11288)),
        ],
        "« The Wall » doit mener à « The Wall (Remastered 2011 Version) »"
    );
    assert_eq!(
        index.lien_du_nom("The Wall (Remastered 2011 Version)"),
        Some(album(11288)),
        "le titre complet reste lié"
    );
}

#[test]
fn le_titre_sans_mention_de_disque_rejoint_l_album_8150() {
    let mut cd2 = exemplaire(12, "The Wall [CD 2]", 2739);
    cd2.sample_rate = Some(48_000);
    let index = index(
        vec![
            exemplaire(11, "The Wall, Disc 1", 2739),
            cd2,
            exemplaire(13, "Animals (Disc 1)", 2739),
        ],
        &[],
    );
    assert_eq!(index.lien_du_nom("The Wall"), Some(album(12)));
    assert_eq!(index.lien_du_nom("Animals"), Some(album(13)));
}

#[test]
fn un_titre_exact_et_ses_editions_du_meme_artiste_forment_un_album_8150() {
    let mut remaster = exemplaire(11288, "The Wall (Remastered)", 2739);
    remaster.sample_rate = Some(96_000);
    let index = index(vec![exemplaire(10328, "The Wall", 2739), remaster], &[]);
    assert_eq!(
        index.lien_du_nom("The Wall"),
        Some(album(11288)),
        "l'édition remasterisée du même artiste compte parmi les exemplaires"
    );
}

#[test]
fn un_titre_exact_l_emporte_sur_l_edition_d_un_autre_artiste_8150() {
    let index = index(
        vec![
            exemplaire(500, "The Wall", 50),
            exemplaire(11288, "The Wall (Remastered 2011 Version)", 2739),
        ],
        &[],
    );
    assert_eq!(index.lien_du_nom("The Wall"), Some(album(500)));
}

#[test]
fn les_titres_de_base_de_deux_artistes_restent_ambigus_8150() {
    let index = index(
        vec![
            exemplaire(500, "The Wall (Deluxe Edition)", 50),
            exemplaire(11288, "The Wall (Remastered 2011 Version)", 2739),
        ],
        &[],
    );
    assert_eq!(index.lien_du_nom("The Wall"), None);
}

#[test]
fn une_parenthese_qui_n_est_pas_une_edition_reste_dans_le_titre_8150() {
    assert_eq!(titre_de_base("Wish You Were Here (Live)"), None);
    assert_eq!(titre_de_base("Cross-Eyed Mary"), None);
    assert_eq!(titre_de_base("Rock And Roll (Part 2)"), None);
    assert_eq!(
        titre_de_base("Abbey Road (2019 Remaster) [Disc 2]").as_deref(),
        Some("Abbey Road")
    );
    assert_eq!(
        titre_de_base("Wish You Were Here - 2011").as_deref(),
        Some("Wish You Were Here")
    );
}

#[test]
fn la_page_d_un_exemplaire_ne_lie_pas_un_autre_exemplaire_8150() {
    let mut hi_res = exemplaire(11288, "The Wall", 2739);
    hi_res.sample_rate = Some(96_000);
    let index = index(vec![exemplaire(10328, "The Wall", 2739), hi_res], &[]);
    let s = segments_de_la_bio(
        "The Wall est un opéra rock.",
        &index,
        &[album(10328), artiste(2739)],
        &[],
    );
    assert!(
        liens(&s).is_empty(),
        "la bio de l'exemplaire 10328 ne doit pas renvoyer vers 11288 : {:?}",
        liens(&s)
    );
}

#[test]
fn un_label_de_la_bibliotheque_cite_devient_un_segment_label_8150() {
    let index = index(
        vec![exemplaire(10328, "The Wall", 2739)],
        &["Harvest", "EMI"],
    );
    let bio = "Pink Floyd signe chez Harvest, filiale d'EMI.";
    let s = segments_de_la_bio(bio, &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![
            lie("Pink Floyd", artiste(2739)),
            ("Harvest".to_string(), None, Some("Harvest".to_string())),
        ],
        "« Harvest » doit ouvrir la page du label ; « EMI » est trop court"
    );
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(
        json[2],
        serde_json::json!({"texte": "Harvest", "label": "Harvest"}),
        "un segment label porte le nom du label, sans lien interne"
    );
    let pas_en_minuscule = segments_de_la_bio("une récolte (harvest) précoce", &index, &[], &[]);
    assert!(liens(&pas_en_minuscule).is_empty());
}

#[test]
fn un_artiste_ou_un_album_l_emporte_sur_un_label_homonyme_8150() {
    let index = index(
        vec![exemplaire(700, "Harvest", 60)],
        &["Harvest", "Pink Floyd", "Harvest Records"],
    );
    assert_eq!(index.lien_du_nom("Harvest"), Some(album(700)));
    assert_eq!(index.lien_du_nom("Pink Floyd"), Some(artiste(2739)));
    let s = segments_de_la_bio("Il signe chez Harvest Records.", &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![(
            "Harvest Records".to_string(),
            None,
            Some("Harvest Records".to_string())
        )],
        "le plus long nom l'emporte"
    );
}

#[test]
fn les_liens_d_artistes_ne_changent_pas_8150() {
    let index = IndexDesNoms::construire_avec(
        vec![
            (1, "Genesis".to_string()),
            (2, "Genesis".to_string()),
            (3, "Daft Punk".to_string()),
            (2739, "Pink Floyd".to_string()),
        ],
        vec![
            exemplaire(104, "Daft Punk", 3),
            exemplaire(105, "Genesis", 1),
            exemplaire(106, "Pink Floyd", 2739),
            exemplaire(107, "Pink Floyd", 2739),
        ],
        ["Genesis".to_string()],
    );
    assert_eq!(index.lien_du_nom("Daft Punk"), Some(artiste(3)));
    assert_eq!(
        index.lien_du_nom("Pink Floyd"),
        Some(artiste(2739)),
        "un artiste l'emporte toujours sur ses albums éponymes"
    );
    assert_eq!(
        index.lien_du_nom("Genesis"),
        None,
        "deux artistes homonymes restent ambigus, même face à un album ou un label"
    );
}

#[test]
fn l_index_lit_artistes_albums_et_labels_de_la_base_8150() {
    let base = crate::db::sqlite::SqliteDb::open_in_memory().expect("base en mémoire");
    base.init_schema().expect("schéma");
    crate::db::migrations::run_migrations(&base).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(base);
    for sql in [
        "INSERT INTO artists (id, name) VALUES (2739, 'Pink Floyd')",
        "INSERT INTO artists (id, name) VALUES (50, 'Roger Waters')",
        "INSERT INTO albums (id, title, artist_id, source, sample_rate, bit_depth, label) \
         VALUES (10328, 'The Wall', 2739, 'local', 44100, 16, 'Harvest')",
        "INSERT INTO albums (id, title, artist_id, source, sample_rate, bit_depth, label) \
         VALUES (11288, 'The Wall', 2739, 'local', 96000, 24, 'Harvest')",
        "INSERT INTO albums (id, title, artist_id, source) \
         VALUES (10332, 'The Dark Side of the Moon (Remastered)', 2739, 'local')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (600, 'Amused to Death', 2739, 'local')",
        "INSERT INTO albums (id, title, artist_id, source) VALUES (601, 'Amused to Death', 50, 'local')",
    ] {
        db.execute(sql, &[]).expect(sql);
    }
    let index = charger_index(&db).expect("index");
    assert_eq!(index.lien_du_nom("The Wall"), Some(album(11288)));
    assert_eq!(
        index.lien_du_nom("The Dark Side of the Moon"),
        Some(album(10332))
    );
    assert_eq!(index.lien_du_nom("Amused to Death"), None);
    let s = segments_de_la_bio("Publié par Harvest.", &index, &[], &[]);
    assert_eq!(
        liens(&s),
        vec![("Harvest".to_string(), None, Some("Harvest".to_string()))]
    );
}
