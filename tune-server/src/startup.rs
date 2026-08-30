use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tracing::{info, warn};

use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Témoin déposé le temps du balayage ASIO de démarrage, dans le dossier de
/// données (celui du journal). Sa présence au démarrage suivant signifie que le
/// balayage précédent n'est jamais revenu — le processus est mort dedans.
const ASIO_WARM_SENTINEL: &str = "asio-warm.pending";

/// Variable d'environnement de secours pour couper le balayage ASIO du
/// démarrage sans toucher au fichier témoin.
const ASIO_WARM_DISABLE_ENV: &str = "TUNE_DISABLE_ASIO_SCAN";

/// Ce que le démarrage doit faire du balayage ASIO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsioWarmDecision {
    /// Rien ne s'y oppose : on énumère.
    Run,
    /// Coupé explicitement par l'environnement.
    SkippedByEnv,
    /// Un balayage précédent a emporté le processus : on ne recommence pas.
    SkippedAfterCrash,
}

/// État exposé aux diagnostics et à l'interface.
///
/// Le chemin reste fourni : c'est la seule pièce qui permettait jusque-là de
/// réparer le blocage à la main, et il est utile dans un rapport de support.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AsioWarmStatus {
    pub(crate) supported: bool,
    pub(crate) state: &'static str,
    pub(crate) blocked_after_crash: bool,
    pub(crate) disabled_by_env: bool,
    pub(crate) can_rearm: bool,
    pub(crate) retry: &'static str,
    pub(crate) sentinel_path: String,
    pub(crate) message: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsioWarmRearm {
    Rearmed,
    AlreadyReady,
    Unsupported,
    DisabledByEnv,
}

/// Décide si le balayage ASIO de démarrage peut être tenté.
///
/// `list_asio_devices()` charge **en processus** chaque pilote ASIO tiers
/// enregistré sur la machine (COM + `ASIOInit`). Un pilote fautif — pilote
/// ASIO d'un DAC débranché, ASIO4ALL, résidu de station de travail audio —
/// tue le processus au niveau natif : pas de panique Rust, donc pas de
/// `tune-crash.log`, et le serveur disparaît sans le moindre message.
/// Depuis la 0.9.45 ce balayage tourne à **chaque** démarrage : sur une
/// machine dont un pilote plante, Tune devient définitivement inutilisable
/// (Alain Bonnel, fil forum 1313 / #1283 : 0.9.44 démarre, tout ce qui suit
/// meurt une centaine de millisecondes après `com_sta_initialized`, sur plus
/// de trente lancements d'affilée).
///
/// Le témoin transforme cette panne définitive en panne d'un seul démarrage.
pub(crate) fn asio_warm_decision(sentinel: &Path, disabled_by_env: bool) -> AsioWarmDecision {
    if disabled_by_env {
        return AsioWarmDecision::SkippedByEnv;
    }
    if sentinel.exists() {
        return AsioWarmDecision::SkippedAfterCrash;
    }
    AsioWarmDecision::Run
}

/// Chemin du témoin : à côté du journal, donc `%LOCALAPPDATA%\TuneServer` sous
/// Windows — le dossier que l'on demande déjà aux testeurs d'ouvrir.
fn asio_warm_sentinel_path() -> PathBuf {
    crate::config::default_log_file_path()
        .parent()
        .map(|dir| dir.join(ASIO_WARM_SENTINEL))
        .unwrap_or_else(|| PathBuf::from(ASIO_WARM_SENTINEL))
}

fn asio_warm_status_at(sentinel: &Path, disabled_by_env: bool, supported: bool) -> AsioWarmStatus {
    let blocked_after_crash = supported && sentinel.exists();
    let (state, can_rearm, retry, message) = if !supported {
        (
            "unsupported",
            false,
            "none",
            "Le préchauffage ASIO ne concerne que Windows.",
        )
    } else if disabled_by_env {
        (
            "disabled_by_env",
            false,
            "remove_environment_override",
            "Le balayage ASIO est désactivé par TUNE_DISABLE_ASIO_SCAN.",
        )
    } else if blocked_after_crash {
        (
            "blocked_after_crash",
            true,
            "rearm_then_restart",
            "Le balayage ASIO est suspendu après un plantage. Réarmez-le puis redémarrez Tune pour tenter une nouvelle fois.",
        )
    } else {
        (
            "ready",
            false,
            "next_restart",
            "Le balayage ASIO est autorisé au prochain démarrage.",
        )
    };

    AsioWarmStatus {
        supported,
        state,
        blocked_after_crash,
        disabled_by_env,
        can_rearm,
        retry,
        sentinel_path: sentinel.display().to_string(),
        message,
    }
}

pub(crate) fn asio_warm_status() -> AsioWarmStatus {
    asio_warm_status_at(
        &asio_warm_sentinel_path(),
        asio_warm_disabled_by_env(),
        cfg!(target_os = "windows"),
    )
}

fn rearm_asio_warm_scan_at(
    sentinel: &Path,
    disabled_by_env: bool,
    supported: bool,
) -> Result<AsioWarmRearm, String> {
    if !supported {
        return Ok(AsioWarmRearm::Unsupported);
    }
    // Un bouton ne doit jamais contourner un coupe-circuit posé par
    // l'exploitant. Tant que l'environnement le demande, le témoin reste là.
    if disabled_by_env {
        return Ok(AsioWarmRearm::DisabledByEnv);
    }
    match std::fs::remove_file(sentinel) {
        Ok(()) => Ok(AsioWarmRearm::Rearmed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(AsioWarmRearm::AlreadyReady)
        }
        Err(error) => Err(format!(
            "impossible de retirer le témoin ASIO {} : {error}",
            sentinel.display()
        )),
    }
}

/// Autorise une seule nouvelle tentative au prochain démarrage.
///
/// On ne relance pas l'énumération dans le processus courant : une sortie peut
/// déjà posséder le pilote et le re-sondage à chaud est précisément dangereux.
pub(crate) fn rearm_asio_warm_scan() -> Result<AsioWarmRearm, String> {
    rearm_asio_warm_scan_at(
        &asio_warm_sentinel_path(),
        asio_warm_disabled_by_env(),
        cfg!(target_os = "windows"),
    )
}

fn asio_warm_disabled_by_env() -> bool {
    std::env::var(ASIO_WARM_DISABLE_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Lance le préchauffage du cache ASIO, protégé par le témoin de plantage.
#[cfg(feature = "local-audio")]
fn spawn_asio_warm_scan() {
    // `list_asio_devices()` ne fait rien hors Windows : pas de témoin, pas de
    // thread, comportement inchangé sur macOS et Linux.
    if !cfg!(target_os = "windows") {
        return;
    }

    let sentinel = asio_warm_sentinel_path();
    let decision = asio_warm_decision(&sentinel, asio_warm_disabled_by_env());

    tokio::task::spawn_blocking(move || match decision {
        AsioWarmDecision::SkippedByEnv => {
            info!(
                env = ASIO_WARM_DISABLE_ENV,
                "asio_warm_scan_disabled_by_env"
            );
        }
        AsioWarmDecision::SkippedAfterCrash => {
            warn!(
                sentinel = %sentinel.display(),
                "asio_warm_scan_skipped_after_crash — the previous boot died while enumerating \
                 ASIO drivers, so the scan is disabled to keep the server startable. One of the \
                 machine's ASIO drivers is faulty. Delete this file to try again."
            );
        }
        AsioWarmDecision::Run => {
            if let Some(dir) = sentinel.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Déposé AVANT l'énumération : si un pilote emporte le processus,
            // le fichier reste et le démarrage suivant saute le balayage.
            let armed = std::fs::write(&sentinel, "asio warm scan in progress\n").is_ok();
            info!(armed, "asio_warm_scan_started");

            let devices = tune_core::outputs::local::list_asio_devices();

            let _ = std::fs::remove_file(&sentinel);
            info!(count = devices.len(), "asio_warm_scan_complete");
        }
    });
}

/// Restore zone volumes and playback positions from DB, persist config settings.
pub async fn init_state(state: &AppState, config: &TuneConfig) {
    // Turn any update markers left by a just-applied update into a persisted
    // last_update_result the UI can show. Catches a silent Windows bat-swap
    // failure (came back on the old binary) instead of it looking like a no-op.
    crate::routes::system::update::record_post_update_result(state);

    // Warm the ASIO device cache once at boot, while the audio devices are still
    // idle. An ASIO driver — notably SOtM Diretta — can't be re-enumerated once a
    // zone owns it for playback; `list_asio_devices` then serves this cache
    // instead of re-opening the driver. Without a warm pass, the cache stays
    // empty until someone opens the device list, so if auto-resume starts a zone
    // at boot first, the on-demand listing runs while the driver is busy and the
    // DAC never appears — the zone is stuck on the wrong output with no sound
    // (JP Borderies: SOtM DAC absent from the list). Enumerating here, before any
    // playback, captures it. Fire-and-forget; no-op off Windows / without `asio`.
    // `outputs::local` only exists under `local-audio` (the oaat-only CI build
    // compiles without it).
    #[cfg(feature = "local-audio")]
    spawn_asio_warm_scan();

    reset_zones_offline(state);
    marquer_enrichissements_interrompus(state);
    ouvrir_le_registre_des_executions(state);
    deduplicate_zones(state);
    ensure_zones_is_hidden(state);
    cleanup_orphan_queues(state);
    reconcile_favorites(state);
    deduplicate_radios(state);
    restore_zone_volumes(state).await;
    restore_playback_positions(state).await;
    restore_queues(state, config);
    restore_queue_metadata(state, config).await;
    restore_oaat_groups(state).await;
    persist_initial_settings(state, config);
    resolve_ytdlp(state).await;
    restore_convolvers(state).await;
    warm_sqlite_cache(state);

    // Re-register manually-added devices (BluOS, legacy DLNA renderers that
    // don't answer SSDP M-SEARCH). Done off the startup path so an offline
    // device's probe timeout doesn't delay boot.
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reregister_manual_devices(&state_clone).await;
        // Re-probe auto-discovered renderers whose lazy SSDP responder won't
        // resurface them after a restart (Cyrus Stream X2, #1126).
        crate::discovery_setup::reregister_known_renderers(&state_clone).await;
    });

    // Re-probe auto-discovered DLNA renderers from their persisted LOCATION,
    // so one with a lazy SSDP responder (Cyrus Stream X2) comes back online
    // after a restart without waiting for multicast (#1126). Runs concurrently
    // with SSDP; the registry is keyed by UUID so the first to win re-attaches
    // the zone and the other is a no-op.
    //
    // UNE SEULE fois. Ce bloc etait present en double, commentaire compris :
    // chaque appareil persiste etait sonde DEUX fois en parallele. Sur un
    // renderer eteint ou parti du reseau, cela doublait la sequence de
    // reprises — 16 tentatives au lieu de 8, ~3 minutes de tampons reseau et
    // de journal au demarrage (journal de JP Borderies, 0.9.83, ou chaque
    // ligne `discovered_dlna_reprobe_retry` apparait exactement deux fois).
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reprobe_persisted_dlna_devices(&state_clone).await;
    });
}

/// Reset all zones to offline at startup.  Discovery will set actually-present
/// devices back online.  This prevents stale "online" zones from accumulating
/// across restarts and hitting the free-tier zone limit.
fn reset_zones_offline(state: &AppState) {
    match state.backend.execute("UPDATE zones SET online = 0", &[]) {
        Ok(n) => {
            info!(count = n, "zones_reset_offline_at_startup");
        }
        Err(e) => {
            tracing::warn!(error = %e, "zones_reset_offline_failed");
        }
    }
}

/// Réglages d'avancement d'enrichissement dont l'état « en cours » est écrit en
/// base. Chacun ne connaît que deux écritures : `running` au lancement et à
/// chaque jalon, `done` à la fin NORMALE de la boucle.
const REGLAGES_AVANCEMENT_ENRICHISSEMENT: [&str; 3] = [
    "enrich_all_status",
    "artist_artwork_enrich_result",
    // Passe de fond « paroles » (#2172) : sans cette ligne, un arrêt en cours
    // de passe laisserait `status: "running"` en base pour toujours et le
    // bouton de relance grisé — exactement le défaut #2002. La constante
    // plutôt que le littéral : renommer la clé d'un côté ne peut plus
    // désynchroniser l'autre.
    tune_core::library::lyrics_pass::SETTING_FILL_RESULT,
];

/// Les mêmes, dans leur forme dégradée : une chaîne nue, écrite `running` et
/// jamais relue par personne aujourd'hui. On les neutralise quand même — un
/// mensonge permanent en base finit toujours par trouver un lecteur.
const DRAPEAUX_AVANCEMENT_ENRICHISSEMENT: [&str; 2] =
    ["artwork_enrich_status", "artist_artwork_enrich_status"];

/// Réécriture d'un avancement resté à `running`, ou `None` s'il n'y a rien à
/// faire (déjà terminé, illisible, ou pas un objet).
///
/// Rend `(json, traité, total)` — les deux compteurs pour le journal.
///
/// ⚠️ Deux champs sont neutralisés, pas un. Les clients **déjà livrés** ne
/// lisent pas le même : `status` pour l'enrichissement de métadonnées, `phase`
/// pour les images d'artistes (`if (!phase || phase === 'done') return;`).
/// N'en corriger qu'un ne débloquerait personne avant la prochaine version du
/// client — or ceux qui sont coincés le sont sur une version déjà publiée.
///
/// ⚠️ Les compteurs sont CONSERVÉS. « Interrompu à 5 650 / 16 261 » se
/// comprend ; un réglage effacé ne dirait plus rien du tout, et l'utilisateur
/// ne saurait pas où sa passe s'est arrêtée.
/// `pub(crate)` parce que le démarrage n'est plus le seul déclencheur : la fin
/// d'une passe d'images d'artistes qui s'est arrêtée sans écrire son `done`
/// applique la MÊME réécriture, tout de suite, sans attendre un redémarrage
/// (`routes::library::artwork::FinDePasseArtistes`, #2073). Deux règles
/// séparées auraient divergé au premier champ neutralisé.
pub(crate) fn avancement_interrompu(brut: &str) -> Option<(String, u64, u64)> {
    let mut valeur = match serde_json::from_str::<serde_json::Value>(brut) {
        Ok(v) => v,
        Err(_) => {
            // Réglage illisible : on n'y touche pas. Écraser ce qu'on ne
            // comprend pas serait pire que de le laisser.
            warn!("enrichissement_avancement_illisible");
            return None;
        }
    };
    if valeur.get("status").and_then(|v| v.as_str()) != Some("running") {
        return None;
    }
    let objet = valeur.as_object_mut()?;
    objet.insert("status".into(), serde_json::json!("interrupted"));
    // `phase` n'existe que sur les images d'artistes ; l'y poser à `done` est
    // ce qui empêche le client déjà livré de reprendre un suivi fantôme. Sur
    // les autres réglages la clé est simplement ajoutée, et ignorée.
    objet.insert("phase".into(), serde_json::json!("done"));
    let traite = objet
        .get("enriched")
        .or_else(|| objet.get("processed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = objet.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    Some((valeur.to_string(), traite, total))
}

/// Déclarer interrompue toute passe d'enrichissement que la base croit encore
/// en cours.
///
/// Une passe d'enrichissement vit dans un `tokio::spawn` de ce processus ; son
/// avancement, lui, est écrit en base — et il y reste. Le `done` de fin est
/// posé APRÈS la boucle : un redémarrage, un arrêt brutal ou une panique le
/// sautent, et le réglage affirme alors pour toujours qu'une passe tourne
/// pendant que le fil qui l'écrivait n'existe plus.
///
/// Ce n'est pas un défaut d'affichage. Le client fait confiance à cet état pour
/// reprendre le suivi à l'ouverture de l'écran (#1867) **et** désactive le
/// bouton de relance tant qu'il le lit `running` : l'utilisateur se retrouve
/// devant un bouton grisé portant « 5650/16261 pistes… » qu'aucun geste ne
/// débloque, et la seule action qui réparerait est précisément celle qu'on lui
/// interdit (Bilou, 0.9.90, #2002).
///
/// Le démarrage de ce processus est la seule preuve nécessaire : aucune passe
/// ne survit au processus qui la portait. Pas de délai de grâce, pas
/// d'horodatage à comparer — si on est ici, elles sont mortes.
///
/// La réécriture elle-même vit dans [`avancement_interrompu`], testée à part.
fn marquer_enrichissements_interrompus(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    for cle in REGLAGES_AVANCEMENT_ENRICHISSEMENT {
        let Ok(Some(brut)) = settings.get(cle) else {
            continue;
        };
        let Some((neuf, traite, total)) = avancement_interrompu(&brut) else {
            continue;
        };
        match settings.set(cle, &neuf) {
            Ok(()) => info!(
                cle,
                traite,
                total,
                "enrichissement_marque_interrompu — la passe ne survit pas au processus ; le bouton de relance est rendu à l'utilisateur"
            ),
            Err(e) => warn!(cle, error = %e, "enrichissement_marque_interrompu_echec"),
        }
    }

    for cle in DRAPEAUX_AVANCEMENT_ENRICHISSEMENT {
        if let Ok(Some(v)) = settings.get(cle)
            && v == "running"
            && let Err(e) = settings.set(cle, "interrupted")
        {
            warn!(cle, error = %e, "enrichissement_drapeau_interrompu_echec");
        }
    }
}

/// Rendre le registre des executions automatisees utilisable pour ce
/// demarrage (#2080).
///
/// Deux gestes, dans cet ORDRE, et l'ordre porte le raisonnement :
///
/// 1. **Clore les orphelines.** Toute execution que la base croit encore en
///    cours a ete ecrite par un processus qui n'existe plus — aucune passe ne
///    survit au processus qui la portait, et etre ici le prouve. C'est le
///    defaut #2002 transpose : un `running` fige a jamais verrouillait le
///    bouton de relance sur une passe morte. Ici on ne verrouille rien, mais un
///    registre qui affirme pour toujours « ca tourne » serait pire qu'un
///    registre vide.
/// 2. **Purger par age.** Apres, jamais avant : une orpheline tres ancienne est
///    d'abord fermee proprement, puis effacee si elle depasse la retention.
///    Dans l'autre ordre elle disparaitrait sans avoir jamais ete close, et le
///    nombre de fermetures journalise ne dirait plus la verite.
///
/// Ce doit etre le PREMIER contact avec le registre au demarrage : les passes
/// cablees ouvrent leurs lignes bien apres, depuis `spawn_background_tasks`.
fn ouvrir_le_registre_des_executions(state: &AppState) {
    let registre = tune_core::db::task_run_repo::TaskRunRepo::with_backend(state.backend.clone());

    match registre.clore_orphelines() {
        Ok(0) => {}
        Ok(n) => info!(
            closes = n,
            boot_id = tune_core::db::task_run_repo::boot_id(),
            "registre_executions_orphelines_closes — passes que la base croyait encore en cours"
        ),
        Err(e) => warn!(error = %e, "registre_executions_cloture_echouee"),
    }

    match registre.purger_par_age() {
        Ok(0) => {}
        Ok(n) => info!(
            effacees = n,
            jours = tune_core::db::task_run_repo::RETENTION_JOURS,
            "registre_executions_purge_par_age"
        ),
        Err(e) => warn!(error = %e, "registre_executions_purge_echouee"),
    }
}

/// Remove duplicate zones (same output_device_id) and add a unique index to
/// prevent future duplicates.  Must run before any discovery task starts.
fn deduplicate_zones(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    match zone_repo.deduplicate() {
        Ok(removed) if removed > 0 => {
            info!(removed, "zone_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "zone_dedup_failed");
        }
    }
    // Add a unique index on output_device_id (idempotent) so duplicate zones
    // can never be created again at the SQL level.
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_zones_output_device_id ON zones(output_device_id) WHERE output_device_id IS NOT NULL;"
    ) {
        tracing::warn!(error = %e, "zone_unique_index_failed");
    }
}

/// Re-rattache les favoris orphelins aux items vivants retrouvés par identité
/// (instantané titre/artiste/chemin, historique d'écoute en secours). Un
/// rescan qui recrée albums/pistes sous de nouveaux rowids (racines music
/// déplacées, library clear) laissait des favoris fantômes : cœurs éteints et
/// filtre « Favoris » vide (bug .18, v0.9.50). Au démarrage on ne supprime
/// JAMAIS un favori introuvable — un volume pas encore monté ou un scan à
/// venir peut encore le ramener ; seule la passe post-scan complet supprime.
fn reconcile_favorites(state: &AppState) {
    let reconciler = tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(
        state.backend.clone(),
    );
    match reconciler.run(false) {
        Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
            info!(
                scanned = stats.scanned,
                snapshots = stats.snapshots_backfilled,
                relinked = stats.relinked,
                deduplicated = stats.deduplicated,
                unresolved = stats.unresolved,
                "favorites_reconciled_at_startup"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "favorites_reconcile_failed"),
    }
    // Les albums masqués (#1391) suivent la MÊME mécanique d'instantané et
    // les mêmes règles de re-rattachement : au démarrage on ne supprime
    // jamais un marqueur introuvable — un volume pas encore monté peut encore
    // ramener l'album, et un album masqué qui réapparaîtrait visible est
    // précisément le bug que la table évite.
    match tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone())
        .reconcile(false)
    {
        Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
            info!(
                scanned = stats.scanned,
                relinked = stats.relinked,
                deduplicated = stats.deduplicated,
                unresolved = stats.unresolved,
                "hidden_albums_reconciled_at_startup"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "hidden_albums_reconcile_failed"),
    }
    // Les paires « ces deux albums ne sont pas des doublons » (#1276) suivent
    // la MÊME mécanique et la même règle : au démarrage on ne supprime jamais
    // une paire introuvable. Perdre l'arbitrage rouvrirait la fusion
    // destructrice de `merge-duplicates`, qui SUPPRIME la ligne perdante.
    match tune_core::db::album_distinct_repo::AlbumDistinctRepo::with_backend(state.backend.clone())
        .reconcile(false)
    {
        Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
            info!(
                scanned = stats.scanned,
                relinked = stats.relinked,
                deduplicated = stats.deduplicated,
                unresolved = stats.unresolved,
                "album_distinct_pairs_reconciled_at_startup"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "album_distinct_pairs_reconcile_failed"),
    }
}

fn cleanup_orphan_queues(state: &AppState) {
    let sqls = ["DELETE FROM queue_items WHERE zone_id NOT IN (SELECT id FROM zones)"];
    for sql in &sqls {
        match state.backend.execute(sql, &[]) {
            Ok(removed) if removed > 0 => {
                info!(removed, sql = *sql, "orphan_queue_records_cleaned");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "orphan_queue_cleanup_failed");
            }
        }
    }
}

fn ensure_zones_is_hidden(state: &AppState) {
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            // Try ALTER TABLE; ignore "duplicate column" error.
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN is_hidden INTEGER DEFAULT 0",
                &[],
            );
            match result {
                Ok(_) => info!("zones_is_hidden_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_is_hidden_column_add_failed"),
            }
        }
        tune_core::db::engine::Engine::Sqlite => {
            // Migration v38 handles this.
        }
    }

    // Ensure last_play_state column exists (migration v39 for SQLite,
    // idempotent ALTER for Postgres).
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN last_play_state TEXT DEFAULT 'stopped'",
                &[],
            );
            match result {
                Ok(_) => info!("zones_last_play_state_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_last_play_state_add_failed"),
            }
        }
        _ => {}
    }

    // Aucune zone ne peut être en lecture au démarrage : c'est ce processus qui
    // la produit. Un `playing` trouvé ici est donc forcément un reliquat — arrêt
    // brutal pendant une lecture, enceinte DLNA coupée au secteur, sondage
    // interrompu.
    //
    // Et ce reliquat coûte cher. `any_zone_playing` (replaygain.rs) lit CETTE
    // colonne pour décider si l'analyse acoustique et le ReplayGain doivent
    // s'effacer devant la lecture. Une zone figée à `playing` les bloque donc
    // **définitivement**, et comme la valeur est persistée, redémarrer n'y
    // change rien — trois signalements convergents (#1464 Bertrand, #1456 Bruno
    // Lescarret, #1457 Bilou), tous décrivant une jauge immobile « sans lecture
    // en cours », tous ayant redémarré en vain.
    //
    // Le symptôme est illisible parce que rien ne relie la cause à l'effet :
    // l'utilisateur ne joue rien, voit une analyse figée, et conclut à un
    // blocage du scan.
    match state.backend.execute(
        "UPDATE zones SET last_play_state = 'stopped' WHERE last_play_state = 'playing'",
        &[],
    ) {
        Ok(n) if n > 0 => {
            info!(zones = n, "zones_stale_playing_state_reset");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "zones_stale_playing_reset_failed"),
    }
}

fn deduplicate_radios(state: &AppState) {
    let dedup_sql = "DELETE FROM radio_stations WHERE id NOT IN (SELECT MIN(id) FROM radio_stations GROUP BY name, url)";
    match state.backend.execute(dedup_sql, &[]) {
        Ok(removed) if removed > 0 => {
            info!(removed, "radio_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "radio_dedup_failed");
        }
    }
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_radio_stations_name_url ON radio_stations(name, url);"
    ) {
        tracing::warn!(error = %e, "radio_unique_index_failed");
    }
}

/// Restore persisted queue snapshots from JSON files on disk.
fn restore_queues(state: &AppState, config: &TuneConfig) {
    tune_core::queue_persistence::restore_all_queues(&state.backend, &config.db_path);
}

/// After queues are restored into the DB, load snapshot metadata (repeat_mode,
/// shuffle, queue_length, current_position) into the PlaybackManager so the
/// poller's `next_position()` sees the correct values after a server restart.
async fn restore_queue_metadata(state: &AppState, config: &TuneConfig) {
    let snapshots = tune_core::queue_persistence::load_all_snapshots(&config.db_path);
    let queue_repo =
        tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone());

    for snap in &snapshots {
        let zone_id = snap.zone_id;

        // Determine queue length from DB (authoritative after restore_all_queues).
        let local_count = queue_repo.count(zone_id).unwrap_or(0);
        let streaming_count = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let queue_len = if local_count > 0 {
            local_count
        } else {
            streaming_count
        };

        if queue_len > 0 {
            state
                .playback
                .update_queue_info(zone_id, snap.current_position, queue_len)
                .await;
        }

        // Restore repeat mode
        let repeat = match snap.repeat_mode.as_str() {
            "one" => tune_core::playback::RepeatMode::One,
            "all" => tune_core::playback::RepeatMode::All,
            _ => tune_core::playback::RepeatMode::Off,
        };
        state.playback.set_repeat(zone_id, repeat).await;

        // Restore shuffle
        state.playback.set_shuffle(zone_id, snap.shuffle).await;

        info!(
            zone_id,
            queue_len,
            position = snap.current_position,
            repeat_mode = %snap.repeat_mode,
            shuffle = snap.shuffle,
            "queue_metadata_restored"
        );
    }
}

async fn restore_convolvers(state: &AppState) {
    #[cfg(not(feature = "local-audio"))]
    let _ = state;
    #[cfg(feature = "local-audio")]
    {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        if let Ok(zones) = zone_repo.list() {
            for zone in &zones {
                let Some(zone_id) = zone.id else { continue };
                let key = format!("ir_path_{zone_id}");
                if let Ok(Some(ir_path)) = settings.get(&key) {
                    if !std::path::Path::new(&ir_path).exists() {
                        continue;
                    }
                    let device_id = zone.output_device_id.as_deref().unwrap_or("");
                    if !device_id.starts_with("local:") {
                        continue;
                    }
                    let outputs = state.outputs.lock().await;
                    if let Some(output) = outputs.get(device_id) {
                        let output = output.lock().await;
                        if let Some(local) = output
                            .as_any()
                            .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                        {
                            match local.set_convolver_ir(&ir_path) {
                                Ok(()) => {
                                    info!(zone_id, ir_path = %ir_path, "convolver_restored")
                                }
                                Err(e) => {
                                    warn!(zone_id, error = %e, "convolver_restore_failed")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Touch key tables so SQLite page cache is warm for the first UI load.
fn warm_sqlite_cache(state: &AppState) {
    use tune_core::db::{album_repo::AlbumRepo, artist_repo::ArtistRepo, track_repo::TrackRepo};
    let _ = TrackRepo::with_backend(state.backend.clone()).count();
    let _ = AlbumRepo::with_backend(state.backend.clone()).count();
    let _ = ArtistRepo::with_backend(state.backend.clone()).count();
    info!("sqlite_cache_warmed");
}

/// Initialize PlaybackManager volume from DB-stored zone volumes and mark devices offline.
///
/// Une zone stockée à 100 % était ramenée à 20 % ici, « garde-fou contre un
/// réveil à plein volume » (2fdc2b5e, collatéral d'un défaut DLNA où le poller
/// écrivait 100 en base pour un renderer à sortie fixe). Ce garde-fou ne
/// protégeait de rien : `PlaybackManager::set_volume` n'écrit ni la base ni la
/// sortie. Il laissait trois valeurs pour une seule zone — base 100, mémoire
/// 0.2, `LocalOutput::user_volume` 1.0 — et envoyait un événement `volume: 0.2`
/// que personne n'avait demandé (les 20 % de #1504 et #1480, attribués à tort
/// au défaut 50 % de `ZoneState::default()` dans #1548).
///
/// La cause d'origine est traitée à la source : le poller ignore désormais un
/// renderer qui annonce 100 % (`status.volume < 0.999`), donc un 100 % en base
/// est aujourd'hui un choix de l'utilisateur. Et la vraie protection contre le
/// réveil à plein volume est dans `register_local_outputs`, qui ensemence la
/// sortie avec la valeur stockée — celle-là agit sur le son. Refs #1596.
async fn restore_zone_volumes(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            if let Some(id) = zone.id {
                let vol = (zone.volume as f64) / 100.0;
                if zone.fixed_volume {
                    // Contrat « Volume fixe (bit-perfect) » : 100 % est un
                    // ENGAGEMENT, pas un oubli — le DoP meurt au moindre gain
                    // logiciel (les marqueurs 0x05/0xFA ne survivent pas à une
                    // multiplication). Le garde-fou ci-dessous rabaissait ces
                    // zones à 20 % à chaque redémarrage, en mémoire seulement :
                    // la base disait 100, l'effectif était 0.2, et le DSD de
                    // Cyrille ressortait en grésillement alors que tous ses
                    // réglages étaient bons (forum 1320, #1504 pour le
                    // désaccord d'affichage).
                    state.playback.set_volume(id, 1.0).await;
                    info!(zone_id = id, zone_name = %zone.name, "zone_volume_fixed_restored_full");
                } else {
                    state.playback.set_volume(id, vol).await;
                    info!(zone_id = id, zone_name = %zone.name, volume = vol, "zone_volume_restored");
                }
            }
        }
    }
}

/// Restore last playback positions from DB so the UI shows where playback left off.
async fn restore_playback_positions(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            let Some(zone_id) = zone.id else { continue };
            if zone.last_position_ms == 0
                && zone.last_track_id.is_none()
                && zone.last_track_source.as_deref() != Some("radio")
            {
                continue;
            }
            let np = if let Some(track_id) = zone.last_track_id {
                if let Ok(Some(track)) = track_repo.get(track_id) {
                    // Restore the source/source_id from the *zone* row (the
                    // saved playback origin), not the library row — a track may
                    // have been played from a streaming source.
                    tune_core::playback::NowPlaying {
                        source: zone
                            .last_track_source
                            .clone()
                            .unwrap_or_else(|| "local".into()),
                        source_id: zone.last_track_source_id.clone(),
                        ..tune_core::playback::NowPlaying::from_track(&track)
                    }
                } else {
                    continue;
                }
            } else if zone.last_track_source.as_deref() == Some("radio") {
                continue;
            } else {
                continue;
            };
            let clamped_pos = if np.duration_ms > 0 {
                zone.last_position_ms
                    .min(np.duration_ms.saturating_sub(1000))
            } else {
                zone.last_position_ms
            };
            let dur = np.duration_ms;
            state
                .playback
                .restore_position(zone_id, clamped_pos, np)
                .await;
            info!(
                zone_id,
                zone_name = %zone.name,
                position_ms = clamped_pos,
                original_ms = zone.last_position_ms,
                duration_ms = dur,
                track_id = ?zone.last_track_id,
                "playback_position_restored"
            );
        }
    }
}

/// Restore persisted OAAT multiroom groups from the settings DB.
#[cfg(feature = "oaat")]
async fn restore_oaat_groups(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups_json = settings
        .get("oaat_groups")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".into());
    let mut groups: Vec<serde_json::Value> = serde_json::from_str(&groups_json).unwrap_or_default();

    let mut restored = 0usize;
    let mut to_probe: Vec<(String, String, Vec<(String, u16)>)> = Vec::new();
    for group in &groups {
        let id = match group["id"].as_str() {
            Some(id) => id.to_string(),
            None => continue,
        };
        let name = group["name"].as_str().unwrap_or("OAAT Group").to_string();
        let endpoints: Vec<(String, u16)> = group["endpoints"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|ep| {
                let host = ep["host"].as_str()?.to_string();
                let port = ep["port"].as_u64()? as u16;
                Some((host, port))
            })
            .collect();

        if endpoints.is_empty() {
            continue;
        }

        let output = tune_core::outputs::oaat::OaatMultiroomOutput::new(
            name.clone(),
            id.clone(),
            endpoints.clone(),
        );
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(output));
        drop(outputs);

        info!(group_id = %id, name = %name, endpoints = endpoints.len(), "oaat_group_restored");
        to_probe.push((id.clone(), name.clone(), endpoints));
        restored += 1;
    }

    // Les sondes partent HORS du chemin de démarrage (#1779).
    //
    // `restore_oaat_groups` est appelée par `init_state`, donc en série avec le
    // reste du démarrage : une sonde de 1,5 s par groupe injoignable retarderait
    // d'autant l'ouverture du serveur. C'est exactement la raison pour laquelle
    // les re-sondages d'appareils juste en dessous sont déjà déportés dans une
    // tâche — « so an offline device's probe timeout doesn't delay boot ».
    //
    // Et on ne REFUSE pas ici, contrairement à la création. À la création, un
    // membre injoignable est forcément un appareil qui n'est pas un point de
    // diffusion Tune : l'utilisateur vient de le choisir, il est allumé, il est
    // devant lui. Au démarrage, la même sonde ne distingue plus « renderer DLNA,
    // ne marchera jamais » de « point de diffusion pas encore démarré » — et le
    // serveur démarre souvent avant le reste de l'installation. Refuser
    // casserait un groupe valide dont un membre a mis dix secondes de plus.
    //
    // On enregistre donc toujours, puis on inscrit le constat dans l'entrée
    // persistée : `list_oaat_groups` la renvoie telle quelle, donc l'interface
    // peut enfin dire « ce membre ne répond pas » au lieu de laisser l'échec
    // dans un journal que personne ne lit.
    if !to_probe.is_empty() {
        let state = state.clone();
        tokio::spawn(async move {
            let mut results: Vec<(String, Vec<String>)> = Vec::new();
            for (id, name, endpoints) in &to_probe {
                let unreachable =
                    crate::routes::zone_manager::unreachable_endpoints(endpoints).await;
                if !unreachable.is_empty() {
                    warn!(
                        group_id = %id,
                        name = %name,
                        unreachable = unreachable.join(", "),
                        total = endpoints.len(),
                        "oaat_group_has_unreachable_endpoints — ce groupe ne pourra pas jouer tant que ces membres ne repondent pas sur leur port OAAT"
                    );
                }
                results.push((id.clone(), unreachable));
            }

            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            // Relu MAINTENANT, pas au démarrage : entre-temps l'utilisateur a pu
            // créer ou supprimer un groupe, et réécrire une copie vieille de
            // quelques secondes les effacerait.
            let mut groups: Vec<serde_json::Value> = settings
                .get("oaat_groups")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let mut changed = false;
            for g in groups.iter_mut() {
                let Some(gid) = g["id"].as_str().map(|s| s.to_string()) else {
                    continue;
                };
                if let Some((_, unreachable)) = results.iter().find(|(id, _)| *id == gid) {
                    g["unreachable_endpoints"] = serde_json::json!(unreachable);
                    g["probed_at"] = serde_json::json!(crate::routes::zone_manager::now_iso());
                    changed = true;
                }
            }
            if changed {
                settings
                    .set(
                        "oaat_groups",
                        &serde_json::to_string(&groups).unwrap_or_else(|_| "[]".into()),
                    )
                    .ok();
            }
        });
    }

    if restored > 0 {
        info!(count = restored, "oaat_groups_restore_complete");
    }
}

#[cfg(not(feature = "oaat"))]
async fn restore_oaat_groups(_state: &AppState) {}

/// Create the OpenHome event listener (shared between SSDP handler and outputs).
pub async fn create_oh_listener() -> Option<Arc<OpenHomeEventListener>> {
    let server_ip = tune_core::discovery::ssdp::get_local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".into());
    match OpenHomeEventListener::new(server_ip).await {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            tracing::warn!(error = %e, "oh_event_listener_init_failed");
            None
        }
    }
}

/// Persist music_dirs and discogs_token from config/env into the settings DB.
fn persist_initial_settings(state: &AppState, config: &TuneConfig) {
    if !config.music_dirs.is_empty() {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        // Seed music_dirs from config ONLY on first run — never clobber a list
        // the user has since edited in Settings. Overwriting on every boot meant
        // a too-broad folder removed via the UI (e.g. C:\ = the whole drive)
        // reappeared on the next restart, so it could never be removed and the
        // temp dir kept being re-scanned (Frédéric). Mirrors the discogs_token
        // first-run guard below. An explicit empty list ("[]") counts as set, so
        // "remove everything" is respected.
        let already_set = settings.get("music_dirs").ok().flatten().is_some();
        if !already_set {
            let normalized_dirs: Vec<String> = config
                .music_dirs
                .iter()
                .map(|d| tune_core::scanner::walker::normalize_path(d))
                .filter(|d| !d.is_empty())
                .collect();
            settings
                .set(
                    "music_dirs",
                    &serde_json::to_string(&normalized_dirs).unwrap(),
                )
                .ok();
        }
    }

    if let Some(ref token) = config.discogs_token {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let already_set = settings
            .get("discogs_token")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !already_set {
            settings.set("discogs_token", token).ok();
            info!("discogs_token_persisted_from_env");
        }
    }

    // Mirror the Last.fm API key/secret from env into the settings DB. The whole
    // scrobbling flow (auth.getSession exchange in service_tokens.rs, and the
    // scrobbler in orchestrator.rs) reads these from the settings table, not from
    // config — so a user who only set TUNE_LASTFM_API_KEY/SECRET in .env got
    // "lastfm_api_key not configured" and no scrobbling, even though the keys were
    // loaded (forum #1113). Read straight from env (the server TuneConfig does not
    // carry Last.fm) and persist once when absent, exactly like discogs_token.
    for (env_var, key) in [
        ("TUNE_LASTFM_API_KEY", "lastfm_api_key"),
        ("TUNE_LASTFM_API_SECRET", "lastfm_api_secret"),
    ] {
        let env_val = match std::env::var(env_var) {
            Ok(v) if !v.is_empty() => v,
            _ => continue,
        };
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let already_set = settings
            .get(key)
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !already_set {
            settings.set(key, &env_val).ok();
            info!("{key}_persisted_from_env");
        }
    }

    // Seed the quality_split default so the DB is the single source of truth.
    // get_config injects a `true` default in memory but never persists it, so an
    // untouched DB has no row — and both the manual and auto scanners fall back
    // to `unwrap_or(true)`, silently splitting albums by quality while the UI
    // shows the toggle "enabled". Seeding once (only when the row is absent)
    // makes the toggle authoritative and inspectable via SQL. Reported by Fabien:
    // `SELECT value FROM settings WHERE key='quality_split'` returned empty, and
    // disabling the option in the UI had no visible effect on the next scan.
    {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let has_row = settings
            .get("quality_split")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !has_row {
            settings.set("quality_split", "true").ok();
            info!("quality_split_default_seeded value=true");
        }
    }
}

/// Resolve the managed `yt-dlp` binary at boot (from the `yt_dlp_path` setting,
/// the auto-download location, or PATH) so YouTube playback works if it was
/// previously enabled. Does not download anything — that's the opt-in button.
async fn resolve_ytdlp(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let configured = settings.get("yt_dlp_path").ok().flatten();
    match tune_core::ytdlp::resolve(configured.as_deref()).await {
        Some(path) => info!(path = %path.display(), "youtube_ytdlp_ready"),
        None => info!("youtube_ytdlp_absent — YouTube playback not enabled"),
    }
}

/// Niveau à donner à une sortie locale qui vient de naître, d'après ce que la
/// base dit de sa zone. Une zone « Volume fixe (bit-perfect) » reste à pleine
/// échelle — c'est son contrat, le DoP ne survit pas à une multiplication.
///
/// Volontairement HORS du gate `local-audio` : c'est de l'arithmétique, sans
/// dépendance à `outputs::local`, et les tests tournent dans les deux jeux de
/// fonctionnalités.
#[cfg_attr(not(feature = "local-audio"), allow(dead_code))]
fn seed_volume_for(zone_volume: i32, fixed_volume: bool) -> f64 {
    if fixed_volume {
        1.0
    } else {
        (zone_volume as f64 / 100.0).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalZoneAction {
    Create,
    Reconnect,
    Skip,
}

#[cfg_attr(not(feature = "local-audio"), allow(dead_code))]
pub(crate) fn first_system_default_name<'a>(
    devices: impl IntoIterator<Item = (&'a str, bool)>,
) -> Option<&'a str> {
    devices
        .into_iter()
        .find_map(|(name, is_default)| is_default.then_some(name))
}

/// Décide quoi faire d'une sortie locale sans toucher à la base.
///
/// `is_system_default` ne doit être vrai que pour l'unique sortie sélectionnée
/// par l'appelant parmi celles que le backend marque `is_default`. Cette
/// sélection préalable empêche un backend défectueux qui en marquerait deux de
/// recréer le défaut « une zone par périphérique ».
pub(crate) fn local_zone_action(
    zone_exists: bool,
    auto_create: bool,
    is_system_default: bool,
) -> LocalZoneAction {
    if zone_exists {
        return LocalZoneAction::Reconnect;
    }
    if auto_create && is_system_default {
        LocalZoneAction::Create
    } else {
        LocalZoneAction::Skip
    }
}

/// Register local audio output devices (USB DAC, headphones, speakers).
///
/// Sur une base neuve, seule la sortie système reçoit automatiquement une
/// zone. Les autres sorties restent enregistrées dans `OutputRegistry`, donc
/// proposées par l'interface pour une création manuelle.
#[cfg(feature = "local-audio")]
pub async fn register_local_outputs(state: &AppState) {
    // Prefer DB-persisted backend (set via UI) over config/env default
    let audio_backend_owned = state.effective_audio_backend();
    let audio_backend = &audio_backend_owned;
    let exclusive_mode = state.effective_exclusive_mode();
    // Publish it: this is the value the outputs below are built with, and the
    // only honest answer for the signal path until the next restart.
    if let Ok(mut slot) = state.active_audio_backend.write() {
        *slot = Some(audio_backend_owned.clone());
    }

    // Enumerate output devices OFF the async runtime and under a hard timeout.
    //
    // Enumerating ASIO opens each driver to read its formats, and an ASIO driver
    // can only be opened by ONE process at a time: if another app (JRiver, foobar,
    // a DSD ASIO proxy…) already holds it, the open BLOCKS — potentially forever.
    // This call sits on the critical boot path *before* the HTTP listener starts
    // serving, so a blocked ASIO probe used to wedge the whole server: the port was
    // bound but nothing accepted connections → completely blank web UI (JP
    // Borderies, Denafrips USB DAC in ASIO with JRiver open). Running it in
    // `spawn_blocking` under a timeout guarantees the web UI always comes up; if the
    // scan does not respond we start WITHOUT local zones for this boot rather than
    // hang. The device becomes usable again once its driver is free (close the other
    // app) and Tune is relaunched.
    async fn scan_devices(backend: String) -> Option<Vec<tune_core::outputs::local::AudioDevice>> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(8),
            tokio::task::spawn_blocking(move || {
                tune_core::outputs::local::list_audio_devices_with_backend(&backend)
            }),
        )
        .await
        {
            Ok(Ok(devices)) => Some(devices),
            Ok(Err(_)) => {
                warn!("local_audio_enumeration_panicked — starting without local zones this boot");
                None
            }
            Err(_) => {
                warn!(
                    "local_audio_enumeration_timeout — an audio driver (most likely an ASIO device \
                     held by another application such as JRiver) did not respond within 8s. Starting \
                     the server WITHOUT local zones so the web UI stays available; close the other app \
                     and relaunch Tune to use the device."
                );
                None
            }
        }
    }

    // `None` means the scan timed out or panicked. When that happens we do NOT
    // attempt the WASAPI fallback: a hung ASIO probe still holds the internal scan
    // lock, so a second enumeration would only block (and time out) again — better
    // to bring the UI up now and let the next relaunch (with the driver free) pick
    // the device up.
    let scan = scan_devices(audio_backend_owned.clone()).await;
    let mut devices = scan.clone().unwrap_or_default();
    // When ASIO is selected AND the host actually responded but exposed no devices,
    // also enumerate WASAPI so the user still has fallback outputs available.
    if devices.is_empty() && scan.is_some() && audio_backend.eq_ignore_ascii_case("asio") {
        warn!("asio_returned_no_devices — also enumerating WASAPI as fallback");
        devices = scan_devices("wasapi".to_string()).await.unwrap_or_default();
    }
    if !devices.is_empty() {
        let mut outputs = state.outputs.lock().await;
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        let auto_create =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
                .get("zone_auto_create")
                .ok()
                .flatten()
                .map(|v| v != "false")
                .unwrap_or(true);
        // Un backend est censé marquer une seule sortie par défaut. `find`
        // rend cette unicité vraie même s'il en renvoie plusieurs par erreur.
        let system_default_device_id = first_system_default_name(
            devices
                .iter()
                .map(|dev| (dev.name.as_str(), dev.is_default)),
        )
        .map(|name| format!("local:{name}"));

        for dev in &devices {
            let device_id = format!("local:{}", dev.name);
            let local_out = tune_core::outputs::local::LocalOutput::with_options_and_endpoint(
                dev.name.clone(),
                (!dev.endpoint_id.is_empty()).then(|| dev.endpoint_id.clone()),
                exclusive_mode,
                audio_backend,
            );
            // Ensemencer la sortie avec le volume stocké.
            //
            // `LocalOutput` naît à `user_volume = 1.0` et rien ne le rectifiait :
            // `restore_zone_volumes` ne touche que la copie mémoire du
            // PlaybackManager, et depuis le compromis « Fabien » l'orchestrateur
            // ne réimpose plus le volume enregistré à la lecture. Une zone locale
            // réglée à 30 % repartait donc à PLEIN VOLUME au premier morceau
            // après un redémarrage — c'est précisément le réveil brutal que
            // l'écrêtage à 20 % prétendait empêcher sans jamais y toucher (#1596).
            //
            // Ce compromis-là ne s'applique pas ici : il protège le niveau
            // *physique* d'un appareil externe, que Tune ne connaît pas. Le gain
            // logiciel local, lui, n'appartient qu'à Tune, et sa valeur de départ
            // n'a aucune raison d'être 100 % plutôt que ce que l'utilisateur a
            // réglé. Une zone « Volume fixe » reste à 1.0 : c'est son contrat.
            if let Ok(Some(zone)) = zone_repo.get_by_device_id(&device_id) {
                let stored = seed_volume_for(zone.volume, zone.fixed_volume);
                if let Err(e) =
                    tune_core::outputs::OutputTarget::set_volume(&local_out, stored).await
                {
                    warn!(device_id = %device_id, error = %e, "local_output_volume_seed_failed");
                } else {
                    info!(device_id = %device_id, volume = stored, "local_output_volume_seeded");
                }
            }
            outputs.register(Box::new(local_out));
            info!(
                name = %dev.name,
                device_id = %device_id,
                default = dev.is_default,
                channels = dev.max_channels,
                rates = ?dev.sample_rates,
                "local_audio_output_registered"
            );

            let zone_name = if dev.is_default {
                "This Computer".to_string()
            } else {
                dev.name.clone()
            };

            let zone_exists = zone_repo
                .get_by_device_id(&device_id)
                .ok()
                .flatten()
                .is_some();
            let is_system_default = system_default_device_id.as_deref() == Some(device_id.as_str());
            let action = local_zone_action(zone_exists, auto_create, is_system_default);
            if action == LocalZoneAction::Skip {
                info!(
                    name = %zone_name,
                    device_id = %device_id,
                    default = is_system_default,
                    auto_create,
                    "local_audio_zone_manual_creation_required"
                );
                continue;
            }

            match zone_repo.get_or_create(&zone_name, Some("local"), &device_id) {
                Ok((zid, true)) => {
                    info!(
                        name = %zone_name,
                        zone_id = zid,
                        device_id = %device_id,
                        "local_audio_zone_auto_created"
                    );
                }
                Ok((zid, false)) => {
                    let _ = zone_repo.set_online_by_device(&device_id, true);
                    // Zones héritées : les anciennes versions nommaient TOUTES
                    // les zones locales « This Computer » — deux DAC devenaient
                    // des jumelles indiscernables (forum #1233, Alain). Un DAC
                    // non-défaut coincé sur l'étiquette générique prend le nom
                    // du périphérique ; un nom personnalisé n'est jamais touché.
                    if !dev.is_default
                        && let Ok(n) = zone_repo.rename_generic_local_label(zid, &dev.name)
                        && n > 0
                    {
                        info!(zone_id = zid, name = %dev.name, "local_zone_generic_label_healed");
                    }
                    // Device par défaut : le device_id étant dérivé du NOM du
                    // périphérique (`local:<name>`), un renommage du Mac ou un
                    // changement de locale macOS crée une SECONDE zone par
                    // défaut portant l'étiquette générique de l'autre langue
                    // (« This Computer » ⇄ « Cet ordinateur »). get_or_create /
                    // deduplicate matchent sur device_id et ne fusionnent jamais
                    // ces jumelles → les deux restent dans le sélecteur (Philippe
                    // Vella). On masque les jumelles génériques, en gardant celle
                    // liée au device vivant. Étiquettes génériques uniquement —
                    // une zone renommée par l'utilisateur n'est jamais touchée.
                    if dev.is_default
                        && let Ok(n) = zone_repo.hide_duplicate_generic_local(zid)
                        && n > 0
                    {
                        info!(
                            zone_id = zid,
                            hidden = n,
                            "local_default_zone_duplicates_hidden"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        name = %zone_name,
                        device_id = %device_id,
                        error = %e,
                        "local_audio_zone_create_failed"
                    );
                }
            }
        }

        info!(count = devices.len(), "local_audio_devices_registered");
    } else {
        info!("no_local_audio_devices_found");
    }
}

/// Remonte les partages reseau enregistres, avant que quoi que ce soit ne lise
/// la bibliotheque.
///
/// Rien ne les remontait au demarrage. Consequence chez Dominique Comet
/// (#1692) : apres chaque redemarrage son partage SMB n'etait plus monte, le
/// repertoire configure existait mais vide, le scan annoncait « 0 fichier », et
/// il devait re-saisir son partage ET ses identifiants pour retrouver sa
/// musique.
///
/// ⚠️ On lit la table que les ROUTES ecrivent (`mount_type/server/share/…/
/// active`), pas celle de `mount_manager.rs` (`host/share_name/…/auto_mount`),
/// qui porte le meme nom, des colonnes differentes, et n'est construite nulle
/// part hors tests. Batir le remontage sur `auto_mount` interrogerait une table
/// que le serveur ne remplit jamais.
///
/// Chaque montage est independant : un partage injoignable est journalise et
/// n'empeche ni les autres ni le demarrage. Un NAS eteint ne doit pas empecher
/// Tune de servir ce qui est local.
pub async fn remount_network_shares(state: &AppState) {
    let rows = match state.backend.query_many(
        "SELECT server, share, mount_path, username, password, id, smb_version \
         FROM network_mounts WHERE mount_type = 'smb' AND COALESCE(active, 1) = 1",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "remount_network_shares_query_failed");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    info!(count = rows.len(), "remounting_network_shares");
    for r in rows {
        let host = r.first().and_then(|v| v.as_string()).unwrap_or_default();
        let share = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let path = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        if host.is_empty() || share.is_empty() || path.is_empty() {
            continue;
        }
        let id = r.get(5).and_then(|v| v.as_i64());
        // Deja monte (redemarrage du seul service, systeme reste debout) :
        // ne pas empiler un second montage sur le meme point.
        //
        // Le test etait « le repertoire contient-il quelque chose ? ». Un point
        // de montage non monte mais portant des residus — un scan a ecrit
        // dedans pendant que le NAS etait tombe — faisait donc sauter le
        // remontage SANS UN MOT, et l'utilisateur se retrouvait avec une
        // bibliotheque a moitie lisible que rien n'expliquait. On demande
        // desormais s'il s'agit reellement d'un point de montage (#1916).
        if crate::smb::est_un_point_de_montage(std::path::Path::new(&path)) {
            tracing::debug!(host = %host, share = %share, path = %path, "network_share_already_mounted_skipping");
            noter_montage(state, id, "mounted", None, None).await;
            continue;
        }
        let user = r.get(3).and_then(|v| v.as_string()).unwrap_or_default();
        let pass = r.get(4).and_then(|v| v.as_string()).unwrap_or_default();
        let connu = r.get(6).and_then(|v| v.as_string()).unwrap_or_default();

        // La RESTITUTION reste propre a chaque appelant — la route rend des
        // erreurs HTTP a un humain qui attend, celle-ci journalise et passe au
        // suivant. La STRATEGIE de montage, elle, est commune (`crate::smb`) :
        // recopiee, elle avait diverge, et ce code imposait encore `vers=3.0`
        // quand la route avait appris a negocier. Le partage SMB 1.0 de
        // Philippe Landes montait donc depuis l'assistant, et le premier
        // redemarrage le lui reprenait (#1834).
        let (result, dialecte_retenu) = if cfg!(target_os = "macos") {
            let creds = if user.is_empty() {
                "guest@".to_string()
            } else if pass.is_empty() {
                format!("{user}@")
            } else {
                format!("{user}:{pass}@")
            };
            let unc = format!("//{creds}{host}/{share}");
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::process::Command::new("mount_smbfs")
                    .args([&unc, &path])
                    .output(),
            )
            .await;
            (res, None)
        } else {
            let u = if user.is_empty() { "guest" } else { &user };
            let unc = format!("//{host}/{share}");
            // Le dialecte deja retenu passe en premier : sans cela, un partage
            // SMB 1.0 rejouerait deux essais voues a l'echec a CHAQUE
            // demarrage, soit vingt secondes avant que sa musique ne soit
            // lisible. Le reste de l'echelle suit quand meme — un NAS mis a
            // jour ne doit pas rester prisonnier de ce qu'il repondait avant.
            let echelle = crate::smb::echelle(if connu.is_empty() {
                None
            } else {
                Some(connu.as_str())
            });
            let mut dernier = None;
            let mut gagnant = None;
            for dialecte in echelle {
                let mut opts = format!("username={u},password={pass}");
                if let Some(v) = dialecte {
                    opts.push_str(&format!(",vers={v}"));
                }
                // JAMAIS `opts` dans une trace : il porte le mot de passe.
                let res = tokio::time::timeout(
                    crate::smb::ESSAI_TIMEOUT,
                    tokio::process::Command::new("mount.cifs")
                        .args([&unc, &path, "-o", &opts])
                        .output(),
                )
                .await;
                let arreter = match &res {
                    Ok(Ok(out)) if out.status.success() => {
                        gagnant = Some(crate::smb::etiquette(dialecte).to_string());
                        true
                    }
                    Ok(Ok(out)) => crate::smb::est_refus_d_authentification(
                        &String::from_utf8_lossy(&out.stderr),
                    ),
                    // mount.cifs absent ou non executable : changer de dialecte
                    // n'y fera rien.
                    Ok(Err(_)) => true,
                    Err(_) => false,
                };
                dernier = Some(res);
                if arreter {
                    break;
                }
            }
            (dernier.expect("l'echelle n'est jamais vide"), gagnant)
        };

        // Chaque issue est desormais ECRITE, pas seulement journalisee. C'est
        // tout l'objet de #1916 : le remontage echouait, seul le journal le
        // savait, l'interface continuait d'afficher le partage comme monte, et
        // la lecture rendait une erreur reseau qui ne nommait jamais la cause.
        // Eric (`ricouxxx`) a du trouver le contournement seul, sur un forum.
        match result {
            Ok(Ok(out)) if out.status.success() => {
                info!(
                    host = %host, share = %share, path = %path,
                    dialect = dialecte_retenu.as_deref().unwrap_or("negocie"),
                    "network_share_remounted"
                );
                noter_montage(state, id, "mounted", None, dialecte_retenu.as_deref()).await;
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                warn!(
                    host = %host, share = %share, error = %stderr,
                    "network_share_remount_failed"
                );
                noter_montage(state, id, "failed", Some(&stderr), None).await;
            }
            Ok(Err(e)) => {
                warn!(host = %host, share = %share, error = %e, "network_share_remount_failed");
                noter_montage(state, id, "failed", Some(&e.to_string()), None).await;
            }
            Err(_) => {
                warn!(host = %host, share = %share, "network_share_remount_timeout");
                noter_montage(state, id, "failed", Some("délai dépassé au montage"), None).await;
            }
        }
    }
}

/// Ecrit le constat du dernier montage sur la ligne du partage.
///
/// `active` dit ce que l'utilisateur VEUT ; ces colonnes disent ce qui s'est
/// PASSE. Sans elles, un remontage en echec etait indiscernable d'un partage
/// sain — c'est exactement ce qui a coute a Eric (`ricouxxx`) un aller-retour
/// sur un forum public pour apprendre qu'il lui suffisait de re-saisir son
/// partage (#1916).
///
/// `dialecte` n'est ecrit que lorsqu'il vient d'etre etabli : passer `None` sur
/// un succes macOS, ou apres un echec, ne doit pas effacer ce qu'on savait.
///
/// Une ecriture ratee n'interrompt rien : le remontage suivant compte plus que
/// la tenue du journal de bord.
async fn noter_montage(
    state: &AppState,
    id: Option<i64>,
    etat: &str,
    erreur: Option<&str>,
    dialecte: Option<&str>,
) {
    use tune_core::db::backend::ToSqlValue;
    let Some(id) = id else { return };
    let pg = state.backend.engine() == tune_core::db::engine::Engine::Postgres;
    let p = |n: usize| {
        if pg { format!("${n}") } else { "?".to_string() }
    };
    let erreur = erreur.map(|e| e.to_string());
    // `network_mounts.id` est INTEGER sous SQLite et TEXT sous PostgreSQL. Lier
    // un entier ferait echouer PostgreSQL en « operator does not exist: text =
    // bigint » ; lier une chaine marche des deux cotes, SQLite appliquant
    // l'affinite numerique de la colonne a l'operande texte.
    let id_texte = id.to_string();
    let mut sql = format!(
        "UPDATE network_mounts SET mount_state = {}, last_mount_error = {}",
        p(1),
        p(2)
    );
    let mut args: Vec<&dyn ToSqlValue> = vec![&etat, &erreur];
    if let Some(d) = dialecte.as_ref() {
        sql.push_str(&format!(", smb_version = {}", p(3)));
        args.push(d);
    }
    sql.push_str(&format!(" WHERE id = {}", p(args.len() + 1)));
    args.push(&id_texte);
    if let Err(e) = state.backend.execute(&sql, &args) {
        warn!(error = %e, id, "network_share_state_write_failed");
    }
}

#[cfg(test)]
mod restore_zone_volumes_tests {
    use super::*;
    use tune_core::db::zone_repo::ZoneRepo;

    fn state_with_zone(volume: i32, fixed: bool) -> (AppState, i64) {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let id = repo
            .create("Zone test", Some("local"), Some("local:Test"))
            .unwrap();
        repo.update_volume(id, volume).unwrap();
        repo.update_fixed_volume(id, fixed).unwrap();
        (state, id)
    }

    /// Forum 1320 (Cyrille) / #1504 — le garde-fou anti-réveil rabaissait
    /// AUSSI les zones « Volume fixe (bit-perfect) » à 20 % au redémarrage :
    /// la base disait 100, l'effectif était 0.2, et le DoP mourait (le
    /// moindre gain logiciel détruit les marqueurs). Une zone fixed_volume
    /// doit redémarrer à exactement 1.0. Ce test ÉCHOUE contre le code
    /// d'avant (0.2 au lieu de 1.0).
    #[tokio::test]
    async fn fixed_volume_zone_restarts_at_full_scale() {
        let (state, id) = state_with_zone(100, true);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!(
            (vol - 1.0).abs() < 1e-9,
            "zone bit-perfect restaurée à {vol} au lieu de 1.0"
        );
    }

    /// #1596 — une zone ordinaire stockée à 100 % revient à 100 %.
    ///
    /// L'écrêtage à 20 % qui vivait ici ne descendait le son de personne : il
    /// ne touchait ni la base ni la sortie. Il ne produisait qu'un désaccord à
    /// trois voix et un événement `volume: 0.2` — les 20 % que Jean Valjean
    /// (#1504) et Bebelalu55 (#1480) ont vus s'afficher. Ce test ÉCHOUE contre
    /// le code d'avant (0.2 au lieu de 1.0).
    #[tokio::test]
    async fn non_fixed_zone_at_full_scale_is_restored_verbatim() {
        let (state, id) = state_with_zone(100, false);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!(
            (vol - 1.0).abs() < 1e-9,
            "un 100 % choisi par l'utilisateur doit revenir à 1.0, obtenu: {vol}"
        );
    }

    /// La mémoire ne doit jamais contredire la base après restauration : c'est
    /// le désaccord que #1548 a soigné côté affichage sans le supprimer.
    #[tokio::test]
    async fn memory_agrees_with_db_for_every_stored_level() {
        for stocke in [0, 20, 55, 99, 100] {
            let (state, id) = state_with_zone(stocke, false);
            restore_zone_volumes(&state).await;
            let vol = state.playback.get_state(id).await.volume;
            let attendu = stocke as f64 / 100.0;
            assert!(
                (vol - attendu).abs() < 1e-9,
                "base {stocke} % / mémoire {vol} — les deux doivent dire la même chose"
            );
        }
    }

    /// #1596 — la protection réelle contre le réveil à plein volume.
    ///
    /// `LocalOutput` naît à 1.0 et personne ne le corrigeait : une zone locale
    /// à 30 % repartait à fond au premier morceau après un redémarrage. C'est
    /// le seul endroit où le volume stocké atteint vraiment le son.
    #[test]
    fn local_output_is_seeded_with_the_stored_level() {
        assert!((seed_volume_for(30, false) - 0.30).abs() < 1e-9);
        assert!((seed_volume_for(0, false) - 0.0).abs() < 1e-9);
        assert!((seed_volume_for(100, false) - 1.0).abs() < 1e-9);
    }

    /// Une zone bit-perfect ne s'ensemence jamais autrement qu'à pleine échelle,
    /// quelle que soit la valeur qui traîne en base (forum 1320, Cyrille).
    #[test]
    fn fixed_volume_output_is_seeded_at_full_scale() {
        assert!((seed_volume_for(20, true) - 1.0).abs() < 1e-9);
        assert!((seed_volume_for(100, true) - 1.0).abs() < 1e-9);
    }

    /// Une valeur aberrante en base ne doit pas amplifier — le gain est un
    /// multiplicateur appliqué à chaque échantillon.
    #[test]
    fn out_of_range_stored_level_never_amplifies() {
        assert!((seed_volume_for(150, false) - 1.0).abs() < 1e-9);
        assert!((seed_volume_for(-5, false) - 0.0).abs() < 1e-9);
    }

    /// Un volume ordinaire est restauré tel quel, fixed ou pas.
    #[tokio::test]
    async fn ordinary_volume_is_restored_verbatim() {
        let (state, id) = state_with_zone(55, false);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!((vol - 0.55).abs() < 1e-9, "volume restauré: {vol}");
    }
}

#[cfg(test)]
mod local_zone_creation_policy_tests {
    use super::*;

    /// #1770 — témoin exact d'une base neuve avec plusieurs sorties : une
    /// sortie ordinaire ne devient pas une zone, même si l'auto-création est
    /// laissée à sa valeur par défaut (`true`).
    #[test]
    fn fresh_install_creates_only_the_system_default_zone() {
        assert_eq!(local_zone_action(false, true, false), LocalZoneAction::Skip);
        assert_eq!(
            local_zone_action(false, true, true),
            LocalZoneAction::Create
        );
    }

    /// Un backend fautif peut marquer plusieurs sorties `is_default`. Tune en
    /// choisit une seule au lieu de recréer le défaut en bloc.
    #[test]
    fn even_two_backend_defaults_select_only_one_system_device() {
        let devices = [("Speakers", false), ("DAC A", true), ("DAC B", true)];
        assert_eq!(
            first_system_default_name(devices),
            Some("DAC A"),
            "la première sortie système est l'unique candidate"
        );
    }

    /// Le réglage d'opt-out conserve son sens : même la sortie système ne doit
    /// pas être imposée quand l'utilisateur a coupé l'auto-création.
    #[test]
    fn disabled_auto_creation_creates_no_default_zone() {
        assert_eq!(local_zone_action(false, false, true), LocalZoneAction::Skip);
    }

    /// Le scénario ASIO → WASAPI de DEvir passe ici : un rescan peut découvrir
    /// dix sorties mais seule la sortie système devient une zone. Un DAC qui
    /// devient la sortie système reste donc immédiatement utilisable.
    #[test]
    fn hotplug_creates_the_system_zone_but_not_the_other_outputs() {
        assert_eq!(local_zone_action(false, true, false), LocalZoneAction::Skip);
        assert_eq!(
            local_zone_action(false, true, true),
            LocalZoneAction::Create
        );
    }

    /// Contre-épreuve installation existante : le nouveau contrat ne supprime
    /// ni ne masque les zones déjà enregistrées. Démarrage ou hotplug, elles
    /// reprennent le chemin `get_or_create` et sont remises en ligne.
    #[test]
    fn existing_non_default_zone_is_reconnected_in_both_phases() {
        assert_eq!(
            local_zone_action(true, false, false),
            LocalZoneAction::Reconnect
        );
    }

    /// Aucun repli silencieux vers « le premier périphérique » si le backend
    /// ne sait pas identifier sa sortie système.
    #[test]
    fn no_backend_default_means_no_automatic_candidate() {
        let devices = [("DAC A", false), ("DAC B", false)];
        assert_eq!(first_system_default_name(devices), None);
    }
}

#[cfg(test)]
mod asio_warm_scan_tests {
    use super::*;

    /// Cas nominal : pas de témoin, pas de coupure — on énumère.
    #[test]
    fn clean_boot_runs_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        assert_eq!(asio_warm_decision(&sentinel, false), AsioWarmDecision::Run);
    }

    /// Le témoin laissé par un démarrage qui n'est jamais revenu doit couper le
    /// balayage. C'est le test qui ÉCHOUE contre le code d'avant : la 0.9.45 à
    /// la 0.9.71 relançaient l'énumération à chaque lancement, donc mouraient
    /// à chaque lancement (fil 1313, Alain Bonnel — plus de trente démarrages
    /// identiques, aucun n'atteignant l'écoute HTTP).
    #[test]
    fn sentinel_from_a_crashed_boot_skips_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        std::fs::write(&sentinel, "asio warm scan in progress\n").unwrap();
        assert_eq!(
            asio_warm_decision(&sentinel, false),
            AsioWarmDecision::SkippedAfterCrash
        );
    }

    /// La coupure par l'environnement prime sur tout le reste : c'est le
    /// dépannage qu'on peut dicter à un testeur au téléphone.
    #[test]
    fn env_kill_switch_wins_over_a_clean_state() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        assert_eq!(
            asio_warm_decision(&sentinel, true),
            AsioWarmDecision::SkippedByEnv
        );
    }

    /// Le témoin vit dans le dossier de données, à côté du journal — celui que
    /// l'on demande déjà d'ouvrir (`%LOCALAPPDATA%\TuneServer`).
    #[test]
    fn sentinel_sits_next_to_the_log_file() {
        let path = asio_warm_sentinel_path();
        assert_eq!(path.file_name().unwrap(), ASIO_WARM_SENTINEL);
        assert_eq!(
            path.parent(),
            crate::config::default_log_file_path().parent()
        );
    }

    /// Le blocage n'est plus une ligne perdue dans le journal : il porte un
    /// état stable, une phrase et l'action possible pour l'interface.
    #[test]
    fn crashed_sentinel_is_visible_and_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        std::fs::write(&sentinel, "asio warm scan in progress\n").unwrap();

        let status = asio_warm_status_at(&sentinel, false, true);
        assert_eq!(status.state, "blocked_after_crash");
        assert!(status.blocked_after_crash);
        assert!(status.can_rearm);
        assert_eq!(status.retry, "rearm_then_restart");
        assert!(status.message.contains("Réarmez"));
        assert_eq!(status.sentinel_path, sentinel.display().to_string());
    }

    /// Réarmer retire exactement le témoin ; le second appel est idempotent et
    /// ne transforme pas une absence normale en erreur.
    #[test]
    fn explicit_rearm_allows_one_future_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        std::fs::write(&sentinel, "asio warm scan in progress\n").unwrap();

        assert_eq!(
            rearm_asio_warm_scan_at(&sentinel, false, true).unwrap(),
            AsioWarmRearm::Rearmed
        );
        assert!(!sentinel.exists());
        assert_eq!(
            rearm_asio_warm_scan_at(&sentinel, false, true).unwrap(),
            AsioWarmRearm::AlreadyReady
        );
    }

    /// L'API d'administration n'a pas le droit de défaire le choix explicite
    /// de l'exploitant ; le fichier reste donc intact.
    #[test]
    fn environment_kill_switch_cannot_be_bypassed_by_rearm() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join(ASIO_WARM_SENTINEL);
        std::fs::write(&sentinel, "asio warm scan in progress\n").unwrap();

        assert_eq!(
            rearm_asio_warm_scan_at(&sentinel, true, true).unwrap(),
            AsioWarmRearm::DisabledByEnv
        );
        assert!(sentinel.exists());
    }
}

#[cfg(test)]
mod demarrage_sans_doublon_tests {
    use super::*;

    /// Le re-sondage DLNA au demarrage etait ecrit DEUX FOIS dans
    /// `init_state`, commentaire compris. Chaque appareil persiste etait donc
    /// sonde deux fois en parallele : sur un renderer eteint, la sequence de
    /// reprises jouait en double — 16 tentatives au lieu de 8, pres de trois
    /// minutes de tampons reseau et de journal au demarrage.
    ///
    /// Repere dans le journal de JP Borderies (0.9.83) : chaque ligne
    /// `discovered_dlna_reprobe_retry` y apparait exactement deux fois, pour
    /// le meme uuid, a la meme milliseconde.
    ///
    /// Ce test lit le CONTENU de `init_state`. Un doublon de ce genre ne se
    /// voit pas a la relecture — les deux blocs sont identiques et separes par
    /// rien — mais il se compte.
    #[test]
    fn le_resondage_dlna_n_est_lance_qu_une_fois() {
        let source = include_str!("startup.rs");
        let init = source
            .split("pub async fn init_state")
            .nth(1)
            .expect("init_state introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de init_state introuvable");

        let n = init.matches("reprobe_persisted_dlna_devices").count();
        assert_eq!(
            n, 1,
            "`reprobe_persisted_dlna_devices` est lance {n} fois dans \
             init_state. Chaque appel sonde TOUS les appareils persistes : le \
             doubler double aussi la sequence de reprises sur un renderer \
             injoignable."
        );
    }

    // ---- #2002 : un `running` en base survit au processus qui l'a ecrit ----

    /// Le cas de Bilou, tel quel : sa passe s'est arretee a 5 650 / 16 261 et
    /// le reglage l'annonce encore en cours.
    #[test]
    fn un_enrichissement_reste_running_est_declare_interrompu() {
        let brut =
            r#"{"status":"running","task_id":"abc","enriched":5650,"errors":3,"total":16261}"#;
        let (neuf, traite, total) =
            avancement_interrompu(brut).expect("un `running` doit etre reecrit");
        let v: serde_json::Value = serde_json::from_str(&neuf).unwrap();

        assert_eq!(v["status"], "interrupted");
        assert_eq!(traite, 5650);
        assert_eq!(total, 16261);

        // Les compteurs survivent : « interrompu a 5 650 / 16 261 » se
        // comprend, un reglage efface ne dirait plus rien.
        assert_eq!(v["enriched"], 5650);
        assert_eq!(v["errors"], 3);
        assert_eq!(v["task_id"], "abc");
    }

    /// Le client des images d'artistes NE LIT PAS `status` mais `phase`
    /// (`if (!phase || phase === 'done') return;`). Corriger `status` seul
    /// laisserait ce client-la reprendre un suivi fantome — et garder son
    /// bouton grise.
    #[test]
    fn la_phase_est_neutralisee_pour_le_client_deja_livre() {
        let brut = r#"{"status":"running","phase":"images","processed":340,"total":1183}"#;
        let (neuf, traite, total) = avancement_interrompu(brut).expect("reecriture attendue");
        let v: serde_json::Value = serde_json::from_str(&neuf).unwrap();

        assert_eq!(v["status"], "interrupted");
        assert_eq!(
            v["phase"], "done",
            "sans `phase: done`, le client deja publie reprend un suivi fantome"
        );
        assert_eq!(traite, 340, "`processed` sert de repli a `enriched`");
        assert_eq!(total, 1183);
    }

    /// Une passe terminee normalement n'est pas retouchee : la reecrire
    /// effacerait le bilan de la derniere passe reussie.
    #[test]
    fn une_passe_terminee_n_est_pas_retouchee() {
        assert!(avancement_interrompu(r#"{"status":"done","enriched":42,"total":42}"#).is_none());
        assert!(avancement_interrompu(r#"{"status":"idle"}"#).is_none());
    }

    /// Un reglage illisible ou d'une autre forme est laisse intact : ecraser ce
    /// qu'on ne comprend pas est pire que de le laisser.
    #[test]
    fn un_reglage_illisible_est_laisse_intact() {
        assert!(avancement_interrompu("pas du json").is_none());
        assert!(avancement_interrompu(r#""running""#).is_none());
        assert!(avancement_interrompu("[1,2,3]").is_none());
        assert!(avancement_interrompu("{}").is_none());
    }

    /// L'appel doit rester dans `init_state` : c'est le demarrage du processus
    /// qui PROUVE qu'aucune passe ne tourne. Deplace ailleurs, la preuve tombe.
    #[test]
    fn le_marquage_est_bien_appele_au_demarrage() {
        let source = include_str!("startup.rs");
        let init = source
            .split("pub async fn init_state")
            .nth(1)
            .expect("init_state introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de init_state introuvable");
        assert_eq!(
            init.matches("marquer_enrichissements_interrompus").count(),
            1,
            "le marquage des enrichissements interrompus doit etre appele une \
             fois et une seule dans init_state"
        );
    }
}

#[cfg(test)]
mod registre_executions_tests {
    use std::sync::Arc;

    use tune_core::db::backend::{DbBackend, ToSqlValue};
    use tune_core::db::migrations;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::task_run_repo::{TACHE_SCAN_DEMARRAGE, TaskRunRepo, boot_id};

    /// Le registre doit etre ouvert DANS `init_state` : c'est le demarrage du
    /// processus qui prouve qu'aucune passe encore inscrite « en cours » ne
    /// tourne. Appele ailleurs — dans une tache de fond, par exemple — il
    /// fermerait des passes vivantes ou n'en fermerait aucune.
    #[test]
    fn le_registre_est_ouvert_au_demarrage() {
        let source = include_str!("startup.rs");
        let init = source
            .split("pub async fn init_state")
            .nth(1)
            .expect("init_state introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de init_state introuvable");
        assert_eq!(
            init.matches("ouvrir_le_registre_des_executions").count(),
            1,
            "l'ouverture du registre doit etre appelee une fois et une seule \
             dans init_state"
        );
    }

    /// L'ordre porte le raisonnement : clore d'abord, purger ensuite. Purger
    /// en premier effacerait une orpheline ancienne SANS l'avoir close, et le
    /// nombre de fermetures journalise ne dirait plus la verite.
    #[test]
    fn on_clot_les_orphelines_avant_de_purger_par_age() {
        let source = include_str!("startup.rs");
        let corps = source
            .split("fn ouvrir_le_registre_des_executions")
            .nth(1)
            .expect("fonction introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de fonction introuvable");
        let cloture = corps.find("clore_orphelines").expect("cloture absente");
        let purge = corps.find("purger_par_age").expect("purge absente");
        assert!(
            cloture < purge,
            "la cloture des orphelines doit preceder la purge par age"
        );
    }

    /// Le chemin complet, bout en bout : une passe que la base croit encore en
    /// cours au demarrage est close, et le bouton — ou l'ecran — ne reste pas
    /// suspendu a une passe morte (#2002).
    #[test]
    fn une_passe_orpheline_survit_au_redemarrage_puis_est_close() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);

        // Le processus precedent a ouvert un scan et n'est jamais revenu.
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES ('boot-mort', ?, 1, '2026-08-27T22:00:00Z', 'en_cours')",
            &[&TACHE_SCAN_DEMARRAGE as &dyn ToSqlValue],
        )
        .unwrap();

        let registre = TaskRunRepo::with_backend(db.clone());
        assert_eq!(
            registre.lister(Some(TACHE_SCAN_DEMARRAGE), 1).unwrap()[0].outcome,
            "en_cours",
            "temoin : sans cloture, la base ment pour toujours"
        );

        assert_eq!(registre.clore_orphelines().unwrap(), 1);

        let apres = &registre.lister(Some(TACHE_SCAN_DEMARRAGE), 1).unwrap()[0];
        assert_eq!(apres.outcome, "interrompu");
        assert_ne!(
            apres.boot_id,
            boot_id(),
            "la ligne garde son boot d'origine"
        );
        assert!(apres.finished_at.is_some());
        assert!(
            apres.duration_ms.is_none(),
            "on n'a jamais vu la fin de cette passe"
        );
    }
}
