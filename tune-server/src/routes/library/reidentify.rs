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
//! 2. Interroge MusicBrainz **au grain de l'album** : une recherche de
//!    pressage, puis un détail avec sa liste de pistes. Deux requêtes en tout,
//!    quel que soit le nombre de pistes — et non deux par piste comme la passe
//!    de fond, ce qui rend l'opération tenable dans le temps d'une requête HTTP.
//! 3. Pose le résultat : les clés en remplacement, le descriptif en
//!    remplissage seul.
//! 4. Si rien n'est trouvé, **repose l'identification d'avant** et le dit.
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
//! `not_found`, `no_tracks`. « Retomber sur le même pressage » est un résultat
//! à part entière, et c'est même l'information la plus utile — elle dit à
//! l'utilisateur que la source en ligne confirme, et donc que l'erreur est
//! ailleurs (souvent dans les balises de ses propres fichiers).

use axum::Json;
use axum::extract::{Path, State};
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
    LocalTrack, apply_album_identification, clear_album_identification, map_recording_ids,
    restore_album_identification,
};

use crate::state::AppState;

/// Combien de pressages candidats on regarde. On n'en retient qu'un — le
/// mieux classé — mais en demander plusieurs laisse au classement de quoi
/// travailler.
const CANDIDATS: usize = 5;

/// Qui demande l'identification — et donc quelle règle s'applique (#4805).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModeIdentification {
    /// Le bouton « Ré-identifier » d'un album. Inchangé : la recherche texte,
    /// SANS les identifiants des balises (c'est eux qu'on soupçonne), et le
    /// mieux classé. L'utilisateur voit le résultat et peut recommencer.
    Manuel,
    /// La passe `identify-all`, où personne ne regarde. Les identifiants des
    /// balises d'abord, puis la recherche texte ; un pressage n'est écrit que
    /// s'il est sûr et colle aux fichiers, sinon l'album est `ambiguous`
    /// ([`choix_de_pressage::identifier_le_pressage`]).
    Lot,
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
    /// D'où vient le pressage posé (mode lot seulement) : `balise_release`,
    /// `balise_enregistrement`, `code_barres` ou `recherche`.
    pub source: Option<&'static str>,
    /// Pourquoi l'album est `ambiguous` (mode lot seulement).
    pub raison_ambigu: Option<&'static str>,
}

/// Pourquoi une identification n'a même pas pu être tentée. À distinguer d'un
/// `not_found`, qui est un résultat : ici, rien n'a été interrogé.
pub(super) enum EchecIdentification {
    AlbumIntrouvable,
    Base(String),
}

/// La chaîne complète pour UN album : recherche, détail, appariement, écriture.
///
/// Deux requêtes MusicBrainz, séparées par [`musicbrainz_release::rate_limit_delay`].
/// L'appelant qui enchaîne des albums doit ajouter SON propre délai entre deux
/// appels — celui d'ici ne couvre que l'intervalle interne.
pub(super) async fn identifier_album(
    state: &AppState,
    album_id: i64,
    mode: ModeIdentification,
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

    // Les identifiants des balises, lus AVANT d'effacer (mode lot seulement).
    let balises = match mode {
        ModeIdentification::Lot => {
            balises_de_l_album(&state.backend, &tracks, album.barcode.as_deref())
        }
        ModeIdentification::Manuel => BalisesDeLAlbum::default(),
    };

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

    if mode == ModeIdentification::Lot {
        return identifier_en_lot(
            state,
            album_id,
            &album.title,
            artist,
            &locales,
            &balises,
            cleared,
        )
        .await;
    }

    // 2. Chercher le pressage. Volontairement SANS le MBID d'avant : c'est lui
    //    qu'on soupçonne, et le scan a pu le lire dans des balises fausses
    //    (`scan_import.rs:443`). On repart du titre et de l'artiste.
    let recherche = musicbrainz_release::lookup_release_candidates(
        &album.title,
        &artist,
        Some(tracks.len() as u32),
        CANDIDATS,
    )
    .await;

    // 🔴 #4991 — relevé AVANT de consommer la recherche. « MusicBrainz n'a pas
    // ce pressage » et « MusicBrainz n'a pas répondu » donnent tous deux une
    // liste vide ; seul ce drapeau les sépare, et le pilote de lot en dépend.
    let refus_musicbrainz = recherche.service_refuse();

    let Some(meilleur) = recherche.meilleur() else {
        // Rien trouvé : l'album doit se retrouver EXACTEMENT comme avant.
        if let Err(e) = restore_album_identification(&state.backend, album_id, &cleared) {
            warn!(album_id, error = %e, "reidentify_restore_failed");
        }
        if refus_musicbrainz {
            // Un refus se DIT dans le journal, là où un « rien trouvé » se
            // constate. Le verdict, lui, ne bouge pas : la route par album
            // rend `not_found` comme avant.
            warn!(album_id, title = %album.title, "reidentify_musicbrainz_refuse");
        } else {
            info!(album_id, title = %album.title, "reidentify_not_found");
        }
        return Ok(Identification {
            verdict: "not_found",
            tracks_total: tracks.len(),
            was_identified_before: cleared.was_identified(),
            previous_release_id: cleared.release_id.clone(),
            searched_title: album.title.clone(),
            searched_artist: artist,
            meilleur: None,
            applied: None,
            refus_musicbrainz,
            source: None,
            raison_ambigu: None,
        });
    };

    musicbrainz_release::rate_limit_delay().await;
    let detail = musicbrainz_release::lookup_release_detail(&meilleur.release_id).await;

    // 3. Associer les pistes du pressage aux pistes locales.
    let recordings = match detail.as_ref() {
        Some(d) => map_recording_ids(&locales, &d.tracks),
        None => Vec::new(),
    };

    // 4. Poser.
    let applied = match apply_album_identification(
        &state.backend,
        album_id,
        &meilleur.release_id,
        meilleur.release_group_id.as_deref(),
        &recordings,
        locales.len(),
        detail.as_ref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            warn!(album_id, error = %e, "reidentify_apply_failed");
            // Ne pas laisser l'album à moitié effacé.
            if let Err(e2) = restore_album_identification(&state.backend, album_id, &cleared) {
                warn!(album_id, error = %e2, "reidentify_restore_failed");
            }
            return Err(EchecIdentification::Base(e));
        }
    };

    // 5. Le verdict. « Le même pressage qu'avant » n'est pas un échec, mais ce
    //    n'est pas non plus une correction : il faut le distinguer.
    let meme_pressage = cleared.release_id.as_deref() == Some(meilleur.release_id.as_str());
    let verdict = if meme_pressage {
        "unchanged"
    } else {
        "reidentified"
    };

    info!(
        album_id,
        verdict,
        release_id = %meilleur.release_id,
        matched = applied.tracks_matched,
        "reidentify_done"
    );

    Ok(Identification {
        verdict,
        tracks_total: locales.len(),
        was_identified_before: cleared.was_identified(),
        previous_release_id: cleared.release_id.clone(),
        searched_title: album.title.clone(),
        searched_artist: artist,
        meilleur: Some(meilleur),
        applied: Some(applied),
        // MusicBrainz a répondu, et son pressage est posé.
        refus_musicbrainz: false,
        source: None,
        raison_ambigu: None,
    })
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
    }
}

/// Le mode lot de [`identifier_album`] : la cascade de
/// [`choix_de_pressage::identifier_le_pressage`], puis la même pose que le
/// bouton. L'identification est déjà effacée (`cleared`) ; tout ce qui n'est
/// pas un pressage sûr la repose telle quelle.
async fn identifier_en_lot(
    state: &AppState,
    album_id: i64,
    titre: &str,
    artist: String,
    locales: &[LocalTrack],
    balises: &BalisesDeLAlbum,
    cleared: tune_core::metadata::reidentify::ClearedIdentification,
) -> Result<Identification, EchecIdentification> {
    let issue = choix_de_pressage::identifier_le_pressage(
        EntreeDIdentification {
            titre,
            artiste: &artist,
            pistes: locales,
            releases_des_balises: &balises.releases,
            enregistrements_des_balises: &balises.enregistrements,
            codes_barres: &balises.codes_barres,
        },
        musicbrainz_release::rechercher_sur_musicbrainz,
        musicbrainz_release::lire_sur_musicbrainz,
    )
    .await;

    let sans_pose = |verdict: &'static str, refus: bool, raison: Option<&'static str>| {
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
        }
    };

    let (pressage, detail, source) = match issue {
        IssueDuChoix::Retenu {
            pressage,
            detail,
            source,
            ..
        } => (pressage, detail, source),
        IssueDuChoix::Ambigu { raison, source } => {
            // 🔴 Rien n'est écrit : un pressage incertain remplacerait les
            //    clés par celles d'un album que l'utilisateur n'a peut-être
            //    pas. Le pilote le compte.
            info!(
                album_id,
                title = %titre,
                raison = raison.as_str(),
                source = source.as_str(),
                "identification_lot_ambigu"
            );
            return Ok(sans_pose("ambiguous", false, Some(raison.as_str())));
        }
        IssueDuChoix::Introuvable => {
            info!(album_id, title = %titre, "reidentify_not_found");
            return Ok(sans_pose("not_found", false, None));
        }
        IssueDuChoix::Refus(refus) => {
            warn!(album_id, title = %titre, refus = %refus, "reidentify_musicbrainz_refuse");
            return Ok(sans_pose("not_found", true, None));
        }
    };

    let recordings = map_recording_ids(locales, &detail.tracks);
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
    })
}

pub(super) async fn reidentify_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> impl IntoResponse {
    let issue = match identifier_album(&state, album_id, ModeIdentification::Manuel).await {
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
            "tracks_total": issue.tracks_total,
            "tracks_matched": applied.tracks_matched,
            "tracks_unmatched": applied.tracks_unmatched,
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
