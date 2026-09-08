//! Le rapport de bogue dit-il enfin sous quels RÉGLAGES il a été produit ?
//! (#2856)
//!
//! ## Le constat
//!
//! `generate_bug_report` composait neuf sections — `Library`, `Zones`,
//! `Streaming Services`, `Network`, `Output Providers`, `OAAT Endpoints`,
//! `Ring starvation`, `Database`, `Recent Logs` — et **aucune** ne disait un
//! seul réglage. Ni l'état de l'enrichissement au scan, ni le moteur audio.
//! Ce sont pourtant les deux premières questions posées à un testeur sur un
//! ticket de métadonnées et sur un ticket de son, et le serveur a les deux
//! valeurs sous la main : la fiche système (`/system/profile`) publiait déjà
//! `enrich_on_scan`, et `/system/diagnostics` publiait déjà le backend actif.
//!
//! ## Pourquoi cette épreuve passe par la ROUTE MONTÉE
//!
//! Une épreuve qui appellerait `support_settings` en direct resterait verte
//! alors même que le rapport ne l'appelle pas — le défaut « écrit mais pas
//! branché » que ce dépôt connaît par cœur. On lit donc ce que rend
//! `GET /api/v1/system/bug-report/markdown`, c'est-à-dire le texte que le
//! testeur COLLE sur le forum, et le JSON du même rapport.
//!
//! ## La seconde moitié : rien de sensible ne doit y entrer
//!
//! Le forum est public et le miroir republie la fiche. Cette épreuve écrit de
//! vrais secrets dans `settings` puis exige qu'aucun ne ressorte — ni son nom
//! de clé, ni sa valeur — de la section `## Settings`, du `settings` du JSON,
//! ni de la fiche système. Le détecteur a sa propre contre-épreuve
//! (`le_detecteur_de_fuite_rougit_sur_un_texte_fabrique`) : sans elle, il
//! pourrait ne rien détecter du tout et cette garde serait verte contre rien.
//!
//! `autotests = false` dans `tune-server/Cargo.toml` : sans la cible
//! `[[test]]` déclarée pour ce fichier, il ne serait JAMAIS compilé, donc vert
//! contre rien.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Le titre de la section que ce fichier garde.
const SECTION: &str = "## Settings";

/// Des secrets réalistes, posés en base avant chaque mesure. La valeur est
/// choisie improbable pour qu'un `contains` la trouve à coup sûr si elle
/// fuyait.
const SECRETS: &[(&str, &str)] = &[
    ("discogs_token", "jeton-discogs-NE-DOIT-PAS-SORTIR-2856"),
    ("license_key", "TUNE-CLE-NE-DOIT-PAS-SORTIR-2856"),
    ("jwt_secret", "secret-jwt-NE-DOIT-PAS-SORTIR-2856"),
    ("lastfm_api_key", "cle-lastfm-NE-DOIT-PAS-SORTIR-2856"),
];

/// Le serveur sur SQLite en mémoire, secrets posés.
fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for (cle, valeur) in SECRETS {
        settings.set(cle, valeur).expect("écriture du réglage");
    }
    state
}

/// Le corps brut d'une route, avec son statut exigé 2xx : un 404 ou un 401
/// prouverait que rien n'a été construit.
async fn texte_de(state: &AppState, route: &str) -> String {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 8 * 1024 * 1024)
        .await
        .expect("corps de reponse");
    let texte = String::from_utf8_lossy(&octets).into_owned();
    assert!(statut.is_success(), "{route} → {statut} : {texte}");
    texte
}

async fn json_de(state: &AppState, route: &str) -> Value {
    let texte = texte_de(state, route).await;
    serde_json::from_str(&texte).unwrap_or_else(|e| panic!("{route} : corps non JSON ({e})"))
}

/// La section `## Settings` du markdown, titre exclu, jusqu'au titre suivant.
///
/// Extraction STRICTE : une section absente fait tomber l'épreuve plutôt que
/// de rendre une chaîne vide, contre laquelle un `!contains(secret)` serait
/// trivialement vert.
fn section_reglages(markdown: &str) -> String {
    let debut = markdown
        .find(SECTION)
        .unwrap_or_else(|| panic!("section « {SECTION} » absente du rapport :\n{markdown}"));
    let apres = &markdown[debut + SECTION.len()..];
    let fin = apres.find("\n#").unwrap_or(apres.len());
    let section = apres[..fin].trim().to_string();
    assert!(
        !section.is_empty(),
        "section « {SECTION} » vide — rien à mesurer"
    );
    section
}

/// La valeur écrite après `- <étiquette>:` dans une section markdown.
fn ligne_apres(section: &str, etiquette: &str) -> String {
    let prefixe = format!("- {etiquette}:");
    section
        .lines()
        .find_map(|l| l.trim().strip_prefix(&prefixe))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| panic!("aucune ligne « {prefixe} » dans :\n{section}"))
}

/// Les secrets qui FUIENT dans un texte : nom de clé ou valeur.
///
/// Rend la liste plutôt qu'un booléen pour que l'échec nomme le coupable.
fn fuites(texte: &str) -> Vec<String> {
    let mut trouvees = Vec::new();
    for (cle, valeur) in SECRETS {
        if texte.contains(valeur) {
            trouvees.push(format!("la VALEUR de {cle}"));
        }
        if texte.contains(cle) {
            trouvees.push(format!("le NOM {cle}"));
        }
    }
    trouvees
}

/// Les clés d'un objet JSON de réglages que le dépôt classe secrètes.
fn cles_secretes(objet: &Value) -> Vec<String> {
    objet
        .as_object()
        .map(|m| {
            m.keys()
                .filter(|k| tune_core::secrets::est_secret(k))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Le rapport que le testeur colle sur le forum porte enfin l'état de
/// l'enrichissement au scan ET le moteur audio.
#[tokio::test]
async fn le_rapport_de_bogue_porte_les_reglages_et_le_moteur_audio() {
    let state = etat();
    let markdown = texte_de(&state, "/api/v1/system/bug-report/markdown").await;
    let section = section_reglages(&markdown);

    // Le fait qui manquait pour arbitrer un ticket de métadonnées.
    let enrichissement = ligne_apres(&section, "enrich_on_scan");
    assert!(
        enrichissement == "true" || enrichissement == "false",
        "« enrich_on_scan » doit être lisible comme un booléen, lu : \
         « {enrichissement} »\n{section}"
    );

    // Le fait qui manquait pour arbitrer un ticket de son. Trois valeurs, pas
    // une : le demandé, l'actif, et ce que le mode exclusif vaut vraiment.
    let backend = ligne_apres(&section, "Audio backend");
    assert!(
        backend.contains("requested=") && backend.contains("active="),
        "le rapport doit dire le backend DEMANDÉ et le backend ACTIF, lu : \
         « {backend} »"
    );
    let exclusif = ligne_apres(&section, "Exclusive mode");
    for champ in ["requested=", "effective=", "forced="] {
        assert!(
            exclusif.contains(champ),
            "le mode exclusif doit dire « {champ} », lu : « {exclusif} »"
        );
    }
}

/// Le même rapport en JSON — c'est lui que le client web envoie au ticket.
#[tokio::test]
async fn le_json_du_rapport_porte_les_memes_reglages() {
    let state = etat();
    let rapport = json_de(&state, "/api/v1/system/bug-report").await;

    assert!(
        rapport["settings"]["enrich_on_scan"].is_boolean(),
        "settings.enrich_on_scan absent du rapport JSON : {}",
        rapport["settings"]
    );
    assert!(
        rapport["audio"]["backend_requested"].is_string(),
        "audio.backend_requested absent : {}",
        rapport["audio"]
    );
    assert!(
        rapport["audio"]["exclusive_mode"]["effective"].is_boolean(),
        "audio.exclusive_mode.effective absent : {}",
        rapport["audio"]
    );

    // Le markdown et le JSON du MÊME rapport ne doivent pas se contredire.
    let markdown = rapport["markdown"].as_str().expect("markdown du rapport");
    let section = section_reglages(markdown);
    assert_eq!(
        ligne_apres(&section, "enrich_on_scan"),
        rapport["settings"]["enrich_on_scan"].to_string(),
        "le markdown et le JSON du même rapport ne disent pas la même chose"
    );
}

/// La fiche système porte le moteur audio, elle aussi : c'est elle que le
/// miroir republie sur le forum.
#[tokio::test]
async fn la_fiche_systeme_porte_le_moteur_audio() {
    let state = etat();
    let fiche = json_de(&state, "/api/v1/system/profile").await;
    assert!(
        fiche["server"]["audio"]["backend_requested"].is_string(),
        "server.audio.backend_requested absent de la fiche : {}",
        fiche["server"]
    );
    assert!(
        fiche["server"]["audio"]["exclusive_mode"]["forced"].is_boolean(),
        "server.audio.exclusive_mode.forced absent : {}",
        fiche["server"]
    );
}

/// SECONDE MOITIÉ — rien de sensible n'entre par cette porte. Le forum est
/// public et le miroir republie la fiche.
#[tokio::test]
async fn aucun_secret_ne_sort_par_les_reglages_publies() {
    let state = etat();

    let markdown = texte_de(&state, "/api/v1/system/bug-report/markdown").await;
    let section = section_reglages(&markdown);
    assert!(
        fuites(&section).is_empty(),
        "la section « {SECTION} » du rapport laisse fuiter {:?} :\n{section}",
        fuites(&section)
    );

    let rapport = json_de(&state, "/api/v1/system/bug-report").await;
    assert!(
        cles_secretes(&rapport["settings"]).is_empty(),
        "le rapport JSON publie des clés secrètes : {:?}",
        cles_secretes(&rapport["settings"])
    );
    assert!(
        fuites(&rapport["settings"].to_string()).is_empty(),
        "le rapport JSON laisse fuiter {:?}",
        fuites(&rapport["settings"].to_string())
    );

    let fiche = json_de(&state, "/api/v1/system/profile").await;
    assert!(
        cles_secretes(&fiche["settings"]).is_empty(),
        "la fiche système publie des clés secrètes : {:?}",
        cles_secretes(&fiche["settings"])
    );
    assert!(
        fuites(&fiche["settings"].to_string()).is_empty(),
        "la fiche système laisse fuiter {:?}",
        fuites(&fiche["settings"].to_string())
    );
    assert!(
        fuites(&fiche["server"].to_string()).is_empty(),
        "le bloc `server` de la fiche laisse fuiter {:?}",
        fuites(&fiche["server"].to_string())
    );
}

/// CONTRE-ÉPREUVE du détecteur lui-même. Sans elle, `fuites` pourrait ne rien
/// détecter du tout et l'épreuve précédente serait verte contre rien.
#[test]
fn le_detecteur_de_fuite_rougit_sur_un_texte_fabrique() {
    let fabrique = format!("- {}: {}\n", SECRETS[0].0, SECRETS[0].1);
    let trouvees = fuites(&fabrique);
    assert_eq!(
        trouvees.len(),
        2,
        "le détecteur doit voir le NOM et la VALEUR : {trouvees:?}"
    );
    assert!(fuites("- enrich_on_scan: true\n").is_empty());
    assert!(tune_core::secrets::est_secret(SECRETS[0].0));
    assert!(!tune_core::secrets::est_secret("enrich_on_scan"));
}
