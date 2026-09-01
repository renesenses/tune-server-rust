use crate::routes::panne_sql::OuDefautJournalise;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use tune_core::audio::replaygain::{ReplayGainMode, ReplayGainSourceMode};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

pub(super) async fn version() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
    }))
}

/// Sonde de santé publique (#2796).
///
/// ## Ce que la route prétendait, et ce qu'elle vérifiait
///
/// Elle annonçait `status: "ok"` en dur, dans un `Json` donc toujours en
/// **HTTP 200**, quel que soit l'état de la base. Le seul reflet du réel était
/// le champ `db`, calculé sur la seule sonde `tracks` : une panne sur
/// `albums`, ou sur la lecture du réglage `server_name`, était convertie en
/// zéro ou en nom par défaut sans dégrader quoi que ce soit. Les trois
/// affirmations de la réponse — code HTTP, champ `status`, détail par
/// composant — pouvaient donc se contredire, et deux d'entre elles mentaient.
///
/// ## Qui lit cette route, et pourquoi le code HTTP se dose
///
/// Elle n'est pas seulement une sonde de supervision : c'est le **test
/// d'existence d'un serveur Tune**. La découverte réseau du client Flutter
/// (`server_discovery.dart`) n'enregistre un hôte que si elle obtient
/// exactement `200`; la télécommande macOS conditionne toute sa connexion à
/// cet appel; les clients iOS/iPadOS lèvent sur non-2xx; la barre latérale web
/// y prend le numéro de version; `SettingsView` s'en sert pour savoir quand le
/// serveur est revenu après un redémarrage. Un 503 rend donc le serveur
/// **invisible**, il ne le signale pas « en peine ».
///
/// D'où la gradation, qui reste dans le contrat demandé sans transformer une
/// requête malchanceuse en disparition :
///
/// - toutes les sondes passent → `ok`, HTTP 200 ;
/// - une partie échoue (la base répond encore, une requête a échoué : verrou
///   SQLite pris pendant un balayage, par exemple) → `degraded`, HTTP **200** ;
/// - **toutes** échouent, c'est-à-dire base indisponible → `error`, HTTP 503.
///
/// Les conteneurs ne rebouclent pas là-dessus : les `HEALTHCHECK` des deux
/// `Dockerfile` visent `/system/stats`, pas cette route.
///
/// ## Pourquoi `components` et pas un nouveau vocabulaire
///
/// Le détail par composant existe déjà **côté clients** et n'a jamais été
/// servi : `SystemHealth.components: Record<string, boolean>` est déclaré dans
/// le client web (`types.ts`), rendu en grille par `DiagnosticsView` et
/// `SettingsView`, et déclaré dans les modèles iOS et macOS. La bannière web
/// distingue déjà `ok` de `degraded`. On remplit ce contrat-là ; on n'en
/// invente pas un second. La santé « avancée » (mémoire, disque, blocage de
/// lecture) reste où elle est, sur `/system/health/monitor`, et n'entre pas
/// ici : ses sondes lancent un sous-processus `df`, ce qu'une route sollicitée
/// par la découverte réseau et par une boucle de 700 ms ne peut pas payer.
pub(super) async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let tracks_result = TrackRepo::with_backend(state.backend.clone()).count();
    let albums_result = AlbumRepo::with_backend(state.backend.clone()).count();
    let uptime_secs = state.started_at.elapsed().as_secs();

    // Le nom voyage AVEC la version (#2110). C'est la même requête que la barre
    // latérale fait déjà pour afficher « v0.9.117 » : la plainte d'origine est
    // qu'elle annonce une version sans dire de quelle machine elle parle. Les
    // séparer imposerait un second appel — et laisserait l'étiquette absente
    // tant qu'il n'a pas répondu. C'est aussi la troisième sonde de la base :
    // elle touche `settings`, une table que les deux comptages ne lisent pas.
    let name_result = SettingsRepo::with_backend(state.backend.clone()).get("server_name");
    let server_name = resolve_server_name(name_result.as_ref().ok().and_then(|v| v.as_deref()));

    let sondes = [
        ("db_tracks", tracks_result.is_ok()),
        ("db_albums", albums_result.is_ok()),
        ("db_settings", name_result.is_ok()),
    ];
    let echecs = sondes.iter().filter(|(_, ok)| !*ok).count();

    // Une panne SQL ne doit pas rester muette dans le journal (#2861) : les
    // valeurs de repli partent quand même dans la réponse, mais accompagnées
    // du `components` qui les contredit, et d'une trace côté serveur.
    let tracks = tracks_result.ou_defaut_journalise();
    let albums = albums_result.ou_defaut_journalise();

    let (code, status, db_status) = match echecs {
        0 => (StatusCode::OK, "ok", "connected"),
        n if n == sondes.len() => (StatusCode::SERVICE_UNAVAILABLE, "error", "error"),
        _ => (StatusCode::OK, "degraded", "degraded"),
    };

    let components: serde_json::Map<String, Value> = sondes
        .iter()
        .map(|(nom, ok)| ((*nom).to_string(), Value::Bool(*ok)))
        .collect();

    (
        code,
        Json(json!({
            "status": status,
            "version": tune_core::version(),
            "server_name": server_name,
            "uptime_seconds": uptime_secs,
            "db": db_status,
            "tracks": tracks,
            "albums": albums,
            "components": components,
        })),
    )
}

pub(super) async fn stats(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let listens = HistoryRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    // Use timeout to avoid blocking if scanner/outputs mutex is held (e.g. during SSDP scan)
    let devices = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        state.scanner.devices().await.len()
    })
    .await
    .unwrap_or(0);
    let outputs = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        state.outputs.lock().await.list().len()
    })
    .await
    .unwrap_or(0);

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "zones": zones,
        "devices": devices,
        "outputs": outputs,
        "server_version": tune_core::version(),
        "server_engine": "rust",
    }))
}

/// `audio_backend` n'est PAS le réglage de la sortie locale — et cette route
/// ne doit jamais le laisser passer pour tel (#2265).
///
/// Dans cette API, `audio_backend` nomme le backend **réellement ouvert**,
/// après un éventuel repli ASIO → WASAPI : c'est ce que rendent
/// `/system/diagnostics`, `/system/profile`, `/zones` et l'instantané
/// WebSocket. Le RÉGLAGE, lui, s'appelle `local_audio_backend` — c'est le seul
/// nom que la lecture consulte (`AppState::effective_audio_backend`, qui
/// interroge la clé `local_audio_backend` et rien d'autre).
///
/// Or les deux extrémités de cette route sont ouvertes : `update_config`
/// persiste ses clés sans liste blanche, et `get_config` renvoie la table
/// `settings` telle quelle. Une ligne `audio_backend` écrite là voyagerait
/// donc dans la réponse **comme si elle était le réglage**, alors qu'aucun
/// chemin de lecture ne la lit.
///
/// Elle aurait un lecteur, et c'est ce qui la rend coûteuse : le client web
/// livré aujourd'hui lit `data.audio_backend ?? data.local_audio_backend` —
/// **l'ancien nom d'abord**. Une telle ligne lui ferait afficher, et garder
/// sélectionné, un backend que le serveur n'ouvrira jamais. C'est l'annonce
/// fantôme que #2053 et #1315 ont déjà coûtée.
///
/// D'où les deux gardes, aux deux bouts : on refuse d'en créer une, et on ne
/// publie pas celle qui existerait déjà. Rien n'est effacé en base — même
/// discipline que le repli de `local_audio_backend` juste en dessous : on
/// corrige la RÉPONSE, pas le contenu de la table.
pub(super) const BACKEND_ACTIF_PAS_UN_REGLAGE: &str = "audio_backend";

/// Le message rendu à qui tente d'écrire `audio_backend` : ce qui se passe,
/// et quoi faire à la place. Un 400 muet renverrait le client à la devinette.
pub(super) fn refus_backend_actif() -> String {
    format!(
        "'{BACKEND_ACTIF_PAS_UN_REGLAGE}' is not a setting: it reports the backend the local \
         output actually opened, after any ASIO to WASAPI fallback. Writing it would store a \
         value that playback never reads. The setting is 'local_audio_backend' — send \
         {{\"local_audio_backend\": \"...\"}} and pick a value from 'supported_audio_backends' \
         in GET /system/config."
    )
}

/// Le message rendu à qui règle un backend que CETTE machine ne sait pas
/// ouvrir : ce qui aurait eu lieu, et la liste des valeurs acceptées ici.
///
/// #1268 — le serveur publie la liste vraie depuis #2806, mais le sélecteur du
/// client web écrit toujours ses trois choix en dur (Auto/WASAPI/ASIO). Un
/// testeur sous Debian ou Fedora peut donc encore demander WASAPI, et le
/// serveur ne peut pas l'en empêcher depuis ici. Il peut en revanche refuser de
/// RETENIR un réglage qu'il n'honorera jamais, et dire lesquels il honore.
#[cfg(feature = "local-audio")]
pub(super) fn refus_backend_non_supporte(demande: &str) -> String {
    let acceptes: Vec<&str> = tune_core::outputs::local::supported_backends()
        .iter()
        .map(|b| b.value)
        .collect();
    let acceptes = acceptes.join(", ");
    format!(
        "'local_audio_backend' value '{demande}' is not available on this server's platform: the \
         local output would silently fall back to the default host and the setting would be shown \
         back as 'auto'. Accepted here: {acceptes} — the same list published as \
         'supported_audio_backends' in GET /system/config."
    )
}
pub(super) async fn get_config(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> Json<Value> {
    let lang = crate::i18n::lang_from_header(&headers);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        // Voir `BACKEND_ACTIF_PAS_UN_REGLAGE` : une ligne écrite sous ce nom
        // n'est le réglage de personne, et la publier ici la ferait passer
        // pour le réglage auprès du client qui lit ce nom en premier.
        if k == BACKEND_ACTIF_PAS_UN_REGLAGE {
            tracing::warn!(
                cle = BACKEND_ACTIF_PAS_UN_REGLAGE,
                valeur = %v,
                "reglage_fantome_non_publie"
            );
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    let defaults: Vec<(&str, Value)> = vec![
        ("api_port", json!(state.port)),
        ("stream_port", json!(state.port)),
        ("tidal_enabled", json!(true)),
        ("qobuz_enabled", json!(true)),
        ("youtube_enabled", json!(true)),
        ("spotify_enabled", json!(false)),
        ("deezer_enabled", json!(true)),
        ("amazon_music_enabled", json!(false)),
        ("discovery_enabled", json!(true)),
        ("zone_auto_create", json!(true)),
        ("squeezebox_enabled", json!(false)),
        ("db_engine", json!(state.backend.engine().as_str())),
        ("db_connected", json!(true)),
        ("metadata_readonly", json!(false)),
        // Default on (unchanged behaviour); scan.rs treats unset as enabled.
        // The web toggle writes "false" to opt out (JF Paquet).
        ("enrich_on_scan", json!(true)),
        // Folder → playlist discovery at scan time — opt-in (Frédéric).
        ("scan_folder_playlists", json!(false)),
        // Import of .m3u/.pls files found at scan time. A different feature
        // from the one above, and default ON since it always behaved that way.
        // The web toggle writes "false" to opt out (JP Borderies).
        ("scan_import_playlists", json!(true)),
        // Le mode PURE impose-t-il le volume à 100 % ? Inactif par défaut :
        // cocher « Audiophile » ne doit pas changer le niveau sans prévenir.
        ("audiophile_lock_volume", json!(false)),
        // Contribution de metadonnees enrichies (bios, images d'artistes) au
        // cloud communautaire. Opt-in STRICT : rien ne sort tant que
        // l'utilisateur n'a pas coche. Le libelle et la phrase qui dit ce qui
        // part sont plus bas, dans `community_contribution`.
        (
            tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY,
            json!(tune_core::cloud::consent::CONTRIBUTION_DEFAULT),
        ),
        ("quality_split", json!(true)),
        ("resample_policy", json!("none")),
        ("audio_buffer_kb", json!(256)),
        ("prebuffer_seconds", json!(1.0)),
        ("prefetch_mode", json!("30s")),
        // Plafond de la lecture aléatoire (#2901) : combien de pistes
        // « tout lire en aléatoire » enfile au maximum. Réglage audio
        // comme les trois ci-dessus, même mécanisme (settings + PATCH),
        // défaut 500 — la valeur que #2228 avait figée dans le code.
        (
            tune_core::playback::queue::SHUFFLE_MAX_TRACKS_KEY,
            json!(tune_core::playback::queue::SHUFFLE_MAX_TRACKS_DEFAULT),
        ),
        // ReplayGain application at playback. Off by default: it multiplies
        // every sample, so it must be an explicit choice, never a surprise.
        ("replaygain_mode", json!("off")),
        ("replaygain_preamp_db", json!(0.0)),
        ("replaygain_prevent_clipping", json!(true)),
        // Plafond dBTP de l'anti-écrêtage (#1694) : 0 (défaut, comportement
        // historique), -0.5 ou -1. Persisté par PATCH /config comme les
        // autres ; honoré dans `gain_factor` (tune-core).
        ("replaygain_true_peak_ceiling_db", json!(0.0)),
        // `replaygain_analysis_enabled` n'est PAS ici : il est publié plus bas
        // avec le bloc `replaygain_source`, par un `insert` inconditionnel qui
        // normalise en plus la valeur persistée (`"false"` → `false`). Une
        // entrée `or_insert` ici serait morte — la contre-épreuve de #1627 l'a
        // montrée : la retirer ne cassait aucun test.
        (
            "local_audio_backend",
            json!(state.config.local_audio_backend),
        ),
        (
            "local_exclusive_mode",
            json!(state.config.local_exclusive_mode),
        ),
    ];
    for (k, v) in defaults {
        config.entry(k.to_string()).or_insert(v);
    }

    // Le plafond de la lecture aléatoire, tel qu'il s'APPLIQUERA (#2901).
    //
    // `PATCH /config` persiste sans valider : `0`, `-1` ou `99999` peuvent
    // se trouver en base. `shuffle_all` les ramène dans les bornes à la
    // lecture ; l'affichage doit dire la MÊME chose, sinon l'utilisateur lit
    // un chiffre que le serveur n'honore pas. Même repli propre que
    // `local_audio_backend` juste en dessous : corrigé dans la RÉPONSE, pas
    // en base — on ne réécrit pas le choix de l'utilisateur derrière son dos.
    let plafond_effectif = tune_core::playback::queue::resolve_shuffle_max_tracks(
        config
            .get(tune_core::playback::queue::SHUFFLE_MAX_TRACKS_KEY)
            .map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            })
            .as_deref(),
    );
    config.insert(
        tune_core::playback::queue::SHUFFLE_MAX_TRACKS_KEY.to_string(),
        json!(plafond_effectif),
    );
    // Les bornes elles-mêmes ne sont pas un réglage : ce sont les valeurs
    // que le contrôle doit respecter. On les publie pour que le client web
    // n'ait pas à les écrire en dur, exactement comme `supported_audio_backends`
    // plus bas (#1268) — le client avait codé ses trois backends à la main et
    // les proposait sur des plateformes qui ne les avaient pas.
    config.insert(
        "shuffle_max_tracks_min".to_string(),
        json!(tune_core::playback::queue::SHUFFLE_MAX_TRACKS_FLOOR),
    );
    config.insert(
        "shuffle_max_tracks_max".to_string(),
        json!(tune_core::playback::queue::SHUFFLE_MAX_TRACKS_CEILING),
    );
    // #1268 — le sélecteur « Backend audio » du client web écrivait ses trois
    // choix en dur (Auto/WASAPI/ASIO) et les proposait tels quels sur Debian
    // et Fedora. On publie ici la liste vraie, filtrée par la plateforme du
    // serveur, pour que l'interface la lise au lieu de deviner.
    //
    // Repli propre : une valeur Windows persistée (bibliothèque migrée d'une
    // machine Windows vers Linux) est ramenée à `auto` dans la RÉPONSE — pas
    // en base : `select_host` joue déjà via le host par défaut pour toute
    // valeur inconnue, l'affichage doit dire la même chose au lieu de laisser
    // le sélecteur sur un choix qui n'existe plus.
    #[cfg(feature = "local-audio")]
    {
        let persisted_supported = config
            .get("local_audio_backend")
            .and_then(|v| v.as_str())
            .is_none_or(tune_core::outputs::local::backend_value_is_supported);
        if !persisted_supported {
            config.insert("local_audio_backend".to_string(), json!("auto"));
        }
        config.insert(
            "supported_audio_backends".to_string(),
            serde_json::to_value(tune_core::outputs::local::supported_backends())
                .unwrap_or_else(|_| json!([])),
        );
        // #2868 — la CAPACITÉ, à côté du RÉGLAGE `local_exclusive_mode` publié
        // plus haut. Même intention que `supported_audio_backends` (#1268) : le
        // client n'a pas à déduire d'un nom de plateforme si la bascule « mode
        // exclusif » a un sens, il le lit.
        //
        // Le prédicat lui-même était faux : il exigeait la feature `asio`,
        // alors que la branche WASAPI exclusive est compilée sur TOUT Windows.
        // Un Windows sans `asio` s'entendait donc répondre « non supporté »
        // pour une capacité qu'il avait.
        config.insert(
            "local_exclusive_mode_supported".to_string(),
            json!(tune_core::outputs::local::LocalOutput::supports_exclusive_mode()),
        );
    }
    #[cfg(not(feature = "local-audio"))]
    {
        config.insert("supported_audio_backends".to_string(), json!([]));
        // Sans `local-audio`, il n'y a pas de sortie locale du tout — donc pas
        // de mode exclusif. On le dit au lieu d'omettre la clé : une clé
        // absente se lit « je ne sais pas », pas « non ».
        config.insert("local_exclusive_mode_supported".to_string(), json!(false));
    }
    config
        .entry("server_version".to_string())
        .or_insert(json!(tune_core::version()));
    config
        .entry("server_engine".to_string())
        .or_insert(json!("rust"));
    // Ensure onboarding_completed is always present as a boolean
    let onboarding_complete = config
        .get("onboarding_complete")
        .and_then(|v| v.as_str())
        .map(|v| v == "true")
        .or_else(|| config.get("onboarding_complete").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    config
        .entry("onboarding_completed".to_string())
        .or_insert(json!(onboarding_complete));
    // DSD → LPCM streaming toggle (Settings → Lecture). PATCH stores it as a
    // raw "true"/"false" string; surface it as a real boolean (default false)
    // so the toggle reflects the persisted state.
    let dsd_lpcm_stream = config
        .get("dsd_lpcm_stream")
        .and_then(|v| v.as_str().map(|s| s == "true").or_else(|| v.as_bool()))
        .unwrap_or(false);
    config.insert("dsd_lpcm_stream".to_string(), json!(dsd_lpcm_stream));
    // Les TROIS modes ReplayGain de #1627 — « néant / tags du fichier /
    // calcul » — publiés comme UN seul fait, en LECTURE.
    //
    // Rien de nouveau n'est persisté et aucune sémantique ne bouge : les deux
    // axes existants (`replaygain_mode` × `replaygain_analysis_enabled`)
    // restent la seule vérité, et restent les seuls écrivables. Ce bloc dit
    // seulement lequel des trois modes en RÉSULTE, pour que l'interface cesse
    // d'avoir à recomposer la règle de son côté — et de la recomposer faux :
    // depuis #2496, « Désactivé » arrête aussi le balayage, ce qu'un client
    // qui lisait les deux réglages séparément ne pouvait pas savoir.
    //
    // `analysis_enabled` = l'état de la bascule ; `analysis_effective` = ce qui
    // se passe vraiment. Même distinction que `community_contribution`
    // ci-dessous, et pour la même raison : promettre une analyse qui n'aura
    // pas lieu est aussi trompeur que de la cacher.
    let rg_source_mode = tune_core::audio::replaygain::active_source_mode(&state.backend);
    let rg_analysis_effective = tune_core::audio::replaygain::analysis_enabled(&state.backend);
    //
    // `analysis_enabled` est publié ici et NULLE PART ailleurs : l'insertion
    // est inconditionnelle, donc elle publie le défaut (`true`) sur une base
    // fraîche ET normalise le `"false"` persisté en booléen. C'était le trou —
    // la clé était simplement absente de la réponse, et le client devait
    // deviner son défaut.
    let rg_analysis_enabled = config
        .get(tune_core::audio::replaygain::ANALYSIS_ENABLED_KEY)
        .and_then(|v| v.as_str().map(|s| s != "false").or_else(|| v.as_bool()))
        .unwrap_or(true);
    config.insert(
        tune_core::audio::replaygain::ANALYSIS_ENABLED_KEY.to_string(),
        json!(rg_analysis_enabled),
    );
    config.insert(
        "replaygain_source".to_string(),
        json!({
            "mode": rg_source_mode.as_str(),
            "analysis_enabled": rg_analysis_enabled,
            "analysis_effective": rg_analysis_effective,
            // Les deux réglages qui COMPOSENT ce mode, nommés pour que le
            // client sache quoi écrire au lieu de deviner les clés.
            "setting_keys": [
                tune_core::audio::replaygain::MODE_KEY,
                tune_core::audio::replaygain::ANALYSIS_ENABLED_KEY,
            ],
            "label": crate::i18n::t(&lang, &format!("settings.replayGainSource.{}", rg_source_mode.as_str())),
        }),
    );
    // Consentement de contribution. Deux valeurs, et elles ne disent pas la
    // meme chose :
    //   - `enabled`   : le choix de l'utilisateur, relu sur la valeur BRUTE en
    //     base avec le meme lecteur que le serveur (`est_vrai`). C'est l'etat
    //     de la bascule. Passer par la carte `config` deja re-typee ferait
    //     diverger les deux — `"1"` y devient un nombre, qu'aucun `as_bool` ne
    //     rattrape, et l'ecran afficherait « non » sur un reglage pose a oui.
    //   - `effective` : ce qui va REELLEMENT se passer, `TUNE_TELEMETRY`
    //     compris. Un exploitant qui a coupe la telemetrie a l'echelle de la
    //     machine ferme la porte pour tout le monde ; sans cette seconde
    //     valeur, l'ecran promettrait un envoi qui n'aura jamais lieu.
    let contribution_enabled = settings
        .get(tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| tune_core::cloud::consent::est_vrai(&v))
        .unwrap_or(tune_core::cloud::consent::CONTRIBUTION_DEFAULT);
    let contribution_effective = tune_core::cloud::consent::contribution_autorisee(&settings);
    config.insert(
        tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY.to_string(),
        json!(contribution_enabled),
    );
    // Le libelle et la phrase d'explication voyagent avec le reglage : le
    // client web n'a pas a re-decrire ce qui part, et les deux ne peuvent pas
    // diverger. Traduit dans la langue choisie dans l'app (Accept-Language).
    config.insert(
        "community_contribution".to_string(),
        json!({
            "setting_key": tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY,
            "enabled": contribution_enabled,
            "effective": contribution_effective,
            "default": tune_core::cloud::consent::CONTRIBUTION_DEFAULT,
            "label": crate::i18n::t(&lang, "settings.communityContribution.label"),
            "description": crate::i18n::t(&lang, "settings.communityContribution.description"),
        }),
    );
    // Derived boolean: web client checks discogs_token_set to display badge.
    // Check both the DB setting and the env/toml fallback so that users
    // who set TUNE_DISCOGS_TOKEN in .env or tune.toml also see it as configured.
    let discogs_token_set = config
        .get("discogs_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
        || state
            .config
            .discogs_token
            .as_deref()
            .is_some_and(|s| !s.is_empty());
    config.insert("discogs_token_set".to_string(), json!(discogs_token_set));
    // Appliance mode (Tune OS image): unlocks the host network settings UI.
    config.insert(
        "appliance".to_string(),
        json!(crate::routes::appliance::is_appliance()),
    );
    // Adresses d'accès depuis un autre appareil (Android ne résout pas .local :
    // l'IP est la seule voie universelle — harmonique131, forum-hifi p.25).
    config.insert("server_urls".to_string(), json!(server_urls(state.port)));
    // Nom de CETTE machine, affiché en permanence par l'interface (#2110).
    // Deux serveurs Tune sur un même réseau donnaient deux interfaces
    // identiques : Philippe et Alain ont conclu à une mise à jour ratée alors
    // qu'ils regardaient deux machines. Toujours présent, jamais vide — le
    // client peut l'afficher sans garde-fou.
    config.insert(
        "server_name".to_string(),
        json!(resolve_server_name(
            config.get("server_name").and_then(|v| v.as_str())
        )),
    );
    // Premium licensing info
    let license_state = state.license.license_state().await;
    let premium_tier = license_state.tier;
    let zone_limit = if premium_tier == tune_core::license::Tier::Premium {
        serde_json::Value::Null
    } else {
        json!(state.license.free_zone_limit())
    };
    let mut premium_features = serde_json::Map::new();
    for f in tune_core::license::Feature::all_premium() {
        let key = serde_json::to_value(f)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let enabled = state.license.check_feature(*f).await;
        premium_features.insert(key, json!(enabled));
    }
    // Masked license key: show only the last 4 characters.
    let license_key_masked = license_state.license_key.as_deref().map(|k| {
        if k.len() <= 4 {
            k.to_string()
        } else {
            let visible = &k[k.len() - 4..];
            let masked = "*".repeat(k.len() - 4);
            format!("{masked}{visible}")
        }
    });
    config.insert("premium_tier".to_string(), json!(premium_tier));
    config.insert(
        "premium_features".to_string(),
        Value::Object(premium_features),
    );
    config.insert("zone_limit".to_string(), zone_limit);
    config.insert("license_key_masked".to_string(), json!(license_key_masked));
    // Caviardage des secrets, EN DERNIER — après `discogs_token_set` et
    // `license_key_masked`, qui se calculent sur les valeurs en clair.
    //
    // Cette route recopie la table `settings` telle quelle : tout ce qu'une
    // fonctionnalité y écrit sort par ici. Il y avait à la place une liste de
    // deux retraits et trois sous-champs Qobuz nommés à la main, et elle avait
    // pris du retard sur ce que la table contient (#2793) — la graine Ed25519
    // d'un appairage AirPlay (`airplay2_pairing:<id>`) et les clés `tunedev_`
    // de l'API développeur (`developer_api_keys`) sortaient en clair. La règle
    // vit désormais dans `tune_core::secrets`, qui classe sur le NOM et couvre
    // donc aussi le réglage ajouté demain.
    tune_core::secrets::caviarder_carte(&mut config);
    Json(Value::Object(config))
}

pub(super) async fn get_settings(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| state.config.db_path.clone());
    let onboarding_completed = settings
        .get("onboarding_complete")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let theme = read_profile_pref(&settings, profile.id(), "theme");

    Json(json!({
        "music_dirs": music_dirs,
        "db_path": db_path,
        "web_dir": state.config.web_dir,
        "artwork_dir": state.config.artwork_dir,
        "port": state.port,
        "auto_scan": state.config.auto_scan,
        "onboarding_completed": onboarding_completed,
        "server_version": tune_core::version(),
        "server_engine": "rust",
        "theme": theme,
    }))
}

#[derive(Deserialize)]
pub(super) struct ConfigPatch(pub(super) serde_json::Map<String, Value>);

const FULL_VOLUME_CONFIRMATION_FIELD: &str = "_confirm_full_volume";

fn enables_volume_lock(body: &serde_json::Map<String, Value>) -> bool {
    body.get("audiophile_lock_volume")
        .is_some_and(|value| value.as_bool() == Some(true) || value.as_str() == Some("true"))
}

fn take_full_volume_confirmation(body: &mut serde_json::Map<String, Value>) -> bool {
    body.remove(FULL_VOLUME_CONFIRMATION_FIELD)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn volume_lock_confirmation_required(
    body: &serde_json::Map<String, Value>,
    already_enabled: bool,
    confirmed: bool,
) -> bool {
    enables_volume_lock(body) && !already_enabled && !confirmed
}

/// Le champ qui porte les trois modes de #1627 dans un `PATCH /config`.
///
/// Même nom que le bloc publié par `GET /config` : ce qui se lit se réécrit.
const REPLAYGAIN_SOURCE_FIELD: &str = "replaygain_source";

/// Traduit `replaygain_source` en les deux réglages qui EXISTENT (#1627).
///
/// Avant : `GET /config` savait dire lequel des trois modes était actif, mais
/// aucune route ne savait en POSER un. Le champ tombait dans la boucle
/// d'écriture générique de [`update_config`], qui persiste n'importe quelle
/// clé : `{"replaygain_source": "file_tags"}` créait une ligne morte
/// `replaygain_source = file_tags` dans `settings`, ne changeait aucun des
/// deux axes, et répondait `{"ok": true}`. Le client était renvoyé « c'est
/// fait » sur un réglage qui n'avait pas bougé.
///
/// Après : le champ est retiré du corps et remplacé par les deux clés que tout
/// le serveur lit déjà. Rien de nouveau n'est persisté, aucune migration,
/// aucun chemin d'application du gain n'est touché.
///
/// `granularite_persistee` est l'axe piste/album tel qu'il est en base ; un
/// `replaygain_mode` explicite dans le MÊME corps prime, ce qui permet de
/// changer la source et la granularité d'un seul appel.
fn expand_replaygain_source(
    values: &mut serde_json::Map<String, Value>,
    granularite_persistee: ReplayGainMode,
) -> Result<Option<ReplayGainSourceMode>, AppError> {
    let Some(brut) = values.remove(REPLAYGAIN_SOURCE_FIELD) else {
        return Ok(None);
    };
    // Deux formes acceptées : la chaîne (`"file_tags"`), et l'OBJET que
    // `GET /config` publie — un client qui relit la config puis la renvoie
    // entière nous le repasse tel quel, et ce va-et-vient honnête ne doit pas
    // finir en 400.
    let demande = match &brut {
        Value::String(s) => Some(s.as_str()),
        Value::Object(o) => o.get("mode").and_then(|m| m.as_str()),
        _ => None,
    };
    let Some(demande) = demande else {
        return Err(AppError::bad_request(
            "replaygain_source expects one of: off, file_tags, tags_then_analysis",
        ));
    };
    // Aucun repli : un mode inconnu est refusé, jamais réinterprété. Le
    // ReplayGain multiplie chaque échantillon — deviner y coûterait un niveau
    // faux envoyé vers un ampli.
    let Some(mode) = ReplayGainSourceMode::from_setting(demande) else {
        return Err(AppError::bad_request(format!(
            "unknown replaygain_source '{demande}': expected off, file_tags or tags_then_analysis"
        )));
    };
    let granularite = values
        .get(tune_core::audio::replaygain::MODE_KEY)
        .and_then(|v| v.as_str())
        .map(ReplayGainMode::from_setting)
        .unwrap_or(granularite_persistee);
    for (cle, valeur) in tune_core::audio::replaygain::source_mode_settings(mode, granularite) {
        values.insert(cle.to_string(), Value::String(valeur.to_string()));
    }
    Ok(Some(mode))
}

pub(super) async fn update_config(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> Result<impl IntoResponse, AppError> {
    let mut values = body.0;
    // #2265 — refuser d'inscrire le nom du backend ACTIF comme s'il était un
    // réglage. Aucun repli, aucune réinterprétation vers `local_audio_backend` :
    // les deux informations sont différentes, et deviner laquelle est demandée
    // reviendrait à changer la sortie audio sur un malentendu de vocabulaire.
    // Même discipline que `replaygain_source` plus bas — on refuse en disant
    // quoi envoyer.
    if values.contains_key(BACKEND_ACTIF_PAS_UN_REGLAGE) {
        return Err(AppError::bad_request(refus_backend_actif()));
    }
    // #1268 — et la même discipline pour le RÉGLAGE lui-même : une valeur que
    // la plateforme du serveur ne peut pas ouvrir n'est plus retenue.
    //
    // Elle l'était : la ligne s'installait en base, `select_host` ouvrait le
    // host par défaut, et `GET /system/config` la ramenait à `auto` dans sa
    // réponse — sans un mot. Le choix du testeur disparaissait donc en
    // silence, ce qui est exactement ce que le ticket demandait de trancher
    // (« refusé, ignoré, ou plus de son ? »). Il est désormais refusé, et le
    // refus nomme les valeurs acceptables ICI.
    //
    // Rien n'est réécrit en base : une ligne héritée d'une machine Windows
    // reste, et le diagnostic continue de la rapporter telle quelle (#1395).
    #[cfg(feature = "local-audio")]
    if let Some(demande) = values.get("local_audio_backend").and_then(|v| v.as_str())
        && !demande.trim().is_empty()
        && !tune_core::outputs::local::backend_value_is_supported(demande.trim())
    {
        tracing::warn!(demande = %demande, "reglage_backend_non_supporte_refuse");
        return Err(AppError::bad_request(refus_backend_non_supporte(
            demande.trim(),
        )));
    }
    let full_volume_confirmed = take_full_volume_confirmation(&mut values);
    let volume_lock_was_enabled =
        tune_core::audio::audiophile::global_volume_lock_enabled(&state.backend);
    if volume_lock_confirmation_required(&values, volume_lock_was_enabled, full_volume_confirmed) {
        tracing::warn!("audiophile_volume_lock_confirmation_required");
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "full_volume_confirmation_required",
                "message": "Enabling the PURE volume lock can set a device volume to 100%. Explicit confirmation is required.",
            })),
        )
            .into_response());
    }

    // #1627 — le sélecteur à trois valeurs, traduit vers les deux axes AVANT
    // la boucle générique. Sans ce passage, le champ serait persisté tel quel
    // comme une ligne morte et le mode ne bougerait pas.
    let source_appliquee = expand_replaygain_source(
        &mut values,
        tune_core::audio::replaygain::ReplayGainSettings::load(&state.backend).mode,
    )?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    for (key, value) in values {
        let str_val = if value.is_string() {
            value
                .as_str()
                .ok_or_else(|| AppError::bad_request("expected string"))?
                .to_string()
        } else {
            value.to_string()
        };
        if let Err(e) = settings.set(&key, &str_val) {
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
    }
    let mut reponse = json!({"ok": true});
    // Écho du mode réellement posé : le client n'a pas à relire `GET /config`
    // pour savoir si sa demande a été comprise.
    if let Some(mode) = source_appliquee {
        reponse[REPLAYGAIN_SOURCE_FIELD] = json!(mode.as_str());
    }
    Ok(Json(reponse).into_response())
}

#[cfg(test)]
mod nom_du_serveur_tests {
    use super::resolve_server_name;

    /// Le réglage prime, espaces compris : c'est le nom que l'utilisateur lit.
    #[test]
    fn le_reglage_prime_sur_le_nom_d_hote() {
        assert_eq!(resolve_server_name(Some("Salon")), "Salon");
        assert_eq!(resolve_server_name(Some("  Salon  ")), "Salon");
    }

    /// Absent OU vide ⇒ nom d'hôte réel. Le cas « vide » compte : le vidage du
    /// champ dans l'interface écrit une chaîne vide dans `settings`, il ne
    /// supprime pas la clé. Sans ce filtre, l'étiquette s'afficherait vide.
    #[test]
    fn le_defaut_est_le_nom_d_hote_du_systeme() {
        let attendu = tune_core::discovery::system_hostname();
        assert!(
            !attendu.is_empty(),
            "system_hostname() ne doit jamais rendre une chaîne vide : \
             c'est le défaut sur lequel l'étiquette s'appuie"
        );
        assert_eq!(resolve_server_name(None), attendu);
        assert_eq!(resolve_server_name(Some("")), attendu);
        assert_eq!(resolve_server_name(Some("   ")), attendu);
    }

    /// Contre-épreuve du défaut : le nom d'hôte doit distinguer deux machines,
    /// donc ne jamais être un identifiant technique ni une constante partagée.
    #[test]
    fn le_defaut_n_est_ni_un_uuid_ni_une_constante_de_marque() {
        let defaut = resolve_server_name(None);
        assert_ne!(
            defaut, "Tune Server",
            "« Tune Server » est le nom UPnP, identique sur les deux machines : \
             il ne désambiguïse rien"
        );
        assert_ne!(defaut, "Local", "« Local » ne nomme aucune machine");
        let ressemble_a_un_uuid =
            defaut.len() == 36 && defaut.chars().filter(|c| *c == '-').count() == 4;
        assert!(
            !ressemble_a_un_uuid,
            "le défaut ne doit pas être un UUID : l'humain doit pouvoir le lire"
        );
    }
}

#[cfg(test)]
mod volume_lock_confirmation_tests {
    use super::{
        FULL_VOLUME_CONFIRMATION_FIELD, enables_volume_lock, take_full_volume_confirmation,
        volume_lock_confirmation_required,
    };
    use serde_json::{Map, json};

    #[test]
    fn detecte_uniquement_l_armement_du_verrou() {
        let mut enable = Map::new();
        enable.insert("audiophile_lock_volume".into(), json!(true));
        assert!(enables_volume_lock(&enable));
        assert!(volume_lock_confirmation_required(&enable, false, false));
        assert!(!volume_lock_confirmation_required(&enable, false, true));
        assert!(!volume_lock_confirmation_required(&enable, true, false));

        enable.insert("audiophile_lock_volume".into(), json!("true"));
        assert!(enables_volume_lock(&enable));

        let mut disable = Map::new();
        disable.insert("audiophile_lock_volume".into(), json!(false));
        assert!(!enables_volume_lock(&disable));
        assert!(!volume_lock_confirmation_required(&disable, true, false));

        let mut unrelated = Map::new();
        unrelated.insert("theme".into(), json!("dark"));
        assert!(!enables_volume_lock(&unrelated));
    }

    #[test]
    fn le_temoin_de_confirmation_est_reserve_et_non_persistable() {
        let mut patch = Map::new();
        patch.insert(FULL_VOLUME_CONFIRMATION_FIELD.into(), json!(true));
        assert!(take_full_volume_confirmation(&mut patch));
        assert!(!patch.contains_key(FULL_VOLUME_CONFIRMATION_FIELD));
    }
}

#[derive(Deserialize)]
pub(super) struct ThemeRequest {
    theme: String,
}

pub(super) async fn set_theme(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<ThemeRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    write_profile_pref(&settings, profile.id(), "theme", &body.theme);
    Json(json!({ "theme": body.theme }))
}

pub(super) async fn get_theme(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let theme = read_profile_pref(&settings, profile.id(), "theme");
    Json(json!({ "theme": theme }))
}

pub(super) async fn get_env(State(state): State<AppState>) -> Json<Value> {
    // Report what the server actually resolved, not the raw environment: the
    // old version fell back to a hard-coded "tune.db" and to port 8085, so a
    // support page could confidently name a database the server had never
    // opened — and named a SQLite file even on a PostgreSQL deployment.
    let engine = match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => "postgres",
        tune_core::db::engine::Engine::Sqlite => "sqlite",
    };
    Json(json!({
        "TUNE_PORT": state.port.to_string(),
        "TUNE_DB_PATH": state.db.as_ref().map(|_| state.config.db_path.clone()),
        "engine": engine,
    }))
}

pub(super) async fn get_mode(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mode = settings
        .get("server_mode")
        .ok()
        .flatten()
        .unwrap_or_else(|| "server".into());
    Json(json!({ "mode": mode }))
}

#[derive(Deserialize)]
pub(super) struct SetMode {
    mode: String,
}

pub(super) async fn set_mode(
    State(state): State<AppState>,
    Json(body): Json<SetMode>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("server_mode", &body.mode).ok();
    Json(json!({ "mode": body.mode }))
}

#[derive(Deserialize)]
pub(super) struct ExportConfigQuery {
    #[serde(default)]
    include_secrets: bool,
}

/// `GET /system/config/export` — sauvegarde de la table `settings`.
///
/// **Réservée à l'administrateur** (#2793). Sans `RequireAdmin`, le
/// middleware d'authentification se contentait de vérifier qu'un jeton était
/// valide : n'importe quel compte, même sans rôle, obtenait le dump complet —
/// et `?include_secrets=true` le lui rendait en clair, secret de signature JWT
/// compris. `RequireAdmin` laisse passer sans condition quand l'authentification
/// est désactivée (`auth.rs:502`), donc l'installation mono-utilisateur, qui est
/// le cas courant, ne voit aucun changement.
pub(super) async fn export_config(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Query(q): Query<ExportConfigQuery>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    // By default, omit secrets so a shared or leaked backup file carries no
    // credentials. import_config merges (it only sets keys present in the
    // payload), so restoring a redacted backup to the SAME server leaves the
    // existing secrets untouched. Pass ?include_secrets=true for a full backup
    // when migrating to a fresh server.
    //
    // La liste de trois retraits nommés à la main a été remplacée par la même
    // règle que `get_config` : c'était la seconde des « listes partielles » de
    // #2793, et elle ne connaissait ni la graine AirPlay ni les clés
    // développeur.
    //
    // On RETIRE, on ne masque pas — c'est la différence avec `get_config`, et
    // elle est délibérée : une sauvegarde se ré-importe. Poser `********` à la
    // place de `jwt_secret` écraserait le vrai secret de signature à la
    // restauration ; l'absence de la clé, elle, est ce que `import_config` sait
    // déjà ignorer.
    if !q.include_secrets {
        tune_core::secrets::retirer_les_secrets(&mut config);
    }
    Json(Value::Object(config))
}

/// `POST /system/config/import` — restauration de réglages.
///
/// **Réservée à l'administrateur** (#2793) : la route appelait `settings.set`
/// sur chaque clé reçue, donc un utilisateur standard pouvait poster
/// `{"auth_enabled": "false"}` et éteindre l'authentification du serveur.
///
/// L'application est en DEUX TEMPS : tout le corps est validé et converti
/// d'abord, et rien n'est écrit tant qu'une entrée est refusée. Avant, la
/// validation vivait dans la boucle d'écriture, donc un corps dont la dixième
/// entrée était invalide laissait les neuf premières appliquées.
///
/// Une écriture qui échoue en cours de route est désormais DITE (`500`) avec
/// le nombre de clés déjà appliquées, au lieu d'être avalée par un
/// `if ….is_ok()` qui rendait `200` et un compte silencieusement trop bas :
/// l'appelant croyait sa restauration complète.
pub(super) async fn import_config(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<impl IntoResponse, AppError> {
    let mut a_ecrire: Vec<(String, String)> = Vec::with_capacity(body.len());
    for (key, value) in body {
        if key.trim().is_empty() {
            return Err(AppError::bad_request("empty setting key"));
        }
        let str_val = match value {
            Value::String(s) => s,
            other => other.to_string(),
        };
        a_ecrire.push((key, str_val));
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut imported = 0;
    for (key, str_val) in a_ecrire {
        settings.set(&key, &str_val).map_err(|e| {
            AppError::internal(format!(
                "import stopped after {imported} settings: writing '{key}' failed: {e}"
            ))
        })?;
        imported += 1;
    }
    Ok(Json(json!({ "imported": imported })))
}

// ---------------------------------------------------------------------------
// Default zone
// ---------------------------------------------------------------------------

pub(super) async fn get_default_zone(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let zone_id: Option<i64> = settings
        .get("default_zone_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    Json(json!({ "zone_id": zone_id }))
}

#[derive(Deserialize)]
pub(super) struct DefaultZoneBody {
    zone_id: Option<i64>,
}

pub(super) async fn set_default_zone(
    State(state): State<AppState>,
    Json(body): Json<DefaultZoneBody>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match body.zone_id {
        Some(id) => {
            settings.set("default_zone_id", &id.to_string()).ok();
            Json(json!({ "zone_id": id }))
        }
        None => {
            settings.delete("default_zone_id").ok();
            Json(json!({ "zone_id": null }))
        }
    }
}

pub(super) async fn clear_cache(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("scan_result", "{}").ok();
    Json(json!({ "cleared": true }))
}

pub(super) async fn get_music_dirs(State(state): State<AppState>) -> Json<Value> {
    let dirs = super::get_music_dirs_list(&state.backend);
    Json(json!({ "dirs": dirs }))
}

#[derive(Deserialize)]
pub(super) struct BrowseDirsQuery {
    path: Option<String>,
}

/// Explorateur de dossiers servi par le serveur (#1275) — la moitié serveur du
/// sélecteur de dossiers des réglages Bibliothèque.
///
/// C'est une route de LECTURE DU SYSTÈME DE FICHIERS de la machine serveur.
/// Elle porte donc deux gardes, et pas une :
///
/// 1. **Le rôle.** `RequireAdmin`, comme la route d'écriture qu'elle alimente
///    (`POST /system/music-dirs`). Elle en était dépourvue : n'importe quel
///    porteur de jeton — y compris un compte créé par `/auth/register`, qui
///    est public et ne crée que des non-administrateurs — pouvait énumérer le
///    disque, alors que le geste qu'elle prépare, lui, exige admin.
/// 2. **Le périmètre.** Le rôle ne suffit pas : sur une installation par
///    défaut `auth_enabled` est absent, `RequireAdmin` laisse donc passer, et
///    la route redevient anonyme sur le réseau local. Les arbres système sont
///    refusés indépendamment de l'authentification — voir
///    [`super::explorateur`] pour le périmètre retenu et sa justification.
pub(super) async fn browse_dirs(
    _admin: crate::auth::RequireAdmin,
    Query(q): Query<BrowseDirsQuery>,
) -> (StatusCode, Json<Value>) {
    use super::explorateur;

    let base = q.path.unwrap_or_else(|| {
        if cfg!(target_os = "windows") {
            "C:\\".into()
        } else {
            "/".into()
        }
    });

    if let Err(refus) = explorateur::verifier_le_chemin_demande(&base) {
        tracing::warn!(path = %base, motif = ?refus, "browse_dirs_refuse");
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "dirs": [], "parent": null, "current": base, "error": refus.libelle(),
            })),
        );
    }

    let base_path = std::path::Path::new(&base);
    if !base_path.exists() || !base_path.is_dir() {
        // Un seul et même refus pour « n'existe pas » et « existe mais n'est
        // pas un dossier » : les distinguer donnerait de quoi sonder la
        // présence d'un fichier sans jamais le lire.
        return (
            StatusCode::OK,
            Json(
                json!({ "dirs": [], "parent": null, "current": base, "error": "not a directory" }),
            ),
        );
    }
    // Le texte du chemin est irréprochable ; sa CIBLE peut ne pas l'être — un
    // lien symbolique posé dans une racine de bibliothèque suffit.
    if !explorateur::la_cible_reste_dans_le_perimetre(base_path) {
        tracing::warn!(path = %base, "browse_dirs_refuse_cible_hors_perimetre");
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "dirs": [], "parent": null, "current": base,
                "error": explorateur::Refus::ArbreSysteme.libelle(),
            })),
        );
    }

    let parent = base_path.parent().map(|p| p.to_string_lossy().to_string());

    let mut dirs: Vec<Value> = Vec::new();

    // On Windows, list drives when at root
    #[cfg(target_os = "windows")]
    if base == "C:\\" || base == "\\" || base == "/" {
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if std::path::Path::new(&drive).exists() {
                dirs.push(json!({
                    "name": format!("{} Drive", letter as char),
                    "path": drive,
                    "has_children": true,
                }));
            }
        }
        return (
            StatusCode::OK,
            Json(json!({ "dirs": dirs, "parent": null, "current": base })),
        );
    }

    if let Ok(entries) = std::fs::read_dir(base_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden dirs and system dirs
            if name.starts_with('.')
                || name == "$RECYCLE.BIN"
                || name == "System Volume Information"
            {
                continue;
            }
            // Les arbres système disparaissent aussi de la LISTE, pas seulement
            // de la navigation : les énumérer les nomme, et nommer `/root` ou
            // `C:\ProgramData` sur une machine de réseau local est déjà la
            // moitié d'une reconnaissance. Le filtre passe avant le sondage
            // `has_children`, qui sinon irait lire `/proc` et `/sys`.
            let texte = path.to_string_lossy();
            if explorateur::dans_un_arbre_systeme(&texte) {
                continue;
            }
            // Un lien symbolique ne coûte une forme canonique que s'il en est
            // un : la calculer pour chaque entrée d'une racine réseau serait
            // payer un aller-retour par dossier.
            if entry.file_type().is_ok_and(|t| t.is_symlink())
                && !explorateur::la_cible_reste_dans_le_perimetre(&path)
            {
                continue;
            }
            let has_children = std::fs::read_dir(&path)
                .map(|mut rd| rd.any(|e| e.is_ok_and(|e| e.path().is_dir())))
                .unwrap_or(false);
            dirs.push(json!({
                "name": name,
                "path": path.to_string_lossy(),
                "has_children": has_children,
            }));
        }
    }

    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });

    (
        StatusCode::OK,
        Json(json!({
            "dirs": dirs,
            "parent": parent,
            "current": base_path.to_string_lossy(),
        })),
    )
}

#[derive(Deserialize)]
pub(super) struct AddMusicDir {
    path: String,
}

pub(super) async fn add_music_dir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);

    if normalized.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path is empty" })),
        )
            .into_response());
    }

    let path = std::path::Path::new(&normalized);
    if !path.exists() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "directory does not exist",
                "path": normalized,
            })),
        )
            .into_response());
    }
    if !path.is_dir() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "path is not a directory",
                "path": normalized,
            })),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let newly_added = !dirs.contains(&normalized);
    if newly_added {
        dirs.push(normalized);
    }

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();

    // Scan right away so the new folder's tracks appear without an app restart.
    // Previously add_music_dir only saved the path: the startup scan and the
    // file-watcher are both initialised once at boot with the old dir list, so a
    // folder added later was neither scanned nor watched — it only showed up
    // after a restart (Jean-Pierre).
    if newly_added {
        super::scan::spawn_library_scan(state.clone(), false, None).await;
    }
    Ok(Json(json!({ "dirs": dirs })).into_response())
}

#[derive(Deserialize)]
pub(super) struct RemoveMusicDir {
    path: String,
    /// Nombre de pistes que l'utilisateur accepte de perdre **avec** le
    /// dossier.
    ///
    /// Absent — le cas de tout client existant, qui n'envoie que `path` — le
    /// retrait ne supprime rien et se contente de dire ce qu'il laisse
    /// derrière lui. Présent, c'est le geste que le fil réclamait : « retirer
    /// un dossier des réglages devrait proposer de retirer aussi ce qu'il
    /// contenait », en un aller-retour au lieu de trois.
    ///
    /// Un NOMBRE et non un booléen, même contrat que `/scan?confirm_purge=N`
    /// (#1943) et que `/music-dirs/purge-orphans` : une confirmation prise sur
    /// un écran périmé ne peut pas autoriser une purge plus large que celle
    /// qui a été montrée.
    #[serde(default)]
    confirm_purge: Option<u64>,
}

/// `POST /system/music-dirs/remove` — retirer une racine, et ce qu'elle
/// emporte si on le demande.
///
/// # Le garde-fou, ici, n'est pas celui de `/purge-orphans`
///
/// `refus_de_purge` protège la route de rattrapage parce que celle-ci peut
/// NOMMER n'importe quel chemin sans que l'utilisateur ait rien retiré : un
/// montage tombé y est hors d'atteinte parce qu'il est, par définition,
/// encore dans `music_dirs`.
///
/// Ce raisonnement ne se transpose pas ici : le retrait EST le signal
/// utilisateur qui manquait à #2149, et il vient de s'exécuter. Ce qui
/// protège cet appel, c'est autre chose, et c'est plus fort :
///
/// 1. l'ensemble supprimé est calculé par [`orphelines_parmi`] **contre les
///    racines qui restent** — une racine imbriquée ou chevauchante ne peut pas
///    perdre une piste, sans qu'aucun refus n'ait à l'intercepter ;
/// 2. `confirm_purge` doit couvrir le nombre EXACT constaté, sinon le plafond
///    de #1943 ([`super::scan::purge_refusee`]) refuse tout ;
/// 3. le disque n'est jamais consulté — ni son état ni sa lisibilité n'entrent
///    dans la décision, exactement comme sur `/purge-orphans`.
///
/// Le retrait lui-même réussit toujours : il rend 200 avec la nouvelle liste.
/// Un refus de purge se lit dans le corps (`purge_refused`), jamais dans le
/// code HTTP — sans quoi un client conclurait que le dossier est encore là.
pub(super) async fn remove_music_dir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RemoveMusicDir>,
) -> Result<Json<Value>, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    dirs.retain(|d| {
        let norm_d = tune_core::scanner::walker::normalize_path(d);
        norm_d != normalized
    });

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();

    // Les pistes DEVENUES orphelines : sous le dossier retiré, et sous aucune
    // des racines qui RESTENT. Le compte se mesurait auparavant sur le seul
    // dossier retiré : retirer `/media/disque` alors que
    // `/media/disque/Classique` reste configuré annonçait les pistes de
    // `Classique` comme orphelines, puis `/purge-orphans` refusait en bloc
    // (`ContientUneRacine`) — l'utilisateur restait devant un nombre qu'aucun
    // geste ne pouvait honorer. C'est l'angle mort de #2149.
    let orphelines = pistes_orphelines_sous(&state, &normalized, &dirs);
    let plan = impact(&state, &orphelines);

    // Sans confirmation : on DIT, on ne touche à rien. Comportement de tout
    // client existant, inchangé.
    let Some(confirmee) = body.confirm_purge else {
        if plan.tracks > 0 {
            tracing::info!(
                dossier = %normalized,
                pistes = plan.tracks,
                "music_dir_removed_tracks_left_behind — ces pistes ne sont plus sous aucune \
                 racine configurée. Le scan ne les visitera plus et ne les purgera jamais \
                 (HorsPerimetre, #1943) : seul un geste explicite — ce même appel avec \
                 confirm_purge=N, ou /music-dirs/purge-orphans — peut les retirer."
            );
        }
        return Ok(Json(json!({
            "dirs": dirs,
            "orphan_tracks": plan.tracks,
            "impact": impact_json(&plan),
            "confirm_purge_required": plan.tracks,
        })));
    };

    if orphelines.is_empty() {
        return Ok(Json(json!({
            "dirs": dirs,
            "orphan_tracks": 0,
            "purged": 0,
            "purge_refused": false,
            "impact": impact_json(&plan),
        })));
    }

    // Le plafond de #1943 s'applique à ce geste comme aux autres, par la
    // fonction de PRODUCTION et non par une copie.
    let total_local = pistes_locales(&state).len();
    if super::scan::purge_refusee(orphelines.len(), total_local, Some(confirmee)) {
        tracing::error!(
            dossier = %normalized,
            candidats = orphelines.len(),
            total_local,
            confirmee,
            "music_dir_removed_purge_refusee — la confirmation ne couvre pas l'ampleur \
             constatée. Le dossier est retiré des réglages ; aucune piste n'a été supprimée."
        );
        return Ok(Json(json!({
            "dirs": dirs,
            "orphan_tracks": plan.tracks,
            "purged": 0,
            "purge_refused": true,
            "purge_refused_reason": "confirmation_insuffisante",
            "confirm_purge_required": plan.tracks,
            "impact": impact_json(&plan),
            "message": format!(
                "Le dossier a bien été retiré des réglages. En revanche la confirmation \
                 reçue ne couvre pas les {} pistes concernées : aucune n'a été supprimée. \
                 Confirmez ce nombre exact pour les retirer aussi.",
                plan.tracks
            ),
        })));
    }

    let r = executer_purge(&state, &orphelines);
    tracing::warn!(
        dossier = %normalized,
        purgees = r.purgees,
        albums_orphelins = r.albums_orphelins,
        artistes_orphelins = r.artistes_orphelins,
        favoris_rerattaches = r.favoris_rerattaches,
        favoris_non_resolus = r.favoris_non_resolus,
        masques_rerattaches = r.masques_rerattaches,
        masques_non_resolus = r.masques_non_resolus,
        paires_distinctes_rerattachees = r.paires_distinctes_rerattachees,
        paires_distinctes_non_resolues = r.paires_distinctes_non_resolues,
        "music_dir_removed_avec_purge — retrait du dossier et suppression de son contenu, \
         explicitement confirmée par l'utilisateur (#2149)."
    );

    Ok(Json(json!({
        "dirs": dirs,
        "orphan_tracks": plan.tracks,
        "purged": r.purgees,
        "purge_refused": false,
        "orphan_albums_removed": r.albums_orphelins,
        "orphan_artists_removed": r.artistes_orphelins,
        "favorites_relinked": r.favoris_rerattaches,
        "favorites_unresolved": r.favoris_non_resolus,
        "hidden_relinked": r.masques_rerattaches,
        "hidden_unresolved": r.masques_non_resolus,
        "distinct_pairs_relinked": r.paires_distinctes_rerattachees,
        "distinct_pairs_unresolved": r.paires_distinctes_non_resolues,
        "impact": impact_json(&plan),
    })))
}

// ───────────────────────────────────────────────────────────────────────────
// Purge des pistes hors périmètre — le geste explicite qui manquait (#2149)
// ───────────────────────────────────────────────────────────────────────────
//
// ## Le défaut
//
// `remove_music_dir` retire la racine des réglages et s'arrête là. La purge de
// fin de scan, elle, classe toute piste qui n'est sous AUCUNE racine
// configurée en `VerdictPurge::HorsPerimetre` et la CONSERVE — délibérément,
// c'est le garde-fou de #1943 par lequel 21 277 pistes de Yacine étaient
// parties. Les deux comportements sont justes ; leur composition ne l'est pas :
// une racine retirée emmène ses pistes dans un angle mort permanent. Elles ne
// sont plus visitées, ne peuvent plus être purgées, et restent affichées avec
// des chemins morts (Rhorn, 0.9.75, bibliothèque migrée d'un NAS à un autre).
//
// ## Ce qui n'est PAS fait ici
//
// `verdict_purge` n'est pas touché. L'assouplir rouvrirait #1943 : une racine
// ABSENTE au scan et une racine RETIRÉE par l'utilisateur produisent le même
// état en base et ne veulent pas dire la même chose. Ce qui manquait n'est pas
// une permission de plus donnée au scan, c'est un SIGNAL utilisateur.
//
// ## Deux portes d'entrée, UNE suppression
//
// - `POST /music-dirs/remove` avec `confirm_purge=N` — le geste en une fois,
//   pour qui retire un dossier maintenant ;
// - `POST /music-dirs/purge-orphans` — le rattrapage, pour qui a retiré son
//   ancien NAS il y a trois versions et ne peut plus le désigner par un
//   retrait (le cas de Rhorn).
//
// Les deux calculent l'ensemble à supprimer par la même fonction
// [`orphelines_parmi`] et l'exécutent par le même [`executer_purge`]. Un
// second chemin de purge, c'était la garantie que l'un des deux oublierait les
// albums vides, les favoris ou les marqueurs de masquage.
//
// `orphelines_parmi` intersecte toujours avec les racines qui RESTENT : une
// racine imbriquée ou chevauchante ne peut pas perdre une piste **par
// construction**, et pas seulement parce qu'un refus l'a interceptée. C'est ce
// qui manquait au premier correctif : le compte rendu par `remove_music_dir`
// se mesurait sur le seul dossier retiré, annonçait des pistes vivantes comme
// orphelines, et le nettoyage promis se heurtait ensuite à
// `RefusPurge::ContientUneRacine`.
//
// ## Le garde-fou, et pourquoi il est structurel
//
// Un dossier peut être momentanément indisponible — montage réseau décroché,
// disque débranché. Ce cas ne doit JAMAIS coûter une piste. La protection ici
// n'est pas une heuristique de lisibilité mais une propriété de forme :
//
//   **cette route refuse toute cible qui est encore dans le périmètre.**
//
// Une racine momentanément illisible est TOUJOURS encore dans `music_dirs` —
// c'est ce qui la définit : personne ne l'a retirée. Ses pistes sont donc hors
// d'atteinte de cette route, quel que soit le corps de la requête, et sans que
// le disque soit consulté une seule fois. Le système de fichiers n'entre pas
// dans la décision : ni son état, ni sa lisibilité ne peuvent changer ce qui
// est supprimé. C'est le seul garde-fou qu'un montage qui décroche ne peut pas
// contourner.
//
// Symétriquement, une cible AU-DESSUS d'une racine vivante est refusée :
// purger `/mnt` alors que `/mnt/nas/Musique` est configuré emporterait des
// pistes vivantes.
//
// ## Et le plafond de #1943 ?
//
// Il s'applique, avec confirmation CHIFFRÉE (`confirm_purge=N`), par la
// fonction de production `purge_refusee` — la même que le scan. Une
// suppression explicitement demandée reste une suppression irréversible.

/// Pourquoi une purge explicite est refusée.
///
/// Chaque variante est un refus **structurel** : il se décide sur la liste des
/// racines configurées et le chemin demandé, avant toute lecture de la base et
/// sans jamais toucher au disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusPurge {
    /// Chemin vide : on ne purge pas « tout ».
    CibleVide,
    /// La cible EST une racine configurée, ou vit SOUS une racine configurée.
    ///
    /// C'est le garde-fou central. Un montage tombé laisse sa racine dans
    /// `music_dirs` : ses pistes tombent donc toujours ici, et rien ne part.
    DansLePerimetre,
    /// La cible est AU-DESSUS d'une racine configurée : la purger emporterait
    /// des pistes vivantes.
    ContientUneRacine,
}

impl RefusPurge {
    pub(crate) fn motif(self) -> &'static str {
        match self {
            RefusPurge::CibleVide => "cible_vide",
            RefusPurge::DansLePerimetre => "dans_le_perimetre",
            RefusPurge::ContientUneRacine => "contient_une_racine",
        }
    }

    pub(crate) fn message(self, cible: &str) -> String {
        match self {
            RefusPurge::CibleVide => {
                "Aucun dossier n'a été indiqué. Cette opération retire des pistes \
                 définitivement : elle exige un chemin précis."
                    .to_string()
            }
            RefusPurge::DansLePerimetre => format!(
                "{cible} fait encore partie des dossiers de musique. Rien n'a été retiré. \
                 Un dossier momentanément indisponible — partage réseau décroché, disque \
                 débranché — est exactement dans ce cas : il reste configuré, et ses pistes \
                 sont conservées. Retirez d'abord le dossier des réglages si vous voulez \
                 vraiment vous séparer de son contenu."
            ),
            RefusPurge::ContientUneRacine => format!(
                "{cible} contient un dossier de musique encore configuré. Rien n'a été \
                 retiré : la purge y emporterait des pistes vivantes. Visez le dossier \
                 retiré lui-même, pas un de ses parents."
            ),
        }
    }
}

/// La cible est-elle purgeable, au vu des seules racines configurées ?
///
/// `None` = purgeable. Aucune E/S : c'est ce qui rend la protection
/// insensible à l'état du disque.
pub(crate) fn refus_de_purge(cible: &str, racines: &[String]) -> Option<RefusPurge> {
    let cible = cible.trim_end_matches(['/', '\\']);
    if cible.is_empty() {
        return Some(RefusPurge::CibleVide);
    }
    for r in racines {
        let r = tune_core::scanner::walker::normalize_path(r);
        let r = r.trim_end_matches(['/', '\\']);
        if r.is_empty() {
            continue;
        }
        if super::scan::sous_le_dossier(cible, r) {
            return Some(RefusPurge::DansLePerimetre);
        }
        if super::scan::sous_le_dossier(r, cible) {
            return Some(RefusPurge::ContientUneRacine);
        }
    }
    None
}

/// Regrouper des pistes hors périmètre sous le dossier le plus HAUT qui ne
/// contient QUE des pistes hors périmètre.
///
/// Sans ce repli, l'écran listerait un dossier par album. Avec lui, Rhorn voit
/// « /Volumes/AncienNAS — 4 212 pistes », c'est-à-dire la chose qu'il a
/// effectivement débranchée.
///
/// Le repli s'arrête net dès qu'un dossier porte encore une piste vivante :
/// une cible remontée trop haut serait de toute façon refusée par
/// [`refus_de_purge`], mais mieux vaut ne jamais la proposer.
pub(crate) fn regrouper_hors_perimetre(
    hors_perimetre: &[&str],
    vivantes: &[&str],
) -> Vec<(String, usize)> {
    use std::collections::{HashMap, HashSet};

    // Tout ancêtre d'une piste vivante est un dossier vivant : on ne remonte
    // jamais au-delà.
    let mut vivants: HashSet<&str> = HashSet::new();
    for p in vivantes {
        let mut cur = *p;
        while let Some(parent) = super::scan::dossier_parent(cur) {
            cur = parent;
            if !vivants.insert(cur) {
                break; // déjà marqué : ses ancêtres le sont aussi.
            }
        }
    }

    let mut groupes: HashMap<&str, usize> = HashMap::new();
    for p in hors_perimetre {
        let mut plus_haut: Option<&str> = None;
        let mut cur = *p;
        while let Some(parent) = super::scan::dossier_parent(cur) {
            if vivants.contains(parent) {
                break;
            }
            plus_haut = Some(parent);
            cur = parent;
        }
        if let Some(d) = plus_haut {
            *groupes.entry(d).or_insert(0) += 1;
        }
    }

    let mut sortie: Vec<(String, usize)> = groupes
        .into_iter()
        .map(|(d, n)| (d.to_string(), n))
        .collect();
    sortie.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sortie
}

/// `(id, file_path)` de toutes les pistes LOCALES.
fn pistes_locales(state: &AppState) -> Vec<(i64, String)> {
    state
        .backend
        .query_many(
            "SELECT id, file_path FROM tracks WHERE source = 'local' AND file_path IS NOT NULL",
            &[],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let p = r.get(1)?.as_string()?;
            if p.is_empty() { None } else { Some((id, p)) }
        })
        .collect()
}

/// Pistes qui vivent sous `dossier` **et sous aucune** des racines encore
/// configurées.
///
/// Fonction PURE : c'est elle qui décide ce qu'une purge emporte, et elle se
/// vérifie sans base ni disque.
///
/// # Pourquoi l'intersection avec les racines restantes
///
/// « Sous le dossier retiré » ne veut pas dire « orpheline ».
/// `music_dirs = ["/media/disque", "/media/disque/Classique"]` est un réglage
/// courant — on indexe un disque entier, puis on ajoute un sous-dossier pour
/// le traiter à part. Retirer `/media/disque` laisse `Classique` configuré :
/// ses pistes sont vivantes. Sans cette intersection, elles étaient comptées
/// comme orphelines par `remove_music_dir` (#2149), et l'utilisateur se
/// voyait proposer un nettoyage que `refus_de_purge` refusait ensuite en bloc
/// (`ContientUneRacine`) — l'angle mort entre les deux moitiés.
///
/// Une racine imbriquée ou chevauchante ne peut donc pas perdre une piste
/// **par construction**, et pas seulement par un refus en amont.
///
/// # Pourquoi en Rust et pas en SQL
///
/// Un `LIKE` sur un chemin Windows fait de l'antislash un caractère
/// d'échappement côté PostgreSQL, et c'est déjà ce qui avait rendu des
/// dossiers vides (#1753, #2016). `sous_le_dossier` — l'unique
/// implémentation, partagée avec `enrich_scope` (#1660) — accepte les deux
/// séparateurs et refuse un simple préfixe de nom.
pub(crate) fn orphelines_parmi(
    pistes: &[(i64, String)],
    dossier: &str,
    racines_restantes: &[String],
) -> Vec<i64> {
    let d = dossier.trim_end_matches(['/', '\\']);
    if d.is_empty() {
        return Vec::new();
    }
    let restantes: Vec<String> = racines_restantes
        .iter()
        .map(|r| tune_core::scanner::walker::normalize_path(r))
        .filter(|r| !r.trim_end_matches(['/', '\\']).is_empty())
        .collect();
    pistes
        .iter()
        .filter(|(_, p)| super::scan::sous_le_dossier(p, d))
        .filter(|(_, p)| {
            !restantes
                .iter()
                .any(|r| super::scan::sous_le_dossier(p, r.trim_end_matches(['/', '\\'])))
        })
        .map(|(id, _)| *id)
        .collect()
}

/// [`orphelines_parmi`] appliquée à la base.
fn pistes_orphelines_sous(
    state: &AppState,
    dossier: &str,
    racines_restantes: &[String],
) -> Vec<i64> {
    orphelines_parmi(&pistes_locales(state), dossier, racines_restantes)
}

/// Compter les lignes d'une table liée qui référencent ces pistes.
///
/// Les ids viennent de notre propre base et sont des `i64` : les interpoler
/// est sûr, et évite un placeholder par piste (SQLite plafonne à 999).
fn compter_liees(state: &AppState, sql_avant_in: &str, ids: &[i64]) -> i64 {
    let mut total = 0i64;
    for lot in ids.chunks(500) {
        let liste = lot
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("{sql_avant_in} ({liste})");
        if let Ok(Some(row)) = state.backend.query_one(&sql, &[]) {
            total += row.first().and_then(|v| v.as_i64()).unwrap_or(0);
        }
    }
    total
}

/// Ce que la purge emporterait, et ce qu'elle laisserait.
///
/// Rendu AVANT toute suppression, pour que l'écran puisse le montrer. Les
/// chiffres sont ceux des tables liées telles que le schéma les traite :
///
/// - `playlists` : `playlist_tracks` est en `ON DELETE CASCADE` et les clés
///   étrangères sont ACTIVES sous SQLite (`PRAGMA foreign_keys=ON`,
///   `db/sqlite.rs`). Les entrées disparaissent des listes de lecture ; les
///   listes elles-mêmes restent, éventuellement plus courtes. Aucune référence
///   pendante.
/// - `queue_items` : même cascade — les pistes quittent les files d'attente.
/// - `listen_history` : `ON DELETE SET NULL`. **L'historique n'est pas
///   effacé** : la ligne survit avec son titre et son artiste, et perd son
///   `track_id`. C'est aussi le filet de secours de la réconciliation des
///   favoris, qui y relit l'identité d'un item disparu.
/// - `favorites` : table POLYMORPHE (`item_type`/`item_id`), donc sans clé
///   étrangère — c'est là que naîtraient les références pendantes. On lance
///   `FavoritesReconciler::run(false)` après la purge : chaque favori orphelin
///   est re-rattaché par identité (chemin, puis titre+artiste) à la piste
///   vivante correspondante — le cas exact de Rhorn, dont la bibliothèque
///   existe toujours, sous un autre NAS. `false` = `delete_unresolved` :
///   **aucun favori n'est jamais supprimé par cette route.**
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImpactPurge {
    pub tracks: i64,
    pub playlists: i64,
    pub playlist_entries: i64,
    pub favorites: i64,
    pub history_entries: i64,
    pub queue_entries: i64,
}

fn impact(state: &AppState, ids: &[i64]) -> ImpactPurge {
    if ids.is_empty() {
        return ImpactPurge::default();
    }
    ImpactPurge {
        tracks: ids.len() as i64,
        playlists: compter_liees(
            state,
            "SELECT COUNT(DISTINCT playlist_id) FROM playlist_tracks WHERE track_id IN",
            ids,
        ),
        playlist_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM playlist_tracks WHERE track_id IN",
            ids,
        ),
        favorites: compter_liees(
            state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' AND item_id IN",
            ids,
        ),
        history_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM listen_history WHERE track_id IN",
            ids,
        ),
        queue_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM queue_items WHERE track_id IN",
            ids,
        ),
    }
}

/// Ce qu'une purge a emporté, et ce qu'elle a réparé.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResultatPurge {
    purgees: i64,
    albums_orphelins: i64,
    artistes_orphelins: i64,
    favoris_rerattaches: i64,
    favoris_non_resolus: i64,
    masques_rerattaches: i64,
    masques_non_resolus: i64,
    paires_distinctes_rerattachees: i64,
    paires_distinctes_non_resolues: i64,
}

/// **LE** chemin de purge — il n'en existe qu'un.
///
/// `POST /music-dirs/purge-orphans` (rattrapage d'un dossier retiré autrefois)
/// et `POST /music-dirs/remove` avec `confirm_purge` (le geste en une fois)
/// l'appellent tous les deux. Deux portes d'entrée, une seule suppression :
/// écrire un second chemin, c'était garantir que l'un des deux oublierait les
/// albums vides, les favoris ou les marqueurs de masquage.
///
/// L'appelant a déjà tranché *quoi* supprimer ([`orphelines_parmi`]) et *si*
/// l'utilisateur l'a confirmé ([`super::scan::purge_refusee`]). Ici on
/// exécute, et on répare les tables sans clé étrangère.
fn executer_purge(state: &AppState, ids: &[i64]) -> ResultatPurge {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut r = ResultatPurge::default();
    for id in ids {
        if track_repo.delete(*id).is_ok() {
            r.purgees += 1;
        }
    }

    // Les albums et artistes devenus vides partent avec elles — sans quoi la
    // bibliothèque garde des albums à zéro piste, ce que #593 avait déjà
    // montré à l'écran.
    r.albums_orphelins = AlbumRepo::with_backend(state.backend.clone())
        .delete_orphans()
        .unwrap_or(0);
    r.artistes_orphelins = ArtistRepo::with_backend(state.backend.clone())
        .cleanup_orphans()
        .unwrap_or(0);

    // `favorites` n'a pas de clé étrangère : sans cette réconciliation, les
    // cœurs pointeraient des ids morts. `false` = ne JAMAIS supprimer un
    // favori qu'on n'a pas su re-rattacher.
    let favoris = tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(
        state.backend.clone(),
    )
    .run(false)
    .unwrap_or_default();
    r.favoris_rerattaches = favoris.relinked as i64;
    r.favoris_non_resolus = favoris.unresolved as i64;

    // Même absence de clé étrangère, même règle pour les albums masqués
    // (#1391) : la purge fait mourir des `albums.id`, donc des marqueurs
    // `hidden_items` deviennent orphelins. On les re-rattache par identité —
    // c'est le cinquième ancrage de la réconciliation, après le démarrage,
    // `scan.rs`, `auto_scan.rs` et la purge d'orphelines de fin de scan.
    // `false` = on ne SUPPRIME jamais un marqueur ici : un album masqué que
    // l'on ne retrouve pas peut revenir au prochain scan, et la liste de
    // révision le montre entre-temps grâce à son instantané.
    let masques = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone())
        .reconcile(false)
        .unwrap_or_default();
    r.masques_rerattaches = masques.relinked as i64;
    r.masques_non_resolus = masques.unresolved as i64;

    // Même absence de clé étrangère, même règle pour les paires « ces deux
    // albums ne sont pas des doublons » (#1276) : la purge fait mourir des
    // `albums.id`, donc des paires deviennent orphelines. `false` = on ne
    // SUPPRIME jamais un arbitrage ici — le perdre laisserait
    // `merge-duplicates` fusionner (et supprimer) au prochain passage.
    let distinctes =
        tune_core::db::album_distinct_repo::AlbumDistinctRepo::with_backend(state.backend.clone())
            .reconcile(false)
            .unwrap_or_default();
    r.paires_distinctes_rerattachees = distinctes.relinked as i64;
    r.paires_distinctes_non_resolues = distinctes.unresolved as i64;

    r
}

fn impact_json(i: &ImpactPurge) -> Value {
    json!({
        "tracks": i.tracks,
        "playlists": i.playlists,
        "playlist_entries": i.playlist_entries,
        "favorites": i.favorites,
        "history_entries": i.history_entries,
        "queue_entries": i.queue_entries,
    })
}

/// `GET /system/music-dirs/orphans` — ce qui traîne hors du périmètre.
///
/// Lecture seule. C'est la moitié qui rattrape les dossiers **déjà** retirés :
/// proposer la purge au moment du retrait ne sert à rien à qui a retiré son
/// ancien NAS il y a trois versions.
pub(super) async fn orphan_tracks(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Json<Value> {
    let racines: Vec<String> = super::get_music_dirs_list(&state.backend)
        .iter()
        .map(|d| tune_core::scanner::walker::normalize_path(d))
        .collect();
    let toutes = pistes_locales(&state);

    // Une liste de racines VIDE ne veut pas dire « tout est orphelin » : elle
    // veut dire qu'on ne sait rien. Même prudence que `verdict_purge`.
    if racines.is_empty() {
        return Json(json!({
            "groups": [],
            "total": 0,
            "note": "Aucun dossier de musique n'est configuré : rien ne peut être déclaré \
                     hors périmètre.",
        }));
    }

    let dans_le_perimetre = |p: &str| racines.iter().any(|r| super::scan::sous_le_dossier(p, r));
    let hors_refs: Vec<&str> = toutes
        .iter()
        .filter(|(_, p)| !dans_le_perimetre(p))
        .map(|(_, p)| p.as_str())
        .collect();
    let vivantes_refs: Vec<&str> = toutes
        .iter()
        .filter(|(_, p)| dans_le_perimetre(p))
        .map(|(_, p)| p.as_str())
        .collect();

    let groupes = regrouper_hors_perimetre(&hors_refs, &vivantes_refs);
    let ids: Vec<i64> = toutes
        .iter()
        .filter(|(_, p)| !dans_le_perimetre(p))
        .map(|(id, _)| *id)
        .collect();

    Json(json!({
        "groups": groupes
            .iter()
            .map(|(d, n)| json!({ "path": d, "tracks": *n as i64 }))
            .collect::<Vec<_>>(),
        "total": ids.len() as i64,
        "impact": impact_json(&impact(&state, &ids)),
    }))
}

#[derive(Deserialize)]
pub(super) struct PurgeOrphans {
    path: String,
    /// Nombre de pistes que l'utilisateur accepte de perdre.
    ///
    /// Un NOMBRE, pas un booléen — même contrat que `?confirm_purge=N` sur
    /// `/scan` (#1943) : une confirmation périmée ne peut pas autoriser une
    /// purge plus large que celle qui a été montrée.
    #[serde(default)]
    confirm_purge: Option<u64>,
}

/// `POST /system/music-dirs/purge-orphans` — retirer les pistes d'un dossier
/// qui n'est plus dans le périmètre.
///
/// Sans `confirm_purge`, c'est un **essai à blanc** : rien n'est supprimé, on
/// rend le plan. C'est la forme que prend la promesse « retirer un dossier
/// devrait proposer de retirer aussi ce qu'il contenait » : l'écran retire le
/// dossier, appelle ceci pour obtenir les chiffres, les montre, et ne
/// rappelle avec `confirm_purge` que si l'utilisateur dit oui.
pub(super) async fn purge_orphan_tracks(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PurgeOrphans>,
) -> Result<impl IntoResponse, AppError> {
    let cible = tune_core::scanner::walker::normalize_path(&body.path);
    let racines = super::get_music_dirs_list(&state.backend);

    if let Some(refus) = refus_de_purge(&cible, &racines) {
        tracing::warn!(
            cible = %cible,
            motif = refus.motif(),
            "purge_orphelines_refusee — aucune piste retirée."
        );
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "purged": 0,
                "refused": true,
                "reason": refus.motif(),
                "message": refus.message(&cible),
            })),
        )
            .into_response());
    }

    // `refus_de_purge` a déjà garanti qu'aucune racine configurée ne vit sous
    // la cible : l'intersection est ici un no-op, et c'est voulu — une seule
    // définition de « piste orpheline » pour les deux portes d'entrée.
    let ids = pistes_orphelines_sous(&state, &cible, &racines);
    let plan = impact(&state, &ids);
    let total_local = pistes_locales(&state).len();

    if ids.is_empty() {
        return Ok(Json(json!({
            "purged": 0,
            "refused": false,
            "impact": impact_json(&plan),
            "message": format!("Aucune piste n'est enregistrée sous {cible}."),
        }))
        .into_response());
    }

    // Essai à blanc : pas de confirmation ⇒ pas de suppression.
    let Some(_) = body.confirm_purge else {
        return Ok(Json(json!({
            "purged": 0,
            "refused": false,
            "dry_run": true,
            "confirm_purge_required": plan.tracks,
            "impact": impact_json(&plan),
            "message": format!(
                "{} pistes seraient retirées définitivement. Rappelez cette route avec \
                 confirm_purge={} pour confirmer.",
                plan.tracks, plan.tracks
            ),
        }))
        .into_response());
    };

    // Le plafond de #1943 s'applique à une suppression explicite aussi — par
    // la fonction de PRODUCTION, pas par une copie.
    if super::scan::purge_refusee(ids.len(), total_local, body.confirm_purge) {
        tracing::error!(
            cible = %cible,
            candidats = ids.len(),
            total_local,
            confirmee = ?body.confirm_purge,
            "purge_orphelines_refusee_trop_massive — la confirmation ne couvre pas l'ampleur \
             constatée. Aucune piste retirée."
        );
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "purged": 0,
                "refused": true,
                "reason": "confirmation_insuffisante",
                "confirm_purge_required": plan.tracks,
                "impact": impact_json(&plan),
                "message": format!(
                    "La confirmation reçue ne couvre pas les {} pistes concernées. Rien n'a \
                     été retiré. Confirmez ce nombre exact pour poursuivre.",
                    plan.tracks
                ),
            })),
        )
            .into_response());
    }

    let r = executer_purge(&state, &ids);

    tracing::warn!(
        cible = %cible,
        purgees = r.purgees,
        albums_orphelins = r.albums_orphelins,
        artistes_orphelins = r.artistes_orphelins,
        favoris_rerattaches = r.favoris_rerattaches,
        favoris_non_resolus = r.favoris_non_resolus,
        masques_rerattaches = r.masques_rerattaches,
        masques_non_resolus = r.masques_non_resolus,
        paires_distinctes_rerattachees = r.paires_distinctes_rerattachees,
        paires_distinctes_non_resolues = r.paires_distinctes_non_resolues,
        "purge_orphelines_effectuee — suppression explicitement confirmée par l'utilisateur."
    );

    Ok(Json(json!({
        "purged": r.purgees,
        "refused": false,
        "orphan_albums_removed": r.albums_orphelins,
        "orphan_artists_removed": r.artistes_orphelins,
        "favorites_relinked": r.favoris_rerattaches,
        "favorites_unresolved": r.favoris_non_resolus,
        "hidden_relinked": r.masques_rerattaches,
        "hidden_unresolved": r.masques_non_resolus,
        "distinct_pairs_relinked": r.paires_distinctes_rerattachees,
        "distinct_pairs_unresolved": r.paires_distinctes_non_resolues,
        "impact": impact_json(&plan),
    }))
    .into_response())
}

/// `POST /system/stop` — arrêter le PROCESSUS serveur, sans toucher à la
/// machine. C'est le geste qui manquait sur un poste de bureau : « Éteindre »
/// est réservé aux appliances (il coupe toute la machine), « Redémarrer »
/// revient toujours — il n'y avait aucun moyen d'ARRÊTER Tune depuis
/// l'interface (Bertrand, 25/08, confirmé absent en Expert aussi).
///
/// Honnêteté : sur une installation supervisée (systemd `Restart=always`,
/// service Windows), le superviseur peut relancer le processus aussitôt —
/// l'écran le dit dans la confirmation.
pub(super) async fn stop(_admin: crate::auth::RequireAdmin) -> impl IntoResponse {
    tokio::spawn(async {
        // Laisser la réponse HTTP partir avant de mourir.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("server_stop_requested_from_ui");
        std::process::exit(0);
    });
    Json(json!({ "stopping": true }))
}

pub(super) async fn restart(_admin: crate::auth::RequireAdmin) -> impl IntoResponse {
    tokio::spawn(async {
        // Let the HTTP response flush before we swap the process image.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // UNIX: re-exec in place with execv (same PID) so the server actually
        // comes back WITHOUT relying on an external supervisor. The previous
        // `exit(0)` only recovered when something restarted us on exit (systemd
        // Restart=always) — on a bare/manual install with no supervisor (e.g.
        // Yacine's Synology DSM scheduled task) it just killed Tune and it never
        // came back. Same approach as the update flow (#528). The listening
        // socket is CLOEXEC so exec() releases port 8888 for the new image.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                // Ne pas rouvrir le navigateur au redémarrage : l'onglet existant
                // se reconnecte tout seul (Jean, forum #1236 — deux onglets).
                unsafe { std::env::remove_var("TUNE_OPEN_BROWSER") };
                tracing::info!(exe = %exe.display(), "restart_reexec");
                let err = std::process::Command::new(&exe).args(&args).exec();
                // exec() only returns on failure → fall back to spawn+exit so a
                // supervised deployment still recovers.
                tracing::warn!(error = %err, "restart_reexec_failed — falling back to spawn+exit");
                let _ = std::process::Command::new(&exe)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn();
            }
        }

        // WINDOWS: we can't exec() in place. A plain restart is NOT swapping the
        // binary (unlike the update flow, which must exit and let tune-update.bat
        // do the PID-gated swap), so we CAN relaunch the SAME exe ourselves:
        // spawn a fresh copy, then exit. Without this, `exit(0)` just killed Tune
        // on a bare Windows install with no supervisor (Mika, #1209: "Network
        // error: server unreachable" then "Failed to load zones" — the server
        // never came back and had to be relaunched by hand). The listening socket
        // is created non-inheritable (socket2 sets WSA_FLAG_NO_HANDLE_INHERIT), so
        // the child does NOT inherit it and this process's exit fully releases
        // port 8888; the child's bind() retries for ~20s (main.rs) to cover the
        // brief release window. On a supervised install the child simply races the
        // supervisor's relaunch and whichever loses exits cleanly on the bind
        // guard — no crash loop.
        #[cfg(windows)]
        {
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                tracing::info!(exe = %exe.display(), "restart_windows_spawn");
                match std::process::Command::new(&exe)
                    .args(&args)
                    // Onglet existant déjà connecté — pas de nouvel onglet (#1236).
                    .env_remove("TUNE_OPEN_BROWSER")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                {
                    Ok(child) => {
                        tracing::info!(pid = child.id(), "restart_windows_new_process_spawned");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "restart_windows_spawn_failed — manual restart required");
                    }
                }
                // Give the child a moment to start before we release the port.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}

// ---------------------------------------------------------------------------
// Metadata fields configuration
// ---------------------------------------------------------------------------

/// Full catalog of available extended metadata fields.
/// (key, label_fr, category, scope)
///
/// `scope` says which entity a field belongs to — "track", "album" or "both" —
/// so clients can build track/album editors from the catalog instead of
/// hardcoding their own whitelist (the web UI's ALBUM_RELEVANT_KEYS).
const METADATA_FIELDS: &[(&str, &str, &str, &str)] = &[
    // Identification
    (
        "album_artist",
        "Artiste de l'album",
        "Identification",
        "both",
    ),
    ("sort_artist", "Tri artiste", "Identification", "both"),
    ("sort_album", "Tri album", "Identification", "album"),
    ("disc_number", "N° disque", "Identification", "track"),
    (
        "disc_subtitle",
        "Sous-titre disque",
        "Identification",
        "track",
    ),
    ("track_number", "N° piste", "Identification", "track"),
    ("genre", "Genre", "Identification", "both"),
    ("genres", "Genres (multi)", "Identification", "both"),
    ("year", "Année", "Identification", "both"),
    // Crédits
    ("composer", "Compositeur", "Crédits", "both"),
    ("conductor", "Chef d'orchestre", "Crédits", "both"),
    ("lyricist", "Parolier", "Crédits", "both"),
    ("performer", "Interprète", "Crédits", "both"),
    ("remixer", "Remixeur", "Crédits", "both"),
    ("label", "Label", "Crédits", "both"),
    ("producer", "Producteur", "Crédits", "both"),
    // Classification
    ("bpm", "BPM", "Classification", "track"),
    ("mood", "Ambiance", "Classification", "both"),
    ("grouping", "Regroupement", "Classification", "both"),
    ("compilation", "Compilation", "Classification", "album"),
    // Texte
    ("comment", "Commentaire", "Texte", "both"),
    ("lyrics", "Paroles", "Texte", "track"),
    // Identifiants
    ("isrc", "ISRC", "Identifiants", "track"),
    ("barcode", "Code-barres", "Identifiants", "album"),
    ("catalog_number", "Réf. catalogue", "Identifiants", "album"),
    ("media_type", "Support", "Identifiants", "album"),
    (
        "musicbrainz_recording_id",
        "MusicBrainz Recording ID",
        "Identifiants",
        "track",
    ),
    (
        "musicbrainz_release_id",
        "MusicBrainz Release ID",
        "Identifiants",
        "album",
    ),
    (
        "musicbrainz_release_group_id",
        "MusicBrainz Release Group ID",
        "Identifiants",
        "album",
    ),
    (
        "mb_release_track_id",
        "MusicBrainz Release Track ID",
        "Identifiants",
        "track",
    ),
    ("release_country", "Pays de sortie", "Identifiants", "album"),
    // Dates
    ("release_date", "Date de sortie", "Dates", "album"),
    ("original_date", "Date originale", "Dates", "album"),
    ("original_year", "Année originale", "Dates", "album"),
    // Technique
    ("format", "Format audio", "Technique", "track"),
    (
        "sample_rate",
        "Fréquence d'échantillonnage",
        "Technique",
        "track",
    ),
    ("bit_depth", "Profondeur de bits", "Technique", "track"),
    ("channels", "Canaux", "Technique", "track"),
    ("duration_ms", "Durée", "Technique", "track"),
    ("file_size", "Taille du fichier", "Technique", "track"),
    ("file_path", "Chemin du fichier", "Technique", "track"),
    ("encoder", "Encodeur", "Technique", "track"),
    (
        "encoder_software",
        "Logiciel d'encodage",
        "Technique",
        "track",
    ),
    ("source_media", "Support (MEDIA)", "Technique", "track"),
    ("copyright", "Copyright", "Technique", "both"),
    ("language", "Langue", "Technique", "both"),
    // ReplayGain
    ("rg_track_gain", "ReplayGain piste", "ReplayGain", "track"),
    ("rg_album_gain", "ReplayGain album", "ReplayGain", "album"),
];

const DEFAULT_VISIBLE_FIELDS: &[&str] = &[
    "composer",
    "conductor",
    "label",
    "genre",
    "year",
    "format",
    "sample_rate",
    "bit_depth",
    "release_country",
    "mb_release_track_id",
    "encoder_software",
    "source_media",
];

fn metadata_fields_key(pid: i64) -> String {
    format!("metadata_visible_fields:{pid}")
}

/// Read a per-profile preference stored under `key:{pid}`, falling back to the
/// legacy global `key` (installs from before per-profile prefs migrate
/// transparently on first read) then `None`.
fn read_profile_pref(settings: &SettingsRepo, pid: i64, key: &str) -> Option<String> {
    settings
        .get(&format!("{key}:{pid}"))
        .ok()
        .flatten()
        .or_else(|| settings.get(key).ok().flatten())
}

/// Persist a per-profile preference under `key:{pid}`.
fn write_profile_pref(settings: &SettingsRepo, pid: i64, key: &str, value: &str) {
    settings.set(&format!("{key}:{pid}"), value).ok();
}

/// Read the profile-scoped visible fields, falling back to the legacy global
/// key (pre-per-profile installs migrate transparently on first read) then the
/// built-in defaults.
fn read_visible_fields(settings: &SettingsRepo, pid: i64) -> Vec<String> {
    settings
        .get(&metadata_fields_key(pid))
        .ok()
        .flatten()
        .or_else(|| settings.get("metadata_visible_fields").ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            DEFAULT_VISIBLE_FIELDS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

pub(super) async fn get_metadata_fields(
    headers: axum::http::HeaderMap,
    profile: ActiveProfile,
    State(state): State<AppState>,
) -> Json<Value> {
    // Localize the field labels + category names to the client's selected UI
    // language (sent in Accept-Language), falling back to French.
    let lang = crate::i18n::lang_from_header(&headers);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled_keys: Vec<String> = read_visible_fields(&settings, profile.id());

    // Group fields by category (stable French key), preserving catalog order.
    let mut categories: Vec<(&str, Vec<Value>)> = Vec::new();
    for &(key, _label, category, scope) in METADATA_FIELDS {
        let enabled = enabled_keys.iter().any(|k| k == key);
        let field = json!({
            "key": key,
            "label": crate::i18n::t(&lang, &format!("metafield.{key}")),
            "enabled": enabled,
            "scope": scope,
        });

        if let Some(cat) = categories.iter_mut().find(|(name, _)| *name == category) {
            cat.1.push(field);
        } else {
            categories.push((category, vec![field]));
        }
    }

    let result: Vec<Value> = categories
        .into_iter()
        .map(|(name, fields)| {
            json!({ "name": crate::i18n::t(&lang, &format!("metacat.{name}")), "fields": fields })
        })
        .collect();

    Json(json!({ "categories": result }))
}

#[derive(Deserialize)]
pub(super) struct MetadataFieldsBody {
    fields: Vec<String>,
}

pub(super) async fn set_metadata_fields(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<MetadataFieldsBody>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Only keep keys that exist in the catalog
    let valid_keys: Vec<&str> = body
        .fields
        .iter()
        .filter_map(|k| {
            METADATA_FIELDS
                .iter()
                .find(|(key, _, _, _)| *key == k.as_str())
                .map(|(key, _, _, _)| *key)
        })
        .collect();
    let json_val = serde_json::to_string(&valid_keys).unwrap_or_else(|_| "[]".into());
    // Persist under the profile-scoped key so different profiles keep separate
    // visible-field sets and an update never loses them.
    settings
        .set(&metadata_fields_key(profile.id()), &json_val)
        .ok();
    Json(json!({ "fields": valid_keys }))
}

// --- Prefetch settings ---

pub(super) async fn get_prefetch(State(state): State<AppState>) -> Json<Value> {
    let mode = tune_core::prefetch::PrefetchEngine::read_mode(&state.backend);
    let status = state.orchestrator.prefetch.status().await;
    Json(json!({
        "mode": mode.as_str(),
        "buffer": status,
    }))
}

#[derive(Deserialize)]
pub(super) struct PrefetchModeBody {
    mode: String,
}

pub(super) async fn set_prefetch(
    State(state): State<AppState>,
    Json(body): Json<PrefetchModeBody>,
) -> Json<Value> {
    let mode = tune_core::prefetch::PrefetchMode::from_str_setting(&body.mode);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("prefetch_mode", mode.as_str()).ok();

    // If switching to Off, clear any buffered data
    if mode == tune_core::prefetch::PrefetchMode::Off {
        state.orchestrator.prefetch.clear().await;
    }

    Json(json!({
        "mode": mode.as_str(),
        "ok": true,
    }))
}

// ---------------------------------------------------------------------------
// License endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct LicenseBody {
    key: String,
}

pub(super) async fn get_license(State(state): State<AppState>) -> Json<Value> {
    let ls = state.license.license_state().await;
    Json(json!({
        "tier": ls.tier,
        "license_key_masked": ls.license_key.as_deref().map(|k| {
            if k.len() <= 4 { "****".to_string() }
            else { format!("{}{}", "*".repeat(k.len() - 4), &k[k.len()-4..]) }
        }),
        "expires_at": ls.expires_at,
        "last_validated": ls.last_validated,
        "hardware_fingerprint": ls.hardware_fingerprint,
        // Grâce hors ligne (#1999) — même objet que /cloud/license/status.
        "offline_grace": tune_core::license::offline_grace(&ls),
    }))
}

pub(super) async fn set_license(
    State(state): State<AppState>,
    Json(body): Json<LicenseBody>,
) -> impl IntoResponse {
    // Store the key as "pending" (no Premium granted yet), then confirm it with
    // the licensing server before unlocking anything. A fake key therefore never
    // unlocks Premium, while a genuine key is activated in this same round-trip.
    if let Err(e) = state.license.set_license_key(&body.key).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": e})),
        )
            .into_response();
    }

    let tier = crate::routes::cloud::validate_stored_license(&state).await;
    let premium = tier == tune_core::license::Tier::Premium;
    let ls = state.license.license_state().await;
    Json(json!({
        "status": if premium { "ok" } else { "pending" },
        "tier": ls.tier,
        "message": if premium {
            "Licence validée : Premium activé."
        } else {
            "Clé enregistrée. Premium s'activera dès qu'elle sera validée en ligne (vérifiez votre connexion et la clé)."
        },
    }))
    .into_response()
}

pub(super) async fn delete_license(State(state): State<AppState>) -> Json<Value> {
    state.license.clear_license().await;
    Json(json!({ "status": "ok", "tier": "free" }))
}

/// Nom convivial de CETTE machine — la réponse à « à quel serveur je parle ? ».
///
/// Le réglage `server_name` prime ; à défaut, le nom d'hôte réel du système.
/// On passe par `tune_core::discovery::system_hostname()`, et non par le
/// `hostname` du sous-processus qu'utilise `server_urls` ci-dessous : ce
/// dernier rend une chaîne vide quand le binaire manque (conteneurs minimaux),
/// alors que `system_hostname()` interroge `gethostname(2)` et ne rend jamais
/// vide. Le défaut doit exister partout, sinon l'étiquette disparaît là où
/// elle sert le plus. C'est aussi la dérivation qui a réparé #1127, où la
/// version « variables d'environnement seules » retombait sur `tune-server`
/// sous systemd et faisait porter le même nom à tous les serveurs du réseau.
///
/// Jamais l'`instance_id` : c'est un UUID de 36 caractères, à usage cloud, créé
/// dix secondes après le démarrage par la tâche de heartbeat — illisible, et
/// absent pendant les premières secondes de vie du serveur.
pub(crate) fn resolve_server_name(configured: Option<&str>) -> String {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => tune_core::discovery::system_hostname(),
    }
}

/// URLs d'accès au serveur depuis un autre appareil du réseau.
/// Priorité à TUNE_ADVERTISE_IP (VPN/NordVPN : l'IP détectée serait celle du
/// tunnel), sinon l'IP LAN détectée par la sonde UDP ; plus le nom mDNS
/// (inutile sur Android, mais pratique partout ailleurs). L'IP est recalculée
/// à chaque appel (elle change en cas de bascule filaire↔WiFi) ; le hostname
/// est mis en cache.
pub(crate) fn server_urls(port: u16) -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(ip) = std::env::var("TUNE_ADVERTISE_IP") {
        if !ip.is_empty() {
            urls.push(format!("http://{ip}:{port}"));
        }
    }
    if urls.is_empty() {
        if let Some(ip) = tune_core::discovery::ssdp::get_local_ip() {
            urls.push(format!("http://{ip}:{port}"));
        }
    }
    static HOSTNAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let host = HOSTNAME.get_or_init(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    });
    if !host.is_empty() && host != "localhost" && !host.contains('.') {
        urls.push(format!("http://{host}.local:{port}"));
    }
    urls
}

// ───────────────────────────────────────────────────────────────────────────
// #2149 — retirer un dossier des réglages laisse ses pistes en base
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod purge_hors_perimetre_tests {
    use super::{
        PurgeOrphans, RefusPurge, RemoveMusicDir, orphelines_parmi, refus_de_purge,
        regrouper_hors_perimetre,
    };
    use crate::auth::RequireAdmin;
    use crate::state::AppState;
    use axum::Json;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::db::models::Track;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::track_repo::TrackRepo;

    /// Les chemins de test sont écrits en `/` et passés par `normalize_path`,
    /// qui les retourne en antislashs sous Windows. Sans quoi ces tests
    /// seraient verts sur Mac et rouges chez Rhorn.
    fn n(p: &str) -> String {
        tune_core::scanner::walker::normalize_path(p)
    }

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn racines(state: &AppState, dirs: &[&str]) {
        let v: Vec<String> = dirs.iter().map(|d| n(d)).collect();
        SettingsRepo::with_backend(state.backend.clone())
            .set("music_dirs", &serde_json::to_string(&v).unwrap())
            .unwrap();
    }

    fn piste(state: &AppState, chemin: &str) -> i64 {
        let repo = TrackRepo::with_backend(state.backend.clone());
        let mut t = Track::new(format!("piste {chemin}"));
        t.file_path = Some(n(chemin));
        repo.create(&t).unwrap()
    }

    fn compte(state: &AppState) -> i64 {
        state
            .backend
            .query_one("SELECT COUNT(*) FROM tracks", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    }

    fn existe(state: &AppState, id: i64) -> bool {
        state
            .backend
            .query_one(
                "SELECT COUNT(*) FROM tracks WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            > 0
    }

    async fn retirer(state: &AppState, chemin: &str) -> serde_json::Value {
        retirer_avec(state, chemin, None).await
    }

    async fn retirer_avec(
        state: &AppState,
        chemin: &str,
        confirm: Option<u64>,
    ) -> serde_json::Value {
        super::remove_music_dir(
            RequireAdmin,
            State(state.clone()),
            Json(RemoveMusicDir {
                path: chemin.to_string(),
                confirm_purge: confirm,
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("remove_music_dir a échoué"))
        .0
    }

    fn albums(state: &AppState) -> i64 {
        state
            .backend
            .query_one("SELECT COUNT(*) FROM albums", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    }

    async fn purger(
        state: &AppState,
        chemin: &str,
        confirm: Option<u64>,
    ) -> (u16, serde_json::Value) {
        let r = super::purge_orphan_tracks(
            RequireAdmin,
            State(state.clone()),
            Json(PurgeOrphans {
                path: chemin.to_string(),
                confirm_purge: confirm,
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("purge_orphan_tracks a échoué"))
        .into_response();
        let code = r.status().as_u16();
        let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&corps).unwrap())
    }

    // ── Le défaut de Rhorn ──────────────────────────────────────────────

    /// Le cœur de #2149 : un dossier retiré, ses pistes s'en vont — mais
    /// seulement sur demande explicite et chiffrée.
    #[tokio::test]
    async fn un_dossier_retire_puis_purge_perd_ses_pistes() {
        let state = etat();
        racines(&state, &["/nas1/Musique", "/nas2/Musique"]);
        let vieille = piste(&state, "/nas1/Musique/Bach/01.flac");
        let neuve = piste(&state, "/nas2/Musique/Bach/01.flac");

        let rep = retirer(&state, "/nas1/Musique").await;
        assert_eq!(
            rep["orphan_tracks"].as_i64(),
            Some(1),
            "le retrait doit DIRE ce qu'il laisse derrière lui : {rep}"
        );
        assert!(existe(&state, vieille), "le retrait seul ne supprime rien");

        let (code, rep) = purger(&state, "/nas1/Musique", Some(1)).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(1), "{rep}");
        assert!(!existe(&state, vieille), "la piste du dossier retiré reste");
        assert!(
            existe(&state, neuve),
            "la piste du dossier VIVANT est partie"
        );
    }

    /// Sans confirmation, c'est un essai à blanc : les chiffres, rien d'autre.
    /// C'est ce qui permet à l'écran de « proposer de retirer aussi ce qu'il
    /// contenait » avant d'agir.
    #[tokio::test]
    async fn sans_confirmation_rien_ne_part() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        for i in 0..3 {
            piste(&state, &format!("/vieux_nas/Musique/{i}.flac"));
        }
        piste(&state, "/nas1/Musique/vivante.flac");

        let (code, rep) = purger(&state, "/vieux_nas/Musique", None).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["dry_run"].as_bool(), Some(true), "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(0), "{rep}");
        assert_eq!(rep["confirm_purge_required"].as_i64(), Some(3), "{rep}");
        assert_eq!(compte(&state), 4, "un essai à blanc a supprimé des pistes");
    }

    // ── Le danger : ne pas transformer un oubli en perte de données ─────

    /// **La preuve du garde-fou.** Un dossier momentanément illisible — NAS
    /// décroché, disque débranché — reste dans `music_dirs` : personne ne l'a
    /// retiré. Il est donc encore dans le périmètre, et AUCUNE requête, quel
    /// que soit son contenu, ne peut lui prendre une piste.
    ///
    /// Le disque n'est jamais consulté : le chemin de test n'existe même pas
    /// sur la machine qui exécute ce test, et c'est le point — ni l'état ni la
    /// lisibilité du support n'entrent dans la décision.
    #[tokio::test]
    async fn un_dossier_momentanement_illisible_ne_perd_aucune_piste() {
        let state = etat();
        // Le montage est tombé, mais la racine est TOUJOURS configurée.
        racines(&state, &["/mnt/nas_decroche/Musique"]);
        let ids: Vec<i64> = (0..5)
            .map(|i| piste(&state, &format!("/mnt/nas_decroche/Musique/{i}.flac")))
            .collect();
        assert!(
            !std::path::Path::new(&n("/mnt/nas_decroche/Musique")).exists(),
            "le dossier de test doit être absent du disque, c'est tout l'objet"
        );

        // Même en confirmant le nombre exact, et même en visant un parent.
        for (cible, confirm) in [
            ("/mnt/nas_decroche/Musique", Some(5)),
            ("/mnt/nas_decroche/Musique", Some(9999)),
            ("/mnt/nas_decroche/Musique/Bach", Some(5)),
            ("/mnt/nas_decroche", Some(5)),
            ("/mnt", Some(5)),
        ] {
            let (code, rep) = purger(&state, cible, confirm).await;
            assert_eq!(code, 409, "cible {cible} n'a pas été refusée : {rep}");
            assert_eq!(rep["purged"].as_i64(), Some(0), "{rep}");
            assert_eq!(rep["refused"].as_bool(), Some(true), "{rep}");
        }
        assert_eq!(compte(&state), 5, "une piste a été perdue");
        for id in ids {
            assert!(existe(&state, id), "la piste {id} a disparu");
        }
    }

    /// Le refus est NOMMÉ, et la phrase dit ce qui s'est passé — un montage
    /// tombé ne doit pas laisser l'utilisateur devant un échec muet.
    #[test]
    fn le_refus_est_nomme_et_dit_pourquoi() {
        let r = refus_de_purge(&n("/mnt/nas/Musique"), &[n("/mnt/nas/Musique")]).unwrap();
        assert_eq!(r, RefusPurge::DansLePerimetre);
        let m = r.message(&n("/mnt/nas/Musique")).to_lowercase();
        assert!(
            m.contains("réglages") || m.contains("dossiers de musique"),
            "{m}"
        );
        assert!(m.contains("indisponible") || m.contains("décroché"), "{m}");

        assert_eq!(
            refus_de_purge(&n("/mnt"), &[n("/mnt/nas/Musique")]),
            Some(RefusPurge::ContientUneRacine),
            "purger un parent d'une racine vivante doit être refusé"
        );
        assert_eq!(
            refus_de_purge("", &[n("/mnt/nas")]),
            Some(RefusPurge::CibleVide)
        );
        assert_eq!(
            refus_de_purge(&n("/vieux_nas"), &[n("/mnt/nas/Musique")]),
            None,
            "un dossier hors périmètre doit être purgeable"
        );
    }

    /// Un préfixe de chaîne n'est pas un dossier : `/nas/Musique2` n'est pas
    /// sous `/nas/Musique`. Le garde-fou passerait à côté sinon.
    #[test]
    fn un_prefixe_de_nom_n_est_pas_un_sous_dossier() {
        assert_eq!(
            refus_de_purge(&n("/nas/Musique2"), &[n("/nas/Musique")]),
            None
        );
        assert_eq!(
            refus_de_purge(&n("/nas/Musique/Jazz"), &[n("/nas/Musique")]),
            Some(RefusPurge::DansLePerimetre)
        );
    }

    /// Le plafond de #1943 s'applique aussi à une suppression explicite : une
    /// confirmation qui ne couvre pas l'ampleur constatée ne suffit pas.
    #[tokio::test]
    async fn le_plafond_1943_s_applique_a_la_suppression_explicite() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        for i in 0..60 {
            piste(&state, &format!("/nas1/Musique/{i}.flac"));
        }
        for i in 0..40 {
            piste(&state, &format!("/vieux_nas/{i}.flac"));
        }

        // 40 sur 100 = 40 % > 20 % : confirmation trop courte ⇒ refus.
        let (code, rep) = purger(&state, "/vieux_nas", Some(10)).await;
        assert_eq!(code, 409, "{rep}");
        assert_eq!(rep["reason"].as_str(), Some("confirmation_insuffisante"));
        assert_eq!(rep["confirm_purge_required"].as_i64(), Some(40), "{rep}");
        assert_eq!(compte(&state), 100, "des pistes sont parties sur un refus");

        // Le nombre exact lève le plafond.
        let (code, rep) = purger(&state, "/vieux_nas", Some(40)).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(40), "{rep}");
        assert_eq!(compte(&state), 60);
    }

    /// Aucune racine configurée = on ne sait rien, pas « tout est orphelin ».
    /// Même prudence que `verdict_purge`.
    #[tokio::test]
    async fn sans_aucune_racine_configuree_rien_n_est_declare_orphelin() {
        let state = etat();
        racines(&state, &[]);
        piste(&state, "/nas1/Musique/a.flac");

        let r = super::orphan_tracks(RequireAdmin, State(state.clone()))
            .await
            .0;
        assert_eq!(r["total"].as_i64(), Some(0), "{r}");
        assert_eq!(r["groups"].as_array().map(Vec::len), Some(0), "{r}");
    }

    // ── Les objets liés ─────────────────────────────────────────────────

    /// Une piste dans une liste de lecture : comportement EXPLICITE. L'entrée
    /// quitte la liste (cascade), la liste survit, les autres entrées restent,
    /// et l'impact est annoncé AVANT la suppression.
    #[tokio::test]
    async fn une_piste_en_liste_de_lecture_quitte_la_liste_sans_la_detruire() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        let vieille = piste(&state, "/vieux_nas/Bach/01.flac");
        let gardee = piste(&state, "/nas1/Musique/Bach/02.flac");
        state
            .backend
            .execute(
                "INSERT INTO playlists (id, name) VALUES (1, 'Ma liste')",
                &[],
            )
            .unwrap();
        for (pos, t) in [(0i64, vieille), (1, gardee)] {
            state
                .backend
                .execute(
                    "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, ?, ?)",
                    &[&t as &dyn ToSqlValue, &pos as &dyn ToSqlValue],
                )
                .unwrap();
        }

        // L'essai à blanc annonce l'impact avant d'agir.
        let (_, plan) = purger(&state, "/vieux_nas", None).await;
        assert_eq!(plan["impact"]["playlists"].as_i64(), Some(1), "{plan}");
        assert_eq!(
            plan["impact"]["playlist_entries"].as_i64(),
            Some(1),
            "{plan}"
        );

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let restant: Vec<i64> = state
            .backend
            .query_many(
                "SELECT track_id FROM playlist_tracks WHERE playlist_id = 1",
                &[],
            )
            .unwrap()
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        assert_eq!(
            restant,
            vec![gardee],
            "l'entrée de la piste retirée doit partir, l'autre rester — aucune \
             référence pendante"
        );
        let listes: i64 = state
            .backend
            .query_one("SELECT COUNT(*) FROM playlists", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap();
        assert_eq!(listes, 1, "la liste de lecture elle-même a été détruite");
    }

    /// L'historique d'écoute SURVIT : `ON DELETE SET NULL`. Une piste retirée
    /// ne réécrit pas le passé de l'utilisateur.
    #[tokio::test]
    async fn l_historique_d_ecoute_survit_a_la_purge() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        let vieille = piste(&state, "/vieux_nas/Bach/01.flac");
        piste(&state, "/nas1/Musique/vivante.flac");
        state
            .backend
            .execute(
                "INSERT INTO listen_history (track_id, title, artist_name, listened_at) \
                 VALUES (?, 'Toccata', 'Bach', '2026-08-01T10:00:00Z')",
                &[&vieille as &dyn ToSqlValue],
            )
            .unwrap();

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let rows = state
            .backend
            .query_many("SELECT track_id, title FROM listen_history", &[])
            .unwrap();
        assert_eq!(rows.len(), 1, "la ligne d'historique a été effacée");
        assert!(
            rows[0].first().and_then(|v| v.as_i64()).is_none(),
            "track_id devait passer à NULL"
        );
        assert_eq!(
            rows[0].get(1).and_then(|v| v.as_string()).as_deref(),
            Some("Toccata"),
            "le titre écouté doit rester lisible"
        );
    }

    /// Un favori de piste n'est JAMAIS supprimé par cette route. Chez Rhorn,
    /// la même musique existe sous le nouveau NAS : le favori s'y re-rattache.
    #[tokio::test]
    async fn un_favori_est_rerattache_jamais_supprime() {
        let state = etat();
        racines(&state, &["/nas2/Musique"]);
        state
            .backend
            .execute("INSERT INTO artists (name) VALUES ('Bach')", &[])
            .unwrap();
        let bach: i64 = state
            .backend
            .query_one("SELECT id FROM artists WHERE name = 'Bach'", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap();
        let repo = TrackRepo::with_backend(state.backend.clone());
        let toccata = |chemin: &str| {
            let mut t = Track::new("Toccata".into());
            t.file_path = Some(n(chemin));
            t.artist_id = Some(bach);
            t.artist_name = Some("Bach".into());
            repo.create(&t).unwrap()
        };
        // La même musique, sous l'ancien NAS et sous le nouveau — le cas exact
        // de Rhorn, qui a migré sa bibliothèque d'un support à l'autre.
        let ancienne = toccata("/vieux_nas/Bach/Toccata.flac");
        let nouvelle = toccata("/nas2/Musique/Bach/Toccata.flac");

        // Le favori pointe l'ANCIENNE piste, avec son instantané d'identité.
        state
            .backend
            .execute(
                "INSERT INTO favorites (profile_id, item_type, item_id, item_name, item_artist, \
                 item_path) VALUES (1, 'track', ?, 'Toccata', 'Bach', ?)",
                &[
                    &ancienne as &dyn ToSqlValue,
                    &n("/vieux_nas/Bach/Toccata.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let favs = state
            .backend
            .query_many(
                "SELECT item_id FROM favorites WHERE item_type = 'track'",
                &[],
            )
            .unwrap();
        assert_eq!(favs.len(), 1, "le favori a été SUPPRIMÉ : {rep}");
        assert_eq!(
            favs[0].first().and_then(|v| v.as_i64()),
            Some(nouvelle),
            "le favori devait être re-rattaché à la piste vivante"
        );
    }

    // ── Le regroupement montré à l'écran ────────────────────────────────

    /// Rhorn doit lire « /vieux_nas — 3 pistes », pas trois lignes d'albums.
    /// Et le repli s'arrête sous un dossier qui porte encore du vivant.
    #[test]
    fn les_orphelines_sont_regroupees_sous_le_plus_haut_dossier_mort() {
        let hors = [
            n("/vieux_nas/Bach/01.flac"),
            n("/vieux_nas/Bach/02.flac"),
            n("/vieux_nas/Mozart/01.flac"),
        ];
        let hors_refs: Vec<&str> = hors.iter().map(|s| s.as_str()).collect();
        let g = regrouper_hors_perimetre(&hors_refs, &[]);
        assert_eq!(g, vec![(n("/vieux_nas"), 3)], "{g:?}");

        // Une piste vivante voisine empêche de remonter jusqu'à `/data`.
        let vivante = n("/data/actuel/a.flac");
        let mortes = [n("/data/ancien/01.flac"), n("/data/ancien/02.flac")];
        let mortes_refs: Vec<&str> = mortes.iter().map(|s| s.as_str()).collect();
        let g = regrouper_hors_perimetre(&mortes_refs, &[vivante.as_str()]);
        assert_eq!(g, vec![(n("/data/ancien"), 2)], "{g:?}");
    }

    /// La route de listage rend les groupes et l'impact — c'est ce qui permet
    /// de rattraper un dossier retiré il y a trois versions.
    #[tokio::test]
    async fn la_route_de_listage_montre_les_dossiers_deja_retires() {
        let state = etat();
        racines(&state, &["/nas2/Musique"]);
        piste(&state, "/nas2/Musique/vivante.flac");
        for i in 0..4 {
            piste(&state, &format!("/vieux_nas/Bach/{i}.flac"));
        }

        let r = super::orphan_tracks(RequireAdmin, State(state.clone()))
            .await
            .0;
        assert_eq!(r["total"].as_i64(), Some(4), "{r}");
        let g = r["groups"].as_array().unwrap();
        assert_eq!(g.len(), 1, "{r}");
        assert_eq!(g[0]["path"].as_str(), Some(n("/vieux_nas").as_str()), "{r}");
        assert_eq!(g[0]["tracks"].as_i64(), Some(4), "{r}");
    }

    // ── Racines imbriquées : l'angle mort restant de #2149 ───────────────

    /// Retirer une racine PARENTE ne rend pas orpheline la piste d'une racine
    /// imbriquée encore configurée.
    ///
    /// `music_dirs = ["/media/disque", "/media/disque/Classique"]` est un
    /// réglage courant : on indexe un disque entier, puis on ajoute un
    /// sous-dossier pour le traiter à part. Le jour où l'on retire le disque
    /// des réglages, `Classique` reste configuré — ses pistes sont vivantes.
    ///
    /// Le compte annoncé se mesurait sur le seul dossier retiré, sans
    /// intersecter les racines RESTANTES : il comptait les pistes de
    /// `Classique` comme orphelines, puis `/purge-orphans` refusait en bloc
    /// (`ContientUneRacine`). L'utilisateur restait devant un nombre qu'aucun
    /// geste ne pouvait honorer.
    #[tokio::test]
    async fn retirer_une_racine_parente_ne_compte_pas_la_racine_imbriquee() {
        let state = etat();
        racines(&state, &["/media/disque", "/media/disque/Classique"]);
        piste(&state, "/media/disque/Pop/01.flac");
        piste(&state, "/media/disque/Classique/Bach/01.flac");

        let rep = retirer(&state, "/media/disque").await;
        assert_eq!(
            rep["orphan_tracks"].as_i64(),
            Some(1),
            "seule la piste hors de la racine restante est orpheline : {rep}"
        );
        assert_eq!(compte(&state), 2, "le retrait seul ne supprime rien");
    }

    /// **Le cœur du danger.** La purge en un geste ne peut pas prendre une
    /// piste qui appartient à une AUTRE racine — imbriquée, chevauchante ou
    /// simplement voisine. Ce n'est pas un refus en amont : l'ensemble
    /// supprimé est calculé contre les racines restantes, donc la piste
    /// vivante n'est jamais candidate.
    #[tokio::test]
    async fn la_purge_du_parent_epargne_la_racine_imbriquee() {
        let state = etat();
        racines(
            &state,
            &["/media/disque", "/media/disque/Classique", "/nas2/Musique"],
        );
        let morte = piste(&state, "/media/disque/Pop/01.flac");
        let imbriquee = piste(&state, "/media/disque/Classique/Bach/01.flac");
        let voisine = piste(&state, "/nas2/Musique/Jazz/01.flac");

        let rep = retirer_avec(&state, "/media/disque", Some(1)).await;
        assert_eq!(rep["purged"].as_i64(), Some(1), "{rep}");
        assert_eq!(rep["purge_refused"].as_bool(), Some(false), "{rep}");
        assert!(!existe(&state, morte), "la piste devenue orpheline reste");
        assert!(
            existe(&state, imbriquee),
            "la piste de la racine IMBRIQUÉE encore configurée a été détruite : {rep}"
        );
        assert!(
            existe(&state, voisine),
            "la piste d'une autre racine a été détruite"
        );

        // Et la racine imbriquée reste configurée : on n'a retiré qu'elle.
        let dirs: Vec<String> = rep["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(dirs, vec![n("/media/disque/Classique"), n("/nas2/Musique")]);
    }

    /// Le chevauchement dans l'autre sens : retirer la racine ENFANT alors que
    /// la parente reste configurée ne rend rien orphelin — les pistes sont
    /// toujours dans le périmètre, elles y restent.
    #[tokio::test]
    async fn retirer_une_racine_couverte_par_sa_parente_ne_rend_rien_orphelin() {
        let state = etat();
        racines(&state, &["/media/disque", "/media/disque/Classique"]);
        let sous_les_deux = piste(&state, "/media/disque/Classique/Bach/01.flac");

        let rep = retirer_avec(&state, "/media/disque/Classique", Some(1)).await;
        assert_eq!(rep["orphan_tracks"].as_i64(), Some(0), "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(0), "{rep}");
        assert!(existe(&state, sous_les_deux), "{rep}");
    }

    /// La fonction qui décide, prise seule : pas de base, pas de disque.
    /// Les deux séparateurs, et un préfixe de nom qui n'est pas un dossier.
    #[test]
    fn orphelines_parmi_epargne_les_racines_restantes() {
        let pistes: Vec<(i64, String)> = [
            (1, n("/media/disque/Pop/01.flac")),
            (2, n("/media/disque/Classique/Bach/01.flac")),
            (3, n("/media/disque2/Autre/01.flac")),
            (4, n("/ailleurs/01.flac")),
        ]
        .into_iter()
        .collect();

        // `/media/disque2` n'est PAS sous `/media/disque` : préfixe de nom.
        let ids = orphelines_parmi(
            &pistes,
            &n("/media/disque"),
            &[n("/media/disque/Classique")],
        );
        assert_eq!(ids, vec![1], "attendu la seule piste devenue orpheline");

        // Sans racine restante, tout ce qui est sous la cible est orphelin.
        let ids = orphelines_parmi(&pistes, &n("/media/disque"), &[]);
        assert_eq!(ids, vec![1, 2]);

        // Une cible vide ne purge JAMAIS « tout ».
        assert!(orphelines_parmi(&pistes, "", &[]).is_empty());
        assert!(orphelines_parmi(&pistes, "/", &[]).is_empty());

        // Une racine restante vide ne doit pas neutraliser le filtre.
        let ids = orphelines_parmi(&pistes, &n("/ailleurs"), &["".into(), "/".into()]);
        assert_eq!(ids, vec![4]);
    }

    // ── Le geste en une fois ────────────────────────────────────────────

    /// La promesse du fil : « retirer un dossier des réglages devrait proposer
    /// de retirer aussi ce qu'il contenait ». Un aller-retour pour le plan,
    /// un second pour l'exécution — et les albums et artistes vidés partent
    /// avec les pistes, par LE chemin de purge, pas par un second.
    #[tokio::test]
    async fn le_retrait_confirme_emporte_pistes_albums_et_artistes() {
        let state = etat();
        racines(&state, &["/vieux_nas/Musique", "/nas2/Musique"]);
        let bd = &state.backend;
        bd.execute("INSERT INTO artists (name) VALUES ('Bach')", &[])
            .unwrap();
        let bach = bd.last_insert_rowid();
        bd.execute(
            "INSERT INTO albums (title, artist_id, track_count) VALUES ('Toccata', ?, 1)",
            &[&bach as &dyn ToSqlValue],
        )
        .unwrap();
        let album = bd.last_insert_rowid();
        let repo = TrackRepo::with_backend(bd.clone());
        let mut t = Track::new("01".into());
        t.file_path = Some(n("/vieux_nas/Musique/Bach/01.flac"));
        t.album_id = Some(album);
        t.artist_id = Some(bach);
        let morte = repo.create(&t).unwrap();
        let gardee = piste(&state, "/nas2/Musique/vivante.flac");

        // 1er appel : le plan, rien n'est touché.
        let plan = retirer(&state, "/vieux_nas/Musique").await;
        assert_eq!(plan["orphan_tracks"].as_i64(), Some(1), "{plan}");
        assert_eq!(plan["confirm_purge_required"].as_i64(), Some(1), "{plan}");
        assert_eq!(plan["impact"]["tracks"].as_i64(), Some(1), "{plan}");
        assert!(existe(&state, morte), "un essai à blanc a supprimé");
        assert_eq!(albums(&state), 1);

        // 2e appel : confirmé. (Le dossier est déjà hors des réglages.)
        let rep = retirer_avec(&state, "/vieux_nas/Musique", Some(1)).await;
        assert_eq!(rep["purged"].as_i64(), Some(1), "{rep}");
        assert_eq!(rep["orphan_albums_removed"].as_i64(), Some(1), "{rep}");
        assert_eq!(rep["orphan_artists_removed"].as_i64(), Some(1), "{rep}");
        assert!(!existe(&state, morte), "{rep}");
        assert!(existe(&state, gardee), "{rep}");
        assert_eq!(albums(&state), 0, "l'album vidé devait partir : {rep}");
    }

    /// Un client qui n'envoie que `path` — c'est-à-dire TOUS ceux qui
    /// existent aujourd'hui — ne perd jamais rien. La purge est un opt-in.
    #[tokio::test]
    async fn un_client_qui_n_envoie_que_le_chemin_ne_perd_rien() {
        let state = etat();
        racines(&state, &["/vieux_nas", "/nas2"]);
        for i in 0..5 {
            piste(&state, &format!("/vieux_nas/{i}.flac"));
        }
        let corps: RemoveMusicDir = serde_json::from_str(r#"{"path":"/vieux_nas"}"#).unwrap();
        let rep = super::remove_music_dir(RequireAdmin, State(state.clone()), Json(corps))
            .await
            .unwrap_or_else(|_| panic!("remove_music_dir a échoué"))
            .0;
        assert_eq!(rep["orphan_tracks"].as_i64(), Some(5), "{rep}");
        assert!(
            rep["purged"].is_null(),
            "aucune purge ne doit avoir eu lieu : {rep}"
        );
        assert_eq!(compte(&state), 5);
    }

    /// Le plafond de #1943 vaut aussi pour ce geste-ci. Et le RETRAIT, lui,
    /// réussit quand même : le refus ne porte que sur la suppression, sans
    /// quoi un client conclurait que le dossier est encore configuré.
    #[tokio::test]
    async fn le_plafond_1943_s_applique_au_retrait_confirme() {
        let state = etat();
        racines(&state, &["/nas1", "/vieux_nas"]);
        for i in 0..60 {
            piste(&state, &format!("/nas1/{i}.flac"));
        }
        for i in 0..40 {
            piste(&state, &format!("/vieux_nas/{i}.flac"));
        }

        let rep = retirer_avec(&state, "/vieux_nas", Some(10)).await;
        assert_eq!(rep["purge_refused"].as_bool(), Some(true), "{rep}");
        assert_eq!(
            rep["purge_refused_reason"].as_str(),
            Some("confirmation_insuffisante"),
            "{rep}"
        );
        assert_eq!(rep["confirm_purge_required"].as_i64(), Some(40), "{rep}");
        assert_eq!(compte(&state), 100, "des pistes sont parties sur un refus");
        assert_eq!(
            rep["dirs"].as_array().map(Vec::len),
            Some(1),
            "le retrait du dossier, lui, doit avoir réussi : {rep}"
        );
    }

    // ── Les marqueurs sans clé étrangère ────────────────────────────────

    /// Un album MASQUÉ (#1391) dont les pistes sont purgées ne laisse pas un
    /// marqueur pendant : la purge est le cinquième ancrage de la
    /// réconciliation par identité. Chez Rhorn, l'album existe toujours sous
    /// le nouveau NAS — le masquage doit le suivre, pas mourir avec l'ancien.
    #[tokio::test]
    async fn un_album_masque_suit_la_purge_de_ses_pistes() {
        let state = etat();
        racines(&state, &["/vieux_nas", "/nas2"]);
        let bd = &state.backend;
        bd.execute("INSERT INTO artists (name) VALUES ('Talvin Singh')", &[])
            .unwrap();
        let ar = bd.last_insert_rowid();
        let album = |titre: &str| {
            bd.execute(
                "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, 1)",
                &[&titre as &dyn ToSqlValue, &ar as &dyn ToSqlValue],
            )
            .unwrap();
            bd.last_insert_rowid()
        };
        let repo = TrackRepo::with_backend(bd.clone());
        let pose = |chemin: &str, al: i64| {
            let mut t = Track::new("OK 01".into());
            t.file_path = Some(n(chemin));
            t.album_id = Some(al);
            t.artist_id = Some(ar);
            repo.create(&t).unwrap()
        };
        // Le MÊME album, des deux côtés de la migration.
        let ancien = album("OK");
        pose("/vieux_nas/Talvin Singh/OK/01.flac", ancien);
        let nouveau = album("OK");
        pose("/nas2/Talvin Singh/OK/01.flac", nouveau);

        let masques = tune_core::db::hidden_repo::HiddenRepo::with_backend(bd.clone());
        assert!(masques.hide_album(ancien).unwrap());

        let rep = retirer_avec(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(rep["purged"].as_i64(), Some(1), "{rep}");
        assert_eq!(rep["hidden_relinked"].as_i64(), Some(1), "{rep}");
        assert!(
            masques.is_album_hidden(nouveau).unwrap(),
            "le masquage devait suivre l'album vivant : {rep}"
        );
        assert!(!masques.is_album_hidden(ancien).unwrap(), "{rep}");
        // Aucun marqueur pendant : la table ne référence que du vivant.
        let restants = bd
            .query_many(
                "SELECT item_id FROM hidden_items WHERE item_id NOT IN (SELECT id FROM albums)",
                &[],
            )
            .unwrap();
        assert!(restants.is_empty(), "marqueur hidden_items orphelin laissé");
    }
}

/// #1627 — les trois modes ReplayGain publiés comme UN fait, en lecture.
///
/// La demande d'origine (« 1- néant, 2- fichier, 3- calcul ») n'a jamais eu de
/// réglage unique côté serveur, et n'en gagne pas ici : ce bloc DÉRIVE le mode
/// des deux axes existants. Ce qu'il apporte, et que le client ne pouvait pas
/// obtenir seul : le défaut de `replaygain_analysis_enabled` n'était publié
/// nulle part, et la règle a changé (#2496 — « Désactivé » arrête aussi le
/// balayage). Un client qui recomposait la règle de son côté présentait donc le
/// mauvais mode comme actif sur toute base fraîche.
#[cfg(test)]
mod replaygain_source_tests {
    use super::get_config;
    use crate::state::AppState;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use tune_core::db::settings_repo::SettingsRepo;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    async fn config_de(state: &AppState) -> serde_json::Value {
        get_config(HeaderMap::new(), State(state.clone())).await.0
    }

    #[tokio::test]
    async fn les_trois_modes_voyagent_dans_get_config() {
        let state = etat();
        let settings = SettingsRepo::with_backend(state.backend.clone());

        // Base fraîche : rien n'est écrit, et pourtant le mode est dit.
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "off");
        assert_eq!(c["replaygain_source"]["label"], "Désactivé");
        // Le défaut de la coche est publié — c'était le trou : absent de la
        // réponse, il obligeait le client à le deviner.
        assert_eq!(c["replaygain_analysis_enabled"], true);
        // ... mais rien ne tourne tant que le mode est « Désactivé » (#2496).
        assert_eq!(c["replaygain_source"]["analysis_effective"], false);
        assert_eq!(
            c["replaygain_source"]["setting_keys"],
            serde_json::json!(["replaygain_mode", "replaygain_analysis_enabled"]),
            "le client doit savoir QUOI écrire : ce bloc est en lecture seule"
        );

        // 3- calcul.
        settings.set("replaygain_mode", "track").unwrap();
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "tags_then_analysis");
        assert_eq!(c["replaygain_source"]["analysis_effective"], true);
        assert_eq!(
            c["replaygain_source"]["label"],
            "Tags des fichiers, puis analyse"
        );

        // 2- fichier.
        settings
            .set("replaygain_analysis_enabled", "false")
            .unwrap();
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "file_tags");
        assert_eq!(c["replaygain_analysis_enabled"], false);
        assert_eq!(c["replaygain_source"]["analysis_effective"], false);
        assert_eq!(c["replaygain_source"]["label"], "Tags des fichiers");

        // Les deux axes restent intacts et publiés tels quels : ce bloc
        // n'a rien remplacé.
        assert_eq!(c["replaygain_mode"], "track");
    }

    /// Le libellé suit la langue de l'app (`Accept-Language`), comme le reste
    /// des chaînes que l'API renvoie déjà.
    #[tokio::test]
    async fn le_libelle_du_mode_est_traduit() {
        let state = etat();
        SettingsRepo::with_backend(state.backend.clone())
            .set("replaygain_mode", "album")
            .unwrap();
        let mut h = HeaderMap::new();
        h.insert("accept-language", "en-GB,en;q=0.9".parse().unwrap());

        let c = get_config(h, State(state.clone())).await.0;
        assert_eq!(c["replaygain_source"]["mode"], "tags_then_analysis");
        assert_eq!(c["replaygain_source"]["label"], "File tags, then analysis");
    }

    // ---- l'autre moitié de #1627 : ÉCRIRE l'un des trois modes -------------

    /// Rejoue EXACTEMENT ce que fait `update_config` : la traduction du champ
    /// à trois valeurs, puis la boucle d'écriture générique, inchangée.
    fn patch(state: &AppState, corps: serde_json::Value) -> Result<Option<String>, String> {
        let mut values = corps.as_object().expect("objet JSON").clone();
        let granularite =
            tune_core::audio::replaygain::ReplayGainSettings::load(&state.backend).mode;
        let applique = super::expand_replaygain_source(&mut values, granularite)
            .map_err(|_| "bad_request".to_string())?;
        let settings = SettingsRepo::with_backend(state.backend.clone());
        for (cle, valeur) in values {
            let brut = match valeur.as_str() {
                Some(s) => s.to_string(),
                None => valeur.to_string(),
            };
            settings.set(&cle, &brut).unwrap();
        }
        Ok(applique.map(|m| m.as_str().to_string()))
    }

    /// AVANT : `{"replaygain_source": "..."}` tombait dans la boucle générique,
    /// créait une ligne morte `replaygain_source` dans `settings`, ne touchait
    /// NI `replaygain_mode` NI `replaygain_analysis_enabled` — et répondait
    /// `{"ok": true}`. Le client croyait avoir posé un mode.
    ///
    /// APRÈS : le champ écrit les deux axes existants, et rien d'autre.
    #[tokio::test]
    async fn les_trois_modes_s_ecrivent_par_un_seul_champ() {
        let state = etat();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        assert_eq!(config_de(&state).await["replaygain_source"]["mode"], "off");

        // 3- calcul.
        assert_eq!(
            patch(
                &state,
                serde_json::json!({"replaygain_source": "tags_then_analysis"})
            )
            .unwrap(),
            Some("tags_then_analysis".to_string()),
            "la réponse doit dire le mode posé"
        );
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "tags_then_analysis");
        assert_eq!(
            c["replaygain_mode"], "track",
            "depuis « néant », la granularité par défaut est la piste — \
             réécrire `off` ne changerait rien en répondant « ok »"
        );
        assert_eq!(c["replaygain_analysis_enabled"], true);
        assert_eq!(c["replaygain_source"]["analysis_effective"], true);

        // Aucune clé nouvelle en base : les deux axes restent la seule vérité.
        assert_eq!(
            settings.get("replaygain_source").unwrap(),
            None,
            "`replaygain_source` ne doit JAMAIS être persisté : c'est une vue"
        );

        // 2- fichier, en conservant la granularité que l'utilisateur avait.
        patch(&state, serde_json::json!({"replaygain_mode": "album"})).unwrap();
        patch(
            &state,
            serde_json::json!({"replaygain_source": "file_tags"}),
        )
        .unwrap();
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "file_tags");
        assert_eq!(
            c["replaygain_mode"], "album",
            "changer de source ne doit pas reculer l'album vers la piste"
        );
        assert_eq!(c["replaygain_analysis_enabled"], false);

        // 1- néant : le gain s'arrête, la coche d'analyse n'est pas écrasée.
        settings.set("replaygain_analysis_enabled", "true").unwrap();
        patch(&state, serde_json::json!({"replaygain_source": "off"})).unwrap();
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_source"]["mode"], "off");
        assert_eq!(c["replaygain_mode"], "off");
        assert_eq!(
            c["replaygain_analysis_enabled"], true,
            "« néant » ne touche qu'un seul des deux axes"
        );
        assert_eq!(c["replaygain_source"]["analysis_effective"], false);
    }

    /// Source ET granularité dans le même appel, et aller-retour du bloc que
    /// `GET /config` publie : un client qui relit puis renvoie la config
    /// entière ne doit pas se faire refuser.
    #[tokio::test]
    async fn la_granularite_du_meme_corps_prime_et_l_objet_relu_est_accepte() {
        let state = etat();
        patch(
            &state,
            serde_json::json!({"replaygain_source": "file_tags", "replaygain_mode": "album"}),
        )
        .unwrap();
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_mode"], "album");
        assert_eq!(c["replaygain_source"]["mode"], "file_tags");

        // Aller-retour : on renvoie tel quel le bloc publié.
        let bloc = c["replaygain_source"].clone();
        patch(&state, serde_json::json!({ "replaygain_source": bloc })).unwrap();
        let c = config_de(&state).await;
        assert_eq!(
            c["replaygain_source"]["mode"], "file_tags",
            "relire puis renvoyer la config ne doit rien changer"
        );
        assert_eq!(c["replaygain_mode"], "album");
    }

    /// Un mode inconnu est REFUSÉ, et rien n'est écrit. Le ReplayGain
    /// multiplie chaque échantillon : deviner y coûterait un niveau faux.
    #[tokio::test]
    async fn un_mode_inconnu_est_refuse_sans_rien_ecrire() {
        let state = etat();
        patch(
            &state,
            serde_json::json!({"replaygain_source": "tags_then_analysis"}),
        )
        .unwrap();
        let avant = config_de(&state).await;
        assert_eq!(avant["replaygain_source"]["mode"], "tags_then_analysis");
        assert_eq!(avant["replaygain_mode"], "track");

        for mauvais in [
            serde_json::json!("calcul"),
            serde_json::json!("neant"),
            serde_json::json!("track"),
            serde_json::json!(true),
            serde_json::json!(3),
            serde_json::json!({"granularity": "track"}),
        ] {
            let r = patch(&state, serde_json::json!({ "replaygain_source": mauvais }));
            assert!(
                r.is_err(),
                "cette valeur doit être refusée, pas interprétée"
            );
        }

        // Rien n'a bougé, et aucune ligne morte n'a été créée.
        let apres = config_de(&state).await;
        assert_eq!(apres["replaygain_source"]["mode"], "tags_then_analysis");
        assert_eq!(apres["replaygain_mode"], "track");
        assert_eq!(
            SettingsRepo::with_backend(state.backend.clone())
                .get("replaygain_source")
                .unwrap(),
            None
        );

        // Espaces et casse restent tolérés — c'est bien une valeur VALIDE.
        patch(&state, serde_json::json!({"replaygain_source": "  OFF "})).unwrap();
        assert_eq!(config_de(&state).await["replaygain_mode"], "off");
    }

    /// TÉMOIN ANTI-RÉGRESSION — vert avant comme après.
    ///
    /// Ceux qui écoutent aujourd'hui pilotent le ReplayGain par les deux
    /// réglages historiques. Un `PATCH` sans `replaygain_source` doit se
    /// comporter EXACTEMENT comme avant : chaque axe écrit seul, aucun autre
    /// touché, et le niveau appliqué inchangé.
    #[tokio::test]
    async fn temoin_un_patch_sans_le_nouveau_champ_ne_change_rien() {
        let state = etat();
        let settings = SettingsRepo::with_backend(state.backend.clone());

        patch(&state, serde_json::json!({"replaygain_mode": "album"})).unwrap();
        assert_eq!(
            settings.get("replaygain_analysis_enabled").unwrap(),
            None,
            "écrire la granularité seule ne doit pas poser la coche d'analyse"
        );
        let c = config_de(&state).await;
        assert_eq!(c["replaygain_mode"], "album");

        patch(
            &state,
            serde_json::json!({"replaygain_analysis_enabled": "false"}),
        )
        .unwrap();
        let c = config_de(&state).await;
        assert_eq!(
            c["replaygain_mode"], "album",
            "écrire la coche seule ne doit pas toucher la granularité"
        );
        assert_eq!(c["replaygain_analysis_enabled"], false);

        // Le niveau lui-même : préampli et anti-écrêtage voyagent intacts.
        patch(
            &state,
            serde_json::json!({"replaygain_preamp_db": "-3.5", "replaygain_prevent_clipping": "false"}),
        )
        .unwrap();
        let applique = tune_core::audio::replaygain::ReplayGainSettings::load(&state.backend);
        assert_eq!(
            applique.mode,
            tune_core::audio::replaygain::ReplayGainMode::Album
        );
        assert!((applique.preamp_db - (-3.5)).abs() < 1e-9);
        assert!(!applique.prevent_clipping);
    }
}
