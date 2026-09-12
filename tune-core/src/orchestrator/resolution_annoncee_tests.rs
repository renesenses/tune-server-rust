use super::resolution_annoncee;
use crate::db::models::Track;
use crate::playback::NowPlaying;

/// Une piste de la bibliotheque dont la ligne ne porte NI frequence NI
/// profondeur ne doit rien annoncer.
///
/// Le repli `.or(resolved)` de `play_inner` y substituait la resolution de
/// SORTIE — et pour une piste locale cette valeur est FABRIQUEE :
/// `resolve_local_track` fait `track.sample_rate.unwrap_or(44100)` et
/// `track.bit_depth.unwrap_or(16)` precisement quand la ligne se tait, puis
/// `cap_output_bit_depth` la ramene dans 16..24. Le client affichait donc
/// « 44,1 kHz / 16 bits » pour un fichier que personne n'a mesure.
///
/// C'est la lecture aleatoire qui le rend visible : elle demarre sans
/// cesse une PREMIERE piste tiree au hasard, donc une ligne muette bien
/// plus souvent qu'un album qu'on a choisi (fil 1036, william — #2250).
#[test]
fn une_ligne_locale_muette_n_annonce_rien_plutot_qu_une_valeur_inventee() {
    assert_eq!(
        resolution_annoncee(None, Some(44100), true),
        None,
        "frequence : une piste locale sans frequence en base doit rester muette, \
         pas heriter du 44100 fabrique par resolve_local_track"
    );
    assert_eq!(
        resolution_annoncee(None, Some(16), true),
        None,
        "profondeur : une piste locale sans profondeur en base doit rester muette, \
         pas heriter du 16 fabrique par resolve_local_track"
    );
}

/// Quand la ligne SAIT, c'est elle qui parle — y compris (surtout) lorsque
/// la sortie transcode. C'est la regle deja en place, que ce correctif ne
/// doit pas defaire.
#[test]
fn une_ligne_locale_qui_sait_repond_par_sa_propre_valeur() {
    assert_eq!(
        resolution_annoncee(Some(96000), Some(44100), true),
        Some(96000)
    );
    assert_eq!(resolution_annoncee(Some(24), Some(16), true), Some(24));
}

/// Le streaming n'a AUCUNE ligne en bibliotheque : la resolution resolue y
/// est celle de la source, pas d'une sortie. Le repli doit survivre —
/// sinon Qobuz et Tidal perdent leur affichage.
#[test]
fn le_streaming_garde_le_repli_sur_le_format_du_flux() {
    assert_eq!(resolution_annoncee(None, Some(96000), false), Some(96000));
    assert_eq!(resolution_annoncee(None, Some(24), false), Some(24));
}

/// La MEME ligne doit s'annoncer pareil qu'on l'atteigne en premiere piste
/// (`play_inner`) ou par avance gapless (`advance_queue_metadata`, qui
/// passe par `NowPlaying::from_track`).
///
/// C'est l'asymetrie que decrivait william : la piste 1 d'une file
/// aleatoire affichait un chiffre, la piste 2 de la meme file n'affichait
/// rien — pour des lignes de base identiquement muettes.
#[test]
fn les_deux_surfaces_annoncent_la_meme_chose_pour_une_ligne_muette() {
    let mut ligne = Track::new("16 bits, profondeur absente de la base".into());
    ligne.id = Some(1);
    ligne.sample_rate = None;
    ligne.bit_depth = None;

    let par_avance_gapless = NowPlaying::from_track(&ligne);

    // Ce que `play_inner` annonce pour cette meme ligne, la sortie ayant
    // fabrique 44100/16.
    let par_premiere_piste_sr =
        resolution_annoncee(ligne.sample_rate.map(|v| v as u32), Some(44100), true);
    let par_premiere_piste_bd =
        resolution_annoncee(ligne.bit_depth.map(|v| v as u32), Some(16), true);

    assert_eq!(
        par_premiere_piste_sr, par_avance_gapless.sample_rate,
        "premiere piste et avance gapless doivent annoncer la MEME frequence"
    );
    assert_eq!(
        par_premiere_piste_bd, par_avance_gapless.bit_depth,
        "premiere piste et avance gapless doivent annoncer la MEME profondeur"
    );
}

/// Garde-fou de site d'appel : `play_inner` doit passer par la regle, et
/// non refaire un `.or(resolved.…)` en direct. Sans cela, la regle
/// ci-dessus serait vraie en isolation et fausse en production.
#[test]
fn play_inner_passe_bien_par_la_regle() {
    // `play_inner` vit dans le module de famille `transport` (REF-2, #2219).
    let src = include_str!("../orchestrator/transport.rs");
    let np = src
        .find("            sample_rate: resolution_annoncee(")
        .zip(src.find("            bit_depth: resolution_annoncee("));
    assert!(
        np.is_some(),
        "le NowPlaying de play_inner doit construire sample_rate ET bit_depth \
         via resolution_annoncee(), sinon la regle ne protege rien"
    );
    assert!(
        !src.contains(
            ".and_then(|t| t.bit_depth.map(|v| v as u32))\n                .or(resolved.bit_depth)"
        ),
        "le repli direct .or(resolved.bit_depth) doit avoir disparu de play_inner"
    );
}
