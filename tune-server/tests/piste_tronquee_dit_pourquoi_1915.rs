//! Fil 1915 (Reivax66) — une piste dont le flux se coupe loin de sa fin le
//! DIT, au lieu de passer pour une fin naturelle.
//!
//! Journal du testeur : « R and R » (11:52), `local_audio_gapless_read_error
//! error=error decoding response body` à 7:51, puis fin naturelle acceptée et
//! `queue_ended` — un tiers de piste perdu, sans un mot. La boucle producteur
//! (`BoucleProducteur::tourner`, `tune-core/src/outputs/local.rs`) rendait
//! toute erreur de lecture comme une fin de flux.
//!
//! Le correctif : loin de la fin (`decisions::position_loin_de_la_fin`), la
//! boucle appelle `record_truncated_track_failure`, qui pose un constat
//! préfixé `piste_tronquee:` sur `open_failure`, puis rend `Interrompue`. Le
//! sondeur passe alors à la piste suivante en émettant un
//! `zone.playback_error` non fatal et un `playback.track_skipped`.
//!
//! ⚠️ **Pourquoi une garde de SITE par `include_str!`.** `outputs/local.rs`
//! vit derrière la feature `local-audio`, que le job `Test` de la CI n'active
//! pas : les épreuves de comportement de `local/piste_tronquee_1915.rs` n'y
//! tournent pas (même raison que `echec_de_decodage_dit_pourquoi_i3270.rs`).
//! Le côté SONDEUR, lui, n'est pas derrière la feature : ses épreuves de
//! comportement (`poller/piste_tronquee_1915_tests.rs`) tournent dans le job
//! `Test` et ne sont pas redoublées ici.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

const LOCAL: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// La production seule : le `mod tests` de fin de fichier citerait nos motifs.
fn production() -> &'static str {
    let fin = LOCAL
        .find("#[cfg(test)]\nmod tests")
        .expect("local.rs doit garder son `#[cfg(test)] mod tests` en fin de fichier");
    &LOCAL[..fin]
}

/// Le bras d'erreur de lecture de la boucle producteur : de l'événement de la
/// piste enchaînée — celle du journal — jusqu'à la fin de flux qu'il rend
/// quand l'erreur est bien au bout de la piste.
fn bras_d_erreur_de_lecture() -> &'static str {
    let src = production();
    let debut = src
        .find("\"local_audio_gapless_read_error\"")
        .expect("l'événement `local_audio_gapless_read_error` a disparu de local.rs");
    let reste = &src[debut..];
    let fin = reste
        .find("return FinDeBoucle::FinDeFlux;")
        .expect("le bras d'erreur de lecture ne rend plus de fin de flux");
    &reste[..fin]
}

/// LE verrou : avant de rendre une fin de flux, le bras d'erreur confronte la
/// position à la durée, et une coupure loin de la fin alimente le canal du
/// sondeur AVANT de rendre `Interrompue`.
#[test]
fn une_coupure_loin_de_la_fin_alimente_le_canal_et_n_enchaine_rien() {
    let bras = bras_d_erreur_de_lecture();
    let seuil = bras.find("position_loin_de_la_fin(");
    let rapport = bras.find("record_truncated_track_failure(");
    let interrompue = bras.find("return FinDeBoucle::Interrompue;");
    assert!(
        seuil.is_some(),
        "le bras d'erreur de lecture ne confronte plus la position à la durée : \
         toute erreur redevient une fin naturelle (fil 1915) :\n{bras}"
    );
    assert!(
        rapport.is_some(),
        "le bras d'erreur de lecture n'appelle plus `record_truncated_track_failure` : \
         la coupure ne dirait plus rien à l'écran :\n{bras}"
    );
    assert!(
        interrompue.is_some(),
        "une coupure doit rendre `Interrompue` : sinon la chaîne gapless se \
         poursuit et la sortie annonce une fin naturelle :\n{bras}"
    );
    // Les trois existent (vérifié ci-dessus) : l'ordre peut être comparé.
    assert!(
        seuil < rapport && rapport < interrompue,
        "ordre attendu : seuil, puis rapport, puis `Interrompue` :\n{bras}"
    );
}

/// Le rapporteur écrit dans le verrou, AVEC le préfixe que le sondeur lit
/// pour passer à la suivante. Sans le préfixe, le constat prendrait le chemin
/// des pannes de sortie : message fatal et zone arrêtée.
#[test]
fn le_rapporteur_de_piste_tronquee_ecrit_un_constat_prefixe() {
    let src = production();
    let debut = src
        .find("\nfn record_truncated_track_failure(")
        .expect("le rapporteur `record_truncated_track_failure` a disparu de local.rs");
    let reste = &src[debut + 1..];
    let corps = &reste[..reste.find("\n}\n").expect("fin du rapporteur introuvable")];
    for motif in [
        "\"piste_tronquee",
        "failure_slot.lock()",
        "*slot = Some(",
        "PREFIXE_PISTE_TRONQUEE",
    ] {
        assert!(
            corps.contains(motif),
            "le rapporteur de piste tronquée ne contient plus `{motif}` :\n{corps}"
        );
    }
}

/// Le seuil a besoin de la durée de CETTE piste. Les deux boucles de
/// `play_url` la fournissent : la piste initiale relit `duration_ms` au moment
/// de l'erreur, la piste enchaînée porte celle de son `PendingNextMedia`.
/// Une construction qui passerait `DUREE_DE_PISTE_INCONNUE` rendrait le
/// correctif muet sur ce chemin sans qu'aucun test ne le voie.
#[test]
fn les_deux_boucles_de_play_url_connaissent_la_duree_de_leur_piste() {
    let src = production();
    for cablage in [
        "duree_de_la_piste_ms: duration_ms_arc.as_ref(),",
        "AtomicU64::new(next.duration_ms.unwrap_or(0))",
        "duree_de_la_piste_ms: &duree_enchainee_ms,",
    ] {
        assert_eq!(
            src.matches(cablage).count(),
            1,
            "le câblage `{cablage}` doit apparaître exactement une fois dans local.rs"
        );
    }
}
