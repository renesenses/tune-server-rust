//! #7152 — la résolution du flux d'une fiche Apple sans `feedUrl`.

use super::*;
use std::sync::Arc;

type Repondeur = fn(&str, &str) -> (u16, String);

/// Faux serveur local : `repondre(ligne de requête, racine)` rend le statut et
/// le corps. Garde le texte complet de chaque requête reçue (en-têtes compris).
async fn faux_serveur(repondre: Repondeur) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let racine = format!("http://{}", ecoute.local_addr().unwrap());
    let recues = Arc::new(Mutex::new(Vec::new()));
    let journal = recues.clone();
    let r2 = racine.clone();
    tokio::spawn(async move {
        while let Ok((mut flux, _)) = ecoute.accept().await {
            let mut tampon = vec![0u8; 8192];
            let n = flux.read(&mut tampon).await.unwrap_or(0);
            let texte = String::from_utf8_lossy(&tampon[..n]).to_string();
            journal.lock().unwrap().push(texte.clone());
            let ligne = texte.lines().next().unwrap_or("").to_string();
            let (statut, corps) = repondre(&ligne, &r2);
            let http = format!(
                "HTTP/1.1 {statut} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                corps.len()
            );
            let _ = flux.write_all(http.as_bytes()).await;
        }
    });
    (racine, recues)
}

fn annuaires(racine: &str) -> Annuaires {
    Annuaires::avec_bases(
        crate::http::client::shared().clone(),
        &format!("{racine}/gp"),
        &format!("{racine}/fy"),
    )
}

fn flux_rss(titre: &str, auteur: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><rss><channel><title>{titre}</title><image><title>image</title></image><itunes:author>{auteur}</itunes:author><item><title>Episode 1</title><enclosure url="http://x/1.mp3"/></item></channel></rss>"#
    )
}

#[test]
fn la_normalisation_est_litterale_mais_sans_accents_ni_ponctuation() {
    assert_eq!(
        normaliser("Sur les épaules de Darwin"),
        "sur les epaules de darwin"
    );
    assert_eq!(
        normaliser("LSD, La série  documentaire"),
        "lsd la serie documentaire"
    );
    assert_eq!(
        normaliser("Sommar &amp; Vinter i P1"),
        normaliser("Sommar & Vinter i P1")
    );
    assert_eq!(normaliser("Entendez-vous l’éco ?"), "entendez vous l eco");
    assert!(titres_concordent(
        "Les pieds sur terre",
        "Les Pieds sur terre"
    ));
    // Jamais « au plus proche » : un sous-titre suffit à refuser.
    assert!(!titres_concordent(
        "Les Pieds sur terre : la sélection des auditeurs",
        "Les Pieds sur terre"
    ));
    assert!(!titres_concordent("", ""));
}

#[test]
fn l_auteur_se_compare_sans_courriel_ni_parentheses() {
    assert!(auteurs_concordent(
        "France Inter (podcast@radiofrance.com)",
        "France Inter"
    ));
    assert!(!auteurs_concordent(
        "France Culture (podcast@radiofrance.com)",
        "France Inter"
    ));
    assert!(!auteurs_concordent("Marguerite Du Trash", "France Inter"));
    assert!(!auteurs_concordent("", ""));
}

#[test]
fn l_entete_de_chaine_ignore_les_titres_des_episodes() {
    let (titre, auteur) =
        entete_de_chaine(&flux_rss("Affaires sensibles", "France Inter")).unwrap();
    assert_eq!(titre, "Affaires sensibles");
    assert_eq!(auteur, "France Inter");
    // Sans titre de chaîne avant le premier épisode : rien.
    assert!(entete_de_chaine("<rss><channel><item><title>Ep</title></item>").is_none());
}

/// Concordance stricte : parmi les fiches gpodder, l'homonyme (même titre,
/// autre auteur) et le titre approchant sont écartés ; la bonne est vérifiée
/// sur l'en-tête de SON flux, demandé par plage d'octets.
#[tokio::test]
async fn flux_resolu_par_concordance_stricte() {
    fn r(ligne: &str, racine: &str) -> (u16, String) {
        if ligne.contains("/gp/search.json") {
            return (
                200,
                format!(
                    r#"[{{"title":"Affaires sensibles","author":"Radio Pirate","url":"{racine}/flux/pirate.xml"}},
                   {{"title":"Affaires sensibles : le best of","author":"France Inter","url":"{racine}/flux/bestof.xml"}},
                   {{"title":"Affaires sensibles","author":"France Inter (podcast@radiofrance.com)","url":"http://{h}/flux/bon.xml"}},
                   {{"title":"Affaires sensibles","author":"France Inter (podcast@radiofrance.com)","url":"{racine}/flux/bon.xml"}}]"#,
                    h = racine.trim_start_matches("http://")
                ),
            );
        }
        if ligne.contains("/flux/bon.xml") {
            return (200, flux_rss("Affaires sensibles", "France Inter"));
        }
        (404, "{}".into())
    }
    let (racine, recues) = faux_serveur(r).await;
    let trouve = annuaires(&racine)
        .resoudre(Some("100001"), "Affaires sensibles", "France Inter")
        .await;
    assert_eq!(
        trouve,
        Some(FluxResolu {
            url: format!("{racine}/flux/bon.xml"),
            source: SourceFlux::Gpodder
        }),
        "requêtes : {:?}",
        recues.lock().unwrap()
    );
    let lues = recues.lock().unwrap().clone();
    assert!(
        !lues
            .iter()
            .any(|q| q.contains("/flux/pirate.xml") || q.contains("/flux/bestof.xml")),
        "l'homonyme et le titre approchant ne sont même pas ouverts : {lues:?}"
    );
    let entete = lues.iter().find(|q| q.contains("/flux/bon.xml")).unwrap();
    assert!(
        entete.to_ascii_lowercase().contains("range: bytes=0-65535"),
        "seul l'en-tête du flux est demandé : {entete}"
    );
}

/// Un homonyme qui se cache : la fiche de l'annuaire porte le bon titre et le
/// bon auteur, mais le FLUX est celui d'un autre auteur. Refusé — et un flux
/// mort l'est aussi.
#[tokio::test]
async fn homonyme_refuse_sur_l_entete_du_flux() {
    fn r(ligne: &str, racine: &str) -> (u16, String) {
        if ligne.contains("/gp/search.json") {
            return (
                200,
                format!(
                    r#"[{{"title":"Le Masque et la Plume","author":"","url":"{racine}/flux/autre.xml"}},
                   {{"title":"Le Masque et la Plume","author":"France Inter","url":"{racine}/flux/mort.xml"}}]"#
                ),
            );
        }
        if ligne.contains("/fy/search/podcast") {
            return (200, r#"{"data":[]}"#.into());
        }
        if ligne.contains("/flux/autre.xml") {
            return (
                200,
                flux_rss("Le Masque et la Plume", "Marguerite Du Trash"),
            );
        }
        (404, "{}".into())
    }
    let (racine, _) = faux_serveur(r).await;
    assert_eq!(
        annuaires(&racine)
            .resoudre(Some("100002"), "Le Masque et la Plume", "France Inter")
            .await,
        None
    );
}

/// gpodder ne connaît pas l'émission : fyyd suit, et son titre HTML-échappé
/// concorde.
#[tokio::test]
async fn fyyd_prend_le_relais_de_gpodder() {
    fn r(ligne: &str, racine: &str) -> (u16, String) {
        if ligne.contains("/gp/search.json") {
            return (200, "[]".into());
        }
        if ligne.contains("/fy/search/podcast") {
            return (
                200,
                format!(
                    r#"{{"data":[{{"title":"Sommar &amp; Vinter i P1","author":"Sveriges Radio","xmlURL":"{racine}/flux/sv.xml"}}]}}"#
                ),
            );
        }
        if ligne.contains("/flux/sv.xml") {
            return (200, flux_rss("Sommar &amp; Vinter i P1", "Sveriges Radio"));
        }
        (404, "{}".into())
    }
    let (racine, _) = faux_serveur(r).await;
    let trouve = annuaires(&racine)
        .resoudre(Some("100003"), "Sommar & Vinter i P1", "Sveriges Radio")
        .await
        .expect("fyyd");
    assert_eq!(trouve.source, SourceFlux::Fyyd);
}

/// Podcast Index par le relais : le jeton du compte part en `Bearer`, le pont
/// par identifiant Apple est demandé en premier ; le jeton n'apparaît jamais
/// dans un `Debug`.
#[tokio::test]
async fn podcast_index_par_le_relais_du_site() {
    fn r(ligne: &str, racine: &str) -> (u16, String) {
        if ligne.contains("/gp/") {
            return (200, "[]".into());
        }
        if ligne.contains("/fy/") {
            return (200, r#"{"data":[]}"#.into());
        }
        if ligne.contains("/site/api/v1/podcastindex/byitunesid?id=100004") {
            return (
                200,
                format!(
                    r#"{{"feeds":[{{"title":"Veleno","author":"la Repubblica","url":"{racine}/flux/veleno.xml"}}]}}"#
                ),
            );
        }
        if ligne.contains("/flux/veleno.xml") {
            return (200, flux_rss("Veleno", "la Repubblica"));
        }
        (404, "{}".into())
    }
    let (racine, recues) = faux_serveur(r).await;
    let relais = RelaisPodcastIndex::new(&format!("{racine}/site"), "jeton-secret-de-banc");
    assert!(!format!("{relais:?}").contains("jeton-secret-de-banc"));
    let trouve = annuaires(&racine)
        .avec_relais(Some(relais))
        .resoudre(Some("100004"), "Veleno", "la Repubblica")
        .await
        .expect("Podcast Index");
    assert_eq!(trouve.source, SourceFlux::PodcastIndex);
    let lues = recues.lock().unwrap().clone();
    let appel = lues.iter().find(|q| q.contains("/byitunesid")).unwrap();
    assert!(
        appel
            .to_ascii_lowercase()
            .contains("authorization: bearer jeton-secret-de-banc"),
        "{appel}"
    );
}

/// Relais sans clé (503) : la source est sautée, sans panne ; la fiche n'a
/// pas de flux.
#[tokio::test]
async fn relais_sans_cle_la_source_est_sautee() {
    fn r(ligne: &str, _: &str) -> (u16, String) {
        if ligne.contains("/gp/") {
            return (200, "[]".into());
        }
        if ligne.contains("/fy/") {
            return (200, r#"{"data":[]}"#.into());
        }
        if ligne.contains("/podcastindex/") {
            return (503, r#"{"error":"podcastindex_inactif"}"#.into());
        }
        (404, "{}".into())
    }
    let (racine, recues) = faux_serveur(r).await;
    let a = annuaires(&racine).avec_relais(Some(RelaisPodcastIndex::new(
        &format!("{racine}/site"),
        "j",
    )));
    assert_eq!(
        a.resoudre(Some("100005"), "Inconnu", "Personne").await,
        None
    );
    // Le relais est en pause : une seconde fiche ne le rappelle pas.
    assert_eq!(
        a.resoudre(Some("100006"), "Autre inconnu", "Personne")
            .await,
        None
    );
    let appels = recues
        .lock()
        .unwrap()
        .iter()
        .filter(|q| q.contains("/podcastindex/"))
        .count();
    assert_eq!(appels, 1, "un 503 met le relais en pause");
}

/// La recherche : la fiche Apple avec flux passe telle quelle, celle sans
/// flux est résolue et GARDÉE à sa place, celle qu'aucun annuaire ne résout
/// n'est pas proposée.
#[tokio::test]
async fn la_recherche_garde_les_fiches_sans_flux_resolues() {
    fn r(ligne: &str, racine: &str) -> (u16, String) {
        if ligne.contains("/itunes/search") {
            return (
                200,
                format!(
                    r#"{{"resultCount":3,"results":[
                {{"trackId":1,"trackName":"Les pieds sur terre","artistName":"France Culture"}},
                {{"trackId":2,"trackName":"Avec flux","artistName":"A","feedUrl":"{racine}/flux/a.xml"}},
                {{"trackId":3,"trackName":"Exclusivité","artistName":"Apple Seulement"}}]}}"#
                ),
            );
        }
        if ligne.contains("/gp/search.json?q=Les%20pieds") {
            return (
                200,
                format!(
                    r#"[{{"title":"Les Pieds sur terre","author":"France Culture (podcast@radiofrance.com)","url":"{racine}/flux/pst.xml"}}]"#
                ),
            );
        }
        if ligne.contains("/gp/") {
            return (200, "[]".into());
        }
        if ligne.contains("/fy/") {
            return (200, r#"{"data":[]}"#.into());
        }
        if ligne.contains("/flux/pst.xml") {
            return (200, flux_rss("Les pieds sur terre", "France Culture"));
        }
        (404, "{}".into())
    }
    let (racine, recues) = faux_serveur(r).await;
    let svc = crate::streaming::podcasts::PodcastService::with_bases_de_banc(
        crate::http::client::shared().clone(),
        &format!("{racine}/itunes/search"),
        annuaires(&racine),
    );
    // Pays propre au banc : le cache de recherche ne se mélange pas.
    let res = svc.search("banc7152", 20, "zy", None).await.unwrap();
    let vus: Vec<(&str, &str)> = res
        .iter()
        .map(|p| (p.name.as_str(), p.feed_url.as_str()))
        .collect();
    assert_eq!(
        vus,
        [
            (
                "Les pieds sur terre",
                format!("{racine}/flux/pst.xml").as_str()
            ),
            ("Avec flux", format!("{racine}/flux/a.xml").as_str()),
        ]
    );
    let apple = recues.lock().unwrap()[0].clone();
    assert!(
        apple.lines().next().unwrap_or("").contains("country=zy"),
        "le pays transmis à la route part chez Apple : {apple}"
    );
}
