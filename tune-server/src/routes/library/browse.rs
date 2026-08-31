use crate::routes::panne_sql::OuDefautJournalise;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;
use unicode_normalization::UnicodeNormalization;

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
            let esc = tune_core::db::track_repo::like_escape_clause(state.backend.engine());
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
                .then(|| crate::chemin_inaccessible::conseil(&lang, &norm))
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
    if none_populated {
        if let Some(base) = tune_core::db::track_repo::derive_common_root(state.backend.as_ref()) {
            let pattern = tune_core::db::track_repo::folder_like_pattern(&base);
            let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
                "$1"
            } else {
                "?1"
            };
            let esc = tune_core::db::track_repo::like_escape_clause(state.backend.engine());
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
    }

    Ok(Json(json!({ "roots": roots })))
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
    match std::fs::read_dir(resolved) {
        Err(e) => {
            warn!(path = %resolved.display(), error = %e, "browse_dir_unreadable");
            unreadable = Some(e.to_string());
        }
        Ok(entries) => {
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
                    let pattern = tune_core::db::track_repo::folder_like_pattern(&dir_path);
                    let track_count: i64 = match state.backend.query_one(
                        &format!(
                            "SELECT COUNT(*) FROM tracks WHERE file_path LIKE {}{}",
                            if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
                                "$1"
                            } else {
                                "?1"
                            },
                            tune_core::db::track_repo::like_escape_clause(state.backend.engine())
                        ),
                        &[&pattern as &dyn tune_core::db::backend::ToSqlValue],
                    ) {
                        Ok(Some(cols)) => cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                        Ok(None) => 0,
                        Err(e) => {
                            warn!(path = %dir_path, error = %e, "browse_dir_count_failed");
                            0
                        }
                    };
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
        .and_then(|_| crate::chemin_inaccessible::conseil(&lang, &q.path));

    subdirs.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
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
    let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?1"
    };
    let sql = format!(
        "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, \
               t.disc_number, t.track_number, t.duration_ms, t.file_path, \
               t.format, t.sample_rate, t.bit_depth, t.genre, t.year, al.cover_path \
               FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
               LEFT JOIN artists ar ON t.artist_id = ar.id \
               WHERE t.file_path LIKE {ph}{esc} \
               ORDER BY CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER), t.title",
        esc = tune_core::db::track_repo::like_escape_clause(state.backend.engine())
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[&dir_prefix as &dyn tune_core::db::backend::ToSqlValue],
        )
        .ou_defaut_journalise();
    let tracks: Vec<Value> = rows
        .iter()
        .filter_map(|cols| {
            let file_path = cols.get(9).and_then(|v| v.as_string());
            let is_direct = file_path
                .as_ref()
                .map(|fp| est_enfant_direct(fp, &repertoire_nfc))
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

    Ok(Json(json!({
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
    })))
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
