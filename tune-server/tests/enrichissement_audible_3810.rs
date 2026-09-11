//! Une passe d'enrichissement dit qu'elle part, et un refus dit pourquoi (#3810).
//!
//! ## Le défaut mesuré
//!
//! **Tades**, fil forum 1746, 10/09/2026, 0.9.145 Windows, **262 858 pistes**,
//! licence Premium **validée en ligne** (`license_validated_from_heartbeat
//! tier=premium` dans son rapport — le verdict `Confirmée` du serveur, donc le
//! portillon Premium n'est PAS en cause chez lui) :
//!
//! > « Toujours pas possible »
//!
//! Son rapport de diagnostic ne porte **aucune ligne d'enrichissement**, et
//! c'est ce qui a fermé l'enquête — à tort. Mesuré dans ce dépôt :
//!
//! * `POST /library/enrich-all`, la route du bouton, n'écrivait **rien** à
//!   l'entrée. Sa seule trace était le `enrich_all_library done` de la fin ;
//!   sur 262 858 pistes à ~1,1 s l'aller-retour MusicBrainz, cette fin arrive
//!   dans **plusieurs semaines**. L'absence de ligne ne distinguait donc pas
//!   « il n'a pas cliqué » de « il a cliqué et la passe tourne depuis ».
//! * Le compte de candidats n'était journalisé que sur une passe **limitée à un
//!   répertoire**. La passe complète — celle du bouton — n'annonçait son
//!   ampleur nulle part, alors que c'est le seul chiffre qui sépare « ça ne
//!   démarre pas » de « ça n'a pas fini ».
//! * Un refus de quota du palier gratuit rendait un `429` **sans une ligne de
//!   journal**, pendant que l'interface v2 avale le corps de l'erreur pour
//!   afficher un « échec du démarrage » générique (#3732). Le refus était donc
//!   invisible des DEUX côtés.
//!
//! Ce fichier ne corrige aucun symptôme non reproduit : il rend la prochaine
//! mesure concluante.
//!
//! ## Pourquoi un binaire à lui seul, un seul test dedans
//!
//! `tracing` met en cache pour tout le processus la décision « ce point d'appel
//! intéresse-t-il quelqu'un ? » : un abonné posé au milieu d'un binaire qui
//! lance des tests en parallèle rend des captures vides sans prévenir. Abonné
//! **global**, un test.
//!
//! ⚠️ `tune-server` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-server/Cargo.toml`, ce fichier ne serait JAMAIS compilé.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn post(app: &axum::Router, chemin: &str, corps: serde_json::Value) -> (StatusCode, String) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// La passe part dans une tâche détachée : la ligne du compte de candidats
/// n'est pas encore écrite quand le `202` revient. On attend qu'elle paraisse,
/// avec une borne — un test qui attendrait sans fin ne rougirait jamais, il
/// expirerait, et un délai d'expiration ne porte aucun nom d'assertion.
async fn attendre_trace(capture: &JournalCapture, marqueur: &str) -> String {
    for _ in 0..200 {
        let journal = capture.texte();
        if journal.contains(marqueur) {
            return journal;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    capture.texte()
}

#[tokio::test]
async fn la_passe_dit_qu_elle_part_son_ampleur_et_la_raison_de_son_refus() {
    let capture = JournalCapture::default();
    // Niveau INFO : ce que `log_level` laisse passer en service. Une trace
    // posée en `debug!` n'aurait fait que changer de silence.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("base en mémoire");
    let app = tune_server::routes::router(etat.clone());

    // --- 1. Le clic sur « Enrichir les métadonnées » ---
    //
    // Base vide : zéro candidat, donc zéro requête MusicBrainz. Ce qu'on
    // mesure est le DÉPART, pas la passe.
    let (statut, corps) = post(&app, "/api/v1/library/enrich-all", json!({})).await;
    assert_eq!(
        statut,
        StatusCode::ACCEPTED,
        "le contrat de la route ne doit pas bouger : 202 et un `task_id` \
         (corps reçu : {corps})"
    );

    let journal = attendre_trace(&capture, "enrich_all_candidats").await;

    let departs: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("enrich_all_demarre"))
        .collect();
    assert_eq!(
        departs.len(),
        1,
        "un clic sur le bouton doit laisser UNE trace de départ. Sans elle, un \
         rapport pris cinq minutes après le clic ne distingue pas « il n'a pas \
         cliqué » de « la passe tourne » — c'est l'impasse de Tades.\n\
         journal complet :\n{journal}"
    );
    assert!(
        departs[0].contains("premium=false"),
        "la trace de départ doit dire sous quel palier la passe est partie : \
         c'est ce qui écarte — ou retient — le portillon Premium sans avoir à \
         interroger le serveur de licences :\n{}",
        departs[0]
    );

    let candidats: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("enrich_all_candidats"))
        .collect();
    assert_eq!(
        candidats.len(),
        1,
        "la passe COMPLÈTE doit annoncer son ampleur, pas seulement la passe \
         limitée à un répertoire : à ~1,1 s par piste, le nombre de candidats \
         est le seul chiffre qui sépare « ça ne démarre pas » de « ça n'a pas \
         fini ».\njournal complet :\n{journal}"
    );
    assert!(
        candidats[0].contains("candidats=0"),
        "le compte annoncé doit être celui de la sélection — ici une base vide, \
         donc zéro :\n{}",
        candidats[0]
    );

    // --- 2. Le refus de quota du palier gratuit ---
    //
    // Le premier appel a déjà posé la date du jour et incrémenté le compteur :
    // on écrase le seul compteur, sans avoir à recalculer la date comme le
    // serveur la calcule — deux implémentations de « aujourd'hui » finiraient
    // par diverger un 31 décembre.
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone());
    assert!(
        reglages
            .get("enrichment_daily_date")
            .ok()
            .flatten()
            .is_some(),
        "le premier appel doit avoir posé la date du quota — sinon le second \
         repartirait sur un compteur remis à zéro et ce test ne mesurerait rien"
    );
    reglages
        .set("enrichment_daily_count", "999")
        .expect("le compteur de quota s'écrit");

    let (statut, corps) = post(&app, "/api/v1/library/enrich-all", json!({})).await;
    assert_eq!(
        statut,
        StatusCode::TOO_MANY_REQUESTS,
        "quota épuisé : le refus reste un 429 (corps reçu : {corps})"
    );
    assert!(
        corps.contains("free_tier_daily_enrichment_limit_reached"),
        "le corps du refus ne doit pas bouger — la v2 l'ignore, mais la v1 et \
         les intégrations le lisent : {corps}"
    );

    let journal = capture.texte();
    let refus: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("enrichissement_refuse_quota_gratuit"))
        .collect();
    assert_eq!(
        refus.len(),
        1,
        "un 429 doit laisser une trace : la v2 remplace le corps de l'erreur \
         par un « échec du démarrage » générique (#3732), donc sans ligne de \
         journal le refus est invisible des DEUX côtés.\n\
         journal complet :\n{journal}"
    );
    assert!(
        refus[0].contains("used=999") && refus[0].contains("limit=10"),
        "la trace doit porter les DEUX chiffres : « quota dépassé » sans le \
         quota n'apprend rien à qui lit le journal :\n{}",
        refus[0]
    );
}
