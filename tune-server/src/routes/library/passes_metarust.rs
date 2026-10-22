//! Lot 2 de l'enrichissement d'après MetaRust, de Xavier Joly (#4805) : les
//! deux PASSES DE LOT, déclenchées à la main.
//!
//! | route | effet |
//! |---|---|
//! | `POST /library/bpm-all` | BPM de tous les albums locaux : Wikidata, puis le signal |
//! | `GET  /library/bpm-all/status` | où en est la passe |
//! | `POST /library/discogs-all` | propositions Discogs des albums sans MBID |
//! | `GET  /library/discogs-all/status` | où en est la passe |
//!
//! Le travail par album existe depuis le lot 1 (#6199) : le bouton « Estimer
//! les BPM » et le bouton « Chercher sur Discogs » de la fiche. Ce module
//! n'ajoute **aucune** logique d'enrichissement : il appelle les mêmes
//! fonctions ([`super::enrichissement_metarust::estimer_les_bpm`],
//! [`discogs::chercher_par`]) et ne fait que choisir les albums, tenir
//! l'avancement, s'arrêter et reprendre.
//!
//! # Les décisions de Bertrand (13/10/2026), et où elles vivent
//!
//! * **À la main, comme `identify-all`.** Le seul chemin d'entrée est la
//!   route POST : rien depuis le scan, rien au démarrage. Un serveur qui
//!   redémarre au milieu d'une passe ne la relance pas ; l'état dit
//!   `interrupted`, et le prochain clic reprend après le dernier album traité.
//! * **BPM : Wikidata, puis le signal en Rust natif.** Jamais ffmpeg ni
//!   aubio : c'est `tempo::mesurer_bpm_du_fichier`, sur le décodeur de Tune.
//!   La balise du fichier (`tracks.bpm`) n'est jamais écrite.
//! * **Discogs : des propositions, seulement.** Une proposition se range dans
//!   `propositions_discogs` ; rien n'est écrit sur l'album avant que
//!   l'utilisateur l'accepte sur la fiche. Le débit tient sous les 60
//!   requêtes par minute permises avec un jeton : le limiteur PARTAGÉ
//!   `DISCOGS` (0,9 requête/s, soit 54/min, adaptatif sur `429` et
//!   `Retry-After`), le même que la fiche et les portraits d'artistes. Deux
//!   passes n'iraient donc pas plus vite.
//! * **Gratuites, sans Premium.** Décision de Bertrand du 13/10/2026 : aucune
//!   garde de droit, contrairement à `identify-all`. Pour Discogs, les
//!   conditions de l'API interdisent de faire payer l'accès à une partie de
//!   l'application qui l'intègre. Témoin : `les_passes_sont_gratuites`.
//! * **Fraîcheur Discogs : six heures.** Une proposition en attente lue il y a
//!   plus de six heures est périmée : la fiche n'en montre plus le contenu,
//!   elle ne s'accepte plus, et la passe la relit au lieu de la sauter.
//!
//! # Pause et reprise
//!
//! Les deux passes sont des passes d'ENRICHISSEMENT : elles se suspendent par
//! [`Tache::Enrichissement`] (`POST /system/background-tasks/enrichment/pause`),
//! le registre déjà lu par l'écran « Tâches de fond ». La pause est relue
//! entre deux albums ([`taches_de_fond::attendre_la_reprise`]) et, pour le
//! BPM, avant chaque décodage ([`taches_de_fond::attendre_son_tour`], qui
//! cède aussi à la lecture). La passe se GARE : « Reprendre » la fait
//! repartir au même album, sans nouveau clic.
//!
//! # Le disjoncteur Discogs
//!
//! Sur des centaines d'albums, une panne Discogs produirait une passe verte
//! qui conclurait « aucune proposition » partout. Douze pannes d'affilée
//! ([`PANNES_CONSECUTIVES_MAX`], même seuil que `identify-all`) arrêtent la
//! passe en le disant (`discogs_injoignable`). Un jeton refusé (`401`/`403`)
//! l'arrête aussitôt (`discogs_jeton_refuse`) : continuer ne ferait que
//! répéter le refus. Un album sans résultat n'est PAS une panne.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::metadata::discogs_propositions::{self as discogs, Empechement};
use tune_core::taches_de_fond::{self, Tache, est_en_pause};

use crate::state::AppState;

/// Douze pannes d'affilée : le seuil de `identify-all` (#4805).
const PANNES_CONSECUTIVES_MAX: u32 = 12;

/// L'avancement s'écrit en base tous les N albums (et à chaque fin).
const ALBUMS_PAR_ECRITURE: usize = 10;

/// Une passe de lot : son nom, sa clé d'état, son drapeau « en vol ».
pub(super) struct Passe {
    /// Le mot de l'état (`mode`) et du registre des tâches.
    pub mode: &'static str,
    /// La clé de `settings` qui porte l'avancement.
    pub cle_etat: &'static str,
    /// Une passe vit dans CE processus. Un état `running` sans drapeau levé
    /// est une passe coupée par un arrêt du serveur : `interrupted`.
    pub en_vol: &'static AtomicBool,
    pub libelle: &'static str,
    /// La raison écrite quand le disjoncteur saute.
    pub raison_panne: &'static str,
}

static BPM_EN_VOL: AtomicBool = AtomicBool::new(false);
static DISCOGS_EN_VOL: AtomicBool = AtomicBool::new(false);

pub(super) static PASSE_BPM: Passe = Passe {
    mode: "bpm",
    cle_etat: "bpm_lot_status",
    en_vol: &BPM_EN_VOL,
    libelle: "Estimation des BPM de la bibliothèque…",
    raison_panne: "bpm_en_panne",
};

pub(super) static PASSE_DISCOGS: Passe = Passe {
    mode: "discogs",
    cle_etat: "discogs_lot_status",
    en_vol: &DISCOGS_EN_VOL,
    libelle: "Propositions Discogs de la bibliothèque…",
    raison_panne: "discogs_injoignable",
};

/// Le coût annoncé d'une piste pour le BPM : 75 s de signal décodé et
/// rééchantillonné à 22 050 Hz mono, ou une lecture Wikidata. Ordre de
/// grandeur, à remplacer par la mesure du banc de la PR.
const SECONDES_PAR_PISTE_BPM: f64 = 1.0;

/// Le coût annoncé d'un album pour Discogs : une recherche et une fiche au
/// limiteur de 0,9 requête/s, deux recherches et deux fiches au pire.
const SECONDES_PAR_ALBUM_DISCOGS: f64 = 2.3;

// ── Sélections ──────────────────────────────────────────────────────────────

/// Une piste à estimer : locale, un fichier (pas une URL), sans BPM balisé et
/// sans estimation gardée. La sélection des albums et le compte des pistes
/// partagent CE prédicat.
const PISTE_SANS_BPM: &str = "COALESCE(t.source, 'local') = 'local' \
     AND t.file_path IS NOT NULL AND t.file_path <> '' \
     AND t.file_path NOT LIKE '%://%' \
     AND (t.bpm IS NULL OR t.bpm <= 0) \
     AND NOT EXISTS (SELECT 1 FROM bpm_estimes b WHERE b.file_path = t.file_path)";

/// Albums locaux qui ont au moins une piste à estimer, après le curseur. La
/// deuxième colonne compte ces pistes, pour la durée annoncée.
pub(super) fn sql_candidats_bpm() -> String {
    format!(
        "SELECT al.id, \
           (SELECT COUNT(*) FROM tracks t WHERE t.album_id = al.id AND {PISTE_SANS_BPM}) \
         FROM albums al \
         WHERE COALESCE(al.source, 'local') = 'local' \
           AND al.id > ? \
           AND EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = al.id AND {PISTE_SANS_BPM}) \
         ORDER BY al.id"
    )
}

/// Un album sans pressage MusicBrainz, local.
const ALBUM_SANS_MBID: &str = "(al.musicbrainz_release_id IS NULL OR TRIM(al.musicbrainz_release_id) = '') \
     AND COALESCE(al.source, 'local') = 'local'";

/// Un album qui porte un code-barres ou un numéro de catalogue : la seule
/// entrée de la recherche Discogs.
const ALBUM_AVEC_CODE: &str = "((al.barcode IS NOT NULL AND TRIM(al.barcode) <> '') \
     OR (al.catalog_number IS NOT NULL AND TRIM(al.catalog_number) <> ''))";

/// Albums sans MBID, avec un code, et sans proposition FRAÎCHE ni tranchée :
/// une proposition acceptée ou rejetée n'est pas recherchée deux fois, une
/// reprise ne repaye pas les albums déjà servis, mais une proposition en
/// attente PÉRIMÉE (plus de six heures, conditions de Discogs) est relue.
///
/// `seuil` vient de [`discogs::seuil_de_fraicheur`] : une date que le serveur
/// fabrique lui-même, au format fixe de la table, jamais une saisie.
pub(super) fn sql_candidats_discogs(seuil: &str) -> String {
    debug_assert!(
        seuil
            .chars()
            .all(|c| c.is_ascii_digit() || "-:TZ".contains(c))
    );
    format!(
        "SELECT al.id FROM albums al \
         WHERE {ALBUM_SANS_MBID} AND {ALBUM_AVEC_CODE} \
           AND NOT EXISTS (SELECT 1 FROM propositions_discogs p WHERE p.album_id = al.id \
             AND (p.statut <> 'proposee' OR p.cree_le >= '{seuil}')) \
           AND al.id > ? \
         ORDER BY al.id"
    )
}

/// Les albums sans MBID que Discogs ne peut pas servir : ni code-barres ni
/// numéro de catalogue. Annoncés, pour qu'une passe à zéro album se comprenne.
pub(super) fn sql_sans_code_discogs() -> String {
    format!("SELECT COUNT(*) FROM albums al WHERE {ALBUM_SANS_MBID} AND NOT {ALBUM_AVEC_CODE}")
}

// ── État ────────────────────────────────────────────────────────────────────

/// Ce qu'un album a rendu.
#[derive(Debug, Default)]
pub(super) struct Issue {
    pub compte: Vec<(&'static str, u64)>,
    /// Le service n'a pas répondu (compte pour le disjoncteur).
    pub panne: bool,
    /// Arrêter la passe tout de suite, avec cette raison.
    pub arret: Option<&'static str>,
}

/// L'avancement d'une passe.
#[derive(Debug, Default, Clone)]
pub(super) struct Avancement {
    pub total: usize,
    pub traites: usize,
    pub dernier_album_id: i64,
    pub compte: BTreeMap<&'static str, u64>,
    /// Ce que la sélection annonce (pistes à traiter, albums sans code…).
    pub annonce: BTreeMap<&'static str, u64>,
}

fn ecrire_etat(
    backend: &Arc<dyn DbBackend>,
    passe: &Passe,
    task_id: &str,
    status: &str,
    a: &Avancement,
    raison: Option<&str>,
) {
    let corps = json!({
        "status": status,
        "mode": passe.mode,
        "task_id": task_id,
        "total": a.total,
        "traites": a.traites,
        "dernier_album_id": a.dernier_album_id,
        "compte": a.compte,
        "annonce": a.annonce,
        "raison": raison,
    });
    if let Err(e) =
        SettingsRepo::with_backend(backend.clone()).set(passe.cle_etat, &corps.to_string())
    {
        warn!(mode = passe.mode, erreur = %e, "passe_metarust_etat_non_ecrit");
    }
}

fn lire_etat(backend: &Arc<dyn DbBackend>, passe: &Passe) -> Option<Value> {
    SettingsRepo::with_backend(backend.clone())
        .get(passe.cle_etat)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
}

/// L'état servi : celui de la base, `interrupted` si la passe n'est plus en
/// vol, et la pause du registre des tâches.
pub(super) fn etat_servi(backend: &Arc<dyn DbBackend>, passe: &Passe) -> Value {
    let mut e = lire_etat(backend, passe).unwrap_or_else(|| {
        json!({
            "status": "idle", "mode": passe.mode, "total": 0, "traites": 0,
            "dernier_album_id": 0, "compte": {}, "annonce": {}, "raison": Value::Null,
        })
    });
    if e["status"] == "running" && !passe.en_vol.load(Ordering::SeqCst) {
        e["status"] = json!("interrupted");
    }
    e["en_pause"] = json!(est_en_pause(Tache::Enrichissement));
    e
}

/// Le curseur de reprise : le dernier album traité d'une passe arrêtée ou
/// coupée ; 0 pour une passe neuve ou terminée.
pub(super) fn curseur_de_reprise(etat: &Value) -> i64 {
    if matches!(
        etat["status"].as_str(),
        Some("stopped") | Some("interrupted")
    ) {
        etat["dernier_album_id"].as_i64().unwrap_or(0)
    } else {
        0
    }
}

/// Lève le drapeau « en vol » le temps de la passe, et le baisse même sur
/// panique.
struct EnVol(&'static AtomicBool);
impl Drop for EnVol {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// La boucle commune aux deux passes : frontière de pause, l'album, les
/// comptes, le disjoncteur.
pub(super) async fn derouler<F, Fut>(
    backend: Arc<dyn DbBackend>,
    passe: &'static Passe,
    task_id: String,
    albums: Vec<i64>,
    mut a: Avancement,
    mut traiter: F,
) -> Avancement
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = Issue>,
{
    let mut pannes = 0u32;
    for album_id in albums {
        // La frontière de pause : l'album précédent est écrit, rien n'est en
        // vol. La passe se gare et repart au même album.
        taches_de_fond::attendre_la_reprise(Tache::Enrichissement).await;
        let issue = traiter(album_id).await;
        a.traites += 1;
        a.dernier_album_id = album_id;
        for (cle, n) in issue.compte {
            *a.compte.entry(cle).or_default() += n;
        }
        if let Some(raison) = issue.arret {
            warn!(mode = passe.mode, task_id = %task_id, raison, "passe_metarust_arretee");
            ecrire_etat(&backend, passe, &task_id, "stopped", &a, Some(raison));
            return a;
        }
        pannes = if issue.panne { pannes + 1 } else { 0 };
        if pannes >= PANNES_CONSECUTIVES_MAX {
            warn!(mode = passe.mode, task_id = %task_id, pannes, "passe_metarust_disjoncteur");
            ecrire_etat(
                &backend,
                passe,
                &task_id,
                "stopped",
                &a,
                Some(passe.raison_panne),
            );
            return a;
        }
        if a.traites.is_multiple_of(ALBUMS_PAR_ECRITURE) {
            ecrire_etat(&backend, passe, &task_id, "running", &a, None);
        }
    }
    info!(mode = passe.mode, task_id = %task_id, traites = a.traites, compte = ?a.compte, "passe_metarust_terminee");
    ecrire_etat(&backend, passe, &task_id, "done", &a, None);
    a
}

/// Les refus communs aux deux routes, AVANT toute sélection.
fn refus_commun(state: &AppState, passe: &Passe) -> Option<(StatusCode, Json<Value>)> {
    if est_en_pause(Tache::Enrichissement) {
        return Some((
            StatusCode::CONFLICT,
            Json(json!({
                "code": "enrichissement_en_pause",
                "error": "enrichissement_en_pause",
                "message": "L'enrichissement est suspendu. Reprenez-le avant de lancer la passe.",
                "reprendre": "POST /system/background-tasks/enrichment/resume",
            })),
        ));
    }
    if passe.en_vol.load(Ordering::SeqCst) {
        return Some((
            StatusCode::CONFLICT,
            Json(json!({
                "code": "passe_deja_en_cours",
                "error": "passe_deja_en_cours",
                "mode": passe.mode,
                "etat": etat_servi(&state.backend, passe),
            })),
        ));
    }
    None
}

/// Sélectionne (avant le `202` : une panne SQL n'est pas une bibliothèque
/// déjà traitée, #3810), puis lance la boucle.
async fn lancer<T, Fut>(
    state: AppState,
    passe: &'static Passe,
    sql: String,
    annonce_sql: Option<(&'static str, String)>,
    secondes: impl Fn(&[Vec<tune_core::db::backend::SqlValue>]) -> f64,
    traiter: T,
) -> (StatusCode, Json<Value>)
where
    T: FnMut(i64) -> Fut + Send + 'static,
    Fut: Future<Output = Issue> + Send + 'static,
{
    let apres = curseur_de_reprise(&etat_servi(&state.backend, passe));
    let b = state.backend.clone();
    let selection = tokio::task::spawn_blocking(move || {
        let lignes = b.query_many(&sql, &[&apres as &dyn ToSqlValue])?;
        let annonce = match annonce_sql {
            Some((cle, q)) => Some((
                cle,
                b.query_one(&q, &[])?
                    .and_then(|r| r.first().and_then(|v| v.as_i64()))
                    .unwrap_or(0),
            )),
            None => None,
        };
        Ok::<_, String>((lignes, annonce))
    })
    .await;
    let (lignes, annonce) = match selection {
        Ok(Ok(v)) => v,
        erreur => {
            warn!(mode = passe.mode, error = ?erreur, "passe_metarust_selection_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"code": "candidats_indisponibles", "error": "candidats_indisponibles"}),
                ),
            );
        }
    };
    // Le drapeau se lève APRÈS la sélection et avant le 202 ; un second clic
    // concurrent perd la course et reçoit le 409.
    if passe
        .en_vol
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return refus_commun(&state, passe).unwrap_or((
            StatusCode::CONFLICT,
            Json(json!({"code": "passe_deja_en_cours", "error": "passe_deja_en_cours"})),
        ));
    }
    let en_vol = EnVol(passe.en_vol);

    let albums: Vec<i64> = lignes
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let duree_estimee_s = secondes(&lignes).round() as i64;
    let mut a = Avancement {
        total: albums.len(),
        dernier_album_id: apres,
        ..Default::default()
    };
    if passe.mode == "bpm" {
        let pistes: i64 = lignes
            .iter()
            .filter_map(|r| r.get(1).and_then(|v| v.as_i64()))
            .sum();
        a.annonce.insert("pistes", pistes.max(0) as u64);
    }
    if let Some((cle, n)) = annonce {
        a.annonce.insert(cle, n.max(0) as u64);
    }
    let task_id = uuid::Uuid::new_v4().to_string();
    info!(mode = passe.mode, task_id = %task_id, total = a.total, apres, duree_estimee_s, "passe_metarust_demarree");
    ecrire_etat(&state.backend, passe, &task_id, "running", &a, None);

    let reponse = json!({
        "status": "started",
        "mode": passe.mode,
        "task_id": task_id,
        "total": a.total,
        "annonce": a.annonce,
        "reprise_apres_album_id": apres,
        "duree_estimee_s": duree_estimee_s,
        "statut": format!("GET /library/{}-all/status", passe.mode),
        "suspendre": "POST /system/background-tasks/enrichment/pause",
    });
    let garde =
        state
            .background_tasks
            .begin(format!("{}_lot", passe.mode), passe.libelle, "enrichment");
    let backend = state.backend.clone();
    tokio::spawn(async move {
        let _en_vol = en_vol;
        let _garde = garde;
        derouler(backend, passe, task_id, albums, a, traiter).await;
    });
    (StatusCode::ACCEPTED, Json(reponse))
}

// ── BPM ─────────────────────────────────────────────────────────────────────

/// `GET /library/bpm-all/status`
pub(super) async fn bpm_lot_status(State(state): State<AppState>) -> Json<Value> {
    Json(etat_servi(&state.backend, &PASSE_BPM))
}

/// `POST /library/bpm-all`
pub(super) async fn bpm_lot_start(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if let Some(refus) = refus_commun(&state, &PASSE_BPM) {
        return refus;
    }
    let st = state.clone();
    lancer(
        state,
        &PASSE_BPM,
        sql_candidats_bpm(),
        None,
        |lignes| {
            lignes
                .iter()
                .filter_map(|r| r.get(1).and_then(|v| v.as_i64()))
                .sum::<i64>() as f64
                * SECONDES_PAR_PISTE_BPM
        },
        move |album_id| {
            let st = st.clone();
            async move {
                let b = super::enrichissement_metarust::estimer_les_bpm(&st, album_id, false, true)
                    .await;
                Issue {
                    compte: vec![
                        ("pistes_wikidata", b.wikidata as u64),
                        ("pistes_signal", b.signal as u64),
                        ("pistes_sans_resultat", b.sans_resultat as u64),
                        ("pistes_balisees", b.balises as u64),
                        ("pistes_deja_estimees", b.deja_estimees as u64),
                    ],
                    ..Default::default()
                }
            }
        },
    )
    .await
}

// ── Discogs ─────────────────────────────────────────────────────────────────

/// `GET /library/discogs-all/status`
pub(super) async fn discogs_lot_status(State(state): State<AppState>) -> Json<Value> {
    Json(etat_servi(&state.backend, &PASSE_DISCOGS))
}

/// L'issue d'une recherche Discogs, comptée. `derniere_erreur` est le code
/// rendu par le transport (`status_401`…), jamais le jeton.
pub(super) fn issue_discogs(
    r: Result<Option<discogs::Proposition>, Empechement>,
    derniere_erreur: Option<&str>,
) -> Issue {
    let un = |cle| Issue {
        compte: vec![(cle, 1)],
        ..Default::default()
    };
    match r {
        Ok(Some(_)) => un("propositions"),
        Ok(None) => un("sans_resultat"),
        Err(Empechement::SansCode) => un("sans_code"),
        Err(Empechement::DejaIdentifie) => un("deja_identifies"),
        Err(Empechement::AlbumInconnu) => un("introuvables"),
        Err(Empechement::Panne) => {
            let jeton_refuse = matches!(derniere_erreur, Some("status_401") | Some("status_403"));
            Issue {
                compte: vec![("pannes", 1)],
                panne: true,
                arret: jeton_refuse.then_some("discogs_jeton_refuse"),
            }
        }
    }
}

/// `POST /library/discogs-all`
pub(super) async fn discogs_lot_start(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if let Some(refus) = refus_commun(&state, &PASSE_DISCOGS) {
        return refus;
    }
    let st = state.clone();
    let jeton = tokio::task::spawn_blocking(move || crate::routes::discogs::discogs_token(&st))
        .await
        .ok()
        .flatten();
    // La recherche par code-barres exige un jeton (API Discogs) : même refus
    // que la fiche.
    let Some(jeton) = jeton else {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({
                "code": "discogs_token_absent",
                "error": "discogs_token not configured",
                "reglage": "Réglages ▸ Accès ▸ Clés d'API",
            })),
        );
    };
    let backend = state.backend.clone();
    let client = state.http_client.clone();
    lancer(
        state,
        &PASSE_DISCOGS,
        sql_candidats_discogs(&discogs::seuil_de_fraicheur(chrono::Utc::now())),
        Some(("albums_sans_code", sql_sans_code_discogs())),
        |lignes| lignes.len() as f64 * SECONDES_PAR_ALBUM_DISCOGS,
        move |album_id| {
            let backend = backend.clone();
            let client = client.clone();
            let jeton = jeton.clone();
            async move {
                let erreur: Arc<Mutex<Option<String>>> = Arc::default();
                let e = erreur.clone();
                let r = discogs::chercher_par(&backend, album_id, move |url| {
                    let e = e.clone();
                    let fut = super::enrichissement_metarust::lire_discogs(
                        client.clone(),
                        Some(jeton.clone()),
                        url,
                    );
                    async move {
                        let r = fut.await;
                        if let Err(code) = &r
                            && let Ok(mut g) = e.lock()
                        {
                            *g = Some(code.clone());
                        }
                        r
                    }
                })
                .await;
                let derniere = erreur.lock().ok().and_then(|g| g.clone());
                issue_discogs(r, derniere.as_deref())
            }
        },
    )
    .await
}

#[cfg(test)]
#[path = "passes_metarust_tests.rs"]
mod tests;
