use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_http_types::panne_sql::OuDefautJournalise;
use unicode_normalization::UnicodeNormalization;

use super::facets::hors_executeur;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct BrowseQuery {
    path: String,
    /// #9116 — le client sait recevoir des comptes DIFFÉRÉS (voir
    /// [`ModeDesComptes`]). Absent : l'ancien contrat, comptes attendus.
    #[serde(default)]
    comptes_differes: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct FolderQuery {
    path: Option<String>,
    #[serde(default)]
    comptes_differes: Option<String>,
}

#[derive(Deserialize, Default)]
pub(super) struct RacinesQuery {
    #[serde(default)]
    comptes_differes: Option<String>,
}

/// `?comptes_differes=1` (ou `true`) : le client annonce qu'il sait afficher
/// une liste sans comptes et les redemander.
fn veut_des_comptes_differes(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true"))
}

pub(super) async fn browse_roots(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<RacinesQuery>,
) -> Result<Json<Value>, AppError> {
    let lang = crate::i18n::lang_from_header(&headers);
    let differes = veut_des_comptes_differes(&q.comptes_differes);
    // #5677 — un `COUNT(*) … LIKE` par racine (le `LIKE` ne s'appuie sur aucun
    // index : toute la table à chaque fois) et un `is_dir()` par racine, qui
    // peut rester suspendu des secondes sur un partage réseau décroché. Hors de
    // l'exécuteur, comme les routes de #5438.
    hors_executeur("browse_roots", move || {
        lire_les_racines_avec(&state, &lang, differes)
    })
    .await
    .map(Json)
}

/// Le corps de `GET /library/browse` (ancien contrat), exécuté HORS de
/// l'exécuteur (#5677).
#[cfg(test)]
fn lire_les_racines(state: &AppState, lang: &str) -> Value {
    lire_les_racines_avec(state, lang, false)
}

/// Le corps de `GET /library/browse` ; `differes` : voir [`ModeDesComptes`].
fn lire_les_racines_avec(state: &AppState, lang: &str, differes: bool) -> Value {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let comptes = comptes_des_dossiers(state, differes);
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
            // #9116 — lu dans l'arbre des dossiers, ou pas encore connu : la
            // racine s'affiche quand même, son compte arrive ensuite.
            let count: Option<i64> = comptes.racine(state, &norm);
            if count == Some(0) {
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
            // `track_count` reste un entier (les remotes le décodent comme
            // tel) : 0 tant que `comptes_en_attente` est vrai.
            json!({
                "path": norm, "name": name, "track_count": count.unwrap_or(0),
                "exists": exists, "hint": hint,
            })
        })
        .collect();

    // Fallback: if no configured music_dir matches any stored path (the
    // browse_root_zero_tracks drift — e.g. .18 set to /mnt/music while files
    // live under /data/music), the Répertoires view would show only empty roots
    // and browsing would go nowhere. Surface the real root inferred from the
    // data so it still works — the same fallback the Oxygen folder facet uses.
    //
    // Tant que les comptes ne sont pas connus, rien ne dit qu'aucune racine
    // n'est peuplée : le repli attend qu'ils le soient.
    let none_populated = roots
        .iter()
        .all(|r| r.get("track_count").and_then(|v| v.as_i64()).unwrap_or(0) == 0);
    if !comptes.en_attente()
        && none_populated
        && let Some(base) = tune_core::db::track_repo::derive_common_root(state.backend.as_ref())
    {
        let count: i64 = comptes.racine(state, &base).unwrap_or(0);
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

    // #9116 — `comptes_en_attente` : pas encore connus (l'arbre des dossiers
    // se construit), le client les redemande ; `comptes_approximatifs` : ceux
    // d'un état passé de la bibliothèque (un scan tourne).
    json!({
        "roots": roots,
        "comptes_en_attente": comptes.en_attente(),
        "comptes_approximatifs": comptes.approximatifs(),
    })
}

/// #9116 (fil 2935) — d'où viennent les comptes de la vue Répertoires.
///
/// L'arbre des dossiers ([`tune_core::db::arbre_des_dossiers`]) est
/// construit UNE fois par état des chemins de `tracks` : il rend les comptes
/// de chaque dossier et ses pistes directes sans relire la base.
///
/// Thierry C. (fil 2935, 11781) : « à quoi te sert de compter le nombre de
/// fichiers présents dans chaque arborescence quand c'est moi qui prendrai la
/// décision d'ouvrir tel ou tel disque ? ». Un client qui l'annonce
/// (`?comptes_differes=1`, le client web) n'attend donc JAMAIS un comptage :
/// sans arbre, la liste part sans comptes et il les redemande. Les autres
/// (applications iOS, qui affichent « N tracks ») gardent l'ancien contrat :
/// ils attendent le PREMIER arbre (une fois par démarrage), jamais les
/// reconstructions — pendant un scan, tous reçoivent le dernier arbre, dit
/// approché.
enum ModeDesComptes {
    Arbre {
        arbre: std::sync::Arc<tune_core::db::arbre_des_dossiers::ArbreDesDossiers>,
        exact: bool,
    },
    /// Client à comptes différés, premier arbre pas encore prêt.
    EnAttente,
    /// Aucune révision lisible : comptes en SQL, comme avant l'arbre.
    Sql,
}

impl ModeDesComptes {
    fn en_attente(&self) -> bool {
        matches!(self, Self::EnAttente)
    }

    fn approximatifs(&self) -> bool {
        matches!(self, Self::Arbre { exact: false, .. })
    }

    /// Le compte d'une racine (`file_path` seul), ou `None` en attente.
    fn racine(&self, state: &AppState, racine: &str) -> Option<i64> {
        match self {
            Self::Arbre { arbre, .. } => Some(arbre.fichiers_sous(racine)),
            Self::EnAttente => None,
            Self::Sql => Some(
                tune_core::db::track_repo::compter_pistes_sous_dossier(
                    state.backend.as_ref(),
                    racine,
                )
                .unwrap_or_else(|e| {
                    warn!(path = %racine, error = %e, "browse_root_count_failed");
                    0
                }),
            ),
        }
    }
}

fn comptes_des_dossiers(state: &AppState, differes: bool) -> ModeDesComptes {
    use tune_core::db::arbre_des_dossiers::{Attente, Lecture, lire_sans_attendre};
    let attente = if differes {
        Attente::AuPlus(ATTENTE_DES_COMPTES)
    } else {
        Attente::JusquAuPremier
    };
    match lire_sans_attendre(&state.backend, attente) {
        Lecture::Exact(arbre) => ModeDesComptes::Arbre { arbre, exact: true },
        Lecture::Approche(arbre) => ModeDesComptes::Arbre {
            arbre,
            exact: false,
        },
        Lecture::EnAttente if differes => ModeDesComptes::EnAttente,
        // Ancien contrat et construction en échec : comptes en SQL.
        Lecture::EnAttente | Lecture::SansRevision => ModeDesComptes::Sql,
    }
}

/// Ce que la vue Répertoires accepte d'attendre le PREMIER arbre des
/// dossiers, pour un client à comptes différés, au premier appel seulement.
const ATTENTE_DES_COMPTES: std::time::Duration = std::time::Duration::from_millis(250);

/// Sans arbre exact, un dossier dont le sous-arbre compte au plus autant de
/// pistes (un album, un coffret) lit ses pistes directes par la plage
/// récursive, exacte et bon marché ; au-delà (un disque entier), jamais.
const SOUS_ARBRE_BON_MARCHE: usize = 2000;

/// Au-delà, les pistes directes d'un dossier se lisent par la plage d'avant
/// plutôt que par une liste d'identifiants (bornée par les marques de
/// paramètres des deux moteurs). Un dossier « à plat » de plus de 900 pistes
/// est rare ; il paie alors ce qu'il payait.
const PISTES_DIRECTES_PAR_IDS_MAX: usize = 900;

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
/// Sécurité — l'unique message de refus de `GET
/// /library/browse/dir` : chemin vide, relatif, porteur de `..`, hors des
/// dossiers de musique ou absent du disque, la réponse est la même.
pub(crate) const REFUS_DE_PARCOURS: &str = "path not under a configured music directory";

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

/// Une entrée de `read_dir` est-elle un dossier ? Même réponse que
/// `entry.path().is_dir()`, sans son aller-retour réseau.
///
/// #5998 — `is_dir()` refait un `stat` par entrée, et sur un partage SMB ou
/// NFS chaque `stat` est un aller-retour vers le NAS. `DirEntry::file_type`
/// vient de la lecture du dossier elle-même (`d_type` sous Linux, données de
/// `FindNextFile` sous Windows), sans appel de plus. Un lien symbolique, lui,
/// doit être SUIVI comme le faisait `is_dir()` : il y retombe, comme une entrée
/// dont le type n'a pas pu être lu.
fn est_un_dossier(entry: &std::fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) if t.is_symlink() => entry.path().is_dir(),
        Ok(t) => t.is_dir(),
        Err(_) => entry.path().is_dir(),
    }
}

/// Le corps de `GET /library/browse/dir`, exécuté HORS de l'exécuteur (#5677).
/// `q.comptes_differes` : voir [`ModeDesComptes`].
fn lire_le_repertoire(state: &AppState, lang: &str, q: BrowseQuery) -> Result<Value, AppError> {
    let debut = std::time::Instant::now();
    // Sécurité — l'appartenance à un dossier de musique est
    // tranchée AVANT tout accès au disque, sur le texte demandé (rogné et
    // replié en NFC), et chaque refus porte le MÊME message : sans quoi
    // « n'existe pas » et « existe hors de la bibliothèque » se distinguent,
    // et la route devient un oracle d'existence pour toute la machine.
    let refus = || AppError::bad_request(REFUS_DE_PARCOURS);
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    // Comparaison sur une forme Unicode commune : le chemin demandé peut être
    // en NFD (ce qu'expose le partage SMB) alors que le dossier musical
    // configuré est en NFC.
    let racines_nfc: Vec<String> = dirs
        .iter()
        .map(|d| {
            tune_core::scanner::walker::normalize_path(d)
                .nfc()
                .collect()
        })
        .collect();
    let racine_de = |chemin: &str| -> Option<usize> {
        let nfc: String = chemin.nfc().collect();
        tune_core::chemin_sur::racine_contenante(&nfc, &racines_nfc)
            .ok()
            .and_then(|r| racines_nfc.iter().position(|x| x == r))
    };
    let demande = tune_core::scanner::walker::normalize_path(&q.path);
    if racine_de(&demande).is_none() {
        return Err(refus());
    }
    // Seulement maintenant, le disque : la forme (NFC, NFD ou brute) qui
    // existe réellement. Elle ne diffère de la demande que par sa forme
    // Unicode ; l'appartenance est revérifiée sur elle, au même message.
    let normalized_query = resolve_browse_path(&demande).ok_or_else(refus)?;
    let resolved = std::path::Path::new(&normalized_query);
    let Some(i) = racine_de(&normalized_query) else {
        return Err(refus());
    };
    let music_root = &dirs[i];
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
    // #9116 — l'arbre des dossiers : les comptes et les pistes directes de ce
    // dossier, sans relire tout ce qui est sous lui (voir `ModeDesComptes`).
    let t_arbre = std::time::Instant::now();
    let comptes = comptes_des_dossiers(state, veut_des_comptes_differes(&q.comptes_differes));
    let vue = match &comptes {
        ModeDesComptes::Arbre { arbre, .. } => Some(arbre.dossier(&normalized_query)),
        _ => None,
    };
    let ms_arbre = t_arbre.elapsed().as_millis() as u64;
    #[cfg(test)]
    simuler_un_disque_lent(resolved);
    let t_disque = std::time::Instant::now();
    // Le disque ne sert qu'à LISTER les sous-dossiers (un dossier pas encore
    // scanné doit apparaître) ; leurs comptes viennent de la base.
    let lus = std::fs::read_dir(resolved).map(|entries| {
        entries
            .flatten()
            .filter(est_un_dossier)
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    });
    let ms_disque = t_disque.elapsed().as_millis() as u64;
    match lus {
        Err(e) => {
            warn!(path = %resolved.display(), error = %e, "browse_dir_unreadable");
            unreadable = Some(e.to_string());
        }
        Ok(dossiers) => {
            // #9116 — les comptes viennent de l'arbre, sans aucune requête.
            // En attente (client à comptes différés), AUCUN comptage ne
            // retient la liste : `track_count` vaut 0 et `comptes_en_attente`
            // le dit. Sans révision lisible : la requête groupée de #3857.
            let comptes_sql = matches!(comptes, ModeDesComptes::Sql).then(|| {
                tune_core::db::track_repo::compter_pistes_par_sous_dossier(
                    state.backend.as_ref(),
                    &normalized_query,
                )
                .unwrap_or_else(|e| {
                    warn!(path = %normalized_query, error = %e, "browse_dir_count_failed");
                    tune_core::db::track_repo::ComptesParSousDossier::default()
                })
            });
            for path in dossiers {
                let dir_path: String = path.to_string_lossy().nfc().collect();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.starts_with('.') {
                    continue;
                }
                let track_count: i64 = match (&vue, &comptes_sql) {
                    (Some(v), _) => v.pistes_du_sous_dossier(&name),
                    (None, Some(c)) => c.get(&name),
                    (None, None) => 0,
                };
                subdirs.push(json!({ "name": name, "path": dir_path, "track_count": track_count }));
            }
        }
    }
    // Le conseil se calcule sur le chemin TEL QUE CONFIGURE (`q.path`), pas sur
    // le chemin resolu : c'est celui que l'utilisateur a saisi, donc celui qui
    // porte encore la lettre de lecteur qu'il faut lui apprendre a remplacer.
    let ms_sous_dossiers = debut.elapsed().as_millis() as u64;
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
    let (bas, haut) = tune_core::db::track_repo::bornes_du_dossier(&repertoire_nfc);
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
    //
    // #9116 — avec l'arbre EXACT, les pistes directes sont connues par
    // identifiant : aucune lecture pour un dossier qui n'en a pas (tous les
    // niveaux intermédiaires), une lecture par clé primaire sinon.
    //
    // Sans arbre exact, la plage récursive ne part que si le sous-arbre est
    // petit (un album : exacte et bon marché, vérifié en lisant au plus
    // `SOUS_ARBRE_BON_MARCHE` entrées d'index). Pour un disque entier : les
    // identifiants de l'arbre approché, ou, sans arbre du tout, rien encore
    // (`pistes_en_attente`, le client redemande).
    let t_pistes = std::time::Instant::now();
    let par_ids = |ids: &[i64]| {
        let sql = tune_core::db::track_repo::sql_pistes_par_ids(state.backend.engine(), ids.len());
        let params: Vec<&dyn tune_core::db::backend::ToSqlValue> = ids
            .iter()
            .map(|id| id as &dyn tune_core::db::backend::ToSqlValue)
            .collect();
        state
            .backend
            .query_many(&sql, &params)
            .ou_defaut_journalise()
    };
    let par_la_plage = || {
        let sql =
            tune_core::db::track_repo::sql_pistes_du_dossier(state.backend.engine(), depart_enfant);
        state
            .backend
            .query_many(
                &sql,
                &[
                    &dir_prefix as &dyn tune_core::db::backend::ToSqlValue,
                    &sep_txt as &dyn tune_core::db::backend::ToSqlValue,
                    &bas as &dyn tune_core::db::backend::ToSqlValue,
                    &haut as &dyn tune_core::db::backend::ToSqlValue,
                ],
            )
            .ou_defaut_journalise()
    };
    let sous_arbre_bon_marche = || {
        let sql = tune_core::db::track_repo::sql_compter_sous_dossier_plafonne(
            state.backend.engine(),
            SOUS_ARBRE_BON_MARCHE + 1,
        );
        state
            .backend
            .query_one(
                &sql,
                &[
                    &bas as &dyn tune_core::db::backend::ToSqlValue,
                    &haut as &dyn tune_core::db::backend::ToSqlValue,
                ],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .is_some_and(|n| n <= SOUS_ARBRE_BON_MARCHE as i64)
    };
    let mut pistes_en_attente = false;
    let rows = match (&comptes, vue.as_ref().map(|v| v.pistes_directes())) {
        (ModeDesComptes::Arbre { exact: true, .. }, Some([])) => Vec::new(),
        (ModeDesComptes::Arbre { exact: true, .. }, Some(ids))
            if ids.len() <= PISTES_DIRECTES_PAR_IDS_MAX =>
        {
            par_ids(ids)
        }
        (ModeDesComptes::Arbre { exact: true, .. }, _) | (ModeDesComptes::Sql, _) => par_la_plage(),
        (_, _) if sous_arbre_bon_marche() => par_la_plage(),
        (ModeDesComptes::Arbre { .. }, Some([])) => Vec::new(),
        (ModeDesComptes::Arbre { .. }, Some(ids)) if ids.len() <= PISTES_DIRECTES_PAR_IDS_MAX => {
            par_ids(ids)
        }
        (ModeDesComptes::Arbre { .. }, _) => par_la_plage(),
        (ModeDesComptes::EnAttente, _) => {
            pistes_en_attente = true;
            Vec::new()
        }
    };
    let ms_pistes = t_pistes.elapsed().as_millis() as u64;
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

    // #5998 — l'ouverture d'un dossier n'apparaissait nulle part au journal :
    // « plusieurs minutes » chez Tades, et rien pour dire si le temps partait
    // dans le partage réseau ou dans la base. Une ligne par ouverture, avec
    // les deux moitiés séparées.
    info!(
        path = %normalized_query,
        ms_total = debut.elapsed().as_millis() as u64,
        ms_sous_dossiers,
        ms_arbre,
        ms_disque,
        ms_pistes,
        arbre = vue.is_some(),
        approche = comptes.approximatifs(),
        sous_dossiers = subdirs.len(),
        pistes = tracks.len(),
        "browse_dir_lu"
    );

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
        // #9116 — `comptes_en_attente` : comptes des sous-dossiers pas encore
        // connus (0 en attendant) ; `pistes_en_attente` : pistes directes
        // pas encore lues ; le client redemande dans les deux cas.
        // `comptes_approximatifs` : comptes d'un état passé (scan en cours).
        "comptes_en_attente": comptes.en_attente(),
        "comptes_approximatifs": comptes.approximatifs(),
        "pistes_en_attente": pistes_en_attente,
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
            Query(BrowseQuery {
                path: p.clone(),
                comptes_differes: q.comptes_differes.clone(),
            }),
        )
        .await
        .into_response(),
        _ => {
            let roots_json = browse_roots(
                headers,
                State(state),
                Query(RacinesQuery {
                    comptes_differes: q.comptes_differes.clone(),
                }),
            )
            .await;
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
        // #9116 — l'arbre des dossiers prêt d'avance : ce banc mesure
        // l'exécuteur, et ses comptes ne doivent pas dépendre d'une
        // construction en arrière-plan.
        assert!(tune_core::db::arbre_des_dossiers::lire(state.backend.as_ref()).is_some());
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
                    Query(BrowseQuery {
                        path: chemin,
                        comptes_differes: None,
                    }),
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
            browse_roots(
                axum::http::HeaderMap::new(),
                State(state),
                Query(RacinesQuery::default()),
            )
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

/// Sécurité — `GET /library/browse/dir` ne sort jamais des dossiers de musique.
#[cfg(test)]
mod garde_de_remontee_tests {
    use super::{BrowseQuery, lire_le_repertoire};
    use crate::state::AppState;

    struct Banc {
        state: AppState,
        racine: std::path::PathBuf,
        secret: std::path::PathBuf,
        _rt: tokio::runtime::Runtime,
        _dossier: tempfile::TempDir,
    }

    fn banc() -> Banc {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let racine = dossier.path().join("musique");
        std::fs::create_dir_all(racine.join("Jazz")).unwrap();
        let secret = dossier.path().join("secret");
        std::fs::create_dir_all(secret.join("NE_PAS_LISTER")).unwrap();
        let base = dossier.path().join("tune.db");
        let state = rt.block_on(async {
            AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
        });
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&[racine.to_str().unwrap()]).unwrap(),
            )
            .unwrap();
        Banc {
            state,
            racine,
            secret,
            _rt: rt,
            _dossier: dossier,
        }
    }

    fn lire(b: &Banc, chemin: &str) -> Result<serde_json::Value, crate::error::AppError> {
        lire_le_repertoire(
            &b.state,
            "fr",
            BrowseQuery {
                path: chemin.to_string(),
                comptes_differes: None,
            },
        )
    }

    #[test]
    fn un_dossier_de_la_bibliotheque_se_lit() {
        let b = banc();
        let Ok(v) = lire(&b, b.racine.to_str().unwrap()) else {
            panic!("la racine de la bibliothèque doit se lire");
        };
        assert!(v.to_string().contains("Jazz"), "{v}");
    }

    #[test]
    fn une_remontee_hors_de_la_bibliotheque_est_refusee() {
        let b = banc();
        let r = b.racine.to_str().unwrap();
        assert!(b.secret.is_dir());
        for chemin in [
            format!("{r}/../secret"),
            format!("{r}/Jazz/../../secret"),
            format!("{r}/./../secret/"),
            format!("{r}\\..\\secret"),
        ] {
            let lu = lire(&b, &chemin);
            assert!(
                lu.is_err(),
                "{chemin} est hors de la bibliothèque et a été listé : {:?}",
                lu.ok()
            );
        }
        // Encodée : l'extracteur de requête la décode avant la route.
        let enc = urlencoding::encode(&format!("{r}/%2e%2e/secret")).into_owned();
        let decode = urlencoding::decode(&urlencoding::decode(&enc).unwrap())
            .unwrap()
            .into_owned();
        assert!(lire(&b, &decode).is_err(), "{decode}");
    }
}

/// #9116 (fil 2935) — les comptes et les pistes de la vue Répertoires, lus
/// dans l'arbre des dossiers : jamais périmés après une écriture.
#[cfg(test)]
mod comptes_9116_tests {
    use super::{BrowseQuery, lire_le_repertoire, lire_les_racines, lire_les_racines_avec};
    use crate::state::AppState;
    use unicode_normalization::UnicodeNormalization;

    /// L'arbre de l'état courant, construit ici même : la route ne l'attend
    /// que 250 ms, et ces témoins lisent des comptes.
    fn arbre_pret(state: &AppState) {
        assert!(tune_core::db::arbre_des_dossiers::lire(state.backend.as_ref()).is_some());
    }

    fn poser(state: &AppState, chemin: &std::path::Path) -> i64 {
        let chemin: String = chemin.to_str().unwrap().nfc().collect();
        state
            .backend
            .execute_returning_id(
                "INSERT INTO tracks (title, file_path, format, source) VALUES ('t', ?, 'flac', 'local')",
                &[&chemin as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap()
    }

    fn compte_racine(state: &AppState) -> i64 {
        arbre_pret(state);
        lire_les_racines(state, "fr")["roots"][0]["track_count"]
            .as_i64()
            .unwrap()
    }

    fn lire(state: &AppState, dossier: &std::path::Path) -> serde_json::Value {
        arbre_pret(state);
        lire_le_repertoire(
            state,
            "fr",
            BrowseQuery {
                path: dossier.to_str().unwrap().to_string(),
                comptes_differes: None,
            },
        )
        .ok()
        .unwrap()
    }

    fn compte_de(state: &AppState, parent: &std::path::Path, nom: &str) -> i64 {
        let nom: String = nom.nfc().collect();
        lire(state, parent)["directories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"].as_str().map(|n| n.nfc().collect::<String>()) == Some(nom.clone()))
            .and_then(|d| d["track_count"].as_i64())
            .unwrap()
    }

    fn pistes_de(state: &AppState, dossier: &std::path::Path) -> Vec<i64> {
        lire(state, dossier)["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect()
    }

    struct Banc {
        state: AppState,
        racine: std::path::PathBuf,
        _rt: tokio::runtime::Runtime,
        _dossier: tempfile::TempDir,
    }

    fn banc() -> Banc {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let racine = dossier.path().join("musique");
        std::fs::create_dir_all(racine.join("Jazz").join("Miles")).unwrap();
        let base = dossier.path().join("tune.db");
        let state = rt.block_on(async {
            AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
        });
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&[racine.to_str().unwrap()]).unwrap(),
            )
            .unwrap();
        assert!(
            tune_core::db::arbre_des_dossiers::lire_revision_des_chemins(state.backend.as_ref())
                .is_some(),
            "la révision des chemins doit exister, sans quoi ce témoin ne garde rien"
        );
        Banc {
            state,
            racine,
            _rt: rt,
            _dossier: dossier,
        }
    }

    /// Ajout, suppression, déplacement : chaque écriture se voit aussitôt,
    /// dans les comptes (racine, sous-dossier) et dans les pistes directes.
    #[test]
    fn ajout_suppression_et_deplacement_se_voient_aussitot() {
        let b = banc();
        let (s, r) = (&b.state, &b.racine);
        let jazz = r.join("Jazz");
        let a = poser(s, &jazz.join("01.flac"));
        poser(s, &jazz.join("Miles").join("01.flac"));
        assert_eq!(compte_racine(s), 2);
        assert_eq!(compte_de(s, r, "Jazz"), 2);
        assert_eq!(compte_de(s, &jazz, "Miles"), 1);
        assert_eq!(pistes_de(s, &jazz), vec![a]);
        // Relu sans écriture : les mêmes chiffres (servis par l'arbre).
        assert_eq!(compte_racine(s), 2);
        assert_eq!(compte_de(s, r, "Jazz"), 2);
        // Ajout.
        let c = poser(s, &jazz.join("02.flac"));
        assert_eq!(compte_racine(s), 3);
        assert_eq!(compte_de(s, r, "Jazz"), 3);
        assert_eq!(pistes_de(s, &jazz), vec![a, c]);
        // Suppression.
        s.backend
            .execute(
                "DELETE FROM tracks WHERE id = ?",
                &[&a as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap();
        assert_eq!(compte_racine(s), 2);
        assert_eq!(compte_de(s, r, "Jazz"), 2);
        assert_eq!(pistes_de(s, &jazz), vec![c]);
        // Déplacement (« Remplacer le chemin ») : seul `file_path` change.
        let neuf = jazz.join("Miles").join("02.flac");
        let neuf = neuf.to_str().unwrap().to_string();
        s.backend
            .execute(
                "UPDATE tracks SET file_path = ? WHERE id = ?",
                &[
                    &neuf as &dyn tune_core::db::backend::ToSqlValue,
                    &c as &dyn tune_core::db::backend::ToSqlValue,
                ],
            )
            .unwrap();
        assert_eq!(compte_de(s, &jazz, "Miles"), 2);
        assert!(pistes_de(s, &jazz).is_empty());
    }

    /// `?comptes_differes=1`, comme le client web.
    fn lire_differe(state: &AppState, dossier: &std::path::Path) -> serde_json::Value {
        lire_le_repertoire(
            state,
            "fr",
            BrowseQuery {
                path: dossier.to_str().unwrap().to_string(),
                comptes_differes: Some("1".into()),
            },
        )
        .ok()
        .unwrap()
    }

    /// Thierry C. (fil 2935, 11781) : la liste des dossiers n'attend AUCUN
    /// comptage. Arbre pas encore construit (construction retenue) et client à
    /// comptes différés : les racines et les sous-dossiers s'affichent
    /// aussitôt, comptes à 0 et `comptes_en_attente` vrai ; un dossier dont le
    /// sous-arbre est petit a quand même ses pistes ; une fois l'arbre prêt,
    /// les vrais comptes.
    #[test]
    fn la_liste_des_dossiers_n_attend_pas_les_comptes() {
        let b = banc();
        let (s, r) = (&b.state, &b.racine);
        let miles = r.join("Jazz").join("Miles");
        let piste = poser(s, &miles.join("01.flac"));
        // Une grande bibliothèque : l'arbre ne sera pas prêt pendant ces
        // lectures.
        let retenue = tune_core::db::arbre_des_dossiers::retenir_les_constructions();
        let t0 = std::time::Instant::now();
        let racines = lire_les_racines_avec(s, "fr", true);
        let dossier = lire_differe(s, r);
        let album = lire_differe(s, &miles);
        let ms = t0.elapsed().as_millis();
        assert_eq!(racines["comptes_en_attente"], true, "{racines}");
        assert_eq!(racines["roots"].as_array().unwrap().len(), 1, "{racines}");
        assert_eq!(
            dossier["comptes_en_attente"], true,
            "la liste a attendu l'arbre des dossiers : {dossier}"
        );
        assert_eq!(dossier["directories"][0]["name"], "Jazz", "{dossier}");
        assert_eq!(dossier["directories"][0]["track_count"], 0, "{dossier}");
        // Sous-arbre d'une piste : lu par la plage, exact, sans attendre.
        assert_eq!(album["pistes_en_attente"], false, "{album}");
        assert_eq!(album["tracks"][0]["id"], piste, "{album}");
        eprintln!("#9116 : racines + 2 dossiers sans arbre servis en {ms} ms");
        assert!(
            ms < 1000,
            "la liste a attendu l'arbre des dossiers : {ms} ms pour trois lectures sans comptes"
        );
        drop(retenue);
        // L'arbre prêt (construit en arrière-plan, ou ici) : les vrais comptes.
        assert_eq!(compte_racine(s), 1);
        assert_eq!(compte_de(s, r, "Jazz"), 1);
        arbre_pret(s);
        assert_eq!(lire_differe(s, r)["comptes_en_attente"], false);
    }

    /// Un client qui n'annonce PAS les comptes différés (applications iOS :
    /// « N tracks ») garde l'ancien contrat : jamais de 0 d'attente, il attend
    /// le premier arbre.
    #[test]
    fn sans_comptes_differes_l_ancien_contrat_attend_les_comptes() {
        let b = banc();
        let (s, r) = (&b.state, &b.racine);
        poser(s, &r.join("Jazz").join("01.flac"));
        let racines = lire_les_racines(s, "fr");
        assert_eq!(racines["comptes_en_attente"], false, "{racines}");
        assert_eq!(racines["roots"][0]["track_count"], 1, "{racines}");
    }

    /// Pendant un scan, la révision bouge à chaque ajout : le dernier arbre
    /// est servi, annoncé APPROCHÉ, au lieu de plus aucun compte.
    #[test]
    fn pendant_un_scan_le_dernier_arbre_sert_de_valeur_approchee() {
        let b = banc();
        let (s, r) = (&b.state, &b.racine);
        let jazz = r.join("Jazz");
        poser(s, &jazz.join("01.flac"));
        arbre_pret(s);
        // Le scan ajoute une piste ; la construction suivante est retenue.
        let retenue = tune_core::db::arbre_des_dossiers::retenir_les_constructions();
        poser(s, &jazz.join("02.flac"));
        let racines = lire_les_racines_avec(s, "fr", true);
        assert_eq!(racines["comptes_approximatifs"], true, "{racines}");
        assert_eq!(racines["comptes_en_attente"], false, "{racines}");
        assert_eq!(
            racines["roots"][0]["track_count"], 1,
            "le compte du dernier arbre : {racines}"
        );
        let dossier = lire_differe(s, &jazz);
        // Dossier petit : ses pistes viennent de la base, exactes.
        assert_eq!(dossier["tracks"].as_array().unwrap().len(), 2, "{dossier}");
        drop(retenue);
        arbre_pret(s);
        let racines = lire_les_racines_avec(s, "fr", true);
        assert_eq!(racines["comptes_approximatifs"], false, "{racines}");
        assert_eq!(racines["roots"][0]["track_count"], 2, "{racines}");
    }

    /// La route lit l'ARBRE, pas la base : une piste ajoutée en coupant le
    /// déclencheur de la révision des chemins (rien ne prévient l'arbre)
    /// n'apparaît pas dans les comptes, alors que la base la compte. Sans
    /// arbre, chaque niveau recompterait tout ce qui est sous lui (#9116).
    #[test]
    fn la_route_lit_l_arbre_et_non_la_base() {
        let b = banc();
        let (s, r) = (&b.state, &b.racine);
        let jazz = r.join("Jazz");
        poser(s, &jazz.join("Miles").join("01.flac"));
        assert_eq!(compte_racine(s), 1);
        assert_eq!(compte_de(s, r, "Jazz"), 1);
        s.backend
            .execute_batch("DROP TRIGGER library_paths_revision_ins;")
            .unwrap();
        poser(s, &jazz.join("Miles").join("02.flac"));
        assert_eq!(
            tune_core::db::track_repo::compter_pistes_par_sous_dossier(
                s.backend.as_ref(),
                r.to_str().unwrap()
            )
            .unwrap()
            .get("Jazz"),
            2,
            "la base compte bien la seconde piste"
        );
        assert_eq!(
            compte_de(s, r, "Jazz"),
            1,
            "le compte doit venir de l'arbre gardé, sans relire la base"
        );
        assert_eq!(compte_de(s, &jazz, "Miles"), 1);
        assert_eq!(compte_racine(s), 1);
    }

    /// Une écriture qui ne touche pas les chemins (une passe de fond qui pose
    /// son genre, son ReplayGain…) ne fait pas reconstruire l'arbre.
    #[test]
    fn une_ecriture_hors_chemins_ne_bouge_pas_la_revision_des_chemins() {
        let b = banc();
        let id = poser(&b.state, &b.racine.join("Jazz").join("01.flac"));
        let lire = || {
            tune_core::db::arbre_des_dossiers::lire_revision_des_chemins(b.state.backend.as_ref())
                .unwrap()
        };
        let avant = lire();
        b.state
            .backend
            .execute(
                "UPDATE tracks SET genre = 'Jazz', title = 'x' WHERE id = ?",
                &[&id as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap();
        tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone())
            .set("une_cle", "1")
            .unwrap();
        assert_eq!(lire(), avant, "ni le genre, ni le titre, ni les réglages");
        // Un `UPDATE` qui réécrit le même chemin ne compte pas non plus.
        b.state
            .backend
            .execute(
                "UPDATE tracks SET file_path = file_path WHERE id = ?",
                &[&id as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap();
        assert_eq!(lire(), avant, "chemin réécrit à l'identique");
    }

    /// Un dossier accentué que le disque n'expose qu'en NFD : ses comptes et
    /// ses pistes (écrites en NFC par le scanner) se lisent dans l'arbre.
    #[test]
    fn un_dossier_decompose_sur_le_disque_se_lit_dans_l_arbre() {
        let b = banc();
        let nfd: String = "Chostakovitch dirigé".nfd().collect();
        let dossier = b.racine.join(&nfd);
        std::fs::create_dir_all(dossier.join("CD1")).unwrap();
        let direct = poser(&b.state, &dossier.join("01.flac"));
        poser(&b.state, &dossier.join("CD1").join("01.flac"));
        assert_eq!(compte_de(&b.state, &b.racine, &nfd), 2);
        assert_eq!(compte_de(&b.state, &dossier, "CD1"), 1);
        assert_eq!(pistes_de(&b.state, &dossier), vec![direct]);
    }
}
