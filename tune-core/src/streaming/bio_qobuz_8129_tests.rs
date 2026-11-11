//! Témoins de #8129 : les liens des biographies Qobuz.

use super::*;

fn index() -> IndexDesNoms {
    IndexDesNoms::construire(
        vec![
            (1, "Miles Davis".to_string()),
            (2, "John Coltrane".to_string()),
            (3, "Bill Evans".to_string()),
        ],
        vec![
            (100, "Kind of Blue".to_string()),
            (101, "Giant Steps".to_string()),
        ],
    )
}

fn qobuz(genre: GenreDeLien, id: &str) -> LienDeService {
    LienDeService {
        service: "qobuz".into(),
        genre,
        id: id.into(),
    }
}

/// Un extrait de bio Qobuz telle que `/artist/get` la rend : paragraphes,
/// entités, liens vers des artistes, un album, l'extérieur, et un lien piège.
const BIO_MILES: &str = r#"<p>Trompettiste de génie, Miles Davis a joué avec <a href="https://www.qobuz.com/fr-fr/interpreter/john-coltrane/16422">John Coltrane</a> et <a href='/fr-fr/interpreter/cannonball-adderley/26103'>Cannonball Adderley</a>.</p>
<p>Son album <a href="https://open.qobuz.com/album/0886443853839"><em>Kind of Blue</em></a> (1959) reste le disque de jazz le plus vendu&nbsp;; voir aussi <a href="https://fr.wikipedia.org/wiki/Miles_Davis">sa page Wikipédia</a>, <a href="http://example.com/x">un site</a> et <a href="javascript:alert(1)">ceci</a>.</p>
<p>Bill Evans y tient le piano. <a href="https://open.qobuz.com/artist/1234">Miles Davis</a>.</p>"#;

fn page_miles() -> PageDeLaBio<'static> {
    PageDeLaBio {
        genre: GenreDeLien::Artiste,
        id: "1234",
        noms: &["Miles Davis"],
    }
}

#[test]
fn les_pages_qobuz_sont_reconnues_8129() {
    use GenreDeLien::*;
    let cas = [
        (
            "https://open.qobuz.com/artist/36819",
            Some((Artiste, "36819")),
        ),
        (
            "https://play.qobuz.com/album/0886443853839",
            Some((Album, "0886443853839")),
        ),
        (
            "https://www.qobuz.com/fr-fr/interpreter/miles-davis/36819",
            Some((Artiste, "36819")),
        ),
        (
            "http://www.qobuz.com/gb-en/album/kind-of-blue-miles-davis/0886443853839",
            Some((Album, "0886443853839")),
        ),
        (
            "/fr-fr/album/giant-steps/hjk1x2y3",
            Some((Album, "hjk1x2y3")),
        ),
        // Sans identifiant, ou identifiant d'artiste non numérique.
        ("https://www.qobuz.com/fr-fr/interpreter/miles-davis", None),
        ("https://open.qobuz.com/artist/miles", None),
        // Pas Qobuz, ou Qobuz imité.
        ("https://qobuz.com.evil.example/artist/1", None),
        ("https://evilqobuz.com/artist/1", None),
        ("javascript:alert(1)", None),
        ("//open.qobuz.com/artist/1", None),
        ("https://user@open.qobuz.com/artist/1", None),
        ("https://www.qobuz.com/fr-fr/label/blue-note/1", None),
    ];
    for (href, attendu) in cas {
        assert_eq!(
            page_qobuz(href),
            attendu.map(|(g, i)| (g, i.to_string())),
            "{href}"
        );
    }
}

#[test]
fn une_bio_qobuz_garde_ses_liens_8129() {
    let (texte, segments) = bio_qobuz_segmentee(BIO_MILES, &index(), &page_miles()).unwrap();

    // Le texte est recomposé à l'identique, sans balise ni entité.
    let recompose: String = segments.iter().map(|s| s.texte.as_str()).collect();
    assert_eq!(recompose, texte);
    assert!(
        !texte.contains('<') && !texte.contains("&nbsp;"),
        "{texte:?}"
    );
    assert!(texte.starts_with("Trompettiste de génie"), "{texte:?}");

    let lien_de = |t: &str| segments.iter().find(|s| s.texte == t).cloned();

    // Liens Qobuz → fiches Qobuz de Tune, prioritaires sur la bibliothèque
    // (John Coltrane et Kind of Blue y sont aussi).
    let coltrane = lien_de("John Coltrane").expect("John Coltrane lié");
    assert_eq!(
        coltrane.lien_service,
        Some(qobuz(GenreDeLien::Artiste, "16422"))
    );
    assert_eq!(coltrane.lien, None);
    assert_eq!(
        lien_de("Cannonball Adderley").unwrap().lien_service,
        Some(qobuz(GenreDeLien::Artiste, "26103"))
    );
    assert_eq!(
        lien_de("Kind of Blue").unwrap().lien_service,
        Some(qobuz(GenreDeLien::Album, "0886443853839"))
    );

    // Lien https sur un domaine de source → externe ; le reste → texte.
    assert_eq!(
        lien_de("sa page Wikipédia").unwrap().url.as_deref(),
        Some("https://fr.wikipedia.org/wiki/Miles_Davis")
    );
    for t in ["un site", "ceci"] {
        assert!(lien_de(t).is_none(), "{t} ne doit pas être lié");
        assert!(texte.contains(t));
    }

    // Nom de la bibliothèque, sans lien dans le HTML → lien interne.
    assert_eq!(
        lien_de("Bill Evans").unwrap().lien,
        Some(LienInterne {
            genre: GenreDeLien::Artiste,
            id: 3
        })
    );

    // L'artiste de la page : ni lien Qobuz vers lui-même, ni homonyme local.
    for s in &segments {
        if s.texte.contains("Miles Davis") {
            assert!(
                s.lien.is_none() && s.lien_service.is_none() && s.url.is_none(),
                "Miles Davis lié : {s:?}"
            );
        }
    }
}

#[test]
fn le_json_des_segments_porte_le_service_et_l_identifiant_8129() {
    let (_, segments) = bio_qobuz_segmentee(BIO_MILES, &index(), &page_miles()).unwrap();
    let json = serde_json::to_value(&segments).unwrap();
    let coltrane = json
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["texte"] == "John Coltrane")
        .unwrap();
    assert_eq!(
        coltrane["lien_service"],
        serde_json::json!({"service": "qobuz", "type": "artiste", "id": "16422"})
    );
    assert!(coltrane.get("lien").is_none() && coltrane.get("url").is_none());
}

#[test]
fn une_presentation_d_album_ne_lie_ni_l_album_ni_son_artiste_8129() {
    let html = r#"<p>Avec <a href="https://open.qobuz.com/album/0886443853839">Kind of Blue</a>, Miles Davis et John Coltrane signent un classique. Voir <a href="https://open.qobuz.com/album/abc123">Giant Steps</a>.</p>"#;
    let page = PageDeLaBio {
        genre: GenreDeLien::Album,
        id: "0886443853839",
        noms: &["Kind of Blue", "Miles Davis"],
    };
    let (_, segments) = bio_qobuz_segmentee(html, &index(), &page).unwrap();
    let lie = |t: &str| {
        segments
            .iter()
            .any(|s| s.texte == t && (s.lien.is_some() || s.lien_service.is_some()))
    };
    assert!(!lie("Kind of Blue"));
    assert!(!lie("Miles Davis"));
    assert!(lie("John Coltrane"));
    assert_eq!(
        segments
            .iter()
            .find(|s| s.texte == "Giant Steps")
            .unwrap()
            .lien_service,
        Some(qobuz(GenreDeLien::Album, "abc123"))
    );
}

#[test]
fn sans_texte_rien_8129() {
    assert!(bio_qobuz_segmentee("<p> </p>", &index(), &page_miles()).is_none());
}
