//! `POST /library/identify-all` — le PILOTE de lot de l'identification (#4805).
//!
//! # Ce qui manquait, et ce qui ne manquait pas
//!
//! Le travail était écrit, il n'était pas piloté. `POST
//! /library/albums/{id}/reidentify` fait **déjà** toute la chaîne — recherche
//! du pressage, détail, appariement pressage ↔ pistes locales, écriture du
//! `musicbrainz_release_id`, du `musicbrainz_release_group_id` et du
//! `musicbrainz_recording_id` de chaque piste. Il n'existait simplement aucune
//! passe « identifier la bibliothèque » : la route se déclenchait un album à la
//! fois, à la main.
//!
//! Ce module n'ajoute donc **aucune** logique d'identification. Il appelle
//! [`super::reidentify::identifier_album`], celle-là même que sert le bouton
//! « Ré-identifier » d'un album, et ne fait que trois choses qu'elle ne sait
//! pas faire : **choisir les albums**, **tenir la cadence**, et **rendre des
//! comptes**.
//!
//! # Le constat chiffré qui justifie la forme (issue #4805, mesuré le 23/09/2026)
//!
//! Sur le .18 de Bertrand, en lecture seule : 3 909 albums locaux sans
//! `musicbrainz_release_id`, 2 requêtes MusicBrainz par album, 1,1 s entre deux
//! requêtes par IP. 67 % des albums trouvent un pressage
//! plausible, 82 % avec le nettoyage du titre de requête de la PR #4812.
//! Rendement : ~268 pistes identifiées par minute, treize fois la passe par
//! piste.
//!
//! Deux chiffres re-mesurés ici, contre la base du .18 en lecture seule le
//! 23/09/2026 : la sélection de [`sql_candidats_identification`] rend
//! **exactement 3 909 lignes** (3 938 albums locaux moins les 29 déjà
//! identifiés), et la passe coûte **2,77 s par album**, pas 2,2 — voir
//! [`SECONDES_PAR_ALBUM`]. Soit **3 h 01**, et 46 792 pistes locales à portée.
//!
//! Deux heures et demie de requêtes sortantes, c'est ce qui commande tout le
//! reste : le déclenchement à la main, l'arrêt, la reprise, et le disjoncteur.
//!
//! # Les quatre décisions de Bertrand (23/09/2026), et où elles vivent
//!
//! 1. **Déclenchement à la main, depuis les Réglages. Jamais après un scan.**
//!    Il n'y a donc, dans tout ce module, aucun appel depuis `scan_import` ni
//!    depuis `background.rs` : le seul chemin d'entrée est cette route POST.
//! 2. **Seulement les albums SANS identifiant.** C'est le `WHERE` de
//!    [`sql_candidats_identification`], et ce n'est pas une préférence :
//!    `apply_album_identification` **remplace** les clés d'identification (les
//!    descriptifs, eux, sont en `COALESCE`). Une passe de lot qui reprendrait
//!    les albums déjà identifiés écraserait des appariements corrects — dont
//!    ceux que l'utilisateur a posés lui-même, album par album, avec la route
//!    gratuite.
//! 3. **Bibliothèque locale seulement.** Les 5 492 albums UPnP sont un miroir
//!    d'un serveur distant : ni fichier, ni numéro de piste (mesuré : `0`
//!    `track_number` sur les 48 878 pistes UPnP), donc rien à ré-étiqueter et
//!    aucune matière pour apparier un pressage.
//! 4. **Le lot est Premium ; l'album à la main reste gratuit.** Le refus est
//!    un `402` qui **nomme la route gratuite** ([`refus_premium`]) — jamais un
//!    silence, jamais une liste vide.
//!
//! # Arrêt et reprise : le mécanisme existant, pas un sixième bricolage
//!
//! L'arrêt passe par [`tune_core::taches_de_fond`], le registre unique des
//! passes suspendables, sous [`Tache::Identification`]. La pause y est
//! **coopérative** (relue à la frontière entre deux albums, jamais au milieu
//! d'une écriture) et **persistante** (table `settings`, restaurée au
//! démarrage) : un `Restart=always` en pleine nuit ne relance pas deux heures
//! de requêtes sortantes tout seul.
//!
//! La reprise ne coûte rien à écrire parce qu'**il n'y a pas de curseur à
//! garder** : la sélection ne retient que les albums *sans* identifiant, donc
//! tout album déjà identifié est sorti du lot par la requête elle-même. Relancer
//! après un arrêt reprend le reliquat, et seulement lui. C'est la primitive
//! [`tune_core::taches_de_fond::est_en_pause`] (passe reprenable par requête),
//! et non `attendre_la_reprise` (passe qui tient sa liste en mémoire).
//!
//! # 🔴 Le disjoncteur : pourquoi une passe de lot ne peut pas se taire
//!
//! `lookup_release_candidates` rend `Vec::new()` **aussi bien** quand
//! MusicBrainz n'a rien que lorsqu'il a refusé la requête (503, coupure,
//! délai dépassé) : le `None` de `mb_get` est avalé par un `else { return
//! Vec::new() }`. Sur un album à la main, l'utilisateur relance et voit bien
//! que ça ne marche pas. Sur **3 909 albums**, une panne MusicBrainz produirait
//! une passe de 2 h 23 parfaitement verte qui conclurait « aucun pressage
//! trouvé » pour la bibliothèque entière — un repli silencieux, et le
//! testeur en tirerait que sa musique n'est pas identifiable.
//!
//! D'où [`ECHECS_CONSECUTIFS_MAX`] : la passe **s'arrête et le dit**. Le seuil
//! n'est pas choisi au doigt mouillé, il se calcule sur le taux mesuré au §4 de
//! l'issue — 33 % d'échecs sur un service en bonne santé, donc `0,33¹²` ≈
//! 2 · 10⁻⁶ pour douze de suite. Douze échecs consécutifs ne sont pas une
//! bibliothèque exotique, c'est une panne.
//!
//! La vraie correction — distinguer en amont « MusicBrainz a refusé » de
//! « MusicBrainz n'a rien » — appartient à `musicbrainz_release.rs`, le fichier
//! que modifie la PR #4812. Elle est laissée à une PR suivante pour ne pas
//! faire collisionner deux lots sur le même fichier ; c'est écrit tel quel dans
//! la PR.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::metadata::musicbrainz_release;
use tune_core::taches_de_fond::{Tache, est_en_pause};

use super::reidentify::{EchecIdentification, identifier_album};
use crate::state::AppState;

/// La clé de `settings` qui porte l'avancement, sur le modèle de
/// `enrich_all_status`. En base et non en mémoire : une passe de 2 h 23
/// survit à plus d'un rechargement de page, et l'écran doit retrouver où elle
/// en est.
const CLE_ETAT: &str = "identification_lot_status";

/// Combien d'albums d'affilée peuvent ne rien trouver avant que la passe
/// conclue à une panne et s'arrête. Voir l'en-tête du module pour l'arithmétique.
const ECHECS_CONSECUTIFS_MAX: u32 = 12;

/// Tous les combien on réécrit l'avancement en base. À ~2,8 s par album, dix
/// albums font une écriture toutes les trente secondes — assez pour une barre
/// qui bouge, assez peu pour ne pas peser sur la base qui sert la lecture.
const ALBUMS_PAR_ECRITURE: usize = 10;

/// Le coût d'un album, en secondes. **Mesuré**, pas déduit : 10 albums tirés
/// de la sélection réelle du .18, interrogés avec exactement les deux requêtes
/// de la chaîne et exactement ses deux attentes de cadence — 27,74 s, soit
/// **2,77 s par album** (23/09/2026).
///
/// 🔴 Ce n'est pas le 2,2 s du §5 de l'issue #4805. Celui-là ne compte que les
/// deux attentes de 1,1 s et **oublie les deux allers-retours HTTP**, ~0,57 s
/// par album. L'écart n'est pas anecdotique sur une bibliothèque entière :
/// 3 909 candidats font **3 h 01**, et non 2 h 23. Annoncer la borne basse
/// ferait conclure à un blocage à quarante minutes de la fin.
const SECONDES_PAR_ALBUM: f64 = 2.8;

/// La sélection du lot.
///
/// Sortie de la fonction pour être **exécutable** par un témoin : c'est une
/// requête, pas un texte. Un test qui n'en comparerait que la chaîne ne
/// prouverait pas qu'elle rend les bonnes lignes — et ce sont les décisions 2
/// et 3 de Bertrand qui sont écrites ici, pas un détail d'implémentation.
///
/// `COALESCE(source, 'local')` des deux côtés : la colonne est `DEFAULT
/// 'local'` mais reste nullable, et une ligne ancienne portant `NULL` est une
/// ligne locale. Un `source = 'local'` nu l'aurait écartée en silence.
pub(super) fn sql_candidats_identification() -> &'static str {
    "SELECT al.id, al.title \
     FROM albums al \
     WHERE (al.musicbrainz_release_id IS NULL OR al.musicbrainz_release_id = '') \
       AND COALESCE(al.source, 'local') = 'local' \
       AND TRIM(COALESCE(al.title, '')) <> '' \
       AND EXISTS ( \
         SELECT 1 FROM tracks t \
         WHERE t.album_id = al.id AND COALESCE(t.source, 'local') = 'local' \
       ) \
     ORDER BY al.id"
}

/// Le corps du refus Premium.
///
/// 🔴 Il **nomme la route gratuite**. Un refus qui dirait seulement « Premium »
/// laisserait croire que l'identification entière est payante, alors que la
/// décision de Bertrand est l'inverse : c'est le *lot* qui l'est — les 2 h 23
/// de requêtes sortantes — et identifier un album à la main ne l'est pas, ne
/// l'a jamais été et ne le devient pas.
pub(super) fn refus_premium() -> Value {
    json!({
        "code": "premium_required",
        "error": "premium_required",
        "premium": false,
        "feature": "auto_enrichment",
        "message": "L'identification de toute la bibliothèque est une fonction Premium.",
        "gratuit": {
            "route": "POST /library/albums/{id}/reidentify",
            "message": "Identifier un album à la main reste gratuit et sans limite.",
        },
        "upgrade": "Premium unlocks unlimited auto enrichment",
    })
}

/// L'état au repos, servi tant qu'aucune passe n'a jamais tourné.
///
/// Toutes les clés y sont, y compris à zéro : rendre `{"status":"idle"}` seul
/// rendrait la réponse typée fausse et obligerait le client à combler les
/// manques (#1897).
fn etat_au_repos() -> Value {
    json!({
        "status": "idle",
        "total": 0,
        "traites": 0,
        "identifies": 0,
        "sans_correspondance": 0,
        "pistes_identifiees": 0,
        "raison": Value::Null,
    })
}

/// Écrit l'avancement. Une seule fabrique, pour que l'état servi pendant la
/// passe et celui servi à la fin ne puissent pas porter des clés différentes.
#[allow(clippy::too_many_arguments)]
fn ecrire_etat(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    task_id: &str,
    status: &str,
    total: usize,
    traites: usize,
    identifies: usize,
    sans_correspondance: usize,
    pistes: usize,
    raison: Option<&str>,
) {
    let reglages = SettingsRepo::with_backend(backend.clone());
    reglages
        .set(
            CLE_ETAT,
            &json!({
                "status": status,
                "task_id": task_id,
                "total": total,
                "traites": traites,
                "identifies": identifies,
                "sans_correspondance": sans_correspondance,
                "pistes_identifiees": pistes,
                "raison": raison,
            })
            .to_string(),
        )
        .ok();
}

/// `GET /library/identify-all/status` — où en est la passe.
pub(super) async fn identification_lot_status(State(state): State<AppState>) -> Json<Value> {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let mut etat = reglages
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(etat_au_repos);
    // La pause vit dans le registre des tâches de fond, pas ici : l'écran doit
    // pouvoir afficher « Reprendre » sans interroger deux routes.
    etat["en_pause"] = json!(est_en_pause(Tache::Identification));
    Json(etat)
}

/// `POST /library/identify-all` — lancer la passe.
pub(super) async fn identification_lot_start(State(state): State<AppState>) -> impl IntoResponse {
    // 1. Le droit, en premier et SANS quota consommé.
    //
    //    Volontairement `check_feature` et non `gate_enrichment` : ce dernier
    //    laisse le palier gratuit consommer un jeton de son quota du jour et
    //    partir. Un jeton pour 2 h 23 de requêtes sortantes serait un tarif
    //    absurde, et surtout : la décision de Bertrand n'est pas « quota »,
    //    elle est « Premium ». Le refus doit donc être franc et se lire tel
    //    quel, pas se présenter comme une limite qui se lèvera demain.
    if !state.license.check_feature(Feature::AutoEnrichment).await {
        warn!("identification_lot_refusee_premium");
        return (StatusCode::PAYMENT_REQUIRED, Json(refus_premium()));
    }

    // 2. Une passe en pause ne part pas en silence.
    //
    //    Sans ce refus, le clic sur « Identifier la bibliothèque » lancerait
    //    une passe qui sortirait à son premier album et se déclarerait
    //    terminée : exactement le repli muet qu'on cherche à bannir.
    if est_en_pause(Tache::Identification) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "identification_en_pause",
                "error": "identification_en_pause",
                "message": "L'identification est suspendue. Reprenez-la avant de la relancer.",
                "reprendre": "POST /system/taches-de-fond/identification/reprendre",
            })),
        );
    }

    // 3. Une seule passe à la fois — la cadence MusicBrainz est par IP, deux
    //    passes ne vont pas deux fois plus vite, elles se font refuser.
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let deja = reglages
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if deja
        .as_ref()
        .and_then(|e| e["status"].as_str())
        .is_some_and(|s| s == "running")
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "identification_deja_en_cours",
                "error": "identification_deja_en_cours",
                "message": "Une identification de la bibliothèque est déjà en cours.",
                "etat": deja,
            })),
        );
    }

    // 4. La sélection réussit AVANT le 202 : une panne SQL n'est pas une
    //    bibliothèque déjà identifiée (#3810).
    let backend_selection = state.backend.clone();
    let lignes = match tokio::task::spawn_blocking(move || {
        backend_selection.query_many(sql_candidats_identification(), &[])
    })
    .await
    {
        Ok(Ok(rows)) => rows,
        erreur => {
            warn!(error = ?erreur, "identification_lot_selection_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "code": "identification_candidats_indisponibles",
                    "error": "identification_candidats_indisponibles",
                })),
            );
        }
    };

    let albums: Vec<i64> = lignes
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let total = albums.len();
    let task_id = uuid::Uuid::new_v4().to_string();

    // La durée, annoncée et non laissée à deviner : sur une bibliothèque
    // complète elle se compte en heures, et un utilisateur qui ne le sait pas
    // conclut au bout de dix minutes que rien ne se passe.
    let duree_estimee_s = (total as f64 * SECONDES_PAR_ALBUM).round() as i64;

    info!(
        task_id = %task_id,
        candidats = total,
        duree_estimee_s,
        "identification_lot_demarre"
    );
    ecrire_etat(&state.backend, &task_id, "running", total, 0, 0, 0, 0, None);

    let etat_tache = state.clone();
    let task_id_tache = task_id.clone();
    let garde = state.background_tasks.begin(
        "identification_lot",
        "Identification de la bibliothèque…",
        "identification",
    );
    tokio::spawn(async move {
        let _garde = garde;
        executer_le_lot(etat_tache, task_id_tache, albums).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "started",
            "task_id": task_id,
            "total": total,
            "duree_estimee_s": duree_estimee_s,
            "statut": "GET /library/identify-all/status",
            "arreter": "POST /system/taches-de-fond/identification/pause",
        })),
    )
}

/// La boucle. Sortie de la route pour être lisible d'un bloc : une frontière de
/// pause, une chaîne d'identification, un délai de cadence, un disjoncteur.
async fn executer_le_lot(state: AppState, task_id: String, albums: Vec<i64>) {
    let total = albums.len();
    let mut traites = 0usize;
    let mut identifies = 0usize;
    let mut sans_correspondance = 0usize;
    let mut pistes = 0usize;
    let mut echecs_consecutifs = 0u32;

    for (rang, album_id) in albums.into_iter().enumerate() {
        // La frontière de pause, en TÊTE de boucle. L'album précédent est
        // identifié et écrit, aucune requête MusicBrainz n'est en vol. On SORT
        // au lieu de garer la passe : la reprise ne dépend d'aucun curseur en
        // mémoire, la sélection la rejouera sur le seul reliquat.
        if est_en_pause(Tache::Identification) {
            info!(
                task_id = %task_id,
                traites,
                restants = total - traites,
                "identification_lot_en_pause"
            );
            ecrire_etat(
                &state.backend,
                &task_id,
                "paused",
                total,
                traites,
                identifies,
                sans_correspondance,
                pistes,
                Some("pause_utilisateur"),
            );
            return;
        }

        // La cadence MusicBrainz, ENTRE deux albums. `identifier_album` tient
        // le délai entre SES deux requêtes (recherche puis détail) ; celui-ci
        // couvre l'intervalle qu'elle ne voit pas, du détail d'un album à la
        // recherche du suivant. Sans lui, un album sur deux partirait à moins
        // de 1,1 s du précédent et récolterait un 503.
        if rang > 0 {
            musicbrainz_release::rate_limit_delay().await;
        }

        match identifier_album(&state, album_id).await {
            Ok(issue) => {
                traites += 1;
                match issue.verdict {
                    "reidentified" | "unchanged" => {
                        identifies += 1;
                        echecs_consecutifs = 0;
                        pistes += issue.applied.as_ref().map_or(0, |a| a.tracks_matched);
                    }
                    _ => {
                        // `not_found` comme `no_tracks` : rien n'a été posé.
                        sans_correspondance += 1;
                        echecs_consecutifs += 1;
                    }
                }
            }
            Err(EchecIdentification::AlbumIntrouvable) => {
                // L'album a disparu entre la sélection et son tour — un scan a
                // pu passer. Ce n'est pas un échec MusicBrainz : le compteur de
                // disjoncteur ne bouge pas.
                traites += 1;
            }
            Err(EchecIdentification::Base(e)) => {
                warn!(task_id = %task_id, album_id, error = %e, "identification_lot_album_echoue");
                traites += 1;
            }
        }

        // 🔴 Le disjoncteur. Voir l'en-tête du module : sur un service en
        //    bonne santé, douze échecs d'affilée ont une probabilité de
        //    2 · 10⁻⁶. Ce n'est plus une bibliothèque difficile, c'est une
        //    panne — et continuer 2 h 23 pour écrire « aucun pressage » partout
        //    est pire que de s'arrêter en le disant.
        if echecs_consecutifs >= ECHECS_CONSECUTIFS_MAX {
            warn!(
                task_id = %task_id,
                traites,
                echecs_consecutifs,
                "identification_lot_arret_musicbrainz_injoignable"
            );
            ecrire_etat(
                &state.backend,
                &task_id,
                "stopped",
                total,
                traites,
                identifies,
                sans_correspondance,
                pistes,
                Some("musicbrainz_injoignable"),
            );
            return;
        }

        if traites % ALBUMS_PAR_ECRITURE == 0 {
            ecrire_etat(
                &state.backend,
                &task_id,
                "running",
                total,
                traites,
                identifies,
                sans_correspondance,
                pistes,
                None,
            );
        }
    }

    info!(
        task_id = %task_id,
        total,
        identifies,
        sans_correspondance,
        pistes_identifiees = pistes,
        "identification_lot_termine"
    );
    ecrire_etat(
        &state.backend,
        &task_id,
        "done",
        total,
        traites,
        identifies,
        sans_correspondance,
        pistes,
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;

    /// Le témoin est EXÉCUTABLE : la requête est passée à une vraie base, pas
    /// comparée à une chaîne. Une garde de texte serait satisfaite par sa
    /// propre cible et ne dirait rien des lignes rendues — or ce sont les
    /// décisions 2 et 3 de Bertrand qui se jouent ici, pas une formulation.
    fn base_de_test() -> crate::state::AppState {
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let sql = [
            // 1 — local, sans identifiant, avec une piste locale : LE candidat.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (1, 'Kind of Blue', 'local', NULL)",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (10, 'So What', 1, 'local')",
            // 2 — local, DÉJÀ identifié. `apply_album_identification` remplace
            //     les clés : le reprendre écraserait un appariement correct.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (2, 'Blue Train', 'local', 'aaaaaaaa-0000-0000-0000-000000000001')",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (20, 'Moment''s Notice', 2, 'local')",
            // 3 — UPnP, sans identifiant : hors périmètre (ni fichier, ni
            //     numéro de piste, rien à ré-étiqueter).
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (3, 'Giant Steps', 'upnp', NULL)",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (30, 'Naima', 3, 'upnp')",
            // 4 — local, sans identifiant, mais SANS aucune piste : rien à
            //     apparier, et deux requêtes MusicBrainz dépensées pour rien.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (4, 'Album vide', 'local', NULL)",
            // 5 — `source` à NULL, ce qu'écrivent les lignes anciennes. C'est
            //     une ligne LOCALE : un `source = 'local'` nu l'écarterait en
            //     silence.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (5, 'Sans source', NULL, NULL)",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (50, 'Piste sans source', 5, NULL)",
            // 6 — local, sans identifiant, mais dont les pistes sont toutes
            //     UPnP : le miroir distant se glisserait par la bande.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (6, 'Album miroir', 'local', NULL)",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (60, 'Piste miroir', 6, 'upnp')",
            // 7 — identifiant présent mais VIDE, ce que laisse une écriture
            //     ratée. Un album vide d'identifiant est un album à identifier.
            "INSERT INTO albums (id, title, source, musicbrainz_release_id) \
             VALUES (7, 'Identifiant vide', 'local', '')",
            "INSERT INTO tracks (id, title, album_id, source) \
             VALUES (70, 'Piste 7', 7, 'local')",
        ];
        for requete in sql {
            etat.backend.execute(requete, &[]).unwrap();
        }
        etat
    }

    fn retenus(etat: &crate::state::AppState) -> Vec<i64> {
        etat.backend
            .query_many(sql_candidats_identification(), &[])
            .unwrap()
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect()
    }

    /// Décisions 2 et 3 de Bertrand, lues dans les lignes rendues.
    #[tokio::test]
    async fn la_selection_ne_retient_que_le_local_et_le_non_identifie() {
        let etat = base_de_test();

        assert_eq!(
            retenus(&etat),
            vec![1, 5, 7],
            "attendu : l'album local non identifié (1), celui dont `source` est \
             NULL (5) et celui dont l'identifiant est vide (7). Écartés : le \
             déjà identifié (2), l'UPnP (3), le sans-piste (4) et celui dont \
             les pistes sont toutes UPnP (6)."
        );
    }

    /// Le cœur de la reprise, et il n'y a rien d'autre à écrire pour l'obtenir :
    /// un album identifié entre-temps SORT du lot de lui-même. Relancer après
    /// un arrêt ne repart donc pas de zéro — la sélection ne rend que le
    /// reliquat.
    #[tokio::test]
    async fn un_album_identifie_entre_temps_sort_du_lot() {
        let etat = base_de_test();
        assert_eq!(retenus(&etat), vec![1, 5, 7]);

        let mbid = Some("bbbbbbbb-0000-0000-0000-000000000002".to_string());
        etat.backend
            .execute(
                "UPDATE albums SET musicbrainz_release_id = ? WHERE id = 1",
                &[&mbid as &dyn ToSqlValue],
            )
            .unwrap();

        assert_eq!(
            retenus(&etat),
            vec![5, 7],
            "l'album fraîchement identifié doit sortir du lot : c'est ce qui \
             fait qu'une reprise ne recommence pas la passe"
        );
    }

    /// Le refus Premium nomme la route gratuite. Sans elle, le message dirait
    /// à l'utilisateur que l'identification est payante — l'inverse de la
    /// décision 4.
    #[test]
    fn le_refus_premium_nomme_la_route_gratuite() {
        let corps = refus_premium();
        assert_eq!(corps["code"], "premium_required");
        assert_eq!(
            corps["gratuit"]["route"], "POST /library/albums/{id}/reidentify",
            "le refus doit nommer le geste resté gratuit : {corps}"
        );
    }
}
