//! `POST /library/albums/{id}/reidentify` — refaire l'identification d'UN album.
//!
//! Le geste que réclamait le fil forum #1455 (#2128). Jusqu'ici, un album mal
//! identifié l'était pour de bon : toute l'écriture d'enrichissement est
//! `COALESCE`, donc le mauvais MBID en place empêchait à jamais une nouvelle
//! correspondance. Le seul contournement connu — dupliquer le dossier sous un
//! autre nom, rescanner, supprimer l'original — passait par le système de
//! fichiers et faisait perdre favoris, notes et historique.
//!
//! # Ce que fait la route
//!
//! 1. Relève et efface les trois clés d'identification de cet album
//!    ([`tune_core::metadata::reidentify::clear_album_identification`]).
//! 2. Interroge MusicBrainz **au grain de l'album**, comme `identify-all`
//!    (#4805 D, décision du 05/10/2026) : les identifiants des balises d'abord
//!    (MBID de release, d'enregistrement, code-barres), puis la recherche
//!    texte, et un pressage n'est retenu que s'il est sûr et colle aux
//!    fichiers. Deux requêtes dans le cas courant, quel que soit le nombre de
//!    pistes.
//! 3. Pose le résultat : les clés en remplacement, le descriptif en
//!    remplissage seul.
//! 4. Si rien n'est trouvé, ou si l'édition reste **ambiguë**, **repose
//!    l'identification d'avant** et le dit. Sur `ambiguous`, la réponse porte
//!    les `candidates` ; `?release_id=<MBID>` impose l'édition choisie.
//!
//! # Bornes
//!
//! L'effet ne sort pas de l'album demandé. Aucun scan n'est déclenché, aucune
//! passe de fond n'est lancée, aucune tâche d'arrière-plan n'est enregistrée :
//! tout se joue dans la requête, sur les lignes de cet album. Les seules
//! écritures sont des `UPDATE` portant `WHERE id = ?` sur `albums` et
//! `WHERE ... AND album_id = ?` sur `tracks` (voir le module `reidentify` de
//! `tune-core`). Aucune ligne n'est créée ni supprimée, donc aucun `id` ne
//! bouge, donc favoris, notes, historique, listes de lecture et collections —
//! qui s'y rattachent tous par `id` — sont intacts par construction.
//!
//! # Le retour
//!
//! Un verdict explicite, jamais un silence : `reidentified`, `unchanged`,
//! `not_found`, `ambiguous`, `no_tracks`. « Retomber sur le même pressage » est un résultat
//! à part entière, et c'est même l'information la plus utile — elle dit à
//! l'utilisateur que la source en ligne confirme, et donc que l'erreur est
//! ailleurs (souvent dans les balises de ses propres fichiers).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use tracing::{info, warn};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::choix_de_pressage::{self, EntreeDIdentification, IssueDuChoix};
use tune_core::metadata::musicbrainz_release;
use tune_core::metadata::reidentify::{
    ConcordanceDesTitres, LocalTrack, apply_album_identification, clear_album_identification,
    enregistrements_si_les_titres_concordent, restore_album_identification,
};

use crate::state::AppState;

/// Le choix d'édition de l'utilisateur, passé au bouton « Ré-identifier »
/// après une réponse `ambiguous` : `?release_id=<MBID>` (un MBID nu, ou une
/// URL MusicBrainz qui en contient un).
#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct ParametresReidentification {
    #[serde(default)]
    release_id: Option<String>,
}

/// Ce qu'une identification d'album a donné, indépendamment du transport.
///
/// 🔴 Sortie de la route POUR ÊTRE APPELÉE DEUX FOIS (#4805). Le pilote de lot
/// de `identification_lot.rs` doit faire *exactement* ce que fait le bouton
/// « Ré-identifier » d'un album — même recherche, même appariement, mêmes
/// écritures, mêmes garde-fous. Recopier la chaîne aurait fabriqué deux
/// identifications qui divergent au premier correctif ; la route est
/// désormais une mise en forme JSON, et rien d'autre.
pub(super) struct Identification {
    pub verdict: &'static str,
    pub tracks_total: usize,
    pub was_identified_before: bool,
    pub previous_release_id: Option<String>,
    pub searched_title: String,
    pub searched_artist: String,
    /// Le pressage retenu. `None` sur `not_found` / `no_tracks`.
    pub meilleur: Option<musicbrainz_release::MBReleaseMatch>,
    /// Ce qui a été écrit. `None` sur `not_found` / `no_tracks`.
    pub applied: Option<tune_core::metadata::reidentify::AppliedIdentification>,
    /// 🔴 `true` quand MusicBrainz **n'a pas répondu** — `503`, coupure, délai
    /// dépassé (#4991).
    ///
    /// Le `verdict` reste `not_found` dans ce cas, et c'est délibéré : la route
    /// par album rend depuis toujours `not_found` à l'utilisateur qui
    /// ré-identifie, et son contrat ne bouge pas. Ce drapeau est là pour le
    /// seul appelant qui en a besoin — le pilote de lot de
    /// [`super::identification_lot`], dont le disjoncteur ne doit compter que
    /// les refus. Douze albums introuvables d'affilée ne sont pas une panne ;
    /// douze refus, si.
    pub refus_musicbrainz: bool,
    /// D'où vient le pressage posé : `balise_release`,
    /// `balise_enregistrement`, `code_barres`, `recherche` ou
    /// `choix_utilisateur`.
    pub source: Option<&'static str>,
    /// Pourquoi l'album est `ambiguous`.
    pub raison_ambigu: Option<&'static str>,
    /// Sur `ambiguous` : les éditions entre lesquelles rien n'a tranché, pour
    /// que l'utilisateur choisisse (`?release_id=`).
    pub candidats: Vec<musicbrainz_release::MBReleaseMatch>,
    /// La garde des enregistrements (#4805, étape D) : combien de titres
    /// concordent avec le pressage posé. `None` quand rien n'est posé.
    pub concordance: Option<ConcordanceDesTitres>,
    /// Le rattachement des artistes au pressage (#4805, étape B). `None` quand
    /// rien n'a été identifié.
    pub artistes: Option<tune_core::metadata::artistes_du_pressage::BilanArtistes>,
}

/// Pourquoi une identification n'a même pas pu être tentée. À distinguer d'un
/// `not_found`, qui est un résultat : ici, rien n'a été interrogé.
pub(super) enum EchecIdentification {
    AlbumIntrouvable,
    Base(String),
}

/// La chaîne complète pour UN album : choix du pressage, détail, appariement,
/// écriture. Le bouton « Ré-identifier » et la passe `identify-all` la
/// partagent (décision de Bertrand du 05/10/2026, #4805 D) : identifiants des
/// balises d'abord, puis la recherche texte, et un pressage n'est écrit que
/// s'il est sûr et colle aux fichiers
/// ([`choix_de_pressage::identifier_le_pressage`]). Sinon : `ambiguous`, rien
/// d'écrit, et les candidats sont rendus.
///
/// `release_choisie` : l'édition imposée par l'utilisateur (`?release_id=`).
/// Elle est lue et posée telle quelle, sans cascade ni garde.
///
/// Les requêtes MusicBrainz internes sont espacées par le limiteur partagé.
/// Les lectures de pressage passent par la release gardée en base
/// (`musicbrainz_release_cache`, #4805 idée 3) : déjà gardée, elle ne coûte
/// aucune requête, et la passe des crédits relit celle qui vient d'arriver.
/// L'appelant qui enchaîne des albums doit ajouter SON propre délai entre deux
/// appels — celui d'ici ne couvre que l'intervalle interne.
pub(super) async fn identifier_album(
    state: &AppState,
    album_id: i64,
    release_choisie: Option<&str>,
) -> Result<Identification, EchecIdentification> {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(album_id) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(EchecIdentification::AlbumIntrouvable),
        Err(e) => return Err(EchecIdentification::Base(e.to_string())),
    };

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(album_id).unwrap_or_default();
    if tracks.is_empty() {
        // Rien à ré-identifier, et surtout : ne rien effacer pour autant.
        return Ok(Identification {
            verdict: "no_tracks",
            tracks_total: 0,
            was_identified_before: false,
            previous_release_id: None,
            searched_title: album.title.clone(),
            searched_artist: String::new(),
            meilleur: None,
            applied: None,
            // MusicBrainz n'a même pas été interrogé : cet album n'apprend
            // rien sur sa santé.
            refus_musicbrainz: false,
            source: None,
            raison_ambigu: None,
            candidats: Vec::new(),
            concordance: None,
            artistes: None,
        });
    }

    // L'artiste à interroger : celui de l'album quand il est connu, sinon
    // celui de la première piste. Une compilation sans artiste d'album ne doit
    // pas partir avec une chaîne vide, qui rendrait la recherche inexploitable.
    // #4805 — `Unknown Artist` cède la place à l'artiste des pistes quand
    // elles en portent un vrai, et `VA` / `Artistes divers` deviennent
    // `Various Artists`, le nom sous lequel MusicBrainz crédite les
    // compilations : sous leur nom brut, ces albums ne rendaient rien.
    let artiste_des_pistes = tracks
        .iter()
        .filter_map(|t| t.artist_name.as_deref())
        .find(|nom| !musicbrainz_release::est_un_artiste_fictif(nom))
        .or_else(|| tracks.iter().find_map(|t| t.artist_name.as_deref()));
    let artist =
        musicbrainz_release::artiste_de_requete(album.artist_name.as_deref(), artiste_des_pistes);

    // Les identifiants des balises, lus AVANT d'effacer.
    let balises = balises_de_l_album(&state.backend, &tracks, album.barcode.as_deref());

    // 1. Effacer, en gardant le calque de ce qu'on efface.
    let cleared = match clear_album_identification(&state.backend, album_id) {
        Ok(c) => c,
        Err(e) => {
            warn!(album_id, error = %e, "reidentify_clear_failed");
            return Err(EchecIdentification::Base(e));
        }
    };

    let locales: Vec<LocalTrack> = tracks
        .iter()
        .filter_map(|t| {
            Some(LocalTrack {
                id: t.id?,
                disc: t.disc_number,
                position: t.track_number,
                title: t.title.clone(),
            })
        })
        .collect();

    poser_le_pressage(
        state,
        album_id,
        &album.title,
        artist,
        &locales,
        &balises,
        cleared,
        release_choisie,
    )
    .await
}

/// `tracks.composer` des pistes (balise `COMPOSER`), une entrée par piste qui
/// le porte : la matière de `choix_de_pressage::compositeur_majoritaire`.
fn compositeurs_des_pistes(tracks: &[tune_core::db::models::Track]) -> Vec<String> {
    tracks
        .iter()
        .filter_map(|t| t.composer.as_deref().map(str::trim))
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// Les identifiants que les balises des fichiers portent déjà, tels que le
/// scan les a rangés en base (`track_metadata`, clés de
/// `read_extended_metadata`). Idée : MetaRust (Xavier Joly), `resolve_release`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct BalisesDeLAlbum {
    /// `mb_release_id` (`MUSICBRAINZ_ALBUMID`), une entrée par piste qui le porte.
    pub releases: Vec<String>,
    /// `mb_track_id` (`MUSICBRAINZ_TRACKID`), dans l'ordre des pistes.
    pub enregistrements: Vec<String>,
    /// `barcode` des pistes, puis celui de l'album.
    pub codes_barres: Vec<String>,
    /// `tracks.composer` des pistes (balise `COMPOSER`).
    pub compositeurs: Vec<String>,
}

/// Lit [`BalisesDeLAlbum`] : trois requêtes sur la clé primaire de
/// `track_metadata`, aucune sur le réseau. Une panne de lecture rend une
/// liste vide — la cascade passe alors à la recherche texte, comme avant.
pub(super) fn balises_de_l_album(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    tracks: &[tune_core::db::models::Track],
    barcode_album: Option<&str>,
) -> BalisesDeLAlbum {
    let ids: Vec<i64> = tracks.iter().filter_map(|t| t.id).collect();
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let lire = |cle: &str| -> Vec<String> {
        let valeurs = repo.get_key_for_tracks(cle, &ids).unwrap_or_else(|e| {
            warn!(cle, error = %e, "identification_balises_illisibles");
            Default::default()
        });
        // Dans l'ordre des pistes de l'album.
        ids.iter()
            .filter_map(|id| valeurs.get(id).cloned())
            .collect()
    };
    let mut codes_barres = lire("barcode");
    codes_barres.extend(barcode_album.map(str::to_string));
    BalisesDeLAlbum {
        releases: lire("mb_release_id"),
        enregistrements: lire("mb_track_id"),
        codes_barres,
        compositeurs: compositeurs_des_pistes(tracks),
    }
}

/// La fin de [`identifier_album`] : le choix (cascade, ou édition imposée),
/// puis la pose. L'identification est déjà effacée (`cleared`) ; tout ce qui
/// n'est pas un pressage retenu la repose telle quelle.
#[allow(clippy::too_many_arguments)]
async fn poser_le_pressage(
    state: &AppState,
    album_id: i64,
    titre: &str,
    artist: String,
    locales: &[LocalTrack],
    balises: &BalisesDeLAlbum,
    cleared: tune_core::metadata::reidentify::ClearedIdentification,
    release_choisie: Option<&str>,
) -> Result<Identification, EchecIdentification> {
    let issue = match release_choisie {
        Some(id) => choix_de_pressage::lire_le_pressage_choisi(id, locales, |chemin, inc| {
            musicbrainz_release::lire_sur_musicbrainz_gardee(&state.backend, chemin, inc)
        })
        .await
        // Un `release_id` qui n'est pas un MBID est refusé par la route avant
        // d'arriver ici ; par prudence, il ne mène à rien.
        .unwrap_or(IssueDuChoix::Introuvable),
        None => {
            choix_de_pressage::identifier_le_pressage(
                EntreeDIdentification {
                    titre,
                    artiste: &artist,
                    pistes: locales,
                    releases_des_balises: &balises.releases,
                    enregistrements_des_balises: &balises.enregistrements,
                    codes_barres: &balises.codes_barres,
                    compositeurs_des_balises: &balises.compositeurs,
                },
                musicbrainz_release::rechercher_sur_musicbrainz,
                |chemin, inc| {
                    musicbrainz_release::lire_sur_musicbrainz_gardee(&state.backend, chemin, inc)
                },
            )
            .await
        }
    };

    let sans_pose = |verdict: &'static str,
                     refus: bool,
                     raison: Option<&'static str>,
                     candidats: Vec<musicbrainz_release::MBReleaseMatch>| {
        if let Err(e) = restore_album_identification(&state.backend, album_id, &cleared) {
            warn!(album_id, error = %e, "reidentify_restore_failed");
        }
        Identification {
            verdict,
            tracks_total: locales.len(),
            was_identified_before: cleared.was_identified(),
            previous_release_id: cleared.release_id.clone(),
            searched_title: titre.to_string(),
            searched_artist: artist.clone(),
            meilleur: None,
            applied: None,
            refus_musicbrainz: refus,
            source: None,
            raison_ambigu: raison,
            candidats,
            concordance: None,
            artistes: None,
        }
    };

    let (pressage, detail, source) = match issue {
        IssueDuChoix::Retenu {
            pressage,
            detail,
            source,
            ..
        } => (pressage, detail, source),
        IssueDuChoix::Ambigu {
            raison,
            source,
            candidats,
        } => {
            // 🔴 Rien n'est écrit : un pressage incertain remplacerait les
            //    clés par celles d'un album que l'utilisateur n'a peut-être
            //    pas. Le pilote le compte ; le bouton rend les candidats.
            info!(
                album_id,
                title = %titre,
                raison = raison.as_str(),
                source = source.as_str(),
                "identification_lot_ambigu"
            );
            return Ok(sans_pose(
                "ambiguous",
                false,
                Some(raison.as_str()),
                candidats,
            ));
        }
        IssueDuChoix::Introuvable => {
            info!(album_id, title = %titre, "reidentify_not_found");
            return Ok(sans_pose("not_found", false, None, Vec::new()));
        }
        IssueDuChoix::Refus(refus) => {
            warn!(album_id, title = %titre, refus = %refus, "reidentify_musicbrainz_refuse");
            return Ok(sans_pose("not_found", true, None, Vec::new()));
        }
    };

    // 🔴 #4805, étape D : l'appariement se fait au rang. Si trop peu de
    //    titres concordent avec ceux du pressage, l'album est posé mais ses
    //    pistes ne reçoivent AUCUN enregistrement : le rang d'une autre
    //    édition donnerait à chaque fichier l'enregistrement de sa voisine.
    let (recordings, concordance) =
        enregistrements_si_les_titres_concordent(locales, &detail.tracks);
    if !concordance.suffisante() {
        info!(
            album_id,
            release_id = %pressage.release_id,
            fichiers = concordance.fichiers,
            apparies = concordance.apparies,
            concordants = concordance.concordants,
            "reidentify_enregistrements_retenus"
        );
    }
    let applied = match apply_album_identification(
        &state.backend,
        album_id,
        &pressage.release_id,
        pressage.release_group_id.as_deref(),
        &recordings,
        locales.len(),
        Some(&detail),
    ) {
        Ok(a) => a,
        Err(e) => {
            warn!(album_id, error = %e, "reidentify_apply_failed");
            if let Err(e2) = restore_album_identification(&state.backend, album_id, &cleared) {
                warn!(album_id, error = %e2, "reidentify_restore_failed");
            }
            return Err(EchecIdentification::Base(e));
        }
    };
    // #4767 / #4805 E — un AUTRE pressage : les crédits gardés sont ceux de
    // l'ancien. Le curseur de la passe des crédits repasse à NULL pour qu'elle
    // les remplace. Un échec ne défait pas l'identification posée.
    if let Err(e) = tune_core::metadata::reidentify::oublier_les_credits_si_le_pressage_change(
        &state.backend,
        album_id,
        cleared.release_id.as_deref(),
        &pressage.release_id,
    ) {
        warn!(album_id, error = %e, "reidentify_credits_non_remis_a_refaire");
    }
    // #4805, étape B — le MBID des artistes, tiré des crédits du pressage que
    // l'on vient de recevoir : aucune requête de plus. Jamais d'écrasement,
    // rien sur une ambiguïté. Un échec ici ne défait pas l'identification de
    // l'album, qui est posée : il se dit et c'est tout.
    let artistes =
        match tune_core::metadata::artistes_du_pressage::rattacher_les_artistes_de_l_album(
            &state.backend,
            album_id,
            &detail,
            &recordings,
        ) {
            Ok(b) => {
                info!(
                    album_id,
                    ecrits = b.ecrits,
                    deja_poses = b.deja_poses,
                    desaccords = b.desaccords,
                    ambigus = b.ambigus,
                    sans_correspondance = b.sans_correspondance,
                    ecartes = b.ecartes,
                    refuses_par_la_base = b.refuses_par_la_base,
                    "reidentify_artistes"
                );
                Some(b)
            }
            Err(e) => {
                warn!(album_id, error = %e, "reidentify_artistes_failed");
                None
            }
        };

    let verdict = if cleared.release_id.as_deref() == Some(pressage.release_id.as_str()) {
        "unchanged"
    } else {
        "reidentified"
    };
    info!(
        album_id,
        verdict,
        release_id = %pressage.release_id,
        source = source.as_str(),
        matched = applied.tracks_matched,
        "reidentify_done"
    );
    Ok(Identification {
        verdict,
        tracks_total: locales.len(),
        was_identified_before: cleared.was_identified(),
        previous_release_id: cleared.release_id.clone(),
        searched_title: titre.to_string(),
        searched_artist: artist,
        meilleur: Some(pressage),
        applied: Some(applied),
        refus_musicbrainz: false,
        source: Some(source.as_str()),
        raison_ambigu: None,
        candidats: Vec::new(),
        concordance: Some(concordance),
        artistes,
    })
}

pub(super) async fn reidentify_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
    Query(parametres): Query<ParametresReidentification>,
) -> impl IntoResponse {
    // L'édition imposée doit être un MBID : un texte quelconque ne part pas
    // vers MusicBrainz, et le refus le dit.
    let release_choisie = match parametres.release_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(brut) => match choix_de_pressage::normaliser_mbid(brut) {
            Some(id) => Some(id),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": "release_id_invalide",
                        "error": "release_id invalide : un MBID de release MusicBrainz est attendu",
                        "release_id": brut,
                    })),
                )
                    .into_response();
            }
        },
    };
    let issue = match identifier_album(&state, album_id, release_choisie.as_deref()).await {
        Ok(i) => i,
        Err(EchecIdentification::AlbumIntrouvable) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album introuvable"})),
            )
                .into_response();
        }
        Err(EchecIdentification::Base(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };

    match (issue.verdict, &issue.meilleur, &issue.applied) {
        ("no_tracks", _, _) => Json(json!({
            "album_id": album_id,
            "verdict": "no_tracks",
            "tracks_total": 0,
        }))
        .into_response(),
        ("not_found", _, _) => Json(json!({
            "album_id": album_id,
            "verdict": "not_found",
            "tracks_total": issue.tracks_total,
            "previous_identification_restored": issue.was_identified_before,
            "searched_title": issue.searched_title,
            "searched_artist": issue.searched_artist,
            // Sur une édition imposée : celle que MusicBrainz ne connaît pas.
            "release_id": release_choisie,
        }))
        .into_response(),
        // #4805 D — la route le DIT : aucune édition n'est sûre, rien n'a été
        // écrit, voici les candidats ; l'utilisateur choisit par `release_id`.
        ("ambiguous", _, _) => Json(json!({
            "album_id": album_id,
            "verdict": "ambiguous",
            "reason": issue.raison_ambigu,
            "tracks_total": issue.tracks_total,
            "previous_identification_restored": issue.was_identified_before,
            "searched_title": issue.searched_title,
            "searched_artist": issue.searched_artist,
            "candidates": issue.candidats,
            "choose": format!("POST /library/albums/{album_id}/reidentify?release_id=<release_id>"),
        }))
        .into_response(),
        (verdict, Some(meilleur), Some(applied)) => Json(json!({
            "album_id": album_id,
            "verdict": verdict,
            "was_identified_before": issue.was_identified_before,
            "previous_release_id": issue.previous_release_id,
            "release_id": meilleur.release_id,
            "release_group_id": meilleur.release_group_id,
            "release_title": meilleur.title,
            "release_artist": meilleur.artist,
            "release_date": meilleur.date,
            "release_country": meilleur.country,
            "release_disambiguation": meilleur.disambiguation,
            "match_score": meilleur.score,
            "source": issue.source,
            "tracks_total": issue.tracks_total,
            "tracks_matched": applied.tracks_matched,
            "tracks_unmatched": applied.tracks_unmatched,
            // #4805 D : les titres qui concordent avec le pressage, et si les
            // enregistrements ont été retenus faute de concordance.
            "tracks_titles_matching": issue.concordance.map(|c| c.concordants),
            "recordings_withheld": issue.concordance.is_some_and(|c| !c.suffisante()),
            // Ce que Tune a refusé d'écraser, nommément. Sans cette liste,
            // l'utilisateur croirait la ré-identification incomplète.
            "fields_left_as_is": applied.fields_left_as_is,
        }))
        .into_response(),
        // Inatteignable : un verdict posé sans pressage. On le dit au lieu de
        // rendre un corps muet.
        (verdict, _, _) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "verdict sans pressage", "verdict": verdict})),
        )
            .into_response(),
    }
}
