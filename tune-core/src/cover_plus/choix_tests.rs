//! #8452 — la règle franche et le classement des pochettes, sur les exemples
//! de la spécification de radio-metadata.fr (§ 3.3, § 3.4). Aucun réseau.

use serde_json::{Value, json};

use super::choix::*;

fn morceau(
    artiste: &str,
    titre: &str,
    album: &str,
    image: &str,
    date: &str,
    pistes: i64,
    collection_id: i64,
) -> Value {
    json!({
        "artistName": artiste,
        "trackName": titre,
        "collectionName": album,
        "artworkUrl100": format!("https://is1.mzstatic.example/{image}/100x100bb.jpg"),
        "releaseDate": format!("{date}T08:00:00Z"),
        "trackCount": pistes,
        "collectionId": collection_id,
        "artistId": 1,
    })
}

#[test]
fn la_regle_franche() {
    assert!(concorde(
        "Daft Punk",
        "Daft Punk, Pharrell Williams & Nile Rodgers"
    ));
    assert!(!concorde("Orax", "Oraxis"));
    assert!(concorde("The Pretenders", "Pretenders"));
    assert!(concorde("Mink De Ville", "Mink DeVille"));
    assert!(concorde("Adams Apple", "Adam's Apple"));
    assert!(concorde("1000mg", "1000 MG"));
    assert!(concorde("Beyoncé", "BEYONCE"));
    assert!(concorde("Simon & Garfunkel", "Simon and Garfunkel"));
    assert!(!concorde("", "x"));
}

#[test]
fn le_nom_d_artiste_est_strict_sans_prefixe() {
    assert!(!meme_nom("Tank", "Tank And The Bangas"));
    assert!(meme_nom("The Pretenders", "Pretenders"));
    assert!(concorde_artiste(
        "Lady Gaga, Bruno Mars",
        "Lady Gaga & Bruno Mars"
    ));
}

#[test]
fn la_pochette_est_agrandie_a_600() {
    assert_eq!(
        grande("https://x.example/a/100x100bb.jpg", "600x600bb").as_deref(),
        Some("https://x.example/a/600x600bb.jpg")
    );
    assert_eq!(grande("", "600x600bb"), None);
}

#[test]
fn version_et_remixeur_du_titre_diffuse() {
    let v = version_diffusee("Pourquoi faire aujourd'hui (Poirier remix)");
    assert_eq!(v, vec![("remix", "poirier remix".to_string())]);
    assert_eq!(remixeurs(&v), vec!["poirier".to_string()]);
    assert!(version_diffusee("Beginnings").is_empty());
}

/// Chicago - Beginnings : « The Chicago Transit Authority » (1969), pas
/// « Love Songs » (2005), même remasterisé.
#[test]
fn la_date_du_disque_passe_avant_la_reedition() {
    let r = vec![
        morceau(
            "Chicago",
            "Beginnings",
            "Love Songs",
            "love",
            "2005-01-01",
            14,
            10,
        ),
        morceau(
            "Chicago",
            "Beginnings",
            "The Chicago Transit Authority (Remastered)",
            "cta",
            "1969-04-28",
            12,
            11,
        ),
    ];
    let v = classer(&r, "Chicago", "Beginnings", None);
    assert_eq!(v.len(), 2);
    assert!(v[0].url.contains("/cta/600x600bb.jpg"), "{v:?}");
}

/// Stromae - Alors on danse : l'album « Cheese », pas le remix avec Kanye West.
#[test]
fn sans_indication_le_remix_est_ecarte() {
    let r = vec![
        morceau(
            "Stromae",
            "Alors on danse (feat. Kanye West) [Remix]",
            "Alors on danse (Remixes)",
            "remix",
            "2010-01-01",
            3,
            20,
        ),
        morceau(
            "Stromae",
            "Alors on danse",
            "Cheese",
            "cheese",
            "2010-06-01",
            12,
            21,
        ),
    ];
    let v = classer(&r, "Stromae", "Alors on danse", None);
    assert!(v[0].url.contains("/cheese/"), "{v:?}");
}

/// Queen - I Want To Break Free (live) : un vrai album live.
#[test]
fn un_titre_live_garde_le_live() {
    let r = vec![
        morceau(
            "Queen",
            "I Want to Break Free",
            "The Works",
            "works",
            "1984-02-27",
            9,
            30,
        ),
        morceau(
            "Queen",
            "I Want to Break Free (Live at Wembley Stadium)",
            "Live at Wembley Stadium",
            "wembley",
            "1992-05-26",
            27,
            31,
        ),
    ];
    let v = classer(&r, "Queen", "I Want To Break Free (live)", None);
    assert!(v[0].url.contains("/wembley/"), "{v:?}");
    let v = classer(&r, "Queen", "I Want To Break Free", None);
    assert!(v[0].url.contains("/works/"), "{v:?}");
}

/// Un remix crédité au remixeur : Poirier pour Lisa Leblanc.
#[test]
fn un_remix_credite_au_remixeur_est_accepte() {
    let r = vec![morceau(
        "Poirier",
        "Pourquoi faire aujourd'hui (feat. Lisa LeBlanc) [Remix]",
        "Pourquoi faire aujourd'hui (Remix) - Single",
        "poirier",
        "2019-01-01",
        1,
        40,
    )];
    let v = classer(
        &r,
        "Lisa Leblanc",
        "Pourquoi faire aujourd'hui (Poirier remix)",
        None,
    );
    assert_eq!(v.len(), 1, "{v:?}");
}

/// Une mauvaise pochette est pire que pas de pochette.
#[test]
fn ni_artiste_ni_titre_discordants() {
    let r = vec![
        morceau("Oraxis", "Quick", "Album", "a", "2019-01-01", 12, 50),
        morceau("Orax", "Other Song", "Album", "b", "2019-01-01", 12, 51),
    ];
    assert!(classer(&r, "Orax", "Quick", None).is_empty());
}

#[test]
fn une_meme_image_n_apparait_qu_une_fois() {
    let r = vec![
        morceau(
            "Air",
            "La femme d'argent",
            "Moon Safari",
            "moon",
            "1998-01-16",
            10,
            60,
        ),
        morceau(
            "Air",
            "La femme d'argent",
            "Moon Safari",
            "moon",
            "1998-01-16",
            10,
            60,
        ),
    ];
    assert_eq!(classer(&r, "Air", "La femme d'argent", None).len(), 1);
}

/// La date du DISQUE vient de la discographie ; un disque absent d'une
/// discographie complète est classé comme une compilation.
#[test]
fn la_discographie_date_les_disques_et_ecarte_les_tiers() {
    let r = vec![
        morceau(
            "Weather Report",
            "Black Market",
            "This Is Jazz, Vol. 10",
            "jazz",
            "1973-01-01",
            10,
            70,
        ),
        morceau(
            "Weather Report",
            "Black Market",
            "Black Market",
            "bm",
            "1976-03-01",
            7,
            71,
        ),
    ];
    let sans = classer(&r, "Weather Report", "Black Market", None);
    assert!(
        sans[0].url.contains("/jazz/"),
        "sans discographie : {sans:?}"
    );
    let disques = discographie(&[
        json!({"wrapperType": "artist", "artistId": 1}),
        json!({"wrapperType": "collection", "collectionId": 71, "releaseDate": "1976-03-01T08:00:00Z"}),
    ]);
    assert_eq!(disques.get(&71).map(String::as_str), Some("1976-03-01"));
    let avec = classer(&r, "Weather Report", "Black Market", Some(&disques));
    assert!(avec[0].url.contains("/bm/"), "avec discographie : {avec:?}");
}

/// § 3.4, exemple « Lisa Leblanc - Pourquoi faire aujourd'hui (Poirier
/// remix) » : le remix du MÊME remixeur passe devant l'original et devant un
/// autre remix.
#[test]
fn le_remix_du_meme_remixeur_passe_devant() {
    let r = vec![
        morceau(
            "Lisa LeBlanc",
            "Pourquoi faire aujourd'hui",
            "Highways, Heartaches and Time Well Wasted",
            "original",
            "2016-01-01",
            11,
            80,
        ),
        morceau(
            "Lisa LeBlanc",
            "Pourquoi faire aujourd'hui (Autre remix)",
            "Pourquoi faire aujourd'hui (Autre remix) - Single",
            "autre",
            "2018-01-01",
            1,
            81,
        ),
        morceau(
            "Poirier",
            "Pourquoi faire aujourd'hui (feat. Lisa LeBlanc) [Remix]",
            "Pourquoi faire aujourd'hui (Poirier Remix) - Single",
            "poirier",
            "2019-01-01",
            1,
            82,
        ),
    ];
    let v = classer(
        &r,
        "Lisa Leblanc",
        "Pourquoi faire aujourd'hui (Poirier remix)",
        None,
    );
    assert!(v[0].url.contains("/poirier/"), "{v:?}");
    // Sans indication de version : l'original.
    let v = classer(&r, "Lisa Leblanc", "Pourquoi faire aujourd'hui", None);
    assert!(v[0].url.contains("/original/"), "{v:?}");
}

/// § 3.4, critère 0 : à version égale, celle que le DISQUE porte — un vrai
/// album live plutôt qu'un live en bonus d'une réédition studio.
#[test]
fn un_vrai_album_live_plutot_qu_un_bonus_live() {
    let r = vec![
        morceau(
            "Queen",
            "I Want to Break Free (Live)",
            "The Works (Deluxe Edition)",
            "works-deluxe",
            "1984-02-27",
            20,
            90,
        ),
        morceau(
            "Queen",
            "I Want to Break Free (Live at Wembley Stadium)",
            "Live at Wembley Stadium",
            "wembley",
            "1992-05-26",
            27,
            91,
        ),
    ];
    let v = classer(&r, "Queen", "I Want To Break Free (live)", None);
    assert!(v[0].url.contains("/wembley/"), "{v:?}");
}

/// § 3.4, critère 2 : « Various Artists » et best-of passent derrière.
#[test]
fn compilation_et_best_of_passent_derriere() {
    let mut compil = morceau("Chicago", "Beginnings", "Soft Rock 70s", "compil", "1960-01-01", 40, 100);
    compil["collectionArtistName"] = json!("Various Artists");
    let r = vec![
        compil,
        morceau("Chicago", "Beginnings", "Greatest Hits 1969-1974", "best", "1965-01-01", 12, 101),
        morceau("Chicago", "Beginnings", "The Chicago Transit Authority", "cta", "1969-04-28", 12, 102),
    ];
    let v = classer(&r, "Chicago", "Beginnings", None);
    assert!(v[0].url.contains("/cta/"), "{v:?}");
}

/// § 3.3 : collaboration écrite autrement, accent, article, mais jamais un
/// autre artiste au même titre.
#[test]
fn concordance_d_artiste_cas_de_la_specification() {
    let r = vec![morceau(
        "Lady Gaga & Bruno Mars",
        "Die With A Smile",
        "Die With A Smile - Single",
        "dwas",
        "2024-08-16",
        1,
        110,
    )];
    assert_eq!(
        classer(&r, "Lady Gaga, Bruno Mars", "Die With A Smile", None).len(),
        1
    );
    assert!(classer(&r, "Bruno Mars Tribute Band", "Die With A Smile", None).is_empty());
}
