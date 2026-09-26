//! #4366 — un 403 amont doit dire **combien d'en-têtes de yt-dlp ont été
//! rejoués sur la requête qui l'a pris**.
//!
//! Deux testeurs reçoivent la même phrase — `AAC download failed: upstream
//! HTTP 403 Forbidden` — et le dossier est bloqué depuis trois versions faute
//! du seul chiffre qui départage ses hypothèses :
//!
//! - `entetes=0` ⇒ le rejeu n'a pas eu lieu sur CETTE requête : le défaut est
//!   chez nous, et c'est une piste de code ;
//! - `entetes>0` ⇒ les en-têtes sont partis et `googlevideo` refuse quand
//!   même : l'hypothèse « en-têtes absents » tombe.
//!
//! Le commentaire du 19/09 renvoyait à `youtube_stream_url_resolved entetes=N`
//! — écrit à la RÉSOLUTION, dans un autre module, pour une autre requête.
//! Rien ne garantit qu'il décrive celle qui a échoué : demander ce journal au
//! testeur était un tirage au sort.
//!
//! ## Le banc
//!
//! Un vrai serveur HTTP qui répond 403, et la vraie fonction de
//! téléchargement. Le journal est capturé au niveau **WARN/INFO** — celui des
//! exports de terrain — et **dans le fil qui écrit**, puisque
//! `set_default` est local au fil et que le téléchargement est bloquant.

use super::resolve_stream::{noms_des_entetes, telecharger_amont};
use axum::{Router, routing::get};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Journal {
    fn write(&mut self, octets: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(octets);
        Ok(octets.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Le CDN qui refuse, exactement comme `googlevideo` chez FabienM et Bilou.
async fn cdn_qui_refuse() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/videoplayback",
        get(|| async { (axum::http::StatusCode::FORBIDDEN, "") }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/videoplayback", ecoute.local_addr().unwrap());
    let tache = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    (url, tache)
}

/// Lance le téléchargement sur un fil bloquant, l'abonné `tracing` posé DANS
/// ce fil — sans quoi rien n'est capturé (`set_default` est local au fil).
async fn refus_et_journal(url: String, entetes: Vec<(String, String)>) -> (String, String) {
    let journal = Journal::default();
    let capture = journal.clone();
    // Supprimé par `Drop`, panique comprise : le `remove_file` de fin de
    // fonction ne tournait pas quand le fil bloquant paniquait.
    let fichier = crate::test_scratch::scratch_file("tune-temoin-4366", ".m4a");
    let vers_clone = fichier.to_string_lossy().to_string();
    let issue = tokio::task::spawn_blocking(move || {
        let _garde = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(capture)
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        );
        telecharger_amont(&url, &entetes, &vers_clone)
    })
    .await
    .unwrap();
    drop(fichier);
    (issue.unwrap_err(), journal.texte())
}

/// **Le témoin.** Le CDN refuse en 403 alors que deux en-têtes de yt-dlp ont
/// été rejoués. Le journal doit porter le compte — sans lui, le rapport du
/// testeur ne dit pas si le rejeu a eu lieu.
#[tokio::test(flavor = "multi_thread")]
async fn un_403_amont_dit_combien_d_entetes_ont_ete_rejoues() {
    let (url, tache) = cdn_qui_refuse().await;
    let entetes = vec![
        ("User-Agent".to_string(), "Mozilla/5.0 (yt-dlp)".to_string()),
        (
            "Cookie".to_string(),
            "SECRET_A_NE_PAS_JOURNALISER".to_string(),
        ),
    ];

    let (erreur, journal) = refus_et_journal(url, entetes).await;
    tache.abort();

    assert!(
        erreur.contains("403"),
        "le refus amont doit remonter tel quel : {erreur}"
    );
    assert!(
        journal.contains("amont_refuse_entetes_rejouees"),
        "le refus amont doit se nommer au journal.\n{journal}"
    );
    assert!(
        journal.contains("entetes=2"),
        "sans ce compte, le 403 de FabienM et de Bilou reste indéchiffrable.\n{journal}"
    );
    assert!(
        journal.contains("statut=403"),
        "le statut doit se lire sur la même ligne.\n{journal}"
    );
}

/// Le cas symétrique, celui qui accuserait notre code : **zéro** en-tête
/// rejoué sur la requête refusée. Il doit se lire aussi clairement.
#[tokio::test(flavor = "multi_thread")]
async fn un_403_sans_aucun_entete_rejoue_le_dit_aussi() {
    let (url, tache) = cdn_qui_refuse().await;

    let (_, journal) = refus_et_journal(url, Vec::new()).await;
    tache.abort();

    assert!(
        journal.contains("entetes=0"),
        "`entetes=0` est le résultat qui désigne notre code : il doit se lire.\n{journal}"
    );
}

/// ⚠️ **Les VALEURS ne partent jamais au journal.** Un `Cookie` de
/// `googlevideo` est un secret de session, et ces journaux sont collés sur un
/// forum public.
#[tokio::test(flavor = "multi_thread")]
async fn les_valeurs_des_entetes_ne_sont_jamais_journalisees() {
    let (url, tache) = cdn_qui_refuse().await;
    let entetes = vec![(
        "Cookie".to_string(),
        "SECRET_A_NE_PAS_JOURNALISER".to_string(),
    )];

    let (_, journal) = refus_et_journal(url, entetes).await;
    tache.abort();

    assert!(
        journal.contains("Cookie"),
        "le NOM sert au diagnostic.\n{journal}"
    );
    assert!(
        !journal.contains("SECRET_A_NE_PAS_JOURNALISER"),
        "🔴 une valeur d'en-tête a fuité dans le journal.\n{journal}"
    );
}

/// Les noms se lisent dans l'ordre où ils ont été rejoués, séparés par des
/// virgules — une chaîne, donc greppable dans un export de terrain.
#[test]
fn les_noms_se_listent_sans_les_valeurs() {
    let entetes = vec![
        ("User-Agent".to_string(), "yt-dlp".to_string()),
        (
            "Referer".to_string(),
            "https://www.youtube.com/".to_string(),
        ),
    ];
    assert_eq!(noms_des_entetes(&entetes), "User-Agent,Referer");
    assert_eq!(noms_des_entetes(&[]), "");
}
