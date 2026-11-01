//! La vue mini servie par le client web (maquette A « télécommande », validée
//! par Bertrand le 31/10/2026), et ce que la fenêtre a le droit de charger.
//!
//! Depuis la v0.9.64, le widget chargeait `/?mini=1` ; la disposition que ce
//! paramètre désignait est partie avec l'ancienne interface du client web
//! (19/09/2026). La fenêtre de 380 px recevait donc l'interface COMPLÈTE.
//! Le client web sert désormais `?vue=mini` (et relit `?mini=1`), et l'annonce
//! par un fichier statique, `vue-mini.json` : c'est lui qu'on sonde, pas un
//! numéro de version, qui ne dit rien de l'interface qu'un serveur embarque.
//!
//! Les fonctions de ce module sont pures : elles décident, `main.rs` exécute.

use tauri::Url;

/// Taille de la fenêtre mini, dépliée (maquette A) et repliée (maquette C).
pub const MINI_DEPLIE: (f64, f64) = (360.0, 560.0);
pub const MINI_REPLIE: (f64, f64) = (360.0, 140.0);
/// La fenêtre mini se redimensionne, mais pas au-delà d'un téléphone posé en
/// largeur : au-dessus, c'est la fenêtre complète qu'il faut.
pub const MINI_LARGEUR_MAX: f64 = 760.0;
/// Hauteur maximale « sans borne » : `set_max_size` borne les deux axes.
pub const MINI_HAUTEUR_MAX: f64 = 4000.0;
pub const MINI_MIN: (f64, f64) = (320.0, 140.0);

/// L'adresse de la vue mini sur un serveur. `hote=widget` fait apparaître les
/// boutons natifs (épingle, replier, fenêtre complète) dans la page.
pub fn adresse_mini(serveur: &str) -> String {
    format!("{}/?vue=mini&hote=widget", serveur.trim_end_matches('/'))
}

/// Le client web servi par ce serveur sait-il afficher la vue mini ?
///
/// `vue-mini.json` vaut `{"vue_mini":1,...}`. Un serveur plus ancien répond
/// 404, ou renvoie `index.html` pour toute adresse inconnue : ni l'un ni
/// l'autre n'est ce JSON, et la réponse est alors `false` — on garde
/// l'interface embarquée, qui marche.
pub fn annonce_vue_mini(corps: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(corps)
        .ok()
        .and_then(|v| v.get("vue_mini").and_then(serde_json::Value::as_u64))
        .is_some_and(|n| n >= 1)
}

/// Les gestes que la page demande à la fenêtre (`/__widget/<action>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionWidget {
    Epingle,
    Replier,
    Deplier,
    Complet,
}

/// Ce que la fenêtre fait d'une navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Le serveur configuré : on charge.
    Autoriser,
    /// Un geste natif demandé par la page : on l'exécute, on ne charge rien.
    Action(ActionWidget),
    /// Un lien http(s) vers ailleurs (fiche Qobuz, Wikipédia…) : il s'ouvre
    /// dans le navigateur du système, jamais dans la fenêtre du widget.
    Externe,
    /// Tout le reste : refusé.
    Refuser,
}

const PREFIXE_ACTION: &str = "/__widget/";

fn meme_origine(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// La fenêtre qui charge le serveur ne charge QUE le serveur.
///
/// `serveur` est l'adresse configurée (`http://hote:8888`). Une adresse
/// illisible refuse tout : mieux vaut une fenêtre vide qu'une fenêtre ouverte
/// sur n'importe quoi.
pub fn decider_navigation(serveur: &str, cible: &Url) -> Decision {
    let Ok(base) = Url::parse(serveur) else {
        return Decision::Refuser;
    };
    match cible.scheme() {
        "http" | "https" => {}
        // La page blanche que le moteur pose avant le premier chargement.
        "about" if cible.as_str() == "about:blank" => return Decision::Autoriser,
        _ => return Decision::Refuser,
    }
    if !meme_origine(&base, cible) {
        return Decision::Externe;
    }
    match cible.path().strip_prefix(PREFIXE_ACTION) {
        None => Decision::Autoriser,
        Some("epingle") => Decision::Action(ActionWidget::Epingle),
        Some("replier") => Decision::Action(ActionWidget::Replier),
        Some("deplier") => Decision::Action(ActionWidget::Deplier),
        Some("complet") => Decision::Action(ActionWidget::Complet),
        // Une action inconnue n'est pas une page : on ne la charge pas.
        Some(_) => Decision::Refuser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    const SRV: &str = "http://192.168.1.18:8888";

    #[test]
    fn l_adresse_mini_porte_la_vue_et_l_hote() {
        assert_eq!(
            adresse_mini("http://192.168.1.18:8888"),
            "http://192.168.1.18:8888/?vue=mini&hote=widget"
        );
        // Une barre finale dans la configuration ne double pas la barre.
        assert_eq!(
            adresse_mini("http://localhost:8888/"),
            "http://localhost:8888/?vue=mini&hote=widget"
        );
    }

    #[test]
    fn seul_le_json_du_client_web_annonce_la_vue_mini() {
        assert!(annonce_vue_mini(r#"{"vue_mini":1,"hote_widget":1}"#));
        assert!(annonce_vue_mini(r#"{"vue_mini":2}"#));
        // Un serveur ancien : 404 en texte, ou index.html pour toute adresse.
        assert!(!annonce_vue_mini("Not Found"));
        assert!(!annonce_vue_mini("<!doctype html><html></html>"));
        assert!(!annonce_vue_mini(r#"{"vue_mini":0}"#));
        assert!(!annonce_vue_mini(r#"{"version":"1.0.0"}"#));
        assert!(!annonce_vue_mini(""));
    }

    #[test]
    fn le_serveur_configure_se_charge() {
        assert_eq!(
            decider_navigation(SRV, &u("http://192.168.1.18:8888/?vue=mini&hote=widget")),
            Decision::Autoriser
        );
        assert_eq!(
            decider_navigation(SRV, &u("http://192.168.1.18:8888/#library")),
            Decision::Autoriser
        );
        assert_eq!(
            decider_navigation(SRV, &u("about:blank")),
            Decision::Autoriser
        );
    }

    #[test]
    fn les_gestes_natifs_sont_interceptes_jamais_charges() {
        for (chemin, action) in [
            ("epingle", ActionWidget::Epingle),
            ("replier", ActionWidget::Replier),
            ("deplier", ActionWidget::Deplier),
            ("complet", ActionWidget::Complet),
        ] {
            assert_eq!(
                decider_navigation(SRV, &u(&format!("{SRV}/__widget/{chemin}"))),
                Decision::Action(action),
                "{chemin}"
            );
        }
        assert_eq!(
            decider_navigation(SRV, &u(&format!("{SRV}/__widget/inconnue"))),
            Decision::Refuser
        );
    }

    #[test]
    fn rien_d_autre_ne_se_charge_dans_la_fenetre() {
        // Un autre hôte, un autre port, un autre schéma : pas la même origine.
        assert_eq!(
            decider_navigation(SRV, &u("https://www.qobuz.com/album/x")),
            Decision::Externe
        );
        assert_eq!(
            decider_navigation(SRV, &u("http://192.168.1.18:9999/")),
            Decision::Externe
        );
        assert_eq!(
            decider_navigation(SRV, &u("https://192.168.1.18:8888/")),
            Decision::Externe
        );
        // Un geste natif demandé depuis une AUTRE origine n'est pas un geste :
        // seule la page du serveur configuré peut piloter la fenêtre.
        assert_eq!(
            decider_navigation(SRV, &u("http://evil.example/__widget/epingle")),
            Decision::Externe
        );
        // Les schémas qui ne sont pas du web : refusés.
        assert_eq!(
            decider_navigation(SRV, &u("file:///etc/passwd")),
            Decision::Refuser
        );
        assert_eq!(
            decider_navigation(SRV, &u("javascript:alert(1)")),
            Decision::Refuser
        );
        // Adresse configurée illisible : tout est refusé.
        assert_eq!(
            decider_navigation("pas une adresse", &u("http://localhost:8888/")),
            Decision::Refuser
        );
    }

    #[test]
    fn le_port_par_defaut_est_le_meme_port() {
        assert_eq!(
            decider_navigation("http://tune.local", &u("http://tune.local:80/")),
            Decision::Autoriser
        );
    }

    #[test]
    fn la_fenetre_mini_tient_dans_sa_largeur_maximale() {
        let tailles = [MINI_DEPLIE, MINI_REPLIE, MINI_MIN];
        assert!(tailles.iter().all(|(w, _)| *w <= MINI_LARGEUR_MAX));
        assert!(MINI_REPLIE.1 >= MINI_MIN.1);
        assert!(MINI_DEPLIE.1 > MINI_REPLIE.1);
    }
}
