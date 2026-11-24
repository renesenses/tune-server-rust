//! #8452 — le repli « photo de l'artiste » (§ 3.6), hors réseau. Les
//! épreuves face à un faux iTunes sont dans `recherche_tests.rs`.

use super::photo_artiste::*;

#[test]
fn premier_nom_d_une_collaboration() {
    assert_eq!(premier_nom("MOGWAI [+] IGGY POP"), "MOGWAI");
    assert_eq!(premier_nom("X Feat. Y"), "X");
    assert_eq!(premier_nom("Lady Gaga, Bruno Mars"), "Lady Gaga");
    assert_eq!(premier_nom("Simon & Garfunkel"), "Simon");
    assert_eq!(premier_nom("Daft Punk"), "Daft Punk");
}

#[test]
fn le_bandeau_devient_un_carre() {
    assert_eq!(
        carre(
            "https://is1-ssl.mzstatic.example/image/thumb/ab/1200x630cw.png",
            600
        ),
        "https://is1-ssl.mzstatic.example/image/thumb/ab/600x600cc.jpg"
    );
    assert_eq!(
        carre(
            "https://is1-ssl.mzstatic.example/image/thumb/ab/1200x630cw.png",
            1200
        ),
        "https://is1-ssl.mzstatic.example/image/thumb/ab/1200x1200cc.jpg"
    );
    assert_eq!(
        carre("https://x.example/a.gif", 600),
        "https://x.example/a.gif"
    );
}

#[test]
fn la_balise_og_image() {
    let html = r#"<head><meta property="og:title" content="Air"><meta property="og:image" content="https://img.example/1200x630cw.png"></head>"#;
    assert_eq!(
        og_image(html).as_deref(),
        Some("https://img.example/1200x630cw.png")
    );
    assert_eq!(og_image("<head></head>"), None);
}

/// Apple peut changer sa page : la lecture tolère l'ordre des attributs, les
/// guillemets simples, `name=` et les entités, et refuse ce qui n'est pas
/// une adresse `https://`.
#[test]
fn la_balise_og_image_lue_avec_tolerance() {
    let attendu = Some("https://img.example/1200x630cw.png");
    for html in [
        r#"<meta content="https://img.example/1200x630cw.png" property="og:image">"#,
        r#"<meta property='og:image' content='https://img.example/1200x630cw.png' />"#,
        r#"<META NAME="og:image" CONTENT="https://img.example/1200x630cw.png">"#,
        "<meta\n  property=\"og:image\"\n  content=\"https://img.example/1200x630cw.png\"\n>",
    ] {
        assert_eq!(og_image(html).as_deref(), attendu, "{html}");
    }
    assert_eq!(
        og_image(r#"<meta property="og:image" content="https://img.example/a.png?x=1&amp;y=2">"#)
            .as_deref(),
        Some("https://img.example/a.png?x=1&y=2")
    );
    for html in [
        r#"<meta property="og:image" content="http://img.example/a.png">"#,
        r#"<meta property="og:image" content="javascript:alert(1)">"#,
        r#"<meta property="og:image" content="">"#,
        r#"<meta property="og:image">"#,
        r#"<meta property="og:image" content="https://img.example/<script>">"#,
        "<html><head><title>Nouvelle page</title></head></html>",
        "",
    ] {
        assert_eq!(og_image(html), None, "{html}");
    }
    // Une page énorme et sans balise : aucune panique, aucun résultat.
    let enorme = "<div>".repeat(200_000);
    assert_eq!(og_image(&enorme), None);
}

/// Des pages méconnaissables d'affilée suspendent le repli quelques heures ;
/// une page reconnue remet le compte à zéro.
#[test]
fn des_pages_meconnaissables_suspendent_le_repli() {
    let t0 = std::time::Instant::now();
    let mut s = Sante::new();
    for _ in 0..ECHECS_AVANT_SUSPENSION - 1 {
        s.constater(false, t0);
    }
    assert!(!s.suspendu(t0));
    s.constater(true, t0);
    for _ in 0..ECHECS_AVANT_SUSPENSION - 1 {
        s.constater(false, t0);
    }
    assert!(!s.suspendu(t0), "une page reconnue remet le compte à zéro");
    s.constater(false, t0);
    assert!(s.suspendu(t0));
    assert!(s.suspendu(t0 + SUSPENSION - std::time::Duration::from_secs(1)));
    assert!(!s.suspendu(t0 + SUSPENSION), "la suspension a une fin");
}

#[test]
fn la_photo_vient_du_cdn_d_apple_seulement() {
    assert!(image_admise(
        "https://is1-ssl.mzstatic.com/image/thumb/a/600x600cc.jpg"
    ));
    for mauvaise in [
        "http://is1-ssl.mzstatic.com/a.jpg",
        "https://mzstatic.com.exemple.net/a.jpg",
        "https://img.exemple.net/mzstatic.com/a.jpg",
        "pas une adresse",
    ] {
        assert!(!image_admise(mauvaise), "{mauvaise}");
    }
}

/// Le passe-droit des épreuves (une autre racine que celle d'iTunes, des
/// pages lues hors de music.apple.com) ne doit pas exister en production :
/// les champs de `Source` sont privés, et seul un constructeur réservé aux
/// épreuves (`cfg(test)` ou `services-factices`) vise une autre racine. Garde
/// de texte : une visibilité ne se constate pas à l'exécution d'une épreuve,
/// qui compile justement avec `cfg(test)`.
#[test]
fn le_passe_droit_des_epreuves_n_existe_pas_en_production() {
    let code = include_str!("mod.rs");
    let debut = code.find("pub struct Source<'a> {").expect("struct Source");
    let corps = &code[debut..debut + code[debut..].find('}').unwrap()];
    assert!(!corps.contains("pub base"), "champ `base` public : {corps}");
    assert!(
        !corps.contains("pub limiter"),
        "champ `limiter` public : {corps}"
    );
    let factice = code.find("pub fn factice(").expect("constructeur factice");
    let avant = &code[..factice];
    let attribut = avant.rfind("#[cfg(any(test, feature = \"services-factices\"))]");
    assert!(
        attribut.is_some_and(|a| !avant[a..].contains("\n\n")),
        "le constructeur factice doit être sous cfg(test | services-factices)"
    );
    // Et l'autre racine n'est jamais rendue hors épreuves.
    let racine = code.find("fn racine_factice(").expect("racine_factice");
    assert!(
        code[racine..]
            .split("\n}")
            .next()
            .unwrap()
            .contains("#[cfg(any(test, feature = \"services-factices\"))]")
    );
}
