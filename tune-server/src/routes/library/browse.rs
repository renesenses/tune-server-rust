use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;
use tune_http_types::panne_sql::OuDefautJournalise;
use unicode_normalization::UnicodeNormalization;

use super::facets::hors_executeur;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct BrowseQuery {
    path: String,
}

#[derive(Deserialize)]
pub(super) struct FolderQuery {
    path: Option<String>,
}

pub(super) async fn browse_roots(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let lang = crate::i18n::lang_from_header(&headers);
    // #5677 — un `COUNT(*) … LIKE` par racine (le `LIKE` ne s'appuie sur aucun
    // index : toute la table à chaque fois) et un `is_dir()` par racine, qui
    // peut rester suspendu des secondes sur un partage réseau décroché. Hors de
    // l'exécuteur, comme les routes de #5438.
    hors_executeur("browse_roots", move || lire_les_racines(&state, &lang))
        .await
        .map(Json)
}

/// Le corps de `GET /library/browse`, exécuté HORS de l'exécuteur (#5677).
fn lire_les_racines(state: &AppState, lang: &str) -> Value {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let mut roots: Vec<Value> = dirs
        .iter()
        .map(|d| {
            let norm = tune_core::scanner::walker::normalize_path(d);
            let norm_nfc: String = norm.nfc().collect();
            // Un seul constructeur de motif pour tout le fichier : il replie en
            // NFC (la forme dans laquelle le scanner écrit `file_path`) et rogne
            // le séparateur final, sans quoi une bibliothèque pointée sur une
            // racine de lecteur ou de partage (« D:\ », « \\NAS\ ») produit un
            // séparateur doublé (« D:\\% ») qui ne correspond à rien → 0 piste.
            let pattern = tune_core::db::track_repo::folder_like_pattern(&norm);
            let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
                "$1"
            } else {
                "?1"
            };
            let esc = tune_core::db::track_repo::like_escape_clause();
            let count: i64 = match state.backend.query_one(
                &format!("SELECT COUNT(*) FROM tracks WHERE file_path LIKE {ph}{esc}"),
                &[&pattern as &dyn tune_core::db::backend::ToSqlValue],
            ) {
                Ok(Some(cols)) => cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                Ok(None) => 0,
                Err(e) => {
                    warn!(path = %norm_nfc, error = %e, "browse_root_count_failed");
                    0
                }
            };
            if count == 0 {
                let sample = state
                    .backend
                    .query_one("SELECT file_path FROM tracks LIMIT 1", &[])
                    .ok()
                    .flatten()
                    .and_then(|r| r.first().and_then(|v| v.as_string()));
                warn!(
                    music_dir = %norm_nfc,
                    pattern = %pattern,
                    sample_file_path = ?sample,
                    "browse_root_zero_tracks"
                );
            }
            let name = std::path::Path::new(&norm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&norm);
            // Whether the configured directory still exists on disk. A stale
            // music dir (renamed/unmounted share, e.g. a NAS mount that moved)
            // otherwise shows as an empty phantom folder with no explanation
            // (Yacine: two configured roots — one gone, one empty — while the
            // real music sits under a different root). Surfacing this lets the
            // UI flag "introuvable / vérifier le montage" vs a genuinely empty
            // but valid directory.
            #[cfg(test)]
            simuler_un_disque_lent(std::path::Path::new(&norm));
            let exists = std::path::Path::new(&norm).is_dir();
            // `exists: false` dit QUE le dossier est introuvable, jamais
            // POURQUOI. Or la cause la plus frequente sous Windows a une
            // reparation en un geste, et personne ne la devine : une lettre de
            // lecteur reseau n'appartient qu'a la session qui l'a creee
            // (testeur EverSolo, 04/08/2026 — `Z:\EDF7-FE43\EverSoloMusic`
            // annonce a 0 piste quand l'appareil y voit 34 169 titres). Le
            // conseil n'est calcule que sur un dossier introuvable : sur une
            // racine saine il n'aurait rien a expliquer (#1190).
            let hint = (!exists)
                .then(|| crate::chemin_inaccessible::conseil(lang, &norm))
                .flatten();
            json!({
                "path": norm, "name": name, "track_count": count,
                "exists": exists, "hint": hint,
            })
        })
        .collect();

    // Fallback: if no configured music_dir matches any stored path (the
    // browse_root_zero_tracks drift — e.g. .18 set to /mnt/music while files
    // live under /data/music), the Répertoires view would show only empty roots
    // and browsing would go nowhere. Surface the real root inferred from the
    // data so it still works — the same fallback the Oxygen folder facet uses.
    let none_populated = roots
        .iter()
        .all(|r| r.get("track_count").and_then(|v| v.as_i64()).unwrap_or(0) == 0);
    if none_populated
        && let Some(base) = tune_core::db::track_repo::derive_common_root(state.backend.as_ref())
    {
        let pattern = tune_core::db::track_repo::folder_like_pattern(&base);
        let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
            "$1"
        } else {
            "?1"
        };
        let esc = tune_core::db::track_repo::like_escape_clause();
        let count: i64 = state
            .backend
            .query_one(
                &format!("SELECT COUNT(*) FROM tracks WHERE file_path LIKE {ph}{esc}"),
                &[&pattern as &dyn tune_core::db::backend::ToSqlValue],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let dup = roots
            .iter()
            .any(|r| r.get("path").and_then(|v| v.as_str()) == Some(base.as_str()));
        if count > 0 && !dup {
            let name = std::path::Path::new(&base)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&base)
                .to_string();
            let exists = std::path::Path::new(&base).is_dir();
            warn!(root = %base, count, "browse_roots_data_derived_fallback");
            roots.push(json!({
                "path": base, "name": name, "track_count": count,
                "exists": exists, "derived": true
            }));
        }
    }

    json!({ "roots": roots })
}

/// Résout le chemin demandé en tenant compte de la forme de normalisation
/// Unicode réellement utilisée par le système de fichiers.
///
/// Tune renvoie les chemins en NFC, et le client les lui renvoie tels quels.
/// Sur APFS la recherche est insensible à la forme, donc NFC suffit — mais pas
/// sur un partage réseau : un volume SMB monté depuis macOS est sensible à la
/// forme, et un dossier accentué créé côté NAS (« CDThèque ») peut n'exister
/// qu'en NFD. Le chemin était alors déclaré invalide et la navigation
/// s'arrêtait là (retour Yves Corbat, NAS Synology en SMB).
///
/// Renvoie le chemin absolu qui existe réellement, ou `None`.
fn resolve_browse_path(raw: &str) -> Option<String> {
    let base = tune_core::scanner::walker::normalize_path(raw);
    let nfc: String = base.nfc().collect();
    let nfd: String = base.nfd().collect();
    // La forme brute est essayée aussi : elle est déjà correcte quand le client
    // renvoie ce que le système de fichiers a fourni.
    for candidate in [nfc, nfd, base] {
        let path = std::path::Path::new(&candidate);
        if path.is_absolute() && path.exists() {
            return Some(candidate);
        }
    }
    None
}

/// `fichier` est-il un enfant **direct** du répertoire `repertoire_nfc` ?
///
/// Le `LIKE` qui précède est récursif : il ramène aussi les pistes des
/// sous-dossiers, et ce filtre les écarte. Les deux côtés sont repliés en NFC
/// avant comparaison — `repertoire_nfc` vient du disque (donc potentiellement
/// décomposé), `fichier` vient de `tracks.file_path` (composé par le scanner).
/// Comparer deux formes différentes rendait `false` pour CHAQUE piste du
/// dossier, et l'écran annonçait « aucune piste » sur un dossier scanné.
fn est_enfant_direct(fichier: &str, repertoire_nfc: &str) -> bool {
    std::path::Path::new(fichier)
        .parent()
        .and_then(|p| p.to_str())
        .is_some_and(|parent| parent.nfc().collect::<String>() == repertoire_nfc)
}

pub(super) async fn browse_directory(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> Result<impl IntoResponse, AppError> {
    let lang = crate::i18n::lang_from_header(&headers);
    // #5677 — tout ce qui suit est synchrone : `exists()`/`read_dir`/`is_dir()`
    // sur le disque (un partage SMB ou NFS lent ou décroché peut y rester
    // suspendu des secondes), puis deux lectures SQLite à `LIKE` récursif sur
    // toute la table. Posées sur un fil de l'exécuteur, elles l'immobilisent ;
    // quand elles les tiennent tous, le flux HTTP vers le renderer ne part
    // plus. Hors de l'exécuteur, comme les routes de #5438.
    let corps = hors_executeur("browse_directory", move || {
        lire_le_repertoire(&state, &lang, q)
    })
    .await??;
    Ok(Json(corps))
}

/// Retard injecté par les tests avant la lecture d'un dossier donné : il
/// simule un `read_dir` lent (partage réseau) sans dépendre d'un vrai montage.
/// N'existe pas hors des tests.
#[cfg(test)]
static DOSSIERS_LENTS: std::sync::Mutex<Vec<(std::path::PathBuf, std::time::Duration)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn simuler_un_disque_lent(dossier: &std::path::Path) {
    let retard = DOSSIERS_LENTS
        .lock()
        .unwrap()
        .iter()
        .find(|(d, _)| d == dossier)
        .map(|(_, r)| *r);
    if let Some(retard) = retard {
        std::thread::sleep(retard);
    }
}

/// Le corps de `GET /library/browse/dir`, exécuté HORS de l'exécuteur (#5677).
fn lire_le_repertoire(state: &AppState, lang: &str, q: BrowseQuery) -> Result<Value, AppError> {
    let normalized_query =
        resolve_browse_path(&q.path).ok_or_else(|| AppError::bad_request("invalid path"))?;
    let resolved = std::path::Path::new(&normalized_query);

    // Verify path is under a configured music dir.
    // Use std::path::Path::starts_with for OS-aware prefix matching
    // (handles both `/` and `\` separators on Windows).
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    // Comparaison sur une forme Unicode commune : le chemin résolu peut être en
    // NFD (ce qu'expose le partage SMB) alors que le dossier musical configuré
    // est en NFC. Sans cela, un chemin pourtant valide était déclaré hors des
    // dossiers musicaux — le même défaut que la résolution ci-dessus, une ligne
    // plus loin.
    let resolved_nfc: String = normalized_query.nfc().collect();
    let resolved_nfc = std::path::Path::new(&resolved_nfc);
    let music_root = dirs.iter().find(|d| {
        let norm_dir: String = tune_core::scanner::walker::normalize_path(d)
            .nfc()
            .collect();
        resolved_nfc.starts_with(&norm_dir)
    });
    let Some(music_root) = music_root else {
        return Err(AppError::bad_request(
            "path not under a configured music directory",
        ));
    };
    let music_root = tune_core::scanner::walker::normalize_path(music_root);

    // List subdirectories. On lit le chemin RÉSOLU, pas `q.path` brut : c'est
    // celui dont on vient de vérifier l'existence et l'appartenance à un dossier
    // musical. Lire l'autre revenait à valider un chemin et en ouvrir un second.
    let mut subdirs: Vec<Value> = Vec::new();
    // `read_dir` echouait en silence : le `if let Ok` laissait la liste vide et
    // l'interface annoncait « Dossier vide » pour un dossier qui n'est pas vide
    // mais INJOIGNABLE — lecteur reseau non monte, permissions refusees. Sous
    // Windows le cas est courant : une lettre mappee (`Z:`) appartient a la
    // session qui l'a creee et reste invisible au processus serveur, a plus
    // forte raison lance en service (testeur EverSolo, 04/08/2026 : 0 piste
    // annoncee pour un partage qui en contient 34 169). On remonte desormais la
    // raison au lieu de mentir (#1190).
    let mut unreadable: Option<String> = None;
    #[cfg(test)]
    simuler_un_disque_lent(resolved);
    match std::fs::read_dir(resolved) {
        Err(e) => {
            warn!(path = %resolved.display(), error = %e, "browse_dir_unreadable");
            unreadable = Some(e.to_string());
        }
        Ok(entries) => {
            // UNE requete pour TOUT le niveau, au lieu d'un
            // `SELECT COUNT(*) … LIKE` par sous-dossier (#3857). Le commentaire
            // de `like_escape_clause` dit pourquoi la boucle coutait si cher :
            // ce `LIKE` ne peut pas s'appuyer sur l'index de `file_path`, donc
            // chaque compte parcourait toute la table. Mesure sur 155 829
            // pistes (la bibliotheque de Pierre M, fil forum 1671) : un coffret
            // de 63 dossiers `CDxx` passe de 1 166 ms a 21 ms, et une racine de
            // 11 891 dossiers d'artistes de 303 965 ms a 187 ms.
            //
            // L'echec est journalise UNE fois et rend une table vide : chaque
            // sous-dossier retombe alors a 0, soit exactement ce que la boucle
            // faisait de son cote sur `browse_dir_count_failed`.
            let comptes = match tune_core::db::track_repo::compter_pistes_par_sous_dossier(
                state.backend.as_ref(),
                &normalized_query,
            ) {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %normalized_query, error = %e, "browse_dir_count_failed");
                    tune_core::db::track_repo::ComptesParSousDossier::default()
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let dir_path: String = path.to_string_lossy().nfc().collect();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    if name.starts_with('.') {
                        continue;
                    }
                    let track_count: i64 = comptes.get(&name);
                    subdirs.push(
                        json!({ "name": name, "path": dir_path, "track_count": track_count }),
                    );
                }
            }
            // conn removed — using state.backend
        }
    }
    // Le conseil se calcule sur le chemin TEL QUE CONFIGURE (`q.path`), pas sur
    // le chemin resolu : c'est celui que l'utilisateur a saisi, donc celui qui
    // porte encore la lettre de lecteur qu'il faut lui apprendre a remplacer.
    let access_hint = unreadable
        .as_ref()
        .and_then(|_| crate::chemin_inaccessible::conseil(lang, &q.path));

    // Ordre alphabétique naturel, celui des dossiers du serveur média (#5582) :
    // le tri par octets rangeait toutes les majuscules avant les minuscules,
    // et un dossier « haydn » partait après « Z », hors de la vue (fil 2072).
    subdirs.sort_by(|a, b| {
        tune_core::upnp_server::comparer_naturel(
            a.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        )
    });

    // List tracks in this directory (not recursive — only direct children)
    //
    // `normalized_query` est la forme qui EXISTE SUR LE DISQUE : `resolve_browse_path`
    // essaie NFC puis NFD et rend la première que le système de fichiers ouvre.
    // Sur un partage sensible à la forme — SMB Synology, volume venu de macOS —
    // un dossier accentué n'existe qu'en NFD, et c'est donc du NFD qui ressort.
    // La base, elle, ne contient que du NFC : le scanner replie chaque chemin
    // avant de l'insérer. Les comparer tels quels ne rapproche pas un octet.
    //
    // Le correctif #1329 a réparé les deux premiers usages de ce chemin —
    // l'ouverture du dossier et le contrôle d'appartenance aux dossiers
    // musicaux — et a laissé les deux derniers nus : le motif `LIKE` ci-dessous
    // et la comparaison `parent == …` du filtre `is_direct`. Résultat : le
    // dossier s'ouvrait, ses SOUS-dossiers s'affichaient avec le bon compte
    // (eux passent par `folder_like_pattern`, qui replie), et sa propre liste de
    // pistes restait vide. Les deux points manquants sont repliés ici.
    let repertoire_nfc: String = normalized_query.nfc().collect();
    let dir_prefix = tune_core::db::track_repo::folder_like_pattern(&repertoire_nfc);
    let postgres = state.backend.engine() == tune_core::db::engine::Engine::Postgres;
    let (ph, ph2, pos) = if postgres {
        ("$1", "$2", "strpos")
    } else {
        ("?1", "?2", "instr")
    };
    // Le `LIKE` est RÉCURSIF : il ramène aussi les pistes des sous-dossiers,
    // que `est_enfant_direct` écarte ensuite une par une. Sur la racine d'une
    // grande bibliothèque, c'était donc TOUTE la bibliothèque rapatriée — seize
    // colonnes et deux jointures — pour n'en garder que les quelques fichiers
    // posés à la racine. Mesure sur 155 829 pistes (#3857, Pierre M) :
    // 155 829 lignes en 343 ms, contre 0 ligne en 103 ms une fois le « pas de
    // séparateur après le préfixe » poussé dans le SQL.
    //
    // Ce n'est qu'un PRÉ-filtre : `est_enfant_direct` reste l'autorité, plus
    // bas, et il est plus strict (il compare les deux chemins repliés en NFC).
    // Un pré-filtre plus strict que lui perdrait des pistes ; celui-ci ne peut
    // qu'en laisser passer.
    let sep_txt = std::path::MAIN_SEPARATOR.to_string();
    let depart_enfant =
        repertoire_nfc.trim_end_matches(['/', '\\']).chars().count() + sep_txt.chars().count() + 1;
    // #4625 — les pistes découpées par une feuille CUE (`file_path` NULL, le
    // fichier dans `cue_media_path`) sont des enfants du dossier comme les
    // autres : sans le `COALESCE`, un album CUE s'ouvrait sur une liste vide.
    // La colonne 16 porte ce chemin pour `est_enfant_direct` ; `file_path`
    // reste publié tel qu'en base (NULL pour une tranche CUE).
    let sql = format!(
        "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, \
               t.disc_number, t.track_number, t.duration_ms, t.file_path, \
               t.format, t.sample_rate, t.bit_depth, t.genre, t.year, al.cover_path, \
               COALESCE(t.file_path, t.cue_media_path) \
               FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
               LEFT JOIN artists ar ON t.artist_id = ar.id \
               WHERE COALESCE(t.file_path, t.cue_media_path) LIKE {ph}{esc} \
               AND {pos}(substr(COALESCE(t.file_path, t.cue_media_path), {depart_enfant}), {ph2}) = 0 \
               ORDER BY CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER), t.title",
        esc = tune_core::db::track_repo::like_escape_clause()
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[
                &dir_prefix as &dyn tune_core::db::backend::ToSqlValue,
                &sep_txt as &dyn tune_core::db::backend::ToSqlValue,
            ],
        )
        .ou_defaut_journalise();
    let tracks: Vec<Value> = rows
        .iter()
        .filter_map(|cols| {
            let file_path = cols.get(9).and_then(|v| v.as_string());
            let is_direct = cols
                .get(16)
                .and_then(|v| v.as_string())
                .map(|chemin| est_enfant_direct(&chemin, &repertoire_nfc))
                .unwrap_or(false);
            if !is_direct {
                return None;
            }
            Some(json!({
                "id": cols.first().and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "album_id": cols.get(2).and_then(|v| v.as_i64()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "artist_id": cols.get(4).and_then(|v| v.as_i64()),
                "artist_name": cols.get(5).and_then(|v| v.as_string()),
                "disc_number": cols.get(6).and_then(|v| v.as_i64()),
                "track_number": cols.get(7).and_then(|v| v.as_i64()),
                "duration_ms": cols.get(8).and_then(|v| v.as_i64()),
                "file_path": file_path,
                "format": cols.get(10).and_then(|v| v.as_string()),
                "sample_rate": cols.get(11).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(12).and_then(|v| v.as_i64()),
                "genre": cols.get(13).and_then(|v| v.as_string()),
                "year": cols.get(14).and_then(|v| v.as_i64()),
                "cover_path": cols.get(15).and_then(|v| v.as_string()),
            }))
        })
        .collect();

    // Parent path
    let parent = if q.path != music_root {
        resolved.parent().map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    Ok(json!({
        "path": q.path,
        "parent": parent,
        "music_root": music_root,
        "directories": subdirs,
        "tracks": tracks,
        // `accessible: false` distingue « injoignable » de « vide » : sans lui
        // le client ne peut pas faire la difference et affiche le mauvais
        // message (#1190). `access_error` porte la raison systeme.
        //
        // Cette raison vient du noyau — « Le peripherique n'est pas pret » — et
        // n'indique a personne quoi faire. `access_hint` porte la reparation
        // quand elle est connue : sous Windows, une lettre de lecteur reseau
        // n'appartient qu'a la session qui l'a creee, et il faut lui substituer
        // le chemin UNC.
        "accessible": unreadable.is_none(),
        "access_error": unreadable,
        "access_hint": access_hint,
    }))
}

pub(super) async fn browse_folders(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<FolderQuery>,
) -> axum::response::Response {
    // /library/folders?path=... is an alias for browse_directory
    // Without a path param, return browse roots
    match q.path {
        Some(ref p) if !p.is_empty() => browse_directory(
            headers,
            State(state),
            Query(BrowseQuery { path: p.clone() }),
        )
        .await
        .into_response(),
        _ => {
            let roots_json = browse_roots(headers, State(state)).await;
            roots_json.into_response()
        }
    }
}

#[cfg(test)]
mod browse_path_tests {
    use super::resolve_browse_path;
    use unicode_normalization::UnicodeNormalization;

    /// Le cas Yves : un dossier accentué créé côté NAS doit être atteignable
    /// que le client renvoie la forme composée ou décomposée.
    #[test]
    fn an_accented_directory_resolves_from_either_normalization_form() {
        let tmp = tune_core::test_scratch::scratch_dir("tune-browse");
        let nfd_name: String = "CDThèque Yves".nfd().collect();
        let dir = tmp.join(&nfd_name);
        std::fs::create_dir_all(&dir).expect("création du dossier de test");

        let on_disk = dir.to_string_lossy().to_string();
        let nfc_form: String = on_disk.nfc().collect();
        let nfd_form: String = on_disk.nfd().collect();

        for form in [&nfc_form, &nfd_form] {
            assert!(
                resolve_browse_path(form).is_some(),
                "forme non résolue : {form:?}"
            );
        }
    }

    /// La suite du cas Yves, laissée nue par #1329 : le dossier s'ouvrait, mais
    /// sa liste de pistes restait vide.
    ///
    /// Le disque ne porte que la forme **décomposée** ; le scanner, lui, a
    /// écrit ses chemins en forme **composée**. `resolve_browse_path` rend donc
    /// du NFD — la seule forme ouvrable — et tout ce qui est ensuite confronté à
    /// la base doit être replié, sinon aucune ligne ne correspond.
    ///
    /// Les deux écritures sont construites ici, jamais tapées : un test qui
    /// n'en porterait qu'une laisserait l'autre nue.
    #[test]
    fn les_pistes_d_un_dossier_decompose_sont_cherchees_en_forme_composee() {
        let tmp = tune_core::test_scratch::scratch_dir("tune-browse-nfd-pistes");
        // Chostakovitch dirigé par Bernstein : accent porté par le DOSSIER.
        let nfd_nom: String = "Chostakovitch dirigé par Bernstein".nfd().collect();
        let nfc_nom: String = "Chostakovitch dirigé par Bernstein".nfc().collect();
        assert_ne!(
            nfd_nom, nfc_nom,
            "les deux écritures doivent différer octet à octet"
        );
        let dossier = tmp.join(&nfd_nom);
        std::fs::create_dir_all(&dossier).expect("création du dossier de test");

        // Ce que le client renvoie : la forme composée que Tune lui a servie.
        let demande: String = dossier.to_string_lossy().nfc().collect();
        let resolu = resolve_browse_path(&demande).expect("le dossier doit être atteignable");

        // Ce que le scanner a écrit en base pour la piste de ce dossier.
        let repertoire_nfc: String = resolu.nfc().collect();
        let en_base = format!(
            "{}{}01. Symphonie no 5.flac",
            repertoire_nfc,
            std::path::MAIN_SEPARATOR
        );

        // Le motif est construit à partir de la forme RENDUE PAR LE DISQUE —
        // c'est ce que faisait le handler avant le correctif, et c'est ce que
        // `folder_like_pattern` doit rattraper de lui-même.
        let motif = tune_core::db::track_repo::folder_like_pattern(&resolu);
        assert!(
            en_base.starts_with(motif.trim_end_matches('%')),
            "le motif LIKE {motif:?} ne couvre pas le chemin stocké {en_base:?}"
        );
        assert!(
            super::est_enfant_direct(&en_base, &repertoire_nfc),
            "la piste du dossier n'est pas reconnue comme enfant direct"
        );

        // Et le sens inverse : une ligne restée décomposée en base doit être
        // reconnue elle aussi.
        let en_base_nfd: String = en_base.nfd().collect();
        assert!(
            super::est_enfant_direct(&en_base_nfd, &repertoire_nfc),
            "une ligne décomposée en base doit être reconnue"
        );
    }

    /// Le motif `LIKE` est confronté à `tracks.file_path`, que le scanner écrit
    /// en NFC : il doit sortir en NFC quelle que soit la forme reçue.
    #[test]
    fn le_motif_like_sort_toujours_en_forme_composee() {
        let nfd: String = "/musique/Chostakovitch dirigé/".nfd().collect();
        let nfc: String = "/musique/Chostakovitch dirigé".nfc().collect();
        let attendu = format!("{nfc}{}%", std::path::MAIN_SEPARATOR);
        assert_eq!(
            tune_core::db::track_repo::folder_like_pattern(&nfd),
            attendu
        );
        assert_eq!(
            tune_core::db::track_repo::folder_like_pattern(&nfc),
            attendu
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused() {
        assert!(resolve_browse_path("/chemin/qui/nexiste/pas/du/tout").is_none());
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(resolve_browse_path("Musique").is_none());
    }
}

/// #5677 — l'écran Répertoires ne doit pas immobiliser l'exécuteur, même sur
/// un disque lent (partage SMB/NFS qui tarde ou décroche).
///
/// Le banc, sur le modèle de celui de #5438 : un exécuteur Tokio à
/// [`FILS_EXECUTEUR`] fils, une base SQLite FICHIER, et deux fois plus de
/// requêtes simultanées que de fils, chacune sur un dossier dont la lecture
/// prend [`RETARD_DISQUE`] (retard injecté par [`DOSSIERS_LENTS`] juste avant
/// l'accès disque). Pendant ce temps, une sonde confie toutes les 5 ms une
/// tâche vide à l'exécuteur depuis un fil système et mesure combien elle
/// attend qu'un fil la prenne : tant qu'un fil est libre, elle part aussitôt.
#[cfg(test)]
mod hors_executeur_5677 {
    use super::*;
    use std::time::{Duration, Instant};

    const FILS_EXECUTEUR: usize = 2;
    /// Ce qu'un `read_dir` sur un partage réseau qui se réveille peut coûter.
    const RETARD_DISQUE: Duration = Duration::from_millis(600);
    /// Même seuil que la sonde de l'exécuteur de #5438.
    const SEUIL_EXECUTEUR: Duration = Duration::from_millis(150);

    struct Banc {
        rt: tokio::runtime::Runtime,
        state: AppState,
        racine: std::path::PathBuf,
        lent: std::path::PathBuf,
        _dossier: tempfile::TempDir,
    }

    fn banc() -> Banc {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(FILS_EXECUTEUR)
            .enable_all()
            .build()
            .unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let racine = dossier.path().join("musique");
        let lent = racine.join("Lent");
        std::fs::create_dir_all(lent.join("CD1")).unwrap();
        let base = dossier.path().join("tune.db");
        let state = rt.block_on(async {
            AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
        });
        let racine_txt = racine.to_str().unwrap().to_string();
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&[&racine_txt]).unwrap(),
            )
            .unwrap();
        let piste = |id: i64, chemin: &std::path::Path| {
            format!(
                "INSERT INTO tracks (id, title, file_path, format, source, track_number) \
                 VALUES ({id}, 'Piste {id}', '{}', 'flac', 'local', {id});",
                chemin.to_str().unwrap()
            )
        };
        state
            .backend
            .execute_batch(&format!(
                "{}{}",
                piste(1, &lent.join("01.flac")),
                piste(2, &lent.join("CD1").join("01.flac")),
            ))
            .unwrap();
        Banc {
            rt,
            state,
            racine,
            lent,
            _dossier: dossier,
        }
    }

    fn ralentir(dossier: &std::path::Path) {
        let resolu = resolve_browse_path(dossier.to_str().unwrap()).unwrap();
        DOSSIERS_LENTS
            .lock()
            .unwrap()
            .push((std::path::PathBuf::from(resolu), RETARD_DISQUE));
    }

    /// Lance `requetes` routes ensemble sur l'exécuteur du banc et rend leurs
    /// corps, avec la plus longue attente de la sonde pendant qu'elles tournaient.
    fn sous_la_sonde<F, Fut>(b: &Banc, requetes: usize, route: F) -> (Vec<Value>, Duration)
    where
        F: Fn(AppState) -> Fut,
        Fut: std::future::Future<Output = axum::response::Response> + Send + 'static,
    {
        let arret = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sonde = std::thread::spawn({
            let (arret, executeur) = (arret.clone(), b.rt.handle().clone());
            move || {
                let mut pire = Duration::ZERO;
                while !arret.load(std::sync::atomic::Ordering::Relaxed) {
                    let (fait, recu) = std::sync::mpsc::channel();
                    let t0 = Instant::now();
                    executeur.spawn(async move {
                        let _ = fait.send(());
                    });
                    recu.recv().unwrap();
                    pire = pire.max(t0.elapsed());
                    std::thread::sleep(Duration::from_millis(5));
                }
                pire
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        let taches: Vec<_> = (0..requetes)
            .map(|_| b.rt.spawn(route(b.state.clone())))
            .collect();
        let t0 = Instant::now();
        let corps: Vec<Value> = b.rt.block_on(async {
            let mut corps = Vec::new();
            for t in taches {
                let r = t.await.unwrap();
                assert_eq!(r.status(), axum::http::StatusCode::OK);
                let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
                    .await
                    .unwrap();
                corps.push(serde_json::from_slice(&octets).unwrap());
            }
            corps
        });
        let duree = t0.elapsed();
        arret.store(true, std::sync::atomic::Ordering::Relaxed);
        let pire = sonde.join().unwrap();
        eprintln!(
            "#5677 : {requetes} requêtes sur disque lent ({} ms chacune) servies en {} ms ; \
             pire attente d'un fil de l'exécuteur : {} ms ({FILS_EXECUTEUR} fils)",
            RETARD_DISQUE.as_millis(),
            duree.as_millis(),
            pire.as_millis()
        );
        assert!(
            duree >= RETARD_DISQUE,
            "le retard du disque n'a pas été injecté : {duree:?}"
        );
        (corps, pire)
    }

    #[test]
    fn un_repertoire_lent_ne_bloque_pas_l_executeur() {
        let b = banc();
        ralentir(&b.lent);
        let chemin = b.lent.to_str().unwrap().to_string();
        let (corps, pire) = sous_la_sonde(&b, 2 * FILS_EXECUTEUR, |state| {
            let chemin = chemin.clone();
            async move {
                browse_directory(
                    axum::http::HeaderMap::new(),
                    State(state),
                    Query(BrowseQuery { path: chemin }),
                )
                .await
                .into_response()
            }
        });
        // La réponse ne change pas : le sous-dossier et sa piste comptée, la
        // piste directe, et elle seule.
        for c in &corps {
            assert_eq!(c["accessible"], true, "{c}");
            assert_eq!(c["directories"][0]["name"], "CD1", "{c}");
            assert_eq!(c["directories"][0]["track_count"], 1, "{c}");
            let pistes = c["tracks"].as_array().unwrap();
            assert_eq!(pistes.len(), 1, "{c}");
            assert_eq!(pistes[0]["id"], 1, "{c}");
        }
        assert!(
            pire < SEUIL_EXECUTEUR,
            "une tâche a attendu {} ms un fil de l'exécuteur pendant la lecture d'un \
             répertoire lent : `browse_directory` tient les fils de l'exécuteur (#5677)",
            pire.as_millis()
        );
    }

    #[test]
    fn une_racine_lente_ne_bloque_pas_l_executeur() {
        let b = banc();
        ralentir(&b.racine);
        let (corps, pire) = sous_la_sonde(&b, 2 * FILS_EXECUTEUR, |state| async move {
            browse_roots(axum::http::HeaderMap::new(), State(state))
                .await
                .into_response()
        });
        for c in &corps {
            let racines = c["roots"].as_array().unwrap();
            assert_eq!(racines.len(), 1, "{c}");
            assert_eq!(racines[0]["track_count"], 2, "{c}");
            assert_eq!(racines[0]["exists"], true, "{c}");
        }
        assert!(
            pire < SEUIL_EXECUTEUR,
            "une tâche a attendu {} ms un fil de l'exécuteur pendant la lecture des \
             racines : `browse_roots` tient les fils de l'exécuteur (#5677)",
            pire.as_millis()
        );
    }
}
