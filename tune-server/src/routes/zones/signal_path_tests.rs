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
    // Greffon facultatif (v0.9.156) : un profil ne suffit plus, il faut l'avoir
    // installé — la clé que pose `POST /plugins/equalizer/install`.
    SettingsRepo::with_backend(backend.clone())
        .set("plugin_equalizer_installed", "true")
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
    // Greffon facultatif (v0.9.156) : un profil ne suffit plus, il faut l'avoir
    // installé — la clé que pose `POST /plugins/equalizer/install`.
    SettingsRepo::with_backend(backend.clone())
        .set("plugin_equalizer_installed", "true")
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

    // La réserve est la norme L1 de la cascade — ici 10,486 / 7,175 dB, marge
    // de troncature comprise — et non plus la somme des gains positifs
    // (9,0 / 6,0 dB) : deux cloches empilées à 1 kHz sonnent au-delà de leur
    // gain crête, et depuis #4594 c'est cette borne-là, seule, qui est
    // réservée. Le panneau lit `automatic_headroom_db` à chaud : il annonce
    // donc toujours ce qui est RÉELLEMENT retiré au signal, sans qu'une
    // valeur soit recopiée quelque part. Le verdict bit-perfect, lui, ne
    // dépend pas du chiffre mais de l'EXISTENCE d'un EQ actif
    // (`zone_eq_alters_signal`) : il reste faux.
    assert_eq!(
        step_desc(&sp, "DSP").as_deref(),
        Some("EQ actif (pré-gain auto G -10.5 dB / D -7.2 dB, sans limiteur)")
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

/// #4174 — un DSD natif s'entend plus bas qu'un PCM, et l'écran doit le DIRE.
///
/// Cyrille Moutia, fil 1784 : « rien d'anormal a priori, sauf un son faible
/// pour un DSD 256 ». Son journal montre un passthrough : Tune ne touche à
/// rien, et la référence 0 dB du DSD est posée 6 dB sous la pleine échelle
/// PCM. Sans cette étape, le chemin du signal montrait une chaîne parfaite et
/// l'auditeur en concluait une panne.
#[test]
fn le_dsd_natif_annonce_sa_reference_de_niveau() {
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

    let niveau = step_desc(&sp, "Niveau").expect("l'étape Niveau doit exister sur un DSD natif");
    assert!(
        niveau.contains("6 dB"),
        "l'étape doit nommer l'écart, pas le suggérer : {niveau}"
    );
    assert!(
        niveau.contains("aucun gain"),
        "elle doit dire que Tune NE FAIT RIEN — c'est la question posée : {niveau}"
    );
    // Ne rien faire n'est pas une dégradation : l'étape ne doit pas peindre le
    // chemin en rouge ni contredire le verdict bit-perfect (#2053).
    assert_eq!(
        sp.get("bit_perfect").and_then(Value::as_bool),
        Some(true),
        "dire le niveau ne change pas le verdict"
    );
}

/// ⭐ Le témoin, et il compte autant : dès que Tune TOUCHE au flux, l'étape
/// disparaît. Sur la branche DSD→PCM, `DSD_SACD_GAIN` rattrape déjà les 6 dB
/// (#1638) — l'annoncer là serait faux.
#[test]
fn un_dsd_transcode_n_annonce_aucune_reference_de_niveau() {
    let (backend, zone) = dlna_zone();
    let sp = build_signal_path(
        &dsd128_playing(),
        &zone,
        &backend,
        Some("Renderer PCM"),
        "none",
        Some(&wire_mime("wav", "audio/wav", 176_400, 24)),
    )
    .unwrap();

    assert!(
        transcoder_desc(&sp).is_some(),
        "témoin de mise en place : ce cas DOIT transcoder"
    );
    assert_eq!(
        step_desc(&sp, "Niveau"),
        None,
        "sur la branche transcodée, le gain SACD est appliqué : l'annonce serait fausse"
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

// #2427/#4346: the probe, not the bootstrap WAV, describes the radio source.
#[test]
fn decoded_radio_source_uses_the_detected_source_rate() {
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
        Some(&StreamInfo {
            radio_source: Some(tune_core::http::streamer::RadioSourceInfo {
                format: Some("mp3"),
                sample_rate: Some(48_000),
                bit_depth: None,
            }),
            ..wire("wav", 48_000, 16)
        }),
    )
    .unwrap();

    assert_eq!(step_desc(&sp, "Source").as_deref(), Some("MP3 48kHz"));
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

/// #4297 — le miroir doit suivre l'élargissement du forçage WAV.
///
/// Sur la zone d'Yves (darTZeel LHC-208 : « Forcer le WAV = 24 bits » ET
/// « FLAC natif »), la source est un ALAC 44,1/16. Aucune session de flux n'est
/// fournie : c'est donc bien la RÈGLE de ce miroir qui parle, et non le fil.
/// Elle exigeait `bit_depth > 16` pour armer le forçage, comme la décision, et
/// annonçait « ALAC → FLAC » — l'écart de panneau de la famille #3183, sur
/// exactement le même écran que la capture du testeur.
#[test]
fn le_forcage_wav_24_s_affiche_aussi_sur_une_source_16_bits() {
    let (backend, zone) = dlna_zone();
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = zone.id.unwrap();
    repo.update_dlna_wav24(id, true).unwrap();
    repo.update_dlna_native_flac(id, true).unwrap();
    let zone = repo.get(id).unwrap().unwrap();

    let sp = build_signal_path(
        &alac_16_playing(),
        &zone,
        &backend,
        Some("darTZeel LHC-208"),
        "none",
        None,
    )
    .unwrap();

    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 44kHz/16bit \u{2192} WAV 44kHz/16bit"),
        "« Forcer le WAV » coché : le panneau annonce le WAV, pas un FLAC"
    );
}

/// Source ALAC 44,1/16 en lecture, sans session de flux : le cas d'Yves.
fn alac_16_playing() -> ZoneState {
    let np = NowPlaying {
        title: "Guided By The Moon".into(),
        format: Some("alac".into()),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
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
    let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_peak", "0.95")
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

// ---- #4072 : un gain positif REFUSÉ faute de pic tagué -------------------

/// Un champ quelconque de l'étape nommée.
fn step_field<'a>(v: &'a Value, name: &str, field: &str) -> Option<&'a Value> {
    v.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))?
        .get(field)
}

/// #4072 — le cœur du panneau muet.
///
/// `flac_track_with_rg_tag` tague `rg_track_gain` et RIEN d'autre : aucun pic,
/// exactement le cas mesuré par le banc T9 (+6 dB sur un signal proche du
/// rail ⇒ 66,2 % d'échantillons écrêtés avant le correctif). Avec
/// l'anti-écrêtage armé, le gain est maintenant refusé — facteur 1,0 — et
/// l'ancien seuil `(factor - 1.0).abs() <= 1e-6` faisait alors DISPARAÎTRE
/// l'étape : ReplayGain armé, tags lus, rien qui bouge, rien qui l'explique.
///
/// Ce que ce témoin garde : l'étape existe, elle dit le gain DEMANDÉ et le
/// motif du refus, et `clipping_guard` porte le motif en clé stable.
#[test]
fn un_gain_positif_sans_pic_tague_est_refuse_et_le_panneau_le_dit() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = flac_track_with_rg_tag(&backend, "+6.00 dB");
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
        Some("ReplayGain (track, +6.0 dB refusés : aucun pic tagué, anti-écrêtage armé)")
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "clipping_guard").and_then(|v| v.as_str()),
        Some("refused_no_peak")
    );
    // Un refus ne touche pas un échantillon : le fil reste intact, l'étape
    // n'est pas peinte en rouge, et le VERDICT ne tombe pas. La faire tomber
    // serait le mensonge inverse de #1393 — punir une zone pour un traitement
    // qui n'a précisément pas eu lieu.
    assert_eq!(
        step_field(&sp, "ReplayGain", "bit_perfect").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
}

/// Le même tag +6 dB, anti-écrêtage DÉSARMÉ : le gain s'applique comme avant,
/// l'étape le dit, et le verdict tombe. Sans ce témoin, une garde qui
/// refuserait TOUT gain positif passerait le témoin précédent.
#[test]
fn le_meme_gain_positif_passe_quand_l_anti_ecretage_est_desarme() {
    let (backend, zone) = dlna_zone_migrated();
    let (tid, ps) = flac_track_with_rg_tag(&backend, "+6.00 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_peak", "0.95")
        .unwrap();
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(tune_core::audio::replaygain::MODE_KEY, "track")
        .unwrap();
    settings
        .set(tune_core::audio::replaygain::PREVENT_CLIPPING_KEY, "false")
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
        Some("ReplayGain (track, +6.0 dB, tags du fichier)")
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "clipping_guard").and_then(|v| v.as_str()),
        Some("none")
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
}

/// Avec un pic d'échantillon, la borne inclut la réserve estimée de #4074.
/// Le garde-fou et la nature du pic sont nommés séparément.
#[test]
fn un_gain_positif_avec_sample_peak_affiche_sa_reserve_estimee() {
    let (backend, zone) = dlna_zone_migrated();
    let (tid, ps) = flac_track_with_rg_tag(&backend, "+6.00 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_peak", "0.95")
        .unwrap();
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
        step_field(&sp, "ReplayGain", "peak_kind").and_then(|v| v.as_str()),
        Some("sample_peak")
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "peak_headroom_db").and_then(|v| v.as_f64()),
        Some(3.0)
    );
    // Réserve de 3 dB : -3 - 20 log10(0,95) = -2,55 dB.
    assert_eq!(
        step_desc(&sp, "ReplayGain").as_deref(),
        Some(
            "ReplayGain (track, -2.6 dB, tags du fichier) — pic d'échantillon, réserve estimée de 3.0 dB (crête vraie inconnue)"
        )
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "clipping_guard").and_then(|v| v.as_str()),
        Some("tagged_peak")
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
}

/// #2218 T9 A — l'écrêtage de l'étage ReplayGain, compté par #4020, est
/// désormais LU dans le chemin du signal comme `eq_overs` l'est pour
/// l'égaliseur. Sa portée est celle du processus (l'étage reçoit un `f64` nu,
/// sans piste ni zone) et l'objet le DIT, pour qu'on ne le lise jamais comme
/// le compte de la zone affichée.
#[test]
fn l_etape_replaygain_porte_le_compteur_d_ecretage_et_dit_sa_portee() {
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

    let metrics = step_field(&sp, "ReplayGain", "metrics").expect("métriques d'écrêtage");
    assert_eq!(
        metrics.get("portee").and_then(|v| v.as_str()),
        Some("processus"),
        "la portée doit être dite, jamais devinée : {metrics}"
    );
    for champ in [
        "echantillons_vus",
        "echantillons_ecretes",
        "exces_max_lsb",
        "appels_ecretants",
        "pistes_ecretees",
    ] {
        assert!(
            metrics.get(champ).and_then(|v| v.as_u64()).is_some(),
            "champ {champ} absent : {metrics}"
        );
    }
    assert!(
        metrics
            .get("pourcentage")
            .and_then(|v| v.as_f64())
            .is_some()
    );
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
    let (tid, ps) = flac_track_with_rg_tag(&backend, "-4.20 dB");
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_peak", "0.95")
        .unwrap();
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
// ------------------------------------------------------------------
// #3183 — le miroir lit les quirks du CATALOGUE, comme l'orchestrateur.
//
// Troisième ligne de l'écart n° 3 du ticket : la décision
// (`resolve_local_track`) plafonne à 16 bits sur `dlna_cap_16bit` OU
// `device_quirks.force_16bit` ; le miroir ne consultait que le drapeau de
// zone. Sur un Ruark R3 (catalogue, `force_16bit`) et une source 24 bits,
// l'orchestrateur transcodait pendant que ce panneau annonçait un
// passthrough bit-perfect — le témoin qu'on demande aux testeurs de
// photographier mentait. Même faute sur le plafond de fréquence, que
// l'orchestrateur combine avec le catalogue (`combine_max_sample_rate`) et
// que le miroir lisait sur la seule zone (écart n° 1 du ticket, côté panneau).
//
// Les épreuves « comme l'orchestrateur » confrontent le miroir à la VRAIE
// décision, sur la même base, la même zone et la même piste : elles ne
// rejouent pas la condition, elles la comparent au verdict de
// `resolve_queue_item_url`.

/// L'appareil que l'utilisateur a choisi pour la zone, écrit là où le chemin
/// audio le lit (`device_catalog::resolve_zone_quirks`).
fn choisir_l_appareil(backend: &Arc<dyn DbBackend>, zone_id: i64, marque: &str, modele: &str) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    settings
        .set(&format!("zone_{zone_id}_brand"), marque)
        .unwrap();
    settings
        .set(&format!("zone_{zone_id}_model"), modele)
        .unwrap();
}
/// Un VRAI FLAC de la caisse, annoncé `sample_rate`/`bit_depth` en base (la
/// base peut différer du fichier : c'est la décision prise sur l'annonce
/// qu'on mesure, pas le décodage).
fn piste_flac(backend: &Arc<dyn DbBackend>, sample_rate: i32, bit_depth: i32) -> i64 {
    let chemin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tune-core/tests/fixtures/test.flac"
    );
    let mut t = tune_core::db::models::Track::new("Piste 3183".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(sample_rate);
    t.bit_depth = Some(bit_depth);
    t.channels = 2;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap()
}
/// La décision de l'orchestrateur pour la piste, par sa porte publique, sur la
/// MÊME base que le miroir. Aucune sortie ni service : la zone DLNA adresse un
/// appareil inconnu du registre, réputé accepter tout MIME (comportement de
/// production sans sonde).
async fn decision(
    backend: &Arc<dyn DbBackend>,
    zone_id: i64,
    track_id: i64,
) -> tune_core::orchestrator::ResolvedQueueItem {
    tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(backend.clone())
        .append(
            zone_id,
            &[tune_core::db::play_queue_repo::QueueInput::Local { track_id }],
        )
        .unwrap();
    let orch = tune_core::orchestrator::PlaybackOrchestrator::new(
        backend.clone(),
        Arc::new(tune_core::playback::PlaybackManager::new()),
        Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            tune_core::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            tune_core::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.resolve_queue_item_url(zone_id, 0).await.unwrap()
}
/// La même piste vue par le panneau : en lecture, sans session de flux — le
/// cas où le miroir n'a que les règles pour répondre.
fn en_lecture(track_id: i64, format: &str, sample_rate: u32, bit_depth: u32) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            track_id: Some(track_id),
            title: "Piste 3183".into(),
            source: "local".into(),
            format: Some(format.into()),
            sample_rate: Some(sample_rate),
            bit_depth: Some(bit_depth),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}
fn verdict(sp: &Value) -> Option<bool> {
    sp.get("bit_perfect").and_then(Value::as_bool)
}
/// Le plafond 16 bits du catalogue : la décision transcode en 16 bits, le
/// panneau doit rendre le même verdict.
#[tokio::test]
async fn le_plafond_16_bits_du_catalogue_fait_tomber_le_verdict_comme_l_orchestrateur() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    choisir_l_appareil(&backend, zone_id, "Ruark Audio", "R3");
    assert!(
        !ZoneRepo::with_backend(backend.clone()).get_dlna_cap_16bit(zone_id),
        "témoin : aucun drapeau de zone, seul le catalogue plafonne"
    );
    let track_id = piste_flac(&backend, 96_000, 24);
    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(r.mime_type, "audio/flac");
    assert_eq!(
        r.bit_depth,
        Some(16),
        "l'orchestrateur applique `force_16bit` du catalogue : 16 bits servis"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "flac", 96_000, 24),
        &zone,
        &backend,
        Some("Ruark R3"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(
        verdict(&sp),
        Some(false),
        "l'orchestrateur transcode en 16 bits, le panneau annonçait un passthrough \
         bit-perfect (#3183) : {sp}"
    );
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("FLAC 96kHz/24bit \u{2192} FLAC 96kHz/16bit"),
        "l'étape de transcodage doit montrer la profondeur réellement servie"
    );
}
/// Le cas du ticket, mot pour mot : ALAC 24 bits, « ALAC direct » coché, Ruark
/// R3 au catalogue. L'orchestrateur refuse le passthrough (`!dlna_cap_16bit`
/// dans sa condition `alac_passthrough`) et transcode en FLAC 16 bits.
#[test]
fn alac_direct_sur_un_ruark_r3_du_catalogue_n_est_pas_bit_perfect() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    ZoneRepo::with_backend(backend.clone())
        .update_alac_passthrough(zone_id, true)
        .unwrap();
    choisir_l_appareil(&backend, zone_id, "Ruark Audio", "R3");
    let sp = build_signal_path(
        &alac_hires_playing(),
        &zone,
        &backend,
        Some("Ruark R3"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(
        verdict(&sp),
        Some(false),
        "un Ruark R3 ne décode que 16 bits : le passthrough ALAC 24 bits est refusé \
         par l'orchestrateur, le panneau doit le dire (#3183) : {sp}"
    );
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} FLAC 96kHz/16bit")
    );
}
/// Témoin : le même réglage sur un appareil du catalogue SANS quirk (WiiM Pro,
/// `quirks: {}`) garde le passthrough ALAC bit-perfect. Le plafond vient du
/// quirk, pas du seul fait d'avoir choisi un modèle.
#[test]
fn alac_direct_sur_un_appareil_sans_quirk_reste_bit_perfect() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    ZoneRepo::with_backend(backend.clone())
        .update_alac_passthrough(zone_id, true)
        .unwrap();
    choisir_l_appareil(&backend, zone_id, "WiiM", "WiiM Pro");
    let sp = build_signal_path(
        &alac_hires_playing(),
        &zone,
        &backend,
        Some("WiiM Pro"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(verdict(&sp), Some(true), "{sp}");
    assert_eq!(
        transcoder_desc(&sp),
        None,
        "aucun transcodage : l'ALAC part tel quel"
    );
}
/// Le plafond de fréquence du catalogue (WiiM Mini, 48 kHz) : l'orchestrateur
/// le combine en `min` avec le réglage de zone et rééchantillonne ; le miroir
/// ne lisait que la zone et annonçait un passthrough 96 kHz bit-perfect.
#[tokio::test]
async fn le_plafond_de_frequence_du_catalogue_fait_tomber_le_verdict_comme_l_orchestrateur() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    choisir_l_appareil(&backend, zone_id, "WiiM", "WiiM Mini");
    assert!(
        zone.max_sample_rate.is_none(),
        "témoin : aucun plafond de zone, seul le catalogue plafonne"
    );
    let track_id = piste_flac(&backend, 96_000, 24);
    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.sample_rate,
        Some(48_000),
        "l'orchestrateur applique le plafond de 48 kHz du catalogue"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "flac", 96_000, 24),
        &zone,
        &backend,
        Some("WiiM Mini"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(
        verdict(&sp),
        Some(false),
        "l'orchestrateur rééchantillonne à 48 kHz, le panneau annonçait 96 kHz \
         bit-perfect (#3183) : {sp}"
    );
    assert_eq!(
        step_desc(&sp, "Resampler").as_deref(),
        Some("96kHz \u{2192} 48kHz")
    );
}

// ------------------------------------------------------------------
// #3183 — les deux écarts qui restaient au tag v0.9.151, confrontés à la
// VRAIE décision sur la même base, la même zone et la même piste.
//
// Écart n° 1 : « ALAC direct » coché ET un plafond de fréquence dépassé.
// L'orchestrateur rééchantillonne (le renderer ne lit pas au-dessus du
// plafond), donc transcode en FLAC ; ce miroir recopiait la condition du
// passthrough à la main — CINQUIÈME copie — sans le plafond, et annonçait de
// l'ALAC direct sur un fil de FLAC. Les deux côtés appellent désormais
// `alac_passthrough_applies`.
//
// Écart n° 2 : une zone `diretta` n'est PAS une sortie réseau, et ce n'est
// pas un oubli : c'est une sortie PULL qui décode elle-même. Le passthrough
// n'y a pas d'objet, et l'y armer jetterait l'égaliseur (#1393). Les deux
// côtés doivent le dire pareil, case cochée ou non.

/// Un VRAI ALAC de la caisse, annoncé `sample_rate`/`bit_depth` en base.
fn piste_alac(backend: &Arc<dyn DbBackend>, sample_rate: i32, bit_depth: i32) -> i64 {
    let chemin = if sample_rate > 48_000 {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/alac/ref_24_96000_stereo.m4a"
        )
    } else {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/alac/ref_16_44100_stereo.m4a"
        )
    };
    let mut t = tune_core::db::models::Track::new("Piste ALAC 3183".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("alac".into());
    t.sample_rate = Some(sample_rate);
    t.bit_depth = Some(bit_depth);
    t.channels = 2;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap()
}

/// Écart n° 1 : ALAC 96 kHz/24 bits, « ALAC direct » coché, plafond de zone à
/// 48 kHz. L'orchestrateur sert du FLAC 48 kHz ; le panneau doit montrer le
/// même fil — un transcodage ALAC → FLAC 48 kHz, pas un ALAC direct
/// rééchantillonné (un conteneur que Tune ne sait pas écrire).
#[tokio::test]
async fn alac_direct_au_dessus_du_plafond_de_frequence_est_transcode_comme_l_orchestrateur() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    let repo = ZoneRepo::with_backend(backend.clone());
    repo.update_alac_passthrough(zone_id, true).unwrap();
    repo.update_max_sample_rate(zone_id, Some(48_000)).unwrap();
    // Le miroir lit le plafond sur la zone : la relire après l'écriture.
    let zone = repo.get(zone_id).unwrap().unwrap();
    let track_id = piste_alac(&backend, 96_000, 24);

    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.mime_type, "audio/flac",
        "l'orchestrateur ne sert pas un ALAC 96 kHz à une zone plafonnée à 48 kHz"
    );
    assert_eq!(r.sample_rate, Some(48_000));

    let sp = build_signal_path(
        &en_lecture(track_id, "alac", 96_000, 24),
        &zone,
        &backend,
        Some("Renderer plafonné"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(
        verdict(&sp),
        Some(false),
        "le fil porte du FLAC rééchantillonné : pas bit-perfect : {sp}"
    );
    assert_eq!(
        transcoder_desc(&sp).as_deref(),
        Some("ALAC 96kHz/24bit \u{2192} FLAC 48kHz/24bit"),
        "le panneau annonçait un ALAC direct sur un fil de FLAC (#3183, écart n° 1) : {sp}"
    );
    assert_eq!(
        step_desc(&sp, "Resampler").as_deref(),
        Some("96kHz \u{2192} 48kHz")
    );
}

/// Témoin de l'écart n° 1 : le MÊME réglage, plafond relevé à 96 kHz — la
/// préférence s'applique, des deux côtés.
#[tokio::test]
async fn alac_direct_sous_le_plafond_de_frequence_part_tel_quel_comme_l_orchestrateur() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    let repo = ZoneRepo::with_backend(backend.clone());
    repo.update_alac_passthrough(zone_id, true).unwrap();
    repo.update_max_sample_rate(zone_id, Some(96_000)).unwrap();
    let zone = repo.get(zone_id).unwrap().unwrap();
    let track_id = piste_alac(&backend, 96_000, 24);

    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.mime_type, "audio/mp4",
        "sous le plafond, l'ALAC part direct"
    );

    let sp = build_signal_path(
        &en_lecture(track_id, "alac", 96_000, 24),
        &zone,
        &backend,
        Some("Renderer plafonné"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(verdict(&sp), Some(true), "{sp}");
    assert_eq!(transcoder_desc(&sp), None, "aucun transcodage : {sp}");
    assert_eq!(step_desc(&sp, "Resampler"), None);
}

/// Écart n° 2 : ALAC 44,1/16, « ALAC direct » coché, zone `diretta`. Sans
/// traitement, l'orchestrateur sert le fichier tel quel (la sortie PULL le
/// décode) et le panneau le dit bit-perfect. Égaliseur armé, l'orchestrateur
/// transcode pour qu'il s'entende (#1393) et le panneau annonce l'EQ actif.
/// La case n'entre dans aucune des deux décisions — c'est ce qu'un ajout de
/// `diretta` à `is_network_output_type` casserait, en rendant l'égaliseur
/// muet sur le second cas.
#[tokio::test]
async fn alac_direct_sur_une_zone_diretta_suit_l_orchestrateur_avec_et_sans_eq() {
    let (backend, zone) = diretta_zone();
    let zone_id = zone.id.unwrap();
    ZoneRepo::with_backend(backend.clone())
        .update_alac_passthrough(zone_id, true)
        .unwrap();
    let track_id = piste_alac(&backend, 44_100, 16);

    let sans_eq = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        sans_eq.mime_type, "audio/mp4",
        "sans traitement, la sortie PULL reçoit le fichier tel quel"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "alac", 44_100, 16),
        &zone,
        &backend,
        Some("Diretta Host"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(verdict(&sp), Some(true), "fil intact : {sp}");
    assert_eq!(transcoder_desc(&sp), None, "{sp}");

    armer_l_eq(&backend, zone_id);
    let avec_eq = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        avec_eq.mime_type, "audio/flac",
        "égaliseur armé : la sortie PULL reçoit le flux traité, case ou pas \
         (#1393) — `diretta` n'est pas une sortie réseau (#3183, écart n° 2)"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "alac", 44_100, 16),
        &zone,
        &backend,
        Some("Diretta Host"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(verdict(&sp), Some(false), "l'EQ touche le flux : {sp}");
    let dsp = step_desc(&sp, "DSP").expect("l'étape DSP doit être présente");
    assert!(dsp.starts_with("EQ actif"), "{dsp}");
}

// ------------------------------------------------------------------
// #3183 — la QUATRIÈME copie à la main, celle que le tableau du ticket ne
// comptait pas.
//
// Les trois lignes de l'écart n° 3 (liste « sortie réseau », forçage WAV,
// plafond 16 bits) ont chacune été réconciliées en extrayant la condition
// dans `tune_core::orchestrator`. `needs_transcode_for_output` était la
// quatrième, et elle avait DÉJÀ dérivé : la décision choisit entre
// `needs_transcode_for_chromecast()` et `needs_transcode_for_dlna()` selon le
// type de la zone, ce miroir n'appelait que la seconde.
//
// L'écart porte sur un seul couple, et il est réel : le Default Media
// Receiver ne décode pas l'AIFF (#1210, Mika, BeoPlay A9 via CAST).

/// Une zone `chromecast`, sur une base migrée — comme `dlna_zone()`, l'autre
/// type.
fn chromecast_zone() -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Cast", Some("chromecast"), Some("dev-cast"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (backend, zone)
}

/// Un VRAI AIFF de la caisse, annoncé 44,1/16 en base.
fn piste_aiff(backend: &Arc<dyn DbBackend>) -> i64 {
    let chemin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tune-core/tests/fixtures/test.aiff"
    );
    let mut t = tune_core::db::models::Track::new("Piste 3183 AIFF".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("aiff".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap()
}

/// Le cas divergent, confronté à la VRAIE décision sur la MÊME base : un AIFF
/// sur une zone Chromecast. L'orchestrateur transcode en FLAC ; le panneau
/// annonçait un passthrough bit-perfect.
#[tokio::test]
async fn un_aiff_sur_une_zone_chromecast_tombe_comme_l_orchestrateur() {
    let (backend, zone) = chromecast_zone();
    let zone_id = zone.id.unwrap();
    let track_id = piste_aiff(&backend);
    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.mime_type, "audio/flac",
        "l'orchestrateur transcode l'AIFF en FLAC pour un Chromecast (#1210)"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "aiff", 44_100, 16),
        &zone,
        &backend,
        Some("BeoPlay A9"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(
        verdict(&sp),
        Some(false),
        "l'orchestrateur transcode, le panneau annonçait un passthrough \
         bit-perfect (#3183, quatrième copie) : {sp}"
    );
    assert!(
        transcoder_desc(&sp).is_some(),
        "l'étape de transcodage doit apparaître : {sp}"
    );
}

/// Contre-épreuve : le MÊME AIFF, la même piste, sur une zone `dlna`. Là, le
/// renderer le joue direct — la décision ne transcode pas, et le panneau doit
/// dire passthrough. Sans cette moitié, l'épreuve ci-dessus resterait verte
/// avec un miroir qui annoncerait « transcodage » pour toutes les zones.
#[tokio::test]
async fn le_meme_aiff_sur_une_zone_dlna_reste_un_passthrough() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    let track_id = piste_aiff(&backend);
    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.mime_type, "audio/aiff",
        "un renderer DLNA joue l'AIFF direct : aucun transcodage"
    );
    let sp = build_signal_path(
        &en_lecture(track_id, "aiff", 44_100, 16),
        &zone,
        &backend,
        Some("Marantz SR7009"),
        "",
        None,
    )
    .unwrap();
    assert_eq!(verdict(&sp), Some(true), "{sp}");
    assert_eq!(
        transcoder_desc(&sp),
        None,
        "aucun transcodage : l'AIFF part tel quel"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// REF-6b (#2219) — le chemin du signal préfère ce que la sortie a RÉELLEMENT
// fait à ce que les réglages prédisent.
// ───────────────────────────────────────────────────────────────────────────

/// Une sortie qui a mesuré : FLAC 96 kHz stéréo entré, périphérique ouvert à
/// 48 kHz — ou ailleurs, selon le témoin.
fn transformations_mesurees(
    cadence_ouverte: u32,
    canaux_ouverts: u16,
    dsp_actif: bool,
) -> TransformationsReelles {
    use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
    let entree = AudioSpec::nouvelle(96_000, ProfondeurPcm::Entier24, 2).unwrap();
    TransformationsReelles::nouvelles(
        entree,
        FormatOuvert::new(cadence_ouverte, canaux_ouverts),
        dsp_actif,
    )
}

/// (a) Les réglages prédisent « bit-perfect » — aucun plafond, aucun DSP,
/// aucun mono, FLAC servi tel quel — mais la sortie déclare avoir ouvert le
/// périphérique à 48 kHz. Le verdict tombe et l'étape est nommée MESURÉE,
/// depuis les cadences réelles, pas depuis un plafond qui n'existe pas.
#[test]
fn un_reechantillonnage_mesure_fait_tomber_le_verdict_bit_perfect_predit() {
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    ps.transformations_reelles = Some(transformations_mesurees(48_000, 2, false));

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        sp.get("bit_perfect").and_then(|b| b.as_bool()),
        Some(false),
        "la sortie a rééchantillonné : le verdict prédit ne tient plus"
    );
    assert_eq!(
        step_desc(&sp, "Resampler").as_deref(),
        Some("96kHz \u{2192} 48kHz (mesuré)"),
        "l'étape nomme la cadence réellement ouverte, et dit qu'elle est mesurée"
    );
    // Le badge qualité reste vert : la SOURCE n'a pas perdu son caractère
    // sans perte, seule la promesse bit-perfect est retirée.
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));
}

/// (b) La même zone, sans transformations déclarées, rend EXACTEMENT le
/// résultat d'avant : le JSON du témoin `sortie_mono_desarmee_ninvente_aucune_etape`,
/// verdict bit-perfect compris. `None` ne change rien — c'est ce qui garde
/// les 63 témoins précédents verts sans y toucher.
#[test]
fn sans_transformations_declarees_le_chemin_est_strictement_celui_d_avant() {
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    ps.transformations_reelles = None;

    let attendu = build_signal_path(
        &flac_playing(),
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(sp, attendu, "None doit être strictement neutre");
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    assert_eq!(step_desc(&sp, "Resampler"), None);
    assert_eq!(step_desc(&sp, "Canaux"), None);
    assert_eq!(step_desc(&sp, "DSP"), None);
}

/// Une sortie qui a mesuré « rien » — même cadence, mêmes canaux, pas de
/// DSP — laisse le verdict prédit intact : déclarer, ce n'est pas dégrader.
#[test]
fn des_transformations_mesurees_nulles_ne_font_pas_tomber_le_verdict() {
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    ps.transformations_reelles = Some(transformations_mesurees(96_000, 2, false));

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(true));
    assert_eq!(step_desc(&sp, "Resampler"), None);
    assert_eq!(step_desc(&sp, "Canaux"), None);
}

/// DSP et adaptation de canaux déclarés : chacun porte son étape « (mesuré) »
/// et le verdict tombe, alors qu'aucun réglage de zone ne les prédit.
#[test]
fn un_dsp_et_une_adaptation_de_canaux_mesures_portent_chacun_leur_etape() {
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    ps.transformations_reelles = Some(transformations_mesurees(96_000, 8, true));

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
    assert_eq!(
        step_desc(&sp, "Resampler"),
        None,
        "même cadence : pas de rééchantillonnage"
    );
    assert_eq!(
        step_desc(&sp, "DSP").as_deref(),
        Some("DSP appliqué (mesuré)")
    );
    assert_eq!(
        step_desc(&sp, "Canaux").as_deref(),
        Some("2 \u{2192} 8 canaux (mesuré)")
    );
}

/// #3632 — un FLAC 5.1 dont la sortie a ouvert les SIX voies, telles quelles :
/// le chemin du signal le dit, nomme la disposition, et le verdict tient —
/// aucun échantillon n'a été mixé. Contre-témoin dans le même test : la même
/// source repliée en stéréo par le périphérique porte l'étape d'adaptation
/// « 6 → 2 » et perd le verdict, comme avant.
#[test]
fn une_sortie_multicanal_mesuree_porte_son_etape_et_garde_le_verdict() {
    use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    let entree_5_1 = AudioSpec::nouvelle(96_000, ProfondeurPcm::Entier24, 6).unwrap();
    ps.transformations_reelles = Some(TransformationsReelles::nouvelles(
        entree_5_1,
        FormatOuvert::new(96_000, 6),
        false,
    ));

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("HDMI"),
        "CPAL",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();

    assert_eq!(
        step_desc(&sp, "Canaux").as_deref(),
        Some("6 canaux (5.1), sortie multicanal (mesuré)"),
        "le périphérique a ouvert les six voies : le chemin doit le DIRE, sinon un \
         5.1 joué en 5.1 et un 5.1 replié en stéréo affichent la même chose"
    );
    assert_eq!(
        sp.get("bit_perfect").and_then(|b| b.as_bool()),
        Some(true),
        "six voies entrées, six voies ouvertes : rien n'a été mixé, le verdict tient"
    );

    // Contre-témoin : le même 5.1 replié en stéréo par le périphérique.
    ps.transformations_reelles = Some(TransformationsReelles::nouvelles(
        entree_5_1,
        FormatOuvert::new(96_000, 2),
        false,
    ));
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("HDMI"),
        "CPAL",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(
        step_desc(&sp, "Canaux").as_deref(),
        Some("6 \u{2192} 2 canaux (mesuré)"),
        "replié : l'étape d'adaptation, pas l'étape multicanal"
    );
    assert_eq!(sp.get("bit_perfect").and_then(|b| b.as_bool()), Some(false));
}

// ───────────────────────────────────────────────────────────────────────────
// REF-6b côté PRODUCTEUR (#2219, REF-7) — la sortie locale remplit le contrat
// que #3987 avait posé sans producteur : `EtageDeConversion::transformations()`
// -> `publier_les_transformations` -> `LocalOutput::transformations_reelles()`
// -> sondeur -> `ZoneState::transformations_reelles` -> cette route.
// ───────────────────────────────────────────────────────────────────────────

/// Une zone LOCALE dont le périphérique a ouvert 48 kHz sur une source 44,1 :
/// la mesure que l'étage rend pour cette ouverture — au bit près celle que
/// `une_source_44_1_sur_un_peripherique_ouvert_a_48_declare_le_reechantillonnage`
/// (`tune-core`, `local/transformations_reelles_de_l_etage_ref6b.rs`) vérifie —
/// fait tomber le verdict et nomme l'étape mesurée.
///
/// Sans producteur, la même zone rendait `bit_perfect = true` : aucun plafond,
/// aucun DSP, la déduction ne voit pas qu'un DAC ouvert à 48 kHz a
/// rééchantillonné le 44,1.
#[test]
fn une_zone_locale_ouverte_a_48_khz_sur_une_source_44_1_rend_le_reechantillonnage_mesure() {
    use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
    let (backend, zone) = local_zone_migrated();
    let mut ps = flac_playing();
    if let Some(np) = ps.now_playing.as_mut() {
        np.sample_rate = Some(44_100);
        np.bit_depth = Some(16);
    }
    // Ce que l'étage flottant rend pour (44,1 kHz / 16 bits / stéréo) ouvert à
    // (48 kHz / stéréo) sans DSP — la valeur du témoin tune-core, recopiée.
    ps.transformations_reelles = Some(TransformationsReelles::nouvelles(
        AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).unwrap(),
        FormatOuvert::new(48_000, 2),
        false,
    ));

    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CPAL",
        Some(&wire("flac", 44_100, 16)),
    )
    .unwrap();

    assert_eq!(
        sp.get("bit_perfect").and_then(|b| b.as_bool()),
        Some(false),
        "le périphérique a ouvert 48 kHz : le 44,1 est rééchantillonné, le verdict tombe"
    );
    assert_eq!(
        step_desc(&sp, "Resampler").as_deref(),
        Some("44kHz \u{2192} 48kHz (mesuré)"),
        "l'étape nomme la cadence réellement ouverte et dit qu'elle est mesurée"
    );
    assert_eq!(sp.get("lossless").and_then(|b| b.as_bool()), Some(true));

    // Contre-témoin dans le même test : la même zone SANS mesure garde la
    // déduction — et la déduction, ici, ne voit rien.
    ps.transformations_reelles = None;
    let sans = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CPAL",
        Some(&wire("flac", 44_100, 16)),
    )
    .unwrap();
    assert_eq!(
        sans.get("bit_perfect").and_then(|b| b.as_bool()),
        Some(true),
        "sans producteur, la route ne peut pas savoir : c'est exactement le trou que \
         REF-7 côté producteur ferme"
    );
}

/// Les SITES de publication, gardés par texte — `local.rs` vit derrière
/// `local-audio`, que le job `Test` n'active pas (#2816, témoin endormi) ; ce
/// test-ci s'exécute sur chaque PR.
///
/// Deux publications, pas une de plus ni de moins : à l'ouverture (dès que
/// l'étage existe, AVANT que le puits ne soit pris, donc avant la première
/// trame), et à la frontière gapless (dès que l'étage porte le format de la
/// piste enchaînée, AVANT la reconstruction du convolveur). Et `LocalOutput`
/// rend le créneau par la méthode du contrat.
#[test]
fn le_fil_de_lecture_local_publie_ses_transformations_a_l_ouverture_et_a_chaque_frontiere_gapless()
{
    const LOCAL_RS: &str = include_str!("../../../../tune-core/src/outputs/local.rs");
    const PUBLICATION: &str = "publier_les_transformations(&transformations_reelles, &etage);";
    let production = LOCAL_RS
        .split("#[cfg(test)]\nmod tests")
        .next()
        .expect("local.rs doit garder son `#[cfg(test)] mod tests`");

    assert_eq!(
        production.matches(PUBLICATION).count(),
        2,
        "REF-6b — `play_url` doit publier les transformations de l'étage à DEUX endroits : \
         l'ouverture et la frontière gapless"
    );

    let ouverture = production
        .find("sortie: FormatOuvert::new(output_sr, output_ch),")
        .expect("la construction de l'étage à l'ouverture doit rester identifiable");
    let apres_ouverture = &production[ouverture..];
    let publiee = apres_ouverture
        .find(PUBLICATION)
        .expect("REF-6b — aucune publication après la construction de l'étage");
    let puits_pris = apres_ouverture
        .find("let mut puits = match backend.puits()")
        .expect("la prise du puits doit rester identifiable");
    assert!(
        publiee < puits_pris,
        "REF-6b — la publication d'ouverture doit précéder la prise du puits, donc la \
         première trame servie : l'écran ne doit pas voir une déduction pendant que le DAC \
         joue déjà une mesure"
    );

    let gapless = production
        .find("etage.spec = nouvelle_spec;")
        .expect("la mise à jour gapless de l'étage doit rester identifiable");
    let apres_gapless = &production[gapless..];
    let republiee = apres_gapless
        .find(PUBLICATION)
        .expect("REF-6b — aucune publication après la mise à jour gapless de l'étage");
    let reconstruction = apres_gapless
        .find("rebuild_local_convolver(")
        .expect("la reconstruction du convolveur doit rester identifiable");
    assert!(
        republiee < reconstruction,
        "REF-6b — la publication gapless doit suivre immédiatement la nouvelle spec, avant \
         la reconstruction du convolveur"
    );

    assert!(
        production
            .contains("fn transformations_reelles(&self) -> Option<TransformationsReelles> {")
            && production.contains("self.transformations_reelles.lock().ok().and_then(|t| *t)"),
        "REF-6b — `LocalOutput` doit rendre le créneau par `OutputTarget::transformations_reelles`"
    );
    for effacement in ["fn stop(", "async fn play_url("] {
        let debut = production
            .find(effacement)
            .unwrap_or_else(|| panic!("`{effacement}` doit rester identifiable"));
        let fenetre = &production[debut..(debut + 12_000).min(production.len())];
        assert!(
            fenetre.contains("if let Ok(mut slot) = self.transformations_reelles.lock() {"),
            "REF-6b — `{effacement}` doit effacer le créneau : une mesure de la piste \
             précédente ne doit pas survivre à son arrêt"
        );
    }
}

#[test]
fn replaygain_peak_kind_keeps_unity_informational_and_true_peak_exact() {
    let (backend, zone) = dlna_zone_migrated();
    let (tid, ps) = flac_track_with_rg_tag(&backend, "0");
    let meta = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone());
    meta.set(tid, "rg_track_peak", "0.5").unwrap();
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
        step_field(&sp, "ReplayGain", "peak_headroom_db").and_then(|v| v.as_f64()),
        Some(3.0)
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "bit_perfect").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(sp["bit_perfect"], true);

    meta.set(tid, "rg_track_gain", "+6").unwrap();
    meta.set(tid, "rg_track_true_peak", "1.2").unwrap();
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
        step_field(&sp, "ReplayGain", "peak_kind").and_then(|v| v.as_str()),
        Some("true_peak")
    );
    assert_eq!(
        step_field(&sp, "ReplayGain", "peak_headroom_db").and_then(|v| v.as_f64()),
        Some(0.0)
    );
    assert!(step_desc(&sp, "ReplayGain").unwrap().contains("-1.6 dB"));
    assert!(
        step_desc(&sp, "ReplayGain")
            .unwrap()
            .contains("crête vraie disponible")
    );
}

// #4346: use the public JSON builder, including its global verdict.
#[test]
fn radio_4346_signal_path_preserves_source_codec_and_output_container() {
    use tune_core::http::streamer::RadioSourceInfo;
    let (backend, mut zone) = dlna_zone();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            source: "radio".into(),
            format: Some("wav".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    for (codec, rate, bits, expected, lossless) in [
        (Some("mp3"), Some(44_100), None, "MP3 44kHz", false),
        (Some("aac"), Some(22_050), None, "AAC 22kHz", false),
        (
            Some("flac"),
            Some(48_000),
            Some(16),
            "FLAC 48kHz/16bit",
            true,
        ),
        (None, None, None, "Unknown", false),
    ] {
        let stream = StreamInfo {
            radio_source: Some(RadioSourceInfo {
                format: codec,
                sample_rate: rate,
                bit_depth: bits,
            }),
            ..wire("wav", rate.unwrap_or(44_100).max(44_100), 16)
        };
        for output in ["local", "oaat", "dlna"] {
            zone.output_type = Some(output.into());
            let sp = build_signal_path(
                &ps,
                &zone,
                &backend,
                Some("Renderer"),
                "none",
                Some(&stream),
            )
            .unwrap();
            assert_eq!(
                step_desc(&sp, "Source").as_deref(),
                Some(expected),
                "radio source codec, not WAV"
            );
            assert_eq!(
                sp["lossless"], lossless,
                "WAV decoding must not make a lossy radio lossless"
            );
            if !lossless {
                assert_eq!(
                    sp["bit_perfect"], false,
                    "lossy or unknown radio cannot claim bit-perfect"
                );
            }
            assert!(step_desc(&sp, "Decoder").is_some());
        }
    }
}

#[test]
fn radio_4346_without_probe_cannot_claim_lossless() {
    let (backend, zone) = dlna_zone();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            source: "radio".into(),
            format: Some("wav".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    for stream in [None, Some(wire("wav", 44_100, 16))] {
        let sp = build_signal_path(&ps, &zone, &backend, None, "none", stream.as_ref()).unwrap();
        assert_eq!(
            sp["lossless"], false,
            "pending radio codec must not be inferred from WAV"
        );
        assert_eq!(sp["bit_perfect"], false);
        assert_eq!(step_desc(&sp, "Source").as_deref(), Some("Unknown"));
    }
}

#[test]
fn radio_4346_verbatim_proxy_retains_its_known_codec() {
    let (backend, zone) = dlna_zone();
    for (codec, lossless) in [("mp3", false), ("flac", true)] {
        let ps = ZoneState {
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                source: "radio".into(),
                format: Some(codec.into()),
                sample_rate: Some(48_000),
                bit_depth: Some(16),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sp = build_signal_path(
            &ps,
            &zone,
            &backend,
            None,
            "none",
            Some(&wire(codec, 48_000, 16)),
        )
        .unwrap();
        assert_eq!(
            sp["lossless"], lossless,
            "verbatim proxy source must keep its codec"
        );
    }
}

#[test]
fn radio_4346_flac_truncated_before_local_output_is_not_bit_perfect() {
    use tune_core::http::streamer::RadioSourceInfo;
    let (backend, mut zone) = dlna_zone();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            source: "radio".into(),
            format: Some("wav".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    for output in ["local", "oaat", "dlna"] {
        zone.output_type = Some(output.into());
        let stream = StreamInfo {
            radio_source: Some(RadioSourceInfo {
                format: Some("flac"),
                sample_rate: Some(96_000),
                bit_depth: Some(24),
            }),
            ..wire("wav", 96_000, 16)
        };
        let sp = build_signal_path(&ps, &zone, &backend, None, "ALSA", Some(&stream)).unwrap();
        assert_eq!(sp["lossless"], true, "FLAC remains a lossless source");
        assert_eq!(
            sp["bit_perfect"], false,
            "{output}: 24-bit source truncated to 16-bit before output"
        );
    }
}

// ── #4172 — WASAPI sans contrat exclusif = mode partagé, nommé et non bit-perfect ──

/// Le témoin : « WASAPI » sans contrat de signal se nomme partagé et n'est
/// pas intact — avant, il se nommait « WASAPI » tout court et passait pour
/// bit-perfect, quelle que soit la cadence à laquelle le mixeur Windows
/// sortait réellement.
#[test]
fn wasapi_sans_contrat_exclusif_se_nomme_partage_et_n_est_pas_bit_perfect_4172() {
    use super::signal_path::{etiquette_du_transport_local, transport_partage_est_intact};
    assert_eq!(
        etiquette_du_transport_local("WASAPI", false),
        "WASAPI (shared \u{2014} Windows mixer)"
    );
    assert_eq!(
        etiquette_du_transport_local("WASAPI", true),
        "WASAPI (exclusive)"
    );
    assert_eq!(
        etiquette_du_transport_local("ASIO", true),
        "ASIO (exclusive)"
    );
    assert_eq!(
        etiquette_du_transport_local("CoreAudio", false),
        "CoreAudio"
    );
    assert_eq!(etiquette_du_transport_local("ALSA", false), "ALSA");
    assert!(!transport_partage_est_intact("WASAPI"), "mixeur Windows");
    assert!(transport_partage_est_intact("CoreAudio"), "inchangé");
    assert!(transport_partage_est_intact("ALSA"), "inchangé");
}

/// La garde du BRANCHEMENT : le bras `"local"` de `decrire_le_transport`
/// passe par l'étiquette et, sans contrat, par le verdict du mode partagé.
#[test]
fn le_transport_local_dit_son_mode_et_son_verdict_4172() {
    let src = include_str!("signal_path.rs");
    let bras = src.find("\"local\" => {").expect("le bras local");
    let bloc = &src[bras..bras + 1_500];
    assert!(
        bloc.contains("etiquette_du_transport_local(audio_backend, exclusif_observe)"),
        "le nom vient de l'étiquette"
    );
    assert!(
        bloc.contains("None => transport_partage_est_intact(audio_backend)"),
        "sans contrat, le verdict est celui du mode partagé"
    );
}

// ── #3973 — « jouer, et le dire » : PURE dégradé par une conversion ──

/// Zone en PURE, la sortie a MESURÉ une conversion 96 → 48 kHz : le chemin du
/// signal publie la conversion (de, vers), l'étape porte son code, et PURE
/// est déclaré DÉGRADÉ — au lieu d'un badge PURE allumé sur un signal
/// rééchantillonné.
#[test]
fn pure_avec_conversion_mesuree_est_declare_degrade_3973() {
    let (backend, zone) = local_zone_migrated();
    let zone_id = zone.id.unwrap();
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let mut ps = flac_playing();
    ps.transformations_reelles = Some(transformations_mesurees(48_000, 2, false));
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(sp["pure"], serde_json::json!(true), "{sp}");
    assert_eq!(
        sp["pure_degraded"],
        serde_json::json!(true),
        "PURE + conversion de fréquence = PURE dégradé : {sp}"
    );
    assert_eq!(
        sp["rate_conversion"],
        serde_json::json!({"from_hz": 96_000, "to_hz": 48_000}),
        "{sp}"
    );
    assert_eq!(sp["bit_perfect"], serde_json::json!(false));
    assert_eq!(sp["strict_bitperfect"], serde_json::json!(false));
    let etape = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Resampler")
        .expect("l'étape Resampler");
    assert_eq!(etape["code"], "rate_conversion", "{etape}");
}

/// Même zone en PURE, aucune conversion : PURE n'est PAS dégradé, et
/// `rate_conversion` est nul — la garde contre un correctif qui crierait au
/// loup.
#[test]
fn pure_sans_conversion_n_est_pas_degrade_3973() {
    let (backend, zone) = local_zone_migrated();
    let zone_id = zone.id.unwrap();
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let mut ps = flac_playing();
    ps.transformations_reelles = Some(transformations_mesurees(96_000, 2, false));
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        Some(&wire("flac", 96_000, 24)),
    )
    .unwrap();
    assert_eq!(sp["pure"], serde_json::json!(true));
    assert_eq!(sp["pure_degraded"], serde_json::json!(false), "{sp}");
    assert!(sp["rate_conversion"].is_null(), "{sp}");
}

// #4350 — un FLAC écrit par ffmpeg (vendeur `Lavf…`) SANS MD5 part ré-encodé
// vers une sortie réseau (`flac_ffmpeg_vers_le_reseau_applies`). Le panneau
// annonçait pourtant un passthrough : « À signaler dans le chemin du signal
// (“conteneur réécrit”), pour ne pas afficher un passthrough qui n'en est pas
// un » — la moitié du ticket restée ouverte après la v0.9.154.

/// Un en-tête FLAC de la forme exacte de l'enregistreur (mesurée sur le .18) :
/// STREAMINFO à MD5 nul (ou non), puis VORBIS_COMMENT au vendeur donné. Le
/// panneau ne lit que ces blocs ; aucune trame n'est décodée.
fn fichier_flac(dir: &std::path::Path, nom: &str, vendeur: &str, md5_nul: bool) -> String {
    let mut f = b"fLaC".to_vec();
    let mut streaminfo = [0u8; 34];
    // Cadence 44 100 Hz, 2 canaux, 16 bits — un STREAMINFO renseigné, comme
    // ceux de l'enregistreur (#4800 lit la cadence pour bâtir l'en-tête neuf).
    streaminfo[10..14].copy_from_slice(&[0x0A, 0xC4, 0x42, 0xF0]);
    if !md5_nul {
        streaminfo[18..34].copy_from_slice(&[0x5a; 16]);
    }
    f.extend_from_slice(&[0x00, 0, 0, 34]);
    f.extend_from_slice(&streaminfo);
    let mut vorbis = (vendeur.len() as u32).to_le_bytes().to_vec();
    vorbis.extend_from_slice(vendeur.as_bytes());
    vorbis.extend_from_slice(&0u32.to_le_bytes());
    let l = vorbis.len() as u32;
    f.extend_from_slice(&[0x80 | 4, (l >> 16) as u8, (l >> 8) as u8, l as u8]);
    f.extend_from_slice(&vorbis);
    f.extend_from_slice(&[0xFF, 0xF8, 0x69, 0x18]);
    let chemin = dir.join(nom);
    std::fs::write(&chemin, f).unwrap();
    chemin.to_string_lossy().into_owned()
}

fn piste_en_fichier(backend: &Arc<dyn DbBackend>, chemin: &str) -> i64 {
    let mut t = tune_core::db::models::Track::new("Enregistrement".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.source = "local".into();
    TrackRepo::with_backend(backend.clone()).create(&t).unwrap()
}

fn etape<'v>(sp: &'v Value, nom: &str) -> Option<&'v Value> {
    sp["steps"].as_array()?.iter().find(|e| e["name"] == nom)
}

/// Le cas du ticket : FLAC de l'enregistreur, zone DLNA. Le panneau doit
/// montrer la réécriture — et la dire sans perte, parce qu'elle l'est.
#[test]
fn un_flac_ffmpeg_vers_le_reseau_annonce_son_conteneur_reecrit_4350() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = fichier_flac(dir.path(), "enregistrement.flac", "Lavf60.16.100", true);
    let (backend, zone) = dlna_zone();
    let tid = piste_en_fichier(&backend, &chemin);
    let sp = build_signal_path(
        &en_lecture(tid, "flac", 44_100, 16),
        &zone,
        &backend,
        Some("Eversolo DMP-A8"),
        "",
        Some(&wire("flac", 44_100, 16)),
    )
    .unwrap();
    let transcoder = etape(&sp, "Transcoder").unwrap_or_else(|| {
        panic!("le conteneur est réécrit : l'étape Transcoder doit paraître — {sp}")
    });
    assert_eq!(
        transcoder["code"], "flac_container_rewritten",
        "{transcoder}"
    );
    assert_eq!(
        transcoder["description"], "FLAC 44kHz/16bit \u{2192} FLAC 44kHz/16bit",
        "{transcoder}"
    );
    assert_eq!(
        transcoder["bit_perfect"],
        serde_json::json!(true),
        "ré-encodé sans perte : mêmes échantillons, {transcoder}"
    );
    // #4800 — l'en-tête de ce fichier se lit, et une trame le suit : le
    // conteneur est réécrit sans décodage, et l'écran le dit.
    assert!(
        transcoder["detail"]
            .as_str()
            .is_some_and(|d| d.contains("trames copiées telles quelles")),
        "{transcoder}"
    );
    assert!(
        sp["summary"].as_str().unwrap().contains("transcode"),
        "le résumé ne doit plus annoncer un passthrough : {sp}"
    );
    assert_eq!(verdict(&sp), Some(true), "aucun échantillon touché : {sp}");
}

/// Les contre-épreuves : ce qui ne part PAS ré-encodé ne doit pas l'annoncer.
/// Même vendeur AVEC un MD5 réel (le FLAC de référence du dépôt), un FLAC
/// libFLAC à MD5 nul, et le fichier de l'enregistreur sur une sortie LOCALE.
#[test]
fn le_conteneur_reecrit_ne_s_annonce_que_la_ou_il_a_lieu_4350() {
    let dir = tempfile::tempdir().unwrap();
    let lavf_md5 = fichier_flac(dir.path(), "lavf-md5.flac", "Lavf62.12.101", false);
    let libflac = fichier_flac(dir.path(), "libflac.flac", "reference libFLAC 1.4.3", true);
    let lavf_nul = fichier_flac(dir.path(), "lavf-nul.flac", "Lavf60.16.100", true);

    let (backend, zone) = dlna_zone();
    for chemin in [&lavf_md5, &libflac] {
        let tid = piste_en_fichier(&backend, chemin);
        let sp = build_signal_path(
            &en_lecture(tid, "flac", 44_100, 16),
            &zone,
            &backend,
            Some("Eversolo DMP-A8"),
            "",
            Some(&wire("flac", 44_100, 16)),
        )
        .unwrap();
        assert!(
            etape(&sp, "Transcoder").is_none(),
            "{chemin} part en passthrough, le panneau ne doit pas dire autre chose : {sp}"
        );
    }

    let (backend, zone) = local_zone_migrated();
    let tid = piste_en_fichier(&backend, &lavf_nul);
    let sp = build_signal_path(
        &en_lecture(tid, "flac", 44_100, 16),
        &zone,
        &backend,
        Some("DAC"),
        "CoreAudio",
        None,
    )
    .unwrap();
    assert!(
        etape(&sp, "Transcoder").is_none(),
        "sortie locale : aucune réécriture de conteneur — {sp}"
    );
}

// ---------------------------------------------------------------------------
// #4573 — la réduction de canaux du chemin RÉSEAU, dite à l'écran.
//
// Mesure de Xavier Joly (20/09/2026, Denon AVR-X1600H) : le Sink annonce du
// LPCM en `channels=1` et `channels=2` seulement. Avant ce lot, Tune lui
// servait le FLAC 5.1 tel quel et l'écran n'en disait rien — `wire.channels`
// n'était lu NULLE PART dans ce panneau, et la sonde `runtime_signal_path`
// est fermée aux zones non locales.
// ---------------------------------------------------------------------------

/// Une piste multicanale en base, et l'état qui la joue. `canaux_du_fil` est
/// ce que la SESSION sert vraiment au renderer : 2 après le repli, 6 sans.
fn piste_multicanale(backend: &Arc<dyn DbBackend>, canaux_source: i32) -> (i64, ZoneState) {
    let mut t = tune_core::db::models::Track::new("Piste 5.1".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(48_000);
    t.bit_depth = Some(24);
    t.channels = canaux_source;
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    let np = NowPlaying {
        title: "Piste 5.1".into(),
        track_id: Some(tid),
        format: Some("flac".into()),
        sample_rate: Some(48_000),
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

fn fil_de_canaux(format: &str, canaux: u16) -> StreamInfo {
    StreamInfo {
        format: format.into(),
        sample_rate: 48_000,
        bit_depth: 24,
        channels: canaux,
        ..Default::default()
    }
}

/// 🔴 LE témoin : 5.1 en base, deux canaux sur le fil, zone DLNA — l'écran le
/// dit. Rouge avant ce lot : aucune étape « Canaux » n'existait pour une zone
/// réseau, quel que soit le contenu du fil.
#[test]
fn le_repli_en_stereo_vers_un_renderer_est_dit_a_l_ecran() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = piste_multicanale(&backend, 6);
    let v = build_signal_path(
        &ps,
        &zone,
        &backend,
        None,
        "CoreAudio",
        Some(&fil_de_canaux("wav", 2)),
    )
    .unwrap();
    assert_eq!(
        step_desc(&v, "Canaux").as_deref(),
        Some("6 \u{2192} 2 canaux (annoncés par le lecteur)")
    );
    let etape = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Canaux")
        .unwrap();
    assert_eq!(
        etape["bit_perfect"], false,
        "un mélange BS.775 n'est pas un passthrough"
    );
}

/// 🔴 La contre-épreuve qui compte : le fil porte les SIX voies — le renderer
/// n'a rien déclaré, donc rien n'a été replié. Pas d'étape, pas d'étiquette.
#[test]
fn un_fil_qui_porte_toutes_les_voies_n_affiche_aucune_reduction() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = piste_multicanale(&backend, 6);
    let v = build_signal_path(
        &ps,
        &zone,
        &backend,
        None,
        "CoreAudio",
        Some(&fil_de_canaux("flac", 6)),
    )
    .unwrap();
    assert_eq!(step_desc(&v, "Canaux"), None);
}

/// Une stéréo ordinaire — l'immense majorité des lectures — ne gagne aucune
/// étape. Sans cette garde, le panneau se serait mis à parler de canaux sur
/// chaque piste.
#[test]
fn une_stereo_ordinaire_ne_gagne_aucune_etape_de_canaux() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = piste_multicanale(&backend, 2);
    let v = build_signal_path(
        &ps,
        &zone,
        &backend,
        None,
        "CoreAudio",
        Some(&fil_de_canaux("flac", 2)),
    )
    .unwrap();
    assert_eq!(step_desc(&v, "Canaux"), None);
}

/// Une session qui ne connaît pas encore ses canaux (`StreamInfo::default`,
/// `channels = 0`) n'est pas une déclaration de silence : rien à dire.
#[test]
fn un_fil_muet_sur_ses_canaux_n_affiche_rien() {
    let (backend, zone) = dlna_zone_migrated();
    let (_tid, ps) = piste_multicanale(&backend, 6);
    let v = build_signal_path(
        &ps,
        &zone,
        &backend,
        None,
        "CoreAudio",
        Some(&fil_de_canaux("flac", 0)),
    )
    .unwrap();
    assert_eq!(step_desc(&v, "Canaux"), None);
}

/// 🔴 #4573 — LA contre-épreuve du branchement, par la porte publique de
/// l'orchestrateur : une piste déclarée à SIX voies en base, une zone DLNA
/// dont l'appareil n'est dans AUCUN registre — donc personne pour déclarer
/// quoi que ce soit. Rien ne doit être replié.
///
/// C'est le cas de l'immense majorité du parc : un renderer dont le Sink ne
/// porte aucun `channels=` (le Denon de Xavier pour `audio/flac:*`) ou qu'on
/// n'a pas pu sonder. Réduire là-dessus ferait taire quatre voies sur six
/// chez quelqu'un qui n'a rien demandé.
#[tokio::test]
async fn sans_declaration_du_renderer_un_51_garde_ses_six_voies() {
    let (backend, zone) = dlna_zone();
    let zone_id = zone.id.unwrap();
    let chemin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tune-core/tests/fixtures/test.flac"
    );
    let mut t = tune_core::db::models::Track::new("Piste 5.1".into());
    t.duration_ms = 1_000;
    t.file_path = Some(chemin.into());
    t.format = Some("flac".into());
    t.sample_rate = Some(48_000);
    t.bit_depth = Some(24);
    // Ce que la DÉCISION lit : la ligne `tracks`, pas le fichier.
    t.channels = 6;
    t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
        .create(&t)
        .unwrap();

    let r = decision(&backend, zone_id, track_id).await;
    assert_eq!(
        r.channels,
        Some(6),
        "aucun renderer n'a déclaré ses canaux : la piste part intacte"
    );
}
