//! Resigner une URL de flux Bandcamp expirée, depuis la page de son album
//! (fil 2121).
//!
//! # Le défaut
//!
//! FabienM, 1.0.0-rc1, zone Cast « Parents », 03/10 : une piste Bandcamp
//! relancée depuis la file portait l'URL bcbits signée le 30/09
//! (`ts=1790782809`). Bandcamp a répondu **410 Gone**, le relais n'avait aucun
//! moyen d'en demander une fraîche, et la Beosound n'a reçu aucun octet.
//!
//! Le relais savait pourtant guérir : Qobuz et Tidal lui attachent un
//! [`ReresolveFn`] (#1136), et `send_with_reresolve` l'appelle sur un statut
//! d'expiration (403, 410…). Il ne manquait à Bandcamp que de quoi resigner :
//! l'adresse de la page album ou piste, désormais rangée avec la piste
//! (`album_ref`, migration 114).
//!
//! # La resignature
//!
//! Relire la page (`get_album_tracks`, le MÊME extracteur que la lecture d'un
//! album) donne les URL de flux du jour, une par piste. On y retrouve la nôtre
//! par son identifiant de piste Bandcamp, le dernier segment du chemin
//! (`…/mp3-128/29192493`) : il ne change pas d'une signature à l'autre, alors
//! que `ts`, `t` et `token` changent à chaque lecture de la page.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::http::streamer::ReresolveFn;
use crate::streaming::StreamTrack;
use crate::streaming::registry::ServiceRegistry;

/// Le nom du service Bandcamp dans le registre.
const SERVICE_BANDCAMP: &str = "bandcamp";

/// Âge au-delà duquel une signature Bandcamp est renouvelée AVANT d'envoyer
/// l'URL à une sortie (décision de Bertrand, 04/10 : toutes les sorties).
///
/// La durée de validité réelle d'une signature bcbits n'est pas publiée, et
/// rien dans ce dépôt ne l'a mesurée. Ce qu'on sait : #4577 a mesuré que
/// Bandcamp resigne `ts`/`t`/`token` à CHAQUE lecture de la page (deux lectures
/// à trois secondes d'écart, deux signatures) ; le journal de FabienM (fil
/// 2121) montre une signature morte en 410 à **2,7 jours**. 24 h est donc un
/// choix PRUDENT : nettement sous le seul échec observé, et assez long pour
/// qu'une piste lue depuis sa page (signature de la minute) ne coûte jamais une
/// relecture de plus. Une signature plus jeune passe telle quelle ; si elle
/// expirait malgré tout, le relais réseau sait encore la resigner sur 403/410.
pub(super) const SEUIL_SIGNATURE_BANDCAMP_SECS: u64 = 24 * 3600;

/// L'horodatage de signature d'une URL de flux Bandcamp : le paramètre `ts`
/// de sa requête, en secondes Unix. `None` s'il n'y en a pas.
pub(super) fn horodatage_de_signature(url: &str) -> Option<u64> {
    let requete = url.split_once('?')?.1;
    let requete = requete.split('#').next().unwrap_or(requete);
    requete
        .split('&')
        .find_map(|p| p.strip_prefix("ts="))
        .and_then(|v| v.parse().ok())
}

/// La signature doit-elle être renouvelée avant l'envoi ?
///
/// Oui quand elle a `SEUIL_SIGNATURE_BANDCAMP_SECS` ou plus, et aussi quand
/// elle n'a PAS d'horodatage : c'est le chemin nu d'un favori (#4577), que
/// Bandcamp refuse en 403. Une signature datée du futur (horloge décalée) passe
/// telle quelle : rien ne prouve qu'elle soit morte.
pub(super) fn signature_a_renouveler(url: &str, maintenant_secs: u64) -> bool {
    match horodatage_de_signature(url) {
        None => true,
        Some(ts) => maintenant_secs.saturating_sub(ts) >= SEUIL_SIGNATURE_BANDCAMP_SECS,
    }
}

/// L'identifiant de piste Bandcamp d'une URL de flux : le dernier segment du
/// chemin, requête retirée. `None` quand il n'y en a pas.
pub(super) fn identifiant_de_piste(url: &str) -> Option<&str> {
    let chemin = url.split(['?', '#']).next().unwrap_or(url);
    let dernier = chemin.rsplit('/').next()?;
    (!dernier.is_empty() && !dernier.contains(':')).then_some(dernier)
}

/// L'URL du jour de la même piste, prise dans les pistes de sa page.
pub(super) fn url_resignee(pistes: &[StreamTrack], ancienne: &str) -> Option<String> {
    let cle = identifiant_de_piste(ancienne)?;
    pistes
        .iter()
        .find(|p| identifiant_de_piste(&p.id) == Some(cle))
        .map(|p| p.id.clone())
}

/// Le mécanisme de nouvelle résolution d'une piste Bandcamp : relire `page`
/// auprès du service Bandcamp du registre, et rendre l'URL fraîche de la piste
/// dont `ancienne` est une signature périmée.
///
/// Ne capture que des clones bon marché ; peut être appelé autant de fois que
/// le relais en a besoin sur la durée de la session.
pub(super) fn reresolveur_bandcamp(
    services: Arc<Mutex<ServiceRegistry>>,
    page: String,
    ancienne: String,
) -> ReresolveFn {
    Arc::new(move || {
        let services = services.clone();
        let page = page.clone();
        let ancienne = ancienne.clone();
        Box::pin(async move {
            let registre = services.lock().await;
            let svc = registre
                .get(SERVICE_BANDCAMP)
                .ok_or_else(|| "service bandcamp absent du registre".to_string())?;
            let svc = svc.read().await;
            let pistes = svc
                .get_album_tracks(&page)
                .await
                .map_err(|e| format!("relecture de la page Bandcamp {page} : {e}"))?;
            url_resignee(&pistes, &ancienne).ok_or_else(|| {
                format!(
                    "piste {} introuvable sur la page Bandcamp {page}",
                    identifiant_de_piste(&ancienne).unwrap_or("?")
                )
            })
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piste(id: &str) -> StreamTrack {
        StreamTrack {
            id: id.into(),
            title: String::new(),
            artist: String::new(),
            album: None,
            album_id: Some("https://artiste.bandcamp.com/album/disque".into()),
            duration_ms: 0,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        }
    }

    const PERIMEE: &str =
        "https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493?p=0&ts=1790782809&t=dcd4&token=x";

    #[test]
    fn l_horodatage_de_signature_est_le_ts_de_la_requete() {
        assert_eq!(horodatage_de_signature(PERIMEE), Some(1_790_782_809));
        assert_eq!(
            horodatage_de_signature("https://t4.bcbits.com/stream/e4/mp3-128/1"),
            None
        );
        assert_eq!(
            horodatage_de_signature("https://t4.bcbits.com/stream/e4/mp3-128/1?p=0&t=1"),
            None
        );
    }

    #[test]
    fn le_seuil_tranche_l_age_de_la_signature() {
        let ts = 1_790_782_809;
        // Le cas de Fabien : 2,7 jours plus tard.
        assert!(signature_a_renouveler(PERIMEE, ts + 233_000));
        assert!(signature_a_renouveler(
            PERIMEE,
            ts + SEUIL_SIGNATURE_BANDCAMP_SECS
        ));
        assert!(!signature_a_renouveler(
            PERIMEE,
            ts + SEUIL_SIGNATURE_BANDCAMP_SECS - 1
        ));
        assert!(
            !signature_a_renouveler(PERIMEE, ts + 60),
            "lue depuis sa page à l'instant"
        );
        assert!(
            !signature_a_renouveler(PERIMEE, ts - 3_600),
            "datée du futur : on ne sait pas"
        );
        assert!(
            signature_a_renouveler("https://t4.bcbits.com/stream/e4/mp3-128/29192493", ts),
            "le chemin nu d'un favori n'a aucune signature"
        );
    }

    #[test]
    fn l_identifiant_de_piste_est_le_dernier_segment_sans_la_requete() {
        assert_eq!(identifiant_de_piste(PERIMEE), Some("29192493"));
        assert_eq!(
            identifiant_de_piste("https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493"),
            Some("29192493")
        );
        assert_eq!(identifiant_de_piste("https://t4.bcbits.com/"), None);
    }

    #[test]
    fn la_piste_est_retrouvee_par_son_identifiant_parmi_celles_de_la_page() {
        let pistes = [
            piste("https://t4.bcbits.com/stream/aaaa/mp3-128/11111111?ts=1791020000&t=1"),
            piste("https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493?ts=1791020000&t=2"),
            piste("https://t4.bcbits.com/stream/bbbb/mp3-128/33333333?ts=1791020000&t=3"),
        ];
        assert_eq!(
            url_resignee(&pistes, PERIMEE).as_deref(),
            Some("https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493?ts=1791020000&t=2")
        );
    }

    #[test]
    fn une_page_qui_n_a_plus_la_piste_ne_rend_rien() {
        let pistes = [piste(
            "https://t4.bcbits.com/stream/aaaa/mp3-128/11111111?ts=1",
        )];
        assert_eq!(url_resignee(&pistes, PERIMEE), None);
    }
}
