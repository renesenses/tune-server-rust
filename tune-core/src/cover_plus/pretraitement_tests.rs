//! #8452 — le pré-traitement de l'ICY, sur les exemples de la spécification
//! de radio-metadata.fr (§ 2). Aucun réseau.

use super::pretraitement::*;

fn paire(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

#[test]
fn exemple_de_la_specification_titre_artiste() {
    let p = preparer("GOOD TIMES BAD TIMES - LED ZEPPELIN", "Oüi FM: Alternatif");
    assert_eq!(p.ignorer, None);
    assert_eq!(p.requetes, vec!["GOOD TIMES BAD TIMES LED ZEPPELIN"]);
    assert_eq!(
        p.hypotheses,
        vec![
            paire("GOOD TIMES BAD TIMES", "LED ZEPPELIN"),
            paire("LED ZEPPELIN", "GOOD TIMES BAD TIMES"),
        ]
    );
}

#[test]
fn reparer_le_texte() {
    // UTF-8 lu comme du Latin-1 (deux caractères pour un « ë »).
    let mojibake = format!("Ori{}{}nt", '\u{C3}', '\u{AB}');
    assert_eq!(reparer(&mojibake), "Oriënt");
    assert_eq!(reparer("Simon &amp; Garfunkel"), "Simon & Garfunkel");
    assert_eq!(reparer("Rock &amp;amp; Roll"), "Rock & Roll");
    assert_eq!(reparer("Don&#39;t Stop"), "Don't Stop");
    assert_eq!(reparer("Barry Can`t  Swim"), "Barry Can't Swim");
}

#[test]
fn rien_a_chercher() {
    for (brut, station) in [
        ("", "X"),
        (" - ", "X"),
        ("STOP ADBREAK 1", "X"),
        ("AD 2", "X"),
        ("Spot Block End", "X"),
        ("Now Playing info goes here", "X"),
        ("2GB_PROMO_BREAKFAST_01", "X"),
        ("6PR10011-0660", "X"),
        ("www.maradio.example", "X"),
        ("Radio Globus - Altid godt for dig!", "Radio Globus"),
        ("FluxFM - Livestream", "FluxFM"),
        ("Radio Nova", "Radio Nova"),
    ] {
        assert!(
            preparer(brut, station).ignorer.is_some(),
            "« {brut} » ne doit rien chercher"
        );
    }
}

#[test]
fn la_station_retiree_le_morceau_reste() {
    let p = preparer(
        "Vienna Scientists - Menheads - Get This Text On Television - 674FM",
        "674FM",
    );
    assert_eq!(p.ignorer, None);
    // Plus de deux parties : d'abord les deux premières seules.
    assert_eq!(
        p.requetes,
        vec![
            "Vienna Scientists Menheads",
            "Vienna Scientists Menheads Get This Text On Television",
        ]
    );
}

#[test]
fn titre_by_artiste() {
    let p = preparer("Freddie's Tune by Charlie Byrd on JazzPHX", "KJZZ");
    assert_eq!(p.hypotheses[0], paire("Charlie Byrd", "Freddie's Tune"));
    assert_eq!(p.requetes, vec!["Charlie Byrd Freddie's Tune"]);
    // « Stand By Me - Ben E. King » n'est pas de cette forme.
    let p = preparer("Stand By Me - Ben E. King", "X");
    assert_eq!(p.hypotheses[0], paire("Stand By Me", "Ben E. King"));
}

#[test]
fn habillages_et_numeros_de_piste() {
    let p = preparer("Now playing: Smokie - Living Next Door To Alice", "X");
    assert_eq!(
        p.hypotheses[0],
        paire("Smokie", "Living Next Door To Alice")
    );
    let p = preparer("Nu op Novo3 : Smokie - Living Next Door To Alice", "NOVO3");
    assert_eq!(
        p.hypotheses[0],
        paire("Smokie", "Living Next Door To Alice")
    );
    let p = preparer("Heinrich Bach - 02 - Vox Luminis - Kyrie a 6", "X");
    assert_eq!(p.hypotheses[0], paire("Heinrich Bach", "Vox Luminis"));
    assert!(
        p.requetes
            .contains(&"Heinrich Bach Vox Luminis Kyrie a 6".to_string())
    );
    let p = preparer(
        "Artiste - Titre | Label | REF01 | 1999 | FR",
        "Intergalactic FM",
    );
    assert_eq!(p.hypotheses[0], paire("Artiste", "Titre"));
}

#[test]
fn la_requete_ne_garde_que_ce_qu_itunes_connait() {
    let p = preparer(
        "Daft Punk - Get Lucky (feat. Pharrell Williams) [Radio Edit]",
        "X",
    );
    assert_eq!(p.requetes, vec!["Daft Punk Get Lucky"]);
    // La référence, elle, garde tout.
    assert_eq!(
        p.hypotheses[0],
        paire(
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams) [Radio Edit]"
        )
    );
    let p = preparer("Simon & Garfunkel - The Boxer", "X");
    assert_eq!(p.requetes, vec!["Simon & Garfunkel The Boxer"]);
    let p = preparer("Mogwai feat. Iggy Pop - Punk Rock", "X");
    assert_eq!(p.requetes, vec!["Mogwai Punk Rock"]);
}

#[test]
fn un_remix_cherche_d_abord_avec_le_remixeur() {
    let p = preparer(
        "Lisa Leblanc - Pourquoi faire aujourd'hui (Poirier remix)",
        "X",
    );
    assert_eq!(
        p.requetes,
        vec![
            "Lisa Leblanc Pourquoi faire aujourd'hui poirier",
            "Lisa Leblanc Pourquoi faire aujourd'hui",
        ]
    );
}

#[test]
fn pas_de_decoupe_sur_un_tiret_sans_espaces() {
    let p = preparer("Jay-Z", "X");
    assert!(p.hypotheses.is_empty());
    assert_eq!(p.requetes, vec!["Jay-Z"]);
}

#[test]
fn format_iheart() {
    let p = preparer(
        r#"Lizzo - text="About Damn Time" song_spot="M" amgArtworkURL="https://i.iheart.com/a.jpg""#,
        "KIIS",
    );
    assert_eq!(p.hypotheses[0], paire("Lizzo", "About Damn Time"));
    assert_eq!(
        p.pochette_fournie.as_deref(),
        Some("https://i.iheart.com/a.jpg")
    );
    let p = preparer(r#"KIIS - text="Win tickets" song_spot="T""#, "KIIS");
    assert!(p.ignorer.is_some());
}

/// La pochette d'un flux iHeart n'est gardée que si le relais des pochettes
/// l'accepte : `https://` sur `*.iheart.com`. Sinon elle s'afficherait
/// cassée, avec le label « Cover+ ».
#[test]
fn pochette_iheart_https_et_hote_iheart_seulement() {
    for bonne in [
        "https://i.iheart.com/v3/re/a.jpg",
        "https://iheart.com/a.jpg",
    ] {
        assert!(
            super::pretraitement::pochette_iheart_admise(bonne),
            "{bonne}"
        );
    }
    for mauvaise in [
        "http://i.iheart.com/a.jpg",
        "https://i.iheart.example/a.jpg",
        "https://iheart.com.exemple.net/a.jpg",
        "https://exemple.net/iheart.com/a.jpg",
        "javascript:alert(1)",
        "",
    ] {
        assert!(
            !super::pretraitement::pochette_iheart_admise(mauvaise),
            "{mauvaise}"
        );
        let brut =
            format!(r#"Lizzo - text="About Damn Time" song_spot="M" amgArtworkURL="{mauvaise}""#);
        assert_eq!(preparer(&brut, "KIIS").pochette_fournie, None, "{mauvaise}");
    }
}

#[test]
fn queues_single_ep_et_version_diffusee() {
    let p = preparer("Daft Punk - Get Lucky - Single", "X");
    assert_eq!(p.hypotheses[0], paire("Daft Punk", "Get Lucky"));
    assert_eq!(p.requetes, vec!["Daft Punk Get Lucky"]);
    let p = preparer("Bicep - Glue - EP", "X");
    assert_eq!(p.hypotheses.len(), 2, "{:?}", p.hypotheses);
    // Une queue « … Mix » est une VERSION : rattachée au titre, pour juger.
    let p = preparer("Stromae - Alors on danse - Dubdogz Remix", "X");
    assert_eq!(
        p.hypotheses[0],
        paire("Stromae", "Alors on danse (Dubdogz Remix)")
    );
    assert_eq!(
        p.requetes,
        vec!["Stromae Alors on danse dubdogz", "Stromae Alors on danse"]
    );
}

#[test]
fn tiret_sans_espaces_et_autres_separateurs() {
    let p = preparer("A-ha - Take On Me", "X");
    assert_eq!(p.hypotheses[0], paire("A-ha", "Take On Me"));
    let p = preparer("Jay-Z – Empire State of Mind", "X");
    assert_eq!(p.hypotheses[0], paire("Jay-Z", "Empire State of Mind"));
    let p = preparer("Musikplayer :: Kraftwerk :: Autobahn", "X");
    assert_eq!(p.hypotheses[0], paire("Kraftwerk", "Autobahn"));
    let p = preparer("Jetzt läuft: Kraftwerk - Autobahn", "X");
    assert_eq!(p.hypotheses[0], paire("Kraftwerk", "Autobahn"));
}

#[test]
fn kjzz_titre_by_artiste_from_album() {
    let p = preparer(
        r#"How Deep is the Ocean? by Roy Haynes from "Out of the Afternoon" on JazzPHX"#,
        "KJZZ",
    );
    assert_eq!(
        p.hypotheses[0],
        paire("Roy Haynes", "How Deep is the Ocean?")
    );
    // « by » en majuscule n'est pas ce format.
    let p = preparer("Stand By Me", "X");
    assert!(p.hypotheses.is_empty());
}

#[test]
fn artista_desconocido_et_station_id() {
    for brut in ["Artista desconocido - Pista 1", "Station ID", "AD 12"] {
        assert!(preparer(brut, "X").ignorer.is_some(), "{brut}");
    }
    // « Queen » sur Queen FM reste un morceau : moins de 6 caractères.
    let p = preparer("Queen - Bohemian Rhapsody", "Queen FM");
    assert_eq!(p.ignorer, None);
    assert_eq!(p.hypotheses[0], paire("Queen", "Bohemian Rhapsody"));
}
