//! rc14 — sécurité auditive : décocher un égaliseur d'une SÉRIE TESS (#9520)
//! ne fait pas remonter le niveau d'un coup.
//!
//! La marge (préampli) d'une série est calculée sur toute la cascade (norme
//! L1, `reserve_l1_toujours`). Retirer un maillon la fait baisser : avant ce
//! correctif, le nouvel égaliseur actif remplaçait l'ancien par le seul fondu
//! de 300 ms de #5215, sans aucune borne de volume (relecture du 25/11 : à
//! 40 Hz, −20,2 dB → −7,35 dB). La règle du 10/10 (#6741, fil 1974) vaut
//! désormais aussi entre deux égaliseurs ACTIFS : la hausse de préampli est
//! retenue, puis rendue en moins de 0,5 s jusqu'au niveau d'avant
//! l'égaliseur (`borne_de_remontee`), jamais au-delà ; le surplus reste gardé
//! (voyant, « Rendre le niveau », #6524).
use super::*;
use crate::tess::egaliseurs_en_serie::combiner_en_serie;

const TAUX: u32 = 48_000;
const AMPLITUDE: f32 = 0.05;
/// Blocs de 10 ms.
const BLOC: usize = 480;

fn bande(freq: f64, gain: f64, q: f64, t: &str) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        ..Default::default()
    }
}

/// Deux égaliseurs aux forts gains dans le grave (le cas de la relecture).
fn maillon_a() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![bande(40.0, 12.0, 1.0, "peak")],
        ..Default::default()
    }
}

fn maillon_b() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![bande(60.0, 12.0, 0.707, "low_shelf")],
        ..Default::default()
    }
}

/// Un maillon qui pousse l'aigu : le retirer ne change pas la courbe à 40 Hz,
/// seulement la marge (le relevé de la relecture).
fn maillon_aigu() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![
            bande(2500.0, 12.0, 0.7, "peak"),
            bande(8000.0, 15.0, 0.7, "peak"),
        ],
        ..Default::default()
    }
}

fn moteur(profils: &[EqProfile]) -> EqProcessor {
    EqProcessor::new(
        &combiner_en_serie(profils).expect("au moins un maillon"),
        TAUX,
        2,
    )
}

fn preampli_db(eq: &EqProcessor) -> f64 {
    eq.preamp_db(0).unwrap_or(0.0)
}

/// Une sinusoïde continue, bloc après bloc, et le gain de chaque bloc (dB,
/// sortie / entrée) ; `sortie` garde tous les échantillons du canal gauche.
struct Banc {
    freq: f64,
    n: usize,
    sortie: Vec<f32>,
    gains_db: Vec<f64>,
}

impl Banc {
    fn new(freq: f64) -> Self {
        Self {
            freq,
            n: 0,
            sortie: Vec::new(),
            gains_db: Vec::new(),
        }
    }

    fn jouer(&mut self, eq: &mut EqProcessor, blocs: usize) {
        for _ in 0..blocs {
            let mut pcm = Vec::with_capacity(BLOC * 2);
            for i in 0..BLOC {
                let t = (self.n + i) as f64 / f64::from(TAUX);
                let s = AMPLITUDE * (2.0 * std::f64::consts::PI * self.freq * t).sin() as f32;
                pcm.push(s);
                pcm.push(s);
            }
            self.n += BLOC;
            let entree: f64 = pcm.iter().map(|s| f64::from(*s).powi(2)).sum();
            eq.process_interleaved(&mut pcm);
            let sortie: f64 = pcm.iter().map(|s| f64::from(*s).powi(2)).sum();
            self.gains_db.push(10.0 * (sortie / entree).log10());
            self.sortie.extend(pcm.iter().step_by(2));
        }
    }

    fn dernier_gain_db(&self) -> f64 {
        *self.gains_db.last().unwrap()
    }

    /// Le gain le plus haut à partir du bloc `depuis`.
    fn max_db_depuis(&self, depuis: usize) -> f64 {
        self.gains_db[depuis..]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Le plus grand écart entre deux échantillons voisins à partir du bloc
    /// `depuis` (un clic le ferait bondir).
    fn ecart_max_depuis(&self, depuis: usize) -> f32 {
        self.sortie[depuis * BLOC..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }
}

/// La série installée et stabilisée (1 s), sa borne posée comme le fait la
/// sortie locale ; puis on DÉCOCHE le maillon B en vol.
fn decocher_b(freq: f64, borne: f64) -> (Banc, EqProcessor, usize, f64, f64) {
    decocher(freq, borne, maillon_b())
}

fn decocher(freq: f64, borne: f64, b: EqProfile) -> (Banc, EqProcessor, usize, f64, f64) {
    let mut serie = moteur(&[maillon_a(), b]);
    serie.borner_la_remontee(borne);
    let p_serie = preampli_db(&serie);
    let mut banc = Banc::new(freq);
    banc.jouer(&mut serie, 100);
    let seul = moteur(&[maillon_a()]);
    let p_seul = preampli_db(&seul);
    let mut releve =
        EqProcessor::prendre_la_releve(Some(seul), Some(serie)).expect("un égaliseur reste coché");
    // Comme `LocalOutput::replace_eq_live` : la borne est reposée après la relève.
    releve.borner_la_remontee(borne);
    let bascule = banc.gains_db.len();
    (banc, releve, bascule, p_serie, p_seul)
}

fn db_en_lineaire(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

/// Le banc reproduit bien la relecture : la marge de la série est nettement
/// plus basse que celle du maillon A seul.
#[test]
fn la_marge_de_la_serie_baisse_quand_on_retire_un_maillon() {
    let (_, _, _, p_serie, p_seul) = decocher_b(1000.0, 1.0);
    assert!(
        p_seul - p_serie > 8.0,
        "marge série {p_serie:.2} dB, maillon seul {p_seul:.2} dB : le banc ne reproduit pas le cas"
    );
}

/// TÉMOIN (échouait avant le correctif) — volume monté pour compenser la
/// marge de la série : décocher un maillon ne fait JAMAIS dépasser le niveau
/// d'avant l'égaliseur. Mesuré à 5 kHz, où aucune bande n'agit : le gain y
/// est le préampli seul. Avant : +12 dB d'un coup en 300 ms, au-dessus de la
/// borne de volume.
#[test]
fn decocher_un_maillon_volume_monte_ne_depasse_pas_le_niveau_d_avant_l_egaliseur() {
    let (_, _, _, p_serie, p_seul) = decocher_b(5000.0, 1.0);
    // L'auditeur a monté le volume de la moitié de l'écart de marge.
    let borne_db = (p_serie + p_seul) / 2.0;
    let (mut banc, mut eq, bascule, ..) = decocher_b(5000.0, db_en_lineaire(borne_db));
    let avant = banc.dernier_gain_db();
    banc.jouer(&mut eq, 300);
    let pire = banc.max_db_depuis(bascule);
    assert!(
        pire <= borne_db + 0.1,
        "gain avant {avant:.2} dB, pire après {pire:.2} dB : au-dessus du niveau d'avant \
         l'égaliseur ({borne_db:.2} dB, volume monté)"
    );
    // Il remonte jusqu'à la borne, pas moins (aucune baisse durable).
    let fin = banc.dernier_gain_db();
    assert!(
        (fin - borne_db).abs() < 0.1,
        "gain final {fin:.2} dB, attendu la borne {borne_db:.2} dB"
    );
    // Le surplus gardé se voit (voyant de #6741).
    let garde = eq
        .preampli_garde()
        .expect("le surplus de marge reste gardé et se voit");
    assert!(garde.db < -1.0, "préampli gardé {:.2} dB", garde.db);
}

/// TÉMOIN — le relevé de la relecture, à 40 Hz : A pousse le grave, l'autre
/// maillon l'aigu ; décocher ce dernier faisait monter le 40 Hz de toute la
/// marge rendue (+10 dB en 300 ms). Volume inconnu : rien ne monte ; volume
/// monté : il ne monte que jusqu'au niveau d'avant l'égaliseur.
#[test]
fn decocher_un_maillon_aucun_saut_a_40_hz_au_dela_de_la_borne() {
    let (_, _, _, p_serie, p_seul) = decocher(40.0, 1.0, maillon_aigu());
    assert!(
        p_seul - p_serie > 6.0,
        "série {p_serie:.2}, seul {p_seul:.2}"
    );
    let borne_db = (p_serie + p_seul) / 2.0;
    for (borne, permis) in [(0.0, 0.0), (db_en_lineaire(borne_db), borne_db - p_serie)] {
        let (mut banc, mut eq, bascule, ..) = decocher(40.0, borne, maillon_aigu());
        let avant = banc.dernier_gain_db();
        banc.jouer(&mut eq, 300);
        let pire = banc.max_db_depuis(bascule);
        assert!(
            pire - avant <= permis + 0.2,
            "40 Hz, borne {borne:.3} : {avant:.2} dB avant, {pire:.2} dB au pire après \
             (hausse de {:.1} dB, permis {permis:.1} dB)",
            pire - avant
        );
    }
}

/// Volume inconnu (aucune borne posée) : rien ne remonte (#6708, #6741).
#[test]
fn decocher_un_maillon_sans_borne_connue_ne_remonte_pas() {
    let (mut banc, mut eq, bascule, p_serie, _) = decocher_b(5000.0, 0.0);
    banc.jouer(&mut eq, 300);
    let pire = banc.max_db_depuis(bascule);
    assert!(
        pire <= p_serie + 0.1,
        "sans borne, le gain monte à {pire:.2} dB (série {p_serie:.2} dB)"
    );
}

/// Volume INCHANGÉ : retour exact au niveau du maillon restant, en moins de
/// 0,5 s, puis plus rien de gardé.
#[test]
fn decocher_un_maillon_volume_inchange_retour_exact_en_moins_d_une_demi_seconde() {
    let (mut banc, mut eq, bascule, _, p_seul) = decocher_b(5000.0, 1.0);
    banc.jouer(&mut eq, 50);
    // Bloc qui finit à 0,5 s.
    let a_500_ms = banc.gains_db[bascule + 49];
    assert!(
        (a_500_ms - p_seul).abs() < 0.05,
        "à 0,5 s : {a_500_ms:.3} dB, attendu le maillon seul {p_seul:.3} dB"
    );
    assert!(
        eq.preampli_garde().is_none(),
        "volume inchangé : plus rien n'est gardé"
    );
    // Et la sortie est alors celle d'un égaliseur A neuf, à l'échantillon près
    // une fois l'historique des filtres oublié.
    let pire = banc.max_db_depuis(bascule);
    assert!(
        pire <= p_seul + 0.05,
        "dépassement {pire:.2} dB > {p_seul:.2} dB"
    );
}

/// Volume monté puis « Rendre le niveau » (#6524) : le surplus gardé remonte
/// à son tour, sans saut, jusqu'au niveau du maillon restant.
#[test]
fn volume_monte_rendre_le_niveau_libere_le_surplus() {
    let (_, _, _, p_serie, p_seul) = decocher_b(5000.0, 1.0);
    let borne_db = (p_serie + p_seul) / 2.0;
    let (mut banc, mut eq, _, ..) = decocher_b(5000.0, db_en_lineaire(borne_db));
    banc.jouer(&mut eq, 100);
    eq.liberer_le_gain_garde();
    let geste = banc.gains_db.len();
    banc.jouer(&mut eq, 100);
    let fin = banc.dernier_gain_db();
    assert!(
        (fin - p_seul).abs() < 0.05,
        "après « Rendre le niveau » : {fin:.2} dB, attendu {p_seul:.2} dB"
    );
    assert!(eq.preampli_garde().is_none());
    let pas = banc.gains_db[geste..]
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(0.0_f64, f64::max);
    assert!(pas < 1.0, "marche de {pas:.2} dB en 10 ms");
}

/// Pas de clic : l'écart maximal entre deux échantillons voisins pendant la
/// bascule ne dépasse pas celui de la sinusoïde au niveau le plus haut.
#[test]
fn decocher_un_maillon_sans_clic() {
    for borne in [1.0, 0.0] {
        let (mut banc, mut eq, bascule, ..) = decocher_b(1000.0, borne);
        banc.jouer(&mut eq, 300);
        let pire_db = banc.max_db_depuis(bascule).max(banc.gains_db[bascule - 1]);
        let pente_max =
            f64::from(AMPLITUDE) * db_en_lineaire(pire_db) * 2.0 * std::f64::consts::PI * 1000.0
                / f64::from(TAUX);
        let ecart = banc.ecart_max_depuis(bascule - 1);
        assert!(
            f64::from(ecart) <= pente_max * 1.05,
            "borne {borne} : écart {ecart:.6} > pente de la sinusoïde {pente_max:.6}"
        );
    }
}

/// Les autres façons de faire baisser la marge en série, à volume inconnu
/// (rien ne doit remonter) : baisser les bandes d'un maillon, changer de
/// préréglage (une autre série plus douce), recocher un seul maillon après
/// les avoir tous décochés.
#[test]
fn les_autres_baisses_de_marge_ne_font_pas_sauter_le_niveau() {
    let doux = EqProfile {
        enabled: true,
        bands: vec![bande(60.0, 2.0, 0.707, "low_shelf")],
        ..Default::default()
    };
    let autre_prereglage = EqProfile {
        enabled: true,
        bands: vec![bande(200.0, 3.0, 1.0, "peak")],
        ..Default::default()
    };
    let cas: Vec<(&str, Vec<EqProfile>)> = vec![
        ("bandes du maillon B baissées", vec![maillon_a(), doux]),
        ("autre préréglage", vec![autre_prereglage]),
    ];
    for (nom, apres) in cas {
        let mut serie = moteur(&[maillon_a(), maillon_b()]);
        serie.borner_la_remontee(0.0);
        let p_serie = preampli_db(&serie);
        let mut banc = Banc::new(5000.0);
        banc.jouer(&mut serie, 100);
        let mut eq = EqProcessor::prendre_la_releve(Some(moteur(&apres)), Some(serie)).unwrap();
        eq.borner_la_remontee(0.0);
        let bascule = banc.gains_db.len();
        banc.jouer(&mut eq, 300);
        let pire = banc.max_db_depuis(bascule);
        assert!(
            pire <= p_serie + 1.0,
            "{nom} : le gain monte à {pire:.2} dB (série {p_serie:.2} dB)"
        );
    }

    // Tout décocher (le neutre garde la marge de la série), puis recocher
    // le seul maillon A : sa marge, plus haute, ne doit pas sauter non plus.
    let mut serie = moteur(&[maillon_a(), maillon_b()]);
    serie.borner_la_remontee(0.0);
    let p_serie = preampli_db(&serie);
    let mut banc = Banc::new(5000.0);
    banc.jouer(&mut serie, 100);
    let mut coupe = EqProcessor::prendre_la_releve(None, Some(serie)).unwrap();
    coupe.borner_la_remontee(0.0);
    banc.jouer(&mut coupe, 50);
    let mut eq = EqProcessor::prendre_la_releve(Some(moteur(&[maillon_a()])), Some(coupe)).unwrap();
    eq.borner_la_remontee(0.0);
    let bascule = banc.gains_db.len();
    banc.jouer(&mut eq, 300);
    let pire = banc.max_db_depuis(bascule);
    assert!(
        pire <= p_serie + 1.0,
        "recocher A seul : le gain monte à {pire:.2} dB (série gardée {p_serie:.2} dB)"
    );
}

/// Relecture du 26/11 : volume INCHANGÉ, la retenue revient entière en
/// 450 ms — le voyant « niveau réduit » ne s'allume pas pendant cette rampe
/// ordinaire. Volume monté, le surplus qui demeure le rallume, dès la rampe.
#[test]
fn decocher_un_maillon_volume_inchange_pas_de_voyant_pendant_la_rampe() {
    let (mut banc, mut eq, ..) = decocher_b(5000.0, 1.0);
    assert!(eq.en_extinction(), "la retenue doit être en train de revenir");
    assert!(
        eq.preampli_garde().is_none(),
        "rampe ordinaire : pas de « niveau réduit »"
    );
    banc.jouer(&mut eq, 10);
    assert!(eq.en_extinction() && eq.preampli_garde().is_none());

    let (_, _, _, p_serie, p_seul) = decocher_b(5000.0, 1.0);
    let borne_db = (p_serie + p_seul) / 2.0;
    let (mut banc, mut eq, ..) = decocher_b(5000.0, db_en_lineaire(borne_db));
    banc.jouer(&mut eq, 10);
    assert!(
        eq.preampli_garde().is_some(),
        "volume monté : le surplus gardé se voit dès la rampe"
    );
}
