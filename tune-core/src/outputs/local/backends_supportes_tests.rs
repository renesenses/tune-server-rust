use super::{backend_value_is_supported, supported_backends};

// #1268 — le cas Lapinou/Benjithom : sur Debian et Fedora, le sélecteur
// proposait WASAPI et ASIO. La liste que le serveur publie ne doit JAMAIS
// contenir un backend d'une autre plateforme.
#[test]
fn aucun_backend_windows_hors_windows() {
    #[cfg(not(target_os = "windows"))]
    {
        let interdits = ["wasapi", "asio"];
        for b in supported_backends() {
            assert!(
                !interdits.contains(&b.value),
                "backend Windows « {} » proposé sur une plateforme non-Windows",
                b.value
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        assert!(
            supported_backends().iter().any(|b| b.value == "wasapi"),
            "WASAPI doit rester proposé sous Windows"
        );
    }
}

// `auto` est le défaut ET le repli de select_host : toujours présent,
// toujours premier, sur toutes les plateformes.
#[test]
fn auto_toujours_present_et_premier() {
    let backends = supported_backends();
    assert!(!backends.is_empty());
    assert_eq!(backends[0].value, "auto");
    assert!(backend_value_is_supported("auto"));
    assert!(backend_value_is_supported("AUTO"), "casse indifférente");
}

// ASIO n'apparaît que si le binaire sait réellement l'ouvrir — même
// vérité que `asio_available()`, qui voyage déjà dans la même réponse.
#[test]
fn asio_propose_ssi_disponible() {
    assert_eq!(
        supported_backends().iter().any(|b| b.value == "asio"),
        super::asio_available()
    );
}

// Le repli d'affichage : une valeur Windows persistée sur un serveur
// Linux/macOS est déclarée non supportée, pour que /system/config la
// ramène à `auto` au lieu de la resservir au sélecteur.
#[test]
fn une_valeur_d_une_autre_plateforme_est_declaree_non_supportee() {
    #[cfg(not(target_os = "windows"))]
    {
        assert!(!backend_value_is_supported("wasapi"));
        assert!(!backend_value_is_supported("asio"));
    }
    assert!(!backend_value_is_supported("n_importe_quoi"));
}
