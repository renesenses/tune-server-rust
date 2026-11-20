//! #8171 — que faire quand le périphérique d'une zone locale est INTROUVABLE.
//!
//! Le constat (fil 2708, Windows 10 Pro, rc10) : la zone « Denafrips » est
//! sélectionnée, la lecture démarre, et le son sort des haut-parleurs du PC.
//! Le chemin cpal **partagé** (`find_device_with_fallback`,
//! `outputs/local/resolution.rs`) ne retrouvait plus le DAC — nom changé par
//! le pilote sans identifiant d'endpoint mémorisé, rang `(n)` renuméroté après
//! un rebranchement, DAC éteint — et ouvrait alors la sortie par défaut du
//! système. Les deux chemins exclusifs Windows (WASAPI exclusif, ASIO)
//! refusaient déjà dans ce cas ; seul le mode partagé jouait ailleurs.
//!
//! Sous Windows la conduite est désormais le REFUS : rien n'est ouvert, la
//! lecture s'arrête et l'écran reçoit [`message_peripherique_introuvable`] par
//! le canal d'échec de la sortie (`take_output_failure`, `fatal: true`).
//! Ailleurs (macOS, Linux) le repli sur la sortie système, dit à l'auditeur
//! depuis #2207 et #2269, est inchangé : ce ticket ne porte que sur Windows.
//!
//! Hors de `local-audio` à dessein, comme `pseudo_peripherique_alsa` : la porte
//! `test` de la CI ne compile pas cette feature pour `tune-core`, et cette
//! décision-ci doit pouvoir y être jugée. La plateforme est un PARAMÈTRE, pas
//! un `cfg!` : la décision Windows se juge ainsi depuis Linux (#1837, #2056).

/// La conduite du chemin partagé quand la zone nomme un périphérique précis
/// (autre que `default`) qu'aucune sortie énumérée ne porte plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConduiteSiIntrouvable {
    /// Ne rien ouvrir, arrêter la lecture et dire pourquoi.
    Refuser,
    /// Ouvrir la sortie par défaut du système, en le disant (#2207).
    OuvrirLaSortieSysteme,
}

/// La conduite pour un système donné (`std::env::consts::OS`).
pub fn conduite_si_introuvable(target_os: &str) -> ConduiteSiIntrouvable {
    match target_os {
        // #8171 : sous Windows, la sortie par défaut est presque toujours les
        // haut-parleurs du PC — jamais ce qu'une zone « DAC » a demandé.
        "windows" => ConduiteSiIntrouvable::Refuser,
        _ => ConduiteSiIntrouvable::OuvrirLaSortieSysteme,
    }
}

/// Le message montré à l'auditeur quand la lecture est refusée faute de
/// périphérique : il nomme la sortie demandée, dit que rien n'est parti
/// ailleurs, et donne le geste.
///
/// `flux` précise le chemin (« le flux PCM », « le flux décodé »), pour que le
/// message garde ce que le journal du testeur permet de rapprocher.
pub fn message_peripherique_introuvable(demande: &str, flux: &str) -> String {
    format!(
        "Sortie « {demande} » : ce périphérique est introuvable (éteint, débranché, \
         renommé ou renuméroté par le système) ; {flux} n'a été envoyé vers aucune \
         autre sortie. Rallumez ou rebranchez l'appareil, ou choisissez à nouveau \
         la sortie de cette zone"
    )
}

/// #8171 (retour rc12, fil 2708) — le NOM de la zone désigne-t-il désormais
/// un AUTRE endpoint WASAPI que le sien ?
///
/// Le cas : Windows nomme « Haut-parleurs » aussi bien la carte Realtek que
/// le DAC USB sous son pilote générique. La zone du DAC a été créée DAC
/// allumé, sous « Haut-parleurs », et connaît l'identifiant de son endpoint.
/// DAC éteint, cet identifiant manque à l'énumération ; l'appariement par
/// identifiant échoue, et celui par NOM retrouve… les haut-parleurs du PC,
/// seuls à porter encore ce nom. La zone du DAC jouait donc sur les
/// haut-parleurs, bien que la rc12 ait supprimé le repli sur la sortie par
/// défaut : ce n'était pas un repli, c'était un homonyme.
///
/// Un identifiant WASAPI (`wasapi:{0.0.0.00000000}.{guid}`) ne désigne
/// jamais un autre endpoint (voir `outputs::identite_de_sortie`) : un nom qui
/// mène à un AUTRE identifiant, pendant que celui de la zone est absent, mène
/// à un autre appareil. La règle ne mord que dans ce cas précis :
///
/// - la zone connaît un identifiant WASAPI, absent de l'énumération ;
/// - la candidate retenue par le nom porte un identifiant WASAPI différent.
///
/// Identifiant inconnu, d'une autre famille (ALSA, ASIO, CoreAudio) ou
/// présent : `false`, la résolution d'avant est inchangée.
pub fn le_nom_designe_un_autre_endpoint<'a>(
    endpoint_de_la_zone: Option<&str>,
    endpoint_retenu_par_le_nom: &str,
    endpoints_presents: impl IntoIterator<Item = &'a str>,
) -> bool {
    let est_wasapi = |id: &str| id.trim().to_ascii_lowercase().starts_with("wasapi:");
    let Some(zone) = endpoint_de_la_zone
        .map(str::trim)
        .filter(|id| !id.is_empty() && est_wasapi(id))
    else {
        return false;
    };
    let retenu = endpoint_retenu_par_le_nom.trim();
    if retenu.is_empty() || !est_wasapi(retenu) || retenu.eq_ignore_ascii_case(zone) {
        return false;
    }
    !endpoints_presents
        .into_iter()
        .any(|present| present.trim().eq_ignore_ascii_case(zone))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LE témoin de #8171 : sous Windows, un DAC introuvable n'envoie pas la
    /// lecture sur les haut-parleurs du PC.
    #[test]
    fn sous_windows_un_peripherique_introuvable_est_refuse_et_non_remplace() {
        assert_eq!(
            conduite_si_introuvable("windows"),
            ConduiteSiIntrouvable::Refuser,
            "#8171 : sous Windows, une zone dont le DAC est introuvable retombe sur \
             la sortie par défaut — le son part sur les haut-parleurs du PC au lieu \
             d'être refusé"
        );
    }

    /// Le reste du parc garde le repli dit de #2207 : ce ticket ne le change pas.
    #[test]
    fn hors_windows_le_repli_dit_sur_la_sortie_systeme_est_conserve() {
        for os in ["macos", "linux", "freebsd", ""] {
            assert_eq!(
                conduite_si_introuvable(os),
                ConduiteSiIntrouvable::OuvrirLaSortieSysteme,
                "{os:?} : la conduite hors Windows ne devait pas changer"
            );
        }
    }

    #[test]
    fn le_message_nomme_la_sortie_demandee_et_dit_que_rien_n_est_parti_ailleurs() {
        let m = message_peripherique_introuvable("Denafrips", "le flux PCM");
        assert!(m.contains("« Denafrips »"), "{m}");
        assert!(m.contains("le flux PCM"), "{m}");
        assert!(m.contains("aucune autre sortie"), "{m}");
        assert!(m.contains("choisissez à nouveau la sortie"), "{m}");
    }

    /// GARDE DE SITE — la décision doit être CONSULTÉE par le chemin partagé,
    /// avant toute ouverture de la sortie par défaut, et le refus doit couper.
    ///
    /// `outputs/local/resolution.rs` vit derrière `local-audio`, que la porte
    /// `test` de la CI ne compile pas : on lit son texte, qui échappe aux `cfg`.
    #[test]
    fn find_device_with_fallback_consulte_la_conduite_avant_d_ouvrir_le_defaut() {
        let source = include_str!("local/resolution.rs");
        let debut = source
            .find("pub(super) fn find_device_with_fallback(")
            .expect("find_device_with_fallback introuvable");
        let fin = source[debut..]
            .find("\n/// Probe a device's capabilities")
            .map(|i| debut + i)
            .expect("le corps de find_device_with_fallback doit précéder `Probe a device`");
        let corps = &source[debut..fin];

        // Après l'appariement, la branche « introuvable » : c'est elle qui
        // ouvrait la sortie par défaut.
        let introuvable = corps
            .find("if let DeviceResolution::Matched(matched) = resolution")
            .map(|i| &corps[i..])
            .expect("la branche d'appariement doit rester dans cette fonction");
        let consultation = introuvable
            .find("conduite_si_introuvable(std::env::consts::OS)")
            .expect(
                "#8171 : la branche « introuvable » ne consulte plus \
                 conduite_si_introuvable(std::env::consts::OS) — sous Windows le son \
                 repartirait sur la sortie par défaut",
            );
        let defaut = introuvable
            .find("host.default_output_device()")
            .expect("le repli hors Windows doit rester");
        assert!(
            consultation < defaut,
            "#8171 : la sortie par défaut est ouverte AVANT de savoir s'il faut refuser"
        );
        let refus = &introuvable[consultation..defaut];
        assert!(
            refus.contains("ConduiteSiIntrouvable::Refuser") && refus.contains("return None;"),
            "#8171 : le refus doit rendre None avant tout repli ; bloc lu :\n{refus}"
        );
        assert!(
            refus.contains("audio_device_not_found_refused"),
            "#8171 : le refus doit être journalisé, avec la liste des périphériques vus"
        );
    }

    const DAC: &str = "wasapi:{0.0.0.00000000}.{dac-denafrips}";
    const REALTEK: &str = "wasapi:{0.0.0.00000000}.{realtek}";

    /// LE témoin du retour rc12 : DAC éteint, sa zone « Haut-parleurs »
    /// connaît l'endpoint du DAC, et seul le Realtek porte encore ce nom.
    #[test]
    fn dac_eteint_le_nom_ne_mene_plus_qu_aux_haut_parleurs_du_pc_8171() {
        assert!(le_nom_designe_un_autre_endpoint(
            Some(DAC),
            REALTEK,
            [REALTEK]
        ));
    }

    /// CONTRE-ÉPREUVES : la résolution d'avant, inchangée.
    #[test]
    fn hors_du_cas_precis_la_resolution_par_nom_est_inchangee_8171() {
        // DAC allumé : son identifiant est présent (l'appariement par
        // identifiant a d'ailleurs déjà gagné).
        assert!(!le_nom_designe_un_autre_endpoint(
            Some(DAC),
            REALTEK,
            [REALTEK, DAC]
        ));
        // Même endpoint des deux côtés.
        assert!(!le_nom_designe_un_autre_endpoint(Some(DAC), DAC, [REALTEK]));
        // La zone ne connaît pas d'identifiant (zone d'avant #2269).
        assert!(!le_nom_designe_un_autre_endpoint(None, REALTEK, [REALTEK]));
        assert!(!le_nom_designe_un_autre_endpoint(
            Some("  "),
            REALTEK,
            [REALTEK]
        ));
        // Familles dont l'identifiant n'identifie pas l'appareil.
        assert!(!le_nom_designe_un_autre_endpoint(
            Some("alsa:hw:CARD=2,DEV=0"),
            "alsa:hw:CARD=3,DEV=0",
            ["alsa:hw:CARD=3,DEV=0"]
        ));
        assert!(!le_nom_designe_un_autre_endpoint(
            Some("asio:Combo384 ASIO 1.03"),
            "asio:Realtek ASIO",
            ["asio:Realtek ASIO"]
        ));
        // Candidate sans identifiant : on ne conclut rien.
        assert!(!le_nom_designe_un_autre_endpoint(Some(DAC), "", [""]));
    }

    /// GARDE DE SITE — la règle est consultée par l'ouverture partagée ET par
    /// la disponibilité (#8173 : une seule règle pour les deux), sous la
    /// conduite Windows, et seulement pour un appariement qui n'est pas celui
    /// de l'identifiant.
    #[test]
    fn ouverture_et_disponibilite_consultent_la_regle_de_l_homonyme_8171() {
        let source = include_str!("local/resolution.rs");
        let corps_de = |fonction: &str| -> &str {
            let debut = source.find(fonction).expect(fonction);
            let corps = &source[debut..];
            &corps[..corps.find("\n}\n").expect("fin de fonction")]
        };
        for fonction in [
            "pub(super) fn find_device_with_fallback(",
            "pub(super) fn zone_locale_disponible(",
        ] {
            assert!(
                corps_de(fonction).contains("homonyme_d_un_autre_endpoint("),
                "{fonction} ne consulte pas la règle de l'homonyme"
            );
        }
        let regle = corps_de("fn homonyme_d_un_autre_endpoint(");
        assert!(
            regle.contains("le_nom_designe_un_autre_endpoint("),
            "{regle}"
        );
        assert!(
            regle.contains("DeviceMatch::ByEndpointId"),
            "l'appariement par identifiant doit rester souverain"
        );
        assert!(
            regle.contains("conduite_si_introuvable(std::env::consts::OS)"),
            "la règle ne vaut que sous la conduite Windows (refus)"
        );
    }
}
