/// ⚠️ `include_str!` rend le fichier ENTIER. On coupe à ce module pour que
/// les motifs cherchés ne puissent pas se trouver eux-mêmes dans les
/// messages d'assertion ci-dessous (#2082).
fn code_de_production() -> &'static str {
    static PRODUCTION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PRODUCTION.get_or_init(|| {
        const TOUT: &str = include_str!("../poller.rs");
        const BORNE: &str = "mod canal_radio_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
        // `tick` vit dans son propre module (REF-1, #2219) et se lit en
        // premier : il précédait le reste de l'impl dans le fichier d'origine.
        format!("{}{}", include_str!("../poller/tick.rs"), &TOUT[..fin])
    })
}

fn position(motif: &str) -> usize {
    code_de_production().find(motif).unwrap_or_else(|| {
        panic!(
            "motif introuvable dans poller.rs : « {motif} ».\n\
             Le code a été remanié ; ce garde-fou ne garde plus rien tant \
             qu'il n'a pas suivi. Voir #2991."
        )
    })
}

/// Le verdict doit être PRIS sur le `stream_id` qui sert à publier, et
/// JOURNALISÉ, avant que le now-playing ne parte vers l'interface. Sans
/// cette ligne, la garde `if let Some(sid)` redevient muette et « dans Tune
/// ça fonctionne, sur le lecteur réseau non » redevient indiagnosticable.
#[test]
fn le_changement_de_titre_journalise_par_ou_il_passe() {
    let verdict =
        position("let canal = crate::http::streamer::canal_radio(np.stream_id.as_deref());");
    let publication = position("crate::http::streamer::publish_radio_now(");
    let ligne = position("radio_refresh_channel — le morceau a changé");
    let vers_l_interface = position("self.playback.update_now_playing(zone_id, new_np).await;");
    assert!(
        verdict < publication && publication < ligne && ligne < vers_l_interface,
        "le canal doit être établi puis journalisé DANS la branche du \
         changement de titre, avant que le now-playing ne parte à \
         l'interface (#2991)."
    );
}

/// La reprise « le renderer joue, Tune ne le croyait pas » doit relire
/// l'identifiant de session dans l'URI annoncée par l'appareil. Remise à
/// `None`, elle rend le défaut PERMANENT pour toute la session radio :
/// `refresh_radio_metadata` recopie ce `None` dans chaque now-playing
/// suivant.
#[test]
fn la_reprise_depuis_lappareil_ne_perd_plus_lidentifiant_de_session() {
    let relecture = position("decisions::stream_id_de_l_uri(status.current_uri.as_deref())");
    let pose = position("stream_id: stream_id_repris,");
    let annonce = position("\"playback_recovered_from_device\"");
    assert!(
        relecture < pose && pose < annonce,
        "la reprise depuis l'appareil doit reposer l'identifiant relu dans \
         l'URI, et non `None` (#2991)."
    );
    assert!(
        !code_de_production().contains("stream_id: None,\n                            ..Default::default()\n                        };\n                        self.playback.play(zone_id, np).await;"),
        "le `stream_id: None` en dur est revenu dans la reprise depuis \
         l'appareil (#2991)."
    );
}
