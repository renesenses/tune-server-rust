use axum::extract::Path;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

const SUPPORTED_LOCALES: &[&str] = &["en", "fr", "de", "es", "it", "zh", "ko", "ja"];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/locales", get(list_locales))
        .route("/locales/{locale}", get(get_locale))
        .route("/locales/{locale}/{namespace}", get(get_namespace))
        .route("/locale/detect", get(detect_locale))
}

/// Return available locales.
async fn list_locales() -> Json<Value> {
    Json(json!(SUPPORTED_LOCALES))
}

/// Return all translations for a locale.
async fn get_locale(Path(locale): Path<String>) -> Json<Value> {
    let translations = get_translations(&locale);
    if translations.is_null() || translations.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        // Fallback to English
        Json(get_translations("en"))
    } else {
        Json(translations)
    }
}

/// Return just one namespace (e.g., /locales/fr/common).
async fn get_namespace(Path((locale, namespace)): Path<(String, String)>) -> Json<Value> {
    let translations = get_translations(&locale);
    let section = translations
        .get(&namespace)
        .cloned()
        .unwrap_or_else(|| {
            // Fallback to English namespace
            get_translations("en")
                .get(&namespace)
                .cloned()
                .unwrap_or(json!({}))
        });
    Json(section)
}

/// Read Accept-Language header and return best match.
async fn detect_locale(headers: HeaderMap) -> Json<Value> {
    let accept = headers
        .get("accept-language")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("en");

    let detected = parse_accept_language(accept);

    Json(json!({
        "detected": detected,
        "supported": SUPPORTED_LOCALES,
    }))
}

/// Parse Accept-Language header and find best match.
/// Example: "fr-FR,fr;q=0.9,en-US;q=0.8,en;q=0.7" -> "fr"
fn parse_accept_language(header: &str) -> String {
    let mut candidates: Vec<(f32, String)> = header
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            let (lang, quality) = if let Some(idx) = part.find(";q=") {
                let q: f32 = part[idx + 3..].trim().parse().unwrap_or(1.0);
                (part[..idx].trim(), q)
            } else {
                (part, 1.0)
            };
            // Normalize: "fr-FR" -> "fr"
            let normalized = lang.split('-').next().unwrap_or(lang).to_lowercase();
            if normalized.is_empty() {
                None
            } else {
                Some((quality, normalized))
            }
        })
        .collect();

    // Sort by quality descending
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Find first match
    for (_q, lang) in &candidates {
        if SUPPORTED_LOCALES.contains(&lang.as_str()) {
            return lang.clone();
        }
    }

    "en".to_string()
}

fn get_translations(locale: &str) -> Value {
    match locale {
        "fr" => json!({
            "common": {
                "play": "Lire",
                "pause": "Pause",
                "stop": "Arr\u{00ea}ter",
                "next": "Suivant",
                "previous": "Pr\u{00e9}c\u{00e9}dent",
                "search": "Rechercher",
                "settings": "Param\u{00e8}tres",
                "library": "Biblioth\u{00e8}que",
                "artists": "Artistes",
                "albums": "Albums",
                "tracks": "Pistes",
                "genres": "Genres",
                "playlists": "Playlists",
                "radios": "Radios",
                "queue": "File d'attente",
                "favorites": "Favoris",
                "history": "Historique",
                "volume": "Volume",
                "mute": "Muet",
                "unmute": "R\u{00e9}activer le son",
                "shuffle": "Al\u{00e9}atoire",
                "repeat": "R\u{00e9}p\u{00e9}ter",
                "repeatOne": "R\u{00e9}p\u{00e9}ter un titre",
                "nowPlaying": "En cours de lecture",
                "addToQueue": "Ajouter \u{00e0} la file",
                "addToPlaylist": "Ajouter \u{00e0} une playlist",
                "removeFromQueue": "Retirer de la file",
                "clearQueue": "Vider la file d'attente",
                "playAll": "Tout lire",
                "shuffleAll": "Lecture al\u{00e9}atoire",
                "sortBy": "Trier par",
                "filterBy": "Filtrer par",
                "noResults": "Aucun r\u{00e9}sultat",
                "loading": "Chargement...",
                "error": "Erreur",
                "retry": "R\u{00e9}essayer",
                "cancel": "Annuler",
                "save": "Enregistrer",
                "delete": "Supprimer",
                "edit": "Modifier",
                "close": "Fermer",
                "back": "Retour",
                "home": "Accueil",
                "more": "Plus",
                "less": "Moins",
                "all": "Tout",
                "none": "Aucun",
                "yes": "Oui",
                "no": "Non",
                "ok": "OK",
                "confirm": "Confirmer",
                "duration": "Dur\u{00e9}e",
                "year": "Ann\u{00e9}e",
                "format": "Format",
                "quality": "Qualit\u{00e9}",
                "bitrate": "D\u{00e9}bit",
                "sampleRate": "Fr\u{00e9}quence",
                "bitDepth": "R\u{00e9}solution",
            },
            "settings": {
                "title": "Param\u{00e8}tres",
                "musicDirs": "Emplacements de la biblioth\u{00e8}que",
                "musicDirsDescription": "Dossiers contenant vos fichiers audio",
                "streaming": "Services de streaming",
                "streamingDescription": "Connectez vos comptes Tidal, Qobuz, Spotify...",
                "zones": "Zones de lecture",
                "zonesDescription": "Configurez vos appareils audio",
                "system": "Syst\u{00e8}me",
                "systemDescription": "Informations et maintenance du serveur",
                "profiles": "Profils",
                "profilesDescription": "G\u{00e9}rez les profils utilisateurs",
                "appearance": "Apparence",
                "theme": "Th\u{00e8}me",
                "themeDark": "Sombre",
                "themeLight": "Clair",
                "themeSystem": "Syst\u{00e8}me",
                "language": "Langue",
                "scanLibrary": "Scanner la biblioth\u{00e8}que",
                "scanning": "Scan en cours...",
                "lastScan": "Dernier scan",
                "version": "Version",
                "restart": "Red\u{00e9}marrer",
                "shutdown": "Arr\u{00ea}ter",
                "logs": "Journaux",
                "diagnostics": "Diagnostics",
            },
            "onboarding": {
                "welcome": "Bienvenue dans Tune !",
                "welcomeSubtitle": "Configurons votre serveur musical en quelques \u{00e9}tapes.",
                "addMusic": "Ajoutez votre musique",
                "addMusicDescription": "Indiquez les dossiers contenant vos fichiers audio.",
                "connectStreaming": "Connectez vos services de streaming",
                "connectStreamingDescription": "Acc\u{00e9}dez \u{00e0} vos biblioth\u{00e8}ques Tidal, Qobuz, Spotify...",
                "setupZones": "Configurez vos zones de lecture",
                "setupZonesDescription": "D\u{00e9}tectez automatiquement vos appareils DLNA/AirPlay.",
                "createProfile": "Cr\u{00e9}ez votre profil",
                "createProfileDescription": "Personnalisez votre exp\u{00e9}rience musicale.",
                "complete": "Configuration termin\u{00e9}e !",
                "completeDescription": "Votre serveur Tune est pr\u{00ea}t. Bonne \u{00e9}coute !",
                "skip": "Passer",
                "next": "Suivant",
                "finish": "Terminer",
                "stepOf": "\u{00c9}tape {current} sur {total}",
            },
            "library": {
                "allArtists": "Tous les artistes",
                "allAlbums": "Tous les albums",
                "allTracks": "Toutes les pistes",
                "recentlyAdded": "Ajout\u{00e9}s r\u{00e9}cemment",
                "recentlyPlayed": "\u{00c9}cout\u{00e9}s r\u{00e9}cemment",
                "mostPlayed": "Les plus \u{00e9}cout\u{00e9}s",
                "noTracks": "Aucune piste dans la biblioth\u{00e8}que",
                "noAlbums": "Aucun album dans la biblioth\u{00e8}que",
                "noArtists": "Aucun artiste dans la biblioth\u{00e8}que",
                "tracksCount": "{count} piste(s)",
                "albumsCount": "{count} album(s)",
                "artistsCount": "{count} artiste(s)",
                "biography": "Biographie",
                "discography": "Discographie",
                "similarArtists": "Artistes similaires",
                "credits": "Cr\u{00e9}dits",
                "composer": "Compositeur",
                "label": "Label",
            },
            "playback": {
                "nowPlaying": "Lecture en cours",
                "upNext": "\u{00c0} suivre",
                "playbackError": "Erreur de lecture",
                "cannotPlay": "Impossible de lire ce fichier",
                "gapless": "Lecture sans pause",
                "crossfade": "Fondu encha\u{00ee}n\u{00e9}",
                "replayGain": "Normalisation du volume",
                "outputDevice": "Appareil de sortie",
                "zone": "Zone",
                "zones": "Zones",
                "transferPlayback": "Transf\u{00e9}rer la lecture",
                "groupZones": "Grouper les zones",
            },
            "streaming": {
                "connect": "Connecter",
                "disconnect": "D\u{00e9}connecter",
                "connected": "Connect\u{00e9}",
                "notConnected": "Non connect\u{00e9}",
                "authenticating": "Authentification...",
                "authFailed": "\u{00c9}chec de l'authentification",
                "newReleases": "Nouveaut\u{00e9}s",
                "featured": "S\u{00e9}lection",
                "forYou": "Pour vous",
                "charts": "Classements",
                "explore": "Explorer",
            },
            "errors": {
                "networkError": "Erreur r\u{00e9}seau",
                "serverError": "Erreur serveur",
                "notFound": "Introuvable",
                "unauthorized": "Non autoris\u{00e9}",
                "forbidden": "Acc\u{00e8}s refus\u{00e9}",
                "timeout": "D\u{00e9}lai d'attente d\u{00e9}pass\u{00e9}",
                "unknownError": "Erreur inconnue",
                "tryAgain": "Veuillez r\u{00e9}essayer.",
                "contactSupport": "Si le probl\u{00e8}me persiste, contactez le support.",
            },
        }),
        "en" => json!({
            "common": {
                "play": "Play",
                "pause": "Pause",
                "stop": "Stop",
                "next": "Next",
                "previous": "Previous",
                "search": "Search",
                "settings": "Settings",
                "library": "Library",
                "artists": "Artists",
                "albums": "Albums",
                "tracks": "Tracks",
                "genres": "Genres",
                "playlists": "Playlists",
                "radios": "Radios",
                "queue": "Queue",
                "favorites": "Favorites",
                "history": "History",
                "volume": "Volume",
                "mute": "Mute",
                "unmute": "Unmute",
                "shuffle": "Shuffle",
                "repeat": "Repeat",
                "repeatOne": "Repeat one",
                "nowPlaying": "Now playing",
                "addToQueue": "Add to queue",
                "addToPlaylist": "Add to playlist",
                "removeFromQueue": "Remove from queue",
                "clearQueue": "Clear queue",
                "playAll": "Play all",
                "shuffleAll": "Shuffle all",
                "sortBy": "Sort by",
                "filterBy": "Filter by",
                "noResults": "No results",
                "loading": "Loading...",
                "error": "Error",
                "retry": "Retry",
                "cancel": "Cancel",
                "save": "Save",
                "delete": "Delete",
                "edit": "Edit",
                "close": "Close",
                "back": "Back",
                "home": "Home",
                "more": "More",
                "less": "Less",
                "all": "All",
                "none": "None",
                "yes": "Yes",
                "no": "No",
                "ok": "OK",
                "confirm": "Confirm",
                "duration": "Duration",
                "year": "Year",
                "format": "Format",
                "quality": "Quality",
                "bitrate": "Bitrate",
                "sampleRate": "Sample rate",
                "bitDepth": "Bit depth",
            },
            "settings": {
                "title": "Settings",
                "musicDirs": "Music library locations",
                "musicDirsDescription": "Folders containing your audio files",
                "streaming": "Streaming services",
                "streamingDescription": "Connect your Tidal, Qobuz, Spotify accounts...",
                "zones": "Playback zones",
                "zonesDescription": "Configure your audio devices",
                "system": "System",
                "systemDescription": "Server information and maintenance",
                "profiles": "Profiles",
                "profilesDescription": "Manage user profiles",
                "appearance": "Appearance",
                "theme": "Theme",
                "themeDark": "Dark",
                "themeLight": "Light",
                "themeSystem": "System",
                "language": "Language",
                "scanLibrary": "Scan library",
                "scanning": "Scanning...",
                "lastScan": "Last scan",
                "version": "Version",
                "restart": "Restart",
                "shutdown": "Shutdown",
                "logs": "Logs",
                "diagnostics": "Diagnostics",
            },
            "onboarding": {
                "welcome": "Welcome to Tune!",
                "welcomeSubtitle": "Let's set up your music server in a few steps.",
                "addMusic": "Add your music",
                "addMusicDescription": "Point to the folders containing your audio files.",
                "connectStreaming": "Connect your streaming services",
                "connectStreamingDescription": "Access your Tidal, Qobuz, Spotify libraries...",
                "setupZones": "Set up your playback zones",
                "setupZonesDescription": "Automatically discover your DLNA/AirPlay devices.",
                "createProfile": "Create your profile",
                "createProfileDescription": "Personalize your music experience.",
                "complete": "Setup complete!",
                "completeDescription": "Your Tune server is ready. Enjoy your music!",
                "skip": "Skip",
                "next": "Next",
                "finish": "Finish",
                "stepOf": "Step {current} of {total}",
            },
            "library": {
                "allArtists": "All artists",
                "allAlbums": "All albums",
                "allTracks": "All tracks",
                "recentlyAdded": "Recently added",
                "recentlyPlayed": "Recently played",
                "mostPlayed": "Most played",
                "noTracks": "No tracks in library",
                "noAlbums": "No albums in library",
                "noArtists": "No artists in library",
                "tracksCount": "{count} track(s)",
                "albumsCount": "{count} album(s)",
                "artistsCount": "{count} artist(s)",
                "biography": "Biography",
                "discography": "Discography",
                "similarArtists": "Similar artists",
                "credits": "Credits",
                "composer": "Composer",
                "label": "Label",
            },
            "playback": {
                "nowPlaying": "Now playing",
                "upNext": "Up next",
                "playbackError": "Playback error",
                "cannotPlay": "Cannot play this file",
                "gapless": "Gapless playback",
                "crossfade": "Crossfade",
                "replayGain": "Volume normalization",
                "outputDevice": "Output device",
                "zone": "Zone",
                "zones": "Zones",
                "transferPlayback": "Transfer playback",
                "groupZones": "Group zones",
            },
            "streaming": {
                "connect": "Connect",
                "disconnect": "Disconnect",
                "connected": "Connected",
                "notConnected": "Not connected",
                "authenticating": "Authenticating...",
                "authFailed": "Authentication failed",
                "newReleases": "New releases",
                "featured": "Featured",
                "forYou": "For you",
                "charts": "Charts",
                "explore": "Explore",
            },
            "errors": {
                "networkError": "Network error",
                "serverError": "Server error",
                "notFound": "Not found",
                "unauthorized": "Unauthorized",
                "forbidden": "Forbidden",
                "timeout": "Request timed out",
                "unknownError": "Unknown error",
                "tryAgain": "Please try again.",
                "contactSupport": "If the problem persists, contact support.",
            },
        }),
        "de" => json!({
            "common": {
                "play": "Abspielen",
                "pause": "Pause",
                "stop": "Stopp",
                "next": "N\u{00e4}chster",
                "previous": "Vorheriger",
                "search": "Suchen",
                "settings": "Einstellungen",
                "library": "Bibliothek",
                "artists": "K\u{00fc}nstler",
                "albums": "Alben",
                "tracks": "Titel",
                "genres": "Genres",
                "playlists": "Playlists",
                "radios": "Radios",
                "queue": "Warteschlange",
                "favorites": "Favoriten",
                "history": "Verlauf",
                "volume": "Lautst\u{00e4}rke",
            },
            "settings": {
                "title": "Einstellungen",
                "musicDirs": "Musikverzeichnisse",
                "streaming": "Streaming-Dienste",
                "zones": "Wiedergabezonen",
                "system": "System",
            },
            "onboarding": {
                "welcome": "Willkommen bei Tune!",
                "addMusic": "Musik hinzuf\u{00fc}gen",
                "connectStreaming": "Streaming-Dienste verbinden",
                "setupZones": "Wiedergabezonen einrichten",
                "createProfile": "Profil erstellen",
                "complete": "Einrichtung abgeschlossen!",
                "skip": "\u{00dc}berspringen",
                "next": "Weiter",
                "finish": "Fertig",
            },
        }),
        "es" => json!({
            "common": {
                "play": "Reproducir",
                "pause": "Pausa",
                "stop": "Detener",
                "next": "Siguiente",
                "previous": "Anterior",
                "search": "Buscar",
                "settings": "Ajustes",
                "library": "Biblioteca",
                "artists": "Artistas",
                "albums": "\u{00c1}lbumes",
                "tracks": "Pistas",
                "genres": "G\u{00e9}neros",
                "playlists": "Listas",
                "radios": "Radios",
                "queue": "Cola",
                "favorites": "Favoritos",
                "history": "Historial",
                "volume": "Volumen",
            },
            "settings": {
                "title": "Ajustes",
                "musicDirs": "Directorios de m\u{00fa}sica",
                "streaming": "Servicios de streaming",
                "zones": "Zonas de reproducci\u{00f3}n",
                "system": "Sistema",
            },
            "onboarding": {
                "welcome": "\u{00a1}Bienvenido a Tune!",
                "addMusic": "A\u{00f1}ade tu m\u{00fa}sica",
                "connectStreaming": "Conecta tus servicios de streaming",
                "setupZones": "Configura tus zonas de reproducci\u{00f3}n",
                "createProfile": "Crea tu perfil",
                "complete": "\u{00a1}Configuraci\u{00f3}n completada!",
                "skip": "Omitir",
                "next": "Siguiente",
                "finish": "Finalizar",
            },
        }),
        "it" => json!({
            "common": {
                "play": "Riproduci",
                "pause": "Pausa",
                "stop": "Ferma",
                "next": "Successivo",
                "previous": "Precedente",
                "search": "Cerca",
                "settings": "Impostazioni",
                "library": "Libreria",
                "artists": "Artisti",
                "albums": "Album",
                "tracks": "Brani",
                "genres": "Generi",
                "playlists": "Playlist",
                "radios": "Radio",
                "queue": "Coda",
                "favorites": "Preferiti",
                "history": "Cronologia",
                "volume": "Volume",
            },
            "settings": {
                "title": "Impostazioni",
                "musicDirs": "Directory musicali",
                "streaming": "Servizi di streaming",
                "zones": "Zone di riproduzione",
                "system": "Sistema",
            },
            "onboarding": {
                "welcome": "Benvenuto in Tune!",
                "addMusic": "Aggiungi la tua musica",
                "connectStreaming": "Collega i tuoi servizi di streaming",
                "setupZones": "Configura le zone di riproduzione",
                "createProfile": "Crea il tuo profilo",
                "complete": "Configurazione completata!",
                "skip": "Salta",
                "next": "Avanti",
                "finish": "Fine",
            },
        }),
        "zh" => json!({
            "common": {
                "play": "\u{64ad}\u{653e}",
                "pause": "\u{6682}\u{505c}",
                "stop": "\u{505c}\u{6b62}",
                "next": "\u{4e0b}\u{4e00}\u{9996}",
                "previous": "\u{4e0a}\u{4e00}\u{9996}",
                "search": "\u{641c}\u{7d22}",
                "settings": "\u{8bbe}\u{7f6e}",
                "library": "\u{97f3}\u{4e50}\u{5e93}",
                "artists": "\u{827a}\u{672f}\u{5bb6}",
                "albums": "\u{4e13}\u{8f91}",
                "tracks": "\u{66f2}\u{76ee}",
                "genres": "\u{6d41}\u{6d3e}",
                "playlists": "\u{64ad}\u{653e}\u{5217}\u{8868}",
                "radios": "\u{7535}\u{53f0}",
                "queue": "\u{961f}\u{5217}",
                "favorites": "\u{6536}\u{85cf}",
                "history": "\u{5386}\u{53f2}",
                "volume": "\u{97f3}\u{91cf}",
            },
            "settings": {
                "title": "\u{8bbe}\u{7f6e}",
                "musicDirs": "\u{97f3}\u{4e50}\u{76ee}\u{5f55}",
                "streaming": "\u{6d41}\u{5a92}\u{4f53}\u{670d}\u{52a1}",
                "zones": "\u{64ad}\u{653e}\u{533a}\u{57df}",
                "system": "\u{7cfb}\u{7edf}",
            },
            "onboarding": {
                "welcome": "\u{6b22}\u{8fce}\u{4f7f}\u{7528} Tune\u{ff01}",
                "addMusic": "\u{6dfb}\u{52a0}\u{60a8}\u{7684}\u{97f3}\u{4e50}",
                "connectStreaming": "\u{8fde}\u{63a5}\u{6d41}\u{5a92}\u{4f53}\u{670d}\u{52a1}",
                "setupZones": "\u{8bbe}\u{7f6e}\u{64ad}\u{653e}\u{533a}\u{57df}",
                "createProfile": "\u{521b}\u{5efa}\u{4e2a}\u{4eba}\u{8d44}\u{6599}",
                "complete": "\u{8bbe}\u{7f6e}\u{5b8c}\u{6210}\u{ff01}",
                "skip": "\u{8df3}\u{8fc7}",
                "next": "\u{4e0b}\u{4e00}\u{6b65}",
                "finish": "\u{5b8c}\u{6210}",
            },
        }),
        "ko" => json!({
            "common": {
                "play": "재생",
                "pause": "일시정지",
                "stop": "정지",
                "next": "다음",
                "previous": "이전",
                "search": "검색",
                "settings": "설정",
                "library": "라이브러리",
                "artists": "아티스트",
                "albums": "앨범",
                "tracks": "트랙",
                "genres": "장르",
                "playlists": "재생목록",
                "radios": "라디오",
                "queue": "대기열",
                "favorites": "즐겨찾기",
                "history": "기록",
                "volume": "볼륨",
            },
            "settings": {
                "title": "설정",
                "musicDirs": "음악 폴더",
                "streaming": "스트리밍 서비스",
                "zones": "재생 영역",
                "system": "시스템",
            },
            "onboarding": {
                "welcome": "Tune에 오신 것을 환영합니다!",
                "addMusic": "음악 추가",
                "connectStreaming": "스트리밍 서비스 연결",
                "setupZones": "재생 영역 설정",
                "createProfile": "프로필 만들기",
                "complete": "설정 완료!",
                "skip": "건너뛰기",
                "next": "다음",
                "finish": "완료",
            },
        }),
        "ja" => json!({
            "common": {
                "play": "\u{518d}\u{751f}",
                "pause": "\u{4e00}\u{6642}\u{505c}\u{6b62}",
                "stop": "\u{505c}\u{6b62}",
                "next": "\u{6b21}\u{3078}",
                "previous": "\u{524d}\u{3078}",
                "search": "\u{691c}\u{7d22}",
                "settings": "\u{8a2d}\u{5b9a}",
                "library": "\u{30e9}\u{30a4}\u{30d6}\u{30e9}\u{30ea}",
                "artists": "\u{30a2}\u{30fc}\u{30c6}\u{30a3}\u{30b9}\u{30c8}",
                "albums": "\u{30a2}\u{30eb}\u{30d0}\u{30e0}",
                "tracks": "\u{30c8}\u{30e9}\u{30c3}\u{30af}",
                "genres": "\u{30b8}\u{30e3}\u{30f3}\u{30eb}",
                "playlists": "\u{30d7}\u{30ec}\u{30a4}\u{30ea}\u{30b9}\u{30c8}",
                "radios": "\u{30e9}\u{30b8}\u{30aa}",
                "queue": "\u{30ad}\u{30e5}\u{30fc}",
                "favorites": "\u{304a}\u{6c17}\u{306b}\u{5165}\u{308a}",
                "history": "\u{5c65}\u{6b74}",
                "volume": "\u{97f3}\u{91cf}",
            },
            "settings": {
                "title": "\u{8a2d}\u{5b9a}",
                "musicDirs": "\u{97f3}\u{697d}\u{30c7}\u{30a3}\u{30ec}\u{30af}\u{30c8}\u{30ea}",
                "streaming": "\u{30b9}\u{30c8}\u{30ea}\u{30fc}\u{30df}\u{30f3}\u{30b0}\u{30b5}\u{30fc}\u{30d3}\u{30b9}",
                "zones": "\u{518d}\u{751f}\u{30be}\u{30fc}\u{30f3}",
                "system": "\u{30b7}\u{30b9}\u{30c6}\u{30e0}",
            },
            "onboarding": {
                "welcome": "Tune\u{3078}\u{3088}\u{3046}\u{3053}\u{305d}\u{ff01}",
                "addMusic": "\u{97f3}\u{697d}\u{3092}\u{8ffd}\u{52a0}",
                "connectStreaming": "\u{30b9}\u{30c8}\u{30ea}\u{30fc}\u{30df}\u{30f3}\u{30b0}\u{30b5}\u{30fc}\u{30d3}\u{30b9}\u{3092}\u{63a5}\u{7d9a}",
                "setupZones": "\u{518d}\u{751f}\u{30be}\u{30fc}\u{30f3}\u{3092}\u{8a2d}\u{5b9a}",
                "createProfile": "\u{30d7}\u{30ed}\u{30d5}\u{30a3}\u{30fc}\u{30eb}\u{3092}\u{4f5c}\u{6210}",
                "complete": "\u{30bb}\u{30c3}\u{30c8}\u{30a2}\u{30c3}\u{30d7}\u{5b8c}\u{4e86}\u{ff01}",
                "skip": "\u{30b9}\u{30ad}\u{30c3}\u{30d7}",
                "next": "\u{6b21}\u{3078}",
                "finish": "\u{5b8c}\u{4e86}",
            },
        }),
        _ => json!({}),
    }
}
