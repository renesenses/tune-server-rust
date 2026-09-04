use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn dlna_zone() -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    // Ces contrats exercent les réglages DLNA ajoutés par migration. Sans
    // migration, ils restaient faussement verts tant que les écritures sur
    // une colonne absente étaient silencieusement ignorées (#2154).
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

// Hi-res ALAC source, currently playing, with a live stream session.
fn alac_hires_playing() -> ZoneState {
    let np = NowPlaying {
        title: "Track".into(),
        format: Some("alac".into()),
        sample_rate: Some(96_000),
        bit_depth: Some(24),
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    }
}

/// Décrit un fil réel : conteneur + fréquence + profondeur effectivement
/// servies. Passer 0 en fréquence ou profondeur simule une session qui ne
/// les connaît pas encore — l'affichage doit alors retomber sur les règles.
fn wire(format: &str, sample_rate: u32, bit_depth: u16) -> StreamInfo {
    StreamInfo {
        format: format.into(),
        sample_rate,
        bit_depth,
        ..Default::default()
    }
}

fn step_desc(v: &Value, name: &str) -> Option<String> {
    v.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|s| s.get("description").and_then(|d| d.as_str()))
        .map(String::from)
}

fn step_detail(v: &Value, name: &str) -> Option<String> {
    v.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|s| s.get("detail").and_then(|d| d.as_str()))
        .map(String::from)
}

/// #2074 — le message que voit l'utilisateur.
///
/// Bandcamp ne sert que du `mp3-128` en écoute libre, et la règle écrite
/// dans `plugins/tune-bandcamp/src/lib.rs` veut que ce débit soit
/// « annoncé comme tel PARTOUT où il apparaît ». Il l'était sur l'écran
/// Bandcamp et NULLE PART ailleurs : arrivée dans une zone, la piste
/// s'affichait « MP3 44kHz/16bit », indiscernable d'un 320 devant un DAC
/// de salon.
#[test]
fn a_lossy_source_announces_its_bitrate_in_the_signal_path() {
    let (backend, zone) = dlna_zone();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Un extrait".into(),
            source: "bandcamp".into(),
            format: Some("mp3".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            bitrate_kbps: Some(128),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };

    let sp = build_signal_path(&ps, &zone, &backend, Some("Marantz"), "", None).unwrap();

    assert_eq!(
        step_desc(&sp, "Source").as_deref(),
        Some("MP3 128 kbit/s 44kHz/16bit"),
        "le débit doit être lisible AVANT que le son n'atteigne le DAC"
    );
    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
}

/// #2074, cas de l'ACHAT — le pendant du test précédent.
///
/// La règle porte sur la qualité réelle du flux, jamais sur la source
/// « Bandcamp » en bloc : un album acheté descend en FLAC par la même
/// porte, et lui coller « 128 kbit/s » serait le même mensonge dans
/// l'autre sens.
#[test]
fn a_lossless_source_announces_no_bitrate() {
    let (backend, zone) = dlna_zone();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Un album acheté".into(),
            source: "bandcamp".into(),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            bitrate_kbps: None,
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };

    let sp = build_signal_path(&ps, &zone, &backend, Some("Marantz"), "", None).unwrap();

    assert_eq!(
        step_desc(&sp, "Source").as_deref(),
        Some("FLAC 44kHz/16bit"),
        "aucun débit ne doit apparaître sur un flux sans perte"
    );
}

/// #2212 — le chemin du signal nomme le pré-gain qui prévient les overs,
/// et ne présente plus l'ancien saturateur implicite comme une protection.
/// Une zone servie par une sortie PULL hors dépôt — le cas `diretta`.
fn diretta_zone() -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Diretta", Some("diretta"), Some("diretta-1"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

/// Un égaliseur ARMÉ sur la zone, écrit là où le chemin audio le lit.
fn armer_l_eq(backend: &Arc<dyn DbBackend>, zone_id: i64) {
    let profile = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            gain: 6.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    SettingsRepo::with_backend(backend.clone())
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profile).unwrap(),
        )
        .unwrap();
}

/// Source DSD128 en lecture, avec une session vivante.
fn dsd_playing() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Locatelli".into(),
            format: Some("dsf".into()),
            sample_rate: Some(5_644_800),
            bit_depth: Some(1),
            stream_id: Some("sid-dsd".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// #1393 — le panneau annonçait un égaliseur qui n'a PAS lieu.
///
/// Eric (fil forum, Windows 0.9.61) : « l'égaliseur ne fait rien » sur un
/// renderer Diretta et sur un PC vu comme zone DLNA. Le versant audible du
/// cas PCM a été corrigé par #1430 (`pull_output_needs_dsp_transcode` force
/// le chemin transcodé pour une sortie pull). Ce même correctif s'ABSTIENT
/// délibérément sur le DSD natif — convertir un flux DSD en PCM pour y
/// passer un EQ serait une dégradation décidée à la place de l'auditeur.
///
/// Le chemin du signal, lui, ne connaissait pas cette abstention : il lisait
/// `configured_dsp_enabled` — le RÉGLAGE en base — et affichait « EQ actif »
/// pour un traitement qui n'existe pas, en faisant au passage tomber le
/// verdict bit-perfect d'un fil que personne n'a touché. C'est la faute de
/// #1315 et #2053 : ne pas annoncer ce qui n'a pas lieu.
///
/// L'étape n'est pas SUPPRIMÉE : la faire disparaître laisserait l'auditeur
/// devant le même curseur inerte, sans explication. Elle dit ce qui est.
#[test]
fn un_eq_arme_sur_du_dsd_brut_est_annonce_contourne_et_non_applique() {
    let (backend, zone) = diretta_zone();
    armer_l_eq(&backend, zone.id.unwrap());

    // Le fil porte le .dsf tel quel : c'est CONSTATÉ, pas déduit.
    let sp = build_signal_path(
        &dsd_playing(),
        &zone,
        &backend,
        Some("Diretta Host"),
        "",
        Some(&wire("dsf", 5_644_800, 1)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "DSP").as_deref(),
        Some("DSP contourné (DSD natif servi brut)"),
        "un EQ que l'orchestrateur n'applique pas ne doit pas être annoncé actif"
    );
    let etape_dsp = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "DSP")
        .unwrap();
    assert_eq!(
        etape_dsp["bit_perfect"].as_bool(),
        Some(true),
        "rien n'a touché le flux : l'étape ne doit pas se déclarer dégradante"
    );
}

/// CONTRE-ÉPREUVE de l'essai ci-dessus, et elle est PERMANENTE.
///
/// Même zone `diretta`, même égaliseur armé, seul le FIL change : du FLAC au
/// lieu du DSD brut. Là, `pull_output_needs_dsp_transcode` force bien le
/// transcodage et l'EQ est réellement appliqué — le panneau doit donc
/// l'annoncer actif, et le verdict bit-perfect doit tomber.
///
/// Sans cette moitié, une garde trop large — « ne jamais annoncer le DSP
/// hors sortie locale » — laisserait la première verte tout en rendant le
/// panneau muet sur le cas d'Eric qui, lui, est bel et bien traité.
#[test]
fn le_meme_eq_sur_un_fil_pcm_reste_annonce_applique() {
    let (backend, zone) = diretta_zone();
    armer_l_eq(&backend, zone.id.unwrap());

    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Locatelli".into(),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-pcm".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Diretta Host"),
        "",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    let dsp = step_desc(&sp, "DSP").expect("l'étape DSP doit rester présente sur du PCM");
    assert!(
        dsp.starts_with("EQ actif"),
        "sur un fil PCM l'EQ est réellement appliqué : {dsp}"
    );
    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
}

#[test]
fn eq_step_exposes_per_channel_headroom_and_no_limiter() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    let profile = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![
            tune_core::audio::eq::EqBandSpec {
                gain: 6.0,
                channel: None,
                ..Default::default()
            },
            tune_core::audio::eq::EqBandSpec {
                gain: 3.0,
                channel: Some(0),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    SettingsRepo::with_backend(backend.clone())
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profile).unwrap(),
        )
        .unwrap();

    let sp = build_signal_path(
        &alac_hires_playing(),
        &zone,
        &backend,
        Some("Marantz"),
        "",
        Some(&wire("alac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "DSP").as_deref(),
        Some("EQ actif (pré-gain auto G -9.0 dB / D -6.0 dB, sans limiteur)")
    );
    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
}

/// #2205/#2233 : le backend Windows connaît déjà le verdict exact à la
/// frontière du callback. Le chemin public doit le croire plutôt que de
/// continuer à déclarer statiquement toute sortie locale bit-perfect.
#[test]
fn local_signal_path_uses_the_runtime_backend_contract_and_its_reason() {
    use tune_core::outputs::traits::{
        OutputDspMetrics, OutputDspState, OutputSampleTransport, OutputSignalPathStatus,
        OutputSignalReason, OutputVolumeState,
    };

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("DAC", Some("local"), Some("local:dac"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let mut ps = wav24_playing();
    ps.output_signal_path = Some(OutputSignalPathStatus {
        bit_perfect: false,
        sample_transport: OutputSampleTransport::Float,
        dsp: OutputDspState::Applied,
        volume: OutputVolumeState::Unity,
        reasons: vec![
            OutputSignalReason::FloatTransport,
            OutputSignalReason::DspApplied,
        ],
    });
    ps.output_dsp_metrics = Some(OutputDspMetrics {
        eq_overs: 17,
        eq_non_finite_samples: 2,
    });

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "ASIO", None).unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    assert_eq!(
        sp.get("runtime_observed").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        sp.get("runtime_reasons"),
        Some(&json!(["float_transport", "dsp_applied"]))
    );
    assert_eq!(
        step_detail(&sp, "Transport").as_deref(),
        Some("Transport flottant imposé par le callback ; DSP appliqué")
    );
    assert_eq!(step_desc(&sp, "DSP").as_deref(), Some("DSP appliqué"));
    assert_eq!(sp["dsp_metrics"]["eq_overs"], 17);
    assert_eq!(sp["dsp_metrics"]["eq_non_finite_samples"], 2);
    assert_eq!(
        sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["name"] == "DSP")
            .unwrap()["metrics"]["eq_overs"],
        17
    );
}

/// Monte une zone locale Windows dont la sonde a publié `reasons`.
fn local_runtime_zone(
    volume_percent: f64,
    volume: tune_core::outputs::traits::OutputVolumeState,
    reasons: Vec<OutputSignalReason>,
) -> (Zone, ZoneState, std::sync::Arc<dyn DbBackend>) {
    use tune_core::outputs::traits::{OutputSampleTransport, OutputSignalPathStatus};

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("DAC", Some("local"), Some("local:dac"))
        .unwrap();
    let mut zone = repo.get(id).unwrap().unwrap();
    zone.volume = volume_percent;

    let mut ps = wav24_playing();
    ps.output_signal_path = Some(OutputSignalPathStatus {
        // Le producteur a bien quitté la branche brute : ce buffer est
        // passé par le flottant pour appliquer le facteur de volume.
        bit_perfect: false,
        sample_transport: OutputSampleTransport::NativeInteger,
        dsp: OutputDspState::Inactive,
        volume,
        reasons,
    });
    (zone, ps, backend)
}

/// #2053 — « Lecture annoncée comme transcodée alors que je ne pense pas
/// avoir paramétré cela » (Tades, Windows).
///
/// Le client n'a que deux mots pour ce champ : « Bit-perfect » ou
/// « Transcodé » (`NowPlaying.svelte`). Tout ce qui n'est pas bit-perfect
/// s'affiche donc comme un transcodage — y compris quand aucune conversion
/// n'a lieu. Depuis la sonde Windows, un simple curseur de volume à 85 %
/// suffisait à déclencher ce mot, sur une zone où rien n'a été paramétré.
///
/// La règle inverse est écrite dans `build_signal_path` depuis #1627
/// (« Volume is excluded — it's a user preference, not a signal
/// degradation ») et reste appliquée à toutes les autres sorties et à
/// toutes les autres plateformes. Elle vaut aussi ici.
#[test]
fn software_volume_alone_does_not_announce_a_transcode() {
    let (zone, ps, backend) = local_runtime_zone(
        85.0,
        OutputVolumeState::Applied,
        vec![OutputSignalReason::SoftwareVolume],
    );

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(true));
    // Rien n'est caché : l'étape reste là, avec son pourcentage, et la
    // cause reste nommée dans le contrat d'exécution.
    assert_eq!(
        step_desc(&sp, "Volume").as_deref(),
        Some("Volume logiciel 85%")
    );
    assert_eq!(
        sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["name"] == "Volume")
            .unwrap()["bit_perfect"],
        json!(true)
    );
    assert_eq!(sp.get("runtime_reasons"), Some(&json!(["software_volume"])));
    assert_eq!(
        step_detail(&sp, "Transport").as_deref(),
        Some("Volume logiciel appliqué")
    );
    assert!(!sp["summary"].as_str().unwrap().contains("transcode"));
}

/// Contre-épreuve : l'exemption ne vaut QUE pour le volume seul. Dès qu'une
/// autre cause s'ajoute, le verdict du producteur reste négatif — on ne
/// relève jamais son verdict en promesse de pureté.
#[test]
fn a_second_cause_beside_volume_keeps_the_negative_verdict() {
    let (zone, ps, backend) = local_runtime_zone(
        85.0,
        OutputVolumeState::Applied,
        vec![
            OutputSignalReason::FloatTransport,
            OutputSignalReason::SoftwareVolume,
        ],
    );

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
    assert_eq!(
        step_detail(&sp, "Transport").as_deref(),
        Some("Transport flottant imposé par le callback ; Volume logiciel appliqué")
    );
}

/// Et un verdict négatif SANS raison nommée n'est pas non plus relevé :
/// l'exemption exige la liste explicite, jamais une liste vide.
#[test]
fn an_unexplained_negative_verdict_is_never_upgraded() {
    let (zone, ps, backend) = local_runtime_zone(85.0, OutputVolumeState::Applied, vec![]);

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
}

// ------------------------------------------------------------------
// Garde-fou : le fil prime, quelles que soient les combinaisons.
//
// Ce module a une raison d'être précise. `build_signal_path` rejouait les
// décisions de l'orchestrateur pour deviner ce qui partait sur le réseau, si
// bien que chaque évolution du chemin audio devait être répliquée ici à la
// main. Le même bug est revenu six fois sous des formes différentes
// (ALAC→FLAC fantôme, cap 16 bits, WAV 24, égaliseur ignoré) parce qu'on
// ajoutait un miroir de plus à chaque fois, sans jamais supprimer la cause.
//
// Le test ci-dessous ne simule PAS l'orchestrateur — ce serait un faux
// garde-fou, qui ne ferait que dupliquer une troisième fois les mêmes
// règles. Il verrouille l'invariant qui rend les miroirs inoffensifs :
// **quand la session de flux renseigne le format réellement servi, c'est lui
// qui s'affiche, et aucun réglage de zone ne peut le contredire.**
//
// Concrètement : si quelqu'un rajoute demain une règle qui écrase la valeur
// du fil, ce test casse, et il casse en nommant la combinaison fautive.
#[test]
fn wire_always_wins_over_every_zone_flag_combination() {
    // Source hi-res ALAC, fil réellement servi en WAV 96 kHz / 24 bits.
    // Plusieurs de ces réglages « voudraient » plafonner à 16 bits.
    let served = wire("wav", 96_000, 24);
    let expected = "ALAC 96kHz/24bit \u{2192} WAV 96kHz/24bit";

    for lpcm in [false, true] {
        for cap16 in [false, true] {
            for wav24 in [false, true] {
                for alac_direct in [false, true] {
                    let (backend, zone) = dlna_zone();
                    let repo = ZoneRepo::with_backend(backend.clone());
                    let id = zone.id.unwrap();
                    repo.update_dlna_lpcm(id, lpcm).unwrap();
                    repo.update_dlna_cap_16bit(id, cap16).unwrap();
                    repo.update_dlna_wav24(id, wav24).unwrap();
                    repo.update_alac_passthrough(id, alac_direct).unwrap();
                    let zone = repo.get(id).unwrap().unwrap();

                    let sp = build_signal_path(
                        &alac_hires_playing(),
                        &zone,
                        &backend,
                        Some("darTZeel LHC-208"),
                        "none",
                        Some(&served),
                    )
                    .unwrap();

                    // Une combinaison peut légitimement ne pas afficher
                    // d'étape Transcodeur ; ce qui ne se pardonne pas, c'est
                    // d'en afficher une qui contredise le fil.
                    if let Some(desc) = transcoder_desc(&sp) {
                        assert_eq!(
                            desc, expected,
                            "lpcm={lpcm} cap16={cap16} wav24={wav24} alac_direct={alac_direct} : \
                             l'affichage contredit le fil reellement servi"
                        );
                    }
                }
            }
        }
    }
}

// Second invariant, complémentaire : le CONTENEUR affiché est celui du fil.
// C'est le bug d'origine de Sevy (#1043) — le fil était en WAV et le chemin
// annonçait FLAC — remis sous test de façon systématique.
#[test]
fn wire_container_is_never_contradicted() {
    for (container, label) in [("wav", "WAV"), ("flac", "FLAC")] {
        let (backend, zone) = dlna_zone();
        let sp = build_signal_path(
            &alac_hires_playing(),
            &zone,
            &backend,
            Some("Eversolo DMP-A10"),
            "none",
            Some(&wire(container, 96_000, 24)),
        )
        .unwrap();
        if let Some(desc) = transcoder_desc(&sp) {
            assert!(
                desc.contains(label),
                "fil={container} mais l'affichage dit: {desc}"
            );
        }
    }
}

fn transcoder_desc(v: &Value) -> Option<String> {
    v.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("Transcoder"))
        .and_then(|s| s.get("description").and_then(|d| d.as_str()))
        .map(String::from)
}

// ------------------------------------------------------------------
// #1315 — l'affichage DSD sur Eversolo.
//
// Yves Corbat le 08/08, Stéphane Villerio le 28/08 avec les trois pièces :
// un DMP-A6 en DLNA, mode audiophile, volume figé à 100 %, une piste
// DSD128. Le panneau affichait un étage « DSD128 5.6 MHz → FLAC
// 5644kHz/1bit » pendant que le journal du serveur disait, à la seconde
// près, `dsd_passthrough_decide … dsd_mode=native passthrough=true` : le
// .dsf partait BRUT. Le transcodage était inventé, et son libellé
// impossible — aucun FLAC ne porte du 1 bit à 5,6 MHz.
//
// Les trois modes DSD d'une sortie réseau ont chacun leur test, pour que
// la disparition de l'étage fantôme ne se paie pas par la disparition des
// étages VRAIS.

/// Une piste DSD128 jouée sur une zone DLNA, avec le fil qu'on veut.
fn dsd128_playing() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Une piste DSD".into(),
            format: Some("dsf".into()),
            sample_rate: Some(5_644_800),
            bit_depth: Some(1),
            stream_id: Some("sid-dsd".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Un fil qui nomme aussi son MIME — c'est par là que le passthrough DSD
/// se reconnaît quand le renderer impose le sien (Yamaha R-N2000A :
/// `audio/dsf` et rien d'autre).
fn wire_mime(format: &str, mime: &str, sample_rate: u32, bit_depth: u16) -> StreamInfo {
    StreamInfo {
        format: format.into(),
        mime_type: mime.into(),
        sample_rate,
        bit_depth,
        ..Default::default()
    }
}

/// Mode 1/3 — DSD NATIF : le .dsf part brut, aucun étage de transcodage.
#[test]
fn dsd_natif_sur_le_fil_n_affiche_aucun_transcodage() {
    let (backend, zone) = dlna_zone();
    let sp = build_signal_path(
        &dsd128_playing(),
        &zone,
        &backend,
        Some("DMP-A6"),
        "none",
        Some(&wire_mime("dsf", "application/x-dsd", 5_644_800, 1)),
    )
    .unwrap();

    assert_eq!(
        transcoder_desc(&sp),
        None,
        "le .dsf part brut : annoncer un transcodage decrit une operation \
         qui n'a pas lieu (#1315)"
    );
    assert_eq!(step_desc(&sp, "Source").as_deref(), Some("DSD128 5.6 MHz"));
    assert_eq!(step_desc(&sp, "Transport").as_deref(), Some("DLNA/UPnP"));
    assert_eq!(
        sp.get("bit_perfect").and_then(Value::as_bool),
        Some(true),
        "un flux brut servi tel quel EST bit-perfect"
    );
    let summary = sp.get("summary").and_then(Value::as_str).unwrap();
    assert!(
        !summary.contains("FLAC"),
        "le resume invente encore un FLAC : {summary}"
    );
}

/// Le MIME suffit, quand le renderer impose le sien (`audio/dsf`) et que
/// la session porte l'extension du fichier.
#[test]
fn dsd_natif_se_reconnait_aussi_au_mime_annonce_par_le_renderer() {
    for mime in ["application/x-dsd", "audio/x-dsf", "audio/dff", "audio/dsf"] {
        let (backend, zone) = dlna_zone();
        let sp = build_signal_path(
            &dsd128_playing(),
            &zone,
            &backend,
            Some("Yamaha R-N2000A"),
            "none",
            Some(&wire_mime("", mime, 5_644_800, 1)),
        )
        .unwrap();
        assert_eq!(
            transcoder_desc(&sp),
            None,
            "mime={mime} : le fil porte du DSD brut, pas un transcodage"
        );
    }
}

/// Mode 2/3 — DoP : le DSD voyage EMBALLÉ dans des trames PCM 24 bits.
/// L'étage existe vraiment et doit rester affiché, avec les chiffres du
/// fil (352,8 kHz / 24 bits pour du DSD128), jamais ceux de la source.
#[test]
fn dsd_en_dop_affiche_l_etage_wav_du_fil() {
    let (backend, zone) = dlna_zone();
    let sp = build_signal_path(
        &dsd128_playing(),
        &zone,
        &backend,
        Some("Wiim Pro"),
        "none",
        Some(&wire_mime("wav", "audio/wav", 352_800, 24)),
    )
    .unwrap();

    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("DSD128 5.6 MHz \u{2192} WAV 352kHz/24bit"),
        "le DoP est un vrai emballage : l'etage doit rester, avec les \
         chiffres du fil"
    );
}

/// Mode 3/3 — TRANSCODÉ en PCM : l'étage est réel, et son libellé aussi.
/// C'est le cas témoin du premier test : la même source, le même code, un
/// fil différent — et l'étage revient.
#[test]
fn dsd_transcode_en_pcm_affiche_bien_son_etage() {
    let (backend, zone) = dlna_zone();
    let sp = build_signal_path(
        &dsd128_playing(),
        &zone,
        &backend,
        Some("DMP-A6"),
        "none",
        Some(&wire_mime("flac", "audio/flac", 176_400, 24)),
    )
    .unwrap();

    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("DSD128 5.6 MHz \u{2192} FLAC 176kHz/24bit"),
        "une conversion REELLE doit rester visible — supprimer le fantome \
         ne doit pas rendre le serveur muet sur ce qu'il fait vraiment"
    );
    assert_eq!(sp.get("bit_perfect").and_then(Value::as_bool), Some(false));
}

/// Aucun réglage de zone ne peut contredire un fil qui porte du DSD brut.
/// Le même invariant que `wire_always_wins_over_every_zone_flag_combination`,
/// appliqué au DSD : c'est le réglage « LPCM » coché qui aurait ramené un
/// « → WAV » sur un fil .dsf.
#[test]
fn aucun_reglage_de_zone_ne_transcode_un_fil_dsd_brut() {
    let served = wire_mime("dsf", "application/x-dsd", 5_644_800, 1);
    for lpcm in [false, true] {
        for cap16 in [false, true] {
            for wav24 in [false, true] {
                for dsd_mode in ["auto", "native", "dop", "pcm"] {
                    let (backend, zone) = dlna_zone();
                    let repo = ZoneRepo::with_backend(backend.clone());
                    let id = zone.id.unwrap();
                    repo.update_dlna_lpcm(id, lpcm).unwrap();
                    repo.update_dlna_cap_16bit(id, cap16).unwrap();
                    repo.update_dlna_wav24(id, wav24).unwrap();
                    repo.update_dsd_mode(id, dsd_mode).unwrap();
                    let zone = repo.get(id).unwrap().unwrap();

                    let sp = build_signal_path(
                        &dsd128_playing(),
                        &zone,
                        &backend,
                        Some("DMP-A6"),
                        "none",
                        Some(&served),
                    )
                    .unwrap();

                    assert_eq!(
                        transcoder_desc(&sp),
                        None,
                        "lpcm={lpcm} cap16={cap16} wav24={wav24} \
                         dsd_mode={dsd_mode} : l'affichage contredit un fil \
                         qui porte du DSD brut"
                    );
                }
            }
        }
    }
}

// ------------------------------------------------------------------
// Contre-épreuve PERMANENTE du libellé impossible (#1315, point 2).
//
// Le test ci-dessus protège le chemin ; celui-ci protège la CLASSE. On
// injecte de force, dans le formateur d'étage de sortie, la contradiction
// exacte qui a produit « FLAC 5644kHz/1bit » — une résolution du domaine
// DSD sous chaque nom de conteneur PCM du code. Aucune ne doit pouvoir en
// ressortir. Si quelqu'un rétablit un jour le format naïf, ce test casse
// en nommant le conteneur fautif.
#[test]
fn aucun_conteneur_pcm_ne_peut_porter_une_resolution_dsd() {
    for container in ["FLAC", "WAV", "ALAC", "AAC", "MP3", "AIFF", "Unknown"] {
        for (sr, bd) in [
            (5_644_800, 1),  // DSD128 brut, le cas de Stéphane Villerio
            (2_822_400, 1),  // DSD64
            (11_289_600, 1), // DSD256
            (22_579_200, 1), // DSD512
        ] {
            let label = output_stage_label(container, sr, bd);
            assert!(
                !label.contains(container),
                "injection acceptee : « {label} » — aucun {container} ne \
                 transporte du {bd} bit a {sr} Hz (#1315)"
            );
            assert!(
                label.starts_with("DSD"),
                "le fil porte du DSD, le libelle doit le dire : {label}"
            );
        }
    }
}

/// L'autre moitié de la contre-épreuve : le garde-fou ne doit pas mordre
/// sur du PCM légitime, jusqu'au 768 kHz/32 bits du marché.
#[test]
fn le_garde_fou_laisse_passer_tout_le_pcm_legitime() {
    for (sr, bd, attendu) in [
        (44_100, 16, "FLAC 44kHz/16bit"),
        (96_000, 24, "FLAC 96kHz/24bit"),
        (352_800, 24, "FLAC 352kHz/24bit"),
        (768_000, 32, "FLAC 768kHz/32bit"),
    ] {
        assert_eq!(output_stage_label("FLAC", sr, bd), attendu);
    }
}

// Sevy, LHC-52: the renderer is served WAV/LPCM (it does not advertise
// audio/flac), so the path must show the REAL wire container, not the
// static ALAC→FLAC transcode guess. The output is 16-bit LPCM, so the
// hi-res 24-bit source reads as downconverted (not bit-perfect).
#[test]
fn dlna_wav_wire_shows_alac_to_wav() {
    let (backend, zone) = dlna_zone();
    let ps = alac_hires_playing();
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("LHC-52"),
        "none",
        Some(&wire("wav", 96_000, 16)),
    )
    .unwrap();
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} WAV 96kHz/16bit")
    );
    // Hi-res source truncated to the 16-bit LPCM cap → not bit-perfect,
    // but still a lossless source.
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
}

// Regression guard: with no live session container (None) the display keeps
// its prior behaviour — ALAC transcodes to FLAC for DLNA.
#[test]
fn dlna_without_session_keeps_flac_target() {
    let (backend, zone) = dlna_zone();
    let ps = alac_hires_playing();
    let sp = build_signal_path(&ps, &zone, &backend, Some("LHC-52"), "none", None).unwrap();
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
    );
}

// A FLAC-advertising renderer (wire = flac) is unaffected by the override.
#[test]
fn dlna_flac_wire_keeps_flac_target() {
    let (backend, zone) = dlna_zone();
    let ps = alac_hires_playing();
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/24bit")
    );
}

// #1504 (Jean Valjean) / #1480 (Bebelalu55) : le panneau bit-perfect doit
// afficher LE MÊME volume que la page. La page montre `zone.volume` (base) ;
// `ps.volume` est une copie mémoire qui peut être périmée (0,5 par défaut
// après un redémarrage, ou laissée par une alarme/minuterie qui n'écrivait
// pas la base). L'étape Volume se lit donc depuis la base, quelle que soit
// la valeur mémoire.
#[test]
fn volume_step_reads_persisted_zone_volume_not_stale_memory() {
    let (backend, zone) = dlna_zone();
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = zone.id.unwrap();
    repo.update_volume(id, 20.0).unwrap();
    let zone = repo.get(id).unwrap().unwrap();

    // Copie mémoire périmée : le défaut 0,5 d'un ZoneState jamais resemé.
    let mut ps = alac_hires_playing();
    ps.volume = 0.5;

    let sp = build_signal_path(&ps, &zone, &backend, Some("Node"), "none", None).unwrap();
    assert_eq!(step_desc(&sp, "Volume").as_deref(), Some("Volume 20%"));
}

// Réciproque : curseur de la page à 100 % → pas d'étape Volume, même si la
// copie mémoire traîne à 20 % (c'était exactement l'affichage signalé).
#[test]
fn volume_step_hidden_when_persisted_volume_is_full() {
    let (backend, zone) = dlna_zone();
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = zone.id.unwrap();
    repo.update_volume(id, 100.0).unwrap();
    let zone = repo.get(id).unwrap().unwrap();

    let mut ps = alac_hires_playing();
    ps.volume = 0.2;

    let sp = build_signal_path(&ps, &zone, &backend, Some("Node"), "none", None).unwrap();
    assert_eq!(step_desc(&sp, "Volume"), None);
}

// Native WAV 24-bit source, served byte-for-byte over the WAV wire.
fn wav24_playing() -> ZoneState {
    let np = NowPlaying {
        title: "Track".into(),
        format: Some("wav".into()),
        sample_rate: Some(96_000),
        bit_depth: Some(24),
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    }
}

// Sandro/Progman: a NATIVE WAV 24-bit source is passthrough (WAV never
// transcodes for DLNA), so it must read bit-perfect even with dlna_wav24 off
// — the badge previously showed red for WAV 24-bit direct.
#[test]
fn dlna_native_wav24_is_bit_perfect() {
    let (backend, zone) = dlna_zone();
    let ps = wav24_playing();
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Diretta"),
        "none",
        Some(&wire("wav", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
}

// Yves, darTZeel LHC-208 et Eversolo DMP-A10 : zones en passthrough natif,
// donc AUCUNE étape de transcodage — la seule ligne portant une résolution
// est « Source ». Quand le scan n'a pas renseigné la piste (bibliothèque
// NAS), les valeurs retombaient sur 44100 Hz et 16 bits écrits en dur, et
// Tune affichait donc une résolution inventée pendant que le DAC lisait la
// vraie. Le fil est maintenant consulté avant d'en arriver là.
#[test]
fn passthrough_without_metadata_reads_the_wire_not_a_default() {
    let (backend, zone) = dlna_zone();
    let np = NowPlaying {
        title: "Track".into(),
        format: Some("flac".into()),
        sample_rate: None,
        bit_depth: None,
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    };
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("darTZeel LHC-208"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(
        step_desc(&sp, "Source").as_deref(),
        Some("FLAC 96kHz/24bit"),
        "sans metadonnees, la resolution doit venir du fil et non du repli 44100/16"
    );
}

// #2427: radio resolution is unknown when NowPlaying is created. Its
// 44.1 kHz value is only a bootstrap for the WAV session; after probing,
// the wire reports the PCM rate actually served and must win.
#[test]
fn decoded_radio_source_uses_the_detected_wire_rate() {
    let (backend, zone) = dlna_zone();
    let np = NowPlaying {
        title: "France Musique".into(),
        source: "radio".into(),
        format: Some("wav".into()),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        stream_id: Some("sid-radio".into()),
        ..Default::default()
    };
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    };

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Renderer"),
        "none",
        Some(&wire("wav", 48_000, 16)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "Source").as_deref(), Some("WAV 48kHz/16bit"));
}

// Sans session ET sans métadonnées, il n'y a rien à lire : le repli reste
// celui d'avant. Ce test existe pour que la suppression du repli soit un
// choix explicite si elle a lieu un jour, pas un effet de bord.
#[test]
fn no_wire_no_metadata_still_falls_back() {
    let (backend, zone) = dlna_zone();
    let np = NowPlaying {
        title: "Track".into(),
        format: Some("flac".into()),
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("LHC"), "none", None).unwrap();
    assert_eq!(
        step_desc(&sp, "Source").as_deref(),
        Some("FLAC 44kHz/16bit")
    );
}

// Le fil prime sur la règle. Ici la zone force le LPCM 16 bits, mais la
// session sert réellement du 24 bits : c'est le 24 qui doit s'afficher.
// Auparavant la règle gagnait et l'affichage annonçait une troncature qui
// n'avait pas lieu.
#[test]
fn wire_resolution_wins_over_mirrored_rule() {
    let (backend, zone) = dlna_zone();
    ZoneRepo::with_backend(backend.clone())
        .update_dlna_lpcm(zone.id.unwrap(), true)
        .unwrap();
    let ps = alac_hires_playing();
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Eversolo DMP-A10"),
        "none",
        Some(&wire("wav", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} WAV 96kHz/24bit")
    );
}

#[test]
fn wav_wire_native_wav_is_bit_perfect_any_depth() {
    assert!(wav_wire_bit_perfect(true, true, false, 24)); // native WAV 24-bit, flag off
    assert!(wav_wire_bit_perfect(true, true, false, 16));
}

#[test]
fn wav_wire_flac_fallback_capped_at_16_bit() {
    // FLAC/ALAC → WAV fallback (source not WAV): 24-bit needs the override.
    assert!(!wav_wire_bit_perfect(true, false, false, 24));
    assert!(wav_wire_bit_perfect(true, false, false, 16)); // fits plain 16-bit LPCM
    assert!(wav_wire_bit_perfect(true, false, true, 24)); // dlna_wav24 preserves 24-bit
}

#[test]
fn wav_wire_lossy_source_never_bit_perfect() {
    assert!(!wav_wire_bit_perfect(false, true, true, 16));
}

// ------------------------------------------------------------------
// ReplayGain dans le chemin du signal (#1627). Miroir de
// `Orchestrator::zone_replaygain_changes_audio` : le panneau ne doit pas
// annoncer « Bit-Perfect » pendant qu'un gain multiplie chaque
// échantillon — même famille d'écart que l'EQ ignoré du verdict
// (#1548/#1559, signalement Bilou).

/// Comme `dlna_zone()`, mais avec les migrations appliquées : les tags
/// ReplayGain vivent dans `track_metadata`, table créée par la migration 34
/// et NON par `init_schema()`. Sans elle, la lecture du gain échoue et le
/// test « pas d'étape » passerait pour la mauvaise raison.
fn dlna_zone_migrated() -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

/// Une piste FLAC en base, taguée `rg_track_gain` (et rien d'autre), et
/// l'état de lecture qui la joue. Le fil sert du FLAC : sans ReplayGain ce
/// chemin est un passthrough bit-perfect — le contraste que les tests
/// veulent.
fn flac_track_with_rg_tag(backend: &Arc<dyn DbBackend>, gain_tag: &str) -> (i64, ZoneState) {
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(96_000);
    t.bit_depth = Some(24);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_gain", gain_tag)
        .unwrap();
    let np = NowPlaying {
        title: "Piste".into(),
        track_id: Some(tid),
        format: Some("flac".into()),
        sample_rate: Some(96_000),
        bit_depth: Some(24),
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    };
    (tid, ps)
}

// RG actif (mode track, tag -4.2 dB) → étape présente avec le gain
// appliqué, et le verdict bit-perfect tombe — alors que le même chemin
// sans RG est un passthrough FLAC bit-perfect.
#[test]
fn replaygain_active_shows_step_and_breaks_bit_perfect() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    SettingsRepo::with_backend(backend.clone())
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "ReplayGain").as_deref(),
        Some("ReplayGain (track, -4.2 dB, tags du fichier)")
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
    // Le RG ne rend pas la SOURCE lossy : le badge qualité reste vert.
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
}

// RG off (défaut) : la même piste taguée n'affiche rien et reste
// bit-perfect — le réglage, pas le tag, décide.
#[test]
fn replaygain_off_shows_nothing_and_stays_bit_perfect() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "ReplayGain"), None);
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

// ---- #2362 : sortie mono ------------------------------------------------

/// Une zone LOCALE, seule à porter la chaîne DSP où le repli est appliqué.
fn local_zone_migrated() -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Bureau", Some("local"), Some("local:dac-1"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

fn flac_playing() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

fn armer_mono(backend: &Arc<dyn DbBackend>, zone_id: i64) {
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{zone_id}_mono_downmix"), "true")
        .unwrap();
}

/// #2362 — le chemin du signal DIT la transformation.
///
/// C'est la contrepartie de #2825, fusionnée cette nuit : là, le volume
/// logiciel prétendait à tort dégrader ; ici, une vraie transformation
/// devait apparaître et n'apparaissait pas. Le même chemin, mono désarmé,
/// est un passthrough FLAC bit-perfect (test suivant) : c'est le RÉGLAGE
/// qui décide, et lui seul.
#[test]
fn sortie_mono_affiche_son_etape_et_fait_tomber_le_verdict() {
    let (backend, zone) = local_zone_migrated();
    armer_mono(&backend, zone.id.unwrap());

    let sp = build_signal_path(
        &flac_playing(),
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "Mono").as_deref(),
        Some("Sortie mono : (G + D) / 2 sur les deux voies")
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
    // Le repli ne rend pas la SOURCE avec perte : le badge qualité reste vert.
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
}

/// Défaut désarmé : aucune étape inventée, verdict intact. Sans ce témoin,
/// le test ci-dessus passerait aussi avec une étape affichée en permanence.
#[test]
fn sortie_mono_desarmee_ninvente_aucune_etape() {
    let (backend, zone) = local_zone_migrated();

    let sp = build_signal_path(
        &flac_playing(),
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "Mono"), None);
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

/// Le périmètre de l'issue est la zone LOCALE. Une zone réseau qui porte le
/// réglage ne doit PAS afficher l'étape : rien ne l'applique sur ce chemin,
/// et l'annoncer décrirait un traitement qui n'a pas lieu.
#[test]
fn sortie_mono_ne_deborde_pas_sur_une_zone_reseau() {
    let (backend, zone) = dlna_zone_migrated();
    armer_mono(&backend, zone.id.unwrap());

    let sp = build_signal_path(
        &flac_playing(),
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "Mono"), None);
}

/// Le mode PURE gouverne le repli comme il gouverne l'égaliseur, le
/// crossfeed et le ReplayGain : rien ne touche le signal, donc aucune étape
/// et le verdict tient. Miroir de `zone_mono_downmix_with`.
#[test]
fn le_mode_pure_desarme_la_sortie_mono() {
    let (backend, zone) = local_zone_migrated();
    let zid = zone.id.unwrap();
    armer_mono(&backend, zid);
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{zid}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();

    let sp = build_signal_path(
        &flac_playing(),
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "Mono"), None);
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

// Mode track SANS tag stocké : gain effectif = 1, donc rien — l'étape
// suit le facteur réellement appliqué, pas le réglage (miroir du seuil
// de `zone_replaygain_changes_audio`).
#[test]
fn replaygain_mode_on_without_stored_gain_shows_nothing() {
    let (backend, zone) = dlna_zone_migrated();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    SettingsRepo::with_backend(backend.clone())
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();
    let np = NowPlaying {
        title: "Piste".into(),
        track_id: Some(tid),
        format: Some("flac".into()),
        sample_rate: Some(96_000),
        bit_depth: Some(24),
        stream_id: Some("sid-1".into()),
        ..Default::default()
    };
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(np),
        volume: 1.0,
        ..Default::default()
    };

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "ReplayGain"), None);
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

// PURE : le gain n'est jamais appliqué (orchestrator.rs, sortie locale et
// chemin transcodé), donc jamais d'étape — quel que soit le réglage.
#[test]
fn replaygain_never_shown_in_pure_mode() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();
    settings
        .set(
            &format!("zone_{}_audiophile", zone.id.unwrap()),
            r#"{"enabled":true}"#,
        )
        .unwrap();

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "ReplayGain"), None);
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

// Mode album sur une piste qui n'a que le tag de piste : c'est le gain de
// piste qui s'applique (repli de `stored_gain_detail`), et l'étape doit
// nommer ce qui joue vraiment — « track », pas le réglage « album ».
#[test]
fn replaygain_album_mode_falls_back_to_track_and_says_so() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    SettingsRepo::with_backend(backend.clone())
        .set(tune_core::audio::replaygain::MODE_KEY, "album")
        .unwrap();

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "ReplayGain").as_deref(),
        Some("ReplayGain (track, -4.2 dB, tags du fichier)")
    );
}

// ---- #1627 : d'où vient le gain -----------------------------------------

/// L'étape ReplayGain complète, faits structurés compris.
fn rg_step(sp: &serde_json::Value) -> serde_json::Value {
    sp.get("steps")
        .and_then(|s| s.as_array())
        .and_then(|steps| {
            steps
                .iter()
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("ReplayGain"))
        })
        .cloned()
        .expect("étape ReplayGain absente")
}

fn signal_path_mode_track(
    backend: &Arc<dyn DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
) -> serde_json::Value {
    SettingsRepo::with_backend(backend.clone())
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();
    build_signal_path(
        ps,
        zone,
        backend,
        Some("Node"),
        "none",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap()
}

// Un gain qui vient des tags du fichier (rsgain, foobar…) est nommé comme
// tel : c'est la réponse à « Tune utilise-t-il mes tags ? » (#1382), rendue
// à l'endroit où la question se pose.
#[test]
fn replaygain_gain_venu_des_tags_est_nomme_tags_du_fichier() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");

    let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

    assert_eq!(
        step.get("description").and_then(|d| d.as_str()),
        Some("ReplayGain (track, -4.2 dB, tags du fichier)")
    );
    assert_eq!(
        step.get("gain_source").and_then(|s| s.as_str()),
        Some("file_tags")
    );
    assert_eq!(
        step.get("granularity").and_then(|s| s.as_str()),
        Some("track")
    );
}

// Le même gain, mais MESURÉ par la passe EBU R128 : le témoin de
// provenance écrit à côté de `rg_track_gain` fait basculer le libellé.
// Sans lui les deux cas étaient indiscernables en base — et l'affichage
// aurait dû inventer.
#[test]
fn replaygain_gain_mesure_par_tune_est_nomme_analyse() {
    let (backend, zone) = dlna_zone_migrated();
    let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(
            tid,
            tune_core::audio::replaygain::TRACK_SOURCE_KEY,
            tune_core::audio::replaygain::SOURCE_ANALYSIS,
        )
        .unwrap();

    let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

    assert_eq!(
        step.get("description").and_then(|d| d.as_str()),
        Some("ReplayGain (track, -4.2 dB, analyse Tune)")
    );
    assert_eq!(
        step.get("gain_source").and_then(|s| s.as_str()),
        Some("analysis")
    );
}

// Bibliothèque analysée AVANT que le témoin existe (le parc installé) :
// `rg_analyzed` seul suffit à trancher, parce que le balayage n'analyse
// QUE les pistes dépourvues de `rg_track_gain`. Sans ce repli, tout le
// parc verrait « tags du fichier » sur des mesures Tune.
#[test]
fn replaygain_base_ancienne_retombe_sur_rg_analyzed() {
    let (backend, zone) = dlna_zone_migrated();
    let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_analyzed", "1700000000")
        .unwrap();

    let step = rg_step(&signal_path_mode_track(&backend, &zone, &ps));

    assert_eq!(
        step.get("gain_source").and_then(|s| s.as_str()),
        Some("analysis")
    );
}
