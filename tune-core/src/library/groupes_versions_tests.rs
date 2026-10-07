//! Épreuves du regroupement des versions (#2264).
//!
//! Chaque rapprochement voulu a sa contre-épreuve : le faux positif que la
//! règle doit refuser (live et studio, remaster, durée qui dérive).

use super::*;

fn local(id: i64, titre: &str, album: &str, duree_ms: i64) -> Exemplaire {
    Exemplaire {
        source: "local".into(),
        track_id: Some(id),
        titre: titre.into(),
        artiste: "Michael Jackson".into(),
        album: album.into(),
        duree_ms: Some(duree_ms),
        qualite: Some(Qualite {
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
        }),
        ..Default::default()
    }
}

fn service(nom: &str, id: &str, titre: &str, album: &str, duree_ms: i64) -> Exemplaire {
    Exemplaire {
        source: nom.into(),
        source_id: Some(id.into()),
        titre: titre.into(),
        artiste: "Michael Jackson".into(),
        album: album.into(),
        duree_ms: Some(duree_ms),
        ..Default::default()
    }
}

fn hires(mut e: Exemplaire, sr: i64, bd: i64) -> Exemplaire {
    e.qualite = Some(Qualite {
        format: Some("FLAC".into()),
        sample_rate: Some(sr),
        bit_depth: Some(bd),
    });
    e
}

fn avec_isrc(mut e: Exemplaire, isrc: &str) -> Exemplaire {
    e.isrc = Some(isrc.into());
    e
}

fn avec_mbid(mut e: Exemplaire, mbid: &str) -> Exemplaire {
    e.mbid_enregistrement = Some(mbid.into());
    e
}

/// Les indices de chaque groupe, dans l'ordre.
fn indices(groupes: &[Groupe]) -> Vec<Vec<usize>> {
    groupes
        .iter()
        .map(|g| g.membres.iter().map(|m| m.indice).collect())
        .collect()
}

// ── 1. L'ISRC d'abord ────────────────────────────────────────────────────

#[test]
fn le_meme_isrc_ecrit_autrement_reunit_deux_titres_differents() {
    let a = avec_isrc(local(1, "Billie Jean", "Thriller", 294_000), "USSM18200001");
    // Titre, album ET durée différents : seul l'ISRC parle, et il suffit.
    let b = avec_isrc(
        service(
            "qobuz",
            "q1",
            "Billie Jean (Single Version)",
            "Number Ones",
            270_000,
        ),
        "us-sm1-82-00001",
    );
    assert_eq!(relation(&a, &b), Relation::Lien(Lien::Isrc));
    assert_eq!(indices(&grouper(&[a, b])), vec![vec![0, 1]]);
}

/// Contre-épreuve : deux ISRC connus et différents sont un VETO, même quand
/// titre, artiste et durée concordent à la milliseconde — c'est le remaster
/// réédité sous un nouvel ISRC.
#[test]
fn deux_isrc_differents_ne_se_rejoignent_jamais() {
    let a = avec_isrc(local(1, "Billie Jean", "Thriller", 294_000), "USSM18200001");
    let b = avec_isrc(
        service("qobuz", "q1", "Billie Jean", "Thriller", 294_000),
        "USSM10800001",
    );
    assert_eq!(relation(&a, &b), Relation::Distinct);
    assert_eq!(indices(&grouper(&[a, b])), vec![vec![0], vec![1]]);
}

// ── 2. Puis le MBID d'enregistrement ─────────────────────────────────────

#[test]
fn le_meme_mbid_reunit_meme_sous_deux_isrc() {
    let mbid = "f1e2d3c4-0000-4000-8000-000000000001";
    let a = avec_mbid(
        avec_isrc(local(1, "Billie Jean", "Thriller", 294_000), "USSM18200001"),
        mbid,
    );
    let b = avec_mbid(
        avec_isrc(
            local(2, "Billie Jean", "Number Ones", 294_100),
            "USSM10300001",
        ),
        &mbid.to_uppercase(),
    );
    assert_eq!(relation(&a, &b), Relation::Lien(Lien::MbidEnregistrement));
}

#[test]
fn deux_mbid_differents_sont_un_veto() {
    let a = avec_mbid(local(1, "Billie Jean", "Thriller", 294_000), "aaaa");
    let b = avec_mbid(local(2, "Billie Jean", "Thriller", 294_000), "bbbb");
    assert_eq!(relation(&a, &b), Relation::Distinct);
}

// ── 3. Puis titre normalisé + artiste + durée à ±2 s ─────────────────────

/// Le cas visé d'abord : le FLAC local et l'édition Qobuz du même master,
/// sans aucun identifiant des deux côtés.
#[test]
fn local_et_qobuz_du_meme_master_se_rejoignent_sans_identifiant() {
    let a = local(
        1,
        "Shine On You Crazy Diamond (Parts 1-5)",
        "Wish You Were Here",
        811_000,
    );
    let b = service(
        "qobuz",
        "q1",
        "Shine on You Crazy Diamond, Pts. 1-5",
        "Wish You Were Here",
        812_900,
    );
    assert_eq!(relation(&a, &b), Relation::Lien(Lien::TitreArtisteDuree));
}

#[test]
fn la_tolerance_de_duree_est_de_deux_secondes_pile() {
    let a = local(1, "Billie Jean", "Thriller", 294_000);
    let dedans = service("tidal", "t1", "Billie Jean", "Thriller", 296_000);
    let dehors = service("tidal", "t2", "Billie Jean", "Thriller", 296_001);
    assert_eq!(
        relation(&a, &dedans),
        Relation::Lien(Lien::TitreArtisteDuree)
    );
    assert_eq!(relation(&a, &dehors), Relation::Inconnue);
}

#[test]
fn une_duree_inconnue_refuse_l_heuristique() {
    let a = local(1, "Billie Jean", "Thriller", 294_000);
    let mut b = service("bandcamp", "u", "Billie Jean", "Thriller", 0);
    b.duree_ms = None;
    assert_eq!(relation(&a, &b), Relation::Inconnue);
    let zero = service("deezer", "d", "Billie Jean", "Thriller", 0);
    assert_eq!(relation(&a, &zero), Relation::Inconnue);
}

#[test]
fn un_autre_artiste_n_est_pas_le_meme_enregistrement() {
    let a = local(1, "Billie Jean", "Thriller", 294_000);
    let mut reprise = service("qobuz", "q", "Billie Jean", "Euphoria Morning", 294_000);
    reprise.artiste = "Chris Cornell".into();
    assert_eq!(relation(&a, &reprise), Relation::Inconnue);
}

// ── Les faux positifs à refuser ──────────────────────────────────────────

/// Live et studio : même titre nu, durée identique par accident. L'album
/// « Live at Wembley » porte le marqueur, le studio non : refusé.
#[test]
fn le_live_au_titre_nu_ne_rejoint_pas_le_studio() {
    let studio = local(1, "Billie Jean", "Thriller", 294_000);
    let live = service(
        "qobuz",
        "q",
        "Billie Jean",
        "Live at Wembley July 16, 1988",
        294_500,
    );
    assert_eq!(relation(&studio, &live), Relation::Inconnue);
    // Contre-épreuve : le même album sans le mot « Live » est rapproché —
    // c'est bien le marqueur qui refuse, pas autre chose.
    let edition = service(
        "qobuz",
        "q",
        "Billie Jean",
        "Wembley July 16, 1988",
        294_500,
    );
    assert_eq!(
        relation(&studio, &edition),
        Relation::Lien(Lien::TitreArtisteDuree)
    );
}

#[test]
fn le_live_marque_dans_le_titre_ne_rejoint_pas_le_studio() {
    let studio = local(1, "Billie Jean", "Thriller", 294_000);
    let live = service("tidal", "t", "Billie Jean (Live)", "Thriller", 294_000);
    assert_eq!(relation(&studio, &live), Relation::Inconnue);
}

/// « live » se compare par JETON : « Alive » n'est pas un marqueur.
#[test]
fn alive_n_est_pas_un_marqueur_de_live() {
    let a = local(1, "Billie Jean", "Alive Again", 294_000);
    let b = service("qobuz", "q", "Billie Jean", "Alive Again", 294_000);
    assert_eq!(relation(&a, &b), Relation::Lien(Lien::TitreArtisteDuree));
}

/// Remaster : suffixe dans le titre, puis marqueur dans l'album.
#[test]
fn le_remaster_ne_rejoint_pas_l_original() {
    let original = local(1, "Heroes", "Heroes", 371_000);
    let par_titre = service("qobuz", "q1", "Heroes - 2017 Remaster", "Heroes", 371_000);
    let par_album = service(
        "qobuz",
        "q2",
        "Heroes",
        "Heroes (2017 Remastered Version)",
        371_200,
    );
    assert_eq!(relation(&original, &par_titre), Relation::Inconnue);
    assert_eq!(relation(&original, &par_album), Relation::Inconnue);
    // Deux remasters entre eux, en revanche, concordent.
    let autre_remaster = service("tidal", "t", "Heroes", "Heroes (2017 Remaster)", 371_900);
    assert_eq!(
        relation(&par_album, &autre_remaster),
        Relation::Lien(Lien::TitreArtisteDuree)
    );
}

/// Un titre inclus n'est pas le même titre.
#[test]
fn somebody_n_est_pas_somebody_to_love() {
    let mut a = local(1, "Somebody", "Some Great Reward", 268_000);
    a.artiste = "Depeche Mode".into();
    let mut b = service(
        "qobuz",
        "q",
        "Somebody To Love",
        "Some Great Reward",
        268_000,
    );
    b.artiste = "Depeche Mode".into();
    assert_eq!(relation(&a, &b), Relation::Inconnue);
}

/// L'heuristique doit concorder avec CHAQUE membre : sans cette garde, trois
/// exemplaires à 1,9 s d'écart l'un de l'autre formaient un groupe de 3,8 s.
#[test]
fn le_groupe_ne_derive_pas_de_proche_en_proche() {
    let a = local(1, "Billie Jean", "Thriller", 294_000);
    let b = service("qobuz", "q", "Billie Jean", "Thriller", 295_900);
    let c = service("tidal", "t", "Billie Jean", "Thriller", 297_800);
    assert_eq!(relation(&a, &b), Relation::Lien(Lien::TitreArtisteDuree));
    assert_eq!(relation(&b, &c), Relation::Lien(Lien::TitreArtisteDuree));
    assert_eq!(relation(&a, &c), Relation::Inconnue);
    assert_eq!(indices(&grouper(&[a, b, c])), vec![vec![0, 1], vec![2]]);
}

/// Un veto avec UN membre suffit à refuser l'entrée, même si l'ISRC relie à
/// un autre membre.
#[test]
fn un_veto_avec_un_seul_membre_ferme_le_groupe() {
    let mbid = "m-1";
    let a = avec_mbid(local(1, "Billie Jean", "Thriller", 294_000), mbid);
    let b = avec_isrc(
        avec_mbid(
            service("qobuz", "q", "Billie Jean", "Thriller", 294_000),
            mbid,
        ),
        "X1",
    );
    // c partage l'ISRC de b mais porte un AUTRE MBID que a.
    let c = avec_isrc(
        avec_mbid(
            service("tidal", "t", "Billie Jean", "Thriller", 294_000),
            "m-2",
        ),
        "X1",
    );
    let g = grouper(&[a, b, c]);
    assert_eq!(indices(&g), vec![vec![0, 1], vec![2]]);
}

#[test]
fn le_groupe_de_la_reference_sort_en_premier_et_les_identifies_fondent_d_abord() {
    let reference = local(1, "Billie Jean", "Thriller", 294_000);
    let live = service("qobuz", "q-live", "Billie Jean (Live)", "Bad Tour", 320_000);
    let live_tidal = avec_isrc(
        service("tidal", "t-live", "Billie Jean (Live)", "Bad Tour", 320_500),
        "L1",
    );
    let live_deezer = avec_isrc(
        service(
            "deezer",
            "d-live",
            "Billie Jean (Live)",
            "Bad Tour",
            340_000,
        ),
        "L1",
    );
    let g = grouper(&[reference, live, live_tidal, live_deezer]);
    // Les deux exemplaires identifiés fondent le groupe live. L'exemplaire
    // Qobuz sans identifiant concorde avec le FONDATEUR (0,5 s) ; l'autre
    // membre, entré par l'ISRC, est dispensé de la comparaison de durée.
    assert_eq!(indices(&g), vec![vec![0], vec![2, 3, 1]]);
    // L'identité publiée est le lien le plus FAIBLE du groupe : un membre y
    // est entré par l'heuristique, le groupe ne se dit donc pas « par ISRC ».
    assert_eq!(g[1].identite(), Some(Lien::TitreArtisteDuree));
    assert_eq!(g[0].identite(), None);
}

// ── Le choix de la version jouée ─────────────────────────────────────────

fn trio() -> Vec<Exemplaire> {
    vec![
        local(1, "Billie Jean", "Thriller", 294_000),
        hires(
            service("qobuz", "q", "Billie Jean", "Thriller", 294_100),
            192_000,
            24,
        ),
        hires(
            service("tidal", "t", "Billie Jean", "Thriller", 294_200),
            96_000,
            24,
        ),
    ]
}

#[test]
fn preferer_le_local_choisit_le_fichier_meme_moins_bon() {
    let e = trio();
    assert_eq!(
        choisir(&e, &[0, 1, 2], &RegleDeChoix::PrefererLocal),
        Some(0)
    );
    // Sans local, la meilleure qualité départage.
    assert_eq!(choisir(&e, &[1, 2], &RegleDeChoix::PrefererLocal), Some(1));
}

#[test]
fn la_meilleure_qualite_choisit_le_192_24() {
    let e = trio();
    assert_eq!(
        choisir(&e, &[0, 1, 2], &RegleDeChoix::MeilleureQualite),
        Some(1)
    );
}

#[test]
fn preferer_un_service_le_fait_passer_devant_le_local() {
    let e = trio();
    let tidal = RegleDeChoix::PrefererService("tidal".into());
    assert_eq!(choisir(&e, &[0, 1, 2], &tidal), Some(2));
    // Le service préféré absent du groupe : la bibliothèque d'abord.
    let deezer = RegleDeChoix::PrefererService("deezer".into());
    assert_eq!(choisir(&e, &[0, 1, 2], &deezer), Some(0));
}

#[test]
fn une_version_indisponible_n_est_jamais_choisie() {
    let mut e = trio();
    e[1].disponible = Some(false);
    assert_eq!(
        choisir(&e, &[0, 1, 2], &RegleDeChoix::MeilleureQualite),
        Some(2)
    );
    assert_eq!(choisir(&e, &[1], &RegleDeChoix::MeilleureQualite), None);
}

/// Une qualité inconnue passe APRÈS une qualité connue, même modeste —
/// sinon un service muet sur sa qualité battrait un MP3 par défaut.
#[test]
fn une_qualite_inconnue_passe_apres_une_qualite_connue() {
    let mut mp3 = service("deezer", "d", "Billie Jean", "Thriller", 294_000);
    mp3.qualite = Some(Qualite {
        format: Some("mp3".into()),
        sample_rate: Some(44_100),
        bit_depth: None,
    });
    let muet = service("spotify", "s", "Billie Jean", "Thriller", 294_000);
    let e = vec![muet, mp3];
    assert_eq!(
        choisir(&e, &[0, 1], &RegleDeChoix::MeilleureQualite),
        Some(1)
    );
}

#[test]
fn le_choix_est_deterministe_a_qualite_egale() {
    let e = vec![
        hires(
            service("tidal", "t", "Billie Jean", "Thriller", 294_000),
            96_000,
            24,
        ),
        hires(
            service("qobuz", "q", "Billie Jean", "Thriller", 294_000),
            96_000,
            24,
        ),
    ];
    // Départage par nom de source : qobuz avant tidal, dans les deux ordres.
    assert_eq!(
        choisir(&e, &[0, 1], &RegleDeChoix::MeilleureQualite),
        Some(1)
    );
    assert_eq!(
        choisir(&e, &[1, 0], &RegleDeChoix::MeilleureQualite),
        Some(1)
    );
}

#[test]
fn la_regle_se_lit_et_s_ecrit_sans_perte() {
    for texte in ["local", "quality", "service:qobuz"] {
        let r = RegleDeChoix::depuis(texte).expect(texte);
        assert_eq!(r.texte(), texte);
    }
    assert_eq!(
        RegleDeChoix::depuis(" service:Qobuz "),
        Some(RegleDeChoix::PrefererService("qobuz".into()))
    );
    for invalide in [
        "",
        "best",
        "service:",
        "service:a b",
        "service:x;DROP",
        "LOCAL",
    ] {
        assert_eq!(RegleDeChoix::depuis(invalide), None, "{invalide:?}");
    }
    assert_eq!(RegleDeChoix::DEFAUT, RegleDeChoix::PrefererLocal);
}

#[test]
fn le_noyau_deplie_pts_et_oublie_la_ponctuation() {
    assert_eq!(
        noyau_de_titre("Shine on You Crazy Diamond, Pts. 1-5"),
        noyau_de_titre("Shine On You Crazy Diamond (Parts 1–5)")
    );
    assert_ne!(noyau_de_titre("Parts 1-5"), noyau_de_titre("Parts 6-9"));
}

/// Le fondateur seul ne suffit pas : un membre entré par l'heuristique
/// compte aussi. Contre-épreuve de la dispense accordée aux identifiants.
#[test]
fn l_heuristique_doit_concorder_avec_les_autres_membres_heuristiques() {
    let a = local(1, "Billie Jean", "Thriller", 294_000);
    let b = service("qobuz", "q", "Billie Jean", "Thriller", 292_100);
    let c = service("tidal", "t", "Billie Jean", "Thriller", 295_900);
    // c concorde avec le fondateur (1,9 s) mais pas avec b (3,8 s).
    assert_eq!(indices(&grouper(&[a, b, c])), vec![vec![0, 1], vec![2]]);
}

/// L'original et son remaster, mêmes titre et durée, deux ISRC : un
/// résultat de service sans ISRC pourrait être l'un ou l'autre. Il reste
/// seul plutôt que d'être rangé au hasard du premier groupe.
#[test]
fn un_exemplaire_ambigu_reste_seul() {
    let original = avec_isrc(local(1, "Billie Jean", "Thriller", 294_000), "USSM18200001");
    let remaster = avec_isrc(local(2, "Billie Jean", "Singles", 294_000), "USSM10800999");
    let muet = service("qobuz", "q", "Billie Jean", "Thriller", 294_500);
    assert_eq!(
        indices(&grouper(&[original.clone(), remaster, muet.clone()])),
        vec![vec![0], vec![1], vec![2]]
    );
    // Contre-épreuve : sans le remaster, il rejoint l'original.
    assert_eq!(indices(&grouper(&[original, muet])), vec![vec![0, 1]]);
}

// ─── Repli (décision 2 du 07/10/2026) ───────────────────────────────────

#[test]
fn le_prefere_indisponible_est_un_repli_signale() {
    let mut qobuz = hires(
        service("qobuz", "q1", "Billie Jean", "Thriller", 294_000),
        192_000,
        24,
    );
    let biblio = local(1, "Billie Jean", "Thriller", 294_000);
    let regle = RegleDeChoix::PrefererService("qobuz".into());
    // Disponible : Qobuz est joué, pas de repli.
    let tous = vec![biblio.clone(), qobuz.clone()];
    let c = choisir_avec_repli(&tous, &[0, 1], &regle).unwrap();
    assert_eq!(c.indice, 1);
    assert!(!c.repli(), "la version préférée joue : pas de repli");
    // Indisponible : la bibliothèque joue, ET le repli le dit.
    qobuz.disponible = Some(false);
    let tous = vec![biblio, qobuz];
    let c = choisir_avec_repli(&tous, &[0, 1], &regle).unwrap();
    assert_eq!(c.indice, 0, "on passe à la suivante disponible");
    assert_eq!(c.prefere_indisponible, Some(1), "et on nomme la préférée");
}

#[test]
fn rien_d_indisponible_aucun_repli_meme_si_la_regle_change_de_source() {
    // Une version locale choisie par `local` alors qu'un service existe
    // n'est PAS un repli : c'est la règle elle-même.
    let tous = vec![
        service("tidal", "t1", "Billie Jean", "Thriller", 294_000),
        local(1, "Billie Jean", "Thriller", 294_000),
    ];
    let c = choisir_avec_repli(&tous, &[0, 1], &RegleDeChoix::PrefererLocal).unwrap();
    assert_eq!(c.indice, 1);
    assert!(!c.repli());
}
