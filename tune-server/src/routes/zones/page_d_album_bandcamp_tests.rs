//! La page d'album Bandcamp envoyée par le client : ce qui entre, ce qui
//! reste dehors (web#1923, web#1924).
use super::page_d_album_bandcamp_sure as sure;

#[test]
fn une_page_d_album_ou_de_piste_bandcamp_entre() {
    for page in [
        "https://framewerk.bandcamp.com/album/love-parade",
        "https://framewerk.bandcamp.com/track/meet-her",
        "https://un-label.bandcamp.com/album/x?from=search",
        "https://Framewerk.Bandcamp.com/album/love-parade",
    ] {
        assert_eq!(sure(Some(page)).as_deref(), Some(page), "{page}");
    }
}

#[test]
fn les_blancs_autour_sont_retires() {
    assert_eq!(
        sure(Some("  https://framewerk.bandcamp.com/album/love-parade\n")).as_deref(),
        Some("https://framewerk.bandcamp.com/album/love-parade")
    );
}

#[test]
fn un_schema_autre_que_https_reste_dehors() {
    for page in [
        "http://framewerk.bandcamp.com/album/love-parade",
        "javascript:alert(1)//bandcamp.com/album/x",
        "file:///etc/bandcamp.com/album/x",
        "//framewerk.bandcamp.com/album/love-parade",
        "framewerk.bandcamp.com/album/love-parade",
    ] {
        assert_eq!(sure(Some(page)), None, "{page}");
    }
}

#[test]
fn un_domaine_sosie_reste_dehors() {
    for page in [
        "https://evilbandcamp.com/album/x",
        "https://bandcamp.com.exemple.net/album/x",
        "https://framewerk.bandcamp.com.exemple.net/album/x",
        "https://exemple.net/framewerk.bandcamp.com/album/x",
        "https://.bandcamp.com/album/x",
        "https://a..bandcamp.com/album/x",
        "https://t4.bcbits.com/stream/abc/mp3-128/111",
    ] {
        assert_eq!(sure(Some(page)), None, "{page}");
    }
}

#[test]
fn identifiants_port_et_caracteres_de_controle_restent_dehors() {
    for page in [
        "https://moi@framewerk.bandcamp.com/album/x",
        "https://framewerk.bandcamp.com@exemple.net/album/x",
        "https://framewerk.bandcamp.com:8443/album/x",
        "https://framewerk.bandcamp.com\\@exemple.net/album/x",
        "https://framewerk.bandcamp.com/album/a b",
        "https://framewerk.bandcamp.com/album/x\u{0}",
    ] {
        assert_eq!(sure(Some(page)), None, "{page:?}");
    }
}

#[test]
fn sans_chemin_ou_trop_longue_reste_dehors() {
    assert_eq!(sure(Some("https://framewerk.bandcamp.com")), None);
    assert_eq!(sure(Some("https://framewerk.bandcamp.com/")), None);
    assert_eq!(sure(Some("")), None);
    assert_eq!(sure(None), None);
    let longue = format!("https://framewerk.bandcamp.com/album/{}", "a".repeat(2100));
    assert_eq!(sure(Some(&longue)), None);
}
