//! #8452 — parité avec le code de référence `pretraitement.py` : chaque
//! ligne est la sortie de `preparer(brut, station)` du Python, relevée le
//! 14/11/2026 sur les exemples de la spécification (§ 2). Une divergence du
//! portage Rust se lit ici, ligne par ligne. Aucun réseau.

use super::pretraitement::preparer;

struct Cas {
    brut: &'static str,
    station: &'static str,
    ignorer: bool,
    requetes: &'static [&'static str],
    hypotheses: &'static [(&'static str, &'static str)],
    pochette_fournie: Option<&'static str>,
}

const CAS: &[Cas] = &[
    Cas {
        brut: "GOOD TIMES BAD TIMES - LED ZEPPELIN",
        station: "Oüi FM: Alternatif",
        ignorer: false,
        requetes: &["GOOD TIMES BAD TIMES LED ZEPPELIN"],
        hypotheses: &[
            ("GOOD TIMES BAD TIMES", "LED ZEPPELIN"),
            ("LED ZEPPELIN", "GOOD TIMES BAD TIMES"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Vienna Scientists - Menheads - Get This Text On Television - 674FM",
        station: "674FM",
        ignorer: false,
        requetes: &[
            "Vienna Scientists Menheads",
            "Vienna Scientists Menheads Get This Text On Television",
        ],
        hypotheses: &[
            ("Vienna Scientists", "Menheads"),
            ("Menheads", "Vienna Scientists"),
            ("Vienna Scientists", "Get This Text On Television"),
            ("Menheads", "Get This Text On Television"),
            ("Get This Text On Television", "Vienna Scientists"),
            ("Get This Text On Television", "Menheads"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Freddie's Tune by Charlie Byrd on JazzPHX",
        station: "KJZZ",
        ignorer: false,
        requetes: &["Charlie Byrd Freddie's Tune"],
        hypotheses: &[
            ("Charlie Byrd", "Freddie's Tune"),
            ("Freddie's Tune", "Charlie Byrd"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Stand By Me - Ben E. King",
        station: "X",
        ignorer: false,
        requetes: &["Stand By Me Ben E. King"],
        hypotheses: &[
            ("Stand By Me", "Ben E. King"),
            ("Ben E. King", "Stand By Me"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Now playing: Smokie - Living Next Door To Alice",
        station: "X",
        ignorer: false,
        requetes: &["Smokie Living Next Door To Alice"],
        hypotheses: &[
            ("Smokie", "Living Next Door To Alice"),
            ("Living Next Door To Alice", "Smokie"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Nu op Novo3 : Smokie - Living Next Door To Alice",
        station: "NOVO3",
        ignorer: false,
        requetes: &["Smokie Living Next Door To Alice"],
        hypotheses: &[
            ("Smokie", "Living Next Door To Alice"),
            ("Living Next Door To Alice", "Smokie"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Heinrich Bach - 02 - Vox Luminis - Kyrie a 6",
        station: "X",
        ignorer: false,
        requetes: &[
            "Heinrich Bach Vox Luminis",
            "Heinrich Bach Vox Luminis Kyrie a 6",
        ],
        hypotheses: &[
            ("Heinrich Bach", "Vox Luminis"),
            ("Vox Luminis", "Heinrich Bach"),
            ("Heinrich Bach", "Kyrie a 6"),
            ("Vox Luminis", "Kyrie a 6"),
            ("Kyrie a 6", "Heinrich Bach"),
            ("Kyrie a 6", "Vox Luminis"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Artiste - Titre | Label | REF01 | 1999 | FR",
        station: "Intergalactic FM",
        ignorer: false,
        requetes: &["Artiste Titre"],
        hypotheses: &[("Artiste", "Titre"), ("Titre", "Artiste")],
        pochette_fournie: None,
    },
    Cas {
        brut: "Daft Punk - Get Lucky (feat. Pharrell Williams) [Radio Edit]",
        station: "X",
        ignorer: false,
        requetes: &["Daft Punk Get Lucky"],
        hypotheses: &[
            (
                "Daft Punk",
                "Get Lucky (feat. Pharrell Williams) [Radio Edit]",
            ),
            (
                "Get Lucky (feat. Pharrell Williams) [Radio Edit]",
                "Daft Punk",
            ),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Simon & Garfunkel - The Boxer",
        station: "X",
        ignorer: false,
        requetes: &["Simon & Garfunkel The Boxer"],
        hypotheses: &[
            ("Simon & Garfunkel", "The Boxer"),
            ("The Boxer", "Simon & Garfunkel"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Mogwai feat. Iggy Pop - Punk Rock",
        station: "X",
        ignorer: false,
        requetes: &["Mogwai Punk Rock"],
        hypotheses: &[
            ("Mogwai feat. Iggy Pop", "Punk Rock"),
            ("Punk Rock", "Mogwai feat. Iggy Pop"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Lisa Leblanc - Pourquoi faire aujourd'hui (Poirier remix)",
        station: "X",
        ignorer: false,
        requetes: &[
            "Lisa Leblanc Pourquoi faire aujourd'hui poirier",
            "Lisa Leblanc Pourquoi faire aujourd'hui",
        ],
        hypotheses: &[
            ("Lisa Leblanc", "Pourquoi faire aujourd'hui (Poirier remix)"),
            ("Pourquoi faire aujourd'hui (Poirier remix)", "Lisa Leblanc"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Jay-Z",
        station: "X",
        ignorer: false,
        requetes: &["Jay-Z"],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Lizzo - text=\"About Damn Time\" song_spot=\"M\" amgArtworkURL=\"https://i.iheart.example/a.jpg\"",
        station: "KIIS",
        ignorer: false,
        requetes: &["Lizzo About Damn Time"],
        hypotheses: &[("Lizzo", "About Damn Time"), ("About Damn Time", "Lizzo")],
        pochette_fournie: Some("https://i.iheart.example/a.jpg"),
    },
    Cas {
        brut: "KIIS - text=\"Win tickets\" song_spot=\"T\"",
        station: "KIIS",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Daft Punk - Get Lucky - Single",
        station: "X",
        ignorer: false,
        requetes: &["Daft Punk Get Lucky"],
        hypotheses: &[("Daft Punk", "Get Lucky"), ("Get Lucky", "Daft Punk")],
        pochette_fournie: None,
    },
    Cas {
        brut: "Bicep - Glue - EP",
        station: "X",
        ignorer: false,
        requetes: &["Bicep Glue"],
        hypotheses: &[("Bicep", "Glue"), ("Glue", "Bicep")],
        pochette_fournie: None,
    },
    Cas {
        brut: "Stromae - Alors on danse - Dubdogz Remix",
        station: "X",
        ignorer: false,
        requetes: &["Stromae Alors on danse dubdogz", "Stromae Alors on danse"],
        hypotheses: &[
            ("Stromae", "Alors on danse (Dubdogz Remix)"),
            ("Alors on danse (Dubdogz Remix)", "Stromae"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "A-ha - Take On Me",
        station: "X",
        ignorer: false,
        requetes: &["A-ha Take On Me"],
        hypotheses: &[("A-ha", "Take On Me"), ("Take On Me", "A-ha")],
        pochette_fournie: None,
    },
    Cas {
        brut: "Jay-Z – Empire State of Mind",
        station: "X",
        ignorer: false,
        requetes: &["Jay-Z Empire State of Mind"],
        hypotheses: &[
            ("Jay-Z", "Empire State of Mind"),
            ("Empire State of Mind", "Jay-Z"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Musikplayer :: Kraftwerk :: Autobahn",
        station: "X",
        ignorer: false,
        requetes: &["Kraftwerk Autobahn"],
        hypotheses: &[("Kraftwerk", "Autobahn"), ("Autobahn", "Kraftwerk")],
        pochette_fournie: None,
    },
    Cas {
        brut: "Jetzt läuft: Kraftwerk - Autobahn",
        station: "X",
        ignorer: false,
        requetes: &["Kraftwerk Autobahn"],
        hypotheses: &[("Kraftwerk", "Autobahn"), ("Autobahn", "Kraftwerk")],
        pochette_fournie: None,
    },
    Cas {
        brut: "How Deep is the Ocean? by Roy Haynes from \"Out of the Afternoon\" on JazzPHX",
        station: "KJZZ",
        ignorer: false,
        requetes: &["Roy Haynes How Deep is the Ocean?"],
        hypotheses: &[
            ("Roy Haynes", "How Deep is the Ocean?"),
            ("How Deep is the Ocean?", "Roy Haynes"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Stand By Me",
        station: "X",
        ignorer: false,
        requetes: &["Stand By Me"],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Artista desconocido - Pista 1",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Station ID",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "AD 12",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Queen - Bohemian Rhapsody",
        station: "Queen FM",
        ignorer: false,
        requetes: &["Queen Bohemian Rhapsody"],
        hypotheses: &[
            ("Queen", "Bohemian Rhapsody"),
            ("Bohemian Rhapsody", "Queen"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: " - ",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "STOP ADBREAK 1",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "AD 2",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Spot Block End",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Now Playing info goes here",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "2GB_PROMO_BREAKFAST_01",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "6PR10011-0660",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "www.maradio.example",
        station: "X",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Radio Globus - Altid godt for dig!",
        station: "Radio Globus",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "FluxFM - Livestream",
        station: "FluxFM",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Radio Nova",
        station: "Radio Nova",
        ignorer: true,
        requetes: &[],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "MOGWAI [+] IGGY POP - Punk Rock",
        station: "X",
        ignorer: false,
        requetes: &["MOGWAI IGGY POP Punk Rock"],
        hypotheses: &[
            ("MOGWAI [+] IGGY POP", "Punk Rock"),
            ("Punk Rock", "MOGWAI [+] IGGY POP"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Tank - When We Get On",
        station: "X",
        ignorer: false,
        requetes: &["Tank When We Get On"],
        hypotheses: &[("Tank", "When We Get On"), ("When We Get On", "Tank")],
        pochette_fournie: None,
    },
    Cas {
        brut: "WHERE IS MY MIND - PIXIES",
        station: "X",
        ignorer: false,
        requetes: &["WHERE IS MY MIND PIXIES"],
        hypotheses: &[
            ("WHERE IS MY MIND", "PIXIES"),
            ("PIXIES", "WHERE IS MY MIND"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Air Sexy Boy",
        station: "X",
        ignorer: false,
        requetes: &["Air Sexy Boy"],
        hypotheses: &[],
        pochette_fournie: None,
    },
    Cas {
        brut: "Barry Can`t  Swim - When Will We Land",
        station: "X",
        ignorer: false,
        requetes: &["Barry Can't Swim When Will We Land"],
        hypotheses: &[
            ("Barry Can't Swim", "When Will We Land"),
            ("When Will We Land", "Barry Can't Swim"),
        ],
        pochette_fournie: None,
    },
    Cas {
        brut: "Ori\u{c3}\u{ab}nt - Test",
        station: "X",
        ignorer: false,
        requetes: &["Oriënt Test"],
        hypotheses: &[("Oriënt", "Test"), ("Test", "Oriënt")],
        pochette_fournie: None,
    },
];

#[test]
fn parite_avec_le_code_de_reference() {
    for c in CAS {
        let p = preparer(c.brut, c.station);
        assert_eq!(p.ignorer.is_some(), c.ignorer, "ignorer : « {} »", c.brut);
        assert_eq!(p.requetes, c.requetes, "requêtes : « {} »", c.brut);
        let h: Vec<(&str, &str)> = p
            .hypotheses
            .iter()
            .map(|(a, t)| (a.as_str(), t.as_str()))
            .collect();
        assert_eq!(h, c.hypotheses, "hypothèses : « {} »", c.brut);
        assert_eq!(
            p.pochette_fournie.as_deref(),
            c.pochette_fournie,
            "pochette fournie : « {} »",
            c.brut
        );
    }
}
