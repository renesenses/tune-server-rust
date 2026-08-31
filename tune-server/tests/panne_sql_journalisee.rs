//! Une panne de base laisse une trace, et la réponse HTTP ne bouge pas (#2861).
//!
//! ## Ce que ce fichier verrouille
//!
//! La #2860 a mesuré le coût du motif : trois erreurs PostgreSQL dans une même
//! requête, toutes avalées par le `unwrap_or_default()` de l'appelant, deux
//! sections de l'écran d'accueil vides pendant des mois — **sans une seule
//! ligne de journal**. Rien, dans ce dépôt, ne rougissait.
//!
//! Le test tient les deux bouts à la fois, parce que corriger l'un en cassant
//! l'autre serait un recul :
//!
//! 1. **La trace existe.** Une requête qui échoue produit un évènement `ERROR`
//!    nommant le fichier, la ligne et l'erreur SQL.
//! 2. **La réponse est inchangée.** Statut et corps sont *identiques* à ce
//!    qu'ils étaient avant le correctif — octet pour octet. Le geste ajoute du
//!    journal, il ne transforme pas un écran dégradé en 500.
//!
//! Les deux familles de sites sont couvertes : le SQL brut écrit dans un
//! gestionnaire (`history::top_artists`, via `backend.query_many`) et l'appel
//! de dépôt (`profiles::list_facet_favorites`, le site que la #2861 nomme).
//!
//! ## Pourquoi un binaire de test à lui seul, et un seul test dedans
//!
//! Leçon déjà payée par `tune-core/tests/journal_descriptif_illisible.rs` :
//! `tracing` met en cache, **pour tout le processus**, la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'un binaire
//! qui lance des tests en parallèle se voit priver d'évènements de façon
//! imprévisible, et la capture revient vide sans prévenir.
//!
//! Ici l'abonné est **global**, installé avant toute autre chose, et ce
//! binaire ne contient **qu'un seul test** : rien ne s'exécute en parallèle,
//! rien d'autre n'enregistre d'abonné, la capture ne dépend d'aucun ordre.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier ne serait jamais
//! compilé sans sa cible `[[test]]` dans `tune-server/Cargo.toml`. Voir
//! `tests_orphelins.rs`, qui refuse tout fichier non enregistré.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};

use tower::ServiceExt;

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois qu'une section se videra.
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

/// Les deux routes sous observation, une par famille de site corrigé.
const ROUTE_SQL_BRUT: &str = "/api/v1/history/top-artists";
const ROUTE_DEPOT: &str = "/api/v1/profiles/1/favorites/facets";

async fn get(app: &axum::Router, chemin: &str) -> (StatusCode, String) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

#[tokio::test]
async fn une_panne_sql_laisse_une_trace_sans_changer_la_reponse() {
    let capture = JournalCapture::default();
    // Niveau WARN : ce qu'un journal ORDINAIRE laisse passer. `log_level` vaut
    // `info` par défaut, donc une trace posée en `debug!` serait restée
    // invisible en service — la panne aurait changé de silence, pas cessé.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("base en mémoire");
    let app = tune_server::routes::router(etat.clone());

    // --- 1. Le cas nominal, AVANT toute casse : le témoin anti-régression ---

    let (statut_sain_sql, corps_sain_sql) = get(&app, ROUTE_SQL_BRUT).await;
    let (statut_sain_depot, corps_sain_depot) = get(&app, ROUTE_DEPOT).await;

    assert_eq!(
        statut_sain_sql,
        StatusCode::OK,
        "{ROUTE_SQL_BRUT} doit répondre 200 sur une base saine — sinon le test \
         n'observe pas ce qu'il croit observer (corps : {corps_sain_sql})"
    );
    assert_eq!(
        statut_sain_depot,
        StatusCode::OK,
        "{ROUTE_DEPOT} doit répondre 200 sur une base saine (corps : {corps_sain_depot})"
    );

    // Une base saine mais vide ne doit produire AUCUNE trace de panne :
    // sinon le journal crierait au loup à chaque bibliothèque neuve, et la
    // vraie panne se noierait dans le bruit.
    let journal_sain = capture.texte();
    assert!(
        !journal_sain.contains("panne_sql_avalee"),
        "une base saine ne doit produire aucune trace de panne, sans quoi la \
         trace ne vaut plus rien comme signal :\n{journal_sain}"
    );

    // --- 2. On casse la base sous les mêmes routes ---
    //
    // Une table absente est la forme la plus simple d'une erreur SQL à
    // l'exécution. C'est bien la même famille que la #2860 : là-bas
    // PostgreSQL refusait `text = bigint`, ici SQLite refuse une table
    // disparue — dans les deux cas `query_many` rend `Err`, et c'est ce
    // `Err` qui se perdait.
    etat.backend
        .execute_batch("DROP TABLE listen_history; DROP TABLE favorite_facets;")
        .expect("les deux tables existent sur une base neuve");

    let (statut_casse_sql, corps_casse_sql) = get(&app, ROUTE_SQL_BRUT).await;
    let (statut_casse_depot, corps_casse_depot) = get(&app, ROUTE_DEPOT).await;

    // --- 3. La réponse n'a PAS bougé — le silence cesse, l'écran ne casse pas ---

    assert_eq!(
        (statut_casse_sql, corps_casse_sql.as_str()),
        (statut_sain_sql, corps_sain_sql.as_str()),
        "la réponse de {ROUTE_SQL_BRUT} a changé : le correctif a remplacé un \
         silence par une panne, ce que la #2861 interdit explicitement"
    );
    assert_eq!(
        (statut_casse_depot, corps_casse_depot.as_str()),
        (statut_sain_depot, corps_sain_depot.as_str()),
        "la réponse de {ROUTE_DEPOT} a changé — retirer ses favoris à quelqu'un \
         parce qu'une requête a échoué serait pire que le défaut d'origine"
    );

    // --- 4. …mais le journal, lui, porte désormais l'échec ---

    let journal = capture.texte();
    let traces: Vec<&str> = journal
        .lines()
        .filter(|l| l.contains("panne_sql_avalee"))
        .collect();

    assert!(
        traces.len() >= 2,
        "deux requêtes ont échoué, le journal doit porter deux traces — c'est \
         tout le défaut de la #2861 : la section ne s'explique pas, elle \
         disparaît.\ntraces trouvées : {}\njournal complet :\n{journal}",
        traces.len()
    );

    // Le lieu : sans lui, la trace dit qu'« une requête » a échoué et n'aide
    // personne à savoir laquelle. `#[track_caller]` doit désigner le site
    // d'appel, PAS `panne_sql.rs` où vit le helper.
    assert!(
        traces.iter().any(|l| l.contains("history.rs")),
        "aucune trace ne désigne history.rs — le lieu remonté est-il celui du \
         helper au lieu de celui de l'appelant ?\n{journal}"
    );
    assert!(
        traces.iter().any(|l| l.contains("profiles.rs")),
        "aucune trace ne désigne profiles.rs :\n{journal}"
    );
    assert!(
        !traces.iter().any(|l| l.contains("panne_sql.rs")),
        "la trace désigne le helper au lieu de l'appelant : `#[track_caller]` \
         ne joue pas, et le lieu ne sert plus à rien.\n{journal}"
    );

    // L'erreur elle-même : une trace qui ne dit pas CE QUI a échoué oblige à
    // deviner. Le message de SQLite pour une table disparue est stable.
    assert!(
        traces.iter().any(|l| l.contains("no such table")),
        "la trace ne rapporte pas l'erreur SQL — c'est elle qui a manqué \
         pendant des mois sur la #2860.\n{journal}"
    );
    assert!(
        traces.iter().any(|l| l.contains("listen_history")),
        "la trace ne nomme pas la table en cause :\n{journal}"
    );
}
