use super::*;

fn mesures() -> MesuresFichier {
    MesuresFichier {
        cadence: 44_100,
        canaux: 2,
        trames: 1_000_000,
        continu_dbfs: vec![-90.0, -90.0],
        correlation: Some(0.4),
        rapport_somme_difference_db: Some(4.0),
        polarite_inversee: false,
        ecart_lr_db: Some(0.1),
        double_mono: false,
        crete_vraie_dbtp: -1.0,
        dr: Some(12.0),
        niveau_rms_dbfs: -18.0,
    }
}

fn cd_preaccentue() -> ContexteAlbum {
    ContexteAlbum {
        album_id: 42,
        annee: Some(1985),
        label: Some("Deutsche Grammophon".into()),
        support: Support::Cd,
        preaccentuation: Some(SourcePreaccentuation::Toc),
        mesures: Some(mesures()),
        ..Default::default()
    }
}

fn etat(r: &ProfilResolu, g: GenreTraitement) -> Option<Etat> {
    r.traitements.iter().find(|t| t.genre == g).map(|t| t.etat)
}

#[test]
fn seule_la_desaccentuation_est_active_par_defaut() {
    let r = resoudre(&profil_vague_1(), &cd_preaccentue(), &[]);
    let actifs: Vec<_> = r.actifs().map(|t| t.genre).collect();
    assert_eq!(actifs, vec![GenreTraitement::Desaccentuation]);
    assert_eq!(etat(&r, GenreTraitement::Loudness), Some(Etat::Propose));
    let d = &r.traitements[0];
    assert_eq!(d.raison.cle, "dsp_contextuel.raison.desaccentuation_cd");
    assert_eq!(d.raison.params["source"], json!("toc"));
    assert_eq!(d.droit, Droit::Gratuit);
}

#[test]
fn mesures_declenchent_les_propositions_avec_leurs_reglages() {
    let mut c = cd_preaccentue();
    let m = c.mesures.as_mut().unwrap();
    m.polarite_inversee = true;
    m.correlation = Some(-0.83);
    m.continu_dbfs = vec![-45.0, -80.0];
    m.ecart_lr_db = Some(-3.4);
    m.crete_vraie_dbtp = 1.24;
    let r = resoudre(&profil_vague_1(), &c, &[]);
    for g in [
        GenreTraitement::Polarite,
        GenreTraitement::SuppressionContinu,
        GenreTraitement::Equilibre,
        GenreTraitement::Reserve,
    ] {
        assert_eq!(etat(&r, g), Some(Etat::Propose), "{g:?}");
    }
    let eq = r
        .traitements
        .iter()
        .find(|t| t.genre == GenreTraitement::Equilibre)
        .unwrap();
    assert_eq!(eq.reglages, Reglages::Equilibre { ecart_db: -3.4 });
    assert_eq!(eq.raison.params["canal"], json!("droit"));
    assert_eq!(eq.raison.params["ecart_db"], json!(3.4));
    let res = r
        .traitements
        .iter()
        .find(|t| t.genre == GenreTraitement::Reserve)
        .unwrap();
    assert_eq!(res.raison.params["crete_dbtp"], json!(1.2));
    assert_eq!(res.raison.params["reserve_db"], json!(1.5));
    let pol = r
        .traitements
        .iter()
        .find(|t| t.genre == GenreTraitement::Polarite)
        .unwrap();
    assert_eq!(pol.raison.params["correlation"], json!(-0.83));
}

#[test]
fn sans_mesures_aucune_condition_de_mesure_n_est_vraie() {
    let mut c = cd_preaccentue();
    c.mesures = None;
    let r = resoudre(&profil_vague_1(), &c, &[]);
    let genres: Vec<_> = r.traitements.iter().map(|t| t.genre).collect();
    assert_eq!(
        genres,
        vec![GenreTraitement::Desaccentuation, GenreTraitement::Loudness]
    );
}

#[test]
fn ordre_canonique_de_la_chaine() {
    let mut c = cd_preaccentue();
    c.support = Support::Vinyle;
    c.mono_declare = true;
    c.origine_dsd = OrigineDsd::ConvertieEnPcm;
    c.mesures.as_mut().unwrap().crete_vraie_dbtp = 0.5;
    let r = resoudre(&profil_vague_1(), &c, &[]);
    let genres: Vec<_> = r.traitements.iter().map(|t| t.genre).collect();
    let mut trie = genres.clone();
    trie.sort();
    assert_eq!(genres, trie);
    assert_eq!(genres.first(), Some(&GenreTraitement::UltrasoniqueDsd));
    assert_eq!(genres.last(), Some(&GenreTraitement::Reserve));
}

#[test]
fn resolution_independante_de_l_ordre_des_regles_et_des_choix() {
    let mut c = cd_preaccentue();
    c.mono_declare = true;
    c.mesures.as_mut().unwrap().double_mono = true;
    let p = profil_vague_1();
    let mut inverse = p.clone();
    inverse.regles.reverse();
    let choix = vec![
        ChoixUtilisateur {
            portee: Portee::Global,
            genre: GenreTraitement::Mono,
            decision: Decision::Jamais,
        },
        ChoixUtilisateur {
            portee: Portee::Album(42),
            genre: GenreTraitement::Mono,
            decision: Decision::Toujours,
        },
    ];
    let mut choix_inverse = choix.clone();
    choix_inverse.reverse();
    let a = resoudre(&p, &c, &choix);
    let b = resoudre(&inverse, &c, &choix_inverse);
    assert_eq!(a, b);
    // Le mono déclaré (priorité 20) l'emporte sur le double-mono mesuré (10).
    let mono = a
        .traitements
        .iter()
        .find(|t| t.genre == GenreTraitement::Mono)
        .unwrap();
    assert_eq!(mono.regle_id.as_deref(), Some("mono_declare"));
    assert_eq!(mono.etat, Etat::Actif);
    assert_eq!(mono.origine_du_choix, OrigineDuChoix::Album);
}

#[test]
fn a_priorite_egale_le_plus_petit_id_gagne() {
    let r = |id: &str, coupure| Regle {
        id: id.into(),
        conditions: vec![Condition::Toujours],
        reglages: Reglages::Rumble {
            coupure_hz: coupure,
            voile: false,
        },
        raison: "x".into(),
        priorite: 5,
        droit: Droit::Gratuit,
        actif_par_defaut: false,
    };
    let p = Profil {
        id: "t".into(),
        regles: vec![r("b", 20.0), r("a", 15.0)],
    };
    let res = resoudre(&p, &ContexteAlbum::default(), &[]);
    assert_eq!(res.traitements[0].regle_id.as_deref(), Some("a"));
}

#[test]
fn choix_album_puis_label_puis_global() {
    let c = cd_preaccentue();
    let p = profil_vague_1();
    let ch = |portee, decision| ChoixUtilisateur {
        portee,
        genre: GenreTraitement::Desaccentuation,
        decision,
    };
    // « Jamais » pour ce label (casse et espaces indifférents).
    let r = resoudre(
        &p,
        &c,
        &[ch(
            Portee::Label("  deutsche GRAMMOPHON ".into()),
            Decision::Jamais,
        )],
    );
    assert_eq!(
        etat(&r, GenreTraitement::Desaccentuation),
        Some(Etat::Refuse)
    );
    // … mais « toujours » pour cet album l'emporte.
    let r = resoudre(
        &p,
        &c,
        &[
            ch(
                Portee::Label("Deutsche Grammophon".into()),
                Decision::Jamais,
            ),
            ch(Portee::Album(42), Decision::Toujours),
        ],
    );
    assert_eq!(
        etat(&r, GenreTraitement::Desaccentuation),
        Some(Etat::Actif)
    );
    // Un choix pour un AUTRE album ne compte pas.
    let r = resoudre(&p, &c, &[ch(Portee::Album(7), Decision::Jamais)]);
    assert_eq!(
        etat(&r, GenreTraitement::Desaccentuation),
        Some(Etat::Actif)
    );
    // À portée égale, « jamais » l'emporte.
    let r = resoudre(
        &p,
        &c,
        &[
            ch(Portee::Global, Decision::Toujours),
            ch(Portee::Global, Decision::Jamais),
        ],
    );
    assert_eq!(
        etat(&r, GenreTraitement::Desaccentuation),
        Some(Etat::Refuse)
    );
}

#[test]
fn toujours_sans_regle_ajoute_le_traitement_par_defaut() {
    let mut c = cd_preaccentue();
    c.support = Support::Cd;
    let r = resoudre(
        &profil_vague_1(),
        &c,
        &[ChoixUtilisateur {
            portee: Portee::Label("Deutsche Grammophon".into()),
            genre: GenreTraitement::Rumble,
            decision: Decision::Toujours,
        }],
    );
    let t = r
        .traitements
        .iter()
        .find(|t| t.genre == GenreTraitement::Rumble)
        .unwrap();
    assert_eq!(t.etat, Etat::Actif);
    assert_eq!(t.regle_id, None);
    assert_eq!(t.raison.cle, CLE_CHOIX_UTILISATEUR);
    assert_eq!(t.raison.params["portee"], json!("label"));
    assert_eq!(t.origine_du_choix, OrigineDuChoix::Label);
}

#[test]
fn mode_pur_n_active_rien() {
    let mut c = cd_preaccentue();
    c.mode_pur = true;
    let r = resoudre(
        &profil_vague_1(),
        &c,
        &[ChoixUtilisateur {
            portee: Portee::Global,
            genre: GenreTraitement::Loudness,
            decision: Decision::Toujours,
        }],
    );
    assert_eq!(r.actifs().count(), 0);
    assert_eq!(
        etat(&r, GenreTraitement::Desaccentuation),
        Some(Etat::Propose)
    );
}

#[test]
fn droit_premium_absent_rend_propose() {
    let mut p = profil_vague_1();
    p.regles[0].droit = Droit::Premium {
        fonction: "profils_sonores".into(),
    };
    let c = cd_preaccentue();
    let r = resoudre(&p, &c, &[]);
    let d = &r.traitements[0];
    assert_eq!((d.etat, d.droit_requis), (Etat::Propose, true));
    let mut c = c;
    c.droits.premium = true;
    let r = resoudre(&p, &c, &[]);
    assert_eq!(r.traitements[0].etat, Etat::Actif);
}

#[test]
fn ultrasonique_jamais_hors_dsd_converti() {
    for origine in [OrigineDsd::Aucune, OrigineDsd::Native, OrigineDsd::Dop] {
        let mut c = cd_preaccentue();
        c.origine_dsd = origine;
        let r = resoudre(
            &profil_vague_1(),
            &c,
            &[ChoixUtilisateur {
                portee: Portee::Global,
                genre: GenreTraitement::UltrasoniqueDsd,
                decision: Decision::Toujours,
            }],
        );
        assert_eq!(
            etat(&r, GenreTraitement::UltrasoniqueDsd),
            None,
            "{origine:?}"
        );
    }
}

#[test]
fn conditions_d_annee_de_label_et_de_genre() {
    let c = ContexteAlbum {
        annee: Some(1962),
        label: Some("Blue Note".into()),
        genres: vec!["Hard Bop".into()],
        ..Default::default()
    };
    assert!(
        Condition::AnneeEntre {
            min: Some(1957),
            max: Some(1967)
        }
        .vraie(&c)
    );
    assert!(
        !Condition::AnneeEntre {
            min: Some(1968),
            max: None
        }
        .vraie(&c)
    );
    assert!(
        Condition::Label {
            un_de: vec!["blue note".into(), "Impulse!".into()]
        }
        .vraie(&c)
    );
    assert!(
        Condition::Genre {
            contient: "bop".into()
        }
        .vraie(&c)
    );
    assert!(
        !Condition::AnneeEntre {
            min: None,
            max: None
        }
        .vraie(&ContexteAlbum::default())
    );
}

#[test]
fn depot_en_memoire() {
    let mut d = DepotEnMemoire::default();
    let ch = |portee, decision| ChoixUtilisateur {
        portee,
        genre: GenreTraitement::Loudness,
        decision,
    };
    d.enregistrer(ch(Portee::Label("ECM ".into()), Decision::Jamais));
    d.enregistrer(ch(Portee::Album(1), Decision::Toujours));
    d.enregistrer(ch(Portee::Album(1), Decision::Jamais));
    d.enregistrer(ch(Portee::Album(2), Decision::Toujours));
    let v = d.choix_pour(1, Some("ecm"));
    assert_eq!(
        v,
        vec![
            ch(Portee::Album(1), Decision::Jamais),
            ch(Portee::Label("ecm".into()), Decision::Jamais),
        ]
    );
    d.oublier(&Portee::Label("ECM".into()), GenreTraitement::Loudness);
    assert_eq!(d.choix_pour(1, Some("ecm")).len(), 1);
}

#[test]
fn profil_resolu_en_json() {
    let mut c = cd_preaccentue();
    c.mesures.as_mut().unwrap().crete_vraie_dbtp = 0.7;
    let r = resoudre(&profil_vague_1(), &c, &[]);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["traitements"][0]["genre"], json!("desaccentuation"));
    assert_eq!(v["traitements"][0]["etat"], json!("actif"));
    assert_eq!(
        v["traitements"][0]["reglages"]["genre"],
        json!("desaccentuation")
    );
    let relu: ProfilResolu = serde_json::from_value(v).unwrap();
    assert_eq!(relu, r);
    // Un profil s'écrit et se relit (profils éditoriaux des vagues 2-3).
    let p = profil_vague_1();
    let relu: Profil = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(relu, p);
}
