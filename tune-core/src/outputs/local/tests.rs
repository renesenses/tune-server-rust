use super::*;

// -----------------------------------------------------------------------
// Fin de piste sur le chemin cpal partagé (#1919, Alain — #2047)
//
// Alain décrit une playlist Qobuz dont une piste « se bloque et lit en
// boucle les 3 à 4 dernières secondes ». Sortie USB du PC vers son DAC :
// chemin cpal PARTAGÉ, aucun renderer réseau. Aléatoire, « en début comme
// en fin de piste ».
//
// `supports_internal_gapless()` répondait `!exclusive_mode` — une capacité
// STATIQUE. La boucle d'enchaînement du fil de lecture abandonne pourtant
// par six chemins (rien en réserve, HTTP en erreur, HTTP injoignable,
// en-tête vide, flux suivant non-WAV, piste chaînée sans fin propre), et
// après chacun d'eux le fil draine puis SORT. La sortie continuait
// néanmoins d'affirmer au poller qu'elle savait enchaîner toute seule.
//
// Le poller relit cette réponse à trois endroits ; celui qui fait le
// symptôme est `decisions::position_reset_fires`. Son propre commentaire
// nomme le résultat : « advancing metadata sends no `play` and steals the
// event from the natural-end path […] causing the endless 1-2s-then-zero
// loop (Rhorn, #1072) ». C'est la boucle qu'entend Alain.
//
// Exactement le défaut corrigé sur OAAT par #1323/#2013 — « la boucle de
// flux ne disait pas au poller qu'elle était morte » —, sur une autre
// sortie.
// -----------------------------------------------------------------------

/// Une boucle d'enchaînement TERMINÉE doit rendre la main au poller.
///
/// Le test compose la sonde de la sortie avec le prédicat du poller qui
/// produit le symptôme, plutôt que de se contenter de relire un booléen :
/// c'est la composition des deux qui décide si un `play` est envoyé.
#[test]
fn une_chaine_locale_terminee_ne_declenche_plus_l_avance_muette() {
    use crate::poller::decisions;

    // Fin de piste : la position chute de 3:34 à 0, gapless armé.
    let chute = decisions::position_reset(214_000, 0, true);
    assert!(
        chute,
        "le banc doit bien représenter une chute de fin de piste"
    );

    let sortie = LocalOutput::new("USB DAC".to_string());
    assert!(
        !sortie.exclusive_mode,
        "le cas d'Alain est le chemin cpal PARTAGÉ"
    );

    // Au repos, la sortie sait enchaîner : le poller doit pouvoir armer le
    // gapless, sinon on casse l'enchaînement sans coupure de tout le monde.
    assert!(
        sortie.supports_internal_gapless(),
        "au repos, le chemin partagé annonce l'enchaînement interne"
    );
    assert!(
        decisions::position_reset_fires(chute, sortie.supports_internal_gapless(), false),
        "tant que la boucle vit, la chute est bien une transition interne"
    );

    // La boucle a rendu les armes (flux suivant non-WAV, HTTP en échec,
    // rien en réserve…). Le fil draine et sort : plus rien ne peut
    // enchaîner.
    sortie.set_chain_exhausted_for_test(true);

    assert!(
        !sortie.supports_internal_gapless(),
        "une boucle terminée ne peut plus rien enchaîner, quoi qu'elle ait su faire avant"
    );
    assert!(
        !decisions::position_reset_fires(chute, sortie.supports_internal_gapless(), false),
        "l'avance métadonnées seule n'envoie AUCUN play : sur une chaîne morte \
         elle vole l'événement au chemin de fin naturelle et produit la boucle \
         de quelques secondes signalée par Alain (#1919)"
    );
}

/// La sonde repart de zéro pour le fil suivant.
///
/// Sans cette remise à zéro le drapeau serait collant : une seule chaîne
/// avortée désarmerait le gapless de la sortie pour le reste de la session,
/// ce qui remplacerait un défaut par une régression audible sur les albums
/// enchaînés.
#[test]
fn la_sonde_repart_de_zero_pour_le_fil_suivant() {
    let sortie = LocalOutput::new("USB DAC".to_string());
    sortie.set_chain_exhausted_for_test(true);
    assert!(!sortie.supports_internal_gapless());

    // `play_url()` ouvre un fil neuf et relève le drapeau. On ne peut pas
    // l'appeler sans carte son ; on vérifie la propriété qu'il garantit.
    sortie.set_chain_exhausted_for_test(false);
    assert!(
        sortie.supports_internal_gapless(),
        "un fil de lecture neuf a une boucle intacte"
    );
}

/// Qui a le droit de déclarer la chaîne épuisée.
///
/// Le cas qui compte est le dernier : un ancien fil, encore en train de
/// drainer, ne doit pas éteindre la sonde du morceau que `play_url()` vient
/// de lancer — sinon le correctif de #1919 se paierait d'une perte de
/// gapless sur le morceau suivant, à chaque changement de piste.
#[test]
fn seul_le_fil_en_titre_declare_sa_chaine_epuisee() {
    // Le fil courant sort de sa boucle : il le dit.
    assert!(
        doit_declarer_chaine_epuisee(false, 7, 7),
        "une boucle terminée doit rendre la main au poller"
    );

    // `stop()` l'a fait taire : le drapeau ne lui appartient plus.
    assert!(
        !doit_declarer_chaine_epuisee(true, 7, 7),
        "un fil supplanté par stop() ne touche pas au drapeau"
    );

    // Un `play_url()` est passé : ce fil est périmé. C'est le cas qui
    // protège le gapless du morceau suivant.
    assert!(
        !doit_declarer_chaine_epuisee(false, 8, 7),
        "un fil d'une génération périmée ne doit JAMAIS éteindre la sonde \
         du morceau courant"
    );

    // Les deux à la fois : périmé ET supplanté.
    assert!(!doit_declarer_chaine_epuisee(true, 8, 7));
}

/// Une sortie EXCLUSIVE ne devient pas enchaînable parce que sa boucle est
/// vivante : elle ne consomme jamais le `next_media` mis en réserve.
/// Verrou anti-régression sur le correctif de DEvir (ASIO Fireface).
#[test]
fn une_sortie_exclusive_reste_non_enchainable() {
    let sortie = LocalOutput::new_with_exclusive("Fireface ASIO".to_string(), true);
    assert!(
        !sortie.supports_internal_gapless(),
        "ASIO/WASAPI exclusif : boucle dédiée qui sort à l'EOF sans consommer next_media"
    );
    sortie.set_chain_exhausted_for_test(false);
    assert!(
        !sortie.supports_internal_gapless(),
        "remettre la sonde à zéro ne doit JAMAIS rendre une sortie exclusive enchaînable"
    );
}

// -----------------------------------------------------------------------
// Égaliseur de zone sur la sortie locale (#1416, Jean Marie)
//
// L'`EqProcessor` n'était appliqué que dans `transcode_source_to_file`,
// chemin qu'une zone locale ne prend JAMAIS (`use_file_transcode_for`
// exige une sortie réseau). L'égaliseur n'agissait donc nulle part sur un
// DAC local. Ces tests verrouillent le branchement dans la chaîne DSP.
// -----------------------------------------------------------------------

/// EQ de test : -12 dB de plateau aigu, audible sur un sinus 8 kHz.
fn test_eq() -> crate::audio::eq::EqProcessor {
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 2000.0,
            gain: -12.0,
            q: 0.71,
            band_type: "high_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    crate::audio::eq::EqProcessor::new(&profile, 44100, 2)
}

#[test]
fn la_sortie_expose_les_compteurs_du_vrai_processeur_eq() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_eq(Some(test_eq()));
    let mut samples = vec![f32::NAN, 0.0];

    apply_local_dsp(
        &mut samples,
        &sortie.eq,
        &sortie.convolver,
        &sortie.crossfeed,
        &sortie.pure_bypass,
        &sortie.mono_downmix,
        2,
        false,
    );

    let metrics = sortie
        .dsp_metrics()
        .expect("un EQ actif doit exposer ses compteurs");
    assert_eq!(metrics.eq_non_finite_samples, 1);
    assert_eq!(metrics.eq_overs, 0);
}

// ---- #2362 : sortie mono sur la chaîne locale ---------------------------

/// Fait traverser la chaîne DSP locale RÉELLE à un tampon, sans égaliseur,
/// sans convolveur, sans crossfeed : seul le repli mono peut donc en
/// changer le contenu.
fn chaine_locale_nue(mono: bool, pure: bool, dop: bool, pcm: &mut Vec<f32>) {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_mono_downmix(mono);
    sortie.set_pure_bypass(pure);
    apply_local_dsp(
        pcm,
        &sortie.eq,
        &sortie.convolver,
        &sortie.crossfeed,
        &sortie.pure_bypass,
        &sortie.mono_downmix,
        2,
        dop,
    );
}

/// Armé, le repli somme réellement les deux voies DANS la chaîne locale.
///
/// La deuxième trame est la mutation discriminante : elle ne porte du
/// signal QUE sur la voie droite. Un « mono » qui garderait la voie gauche
/// — le piège nommé au point 2 de #2362 — rendrait `0.0` et laisserait
/// Nicolas Tardif, dont l'unique enceinte est câblée à gauche, aussi sourd
/// qu'avant à tout ce qui est panné à droite.
#[test]
fn le_repli_mono_somme_les_deux_voies_dans_la_chaine_locale() {
    let mut pcm = vec![0.5, 0.3, 0.0, 0.8, 1.0, 1.0];
    chaine_locale_nue(true, false, false, &mut pcm);
    assert_eq!(pcm, vec![0.4, 0.4, 0.4, 0.4, 1.0, 1.0]);
}

/// CONTRE-ÉPREUVE — désarmé (le défaut), la chaîne est l'identité BIT À
/// BIT. C'est l'engagement de l'issue : « comportement actuel strictement
/// inchangé » tant que personne ne coche.
#[test]
fn sans_repli_la_chaine_locale_est_identite_bit_a_bit() {
    let origine = vec![0.5, 0.3, 0.0, 0.8, 1.0, 1.0];
    let mut pcm = origine.clone();
    chaine_locale_nue(false, false, false, &mut pcm);
    assert_eq!(
        pcm.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
        origine.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
    );
}

/// PURE court-circuite le repli comme il court-circuite l'égaliseur et le
/// crossfeed. `zone_mono_downmix` rend déjà `false` en PURE, mais la garde
/// de la chaîne doit tenir seule : c'est elle qui promet le bit-perfect.
#[test]
fn le_mode_pure_court_circuite_le_repli_mono() {
    let origine = vec![0.5, 0.3, 0.0, 0.8];
    let mut pcm = origine.clone();
    chaine_locale_nue(true, true, false, &mut pcm);
    assert_eq!(pcm, origine);
}

/// DoP n'est pas de l'audio : sommer ses voies détruirait le marqueur et le
/// DAC se TAIRAIT (famille de #1408). La garde existante doit couvrir le
/// repli comme elle couvre les trois autres traitements.
#[test]
fn le_dop_court_circuite_le_repli_mono() {
    let origine = vec![0.5, 0.3, 0.0, 0.8];
    let mut pcm = origine.clone();
    chaine_locale_nue(true, false, true, &mut pcm);
    assert_eq!(pcm, origine);
}

/// Le verdict d'exécution doit NOMMER le repli. Sans ceci, le producteur
/// entier de Windows prendrait la branche « octets source conservés » : le
/// réglage serait accepté, la case cochée, et le son inchangé.
#[test]
fn le_repli_mono_casse_l_identite_et_marque_le_dsp_comme_applique() {
    let eq = std::sync::Mutex::new(None);
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);
    let mono = AtomicBool::new(true);

    assert!(!local_dsp_is_identity(
        &eq, &convolver, &crossfeed, &pure, &mono
    ));
    assert_eq!(
        local_dsp_runtime_state(&eq, &convolver, &crossfeed, &pure, &mono, false),
        OutputDspState::Applied
    );

    // Témoin : le même état, repli désarmé, reste une identité inactive.
    mono.store(false, Ordering::Relaxed);
    assert!(local_dsp_is_identity(
        &eq, &convolver, &crossfeed, &pure, &mono
    ));
    assert_eq!(
        local_dsp_runtime_state(&eq, &convolver, &crossfeed, &pure, &mono, false),
        OutputDspState::Inactive
    );
}

fn stereo_sine_8k(frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let v = ((2.0 * std::f64::consts::PI * 8000.0 * i as f64 / 44100.0).sin() * 0.5) as f32;
            [v, v]
        })
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// LE test qui manquait, et que JP Robbe a construit hors branche : la
/// chaîne réelle, pas le moteur isolé.
///
/// Mes onze tests de #2268 portaient sur `Convolver` seul. Ils ne pouvaient
/// pas voir que `apply_local_dsp` envoie immédiatement au `RingBuf` ce
/// qu'il obtient, sans jamais drainer le convolveur : les `block_size`
/// trames retenues n'atteignaient donc jamais le périphérique.
///
/// Le contrat est ici écrit noir sur blanc — une IR identité doit rendre la
/// piste, à condition de drainer la fin.
#[test]
fn une_ir_identite_rend_la_piste_entiere_si_on_draine_la_fin() {
    let bloc = 4usize;
    let ir = vec![vec![1.0f32, 0.0, 0.0, 0.0]; 2]; // identité, stéréo
    let convolver = std::sync::Mutex::new(Some(crate::audio::convolver::Convolver::new(&ir, bloc)));
    let eq = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    // Une piste d'exactement un bloc, en stéréo.
    let piste: Vec<f32> = (0..bloc * 2).map(|i| (i as f32 + 1.0) / 16.0).collect();
    let mut tampon = piste.clone();
    apply_local_dsp(
        &mut tampon,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );

    // Le buffer rendu est le silence d'amorçage : c'est la latence, et
    // c'est exactement ce que l'ancien code ne rendait jamais visible.
    let queue = flush_local_dsp(
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );

    let mut restitue = tampon.clone();
    restitue.extend_from_slice(&queue);
    assert!(
        restitue.len() >= piste.len(),
        "le drainage doit rendre au moins la piste"
    );

    // Quelque part dans ce qui sort, la piste doit se retrouver intacte.
    let debut = restitue.len() - piste.len();
    for (i, attendu) in piste.iter().enumerate() {
        let obtenu = restitue[debut + i];
        assert!(
            (obtenu - attendu).abs() < 1e-4,
            "trame {i} : {obtenu} au lieu de {attendu} — la fin de piste est perdue"
        );
    }
}

/// #2210 — une chaîne gapless peut changer de cadence ou de layout sans
/// repasser par `play_url`. Le moteur de la piste précédente doit alors
/// disparaître ; il ne peut redevenir actif que sur un format compatible.
#[test]
fn un_changement_de_format_gapless_rebat_ou_desactive_le_convolveur() {
    let config = std::sync::Mutex::new(Some(
        crate::audio::convolver::ConvolverConfig::new(vec![vec![1.0, 0.5]], 48_000).unwrap(),
    ));
    let active = std::sync::Mutex::new(None);

    assert!(rebuild_local_convolver(&config, &active, 48_000, 2).unwrap());
    assert_eq!(active.lock().unwrap().as_ref().unwrap().channels(), 2);

    let error = rebuild_local_convolver(&config, &active, 96_000, 2)
        .expect_err("l'IR 48 kHz ne doit jamais corriger un flux 96 kHz");
    assert!(
        error.contains("48000") && error.contains("96000"),
        "{error}"
    );
    assert!(
        active.lock().unwrap().is_none(),
        "l'ancien moteur ne doit pas survivre au changement de cadence"
    );

    assert!(rebuild_local_convolver(&config, &active, 48_000, 6).unwrap());
    assert_eq!(
        active.lock().unwrap().as_ref().unwrap().channels(),
        6,
        "le retour à la cadence de l'IR reconstruit le moteur au nouveau layout"
    );
}

/// Et la frontière de piste : après remise à zéro, plus rien de la
/// précédente. Sans elle, la queue d'une piste repartait dans la suivante.
#[test]
fn la_remise_a_zero_efface_la_queue_de_la_piste_precedente() {
    let bloc = 4usize;
    let ir = vec![vec![1.0f32, 0.0, 0.0, 0.0]; 2];
    let convolver = std::sync::Mutex::new(Some(crate::audio::convolver::Convolver::new(&ir, bloc)));
    let eq = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    // Première piste : un bloc bien reconnaissable, jamais drainé.
    let mut piste1 = vec![1.0f32; bloc * 2];
    apply_local_dsp(
        &mut piste1,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );

    // Frontière.
    reset_local_dsp(&convolver);

    // Seconde piste : du silence. Rien de la première ne doit en sortir.
    let mut piste2 = vec![0.0f32; bloc * 2];
    apply_local_dsp(
        &mut piste2,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );
    for (i, v) in piste2.iter().enumerate() {
        assert!(
            v.abs() < 1e-6,
            "trame {i} : {v} — la queue de la piste precedente a fui"
        );
    }
}

#[test]
fn local_dsp_applies_the_zone_eq() {
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    let mut samples = stereo_sine_8k(4096);
    let before = rms(&samples);
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );
    // On saute les 512 premières trames (établissement du filtre).
    let after = rms(&samples[1024..]);

    let delta_db = 20.0 * (after / before).log10();
    assert!(
        delta_db < -8.0,
        "un plateau -12 dB doit atténuer un 8 kHz ; mesuré {delta_db:.1} dB"
    );
}

#[test]
fn local_dsp_skips_the_eq_in_pure_mode() {
    // PURE promet un chemin bit-perfect : même avec un EQ installé, le
    // signal doit ressortir strictement identique.
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(true);

    let mut samples = stereo_sine_8k(1024);
    let before = samples.clone();
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );
    assert_eq!(samples, before);
}

#[test]
fn local_dsp_without_eq_is_identity() {
    // Aucune zone sans EQ ne doit changer de son : garde-fou de
    // non-régression pour tous les utilisateurs qui n'ont rien activé.
    let eq = std::sync::Mutex::new(None);
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    let mut samples = stereo_sine_8k(1024);
    let before = samples.clone();
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );
    assert_eq!(samples, before);
}

#[test]
fn local_dsp_eq_runs_on_mono_and_multichannel_too() {
    // Le crossfeed est réservé au stéréo ; l'égaliseur, lui, doit agir
    // quel que soit le nombre de canaux (un DAC mono ou 5.1 a droit à sa
    // correction).
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 2000.0,
            gain: -12.0,
            q: 0.71,
            band_type: "high_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let eq = std::sync::Mutex::new(Some(crate::audio::eq::EqProcessor::new(&profile, 44100, 1)));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    let mut samples: Vec<f32> = (0..4096)
        .map(|i| ((2.0 * std::f64::consts::PI * 8000.0 * i as f64 / 44100.0).sin() * 0.5) as f32)
        .collect();
    let before = rms(&samples);
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        1,
        false,
    );
    let after = rms(&samples[1024..]);
    assert!(20.0 * (after / before).log10() < -8.0);
}

/// Génère du DoP réel avec l'encodeur du serveur, pour ne pas tester une
/// idée qu'on se fait du format.
fn real_dop_bytes(frames: usize, channels: usize) -> Vec<u8> {
    let mut enc = crate::audio::dsd_to_dop::DsdToDoP::new(channels, false);
    // 2 octets DSD par canal et par trame DoP.
    let dsd: Vec<u8> = (0..frames * 2 * channels).map(|i| (i * 37) as u8).collect();
    enc.feed(&dsd)
}

fn versioned_dop_fixture() -> Vec<u8> {
    include_str!("../../../tests/fixtures/dop_stereo_24le_64frames.hex")
        .split_ascii_whitespace()
        .map(|octet| u8::from_str_radix(octet, 16).expect("fixture DoP hex valide"))
        .collect()
}

#[test]
fn versioned_dop_fixture_is_the_real_encoder_output_byte_for_byte() {
    let fixture = versioned_dop_fixture();
    assert_eq!(fixture.len(), 64 * 2 * 3);
    assert_eq!(fixture, real_dop_bytes(64, 2));
}

fn runtime_contract(
    native: bool,
    dop: bool,
    volume_units: u32,
    eq: Option<crate::audio::eq::EqProcessor>,
    pure: bool,
) -> OutputSignalPathStatus {
    windows_signal_path_status(
        native,
        dop,
        volume_units,
        &std::sync::Mutex::new(eq),
        &std::sync::Mutex::new(None),
        &std::sync::Mutex::new(None),
        &AtomicBool::new(pure),
        &AtomicBool::new(false),
    )
}

#[test]
fn native_unity_without_dsp_is_reported_bit_perfect() {
    let status = runtime_contract(true, false, 1000, None, false);
    assert!(status.bit_perfect);
    assert_eq!(
        status.sample_transport,
        OutputSampleTransport::NativeInteger
    );
    assert_eq!(status.dsp, OutputDspState::Inactive);
    assert_eq!(status.volume, OutputVolumeState::Unity);
    assert!(status.reasons.is_empty());
}

#[test]
fn float_transport_names_why_it_is_not_bit_perfect() {
    let status = runtime_contract(false, false, 1000, None, false);
    assert!(!status.bit_perfect);
    assert_eq!(status.sample_transport, OutputSampleTransport::Float);
    assert_eq!(status.reasons, vec![OutputSignalReason::FloatTransport]);
}

#[test]
fn native_transport_names_volume_and_dsp_when_they_modify_pcm() {
    let status = runtime_contract(true, false, 420, Some(test_eq()), false);
    assert!(!status.bit_perfect);
    assert_eq!(status.dsp, OutputDspState::Applied);
    assert_eq!(status.volume, OutputVolumeState::Applied);
    assert_eq!(
        status.reasons,
        vec![
            OutputSignalReason::DspApplied,
            OutputSignalReason::SoftwareVolume,
        ]
    );
}

#[test]
fn dop_reports_both_safety_bypasses_and_keeps_native_bits() {
    let status = runtime_contract(true, true, 42, Some(test_eq()), false);
    assert!(status.bit_perfect);
    assert_eq!(status.dsp, OutputDspState::BypassedDop);
    assert_eq!(status.volume, OutputVolumeState::BypassedDop);
    assert!(status.reasons.is_empty());
}

#[test]
fn producer_verdict_cannot_be_upgraded_by_a_later_state_snapshot() {
    let slot = std::sync::Mutex::new(None);
    let status = publish_windows_signal_path_status(
        &slot,
        false,
        true,
        false,
        1000,
        &std::sync::Mutex::new(None),
        &std::sync::Mutex::new(None),
        &std::sync::Mutex::new(None),
        &AtomicBool::new(false),
        &AtomicBool::new(false),
    );

    assert!(!status.bit_perfect);
    assert_eq!(status.reasons, vec![OutputSignalReason::DspStateUnknown]);
    assert_eq!(slot.lock().unwrap().as_ref(), Some(&status));
}

fn integer_pcm_fixture(bit_depth: u16) -> Vec<u8> {
    let bytes_per_sample = usize::from(bit_depth / 8);
    let fixture_line = match bit_depth {
        16 => 0,
        24 => 1,
        32 => 2,
        _ => panic!("profondeur de test non prise en charge"),
    };
    let mut bytes: Vec<u8> =
        include_str!("../../../tests/fixtures/windows_pcm_integer_boundaries.hex")
            .lines()
            .nth(fixture_line)
            .expect("ligne de profondeur présente dans le témoin versionné")
            .split_ascii_whitespace()
            .map(|octet| u8::from_str_radix(octet, 16).expect("fixture PCM hex valide"))
            .collect();
    assert_eq!(bytes.len() % bytes_per_sample, 0);

    // Deterministic pseudo-random bit patterns, including both signs and
    // every low-bit position. We keep the raw two's-complement word rather
    // than generating floats, because the contract is byte identity.
    let mask = if bit_depth == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bit_depth) - 1
    };
    let mut state = 0xD050_05FA_2205_u64;
    for _ in 0..2048 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let word = state & mask;
        bytes.extend_from_slice(&word.to_le_bytes()[..bytes_per_sample]);
    }
    bytes
}

/// Construit un en-tête WAV canonique : `RIFF/WAVE`, un `fmt ` de
/// `fmt_chunk_size` octets, puis un `data` vide. `ext` porte
/// `(wValidBitsPerSample, sous-format)` quand le `fmt ` est assez long.
fn wav_header(
    format_tag: u16,
    channels: u16,
    sample_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
    fmt_chunk_size: u32,
    ext: Option<(u16, u16)>,
) -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&format_tag.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&sample_rate.to_le_bytes());
    fmt.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&bits_per_sample.to_le_bytes());
    if let Some((valid_bits, sub_format)) = ext {
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&valid_bits.to_le_bytes());
        fmt.extend_from_slice(&0x0000_0003u32.to_le_bytes()); // dwChannelMask
        fmt.extend_from_slice(&sub_format.to_le_bytes());
        fmt.extend_from_slice(&[0u8; 14]); // reste du GUID de sous-format
    }
    fmt.resize(fmt_chunk_size as usize, 0);

    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36u32 + fmt_chunk_size).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&fmt_chunk_size.to_le_bytes());
    header.extend_from_slice(&fmt);
    header.extend_from_slice(b"data");
    header.extend_from_slice(&0u32.to_le_bytes());
    header.resize(header.len().max(44), 0);
    header
}

/// Le pas d'avancement que le RESTE du fichier appliquera pour cette
/// profondeur : `bytes_per_sample` tel que la boucle d'alimentation le
/// calcule (`local.rs`, sentinelle 0 = flottant 32 bits).
fn declared_stride(bit_depth: u16) -> usize {
    if bit_depth == 0 {
        4
    } else {
        usize::from(bit_depth / 8)
    }
}

/// Les profondeurs que l'analyseur d'en-tête est autorisé à rendre sont
/// exactement celles que le décodeur d'échantillons sait lire.
///
/// Ce n'est pas un détail de forme. Une profondeur hors de cet ensemble se
/// propage jusqu'à l'ampli : `pcm_bytes_to_f32` retombe sur un pas de deux
/// octets là où l'appelant en a compté `bit_depth / 8`, et la sortie rend
/// du bruit blanc avec la musique derrière.
#[test]
fn un_en_tete_wav_ne_rend_que_des_profondeurs_que_le_decodeur_sait_lire() {
    // Sous-format PCM / IEEE float des en-têtes EXTENSIBLE.
    const SUB_PCM: u16 = 1;
    const SUB_FLOAT: u16 = 3;

    // --- Ce qui doit continuer de passer, à toutes les profondeurs ---
    let acceptes: [(&str, Vec<u8>, u16); 6] = [
        ("PCM 16", wav_header(1, 2, 44_100, 4, 16, 16, None), 16),
        ("PCM 24", wav_header(1, 2, 96_000, 6, 24, 16, None), 24),
        ("PCM 32", wav_header(1, 2, 192_000, 8, 32, 16, None), 32),
        (
            "IEEE float 32",
            wav_header(3, 2, 44_100, 8, 32, 16, None),
            0,
        ),
        (
            "EXTENSIBLE 24 dans 32",
            wav_header(0xFFFE, 2, 384_000, 8, 32, 40, Some((24, SUB_PCM))),
            32,
        ),
        (
            "EXTENSIBLE float 32",
            wav_header(0xFFFE, 2, 44_100, 8, 32, 40, Some((32, SUB_FLOAT))),
            0,
        ),
    ];
    for (etiquette, header, attendu) in acceptes {
        let parsed = parse_wav_header(&header)
            .unwrap_or_else(|| panic!("{etiquette} : en-tête valide refusé"));
        assert_eq!(parsed.2, attendu, "{etiquette} : mauvaise profondeur");
        assert_eq!(parsed.0, 2, "{etiquette} : mauvais nombre de voies");
    }

    // --- Ce qui doit être REFUSÉ plutôt que mal décodé ---
    //
    // `block_align = 2` sur deux voies = un conteneur d'UN octet (WAV
    // 8 bits, licite). L'ancien code rendait 8 : le pas annoncé valait un
    // octet, la lecture en consommait deux.
    //
    // `block_align = 0` est le pire : l'ancien calcul rendait 0, qui est
    // précisément le sentinelle « IEEE float 32 bits ». Du PCM entier
    // aurait été réinterprété comme des flottants.
    let refuses: [(&str, Vec<u8>); 6] = [
        (
            "PCM conteneur 1 octet",
            wav_header(1, 2, 44_100, 2, 8, 16, None),
        ),
        (
            "PCM conteneur nul",
            wav_header(1, 2, 44_100, 0, 16, 16, None),
        ),
        (
            "PCM conteneur 8 octets",
            wav_header(1, 2, 44_100, 16, 64, 16, None),
        ),
        ("PCM zéro voie", wav_header(1, 0, 44_100, 4, 16, 16, None)),
        (
            "EXTENSIBLE conteneur 1 octet",
            wav_header(0xFFFE, 2, 44_100, 2, 8, 40, Some((8, SUB_PCM))),
        ),
        (
            "EXTENSIBLE conteneur 8 octets",
            wav_header(0xFFFE, 2, 44_100, 16, 64, 40, Some((64, SUB_PCM))),
        ),
    ];
    for (etiquette, header) in refuses {
        assert!(
            parse_wav_header(&header).is_none(),
            "{etiquette} : en-tête indécodable accepté — le flux partira en bruit"
        );
    }

    // Un EXTENSIBLE tronqué annonçait `wBitsPerSample` tel quel : 20 bits
    // n'est décodé nulle part. C'est le conteneur qui fait foi.
    let tronque = wav_header(0xFFFE, 2, 96_000, 6, 20, 18, None);
    assert_eq!(
        parse_wav_header(&tronque).map(|p| p.2),
        Some(24),
        "EXTENSIBLE tronqué : la profondeur doit suivre le conteneur"
    );
}

/// Contre-épreuve de bout en bout : pour CHAQUE en-tête que l'analyseur
/// accepte, le décodeur d'échantillons doit consommer exactement le pas que
/// l'analyseur a annoncé.
///
/// C'est l'invariant que la sortie locale suppose partout sans jamais le
/// vérifier — et sa violation est, littéralement, du bruit.
#[test]
fn le_decodeur_consomme_exactement_le_pas_annonce_par_l_en_tete() {
    let candidats: [(&str, Vec<u8>); 9] = [
        ("PCM 16", wav_header(1, 2, 44_100, 4, 16, 16, None)),
        ("PCM 24", wav_header(1, 2, 96_000, 6, 24, 16, None)),
        ("PCM 32", wav_header(1, 2, 192_000, 8, 32, 16, None)),
        ("IEEE float 32", wav_header(3, 2, 44_100, 8, 32, 16, None)),
        (
            "EXTENSIBLE 24 dans 32",
            wav_header(0xFFFE, 2, 384_000, 8, 32, 40, Some((24, 1))),
        ),
        (
            "EXTENSIBLE float 32",
            wav_header(0xFFFE, 2, 44_100, 8, 32, 40, Some((32, 3))),
        ),
        // Les trois pièges. S'ils sont acceptés, l'invariant doit tenir —
        // et il ne tient pas, ce qui est tout l'objet du correctif.
        (
            "PCM conteneur 1 octet",
            wav_header(1, 2, 44_100, 2, 8, 16, None),
        ),
        (
            "PCM conteneur nul",
            wav_header(1, 2, 44_100, 0, 16, 16, None),
        ),
        (
            "EXTENSIBLE tronqué 20 bits",
            wav_header(0xFFFE, 2, 96_000, 6, 20, 18, None),
        ),
    ];

    for (etiquette, header) in candidats {
        let Some((channels, _, bit_depth, _)) = parse_wav_header(&header) else {
            continue; // refusé : il part au décodeur symphonia, pas au DAC.
        };
        let stride = declared_stride(bit_depth);
        assert_ne!(stride, 0, "{etiquette} : pas d'avancement nul");
        assert_ne!(
            channels, 0,
            "{etiquette} : zéro voie, trame de taille nulle"
        );

        // 64 trames d'octets non nuls, alignées sur le pas ANNONCÉ.
        let frame_bytes = usize::from(channels) * stride;
        let pcm: Vec<u8> = (0..frame_bytes * 64).map(|i| (i % 251 + 1) as u8).collect();
        let samples = pcm_bytes_to_f32(&pcm, bit_depth);

        assert_eq!(
            samples.len() * stride,
            pcm.len(),
            "{etiquette} ({bit_depth} bits) : le décodeur consomme {} octets par échantillon \
             alors que l'en-tête en annonce {stride} — chaque trame est lue au mauvais \
             décalage, et la sortie locale rend du bruit",
            pcm.len() as f64 / samples.len().max(1) as f64,
        );
    }
}

/// Épingle « la plus riche l'emporte » — le départage d'avant #3209.
///
/// Ce test reste vrai APRÈS #3209, et ce n'est pas un hasard : ni
/// `alsa:first-48k` ni `alsa:rich-384k` n'est un PCM `hw:`, donc le
/// critère « le matériel d'abord » ne départage rien et l'ancienne règle
/// s'applique mot pour mot. C'est exactement le cas d'une machine où
/// PipeWire est le seul chemin praticable. Il a gagné un sixième argument
/// (`candidate_sample_rates_measured`) parce que la preuve doit désormais
/// basculer avec l'identité et les capacités, puis un septième
/// (`candidate_hardware_detail`, #2272) pour la même raison.
#[test]
fn linux_duplicate_keeps_the_rich_variant_identity_with_its_capabilities() {
    let mut retained = AudioDevice {
        name: "Eversolo DAC-Z8, USB Audio".into(),
        endpoint_id: "alsa:first-48k".into(),
        is_default: false,
        max_channels: 2,
        sample_rates: vec![44_100, 48_000],
        sample_rates_measured: true,
        backend: "ALSA".into(),
        hardware_detail: None,
    };
    assert!(
        !alsa_pcm_is_direct_hardware("alsa:first-48k")
            && !alsa_pcm_is_direct_hardware("alsa:rich-384k"),
        "ce test n'épingle l'ANCIENNE règle que tant qu'aucun des deux PCM \
         n'atteint le matériel ; sinon il vérifierait autre chose que son nom",
    );
    assert!(merge_linux_duplicate_variant(
        &mut retained,
        "alsa:rich-384k".into(),
        true,
        32,
        vec![44_100, 48_000, 96_000, 192_000, 384_000],
        true,
        None,
    ));
    assert_eq!(retained.endpoint_id, "alsa:rich-384k");
    assert_eq!(retained.max_channels, 32);
    assert_eq!(retained.sample_rates.last(), Some(&384_000));
    assert!(retained.is_default);
}

// -----------------------------------------------------------------------
// #3209 / #1655 — sur ALSA, le greffon ne gagne plus contre le matériel.
//
// Le mécanisme : `snd_device_name_hint` rend la même carte sous une
// dizaine de PCM homonymes, le dédoublonnage Linux les regroupe par nom, et
// le départage retenait « le plus riche ». Or un `plug`/`dmix` ACCEPTE
// tout : il annonce 32 voies pour un DAC stéréo et toutes les cadences. Il
// gagnait donc, puis imposait son identité — et alsa-lib rééchantillonnait
// en silence à 48 kHz (`defaults.pcm.dmix.rate 48000`) pendant que Tune
// croyait jouer en natif.
//
// La règle est PURE : elle reçoit des variantes (pcm_id, voies, cadences)
// et rend l'indice de celle qui doit être retenue. Aucun appel à alsa-lib,
// aucun périphérique — elle est donc éprouvable sur une machine sans DAC,
// et sur toutes les cibles, pas seulement Linux.
// -----------------------------------------------------------------------

/// Les cadences qu'un `plug` accepte : toutes. C'est le mensonge du
/// greffon, et il ne doit plus rien gagner.
fn cadences_du_greffon() -> Vec<u32> {
    vec![
        8_000, 16_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
    ]
}

/// Ce que le DAC annonce vraiment : deux voies, 44,1 → 384.
fn cadences_du_materiel() -> Vec<u32> {
    vec![
        44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
    ]
}

/// LE CAS D'OR. `sysdefault:CARD=X` annonce 32 voies et TOUTES les
/// cadences ; `hw:CARD=X` annonce 2 voies et 44,1 → 384. Le greffon est
/// strictement plus riche sur les deux axes — et perd quand même.
#[test]
fn alsa_le_pcm_materiel_bat_le_greffon_meme_plus_riche() {
    let greffon = AlsaVariant {
        endpoint_id: "sysdefault:CARD=DACZ8,DEV=0".into(),
        max_channels: 32,
        sample_rates: cadences_du_greffon(),
    };
    let materiel = AlsaVariant {
        endpoint_id: "hw:CARD=DACZ8,DEV=0".into(),
        max_channels: 2,
        sample_rates: cadences_du_materiel(),
    };
    assert!(
        greffon.max_channels > materiel.max_channels
            && greffon.sample_rates.len() > materiel.sample_rates.len(),
        "le témoin ne vaut que si le greffon est STRICTEMENT plus riche : \
         c'est ce que l'ancienne règle regardait",
    );

    // alsa-lib rend `sysdefault` avant `hw` — mais la règle ne doit pas
    // dépendre de cet ordre, sans quoi elle serait vraie par accident.
    for (etiquette, variantes) in [
        ("greffon en tête", vec![greffon.clone(), materiel.clone()]),
        ("matériel en tête", vec![materiel.clone(), greffon.clone()]),
    ] {
        let indice = retenir_variante_alsa(&variantes)
            .unwrap_or_else(|| panic!("{etiquette} : aucune variante retenue"));
        assert_eq!(
            variantes[indice].endpoint_id, "hw:CARD=DACZ8,DEV=0",
            "{etiquette} : c'est le greffon qui a été retenu — alsa-lib \
             rééchantillonnera un FLAC 44,1 kHz en silence (#1655)",
        );
        assert_eq!(
            variantes[indice].max_channels, 2,
            "{etiquette} : les capacités retenues doivent être celles du \
             DAC, pas les 32 voies inventées par le greffon",
        );
    }
}

/// LE TÉMOIN PIPEWIRE. Plusieurs variantes homonymes SANS aucun `hw:` :
/// le comportement d'avant est conservé, et le regroupement continue de
/// rendre UNE seule variante (43 fantômes → 48 zones chez JeromeQ sur
/// Ubuntu 24.04 — c'est ce que le regroupement par nom a arrêté).
#[test]
fn alsa_sans_pcm_materiel_le_dedoublonnage_pipewire_est_inchange() {
    let variantes = vec![
        AlsaVariant {
            endpoint_id: "sysdefault:CARD=PCH".into(),
            max_channels: 2,
            sample_rates: vec![44_100, 48_000],
        },
        AlsaVariant {
            endpoint_id: "pipewire".into(),
            max_channels: 32,
            sample_rates: vec![44_100, 48_000, 96_000, 192_000],
        },
        AlsaVariant {
            endpoint_id: "pulse".into(),
            max_channels: 2,
            sample_rates: vec![48_000],
        },
        AlsaVariant {
            endpoint_id: "default".into(),
            max_channels: 8,
            sample_rates: vec![44_100, 48_000],
        },
        AlsaVariant {
            endpoint_id: "plughw:CARD=PCH,DEV=0".into(),
            max_channels: 2,
            sample_rates: vec![44_100, 48_000, 96_000],
        },
    ];
    assert!(
        variantes
            .iter()
            .all(|v| !alsa_pcm_is_direct_hardware(&v.endpoint_id)),
        "aucune de ces variantes ne doit atteindre le matériel, sinon le \
         témoin ne parle plus du cas PipeWire",
    );

    // La plus riche l'emporte, exactement comme avant #3209 — et une
    // SEULE variante est retenue, quel que soit l'ordre d'énumération.
    let mut tournee = variantes.clone();
    for _ in 0..variantes.len() {
        tournee.rotate_left(1);
        let indice = retenir_variante_alsa(&tournee).expect("une variante retenue");
        assert_eq!(
            tournee[indice].endpoint_id, "pipewire",
            "le départage PipeWire a changé : une machine où PipeWire est \
             le seul chemin praticable ne doit rien voir de #3209",
        );
    }

    // Et la règle rend UN indice, jamais une liste : le regroupement par
    // nom reste entier. Sans lui, 43 périphériques → 48 zones.
    assert!(retenir_variante_alsa(&[]).is_none());
}

/// L'ÉGALITÉ. Deux variantes de capacités identiques : la règle est
/// déterministe et ne dépend pas de l'ordre d'énumération. Sans le
/// départage par `pcm_id`, c'est la PREMIÈRE énumérée qui gardait tout —
/// et alsa-lib rend `sysdefault` avant `hw`.
#[test]
fn alsa_l_egalite_se_departage_sans_dependre_de_l_ordre() {
    for (etiquette, a, b, attendu) in [
        (
            "deux sorties matérielles de la même carte",
            "hw:CARD=DACZ8,DEV=1",
            "hw:CARD=DACZ8,DEV=0",
            "hw:CARD=DACZ8,DEV=0",
        ),
        (
            "deux greffons homonymes",
            "sysdefault:CARD=PCH,DEV=0",
            "front:CARD=PCH,DEV=0",
            "front:CARD=PCH,DEV=0",
        ),
    ] {
        let fabriquer = |id: &str| AlsaVariant {
            endpoint_id: id.into(),
            max_channels: 2,
            sample_rates: vec![44_100, 48_000],
        };
        for (ordre, variantes) in [
            ("a puis b", vec![fabriquer(a), fabriquer(b)]),
            ("b puis a", vec![fabriquer(b), fabriquer(a)]),
        ] {
            let indice = retenir_variante_alsa(&variantes).expect("une variante retenue");
            assert_eq!(
                variantes[indice].endpoint_id, attendu,
                "{etiquette} ({ordre}) : à capacités égales le vainqueur \
                 dépend encore de l'ordre rendu par alsa-lib",
            );
        }
    }
}

/// Le câblage : quand le PCM matériel l'emporte, l'AudioDevice publié
/// bascule ENTIÈREMENT — identité, capacités, et la preuve qui les
/// accompagne. Publier « 32 voies mesurées » sous l'endpoint d'un `hw:`
/// stéréo serait un troisième périphérique, qui n'existe pas.
#[test]
fn alsa_le_materiel_retenu_emporte_ses_capacites_et_sa_preuve() {
    // L'ordre d'alsa-lib : le greffon est publié en premier, ses cadences
    // ne valent rien (`sample_rates_measured = false`, #1655).
    let mut publie = AudioDevice {
        name: "Eversolo DAC-Z8, USB Audio".into(),
        endpoint_id: "sysdefault:CARD=DACZ8,DEV=0".into(),
        is_default: false,
        max_channels: 32,
        sample_rates: cadences_du_greffon(),
        sample_rates_measured: false,
        backend: "Alsa".into(),
        hardware_detail: None,
    };
    assert!(merge_linux_duplicate_variant(
        &mut publie,
        "hw:CARD=DACZ8,DEV=0".into(),
        false,
        2,
        cadences_du_materiel(),
        true,
        None,
    ));
    assert_eq!(publie.endpoint_id, "hw:CARD=DACZ8,DEV=0");
    assert_eq!(publie.max_channels, 2);
    assert_eq!(publie.sample_rates, cadences_du_materiel());
    assert!(
        publie.sample_rates_measured,
        "les cadences d'un `hw:` SONT mesurées : la preuve doit basculer \
         avec l'identité, pas rester celle du greffon",
    );

    // Et dans l'autre sens : le greffon qui arrive après ne reprend rien
    // au matériel déjà retenu — ni l'identité, ni ses 32 voies inventées.
    let mut publie = AudioDevice {
        name: "Eversolo DAC-Z8, USB Audio".into(),
        endpoint_id: "hw:CARD=DACZ8,DEV=0".into(),
        is_default: false,
        max_channels: 2,
        sample_rates: cadences_du_materiel(),
        sample_rates_measured: true,
        backend: "Alsa".into(),
        hardware_detail: None,
    };
    assert!(!merge_linux_duplicate_variant(
        &mut publie,
        "dmix:CARD=DACZ8,DEV=0".into(),
        true,
        32,
        cadences_du_greffon(),
        false,
        None,
    ));
    assert_eq!(publie.endpoint_id, "hw:CARD=DACZ8,DEV=0");
    assert_eq!(publie.max_channels, 2);
    assert!(publie.sample_rates_measured);
    // `is_default` reste transmis : c'est un fait sur le NOM, pas sur le
    // PCM — le comportement d'avant est conservé.
    assert!(publie.is_default);
}

// -----------------------------------------------------------------------
// #2862 — les « cadences supportées » d'une zone Windows sont fabriquées.
//
// `is_format_supported` de cpal rend `Ok(true)` sans regarder le format sur
// l'hôte WASAPI. `supported_formats()` déroule donc le produit cartésien
// des 21 `COMMON_SAMPLE_RATES` par 7 formats, et deux DAC différents
// reçoivent la même liste. Tune ne peut pas corriger cpal ; il peut cesser
// de présenter cette liste comme une capacité constatée.
//
// Ces tests portent sur des fonctions PURES et sur la charge utile
// sérialisée : ils tournent sur Linux comme sur Windows. La plateforme est
// un paramètre de `sample_rate_evidence`, jamais un `cfg!` interne — sans
// quoi la décision Windows ne serait pas compilée ici et le test serait
// vert pour la mauvaise raison (#1837, #2056).
// -----------------------------------------------------------------------

/// Les hôtes cpal dont on sait ce que fait l'énumération.
const HOTES_CPAL_CONNUS: &[&str] = &["Alsa", "Asio", "CoreAudio", "Jack", "Wasapi"];

#[test]
fn wasapi_ne_presente_plus_ses_cadences_comme_mesurees() {
    assert_eq!(
        sample_rate_evidence("Wasapi"),
        SampleRateEvidence::Unverified,
        "cpal ne teste RIEN sur WASAPI : is_format_supported rend Ok(true) \
         sans regarder le format, et supported_formats() fabrique les 21 \
         cadences. Les annoncer comme mesurées, c'est affirmer ce que \
         personne n'a vérifié (#2862)"
    );

    // Témoin anti-régression : les hôtes qui INTERROGENT réellement le
    // pilote — ALSA par `hw_params.test_rate`, ASIO par
    // `driver.can_sample_rate` — ne bougent pas d'un iota.
    for hote in ["Alsa", "Asio", "CoreAudio", "Jack"] {
        assert!(
            sample_rate_evidence(hote).is_measured(),
            "l'hôte « {hote} » confronte ses cadences au matériel : le \
             rétrograder en « non vérifié » ferait perdre à Linux et macOS \
             une information qu'ils avaient bel et bien"
        );
    }

    // Un hôte qu'on ne connaît pas ne se voit pas PRÊTER une mesure.
    assert!(
        !sample_rate_evidence("UnHoteQuiNExistePasEncore").is_measured(),
        "un backend inconnu doit tomber du côté « non vérifié » : on ne \
         sait pas s'il interroge le pilote, donc on ne l'affirme pas"
    );

    // La casse du nom d'hôte n'est pas un contrat.
    assert!(!sample_rate_evidence("WASAPI").is_measured());
    assert!(sample_rate_evidence("alsa").is_measured());
}

// -----------------------------------------------------------------------
// #1655 — sur ALSA, « le pilote a répondu » ne veut pas dire « le DAC a
// répondu ».
//
// Relevé de GgB du 30/08/2026, sur 0.9.127, pendant la lecture d'un
// fichier 192 kHz / 24 bits (`/proc/asound/card0/stream0`) :
//
//     Momentary freq = 48001 Hz (0x6.0008)
//     Packet Size = 72
//     Rates: 44100, 48000, …, 705600, 768000
//
// `Momentary freq` n'est pas une cadence nominale : c'est `freqm`, le
// compteur de rétroaction de l'endpoint USB, en 16.16 échantillon par
// micro-trame (noyau Linux, `sound/usb/proc.c`, `proc_dump_ep_status` →
// `get_high_speed_hz(x) = (x * 125 + (1 << 9)) >> 10`). 0x6.0008 vaut
// 6,000122 échantillon par micro-trame de 125 µs, soit 48 000 Hz nominal
// dérivant de +0,002 % — la dérive NORMALE d'un endpoint asynchrone
// (`ASYNC`, `Feedback Format = 16.16`) asservi à l'horloge du DAC. Le
// 48001 n'est donc pas le défaut. Le défaut est le 48 000 nominal qu'il
// révèle : un flux 192 kHz demanderait ≈24 échantillons par micro-trame
// (0x18.xxxx), et ne tiendrait pas dans un paquet de 72 octets
// (24 × 2 canaux × 4 octets = 192 octets minimum).
//
// Côté Tune, le journal du même appareil dit `output_sr=192000` et
// `max_channels=32` — or ce DAC est stéréo. 32 est le PLAFOND que cpal
// impose (`cpal-0.17.3` `src/host/alsa/mod.rs:556`) : seul un greffon
// logiciel annonce autant de canaux. Et le journal porte
// `ALSA lib pcm_direct.c … snd1_pcm_direct_slave_recover`, c'est-à-dire
// `dmix` — dont `alsa.conf` fixe l'esclave à `defaults.pcm.dmix.rate
// 48000`. L'écran affiche donc, comme MESURÉES, les cadences qu'un
// rééchantillonneur a bien voulu accepter.
// -----------------------------------------------------------------------

#[test]
fn un_pcm_alsa_convertisseur_ne_peut_pas_annoncer_une_cadence_mesuree() {
    for pcm in [
        "ALSA:default",
        "ALSA:sysdefault:CARD=DACZ8",
        "ALSA:plughw:CARD=DACZ8,DEV=0",
        "ALSA:dmix:CARD=DACZ8,DEV=0",
        "ALSA:front:CARD=DACZ8,DEV=0",
        "ALSA:iec958:CARD=DACZ8,DEV=0",
        "ALSA:pipewire",
        "ALSA:pulse",
    ] {
        assert!(
            !sample_rate_evidence_for_device("Alsa", pcm, true).is_measured(),
            "« {pcm} » n'est pas le DAC : c'est un greffon qui accepte tout \
             et rééchantillonne. Présenter ses cadences comme mesurées, \
             c'est faire dire au matériel ce que le convertisseur a \
             répondu — l'écran de GgB annonce 44,1 → 384 kHz pendant que \
             l'endpoint USB tourne à 48 kHz (#1655)"
        );
    }
}

#[test]
fn le_pcm_materiel_direct_reste_une_mesure_et_la_supposition_jamais() {
    // Témoin : `hw:` atteint le pilote sans conversion. Le rétrograder
    // ferait perdre à Linux une information qu'il avait bel et bien.
    assert!(
        sample_rate_evidence_for_device("Alsa", "ALSA:hw:CARD=DACZ8,DEV=0", true).is_measured()
    );
    // Le préfixe d'hôte est facultatif : le PCM reste lisible sans lui.
    assert!(sample_rate_evidence_for_device("Alsa", "hw:CARD=DACZ8,DEV=0", true).is_measured());
    assert!(alsa_pcm_is_direct_hardware("hw:CARD=DACZ8,DEV=0"));
    assert!(!alsa_pcm_is_direct_hardware("dmix:CARD=DACZ8,DEV=0"));

    // Une liste SUPPOSÉE n'a mesuré personne — quel que soit l'hôte ou le
    // PCM. C'est le drapeau que `probe_device_fallback_caps` calculait et
    // que l'énumération jetait (`let _ = caps_reliable`) : les 40
    // `local_audio_device_fallback_to_assumed_stereo_44100_48000` du
    // journal de GgB étaient publiés comme des mesures.
    assert!(
        !sample_rate_evidence_for_device("Alsa", "ALSA:hw:CARD=DACZ8,DEV=0", false).is_measured()
    );
    assert!(!sample_rate_evidence_for_device("CoreAudio", "CoreAudio:Engine", false).is_measured());

    // Les hôtes non-ALSA ne dépendent pas d'un nom de PCM : leur verdict
    // reste celui de `sample_rate_evidence`, inchangé.
    assert!(sample_rate_evidence_for_device("CoreAudio", "", true).is_measured());
    assert!(sample_rate_evidence_for_device("Asio", "", true).is_measured());
    assert!(!sample_rate_evidence_for_device("Wasapi", "", true).is_measured());
}

/// La table est indexée sur `cpal::HostId::name()`, qui rend le nom de la
/// VARIANTE (`stringify!`) et non un libellé d'affichage. Si cpal renomme
/// une variante, la table devient muette et l'hôte retombe silencieusement
/// dans le cas « inconnu ». Ce test attrape ce glissement sur la machine
/// qui compile, quelle qu'elle soit.
#[test]
fn la_table_est_indexee_sur_le_vrai_nom_d_hote_cpal() {
    // `HostTrait` vient du glob `use super::*` : la re-importer ici est
    // refusee par le `#![deny(unused_imports)]` de la caisse.
    let nom = cpal::default_host().id().name();
    assert!(
        HOTES_CPAL_CONNUS.contains(&nom),
        "cpal rend « {nom} » comme nom d'hôte, absent de la table de \
         sample_rate_evidence {HOTES_CPAL_CONNUS:?}. Tant qu'il n'y figure \
         pas, cet hôte est classé « non vérifié » — ce qui est prudent mais \
         faux s'il interroge le pilote"
    );
}

/// Ce que la zone reçoit vraiment sur `GET /api/v1/devices/audio` : la
/// liste, et désormais ce qu'elle vaut. Sans ce champ, huit cadences
/// inventées et huit cadences constatées sont indiscernables sur le fil.
#[test]
fn la_charge_utile_dit_si_les_cadences_ont_ete_mesurees() {
    let cadences = vec![
        44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
    ];

    let windows = AudioDevice {
        name: "Haut-Parleurs".into(),
        endpoint_id: String::new(),
        is_default: true,
        max_channels: 2,
        sample_rates: cadences.clone(),
        sample_rates_measured: sample_rate_evidence("Wasapi").is_measured(),
        backend: "Wasapi".into(),
        hardware_detail: None,
    };
    let json = serde_json::to_value(&windows).expect("AudioDevice sérialisable");
    assert_eq!(
        json["sample_rates_measured"],
        serde_json::json!(false),
        "la zone Windows annonce jusqu'à 384 kHz sans que rien ne l'ait \
         vérifié : la charge utile doit le dire, sinon l'écran affirme une \
         capacité qu'il ne connaît pas (#2862). Reçu : {json}"
    );
    assert_eq!(
        json["sample_rates"],
        serde_json::json!(cadences),
        "la liste elle-même n'est pas amputée : on la qualifie, on ne la \
         retire pas — la lecture continue de s'appuyer dessus"
    );

    // Témoin : Linux/macOS gardent exactement le sens qu'ils avaient.
    let linux = AudioDevice {
        backend: "Alsa".into(),
        sample_rates_measured: sample_rate_evidence("Alsa").is_measured(),
        ..windows.clone()
    };
    assert_eq!(
        serde_json::to_value(&linux).expect("AudioDevice sérialisable")["sample_rates_measured"],
        serde_json::json!(true),
        "ALSA teste chaque cadence sur le pilote : rien ne doit changer là"
    );

    // Un enregistrement écrit avant ce champ reste lisible.
    let ancien = serde_json::json!({
        "name": "Haut-Parleurs",
        "endpoint_id": "",
        "is_default": true,
        "max_channels": 2,
        "sample_rates": [44_100, 48_000],
        "backend": "Alsa",
    });
    let relu: AudioDevice =
        serde_json::from_value(ancien).expect("le champ ajouté doit avoir un défaut serde");
    assert!(relu.sample_rates_measured);
}

/// Un indice qui n'est pas BRANCHÉ ne vaut rien. Ce test lit le contenu du
/// fichier, comme les gardes voisines, pour attraper le seul retour en
/// arrière qui compte : quelqu'un qui recâble le champ sur une constante,
/// et l'API réannonce « mesuré » sur toutes les plateformes.
///
/// Les aiguilles sont assemblées à l'exécution : écrites en clair, elles
/// figureraient dans ce fichier et le test serait vert grâce à sa propre
/// source.
#[test]
fn l_indice_de_mesure_est_calcule_et_non_ecrit_en_dur() {
    let source = include_str!("../local.rs");

    let champ_derive = ["sample_rates_measured: ", "rates_evidence.is_measured(),"].concat();
    assert!(
        source.contains(&champ_derive),
        "le seul site qui construit un AudioDevice de production doit \
         dériver `sample_rates_measured` de `sample_rate_evidence` ; écrit \
         en dur, il redit « mesuré » sur WASAPI (#2862)"
    );

    let filtre_tautologique = [".filter(|c| c.sample_rate", " == sample_rate)"].concat();
    let apres = source
        .split_once(filtre_tautologique.as_str())
        .expect("la branche « ouvrir à la cadence source » a disparu")
        .1;
    // Decoupe en CARACTERES : ce fichier est accentue, une coupe a
    // l octet pres tomberait au milieu d un caractere et paniquerait.
    let branche: String = apres.chars().take(4_000).collect();
    let appel_indice = ["sample_rate_evidence_for_device(", "host_id_name,"].concat();
    assert!(
        branche.contains(&appel_indice),
        "le site qui décide d'ouvrir à la cadence source doit savoir ce que \
         vaut le « oui » du périphérique. Sur WASAPI le filtre est \
         TAUTOLOGIQUE — find_matching_config recopie la cadence demandée — \
         et sur ALSA le « oui » d'un `dmix:` n'est pas celui du DAC. Sans \
         cet appel, l'indice n'est ni journalisé (#2862, #1655) ni \
         consultable par la décision (#3233)"
    );
    let pcm_journalise = ["endpoint_id = %", "opened_endpoint_id,"].concat();
    assert!(
        branche.contains(&pcm_journalise),
        "sans le nom du PCM ALSA dans le journal, un relevé de terrain ne \
         peut pas dire si Tune a ouvert le matériel ou un \
         rééchantillonneur — c'est la ligne qui manquait à #1655"
    );
}

// -----------------------------------------------------------------------
// #3233 — la décision, pas seulement l'indice
//
// Pierre M (fil forum 1043, 14/07/2026) : « DSD : le temps défile, pas de
// son ». Un DSD64 décode à 176 400 Hz. Le chemin partagé ouvrait à cette
// cadence dès que `find_matching_config(..).filter(|c| c.sample_rate ==
// sample_rate)` répondait — un filtre TAUTOLOGIQUE, puisque
// `find_matching_config` recopie la cadence demandée dans le config qu'il
// rend. Sur WASAPI, cpal retient les 21 COMMON_SAMPLE_RATES sans rien
// demander au pilote, la branche était donc TOUJOURS prise quel que soit le
// matériel, `needs_resample` restait faux et rubato ne tournait jamais.
//
// #2862 avait rendu la LISTE honnête (`sample_rate_evidence`). La DÉCISION
// qui s'en sert n'avait pas bougé.
//
// `decide_local_rate_opening` est PURE et prend la preuve en paramètre :
// ces tests contredisent réellement la décision WASAPI depuis Linux, au
// lieu d'être verts parce que la branche n'y est pas compilée (#1837,
// #2056), et sans le moindre `cfg`.
// -----------------------------------------------------------------------

/// La preuve WASAPI, telle que la machine la calcule vraiment — jamais
/// écrite en dur ici, sinon le test répliquerait la règle au lieu de s'y
/// confronter.
fn preuve_wasapi() -> SampleRateEvidence {
    sample_rate_evidence_for_device("Wasapi", "", true)
}

/// La preuve ALSA d'un PCM matériel — le cas nominal.
fn preuve_alsa_materielle() -> SampleRateEvidence {
    sample_rate_evidence_for_device("Alsa", "ALSA:hw:CARD=DACZ8,DEV=0", true)
}

/// **Essai 1** — capacités fabriquées, cadence que le matériel ne tient
/// pas. Le cas de Pierre M : DSD64 → 176 400 Hz, DAC Windows dont le
/// mélangeur tourne à 48 kHz, et une énumération qui dit « oui » à tout.
#[test]
fn capacites_fabriquees_la_cadence_source_n_est_pas_ouverte_telle_quelle() {
    let decision = decide_local_rate_opening(176_400, Some(48_000), true, preuve_wasapi());
    assert_eq!(
        decision,
        LocalRateOpening::ResampleToDeviceRate {
            device_sample_rate: 48_000,
            reason: LocalRateFallback::CapabilitiesUnverified,
        },
        "l'énumération WASAPI est fabriquée : ouvrir à 176 400 Hz sur cette \
         seule foi, c'est ce qui fait défiler le temps sans un son (#3233). \
         La décision doit refuser la cadence source ET dire pourquoi"
    );
}

/// **Essai 2 — LE TÉMOIN.** Capacités mesurées, cadence retenue :
/// l'ouverture ne change pas d'un iota. Un correctif qui rééchantillonnerait
/// aussi ici serait exactement le défaut que Tune combat.
#[test]
fn temoin_capacites_mesurees_ouvrent_toujours_a_la_cadence_source() {
    assert_eq!(
        decide_local_rate_opening(176_400, Some(48_000), true, preuve_alsa_materielle()),
        LocalRateOpening::AtSourceRateMeasured,
        "ALSA interroge le pilote cadence par cadence sur un PCM `hw:` : ce \
         « oui » est une MESURE, la cadence source doit s'ouvrir telle \
         quelle comme avant (Cyrille, FiiO K3 / iFi Neo iDSD)"
    );
    // La famille entière des hôtes qui mesurent, pas un seul représentant.
    for hote in ["Alsa", "Asio", "CoreAudio", "Jack"] {
        assert_eq!(
            decide_local_rate_opening(
                352_800,
                Some(44_100),
                true,
                sample_rate_evidence_for_device(hote, "hw:CARD=X,DEV=0", true),
            ),
            LocalRateOpening::AtSourceRateMeasured,
            "l'hôte « {hote} » interroge le pilote : rien ne doit changer là"
        );
    }
}

/// **Essai 3** — capacités fabriquées, mais la cadence est RÉELLEMENT
/// tenue : le périphérique y tourne déjà. Ne pas dégrader inutilement.
///
/// C'est le seul fait que WASAPI livre vraiment — `GetMixFormat` décrit le
/// format que le moteur fait tourner, il n'est pas fabriqué.
#[test]
fn capacites_fabriquees_mais_cadence_tenue_n_est_pas_degradee() {
    assert_eq!(
        decide_local_rate_opening(176_400, Some(176_400), true, preuve_wasapi()),
        LocalRateOpening::DeviceAlreadyAtSourceRate,
        "le périphérique TOURNE à 176 400 Hz : lui imposer un aller-retour \
         par rubato dégraderait sans rien corriger"
    );
    // Et même sans le « oui » de l'énumération : le fait prime sur la liste.
    assert_eq!(
        decide_local_rate_opening(176_400, Some(176_400), false, preuve_wasapi()),
        LocalRateOpening::DeviceAlreadyAtSourceRate,
    );
}

/// Le cas constaté — le périphérique ne retient pas la cadence — garde son
/// comportement de toujours, et gagne un motif distinct du précédent.
#[test]
fn cadence_non_retenue_reste_un_reechantillonnage_mais_nomme() {
    for preuve in [preuve_wasapi(), preuve_alsa_materielle()] {
        assert_eq!(
            decide_local_rate_opening(176_400, Some(48_000), false, preuve),
            LocalRateOpening::ResampleToDeviceRate {
                device_sample_rate: 48_000,
                reason: LocalRateFallback::RateNotSupported,
            },
            "un refus CONSTATÉ n'est pas une capacité SUPPOSÉE : les deux \
             motifs doivent rester distincts sur l'écran de la zone"
        );
    }
}

/// Sans cadence de périphérique, il n'y a **rien vers quoi**
/// rééchantillonner : refuser la cadence source ne ferait que casser la
/// lecture. Le dernier recours ouvre donc à la cadence source, comme avant
/// (PipeWire, énumération muette). Aucune régression n'est introduite là —
/// et c'est la limite assumée du correctif : quand le périphérique ne dit
/// rien du tout, Tune n'a plus aucun fait sur lequel s'appuyer.
#[test]
fn sans_cadence_de_peripherique_le_dernier_recours_est_inchange() {
    assert_eq!(
        decide_local_rate_opening(176_400, None, true, preuve_wasapi()),
        LocalRateOpening::LastResortSourceRate,
        "capacités supposées ET aucune cadence connue : dégrader vers rien \
         n'a pas de sens, on ouvre comme avant"
    );
    for enumere in [true, false] {
        assert_eq!(
            decide_local_rate_opening(176_400, None, enumere, preuve_wasapi()),
            LocalRateOpening::LastResortSourceRate,
        );
    }
    assert_eq!(
        decide_local_rate_opening(176_400, None, false, preuve_alsa_materielle()),
        LocalRateOpening::LastResortSourceRate,
    );
    // Témoin : une énumération MESURÉE qui retient la cadence reste une
    // ouverture à la cadence source, même sans configuration par défaut —
    // c'est exactement ce que faisait le code d'avant.
    assert_eq!(
        decide_local_rate_opening(176_400, None, true, preuve_alsa_materielle()),
        LocalRateOpening::AtSourceRateMeasured,
        "un « oui » mesuré n'a pas besoin d'une cadence par défaut pour \
         valoir : rien ne doit changer sur ALSA `hw:` / ASIO / CoreAudio"
    );
}

/// Contre-épreuve PERMANENTE (leçon #1864) : chaque motif déclaré doit être
/// réellement PRODUIT par la fonction de décision. Un motif ajouté à
/// l'énumération sans être câblé fait tomber ce test.
#[test]
fn chaque_motif_de_cadence_est_reellement_produit() {
    let mut produits: Vec<LocalRateFallback> = Vec::new();
    for source in [44_100u32, 176_400, 352_800] {
        for defaut in [None, Some(44_100u32), Some(48_000), Some(176_400)] {
            for enumere in [true, false] {
                for preuve in [preuve_wasapi(), preuve_alsa_materielle()] {
                    if let LocalRateOpening::ResampleToDeviceRate { reason, .. } =
                        decide_local_rate_opening(source, defaut, enumere, preuve)
                    {
                        produits.push(reason);
                    }
                }
            }
        }
    }
    for motif in LocalRateFallback::ALL {
        assert!(
            produits.contains(&motif),
            "le motif {motif:?} est déclaré mais AUCUN chemin de décision ne \
             le construit — il ne gardera jamais rien"
        );
    }
}

/// Les motifs sont ce que le client reçoit et traduit : codes distincts,
/// en `snake_case`, et un texte qui dit ce qui a changé pour le son.
#[test]
fn tous_les_motifs_de_cadence_ont_un_code_et_un_texte_utilisables() {
    let mut codes: Vec<&str> = Vec::new();
    for motif in LocalRateFallback::ALL {
        let code = motif.code();
        assert!(
            !code.is_empty()
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
            "code « {code} » inutilisable comme clef de traduction"
        );
        assert!(!codes.contains(&code), "code « {code} » en double");
        codes.push(code);
        assert!(
            motif.detail().len() > 20,
            "le motif {motif:?} doit expliquer ce qui change pour le son"
        );
    }
}

/// Ce que la zone REÇOIT : la cadence ouverte à côté de celle de la source.
/// Sans ce champ, Pierre M lit « DSD64 » pendant que Tune convertit, et il
/// faut ses journaux pour le savoir.
#[test]
fn le_statut_de_zone_porte_la_cadence_reellement_ouverte() {
    let degrade = backend_status_with_rate(
        None,
        None,
        Some(ObservedRate {
            source_sample_rate: 176_400,
            opened_sample_rate: 48_000,
            reason: Some(LocalRateFallback::CapabilitiesUnverified),
            evidence_measured: false,
        }),
        "auto",
    );
    let rate = degrade.rate.expect("la cadence observée doit être publiée");
    assert!(rate.resampled, "176 400 → 48 000 est une conversion");
    assert_eq!(rate.reason, Some(LocalRateFallback::CapabilitiesUnverified));
    assert_eq!(
        rate.detail,
        Some(LocalRateFallback::CapabilitiesUnverified.detail()),
        "l'écran sans table de traduction doit avoir la phrase"
    );
    assert!(!rate.evidence_measured);
    let json = serde_json::to_value(&degrade).expect("LocalBackendStatus sérialisable");
    assert_eq!(json["rate"]["resampled"], serde_json::json!(true));
    assert_eq!(
        json["rate"]["reason"],
        serde_json::json!("capabilities_unverified"),
        "le code doit partir en snake_case, comme fallback_reason : {json}"
    );

    // Témoin : une ouverture à la cadence source ne signale aucun écart.
    let intact = backend_status_with_rate(
        None,
        None,
        Some(ObservedRate {
            source_sample_rate: 176_400,
            opened_sample_rate: 176_400,
            reason: None,
            evidence_measured: true,
        }),
        "auto",
    );
    let rate = intact.rate.expect("la cadence observée doit être publiée");
    assert!(!rate.resampled);
    assert_eq!(rate.reason, None);
    assert_eq!(rate.detail, None);

    // Rien n'a encore joué : le champ existe et vaut `null`, ce qui est la
    // seule réponse honnête — comme `device` (#2207).
    assert!(
        backend_status_with_rate(None, None, None, "auto")
            .rate
            .is_none()
    );
}

/// GARDE DE SITE — la fonction peut être parfaite et n'être appelée nulle
/// part, ou être court-circuitée par le filtre d'origine remis devant elle.
/// Ce test relit la PRODUCTION (idiome du dépôt : `terminologie_eq.rs`,
/// `position_publiee_guard`), parce qu'aucun test d'unité ne peut attraper
/// ça : `decide_local_rate_opening` resterait verte pendant que le chemin
/// réel l'ignore.
///
/// Les aiguilles sont assemblées à l'exécution : écrites en clair, elles
/// figureraient dans ce fichier et le test serait vert grâce à sa propre
/// source.
#[test]
fn la_decision_de_cadence_est_branchee_sur_le_chemin_reel() {
    // Source normalisée : on retire tous les blancs, pour que la garde
    // survive à un passage de rustfmt qui recasserait les lignes — même
    // idiome que `les_quatre_charges_utiles_de_zone_appellent_le_contrat`.
    let source: String = include_str!("../local.rs")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    // L'APPEL de production, avec ses arguments : la signature seule ne
    // prouverait rien, et un appel de test non plus.
    let appel_reel = [
        "decide_local_rate_opening(sample_rate,default_sr,",
        "enumerated.is_some(),rate_evidence,)",
    ]
    .concat();
    assert!(
        source.contains(&appel_reel),
        "le chemin cpal partagé doit passer par `decide_local_rate_opening` \
         en lui donnant la cadence de la source, celle du périphérique, la \
         réponse de l'énumération ET la preuve qui dit ce qu'elle vaut. \
         Sans la preuve, la décision redit oui à une liste fabriquée \
         (#3233, Pierre M, fil 1043)"
    );

    // Le retour en arrière EXACT : réutiliser le résultat du filtre comme
    // CONDITION d'ouverture, au lieu de le passer à la décision.
    let court_circuit = [
        "}elseifletSome(cfg)=find_matching_config(&device,channels,sample_rate)",
        ".filter(|c|c.sample_rate==sample_rate)",
    ]
    .concat();
    assert!(
        !source.contains(&court_circuit),
        "le filtre tautologique est redevenu la CONDITION d'ouverture : \
         `find_matching_config` recopie la cadence demandée dans le config \
         qu'il rend, donc sur WASAPI la branche est prise quel que soit le \
         matériel, `needs_resample` reste faux et rubato ne tourne jamais"
    );

    // La conversion décidée doit être remontée, pas seulement journalisée.
    assert!(
        source.contains("note_rate_decision(ObservedRate{"),
        "une décision qui change ce qui part au DAC doit atteindre le \
         client : sans `note_rate_decision`, il ne reste que le journal — \
         c'est exactement ce qui a coûté #1395 et #2207"
    );
}

// -----------------------------------------------------------------------
// #2868 — Windows sans feature `asio` : le mode exclusif EXISTE
//
// Même famille que #2862 : ce que l'écran dit d'une capacité ne
// correspondait pas à ce que le binaire porte. La branche WASAPI exclusive
// vit sous `#[cfg(target_os = "windows")]` SEUL ; `supports_exclusive_mode`
// exigeait pourtant `feature = "asio"` pour l'admettre.
//
// Ces tests portent sur une fonction PURE dont la plateforme est un
// paramètre : ils contredisent réellement la décision Windows depuis Linux,
// au lieu d'être verts parce que la branche n'y est pas compilée (#1837,
// #2056).
// -----------------------------------------------------------------------

#[test]
fn windows_sans_asio_garde_son_mode_exclusif_wasapi() {
    let sans_asio = exclusive_mode_support("windows", false);
    assert!(
        sans_asio.wasapi,
        "`WasapiExclusiveOutput` est gardé par `#[cfg(target_os = \
         \"windows\")]` seul et se prend dès que `exclusive_mode && \
         audio_backend != \"asio\"` : la feature `asio` ne le conditionne \
         pas (#2868)"
    );
    assert!(
        !sans_asio.asio,
        "sans la feature, `AsioExclusiveOutput` n'est pas dans le binaire"
    );
    assert!(
        sans_asio.any(),
        "c'est le cœur de #2868 : un Windows bâti sans `asio` se voyait \
         répondre « mode exclusif non supporté » alors que le chemin WASAPI \
         exclusif était compilé et fonctionnel"
    );

    // Avec la feature, les DEUX chemins coexistent : ASIO ne remplace pas
    // WASAPI exclusif, il s'y ajoute (le choix se fait sur `audio_backend`).
    let avec_asio = exclusive_mode_support("windows", true);
    assert!(avec_asio.asio && avec_asio.wasapi);
    assert!(avec_asio.any());
}

/// Témoin anti-régression : les deux plateformes qui répondaient JUSTE
/// avant #2868 doivent répondre exactement pareil après.
#[test]
fn macos_et_linux_ne_bougent_pas_d_un_iota() {
    for asio in [false, true] {
        assert!(
            exclusive_mode_support("macos", asio).coreaudio,
            "macOS ouvre le hog mode CoreAudio sous `#[cfg(target_os = \
             \"macos\")]`, sans condition de feature"
        );
        assert!(exclusive_mode_support("macos", asio).any());

        assert!(
            !exclusive_mode_support("linux", asio).any(),
            "aucune branche exclusive n'est compilée pour Linux : \
             l'annoncer serait la promesse fantôme que #2868 corrige, à \
             l'envers. `asio = {asio}` n'y change rien — la garde de la \
             branche ASIO exige `target_os = \"windows\"` EN PLUS de la \
             feature"
        );
    }

    // Une cible dont personne n'a écrit la branche ne se voit pas prêter
    // un chemin exclusif.
    assert!(!exclusive_mode_support("freebsd", true).any());
    assert!(!exclusive_mode_support("", true).any());
}

/// `std::env::consts::OS` est le seul vocabulaire que comprend
/// `exclusive_mode_support`. S'il rendait « Windows » ou « win32 », le
/// correctif retomberait silencieusement dans le bras « inconnu » et
/// n'aurait plus aucun effet — exactement le piège de casse relevé sur
/// `HostId::name()` en #2862. Ce test l'attrape sur la machine qui compile.
#[test]
fn le_nom_de_systeme_lu_est_celui_que_la_table_indexe() {
    let os = std::env::consts::OS;
    assert!(
        ["linux", "macos", "windows"].contains(&os),
        "`std::env::consts::OS` rend « {os} », absent de la table de \
         exclusive_mode_support. Tant qu'il n'y figure pas, cette cible est \
         classée « sans mode exclusif » — prudent, mais faux si elle porte \
         une branche"
    );
    assert_eq!(
        LocalOutput::supports_exclusive_mode(),
        exclusive_mode_support(os, cfg!(feature = "asio")).any(),
        "le prédicat public doit dire ce que dit la fonction pure, sinon \
         la preuve ci-dessus ne parle pas du code appelé"
    );
}

/// La règle ne doit pas se recomposer en `cfg!` dans le corps du prédicat :
/// elle redeviendrait invisible depuis Linux, et #2868 pourrait revenir
/// sans qu'aucun test ne rougisse.
///
/// Les aiguilles sont assemblées à l'exécution — écrites en clair, elles
/// figureraient dans ce fichier et le test serait vert grâce à sa propre
/// source.
#[test]
fn le_predicat_de_mode_exclusif_ne_se_recompose_pas_en_cfg() {
    let source = include_str!("../local.rs");

    let entete = ["pub fn supports_exclusive_mode()", " -> bool {"].concat();
    let apres = source
        .split_once(entete.as_str())
        .expect("le prédicat public de mode exclusif a disparu")
        .1;
    // Découpe en CARACTÈRES : ce fichier est accentué, une coupe à l'octet
    // près tomberait au milieu d'un caractère et paniquerait.
    let corps: String = apres.chars().take(200).collect();

    let appel = ["exclusive_mode_support(", "std::env::consts::OS"].concat();
    assert!(
        corps.contains(&appel),
        "le prédicat doit déléguer à la fonction pure en lui PASSANT le \
         système ; reçu : {corps}"
    );
    let cfg_os = ["cfg!(target_os", " = "].concat();
    assert!(
        !corps.contains(&cfg_os),
        "un `cfg!(target_os …)` ici enferme à nouveau la décision Windows \
         dans une branche non compilée ailleurs : c'est exactement ce qui a \
         laissé passer #2868. Reçu : {corps}"
    );
}

/// Les noms que `cpal::Host::id().name()` rend sur Windows. Ce sont les
/// mêmes chaînes que l'énumération stocke dans `AudioDevice::backend` et
/// que la résolution apparie — un seul et même vocabulaire.
const HOTE_WASAPI: &str = "Wasapi";
const HOTE_ASIO: &str = "Asio";

/// Deux DAC USB qui s'annoncent tous deux « Haut-Parleurs », plus une
/// sortie HDMI. C'est la configuration d'Alain (#1084) et celle de Marco
/// Polo (#2272).
fn homonymes_fixture() -> Vec<DeviceIdentity> {
    vec![
        DeviceIdentity {
            endpoint_id: "{ugreen}".into(),
            raw_name: "Haut-Parleurs".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{topping}".into(),
            raw_name: "Haut-Parleurs".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{hdmi}".into(),
            raw_name: "Téléviseur".into(),
            host: HOTE_WASAPI.into(),
        },
    ]
}

/// Le nom d'affichage n'est pas une identité. Ce test tient les deux bouts
/// du même défaut : deux homonymes doivent rester **distincts** (#2272), et
/// une zone doit continuer de désigner **le bon** après un redémarrage ou
/// un rebranchement qui renomme le périphérique (#2269).
#[test]
fn deux_homonymes_restent_distincts_et_la_zone_survit_au_renommage() {
    let enumeres = homonymes_fixture();

    // 1. Les homonymes ne se confondent pas. Le suffixe « (2) » que la
    //    découverte a posé désigne le SECOND, jamais le premier.
    assert_eq!(
        resolve_device("Haut-Parleurs", None, None, HOTE_WASAPI, &enumeres),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
        "« Haut-Parleurs » doit désigner le premier homonyme"
    );
    assert_eq!(
        resolve_device("Haut-Parleurs (2)", None, None, HOTE_WASAPI, &enumeres),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)),
        "« Haut-Parleurs (2) » doit désigner le SECOND homonyme, \
         pas retomber sur le premier par sous-chaîne"
    );

    // 2. Redémarrage : Windows n'énumère pas dans le même ordre. Le rang
    //    « (2) » ment désormais ; l'identifiant stable, lui, dit vrai.
    let apres_redemarrage = vec![
        enumeres[1].clone(),
        enumeres[0].clone(),
        enumeres[2].clone(),
    ];
    assert_eq!(
        resolve_device(
            "Haut-Parleurs (2)",
            Some("{topping}"),
            None,
            HOTE_WASAPI,
            &apres_redemarrage
        ),
        DeviceResolution::Matched(DeviceMatch::ByEndpointId(0)),
        "après réordonnancement, l'identifiant stable doit primer sur le rang"
    );

    // 3. Rebranchement : le pilote renomme l'endpoint au changement de
    //    taux d'échantillonnage (DEvir, #2269). Plus aucun nom ne
    //    correspond — l'identifiant le retrouve quand même.
    let apres_rebranchement = vec![
        DeviceIdentity {
            endpoint_id: "{ugreen}".into(),
            raw_name: "Haut-Parleurs".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{topping}".into(),
            raw_name: "Topping D10s (96 kHz)".into(),
            host: HOTE_WASAPI.into(),
        },
    ];
    assert_eq!(
        resolve_device(
            "Haut-Parleurs (2)",
            Some("{topping}"),
            None,
            HOTE_WASAPI,
            &apres_rebranchement
        ),
        DeviceResolution::Matched(DeviceMatch::ByEndpointId(1)),
        "un renommage ne doit pas casser une zone qui connaît son endpoint"
    );

    // 4. Même scène, mais sans identifiant stable (hôte qui n'en expose
    //    pas, ou zone créée avant #2207). On préfère l'aveu d'ignorance —
    //    donc un repli SIGNALÉ — au premier « Haut-Parleurs » venu choisi
    //    en silence.
    assert_eq!(
        resolve_device(
            "Haut-Parleurs (2)",
            None,
            None,
            HOTE_WASAPI,
            &apres_rebranchement
        ),
        DeviceResolution::NotFound,
        "sans identifiant, un « (2) » orphelin ne doit PAS glisser sur le (1)"
    );

    // 5. La tolérance par sous-chaîne reste acquise aux hôtes verbeux,
    //    tant qu'elle ne désigne qu'une seule candidate.
    let coreaudio = vec![
        DeviceIdentity {
            endpoint_id: "{builtin}".into(),
            raw_name: "MacBook Pro Speakers".into(),
            host: "CoreAudio".into(),
        },
        DeviceIdentity {
            endpoint_id: "{hdmi}".into(),
            raw_name: "Téléviseur".into(),
            host: "CoreAudio".into(),
        },
    ];
    assert_eq!(
        resolve_device(
            "MacBook Pro Speakers (2)",
            None,
            None,
            "CoreAudio",
            &coreaudio
        ),
        DeviceResolution::NotFound,
        "un suffixe posé par Tune ne se rattrape pas par sous-chaîne"
    );
    assert_eq!(
        resolve_device("MacBook Pro", None, None, "CoreAudio", &coreaudio),
        DeviceResolution::Matched(DeviceMatch::BySubstring(0)),
        "un nom tronqué sans ambiguïté reste résolu"
    );
}

/// Une sous-chaîne qui désigne deux candidates ne désigne rien : choisir
/// la première, c'est rejouer le bug de #2272 sous un autre nom.
#[test]
fn une_sous_chaine_ambigue_ne_choisit_pas_a_notre_place() {
    let ambigus = vec![
        DeviceIdentity {
            endpoint_id: "{a}".into(),
            raw_name: "Realtek Digital Output".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{b}".into(),
            raw_name: "Realtek Digital Output (Optical)".into(),
            host: HOTE_WASAPI.into(),
        },
    ];
    assert_eq!(
        resolve_device("Realtek", None, None, HOTE_WASAPI, &ambigus),
        DeviceResolution::NotFound
    );
}

/// L'appariement par nom doit employer la convention `(n)` **exacte** de
/// la découverte, pas un compteur d'occurrences : sur `["A", "A (2)", "A"]`
/// les deux algorithmes divergent, et un troisième périphérique volerait
/// la zone du deuxième.
#[test]
fn la_convention_de_suffixe_est_celle_de_la_decouverte() {
    let colision = vec![
        DeviceIdentity {
            endpoint_id: "{a1}".into(),
            raw_name: "A".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{a2}".into(),
            raw_name: "A (2)".into(),
            host: HOTE_WASAPI.into(),
        },
        DeviceIdentity {
            endpoint_id: "{a3}".into(),
            raw_name: "A".into(),
            host: HOTE_WASAPI.into(),
        },
    ];
    assert_eq!(
        resolve_device("A (2)", None, None, HOTE_WASAPI, &colision),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)),
        "« A (2) » est le nom BRUT du deuxième, il lui appartient"
    );
    assert_eq!(
        resolve_device("A (3)", None, None, HOTE_WASAPI, &colision),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(2)),
        "le troisième reçoit « A (3) », comme à la découverte"
    );
}

// -----------------------------------------------------------------------
// #3230 — un nom porte l'hôte dont il vient
//
// Jean Valjean (forum 893, v0.8.235) : sa zone « Haut-parleurs » est un nom
// WASAPI. `select_host("asio")` élit l'hôte ASIO dès qu'il expose une
// sortie, la résolution cherche ce nom parmi les seules sorties ASIO, ne le
// trouve pas, et ouvrait **le périphérique ASIO par défaut**.
//
// La branche `select_host("asio")` ne se compile que sous Windows avec le
// SDK Steinberg. La DÉCISION, elle, est ici : une fonction pure qui reçoit
// l'hôte d'origine, l'hôte ouvert et les candidates. Elle s'éprouve donc
// sur n'importe quelle plateforme — Shrek compris.
// -----------------------------------------------------------------------

/// Ce que l'hôte ASIO expose chez Jean Valjean : sa carte, et rien qui
/// s'appelle « Haut-parleurs ».
fn sorties_asio_fixture() -> Vec<DeviceIdentity> {
    vec![
        DeviceIdentity {
            // Un pilote ASIO n'expose aucun identifiant d'endpoint stable :
            // `cpal::Device::id()` y échoue, d'où la chaîne vide.
            endpoint_id: String::new(),
            raw_name: "RME Fireface UCX".into(),
            host: HOTE_ASIO.into(),
        },
        DeviceIdentity {
            endpoint_id: String::new(),
            raw_name: "ASIO4ALL v2".into(),
            host: HOTE_ASIO.into(),
        },
    ]
}

/// ESSAI 1 — hôte ASIO élu, nom demandé venant de WASAPI : **refus**.
///
/// Le fait à tenir n'est pas « ça ne trouve rien » : c'est que le verdict
/// est distinct de « introuvable ». `NotFound` fait retomber l'appelant sur
/// la sortie par défaut de l'hôte ouvert — le détournement même de #3230.
#[test]
fn un_nom_wasapi_ne_s_apparie_pas_a_une_sortie_asio() {
    let asio = sorties_asio_fixture();

    let verdict = resolve_device("Haut-parleurs", None, Some(HOTE_WASAPI), HOTE_ASIO, &asio);

    assert_eq!(
        verdict,
        DeviceResolution::ForeignHost {
            requested_host: HOTE_WASAPI.into(),
            open_host: HOTE_ASIO.into(),
        },
        "un nom WASAPI présenté à un hôte ASIO doit être REFUSÉ, pas déclaré \
         introuvable : « introuvable » autorise le repli sur le défaut ASIO, \
         et c'est ce repli-là qui a détourné la zone de Jean Valjean (#3230)"
    );
    assert!(
        !matches!(verdict, DeviceResolution::Matched(_)),
        "aucun appariement ne doit survivre au changement d'hôte"
    );

    // Et la sous-chaîne ne doit pas non plus servir de porte dérobée : un
    // nom d'hôte étranger est écarté AVANT les trois étapes d'appariement.
    assert_eq!(
        resolve_device("ASIO4ALL", None, Some(HOTE_WASAPI), HOTE_ASIO, &asio),
        DeviceResolution::ForeignHost {
            requested_host: HOTE_WASAPI.into(),
            open_host: HOTE_ASIO.into(),
        },
        "même un nom qui RESSEMBLE à une sortie ASIO ne s'apparie pas s'il \
         est présenté comme venant de WASAPI"
    );

    // Et le refus tient même quand l'hôte ouvert n'énumère RIEN : un pilote
    // ASIO happé par une autre application entre l'élection de l'hôte et
    // l'ouverture ne doit pas faire disparaître le refus. C'est pour cela
    // que l'hôte ouvert est un paramètre, et non une déduction sur la liste.
    assert_eq!(
        resolve_device("Haut-parleurs", None, Some(HOTE_WASAPI), HOTE_ASIO, &[]),
        DeviceResolution::ForeignHost {
            requested_host: HOTE_WASAPI.into(),
            open_host: HOTE_ASIO.into(),
        },
        "une énumération vide ne doit pas rendre le nom étranger appariable"
    );
}

/// ESSAI 2 — hôte ASIO élu, nom demandé venant d'ASIO : ouverture normale.
#[test]
fn un_nom_asio_s_apparie_bien_a_sa_sortie_asio() {
    let asio = sorties_asio_fixture();

    assert_eq!(
        resolve_device("RME Fireface UCX", None, Some(HOTE_ASIO), HOTE_ASIO, &asio),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
        "un nom qui vient de l'hôte ouvert doit s'apparier comme avant"
    );
    assert_eq!(
        resolve_device("ASIO4ALL v2", None, Some(HOTE_ASIO), HOTE_ASIO, &asio),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)),
    );
    // La casse du nom d'hôte ne doit pas décider du son qui sort.
    assert_eq!(
        resolve_device("RME Fireface UCX", None, Some("ASIO"), HOTE_ASIO, &asio),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
        "l'appariement d'hôte est insensible à la casse"
    );
}

/// ESSAI 3 — LE TÉMOIN. Une machine à un seul hôte se comporte **exactement**
/// comme avant : l'hôte d'origine y est toujours celui qui est ouvert.
///
/// Le témoin compare terme à terme le verdict avec origine connue et le
/// verdict sans origine — c'est-à-dire le comportement d'avant le
/// correctif. Le moindre écart ici serait une régression pour l'immense
/// majorité des installations, qui n'ont jamais vu deux hôtes.
#[test]
fn temoin_un_seul_hote_ne_change_rien() {
    let enumeres = homonymes_fixture();

    for demande in [
        "Haut-Parleurs",
        "Haut-Parleurs (2)",
        "Téléviseur",
        "Haut-Parleurs (9)",
        "inexistant",
    ] {
        let avant = resolve_device(demande, None, None, HOTE_WASAPI, &enumeres);
        let apres = resolve_device(demande, None, Some(HOTE_WASAPI), HOTE_WASAPI, &enumeres);
        assert_eq!(
            avant, apres,
            "« {demande} » : sur une machine à un seul hôte, connaître \
             l'hôte d'origine ne doit RIEN changer"
        );
    }

    // Idem pour l'appariement par identifiant d'endpoint, qui passe devant
    // le nom : le filtre d'hôte ne doit pas l'écarter.
    assert_eq!(
        resolve_device(
            "nom devenu faux",
            Some("{topping}"),
            None,
            HOTE_WASAPI,
            &enumeres
        ),
        resolve_device(
            "nom devenu faux",
            Some("{topping}"),
            Some(HOTE_WASAPI),
            HOTE_WASAPI,
            &enumeres
        ),
        "l'identifiant stable doit primer de la même façon avec ou sans hôte"
    );

    // Et une zone d'AVANT ce correctif — qui ne connaît pas son hôte
    // d'origine — n'est jamais refusée : `None` veut dire « je ne sais
    // pas », et on ne refuse pas sur une ignorance.
    assert_eq!(
        resolve_device(
            "Haut-parleurs",
            None,
            None,
            HOTE_ASIO,
            &sorties_asio_fixture()
        ),
        DeviceResolution::NotFound,
        "sans origine connue, on retombe sur le comportement d'avant : \
         introuvable, donc repli SIGNALÉ — jamais un refus"
    );
}

/// ESSAI 4 — le repli est observable par le client.
///
/// Le canal est celui de #2207 : `LocalBackendStatus.device`, déjà servi
/// par `/system/config`. Ce correctif n'en ouvre pas un deuxième — il y
/// ajoute le motif, avec le même vocabulaire que `fallback_reason` du
/// backend : un `code()` stable et un `detail()` en clair.
#[test]
fn le_repli_de_peripherique_remonte_au_client() {
    // a) Refus d'hôte étranger : RIEN n'a été ouvert. `opened` est vide —
    //    nommer un périphérique ici serait mentir — et le motif le dit.
    let refus = LocalDeviceStatus::from_observed(ObservedDevice {
        backend: "ASIO",
        requested: "Haut-parleurs".into(),
        opened: String::new(),
        opened_id: None,
        reason: Some(LocalDeviceFallback::ForeignHost),
    });
    assert!(refus.differs, "un refus est un écart, et doit se voir");
    assert_eq!(refus.reason, Some(LocalDeviceFallback::ForeignHost));
    assert_eq!(
        refus.detail,
        Some(LocalDeviceFallback::ForeignHost.detail())
    );
    assert!(
        refus.opened.is_empty(),
        "un refus n'ouvre rien : aucun nom de périphérique ne doit être avancé"
    );

    // La charge utile porte bien le code stable, pas le nom de la variante.
    let json = serde_json::to_value(&refus).expect("sérialisable");
    assert_eq!(json["reason"], "foreign_host");
    assert_eq!(json["differs"], true);

    // b) Le cas historique #2207 : le nom est introuvable sur le BON hôte,
    //    on ouvre la sortie système — et on le dit désormais avec un motif.
    let repli = LocalDeviceStatus::from_observed(ObservedDevice {
        backend: "WASAPI",
        requested: "Topping D10s".into(),
        opened: "Haut-parleurs".into(),
        opened_id: Some("{speakers}".into()),
        reason: Some(LocalDeviceFallback::NotFoundFellBackToDefault),
    });
    assert!(repli.differs);
    assert_eq!(
        repli.reason,
        Some(LocalDeviceFallback::NotFoundFellBackToDefault)
    );
    assert_eq!(
        serde_json::to_value(&repli).expect("sérialisable")["reason"],
        "not_found_fell_back_to_default"
    );

    // c) Le cas nominal reste muet : aucun motif, aucun écart. Un écran qui
    //    ne lit que `differs` voit exactement ce qu'il voyait avant.
    let nominal = LocalDeviceStatus::from_observed(ObservedDevice {
        backend: "ALSA",
        requested: "Topping D10s".into(),
        opened: "Topping D10s".into(),
        opened_id: Some("hw:CARD=D10s".into()),
        reason: None,
    });
    assert!(!nominal.differs);
    assert_eq!(nominal.reason, None);
    assert_eq!(nominal.detail, None);
    let json = serde_json::to_value(&nominal).expect("sérialisable");
    assert!(
        json.get("reason").is_none() && json.get("detail").is_none(),
        "sans motif, les deux champs sont ABSENTS de la charge utile : un \
         client d'avant voit le même objet qu'avant"
    );
}

/// Chaque motif de repli de périphérique est câblé : un code stable, non
/// vide, unique, et un libellé en clair. Même contre-épreuve permanente que
/// pour `LocalBackendFallback` — un motif ajouté sans être décrit tombe.
#[test]
fn chaque_motif_de_repli_de_peripherique_est_cable() {
    let mut codes = std::collections::HashSet::new();
    for motif in LocalDeviceFallback::ALL {
        assert!(!motif.code().is_empty(), "{motif:?} sans code");
        assert!(!motif.detail().is_empty(), "{motif:?} sans libellé");
        assert!(
            codes.insert(motif.code()),
            "code dupliqué : {} — un client ne pourrait plus distinguer \
             les deux motifs",
            motif.code()
        );
        // Le code voyage en JSON : il doit être celui-là, pas le nom Rust.
        assert_eq!(
            serde_json::to_value(motif).expect("sérialisable"),
            serde_json::Value::String(motif.code().to_string()),
        );
    }
}

/// GARDE DE SITE — la production doit vraiment PASSER l'hôte d'origine.
///
/// La branche ASIO de `select_host` ne se compile pas sur Shrek, donc aucun
/// test d'intégration ne peut constater le refus de bout en bout ici. Ce
/// que ce test tient, c'est le câblage : `find_device_with_fallback` doit
/// transmettre `origin_host` à `resolve_device`, et refuser sans repli sur
/// `ForeignHost`. Rebrancher le nom étranger dans la production — passer
/// `None`, ou traiter `ForeignHost` comme `NotFound` — fait tomber ce test
/// même là où la scène complète n'est pas jouable.
///
/// Idiome du dépôt : relire la production par `include_str!`, comme
/// `terminologie_eq.rs` et `position_publiee_guard`.
#[test]
fn find_device_with_fallback_passe_bien_l_hote_d_origine() {
    let source = include_str!("../local.rs");
    let debut = source
        .find("fn find_device_with_fallback(")
        .expect("find_device_with_fallback introuvable");
    let corps = &source[debut..debut + 4000];

    assert!(
        corps.contains("origin_host: Option<&str>"),
        "find_device_with_fallback doit RECEVOIR l'hôte d'origine : sans lui, \
         un nom ne porte rien et #3230 revient"
    );
    let appel = corps
        .find("resolve_device(")
        .map(|i| &corps[i..i + 200])
        .expect("l'appel à resolve_device doit rester dans cette fonction");
    assert!(
        appel.contains("origin_host"),
        "l'appel à resolve_device doit LUI PASSER origin_host ; reçu : {appel}"
    );
    assert!(
        appel.contains("open_host"),
        "l'appel à resolve_device doit aussi LUI PASSER l'hôte ouvert : le \
         déduire de la liste énumérée ferait disparaître le refus quand \
         cette liste est vide ; reçu : {appel}"
    );
    assert!(
        corps.contains("DeviceResolution::ForeignHost"),
        "le refus d'hôte étranger doit être traité ici, pas confondu avec \
         « introuvable » — c'est ce dernier qui déclenche le repli sur le \
         périphérique par défaut de l'hôte ouvert"
    );
    // Le refus doit COUPER : pas de repli derrière lui.
    let refus = corps
        .find("DeviceResolution::ForeignHost")
        .map(|i| &corps[i..])
        .expect("bloc de refus");
    let fin_refus = refus.find("return None;").unwrap_or(usize::MAX);
    let repli = refus.find("default_output_device").unwrap_or(usize::MAX);
    assert!(
        fin_refus < repli,
        "le refus doit rendre None AVANT tout repli sur la sortie par défaut"
    );
}

fn wasapi_endpoint_fixture() -> Vec<WasapiEndpoint> {
    vec![
        WasapiEndpoint {
            id: "{speaker-a}".into(),
            name: "Haut-parleurs".into(),
        },
        WasapiEndpoint {
            id: "{speaker-b}".into(),
            name: "Haut-parleurs".into(),
        },
        WasapiEndpoint {
            id: "{usb-dac}".into(),
            name: "DAC USB".into(),
        },
    ]
}

#[test]
fn wasapi_duplicate_names_resolve_to_distinct_stable_endpoints() {
    let endpoints = wasapi_endpoint_fixture();
    assert_eq!(
        select_wasapi_endpoint("Haut-parleurs", Some("{speaker-a}"), &endpoints)
            .unwrap()
            .id,
        "{speaker-a}"
    );
    assert_eq!(
        select_wasapi_endpoint("Haut-parleurs (2)", Some("{speaker-a}"), &endpoints)
            .unwrap()
            .id,
        "{speaker-b}"
    );
    assert_eq!(
        select_wasapi_endpoint("WASAPI:{usb-dac}", Some("{speaker-a}"), &endpoints)
            .unwrap()
            .name,
        "DAC USB"
    );
}

#[test]
fn wasapi_default_change_only_affects_an_explicit_default_request() {
    let endpoints = wasapi_endpoint_fixture();
    let first_default = select_wasapi_endpoint("default", Some("{speaker-a}"), &endpoints).unwrap();
    let changed_default = select_wasapi_endpoint("default", Some("{usb-dac}"), &endpoints).unwrap();
    assert_eq!(first_default.id, "{speaker-a}");
    assert_eq!(changed_default.id, "{usb-dac}");

    let explicit_before =
        select_wasapi_endpoint("Haut-parleurs", Some("{speaker-a}"), &endpoints).unwrap();
    let explicit_after =
        select_wasapi_endpoint("Haut-parleurs", Some("{usb-dac}"), &endpoints).unwrap();
    assert_eq!(explicit_before.id, "{speaker-a}");
    assert_eq!(explicit_after.id, "{speaker-a}");
}

#[test]
fn wasapi_missing_endpoint_fails_instead_of_selecting_default() {
    let error = select_wasapi_endpoint(
        "DAC disparu",
        Some("{speaker-a}"),
        &wasapi_endpoint_fixture(),
    )
    .expect_err("un endpoint absent doit échouer");
    assert!(error.contains("DAC disparu"));
    assert!(error.contains("{speaker-a}"));
}

#[test]
fn exclusive_open_failure_is_returned_without_authorising_a_fallback() {
    for backend in ["ASIO", "CoreAudio", "WASAPI"] {
        let slot = std::sync::Mutex::new(None);
        record_exclusive_open_failure(backend, "DAC USB", "endpoint {usb-dac} absent", &slot);
        let message = slot.lock().unwrap().clone().expect("erreur remontée");
        assert!(message.contains(backend));
        assert!(message.contains("DAC USB"));
        assert!(message.contains("{usb-dac}"));
        assert!(message.contains("Aucun repli"));
    }
}

#[test]
fn native_windows_ring_is_byte_exact_for_16_24_and_32_bit_pcm() {
    for bit_depth in [16u16, 24, 32] {
        let source = integer_pcm_fixture(bit_depth);
        let native = pcm_bytes_to_native_i32(&source, bit_depth);
        let ring = NativePcmRing::new(native.len());
        assert_eq!(ring.push(&native), native.len());

        let mut observed = vec![0u8; source.len()];
        assert_eq!(ring.pop_pcm_bytes(&mut observed, bit_depth), source.len());
        assert_eq!(
            observed, source,
            "le dernier callback backend a modifié un mot {bit_depth} bits"
        );
    }
}

#[test]
fn native_windows_ring_is_exact_at_asio_i16_and_i24_callback_boundaries() {
    let source_16 = integer_pcm_fixture(16);
    let native_16 = pcm_bytes_to_native_i32(&source_16, 16);
    let ring_16 = NativePcmRing::new(native_16.len());
    assert_eq!(ring_16.push(&native_16), native_16.len());
    let mut asio_16 = vec![0i16; native_16.len()];
    assert_eq!(
        ring_16.pop_mapped(&mut asio_16, |sample| (sample >> 16) as i16),
        native_16.len()
    );
    let observed_16: Vec<u8> = asio_16.iter().flat_map(|word| word.to_le_bytes()).collect();
    assert_eq!(observed_16, source_16);

    let source_24 = integer_pcm_fixture(24);
    let native_24 = pcm_bytes_to_native_i32(&source_24, 24);
    let ring_24 = NativePcmRing::new(native_24.len());
    assert_eq!(ring_24.push(&native_24), native_24.len());
    let zero = cpal::I24::new(0).expect("zero tient sur 24 bits");
    let mut asio_24 = vec![zero; native_24.len()];
    assert_eq!(
        ring_24.pop_mapped(&mut asio_24, |sample| {
            cpal::I24::new(sample >> 8).expect("le mot natif tient sur 24 bits")
        }),
        native_24.len()
    );
    let observed_24: Vec<u8> = asio_24
        .iter()
        .flat_map(|word| word.inner().to_le_bytes()[..3].to_vec())
        .collect();
    assert_eq!(observed_24, source_24);
}

#[test]
fn native_windows_ring_preserves_every_dop_marker_and_payload_byte() {
    let source = versioned_dop_fixture();
    let native = pcm_bytes_to_native_i32(&source, 24);
    let ring = NativePcmRing::new(native.len());
    assert_eq!(ring.push(&native), native.len());
    // Deliberately request a callback larger than the remaining stream:
    // this is the final backend callback, including its silence suffix.
    let mut observed = vec![0xAAu8; (native.len() + 16) * 3];
    let written = ring.pop_pcm_bytes(&mut observed, 24);
    assert_eq!(written, source.len());
    observed[written..].fill(0);
    assert_eq!(&observed[..source.len()], source);
    assert!(observed[source.len()..].iter().all(|octet| *octet == 0));
    for (frame, pair) in observed[..source.len()].chunks_exact(6).enumerate() {
        let marker = if frame % 2 == 0 { 0x05 } else { 0xFA };
        assert_eq!(pair[2], marker);
        assert_eq!(pair[5], marker);
    }
}

#[test]
fn native_windows_preparation_keeps_identity_pcm_out_of_float() {
    for bit_depth in [16u16, 24, 32] {
        let source = integer_pcm_fixture(bit_depth);
        let eq = std::sync::Mutex::new(None);
        let convolver = std::sync::Mutex::new(None);
        let crossfeed = std::sync::Mutex::new(None);
        let pure = AtomicBool::new(false);
        let prepared = prepare_windows_native_pcm(
            &source,
            bit_depth,
            2,
            true,
            false,
            1000,
            &eq,
            &convolver,
            &crossfeed,
            &pure,
            &AtomicBool::new(false),
        )
        .expect("fenêtre PCM complète");
        assert!(prepared.bit_perfect);
        assert!(!prepared.dop);

        let mut observed = vec![0u8; source.len()];
        native_i32_to_pcm_bytes(&prepared.samples, bit_depth, &mut observed);
        assert_eq!(observed, source, "identité perdue en {bit_depth} bits");
    }
}

#[test]
fn native_windows_preparation_forces_dop_onto_the_raw_branch() {
    let source = versioned_dop_fixture();
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(Some(crate::audio::crossfeed::CrossfeedProcessor::new(
        176400, 0.3, 0.3,
    )));
    let pure = AtomicBool::new(false);
    let prepared = prepare_windows_native_pcm(
        &source,
        24,
        2,
        true,
        false,
        250,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
    )
    .expect("DoP complet");
    assert!(prepared.dop);
    assert!(prepared.bit_perfect);

    let mut observed = vec![0u8; source.len()];
    native_i32_to_pcm_bytes(&prepared.samples, 24, &mut observed);
    assert_eq!(
        observed, source,
        "volume et DSP ne doivent jamais toucher DoP"
    );
}

#[test]
fn native_windows_preparation_marks_processed_pcm_as_not_bitperfect() {
    let source = integer_pcm_fixture(24);
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);
    let prepared = prepare_windows_native_pcm(
        &source,
        24,
        2,
        true,
        false,
        500,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
    )
    .expect("PCM complet");
    assert!(!prepared.dop);
    assert!(!prepared.bit_perfect);
}
#[test]
fn windows_float_exclusive_rejects_dop_before_the_ring() {
    let fixture = versioned_dop_fixture();
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);
    let ring = RingBuf::new(4096);

    // 31 frames do not prove either PCM or DoP: they stay in the raw-byte
    // quarantine and absolutely nothing reaches the f32 ring.
    let first_31_frames = 31 * 2 * 3;
    let pending = prepare_windows_exclusive_pcm(
        &fixture[..first_31_frames],
        24,
        2,
        true,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
    );
    assert!(matches!(pending, Ok(None)));
    assert_eq!(ring.available(), 0);

    // Once the byte window is conclusive, rejection happens at the last
    // preparation boundary — still before conversion, DSP and ring feed.
    let rejected = prepare_windows_exclusive_pcm(
        &fixture,
        24,
        2,
        true,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
    );
    assert!(matches!(
        rejected,
        Err(WindowsExclusivePcmError::DopUnsupported)
    ));
    assert_eq!(ring.available(), 0);
}

#[test]
fn windows_float_exclusive_rejects_an_inconclusive_24bit_eof() {
    assert_eq!(
        finish_windows_exclusive_probe(24, true, 31 * 2 * 3),
        Err(WindowsExclusivePcmError::DopCheckIncomplete)
    );
    assert_eq!(finish_windows_exclusive_probe(24, false, 0), Ok(()));
    assert_eq!(finish_windows_exclusive_probe(16, true, 17), Ok(()));
}

#[test]
fn windows_float_exclusive_exposes_the_refusal_reason() {
    let failure = std::sync::Mutex::new(None);
    record_windows_exclusive_pcm_refusal(
        WindowsExclusivePcmError::DopUnsupported,
        "WASAPI",
        "DAC USB",
        &failure,
    );
    let message = failure.lock().unwrap().take().expect("erreur remontée");
    assert!(message.contains("DAC USB"));
    assert!(message.contains("DoP"));
    assert!(message.contains("WASAPI"));
    assert!(message.contains("conversion flottante"));
    assert!(message.contains("refusée avant l'envoi"));
}

#[test]
fn windows_float_exclusive_applies_pcm_dsp_before_the_ring() {
    let mut pcm = Vec::new();
    for i in 0..4096 {
        let v =
            ((2.0 * std::f64::consts::PI * 8000.0 * i as f64 / 44100.0).sin() * 4_000_000.0) as i32;
        for _ in 0..2 {
            pcm.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    let before = pcm_bytes_to_f32(&pcm, 24);
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    let prepared = prepare_windows_exclusive_pcm(
        &pcm,
        24,
        2,
        true,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
    )
    .expect("PCM ordinaire accepté")
    .expect("fenêtre de détection complète");

    let ring = RingBuf::new(prepared.len());
    assert_eq!(ring.push(&prepared), prepared.len());
    let mut observed_at_backend_boundary = vec![0.0; prepared.len()];
    assert_eq!(ring.pop(&mut observed_at_backend_boundary), prepared.len());

    let before_rms = rms(&before[1024..]);
    let after_rms = rms(&observed_at_backend_boundary[1024..]);
    let delta_db = 20.0 * (after_rms / before_rms).log10();
    assert!(
        delta_db < -8.0,
        "le PCM WASAPI/ASIO exclusif doit traverser l'EQ avant le ring ; mesuré {delta_db:.1} dB"
    );
}

#[test]
fn dop_is_recognised_on_real_encoder_output() {
    for ch in [2usize, 1, 6] {
        let bytes = real_dop_bytes(256, ch);
        assert!(
            is_dop_pcm(&bytes, 24, ch as u16),
            "DoP {ch} canaux non reconnu"
        );
    }
}

#[test]
fn dop_detection_survives_a_chunk_boundary() {
    // Les boucles de lecture découpent le flux à l'octet près : la
    // détection ne doit pas dépendre de la parité du marqueur au début du
    // tampon, sinon une trame DoP sur deux serait filtrée — et le DAC se
    // tairait par intermittence.
    let bytes = real_dop_bytes(256, 2);
    let offset = 6; // une trame stéréo complète
    assert!(is_dop_pcm(&bytes[offset..], 24, 2));
}

#[test]
fn ordinary_pcm_is_never_taken_for_dop() {
    // Le faux positif est le risque de cette détection : il désactiverait
    // l'égaliseur en silence. Un sinus 24 bits ne doit jamais passer.
    let mut pcm = Vec::new();
    for i in 0..2048 {
        let v =
            ((2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 8_000_000.0) as i32;
        for _ in 0..2 {
            pcm.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    assert!(!is_dop_pcm(&pcm, 24, 2));
    // Le silence non plus (octet de poids fort à 0 partout).
    assert!(!is_dop_pcm(&vec![0u8; 4096], 24, 2));
    // Ni un tampon dont le marqueur est constant au lieu d'alterner.
    let stuck: Vec<u8> = (0..4096)
        .map(|i| if i % 3 == 2 { 0x05 } else { 0x11 })
        .collect();
    assert!(!is_dop_pcm(&stuck, 24, 2));
}

#[test]
fn dop_is_only_ever_detected_on_24_bit() {
    // DoP n'a pas d'autre porteur : chercher le marqueur dans du 16 ou du
    // 32 bits lirait des octets qui ne sont pas des marqueurs.
    let bytes = real_dop_bytes(256, 2);
    assert!(!is_dop_pcm(&bytes, 16, 2));
    assert!(!is_dop_pcm(&bytes, 32, 2));
    assert!(!is_dop_pcm(&bytes, 24, 0));
}

#[test]
fn dop_detection_needs_enough_frames() {
    // Un tampon trop court ne prouve rien : mieux vaut traiter le son
    // (comportement d'avant) que couper l'égaliseur sur une coïncidence.
    let bytes = real_dop_bytes(DOP_DETECT_FRAMES - 1, 2);
    assert!(!is_dop_pcm(&bytes, 24, 2));
    assert!(is_dop_pcm(&real_dop_bytes(DOP_DETECT_FRAMES, 2), 24, 2));
}

#[test]
fn local_pcm_processing_is_identical_across_the_header_boundary() {
    // Contre-épreuve de #2232 : l'impulsion est dans le bloc déjà lu avec
    // l'en-tête. Sa sortie doit être strictement la même que le flux de
    // référence traité en un seul chunk normal, état IIR compris.
    let mut bytes = Vec::new();
    for frame in 0..2048 {
        let left = if frame == 0 { 16_384i16 } else { 0 };
        let right = if frame == 0 { -16_384i16 } else { 0 };
        bytes.extend_from_slice(&left.to_le_bytes());
        bytes.extend_from_slice(&right.to_le_bytes());
    }

    let baseline_eq = std::sync::Mutex::new(Some(test_eq()));
    let baseline_convolver = std::sync::Mutex::new(None);
    let baseline_crossfeed = std::sync::Mutex::new(None);
    let baseline_pure = AtomicBool::new(false);
    let baseline_mono = AtomicBool::new(false);
    let baseline_dop = AtomicBool::new(false);
    let baseline_volume = AtomicU32::new(500);
    let baseline_user = AtomicU32::new(500);
    let baseline_rg = AtomicU32::new(1000);
    let baseline_processor = LocalPcmProcessor {
        eq: &baseline_eq,
        convolver: &baseline_convolver,
        crossfeed: &baseline_crossfeed,
        pure_bypass: &baseline_pure,
        mono_downmix: &baseline_mono,
        dop_active: &baseline_dop,
        volume: &baseline_volume,
        user_volume: &baseline_user,
        rg_factor: &baseline_rg,
    };
    let mut baseline_staged = bytes.clone();
    let mut baseline_kind = LocalPcmKind::for_bit_depth(16);
    let baseline = baseline_processor
        .process_pcm_chunk(&mut baseline_staged, 4, 16, 2, &mut baseline_kind)
        .expect("chunk de référence");
    assert!(baseline_staged.is_empty());

    let split_eq = std::sync::Mutex::new(Some(test_eq()));
    let split_convolver = std::sync::Mutex::new(None);
    let split_crossfeed = std::sync::Mutex::new(None);
    let split_pure = AtomicBool::new(false);
    let split_mono = AtomicBool::new(false);
    let split_dop = AtomicBool::new(false);
    let split_volume = AtomicU32::new(500);
    let split_user = AtomicU32::new(500);
    let split_rg = AtomicU32::new(1000);
    let split_processor = LocalPcmProcessor {
        eq: &split_eq,
        convolver: &split_convolver,
        crossfeed: &split_crossfeed,
        pure_bypass: &split_pure,
        mono_downmix: &split_mono,
        dop_active: &split_dop,
        volume: &split_volume,
        user_volume: &split_user,
        rg_factor: &split_rg,
    };
    let header_bytes = 137 * 4;
    let mut split_staged = bytes[..header_bytes].to_vec();
    let mut split_kind = LocalPcmKind::for_bit_depth(16);
    let first = split_processor
        .process_pcm_chunk(&mut split_staged, 4, 16, 2, &mut split_kind)
        .expect("bloc PCM de l'en-tête");
    split_staged.extend_from_slice(&bytes[header_bytes..]);
    let second = split_processor
        .process_pcm_chunk(&mut split_staged, 4, 16, 2, &mut split_kind)
        .expect("bloc PCM suivant");

    let mut split_output = first.samples;
    split_output.extend_from_slice(&second.samples);
    assert_eq!(split_output, baseline.samples);
    assert_ne!(split_output, pcm_bytes_to_f32(&bytes, 16));
    assert!(split_staged.is_empty());
}

#[test]
fn local_pcm_processing_quarantines_dop_before_volume_dsp_and_ring() {
    let fixture = real_dop_bytes(64, 2);
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(Some(crate::audio::crossfeed::CrossfeedProcessor::new(
        176400, 0.3, 0.3,
    )));
    let pure = AtomicBool::new(false);
    let mono = AtomicBool::new(false);
    let dop_active = AtomicBool::new(false);
    let volume = AtomicU32::new(400);
    let user = AtomicU32::new(500);
    let rg = AtomicU32::new(800);
    let processor = LocalPcmProcessor {
        eq: &eq,
        convolver: &convolver,
        crossfeed: &crossfeed,
        pure_bypass: &pure,
        mono_downmix: &mono,
        dop_active: &dop_active,
        volume: &volume,
        user_volume: &user,
        rg_factor: &rg,
    };
    let ring = RingBuf::new(fixture.len() / 3);
    let first_31_frames = 31 * 2 * 3;
    let mut staged = fixture[..first_31_frames].to_vec();
    let mut kind = LocalPcmKind::for_bit_depth(24);

    let pending = processor.process_pcm_chunk(&mut staged, 6, 24, 2, &mut kind);
    assert!(pending.is_none());
    assert_eq!(staged.len(), first_31_frames);
    assert_eq!(ring.available(), 0);
    assert_eq!(volume.load(Ordering::SeqCst), 400);

    staged.extend_from_slice(&fixture[first_31_frames..]);
    let prepared = processor
        .process_pcm_chunk(&mut staged, 6, 24, 2, &mut kind)
        .expect("sonde DoP devenue concluante");
    assert!(prepared.dop);
    assert_eq!(kind, LocalPcmKind::Dop);
    assert!(dop_active.load(Ordering::SeqCst));
    assert_eq!(volume.load(Ordering::SeqCst), 1000);
    assert_eq!(prepared.samples, pcm_bytes_to_f32(&fixture, 24));
    assert_eq!(ring.available(), 0, "le caller n'a encore rien publié");
    assert_eq!(ring.push(&prepared.samples), prepared.samples.len());

    // Une fois reconnue, la nature de la piste ne dépend plus de la taille
    // des lectures suivantes : ce petit chunk resterait sinon sous le seuil
    // de la sonde et réactiverait volume et DSP en plein DoP.
    let continuation = real_dop_bytes(4, 2);
    staged.extend_from_slice(&continuation);
    let continued = processor
        .process_pcm_chunk(&mut staged, 6, 24, 2, &mut kind)
        .expect("classification DoP verrouillée pour la piste");
    assert!(continued.dop);
    assert_eq!(continued.samples, pcm_bytes_to_f32(&continuation, 24));
    assert!(dop_active.load(Ordering::SeqCst));
    assert_eq!(volume.load(Ordering::SeqCst), 1000);
}

#[test]
fn local_dsp_leaves_a_dop_stream_strictly_untouched() {
    // Le cœur du défaut : avec un EQ ET un crossfeed installés, un flux DoP
    // doit ressortir bit pour bit identique. Un seul échantillon modifié
    // efface le marqueur, le DAC quitte le mode DSD et se tait (Tades,
    // forum #1408).
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(Some(crate::audio::crossfeed::CrossfeedProcessor::new(
        176400, 0.3, 0.3,
    )));
    let pure = AtomicBool::new(false);

    let mut samples = stereo_sine_8k(1024);
    let before = samples.clone();
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        true,
    );
    assert_eq!(samples, before);
}

#[test]
fn dop_bypass_does_not_disable_the_eq_on_ordinary_pcm() {
    // Non-régression de #1708 : la garde DoP ne doit rien coûter à ceux qui
    // écoutent du PCM, c'est-à-dire presque tout le monde.
    let eq = std::sync::Mutex::new(Some(test_eq()));
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure = AtomicBool::new(false);

    let mut samples = stereo_sine_8k(4096);
    let before = rms(&samples);
    apply_local_dsp(
        &mut samples,
        &eq,
        &convolver,
        &crossfeed,
        &pure,
        &AtomicBool::new(false),
        2,
        false,
    );
    assert!(20.0 * (rms(&samples[1024..]) / before).log10() < -8.0);
}

#[test]
fn sync_volume_to_dop_writes_what_the_callbacks_read() {
    // Le bras ASIO n'est pas compilé hors Windows : ce test couvre le corps
    // qu'il appelle, pour qu'une faute à cet endroit ne se découvre pas au
    // build Windows de la release.
    let volume = AtomicU32::new(1000);
    let user = AtomicU32::new(600);
    let rg = AtomicU32::new(708);

    sync_volume_to_dop(&volume, &user, &rg, false);
    assert_eq!(volume.load(Ordering::SeqCst), 425);

    sync_volume_to_dop(&volume, &user, &rg, true);
    assert_eq!(volume.load(Ordering::SeqCst), 1000);

    // Et le retour au PCM rend la main au curseur.
    sync_volume_to_dop(&volume, &user, &rg, false);
    assert_eq!(volume.load(Ordering::SeqCst), 425);
}

#[test]
fn dop_pins_the_effective_volume_to_unity() {
    // #1735, moitié « volume » : sur un flux DoP, tout facteur autre que
    // l'unité réécrit l'octet de marqueur et le DAC se coupe. Ni le curseur
    // ni le ReplayGain ne doivent pouvoir en sortir.
    assert_eq!(effective_volume_units(500, 1000, true), 1000);
    assert_eq!(effective_volume_units(0, 1000, true), 1000);
    assert_eq!(effective_volume_units(1000, 300, true), 1000);
    assert_eq!(effective_volume_units(120, 450, true), 1000);
}

#[test]
fn ordinary_pcm_keeps_the_volume_it_had_before() {
    // Non-régression : hors DoP, le calcul est exactement celui d'avant —
    // produit volume × ReplayGain, borné à l'unité.
    assert_eq!(effective_volume_units(1000, 1000, false), 1000);
    assert_eq!(effective_volume_units(500, 1000, false), 500);
    assert_eq!(effective_volume_units(0, 1000, false), 0);
    assert_eq!(effective_volume_units(800, 500, false), 400);
    // Le plafond à l'unité protège des pics d'un ReplayGain qui pousse.
    assert_eq!(effective_volume_units(1000, 4000, false), 1000);
    assert_eq!(effective_volume_units(900, 2000, false), 1000);
}

#[test]
fn a_dop_track_survives_a_volume_that_would_have_muted_it() {
    // Le cas de Tades, bout à bout : volume à 60 %, ReplayGain à -3 dB.
    // Avant, le produit (0,42) réécrivait le marqueur et le DAC se taisait.
    let user = 600;
    let rg = 708; // ~ -3 dB
    assert_eq!(effective_volume_units(user, rg, false), 425);
    assert_eq!(effective_volume_units(user, rg, true), 1000);

    // Et l'unité doit être EXACTE, pas approchée : c'est ce qui rend la
    // multiplication inoffensive sur un échantillon 24 bits, exactement
    // représentable dans une mantisse f32.
    let v = effective_volume_units(user, rg, true) as f32 / 1000.0;
    assert_eq!(v, 1.0f32);
    let sample = 0x05A3C7 as f32; // un échantillon 24 bits porteur du marqueur
    assert_eq!((sample * v) as i32, 0x05A3C7);
}

#[test]
fn header_read_retries_only_transient_kinds() {
    use std::io::ErrorKind;
    // #522: the next track's transcode hasn't emitted its WAV header yet →
    // retry instead of abandoning the gapless chain (would skip track 2).
    assert!(header_read_should_retry(ErrorKind::TimedOut));
    assert!(header_read_should_retry(ErrorKind::WouldBlock));
    // Real errors still fail fast (no infinite retry on a dead stream).
    assert!(!header_read_should_retry(ErrorKind::BrokenPipe));
    assert!(!header_read_should_retry(ErrorKind::UnexpectedEof));
    assert!(!header_read_should_retry(ErrorKind::NotFound));
}

// -----------------------------------------------------------------------
// USB DAC hot-unplug teardown (#1626)
// -----------------------------------------------------------------------

/// A `DeviceNotAvailable` stream error must flag `device_gone` so the
/// feeding thread tears down instead of waiting on a ring nobody drains.
#[test]
fn device_not_available_flags_device_gone() {
    let gone = Arc::new(AtomicBool::new(false));
    let mut cb = make_stream_error_cb(gone.clone());
    cb(cpal::StreamError::DeviceNotAvailable);
    assert!(gone.load(Ordering::SeqCst));
    // Repeated invocations (WASAPI fires once, but belt-and-suspenders)
    // keep the flag set and must not panic.
    cb(cpal::StreamError::DeviceNotAvailable);
    assert!(gone.load(Ordering::SeqCst));
}

/// Other stream errors (ALSA underruns are routine) must NOT tear down
/// playback.
#[test]
fn generic_stream_error_does_not_flag_device_gone() {
    let gone = Arc::new(AtomicBool::new(false));
    let mut cb = make_stream_error_cb(gone.clone());
    cb(cpal::StreamError::BackendSpecific {
        err: cpal::BackendSpecificError {
            description: "underrun".into(),
        },
    });
    assert!(!gone.load(Ordering::SeqCst));
}

/// Happy path: everything fits, the feeder reports success.
#[test]
fn feed_ring_reports_success_when_fully_fed() {
    let ring = RingBuf::new(16);
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    assert!(feed_ring_abortable(&ring, &[0.5f32; 8], &rx, &paused, None));
    assert_eq!(ring.available(), 8);
}

/// An abort (stop/new play) is a clean exit, not a stall: the caller's own
/// stop checks handle it, so the feeder must not report a dead consumer.
#[test]
fn feed_ring_abort_is_not_a_stall() {
    let ring = RingBuf::new(4);
    ring.push(&[0.0; 4]); // full — feeding would block
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let abort = AtomicBool::new(true);
    assert!(feed_ring_abortable(
        &ring,
        &[0.5f32; 8],
        &rx,
        &paused,
        Some(&abort)
    ));
}

/// Dead consumer (unplugged DAC: the cpal callback stops popping): the
/// wedge detector must report the stall so the playback thread stops
/// feeding instead of stalling 5s on every chunk forever. Slow test (~5s,
/// the real wedge threshold) — the price of exercising the actual guard.
#[test]
fn feed_ring_reports_stall_when_consumer_dead() {
    let ring = RingBuf::new(4);
    ring.push(&[0.0; 4]); // full, and nobody will ever pop
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let started = std::time::Instant::now();
    assert!(!feed_ring_abortable(
        &ring,
        &[0.5f32; 8],
        &rx,
        &paused,
        None
    ));
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
}

#[test]
fn test_parse_wav_header() {
    let header = crate::audio::wav::build_wav_header(2, 44100, 16);
    let parsed = parse_wav_header(&header);
    assert!(parsed.is_some());
    let (ch, sr, bd, offset) = parsed.unwrap();
    assert_eq!(ch, 2);
    assert_eq!(sr, 44100);
    assert_eq!(bd, 16);
    assert_eq!(offset, 44);
}

#[test]
fn test_pcm_bytes_to_f32_16bit() {
    // 0x7FFF = 32767 -> ~1.0
    let bytes = [0xFF, 0x7F, 0x00, 0x00]; // 32767, 0
    let samples = pcm_bytes_to_f32(&bytes, 16);
    assert_eq!(samples.len(), 2);
    assert!((samples[0] - 0.99997).abs() < 0.001);
    assert!((samples[1]).abs() < 0.001);
}

#[test]
fn test_pcm_bytes_to_f32_24bit() {
    let bytes = [0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x00]; // max positive, zero
    let samples = pcm_bytes_to_f32(&bytes, 24);
    assert_eq!(samples.len(), 2);
    assert!((samples[0] - 1.0).abs() < 0.001);
    assert!((samples[1]).abs() < 0.001);
}

#[test]
fn test_parse_wav_header_24bit() {
    let header = crate::audio::wav::build_wav_header(2, 96000, 24);
    let parsed = parse_wav_header(&header);
    assert!(parsed.is_some());
    let (ch, sr, bd, offset) = parsed.unwrap();
    assert_eq!(ch, 2);
    assert_eq!(sr, 96000);
    assert_eq!(bd, 24);
    assert_eq!(offset, 44);
}

#[test]
fn test_parse_wav_header_ieee_float() {
    // Build a 32-bit IEEE Float WAV header (format tag 3)
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE_FLOAT
    header[22..24].copy_from_slice(&2u16.to_le_bytes()); // channels
    header[24..28].copy_from_slice(&44100u32.to_le_bytes());
    header[28..32].copy_from_slice(&(44100u32 * 2 * 4).to_le_bytes()); // byte_rate
    header[32..34].copy_from_slice(&8u16.to_le_bytes()); // block_align
    header[34..36].copy_from_slice(&32u16.to_le_bytes()); // bits_per_sample
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

    let parsed = parse_wav_header(&header);
    assert!(parsed.is_some());
    let (ch, sr, bd, offset) = parsed.unwrap();
    assert_eq!(ch, 2);
    assert_eq!(sr, 44100);
    assert_eq!(bd, 0); // sentinel for IEEE float
    assert_eq!(offset, 44);
}

#[test]
fn test_parse_wav_header_extensible_24bit() {
    // Build a WAVE_FORMAT_EXTENSIBLE 24-bit WAV header
    let mut header = [0u8; 68]; // 12 (RIFF) + 8 (fmt hdr) + 40 (fmt data) + 8 (data hdr)
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&40u32.to_le_bytes()); // extensible fmt size
    header[20..22].copy_from_slice(&0xFFFEu16.to_le_bytes()); // EXTENSIBLE
    header[22..24].copy_from_slice(&2u16.to_le_bytes()); // channels
    header[24..28].copy_from_slice(&96000u32.to_le_bytes());
    header[28..32].copy_from_slice(&(96000u32 * 2 * 3).to_le_bytes());
    header[32..34].copy_from_slice(&6u16.to_le_bytes()); // block_align = 2*3
    header[34..36].copy_from_slice(&24u16.to_le_bytes()); // wBitsPerSample
    header[36..38].copy_from_slice(&22u16.to_le_bytes()); // cbSize
    header[38..40].copy_from_slice(&24u16.to_le_bytes()); // wValidBitsPerSample
    header[40..44].copy_from_slice(&0u32.to_le_bytes()); // channel mask
    // Sub-format GUID: PCM = {00000001-0000-0010-8000-00aa00389b71}
    header[44..46].copy_from_slice(&1u16.to_le_bytes());
    header[46..60].copy_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ]);
    header[60..64].copy_from_slice(b"data");
    header[64..68].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

    let parsed = parse_wav_header(&header);
    assert!(parsed.is_some());
    let (ch, sr, bd, offset) = parsed.unwrap();
    assert_eq!(ch, 2);
    assert_eq!(sr, 96000);
    assert_eq!(bd, 24);
    assert_eq!(offset, 68);
}

/// La régression : le test existant ne couvrait que 24-valid-dans-24
/// (block align 6 en stéréo), donc le cas où précision et conteneur
/// coïncident. Il ne pouvait pas détecter 24-dans-32, où rendre la
/// précision valide faisait avancer la lecture de trois octets là où le
/// flux en fait quatre — trames désalignées dès le premier échantillon
/// (#2234).
#[test]
fn extensible_24_bits_valides_dans_un_conteneur_de_32() {
    let mut header = vec![0u8; 68];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&40u32.to_le_bytes());
    header[20..22].copy_from_slice(&0xFFFEu16.to_le_bytes()); // EXTENSIBLE
    header[22..24].copy_from_slice(&2u16.to_le_bytes()); // stéréo
    header[24..28].copy_from_slice(&192000u32.to_le_bytes());
    header[28..32].copy_from_slice(&(192000u32 * 2 * 4).to_le_bytes());
    header[32..34].copy_from_slice(&8u16.to_le_bytes()); // block_align = 2 * 4
    header[34..36].copy_from_slice(&32u16.to_le_bytes()); // conteneur : 32
    header[36..38].copy_from_slice(&22u16.to_le_bytes()); // cbSize
    header[38..40].copy_from_slice(&24u16.to_le_bytes()); // précision : 24
    header[40..44].copy_from_slice(&0u32.to_le_bytes()); // channel mask
    header[44..46].copy_from_slice(&1u16.to_le_bytes()); // sous-format PCM
    header[46..60].copy_from_slice(&[
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ]);
    header[60..64].copy_from_slice(b"data");
    header[64..68].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());

    let (ch, sr, bd, offset) = parse_wav_header(&header).expect("en-tête valide");
    assert_eq!(ch, 2);
    assert_eq!(sr, 192000);
    assert_eq!(
        bd, 32,
        "le pas d'avancement vient du CONTENEUR : 4 octets, pas 3"
    );
    assert_eq!(offset, 68);
}

/// Lire au conteneur n'est pas qu'un rattrapage d'alignement : c'est la
/// MÊME valeur normalisée. Les bits valides sont cadrés à gauche, donc
/// `v` sur 24 bits vaut `v << 8` dans son conteneur de 32.
#[test]
fn lire_au_conteneur_donne_la_meme_valeur_quen_24_bits() {
    for v in [1i32, -1, 100, -100, 8_388_607, -8_388_608] {
        // Le même échantillon, écrit dans ses deux conteneurs.
        let en_24 = [
            (v & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            ((v >> 16) & 0xFF) as u8,
        ];
        let cadre = v << 8;
        let en_32 = cadre.to_le_bytes();

        let f24 = pcm_bytes_to_f32(&en_24, 24);
        let f32b = pcm_bytes_to_f32(&en_32, 32);
        assert_eq!(f24.len(), 1);
        assert_eq!(f32b.len(), 1);
        assert!(
            (f24[0] - f32b[0]).abs() < 1e-9,
            "v={v} : 24 bits donne {}, conteneur 32 donne {}",
            f24[0],
            f32b[0]
        );
    }
}

#[test]
fn test_pcm_bytes_to_f32_float() {
    // IEEE Float 32-bit: 0.5 and -0.5
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0.5f32.to_le_bytes());
    bytes.extend_from_slice(&(-0.5f32).to_le_bytes());
    let samples = pcm_bytes_to_f32(&bytes, 0);
    assert_eq!(samples.len(), 2);
    assert!((samples[0] - 0.5).abs() < 0.0001);
    assert!((samples[1] + 0.5).abs() < 0.0001);
}

#[test]
fn test_ring_buffer() {
    let ring = RingBuf::new(16);
    let data = [1.0f32, 2.0, 3.0, 4.0];
    assert_eq!(ring.push(&data), 4);
    assert_eq!(ring.available(), 4);

    let mut out = [0.0f32; 4];
    assert_eq!(ring.pop(&mut out), 4);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    assert_eq!(ring.available(), 0);
}

#[test]
fn test_ring_buffer_overflow() {
    let ring = RingBuf::new(4);
    let data = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
    assert_eq!(ring.push(&data), 4); // only 4 fit
    assert_eq!(ring.available(), 4);
}

#[test]
fn test_ring_buffer_clear() {
    let ring = RingBuf::new(16);
    let data = [1.0f32, 2.0, 3.0, 4.0];
    ring.push(&data);
    assert_eq!(ring.available(), 4);

    ring.clear();
    assert_eq!(ring.available(), 0);

    // After clear, reading should return zeros
    let mut out = [0.0f32; 4];
    ring.push(&[5.0, 6.0]);
    assert_eq!(ring.pop(&mut out), 2);
    assert_eq!(out[0], 5.0);
    assert_eq!(out[1], 6.0);
}

#[test]
fn test_adapt_channels_mono_to_stereo() {
    let mono = [0.5f32, 0.7];
    let stereo = adapt_channels(&mono, 1, 2);
    assert_eq!(stereo, [0.5, 0.5, 0.7, 0.7]);
}

#[test]
fn test_adapt_channels_stereo_to_mono() {
    let stereo = [0.5f32, 0.7, 0.3, 0.9];
    let mono = adapt_channels(&stereo, 2, 1);
    assert_eq!(mono, [0.6, 0.6]);
}

#[test]
fn test_simple_resample_same_rate() {
    let data = [1.0f32, 2.0, 3.0, 4.0];
    let out = simple_resample(&data, 44100, 44100, 2);
    assert_eq!(out, data);
}

#[test]
fn test_simple_resample_upsample() {
    let data = [0.0f32, 0.0, 1.0, 1.0]; // 2 frames stereo
    let out = simple_resample(&data, 44100, 88200, 2);
    // Should produce ~4 frames
    assert_eq!(out.len(), 8);
}

#[test]
fn test_pcm_bytes_to_f32_24bit_negative() {
    // 24-bit minimum: 0x800000 = -8388608 -> -1.0
    let bytes = [0x00, 0x00, 0x80]; // -8388608
    let samples = pcm_bytes_to_f32(&bytes, 24);
    assert_eq!(samples.len(), 1);
    assert!(
        (samples[0] + 1.0).abs() < 0.001,
        "expected -1.0, got {}",
        samples[0]
    );

    // Small negative: 0xFFFFFF = -1 -> ~ -0.000000119
    let bytes2 = [0xFF, 0xFF, 0xFF];
    let samples2 = pcm_bytes_to_f32(&bytes2, 24);
    assert_eq!(samples2.len(), 1);
    assert!(samples2[0] < 0.0, "expected negative, got {}", samples2[0]);
}

#[test]
fn test_24bit_frame_alignment() {
    // Simulate the scenario that caused white noise: initial read
    // from a WAV stream where the PCM data after the header is NOT
    // a multiple of frame_bytes (6 for 24-bit stereo).
    //
    // Build a WAV header + 8 bytes of PCM (6 aligned + 2 remainder).
    let wav_hdr = crate::audio::wav::build_wav_header(2, 44100, 24);
    assert_eq!(wav_hdr.len(), 44);

    // 2 channels * 3 bytes = 6 bytes per frame
    let frame_bytes: usize = 6;

    // Create 8 bytes of PCM data (1 full frame + 2 leftover bytes)
    let pcm_data: Vec<u8> = vec![
        // Frame 0: L=0x000001 R=0x000002
        0x01, 0x00, 0x00, 0x02, 0x00, 0x00, // Frame 1 partial: first 2 bytes
        0x03, 0x00,
    ];

    // Simulate the old buggy code: only process aligned, drop remainder
    let aligned_len = (pcm_data.len() / frame_bytes) * frame_bytes;
    assert_eq!(aligned_len, 6);
    let remainder = pcm_data.len() - aligned_len;
    assert_eq!(remainder, 2, "there should be 2 leftover bytes");

    // The fix: carry remainder into leftover buffer
    let mut leftover: Vec<u8> = Vec::new();
    if aligned_len < pcm_data.len() {
        leftover.extend_from_slice(&pcm_data[aligned_len..]);
    }
    assert_eq!(leftover.len(), 2);
    assert_eq!(leftover, vec![0x03, 0x00]);

    // Simulate next read arriving: 4 more bytes complete frame 1
    let next_read: Vec<u8> = vec![0x00, 0x04, 0x00, 0x00];
    leftover.extend_from_slice(&next_read);
    // Now leftover has 6 bytes = 1 complete frame
    let aligned_len2 = (leftover.len() / frame_bytes) * frame_bytes;
    assert_eq!(aligned_len2, 6);
    let samples = pcm_bytes_to_f32(&leftover[..aligned_len2], 24);
    assert_eq!(samples.len(), 2); // L and R of frame 1
}

#[test]
fn test_resample_chunk_no_silence_padding() {
    // Verify that rubato_resample_chunk does NOT use partial_len (silence
    // padding) during continuous streaming.  This was the root cause of
    // white noise on 24-bit audio: frame counts from HTTP reads rarely
    // aligned to the resampler's block size (1024), so every chunk had a
    // trailing partial block padded with silence.
    use rubato::{
        Async, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
        calculate_cutoff,
    };

    let ch = 2usize;
    let ratio = 48000.0 / 96000.0; // downsample 2:1
    let sinc_len = 64;
    let window = WindowFunction::BlackmanHarris2;
    let f_cutoff = calculate_cutoff(sinc_len, window);
    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window,
    };
    let mut resampler: Option<Async<f32>> =
        Some(Async::new_sinc(ratio, 1.1, &params, 1024, ch, FixedAsync::Input).unwrap());

    let mut resample_leftover: Vec<f32> = Vec::new();

    // Simulate two chunks of 683 frames (not aligned to 1024 block size).
    // This is what happens with 24-bit stereo: 65536 bytes / 6 = ~10922 frames,
    // 10922 % 1024 = 682 remainder.  Here we use a single remainder-sized chunk.
    let chunk1: Vec<f32> = (0..683 * ch).map(|i| (i as f32 * 0.001).sin()).collect();
    let chunk2: Vec<f32> = (0..683 * ch).map(|i| (i as f32 * 0.002).sin()).collect();

    // First call: 683 frames < 1024 block size, so all go to leftover
    let out1 = rubato_resample_chunk(
        &mut resampler,
        &chunk1,
        ch as u16,
        false,
        &mut resample_leftover,
    );
    // No output yet (not enough frames for a complete block)
    assert!(
        out1.is_empty(),
        "expected no output from first partial chunk, got {} samples",
        out1.len()
    );
    assert_eq!(
        resample_leftover.len(),
        683 * ch,
        "leftover should hold all 683 frames"
    );

    // Second call: leftover (683) + new (683) = 1366 frames >= 1024
    let out2 = rubato_resample_chunk(
        &mut resampler,
        &chunk2,
        ch as u16,
        false,
        &mut resample_leftover,
    );
    // Should have output from 1 complete block (1024 input -> ~512 output frames)
    assert!(
        !out2.is_empty(),
        "expected output after accumulating enough frames"
    );
    // Leftover should have 1366 - 1024 = 342 frames
    assert_eq!(resample_leftover.len(), 342 * ch);

    // Flush: process remaining 342 frames with silence padding
    let flushed =
        rubato_resample_chunk(&mut resampler, &[], ch as u16, true, &mut resample_leftover);
    assert!(
        !flushed.is_empty(),
        "flush should produce output from remaining frames"
    );
    assert!(
        resample_leftover.is_empty(),
        "leftover should be empty after flush"
    );

    // Verify no NaN or infinity in output
    for s in out2.iter().chain(flushed.iter()) {
        assert!(s.is_finite(), "output contains non-finite value: {}", s);
    }
}

#[test]
fn test_list_audio_devices() {
    // Should not panic, even if no devices available
    let devices = list_audio_devices();
    // On CI there may be no devices, but on dev machines there should be at least one
    let _ = devices.len();
}
