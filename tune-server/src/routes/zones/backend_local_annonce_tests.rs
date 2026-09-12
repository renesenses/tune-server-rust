use super::local_backend_status_value;

/// La famille des types de sortie, mutée en entier : seule une zone locale
/// porte le champ. Annoncer un repli ASIO sur un renderer DLNA serait
/// l'annonce fantôme que #2053 et #1315 ont déjà coûtée.
#[test]
fn seule_une_zone_locale_porte_le_statut() {
    // Une zone sans `output_type` est locale — même convention que
    // `build_signal_path`. Sans sortie locale compilée il n'y a AUCUN
    // backend à décrire, et le champ doit rester absent partout : c'est la
    // moitié du contrat qui vaut dans les deux constructions.
    #[cfg(feature = "local-audio")]
    for local in [None, Some("local")] {
        assert!(
            local_backend_status_value(local, "asio").is_some(),
            "zone locale ({local:?}) : statut absent"
        );
    }
    #[cfg(not(feature = "local-audio"))]
    for local in [None, Some("local")] {
        assert!(
            local_backend_status_value(local, "asio").is_none(),
            "zone locale ({local:?}) : statut annoncé sans sortie locale compilée"
        );
    }
    for distant in [
        "dlna",
        "chromecast",
        "bluos",
        "airplay",
        "browser",
        "oaat",
        "squeezebox",
    ] {
        assert!(
            local_backend_status_value(Some(distant), "asio").is_none(),
            "zone « {distant} » : statut de backend LOCAL annoncé à tort"
        );
    }
}

/// Le contrat de la charge utile : les cinq champs, nommés, pour que le
/// client puisse dire « vous avez demandé X, Y tourne, parce que Z ».
#[cfg(feature = "local-audio")]
#[test]
fn le_statut_porte_le_demande_a_cote_de_lactif() {
    let v = local_backend_status_value(Some("local"), "ASIO").expect("zone locale");
    for champ in [
        "active",
        "requested",
        "fell_back",
        "fallback_reason",
        "fallback_detail",
        // #2207 — le PÉRIPHÉRIQUE réellement ouvert, face au demandé. Le
        // champ fait partie du contrat même quand rien n'a encore joué :
        // il vaut alors `null`, ce qui est la réponse honnête. C'est son
        // ABSENCE de la charge utile qui serait la régression — le client
        // n'aurait de nouveau que le journal pour savoir où sort le son.
        "device",
        // #3233 — la CADENCE réellement ouverte, face à celle de la
        // source. Même raison que `device` : quand Tune refuse la cadence
        // de la source parce que les capacités du périphérique sont
        // SUPPOSÉES et non mesurées, il convertit — et sans ce champ le
        // client affiche « DSD64 » pendant qu'autre chose part au DAC.
        // `null` tant que rien n'a joué en partagé, ce qui est honnête.
        "rate",
    ] {
        assert!(v.get(champ).is_some(), "champ « {champ} » absent de {v}");
    }
    assert_eq!(
        v["requested"], "asio",
        "le demandé doit être rendu normalisé, pas déduit"
    );
    assert!(
        v["active"].as_str().is_some_and(|s| !s.is_empty()),
        "l'actif doit être nommé : {v}"
    );
}

/// Le VERROU de branchement : la fonction peut être parfaite et n'être
/// appelée nulle part. Les quatre charges utiles qui portent une zone
/// doivent toutes s'en servir — c'est la leçon de #1864, où quinze
/// prédicats sur dix-sept n'étaient jamais construits pendant leur test.
#[test]
fn les_quatre_charges_utiles_de_zone_appellent_le_contrat() {
    // Source normalisée : on retire tous les blancs, pour que le test
    // survive à un passage de rustfmt qui recasserait les lignes.
    fn sans_blancs(fichier: &str) -> String {
        std::fs::read_to_string(std::path::Path::new(fichier))
            .unwrap_or_else(|e| panic!("{fichier} doit être lisible : {e}"))
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    // Les QUATRE sites, un par charge utile qui décrit une zone. Chacun
    // est nommé par l'appel exact qu'il doit contenir.
    for (fichier, appel, quoi) in [
        (
            // `list_zones` vit dans le module enfant `lecture` depuis REF-4 (#2219).
            "src/routes/zones/lecture.rs",
            "local_backend_status_value(z.output_type.as_deref(),&audio_backend_pref",
            "GET /zones",
        ),
        (
            "src/routes/zones/lecture.rs",
            "local_backend_status_value(zone.output_type.as_deref(),&audio_backend_pref",
            "GET /zones/{id}",
        ),
        (
            "src/routes/ws.rs",
            "local_backend_status_value(z.output_type.as_deref(),&audio_backend_pref",
            "instantané WebSocket",
        ),
        (
            "src/routes/playback.rs",
            "local_backend_status_value(zone.output_type.as_deref(),&audio_backend_pref",
            "play / next / previous / resume",
        ),
    ] {
        let src = sans_blancs(fichier);
        assert!(
            src.contains(appel),
            "{quoi} ({fichier}) n'appelle plus local_backend_status_value — \
             la zone repart sans dire quel backend tourne vraiment"
        );
        assert!(
            src.contains("\"audio_backend_status\""),
            "{quoi} ({fichier}) : le champ audio_backend_status a disparu de la charge utile"
        );
    }
}
