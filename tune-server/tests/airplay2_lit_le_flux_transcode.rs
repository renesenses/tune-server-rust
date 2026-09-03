//! AirPlay 2 joue l'ADRESSE DU FLUX, jamais le fichier d'origine (#1216, 4e fois).
//!
//! ## Le fait
//!
//! `tune-core/src/outputs/airplay2/mod.rs` faisait, dans `play_media`, un
//! `unwrap_or` qui preferait le chemin de fichier du media a son adresse.
//!
//! Ce chemin porte `tracks.file_path`, le fichier brut de la bibliotheque.
//! L'orchestrateur le renseigne pour TOUTE piste locale (`local_file_path`,
//! `orchestrator.rs`) sans jamais regarder le type de sortie : c'est une
//! commodite offerte aux sorties qui savent lire un fichier elles-memes —
//! OAAT et son DSD natif — pas une instruction de lecture.
//!
//! Or une zone AirPlay 2 recoit bel et bien un flux TRAITE. La fonction
//! `pull_output_needs_dsp_transcode` (`orchestrator.rs`) est une liste
//! NEGATIVE : elle retient toute sortie qui n'est ni locale, ni OAAT, ni
//! pousseuse d'URI (`is_push_uri_output_type` :
//! dlna/openhome/chromecast/bluos/squeezebox/slimproto), ni navigateur.
//! `airplay2` n'est dans aucune de ces listes : il y tombe. Des lors qu'un
//! egaliseur, une correction de piece ou un ReplayGain sont armes sur la zone,
//! `eq_forces_transcode` vaut vrai et le serveur decode, filtre, gaine,
//! reencode vers un fichier temporaire, ouvre une session et en publie
//! l'adresse dans `media.url` — en sautant meme le cache de transcodage,
//! puisque l'EQ n'entre pas dans sa clef.
//!
//! Puis AirPlay 2 rejouait le fichier d'origine. Le traitement etait calcule,
//! ecrit sur le disque, et jete sans avoir ete lu.
//!
//! Cote ecran, le client web ne dement rien : il n'a AUCUNE garde par type de
//! sortie autour de l'egaliseur, et le panneau « En ecoute » affiche la courbe
//! reelle sous un libelle affirmatif (« Reglage personnalise »). L'utilisateur
//! lit « actif » et n'entend rien — le defaut de Mika sur Beoplay A9 (#1216),
//! deja revu sur les zones navigateur puis sur les sorties PULL type Diretta.
//!
//! ## Ce que ce fichier cloue
//!
//! Une garde de SITE, par `include_str!` : elle lit le TEXTE du module
//! AirPlay 2 quelles que soient les `cfg` et les features. C'est delibere —
//! le job `Test` de la CI (`ci.yml`) tourne `--no-default-features --features
//! oaat,cloud-relay,bandcamp`, donc sans `local-audio`, et les jobs qui
//! portent l'audio local sont conditionnes a `full`. Une garde de
//! comportement seule ne serait jamais jouee sur une PR vers `batch/*`.
//!
//! La garde de COMPORTEMENT, elle, vit dans le module lui-meme :
//! `play_media_envoie_l_adresse_du_flux_et_pas_le_fichier_d_origine`
//! (`tune-core/src/outputs/airplay2/mod.rs`, `mod transport_tests`) monte un
//! faux daemon et lit le `path` reellement envoye sur le fil.
//!
//! ## Pourquoi les commentaires sont retires avant de compter
//!
//! Le module DOIT pouvoir nommer le defaut pour l'expliquer — c'est le sens
//! meme du commentaire qui remplace la ligne fautive. Compter sur le texte brut
//! rendait donc la garde rouge sur sa propre explication, et poussait a ecrire
//! des commentaires evasifs pour la contenter. On ne mesure que le CODE.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compile que
//! parce qu'il est declare dans l'agregateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

/// Le module AirPlay 2, lu comme du TEXTE : aucune feature ne peut le masquer.
const AIRPLAY2: &str = include_str!("../../tune-core/src/outputs/airplay2/mod.rs");

/// Le module prive de ses lignes de commentaire (`//`, `///`, `//!`).
///
/// Volontairement naif : il ne cherche pas a comprendre Rust, seulement a ne
/// pas compter une explication comme si c'etait une instruction.
fn code_seul(source: &str) -> String {
    source
        .lines()
        .filter(|ligne| !ligne.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// La ligne du defaut ne doit jamais revenir.
#[test]
fn airplay2_ne_prefere_plus_le_fichier_d_origine_a_l_adresse_du_flux() {
    let occurrences = code_seul(AIRPLAY2).matches("file_path.unwrap_or").count();
    assert_eq!(
        occurrences, 0,
        "airplay2/mod.rs prefere de nouveau le chemin de fichier a l'adresse \
         du flux : tout le DSP de la zone (egaliseur, correction de piece, \
         ReplayGain) est calcule puis jete. C'est #1216."
    );
}

/// Le code du module ne lit plus DU TOUT le chemin de fichier du media.
///
/// Plus large que la ligne exacte : une variante ecrite autrement
/// (`match`, `if let Some(p) = ...`) rouvrirait le meme trou sans reintroduire
/// `unwrap_or`.
#[test]
fn airplay2_ne_lit_aucun_chemin_de_fichier_du_media() {
    let occurrences = code_seul(AIRPLAY2).matches("media.file_path").count();
    assert_eq!(
        occurrences, 0,
        "airplay2/mod.rs lit de nouveau le chemin de fichier du media. Cette \
         sortie est POUSSEE : elle doit jouer ce que le serveur a decide \
         d'envoyer, c'est-a-dire `media.url`, et rien d'autre."
    );
}

/// Et il envoie bien l'adresse du flux au daemon.
///
/// L'inverse des deux gardes ci-dessus : supprimer la ligne au lieu de la
/// corriger les laisserait vertes, alors que plus rien ne partirait.
#[test]
fn airplay2_envoie_l_adresse_du_flux_au_daemon() {
    let code = code_seul(AIRPLAY2);
    assert_eq!(
        code.matches("let path = media.url;").count(),
        1,
        "airplay2/mod.rs ne construit plus le chemin envoye au daemon depuis \
         `media.url`. Verifier `play_media`."
    );
    assert!(
        code.contains("\"path\": path,"),
        "la commande `play` n'envoie plus `path` au daemon."
    );
}

/// La garde de comportement existe, et porte toujours son nom.
///
/// La garde de site ci-dessus lit du texte : elle ne prouve rien du fil. Si le
/// test de comportement disparait, il ne reste plus que du texte — et ce
/// fichier doit le dire.
#[test]
fn la_garde_de_comportement_du_module_airplay2_est_toujours_la() {
    assert!(
        code_seul(AIRPLAY2)
            .contains("play_media_envoie_l_adresse_du_flux_et_pas_le_fichier_d_origine"),
        "le test de comportement a disparu de airplay2/mod.rs : il ne reste \
         qu'une garde de texte, qui ne mesure pas ce qui part sur le fil."
    );
}
