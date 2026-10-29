//! Noms réels fournis par le signalement et le dossier ; fiches minimales
//! synthétiques, sans flux réseau ni copie d'un relevé de l'annuaire.

use super::*;

fn fiches() -> Vec<StationAnnuaire> {
    [
        ("BBC Radio 3 Unwind", "GB", "classical"),
        ("BBC Radio 3", "GB", "classical"),
        ("FIP Groove", "FR", "groove"),
        ("FIP", "FR", "eclectic"),
        ("Café del Mar", "ES", "chillout"),
    ]
    .into_iter()
    .map(|(name, country, genre)| StationAnnuaire {
        name: name.into(),
        country: Some(country.into()),
        genre: Some(genre.into()),
        ..Default::default()
    })
    .collect()
}

fn noms(corps: &Value) -> Vec<&str> {
    corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

#[test]
fn recherche_7198_tous_les_mots_du_nom_dans_n_importe_quel_ordre() {
    for (q, attendus) in [
        ("BBC unwind", vec!["BBC Radio 3 Unwind"]),
        ("unwind BBC", vec!["BBC Radio 3 Unwind"]),
        ("  uNwInD\t\n BBC  ", vec!["BBC Radio 3 Unwind"]),
        ("BBC\u{a0}unwind", vec!["BBC Radio 3 Unwind"]),
        ("groove fip", vec!["FIP Groove"]),
        ("FIP Groove", vec!["FIP Groove"]),
        ("MAR cafe", vec!["Café del Mar"]),
        ("mar CAFE\u{301}", vec!["Café del Mar"]),
        ("BBC groove", vec![]),
        ("BBC unwind absent", vec![]),
    ] {
        let corps = corps_annuaire(
            &fiches(),
            &[],
            &FiltresAnnuaire {
                q: Some(q.into()),
                ..Default::default()
            },
            &EtatAnnuaire::default(),
        );
        assert_eq!(
            noms(&corps),
            attendus,
            "La recherche {q:?} doit retrouver tous les mots du nom sans imposer leur ordre"
        );
        assert_eq!(corps["total"], attendus.len(), "{q:?} : {corps}");
    }
}

#[test]
fn recherche_7198_un_mot_et_les_champs_existants_restent_recherchables() {
    let mut fiches = fiches();
    fiches[2].description = Some("Sélection musicale".into());
    fiches[2].categories = vec!["Découvertes".into()];
    fiches[2].keywords = vec!["Radio France".into()];
    for (q, attendus) in [
        ("bBc", vec!["BBC Radio 3 Unwind", "BBC Radio 3"]),
        ("FIP", vec!["FIP Groove", "FIP"]),
        ("gro", vec!["FIP Groove"]),
        ("CAFÉ", vec!["Café del Mar"]),
        ("cafe", vec!["Café del Mar"]),
        ("CLASSICAL", vec!["BBC Radio 3 Unwind", "BBC Radio 3"]),
        ("gb", vec!["BBC Radio 3 Unwind", "BBC Radio 3"]),
        ("selection", vec!["FIP Groove"]),
        ("decouvertes", vec!["FIP Groove"]),
        ("france", vec!["FIP Groove"]),
        ("radio france", vec!["FIP Groove"]),
        ("france radio", vec!["FIP Groove"]),
        ("introuvable", vec![]),
    ] {
        let corps = corps_annuaire(
            &fiches,
            &[],
            &FiltresAnnuaire {
                q: Some(q.into()),
                ..Default::default()
            },
            &EtatAnnuaire::default(),
        );
        assert_eq!(noms(&corps), attendus, "{q:?} : {corps}");
    }
    for q in [None, Some("".into()), Some(" \t\n ".into())] {
        let corps = corps_annuaire(
            &fiches,
            &[],
            &FiltresAnnuaire {
                q,
                ..Default::default()
            },
            &EtatAnnuaire::default(),
        );
        assert_eq!(corps["total"], fiches.len());
    }
}

#[test]
fn recherche_7198_filtres_facettes_et_pagination_portent_sur_les_correspondances() {
    let fiches = fiches();
    let filtres = FiltresAnnuaire {
        q: Some("3 bbc".into()),
        country: Some("gb".into()),
        genre: Some("classical".into()),
        offset: Some(1),
        limit: Some(1),
    };
    let corps = corps_annuaire(&fiches, &[], &filtres, &EtatAnnuaire::default());
    assert_eq!(noms(&corps), vec!["BBC Radio 3"]);
    assert_eq!(corps["total"], 2);
    assert_eq!(corps["count"], 1);
    assert_eq!(corps["countries"], json!([{"code": "GB", "count": 2}]));
    assert_eq!(corps["genres"], json!([{"key": "classical", "count": 2}]));
    let corps = corps_annuaire(
        &fiches,
        &[],
        &FiltresAnnuaire {
            country: Some("fr".into()),
            ..filtres
        },
        &EtatAnnuaire::default(),
    );
    assert_eq!(corps["total"], 0);
}
