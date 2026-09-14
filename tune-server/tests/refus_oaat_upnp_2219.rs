//! D4 — une piste de serveur UPnP **refuse** la sortie OAAT, en disant pourquoi.
//!
//! # Ce que le dossier mesure, et ce que Bertrand a tranché
//!
//! La reconnaissance du chantier `unifier-serveurs-upnp-et-bibliotheque` relève
//! quatre sorties et quatre comportements pour une piste `source = "upnp"` :
//!
//! | sortie | aujourd'hui |
//! |---|---|
//! | réseau | joue, sans DSP |
//! | navigateur | joue, sans DSP |
//! | locale | joue avec DSP, sans ReplayGain, seek cassé |
//! | **OAAT** | **silence** |
//!
//! Arbitrage de Bertrand, 14/09/2026 : **jouable partout, défauts assumés et
//! DITS** — et OAAT, la seule sortie qui ne joue pas, **refuse explicitement,
//! avec un motif**.
//!
//! Le silence a une cause nommée dans le dépôt : un point de sortie OAAT « ne
//! consomme que du PCM en conteneur WAV » (`resolve_direct.rs`,
//! `decoder_bandcamp_en_wav`). Les deux autres sources qui passent par
//! `resolve_direct_url` — la radio et Bandcamp — ont chacune leur bras de
//! décodage vers OAAT. Ce chemin-ci n'en a jamais eu : il pousse l'URL
//! compressée telle quelle, la zone affiche « en lecture », et rien ne sort.
//!
//! # Ce que ce fichier garde
//!
//! 1. **Le refus est un refus** : la route rend une erreur, pas un succès.
//! 2. **Le motif est dit**, et il nomme les trois choses qu'un auditeur doit
//!    savoir : que c'est OAAT, que OAAT ne lit que du WAV, et où la piste joue.
//! 3. **La contre-épreuve** : le même appel sur la même zone OAAT avec un flux
//!    déjà en WAV ne porte PAS ce motif. Sans elle, un refus posé trop large
//!    passerait le point 1 en cassant une lecture qui marchait — Asset publie
//!    un `res` `audio/wav` (`.forced.wav`) à côté de son FLAC.
//!
//! La route est appelée, pas le résolveur : `POST /api/v1/zones/{id}/play` avec
//! `{source, source_id}` est le chemin réel d'une piste de serveur média
//! (`routes/playback.rs`, branche « piste distante seule »).
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier est une cible
//! `[[test]]` déclarée dans `Cargo.toml`.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

/// Le périphérique de sortie d'un point OAAT, tel que l'orchestrateur le
/// reconnaît (`resolve_direct.rs` : préfixes `oaat:` et `oaat-group:`).
const SORTIE_OAAT: &str = "oaat:temoin-2219";

/// Une URL de serveur média, de la forme qu'Asset publie — l'`ObjectID` y est,
/// et l'extension dit le format.
const URL_FLAC: &str = "http://127.0.0.1:9/content/d6120941636376083059-coX.flac";
/// Le même serveur, son `res` WAV. Asset en publie un à côté du FLAC.
const URL_WAV: &str = "http://127.0.0.1:9/content/d6120941636376083059-coX.forced.wav";

async fn poster(app: &Router, chemin: &str, corps: Value) -> (StatusCode, String) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .expect("routeur en échec");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps lisible");
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Une zone dont la sortie EST un point OAAT.
fn poser_la_zone_oaat(etat: &AppState) {
    etat.backend
        .execute_batch(&format!(
            "INSERT INTO zones (id, name, output_type, output_device_id) \
             VALUES (1, 'Endpoint', 'oaat', '{SORTIE_OAAT}');"
        ))
        .expect("zone OAAT sur une base neuve");
}

async fn jouer_une_piste_upnp(app: &Router, url: &str) -> (StatusCode, String) {
    poster(
        app,
        "/api/v1/zones/1/play",
        json!({
            "source": "upnp",
            "source_id": url,
            "title": "Wonderwall",
            "artist_name": "Oasis",
            "album_title": "Morning Glory",
        }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn une_piste_upnp_refuse_la_sortie_oaat_en_disant_pourquoi() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_la_zone_oaat(&etat);
    let app = tune_server::routes::router(etat);

    let (statut, corps) = jouer_une_piste_upnp(&app, URL_FLAC).await;

    // --- 1. C'est un refus, pas un succès ---
    assert!(
        statut.is_client_error() || statut.is_server_error(),
        "la lecture devait être REFUSÉE, pas acceptée : une zone qui répond OK \
         puis ne joue rien est exactement le défaut qu'on ferme (statut \
         {statut}, corps {corps})"
    );

    // --- 2. Le motif dit les quatre choses qui comptent ---
    //
    // 🔴 Ces contrôles cherchaient des MOTS ISOLÉS — « OAAT », « WAV »,
    // « silence ». Le motif livré était crevé : les continuations de chaîne
    // avaient été perdues à l'écriture, et le message portait des suites de
    // dix-huit espaces au milieu de ses phrases. Il contenait bien les trois
    // mots, et les trois contrôles passaient. Un texte destiné à être LU se
    // vérifie par des phrases entières, sinon on ne garde qu'un sac de mots.
    for phrase in [
        "un point de sortie OAAT ne lit que du PCM en conteneur WAV",
        "la piste n'a pas été lancée, elle n'aurait produit qu'un silence",
        "Elle joue en revanche sur une zone réseau, navigateur ou locale",
    ] {
        assert!(
            corps.contains(phrase),
            "le motif doit contenir la phrase « {phrase} », mot pour mot et \
             espace pour espace — corps {corps}"
        );
    }
    // Et le titre de la piste, qui est ce que l'auditeur reconnaît.
    assert!(
        corps.contains("Wonderwall"),
        "le motif doit nommer la piste refusée — corps {corps}"
    );
    // Aucune suite d'espaces : un message à trous a déjà été livré une fois.
    assert!(
        !corps.contains("  "),
        "le motif porte une suite d'espaces — une chaîne dont les \
         continuations ont été perdues : {corps}"
    );
}

/// **La contre-épreuve.** Le refus est fermé sur le format, pas sur la source :
/// un `res` déjà en WAV — Asset en publie un — doit continuer de passer.
///
/// L'appel échouera plus loin (l'hôte du banc ne répond pas), et c'est sans
/// importance : ce qu'on exige ici, c'est que le motif du refus OAAT **ne soit
/// pas** celui qu'on lit. Sans ce second témoin, un refus posé sur la seule
/// source passerait le premier test en cassant une lecture qui marchait.
#[tokio::test(flavor = "multi_thread")]
async fn un_res_deja_en_wav_ne_porte_pas_le_refus_oaat() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    poser_la_zone_oaat(&etat);
    let app = tune_server::routes::router(etat);

    let (_statut, corps) = jouer_une_piste_upnp(&app, URL_WAV).await;

    // La phrase cherchée ici est EXACTEMENT celle que le premier témoin exige
    // de trouver. Les deux contrôles se tiennent : si le motif changeait de
    // mots, le premier rougirait, et ce second-ci ne pourrait pas devenir
    // vacuux sans que l'autre ne le dise. C'est ce qui manquait quand ce test
    // cherchait une phrase que le motif abîmé ne contenait plus.
    assert!(
        !corps.contains("un point de sortie OAAT ne lit que du PCM en conteneur WAV"),
        "un flux DÉJÀ en WAV ne doit pas tomber sous le refus : le refus \
         garderait alors plus que le défaut qu'il vise — corps {corps}"
    );
}
