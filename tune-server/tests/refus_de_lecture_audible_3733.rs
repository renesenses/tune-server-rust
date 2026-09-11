//! Un refus de lecture laisse une trace, et la réponse ne bouge pas (#3733).
//!
//! ## Le défaut mesuré
//!
//! **Jean-Luc Cassé** (« Lulu »), Windows, 0.9.143, le 09/09/2026 :
//!
//! > « Ce matin après mise à jour dernière version de Tune (V0.9.143), lecture
//! > des albums impossible. Plusieurs tentatives, fermeture/ouverture de Tune. »
//!
//! La panne n'est **pas reproduite**, et ce fichier ne prétend pas la
//! reproduire. Il ferme autre chose : la boucle d'enquête.
//!
//! `POST /zones/{id}/play` rend `400 "no tracks to play"` dès que la demande ne
//! résout aucune piste. Ce refus n'écrivait **aucune ligne de journal**, et
//! l'interface v2 avale l'erreur (`.catch(() => {})`, #3732). Serveur muet +
//! client muet = « rien ne se passe » : le testeur n'a rien à envoyer, et
//! l'hypothèse ne peut être ni confirmée ni infirmée. C'est un défaut
//! d'observabilité, pas une hypothèse sur la cause.
//!
//! Le rattrapage par album sœur (`find_populated_sibling`) ne journalisait,
//! lui aussi, que son **succès** : le seul cas qui intéresse — l'échec —
//! sortait en silence. Or l'échec a deux formes qui n'envoient pas chercher au
//! même endroit : aucune sœur de même titre et même `artist_id`, ou une sœur
//! trouvée mais vide elle aussi.
//!
//! ## Ce que ce fichier tient, les deux bouts à la fois
//!
//! 1. **La trace existe**, au niveau `WARN` — donc visible avec le `log_level`
//!    ordinaire (`info`). Posée en `debug!`, elle aurait seulement changé de
//!    silence.
//! 2. **La réponse est inchangée**, statut ET corps, octet pour octet. Les
//!    clients déjà livrés comparent la chaîne `no tracks to play` telle quelle :
//!    le geste ajoute du journal, il ne touche pas au contrat.
//!
//! ## Pourquoi un binaire de test à lui seul, et un seul test dedans
//!
//! `tracing` met en cache, **pour tout le processus**, la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'un binaire
//! qui lance des tests en parallèle rend des captures vides sans prévenir
//! (leçon de `tune-core/tests/journal_descriptif_illisible.rs`, reprise par
//! `tune-server/tests/panne_sql_journalisee.rs`). Ici l'abonné est **global**,
//! installé avant toute autre chose, et ce binaire ne contient **qu'un seul
//! test**.
//!
//! ⚠️ `tune-server` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-server/Cargo.toml`, ce fichier ne serait JAMAIS compilé — et un
//! témoin jamais compilé ne garde rien.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois qu'un testeur écrira « ça ne part pas ».
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

#[tokio::test]
async fn un_album_sans_piste_refuse_la_lecture_en_le_disant_au_journal() {
    let capture = JournalCapture::default();
    // Niveau INFO : ce qu'un journal ORDINAIRE laisse passer (`log_level` vaut
    // `info` par défaut). Une trace posée en `debug!` resterait invisible en
    // service, et le défaut n'aurait fait que changer de silence.
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

    // La ligne d'album de Jean-Luc, telle que `delete_orphans()` la laisse : la
    // ligne existe, elle n'a plus une seule piste. Et aucune sœur de même titre
    // — c'est ce que produit un changement d'`artist_id` sur le dossier entier.
    etat.backend
        .execute_batch(
            "INSERT INTO zones (id, name, output_type) VALUES (1, 'Salon', 'local');\
             INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd');\
             INSERT INTO albums (id, title, artist_id) VALUES (7, 'The Dark Side of the Moon', 1);",
        )
        .expect("zone, artiste et album vide sur une base neuve");

    let (statut, corps) = post(&app, "/api/v1/zones/1/play", json!({"album_id": 7})).await;

    // --- 1. Le contrat de réponse n'a PAS bougé ---
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "le refus doit rester un 400 : un client déjà livré le distingue d'un \
         500 (corps reçu : {corps})"
    );
    assert_eq!(
        corps, "no tracks to play",
        "le CORPS du refus doit rester identique à l'octet près — le geste \
         ajoute du journal, il ne réécrit pas le contrat"
    );

    // --- 2. …mais le journal, lui, porte désormais le refus ---
    let journal = capture.texte();

    let refus: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("play_refuse_aucune_piste"))
        .collect();
    assert_eq!(
        refus.len(),
        1,
        "un refus de lecture doit laisser UNE trace : sans elle, le serveur \
         est aussi muet que le client et « lecture impossible » n'est \
         instruisable par personne.\njournal complet :\n{journal}"
    );

    let ligne = refus[0];
    // Le refus doit NOMMER ce qui a été demandé. « une demande n'a rien
    // résolu » n'aide personne : sans l'album, on ne sait pas si la ligne a été
    // vidée ou si le client a posté une liste vide.
    assert!(
        ligne.contains("album_id=Some(7)"),
        "la trace ne nomme pas l'album demandé — c'est la valeur qui permet \
         d'aller voir la ligne en base :\n{ligne}"
    );
    assert!(
        ligne.contains("zone_id=1"),
        "la trace ne nomme pas la zone :\n{ligne}"
    );
    assert!(
        ligne.contains("pistes_demandees=None"),
        "la trace doit dire que le client n'a PAS envoyé de liste de pistes : \
         c'est ce qui sépare « le contenant est vide » de « le client a posté \
         une liste vide » :\n{ligne}"
    );

    // --- 3. Et l'ÉCHEC du rattrapage par album sœur est nommé, lui aussi ---
    let rattrapage: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("album_sans_piste_rattrapage_echoue"))
        .collect();
    assert_eq!(
        rattrapage.len(),
        1,
        "seul le SUCCÈS du rattrapage était journalisé : l'échec, le seul cas \
         que le testeur rapporte, ne laissait rien.\njournal complet :\n{journal}"
    );
    assert!(
        rattrapage[0].contains("soeur_peuplee=None"),
        "la trace doit distinguer « aucune sœur de même titre et même artiste » \
         de « une sœur trouvée mais vide » — les deux n'envoient pas chercher \
         au même endroit :\n{}",
        rattrapage[0]
    );
}
