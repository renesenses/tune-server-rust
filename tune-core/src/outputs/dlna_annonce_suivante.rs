//! #3967 — l'appareil ANNONCE-t-il `SetNextAVTransportURI` ?
//!
//! L'action est optionnelle dans AVTransport:1. Le seul endroit où un
//! renderer dit s'il la propose est son descriptif de service (SCPD), lu
//! UNE fois par sortie — jamais sur le chemin chaud.
//!
//! Jusqu'ici Tune posait la suivante à tout renderer DLNA. Celui qui ne
//! l'annonce pas mais l'acquitte quand même (l'acquittement ne prouve que la
//! bonne forme de la requête) faisait ATTENDRE la fin de piste : le sondeur
//! guettait une transition qui ne pouvait pas venir avant de relancer. Ne
//! pas l'armer du tout, c'est lui rendre l'enchaînement d'un renderer qui
//! refuse : immédiat.

use super::traits::AnnonceSuivante;

/// Ce que dit un SCPD d'AVTransport de l'action `SetNextAVTransportURI`.
///
/// [`AnnonceSuivante::NonAnnoncee`] n'est rendu que si le document est bien
/// une liste d'actions (`<actionList>`) : une page d'erreur HTML, un corps
/// vide ou un document tronqué valent [`AnnonceSuivante::Inconnue`], donc
/// l'armement d'avant.
pub fn depuis_scpd(xml: &str) -> AnnonceSuivante {
    let bas = xml.to_ascii_lowercase();
    // Préfixe d'espace de noms toléré (`<scpd:actionList>`) : on cherche la
    // fin du nom de balise, pas son début.
    if !bas.contains("actionlist>") {
        return AnnonceSuivante::Inconnue;
    }
    if noms_d_action(xml).any(|n| n.eq_ignore_ascii_case("SetNextAVTransportURI")) {
        AnnonceSuivante::Annoncee
    } else {
        AnnonceSuivante::NonAnnoncee
    }
}

/// Le texte de chaque balise `<name>` (avec ou sans préfixe) du document.
/// Les variables d'état ont aussi un `<name>` ; aucune ne s'appelle
/// `SetNextAVTransportURI` (la variable s'appelle `NextAVTransportURI`).
fn noms_d_action(xml: &str) -> impl Iterator<Item = &str> {
    let mut reste = xml;
    std::iter::from_fn(move || {
        loop {
            let ouvre = reste.find('<')?;
            let apres = &reste[ouvre + 1..];
            let fin_balise = apres.find('>')?;
            let balise = &apres[..fin_balise];
            reste = &apres[fin_balise + 1..];
            let nom = balise.rsplit(':').next().unwrap_or(balise).trim();
            if !nom.eq_ignore_ascii_case("name") {
                continue;
            }
            let fin_texte = reste.find('<')?;
            let texte = reste[..fin_texte].trim();
            reste = &reste[fin_texte..];
            return Some(texte);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un SCPD AVTransport:1 réduit aux actions qui comptent ici.
    fn scpd(actions: &[&str]) -> String {
        let liste: String = actions
            .iter()
            .map(|a| format!("<action><name>{a}</name><argumentList></argumentList></action>"))
            .collect();
        format!(
            "<?xml version=\"1.0\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\
             <actionList>{liste}</actionList>\
             <serviceStateTable><stateVariable sendEvents=\"no\"><name>NextAVTransportURI</name>\
             <dataType>string</dataType></stateVariable></serviceStateTable></scpd>"
        )
    }

    #[test]
    fn l_action_listee_est_annoncee() {
        let xml = scpd(&["SetAVTransportURI", "SetNextAVTransportURI", "Play", "Stop"]);
        assert_eq!(depuis_scpd(&xml), AnnonceSuivante::Annoncee);
    }

    /// La contre-épreuve : même document, l'action en moins. La variable
    /// d'état `NextAVTransportURI` reste là et ne doit pas tromper.
    #[test]
    fn l_action_absente_n_est_pas_annoncee() {
        let xml = scpd(&["SetAVTransportURI", "Play", "Stop"]);
        assert!(xml.contains("<name>NextAVTransportURI</name>"));
        assert_eq!(depuis_scpd(&xml), AnnonceSuivante::NonAnnoncee);
    }

    #[test]
    fn espaces_casse_et_prefixe_sont_toleres() {
        let xml = "<s:scpd><s:actionList><s:action><s:name>\n  setnextavtransporturi \n</s:name>\
                   </s:action></s:actionList></s:scpd>";
        assert_eq!(depuis_scpd(xml), AnnonceSuivante::Annoncee);
    }

    /// Rien de ce qui n'est pas une liste d'actions ne vaut « non annoncée » :
    /// c'est la seule réponse qui change une conduite.
    #[test]
    fn un_document_qui_n_est_pas_un_scpd_ne_conclut_rien() {
        for corps in [
            "",
            "<html><body>404 Not Found</body></html>",
            "<?xml version=\"1.0\"?><root><device/></root>",
        ] {
            assert_eq!(depuis_scpd(corps), AnnonceSuivante::Inconnue, "{corps:?}");
        }
    }
}

/// Le vrai `DlnaOutput` contre un vrai serveur HTTP : le SCPD est lu une
/// fois, retenu quand il conclut, relu quand il est illisible.
#[cfg(test)]
mod lecture_par_la_sortie {
    use super::super::dlna::DlnaOutput;
    use super::super::traits::{AnnonceSuivante, OutputTarget};
    use axum::{Router, routing::get};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn sortie(corps: Option<&'static str>) -> (DlnaOutput, Arc<AtomicUsize>) {
        let lectures = Arc::new(AtomicUsize::new(0));
        let compte = lectures.clone();
        let app = Router::new().route(
            "/avt/scpd.xml",
            get(move || {
                let compte = compte.clone();
                async move {
                    compte.fetch_add(1, Ordering::Relaxed);
                    match corps {
                        Some(c) => (axum::http::StatusCode::OK, c),
                        None => (axum::http::StatusCode::NOT_FOUND, "<html>404</html>"),
                    }
                }
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hote = format!("http://{}", ecoute.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let output = DlnaOutput::new(
            "Renderer factice".into(),
            "uuid:3967-scpd".into(),
            hote.clone(),
            format!("{hote}/control"),
            format!("{hote}/control"),
            None,
        )
        .with_av_transport_scpd(Some(format!("{hote}/avt/scpd.xml")));
        (output, lectures)
    }

    const AVEC: &str = "<scpd><actionList><action><name>SetAVTransportURI</name></action>\
                        <action><name>SetNextAVTransportURI</name></action></actionList></scpd>";
    const SANS: &str =
        "<scpd><actionList><action><name>SetAVTransportURI</name></action></actionList></scpd>";

    #[tokio::test]
    async fn annoncee_lue_une_seule_fois() {
        let (output, lectures) = sortie(Some(AVEC)).await;
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::Annoncee
        );
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::Annoncee
        );
        assert_eq!(
            lectures.load(Ordering::Relaxed),
            1,
            "le SCPD est lu une fois"
        );
    }

    #[tokio::test]
    async fn non_annoncee_retenue() {
        let (output, lectures) = sortie(Some(SANS)).await;
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::NonAnnoncee
        );
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::NonAnnoncee
        );
        assert_eq!(lectures.load(Ordering::Relaxed), 1);
    }

    /// Un SCPD injoignable ne fige rien : `Inconnue`, et la lecture suivante
    /// retente.
    #[tokio::test]
    async fn illisible_inconnue_et_relue() {
        let (output, lectures) = sortie(None).await;
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::Inconnue
        );
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::Inconnue
        );
        assert_eq!(lectures.load(Ordering::Relaxed), 2);
    }

    /// Sans URL de SCPD (sortie construite à la main), rien n'est lu.
    #[tokio::test]
    async fn sans_scpd_inconnue() {
        let output = DlnaOutput::new(
            "Renderer".into(),
            "uuid:3967-sans".into(),
            "http://127.0.0.1:9".into(),
            "http://127.0.0.1:9/control".into(),
            "http://127.0.0.1:9/control".into(),
            None,
        );
        assert_eq!(
            output.annonce_la_suivante().await,
            AnnonceSuivante::Inconnue
        );
    }
}
