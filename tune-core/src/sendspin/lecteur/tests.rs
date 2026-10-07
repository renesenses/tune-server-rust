//! Témoins du rôle `player@v1` côté serveur (#3326, S2-c).
//! Chaque règle citée vient de `Sendspin/spec` 1.0.0-rc1, `roles/player/v1.md`
//! ou `messaging.md`.
use super::*;
use crate::sendspin::messages::ClientHello;

fn hello(support: Value) -> ClientHello {
    serde_json::from_value(json!({
        "name": "Cuisine",
        "supported_roles": ["player@v1", "metadata@v1"],
        "player@v1_support": support,
        "supported_pair_methods": {"pairing_psk": {}},
        "unpaired_access": {"enabled": false},
    }))
    .unwrap()
}

fn support_pcm() -> Value {
    json!({
        "buffer_capacity": 100_000,
        "supported_formats": [
            {"codec": "opus", "sample_rate": 48000, "bit_depth": 16, "channels": 2},
            {"codec": "pcm", "sample_rate": 48000, "bit_depth": 24, "channels": 2},
            {"codec": "pcm", "sample_rate": 44100, "bit_depth": 16, "channels": 2},
        ]
    })
}

fn etat(available: bool, commandes: &[&str]) -> Value {
    json!({
        "available": available,
        "player": {
            "volume": 40, "muted": false,
            "output_delay_ms": 20, "required_lead_time_ms": 300, "min_buffer_ms": 100,
            "supported_commands": commandes,
        }
    })
}

fn types(sorties: &[Sortie]) -> Vec<&'static str> {
    sorties
        .iter()
        .map(|s| match s {
            Sortie::Json { type_message, .. } => *type_message,
            Sortie::Audio { .. } => "audio",
        })
        .collect()
}

#[test]
fn i3326_format_prefere_puis_premier_produisible() {
    let annonces = formats_annonces(&support_pcm());
    assert_eq!(annonces.len(), 3);
    // Opus est en tête mais Tune ne sait pas le produire : on saute.
    assert_eq!(
        choisir_format(&annonces, None),
        Some(FormatAudio::pcm(48000, 2, 24))
    );
    // La préférence de l'état l'emporte si elle est annoncée et produisible.
    let p = FormatAudio::pcm(44100, 2, 16);
    assert_eq!(choisir_format(&annonces, Some(&p)), Some(p));
    // Une préférence non annoncée est ignorée.
    let hors = FormatAudio::pcm(96000, 2, 24);
    assert_eq!(
        choisir_format(&annonces, Some(&hors)),
        Some(FormatAudio::pcm(48000, 2, 24))
    );
}

#[test]
fn i3326_un_lecteur_flac_seul_n_obtient_pas_de_role() {
    let h = hello(json!({"buffer_capacity": 1000, "supported_formats": [
        {"codec": "flac", "sample_rate": 48000, "bit_depth": 24, "channels": 2}]}));
    assert!(SessionLecteur::admettre("id", "Cuisine", &h).is_none());
}

#[test]
fn i3326_sans_objet_versionne_le_role_n_est_pas_active() {
    // La forme non versionnée est héritée ; la spécification interdit
    // d'activer une version de rôle dont l'objet de support manque.
    let h: ClientHello = serde_json::from_value(json!({
        "name": "x", "supported_roles": ["player@v1"], "player_support": support_pcm()
    }))
    .unwrap();
    assert!(SessionLecteur::admettre("id", "x", &h).is_none());
    assert!(SessionLecteur::admettre("id", "x", &hello(support_pcm())).is_some());
}

#[test]
fn i3326_entete_audio_gros_boutiste_et_send_ahead_sature() {
    let corps = corps_audio(0x0102_0304_0506_0708, 0x0A0B_0C0D, &[0xEE]);
    assert_eq!(
        corps,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 0x0A, 0x0B, 0x0C, 0x0D, 0xEE]
    );
    assert_eq!(
        corps.len() + 1,
        TAILLE_ENTETE_AUDIO + 1,
        "13 octets d'en-tête avec le type"
    );
    assert_eq!(avance_d_envoi(1_000, 400), 600);
    assert_eq!(avance_d_envoi(1_000, 1_000), 0, "en retard ou pile : 0");
    assert_eq!(avance_d_envoi(1_000, 5_000), 0);
    assert_eq!(
        avance_d_envoi(i64::MAX, 0),
        u32::MAX,
        "sature, n'enroule pas"
    );
}

#[test]
fn i3326_avance_de_depart_inclut_le_delai_de_sortie() {
    let e = EtatLecteur {
        output_delay_ms: 20,
        required_lead_time_ms: 300,
        min_buffer_ms: 100,
        ..Default::default()
    };
    assert_eq!(
        avance_de_depart_us(&e),
        (300 + 20) * 1_000 + MARGE_DEPART_US
    );
    let e2 = EtatLecteur {
        min_buffer_ms: 500,
        ..e
    };
    assert_eq!(
        avance_de_depart_us(&e2),
        (500 + 20) * 1_000 + MARGE_DEPART_US
    );
}

#[test]
fn i3326_comptabilite_du_tampon() {
    let mut c = ComptabiliteTampon::nouvelle(100);
    assert!(c.admet(60, 0, 0));
    c.enregistrer(1_000, 1_000, 60); // fin à 2000 µs
    assert!(!c.admet(60, 0, 0), "60 + 60 > 100");
    assert!(!c.admet(60, 1_999, 0), "le morceau compte jusqu'à sa fin");
    assert!(c.admet(60, 2_000, 0), "fin atteinte : il ne compte plus");
    c.enregistrer(3_000, 1_000, 60);
    // Un délai de sortie avance la fin effective.
    assert!(c.admet(60, 3_500, 500));
    c.enregistrer(5_000, 1_000, 60);
    c.vider();
    assert_eq!(c.total(), 0);
}

#[test]
fn i3326_client_state_fondu_et_valide() {
    let e = fondre_etat(&EtatClient::default(), &etat(true, &["volume"])).unwrap();
    assert!(e.disponible());
    assert_eq!(e.lecteur.as_ref().unwrap().output_delay_ms, 20);
    // Objet de rôle omis : état du rôle inchangé.
    let e2 = fondre_etat(&e, &json!({"available": false})).unwrap();
    assert!(!e2.disponible());
    assert_eq!(e2.lecteur, e.lecteur);
    assert!(
        fondre_etat(&e, &json!({})).is_err(),
        "available est obligatoire"
    );
    let delai = json!({"available": true, "player": {"output_delay_ms": 9000,
        "required_lead_time_ms": 0, "min_buffer_ms": 0, "supported_commands": []}});
    assert_eq!(
        fondre_etat(&e, &delai)
            .unwrap()
            .lecteur
            .unwrap()
            .output_delay_ms,
        5_000,
        "borné à 5000 ms"
    );
}

#[test]
fn i3326_activation_initiale_puis_group_update() {
    let mut s = SessionLecteur::admettre("cid", "Cuisine", &hello(support_pcm())).unwrap();
    let a = s.activation_initiale();
    assert_eq!(types(&a), vec!["server/activate", "group/update"]);
    let Sortie::Json { payload, .. } = &a[0] else {
        panic!()
    };
    assert_eq!(payload["activities"], json!([]));
    assert_eq!(payload["active_roles"], json!(["player@v1"]));
    let Sortie::Json { payload, .. } = &a[1] else {
        panic!()
    };
    assert_eq!(payload["playback_state"], "stopped");
    assert_eq!(payload["group_name"], "Cuisine");
}

#[test]
fn i3326_aucun_flux_ni_commande_avant_le_premier_client_state() {
    let mut s = SessionLecteur::admettre("cid", "Cuisine", &hello(support_pcm())).unwrap();
    let f = FormatAudio::pcm(48000, 2, 24);
    assert_eq!(
        s.executer(OrdreLecteur::Demarrer(f.clone()), 1),
        Err(RefusOrdre::EtatAttendu)
    );
    assert_eq!(
        s.executer(OrdreLecteur::Volume(10), 1),
        Err(RefusOrdre::EtatAttendu)
    );
    assert!(s.recevoir_etat(&etat(false, &["volume"])).unwrap());
    assert_eq!(
        s.executer(OrdreLecteur::Demarrer(f), 1),
        Err(RefusOrdre::Indisponible)
    );
}

#[test]
fn i3326_cycle_de_vie_du_flux() {
    let mut s = SessionLecteur::admettre("cid", "Cuisine", &hello(support_pcm())).unwrap();
    s.recevoir_etat(&etat(true, &["volume"])).unwrap();
    let f = FormatAudio::pcm(48000, 2, 24);
    assert_eq!(
        s.executer(
            OrdreLecteur::Morceau {
                timestamp_us: 1,
                donnees: vec![0; 6]
            },
            0
        ),
        Err(RefusOrdre::FluxInactif),
        "jamais d'audio hors d'un flux"
    );
    assert_eq!(
        s.executer(OrdreLecteur::Vider, 0),
        Err(RefusOrdre::FluxInactif)
    );
    assert_eq!(
        s.executer(OrdreLecteur::Demarrer(FormatAudio::pcm(96000, 2, 24)), 0),
        Err(RefusOrdre::FormatRefuse),
        "format non annoncé"
    );
    let d = s.executer(OrdreLecteur::Demarrer(f.clone()), 777).unwrap();
    assert_eq!(
        types(&d),
        vec!["server/activate", "group/update", "stream/start"]
    );
    let Sortie::Json { payload, .. } = &d[2] else {
        panic!()
    };
    assert_eq!(payload["server_transmitted"], 777);
    assert_eq!(payload["player"]["codec"], "pcm");
    assert_eq!(payload["player"]["bit_depth"], 24);
    // Même format, flux ouvert : aucun nouveau stream/start (piste suivante).
    assert!(
        s.executer(OrdreLecteur::Demarrer(f.clone()), 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        types(&s.executer(OrdreLecteur::Vider, 5).unwrap()),
        vec!["stream/clear"]
    );
    // Pause : flux fermé, groupe arrêté, activité gardée.
    assert_eq!(
        types(&s.executer(OrdreLecteur::Suspendre, 0).unwrap()),
        vec!["stream/end", "group/update"]
    );
    assert_eq!(
        s.executer(OrdreLecteur::Vider, 0),
        Err(RefusOrdre::FluxInactif)
    );
    // Reprise : nouveau stream/start, mais pas de second server/activate.
    assert_eq!(
        types(&s.executer(OrdreLecteur::Demarrer(f), 0).unwrap()),
        vec!["group/update", "stream/start"]
    );
    // Arrêt : l'activité playback est retirée.
    let a = s.executer(OrdreLecteur::Arreter, 0).unwrap();
    assert_eq!(
        types(&a),
        vec!["stream/end", "group/update", "server/activate"]
    );
    // Un second arrêt n'envoie rien : stream/end interdit sans flux.
    assert!(s.executer(OrdreLecteur::Arreter, 0).unwrap().is_empty());
}

#[test]
fn i3326_commandes_seulement_si_proposees() {
    let mut s = SessionLecteur::admettre("cid", "Cuisine", &hello(support_pcm())).unwrap();
    s.recevoir_etat(&etat(true, &["volume"])).unwrap();
    let v = s.executer(OrdreLecteur::Volume(55), 0).unwrap();
    let Sortie::Json {
        type_message,
        payload,
    } = &v[0]
    else {
        panic!()
    };
    assert_eq!(*type_message, "server/command");
    assert_eq!(
        payload,
        &json!({"player": {"command": "volume", "volume": 55}})
    );
    assert_eq!(
        s.executer(OrdreLecteur::Sourdine(true), 0),
        Err(RefusOrdre::CommandeNonProposee("mute"))
    );
    s.recevoir_etat(&etat(true, &["volume", "mute"])).unwrap();
    let m = s.executer(OrdreLecteur::Sourdine(true), 0).unwrap();
    let Sortie::Json { payload, .. } = &m[0] else {
        panic!()
    };
    assert_eq!(
        payload,
        &json!({"player": {"command": "mute", "mute": true}})
    );
}

#[tokio::test]
async fn i3326_liaison_fermee_rend_deconnecte() {
    let (liaison, cote) = relier(vec![FormatAudio::pcm(48000, 2, 16)], None);
    assert!(liaison.connectee());
    assert_eq!(liaison.capacite(), CAPACITE_PAR_DEFAUT);
    drop(cote);
    assert!(!liaison.connectee());
    assert_eq!(
        liaison.ordonner(OrdreLecteur::Vider).await,
        Err(RefusOrdre::Deconnecte)
    );
}
