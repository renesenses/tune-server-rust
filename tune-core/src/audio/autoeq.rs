//! Lecture d'un profil AutoEq au format « ParametricEQ ».
//!
//! [AutoEq](https://github.com/jaakkopasanen/AutoEq) publie, pour quelques
//! milliers de casques et d'écouteurs, une correction mesurée. Un de ses
//! formats d'export est un texte de quelques lignes :
//!
//! ```text
//! Preamp: -6.1 dB
//! Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70
//! Filter 2: ON PK Fc 8800 Hz Gain 5.1 dB Q 1.42
//! ```
//!
//! ## Il n'y a aucun DSP ici
//!
//! Chaque ligne `Filter` décrit exactement ce que [`EqBandSpec`] décrit déjà :
//! un biquad RBJ avec sa fréquence, son gain et son Q. Ce module est donc un
//! **analyseur de texte**, rien de plus — les coefficients, la cascade et le
//! pré-gain restent ceux de [`crate::audio::eq`], inchangés.
//!
//! | AutoEq | Tune ([`EqBandSpec`]) |
//! |---|---|
//! | `Fc … Hz` | `freq` |
//! | `Gain … dB` | `gain` |
//! | `Q …` | `q` |
//! | `PK` | `peak` |
//! | `LS`, `LSC`, `LSQ` | `low_shelf` |
//! | `HS`, `HSC`, `HSQ` | `high_shelf` |
//!
//! ## Le `Preamp` : pourquoi ce module ne l'applique PAS
//!
//! AutoEq préfixe ses profils d'un `Preamp` négatif parce que ses corrections
//! contiennent des gains positifs : sans atténuation préalable, un égaliseur
//! qui pousse de +6 dB écrête. C'est une vraie contrainte, et l'ignorer
//! saturerait.
//!
//! Tune la traite déjà, et plus sévèrement. Depuis d423c16b,
//! [`crate::audio::eq::EqProfile::automatic_headroom_db`] réserve la **somme de tous les gains
//! positifs** de la cascade, appliquée en pré-gain par canal avant les
//! biquads. Or la somme des gains positifs majore toujours le maximum de la
//! réponse combinée, que le `Preamp` d'AutoEq vient précisément compenser :
//! la marge que Tune réserve est donc toujours au moins aussi grande que celle
//! qu'AutoEq demande. Sur le HD 650 d'oratory1990, AutoEq demande −6,1 dB et
//! Tune en réserve −13,8.
//!
//! Ajouter le `Preamp` par-dessus atténuerait donc **deux fois**. Ce module se
//! contente de le lire et de le rendre dans [`ProfilAutoEq::preamp_db`], pour
//! que l'appelant puisse l'afficher et le comparer à la marge réellement
//! réservée ([`ProfilAutoEq::marge_de_tune_couvre_le_preamp`]). Aucun champ
//! n'est ajouté à [`crate::audio::eq::EqProfile`] et le chemin audio n'est pas
//! touché.
//!
//! ## Le Q des filtres en plateau
//!
//! `low_shelf` et `high_shelf` de [`crate::audio::eq`] sont conçus à pente
//! S = 1 (soit Q ≈ 0,707) : leur champ `q` n'entre pas dans les coefficients.
//! Le `Q` d'une ligne `LSC`/`HSC` est donc lu, conservé dans la bande, mais
//! sans effet sur le son. AutoEq exporte systématiquement `Q 0.70` sur ses
//! plateaux, ce qui est précisément cette pente : la correction est donc
//! reproduite fidèlement. Ce n'est pas une garantie pour un fichier écrit à la
//! main avec un autre Q de plateau, et ce module ne prétend pas le contraire.

use super::eq::EqBandSpec;

/// Un profil AutoEq analysé.
#[derive(Debug, Clone)]
pub struct ProfilAutoEq {
    /// Le `Preamp` déclaré par le fichier, en dB (négatif ou nul). `0.0` quand
    /// le fichier n'en porte pas. Rendu pour information : voir l'en-tête du
    /// module, Tune ne l'applique pas.
    pub preamp_db: f64,
    /// Les bandes, dans l'ordre du fichier. Les filtres `OFF` sont écartés.
    pub bandes: Vec<EqBandSpec>,
    /// Combien de lignes `Filter … OFF` ont été écartées.
    ///
    /// Écarter n'est pas taire. Equalizer APO exporte volontiers dix lignes
    /// dont trois désactivées ; l'utilisateur qui voit « 7 bandes importées »
    /// alors que son fichier en montre dix a le droit de savoir où sont
    /// passées les trois autres, sans avoir à relire le fichier. C'est le
    /// compte rendu que la route rend dans `ignored_filter_count`.
    pub filtres_ignores: usize,
}

impl ProfilAutoEq {
    /// La marge que Tune réservera pour ces bandes, en dB (négative ou nulle).
    ///
    /// C'est exactement ce que le chemin audio appliquera :
    /// [`super::eq::EqProfile::automatic_headroom_db`] sur le même jeu de
    /// bandes. Les bandes importées ne visant aucun canal en particulier, la
    /// valeur est la même à gauche et à droite.
    pub fn marge_reservee_db(&self) -> f64 {
        let profil = super::eq::EqProfile {
            bands: self.bandes.clone(),
            ..Default::default()
        };
        profil.automatic_headroom_db(0)
    }

    /// La marge de Tune est-elle au moins aussi protectrice que le `Preamp` ?
    ///
    /// Vraie quand `marge_reservee_db() <= preamp_db` : Tune atténue autant ou
    /// davantage, donc ce que le fichier voulait éviter est évité. Le doute
    /// n'est pas théorique, il se vérifie sur les vrais profils
    /// (`tune-core/tests/autoeq_profils_reels.rs`).
    pub fn marge_de_tune_couvre_le_preamp(&self) -> bool {
        // 0.05 dB : la tolérance d'arrondi d'un fichier écrit à la décimale.
        self.marge_reservee_db() <= self.preamp_db + 0.05
    }
}

/// Ce qui peut clocher dans un fichier AutoEq.
///
/// Chaque variante nomme la ligne fautive (numérotée à partir de 1, comme dans
/// un éditeur). Un profil mal formé doit être **refusé**, pas transformé en
/// bandes absurdes : une fréquence à 0 Hz ou un gain de +80 dB passerait sinon
/// silencieusement dans les bornes de `EqBandSpec::coeffs`, et l'utilisateur
/// entendrait n'importe quoi sans savoir pourquoi.
#[derive(Debug, Clone, PartialEq)]
pub enum ErreurAutoEq {
    /// Aucune ligne `Filter … ON` exploitable.
    AucuneBande,
    /// Une ligne qui n'est ni un `Preamp`, ni un `Filter`, ni un commentaire.
    LigneIncomprise { ligne: usize },
    /// `Filter n:` sans le `ON`/`OFF` qui doit suivre.
    EtatManquant { ligne: usize },
    /// Un type de filtre que Tune ne sait pas construire.
    TypeInconnu { ligne: usize, type_filtre: String },
    /// Un champ obligatoire absent de la ligne (`Fc`, `Gain`).
    ChampManquant { ligne: usize, champ: &'static str },
    /// Un champ présent mais dont la valeur n'est pas un nombre.
    NombreInvalide {
        ligne: usize,
        champ: &'static str,
        valeur: String,
    },
    /// Une valeur numérique hors du domaine que l'égaliseur sait reproduire.
    ValeurHorsDomaine {
        ligne: usize,
        champ: &'static str,
        valeur: f64,
        borne_basse: f64,
        borne_haute: f64,
    },
}

impl std::fmt::Display for ErreurAutoEq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AucuneBande => write!(
                f,
                "aucun filtre actif : ce texte n'est pas un profil AutoEq au format ParametricEQ"
            ),
            Self::LigneIncomprise { ligne } => write!(
                f,
                "ligne {ligne} : attendu « Preamp: … » ou « Filter n: … »"
            ),
            Self::EtatManquant { ligne } => {
                write!(
                    f,
                    "ligne {ligne} : il manque « ON » ou « OFF » après « Filter n: »"
                )
            }
            Self::TypeInconnu { ligne, type_filtre } => write!(
                f,
                "ligne {ligne} : type de filtre « {type_filtre} » inconnu (attendu PK, LS, LSC, LSQ, HS, HSC ou HSQ)"
            ),
            Self::ChampManquant { ligne, champ } => {
                write!(f, "ligne {ligne} : champ « {champ} » absent")
            }
            Self::NombreInvalide {
                ligne,
                champ,
                valeur,
            } => write!(
                f,
                "ligne {ligne} : « {champ} {valeur} » n'est pas un nombre"
            ),
            Self::ValeurHorsDomaine {
                ligne,
                champ,
                valeur,
                borne_basse,
                borne_haute,
            } => write!(
                f,
                "ligne {ligne} : « {champ} » vaut {valeur}, hors du domaine reproductible [{borne_basse} ; {borne_haute}]"
            ),
        }
    }
}

impl std::error::Error for ErreurAutoEq {}

/// Les bornes que `EqBandSpec::coeffs` applique déjà, en dur.
///
/// Elles sont recopiées ici pour REFUSER ce qui les dépasse plutôt que de le
/// rogner en silence : un fichier qui demande +40 dB ne serait pas reproduit,
/// et l'utilisateur mérite de l'apprendre à l'import, pas à l'oreille.
const GAIN_MAX_DB: f64 = 24.0;
const Q_MIN: f64 = 0.1;
const Q_MAX: f64 = 30.0;
/// Au-delà, aucune fréquence d'échantillonnage ne place le filtre sous Nyquist.
const FREQ_MAX_HZ: f64 = 192_000.0;

/// Analyse un profil AutoEq au format ParametricEQ.
///
/// Accepte les fins de ligne `\n` et `\r\n`, les lignes vides, et les
/// commentaires `#`. Tout le reste doit être une ligne `Preamp` ou `Filter`.
pub fn analyser(texte: &str) -> Result<ProfilAutoEq, ErreurAutoEq> {
    let mut preamp_db = 0.0;
    let mut bandes = Vec::new();
    let mut filtres_ignores = 0usize;

    for (index, brute) in texte.lines().enumerate() {
        let ligne = index + 1;
        let contenu = brute.trim();
        if contenu.is_empty() || contenu.starts_with('#') {
            continue;
        }

        let minuscules = contenu.to_ascii_lowercase();
        if minuscules.starts_with("preamp") {
            preamp_db = analyser_preamp(contenu, ligne)?;
        } else if minuscules.starts_with("filter") {
            match analyser_filtre(contenu, ligne)? {
                Some(bande) => bandes.push(bande),
                // Écarté, mais compté : voir `ProfilAutoEq::filtres_ignores`.
                None => filtres_ignores += 1,
            }
        } else {
            return Err(ErreurAutoEq::LigneIncomprise { ligne });
        }
    }

    if bandes.is_empty() {
        return Err(ErreurAutoEq::AucuneBande);
    }

    Ok(ProfilAutoEq {
        preamp_db,
        bandes,
        filtres_ignores,
    })
}

/// `Preamp: -6.1 dB` → `-6.1`.
fn analyser_preamp(contenu: &str, ligne: usize) -> Result<f64, ErreurAutoEq> {
    let apres =
        contenu
            .split_once(':')
            .map(|(_, reste)| reste)
            .ok_or(ErreurAutoEq::ChampManquant {
                ligne,
                champ: "Preamp",
            })?;
    let valeur = apres
        .split_whitespace()
        .next()
        .ok_or(ErreurAutoEq::ChampManquant {
            ligne,
            champ: "Preamp",
        })?;
    let db = nombre(valeur, "Preamp", ligne)?;
    // Un préamp positif n'a aucun sens dans ce format — il n'y aurait rien à
    // compenser — et signale un fichier bricolé.
    domaine(db, "Preamp", ligne, -GAIN_MAX_DB, 0.0)
}

/// `Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70` → une bande, ou `None` si
/// le filtre est `OFF`.
fn analyser_filtre(contenu: &str, ligne: usize) -> Result<Option<EqBandSpec>, ErreurAutoEq> {
    // Le numéro du filtre ne sert à rien : c'est l'ordre du fichier qui compte,
    // et il est déjà celui de la boucle appelante.
    let apres = contenu
        .split_once(':')
        .map(|(_, reste)| reste)
        .ok_or(ErreurAutoEq::EtatManquant { ligne })?;

    let mots: Vec<&str> = apres.split_whitespace().collect();
    let etat = mots.first().ok_or(ErreurAutoEq::EtatManquant { ligne })?;
    match etat.to_ascii_uppercase().as_str() {
        // Un filtre désactivé n'est pas une erreur : Equalizer APO complète ses
        // exports avec des lignes « OFF » que rien n'oblige à interpréter.
        "OFF" => return Ok(None),
        "ON" => {}
        _ => return Err(ErreurAutoEq::EtatManquant { ligne }),
    }

    let type_brut = mots.get(1).ok_or(ErreurAutoEq::ChampManquant {
        ligne,
        champ: "type de filtre",
    })?;
    let band_type = match type_brut.to_ascii_uppercase().as_str() {
        "PK" | "PEQ" => "peak",
        "LS" | "LSC" | "LSQ" => "low_shelf",
        "HS" | "HSC" | "HSQ" => "high_shelf",
        _ => {
            return Err(ErreurAutoEq::TypeInconnu {
                ligne,
                type_filtre: (*type_brut).to_string(),
            });
        }
    };

    let freq = champ_obligatoire(&mots, "Fc", ligne)?;
    let freq = domaine(freq, "Fc", ligne, 1.0, FREQ_MAX_HZ)?;

    let gain = champ_obligatoire(&mots, "Gain", ligne)?;
    let gain = domaine(gain, "Gain", ligne, -GAIN_MAX_DB, GAIN_MAX_DB)?;

    // Le Q est absent des lignes `LS`/`HS` d'Equalizer APO, qui sont à pente
    // fixe. Le défaut vaut alors la pente S = 1 des plateaux de `audio::eq`.
    let q = match champ_optionnel(&mots, "Q", ligne)? {
        Some(valeur) => domaine(valeur, "Q", ligne, Q_MIN, Q_MAX)?,
        None => std::f64::consts::FRAC_1_SQRT_2,
    };

    Ok(Some(EqBandSpec {
        freq,
        gain,
        q,
        band_type: band_type.to_string(),
        // Une correction de casque vaut pour les deux oreilles : la bande ne
        // vise aucun canal, donc les deux (cf. `EqBandSpec::channel`).
        channel: None,
    }))
}

/// Le nombre qui suit le mot-clé `cle`, ou `None` si le mot-clé est absent.
fn champ_optionnel(mots: &[&str], cle: &str, ligne: usize) -> Result<Option<f64>, ErreurAutoEq> {
    let Some(position) = mots.iter().position(|mot| mot.eq_ignore_ascii_case(cle)) else {
        return Ok(None);
    };
    let brut = mots.get(position + 1).ok_or(ErreurAutoEq::NombreInvalide {
        ligne,
        champ: cle_statique(cle),
        valeur: String::new(),
    })?;
    nombre(brut, cle_statique(cle), ligne).map(Some)
}

fn champ_obligatoire(mots: &[&str], cle: &str, ligne: usize) -> Result<f64, ErreurAutoEq> {
    champ_optionnel(mots, cle, ligne)?.ok_or(ErreurAutoEq::ChampManquant {
        ligne,
        champ: cle_statique(cle),
    })
}

/// Les noms de champ portés par les erreurs sont un ensemble fermé ; cette
/// fonction évite d'allouer une `String` par erreur pour trois valeurs connues.
fn cle_statique(cle: &str) -> &'static str {
    match cle {
        "Fc" => "Fc",
        "Gain" => "Gain",
        "Q" => "Q",
        _ => "champ",
    }
}

fn nombre(brut: &str, champ: &'static str, ligne: usize) -> Result<f64, ErreurAutoEq> {
    brut.parse::<f64>()
        .ok()
        .filter(|valeur| valeur.is_finite())
        .ok_or_else(|| ErreurAutoEq::NombreInvalide {
            ligne,
            champ,
            valeur: brut.to_string(),
        })
}

fn domaine(
    valeur: f64,
    champ: &'static str,
    ligne: usize,
    borne_basse: f64,
    borne_haute: f64,
) -> Result<f64, ErreurAutoEq> {
    if valeur < borne_basse || valeur > borne_haute {
        return Err(ErreurAutoEq::ValeurHorsDomaine {
            ligne,
            champ,
            valeur,
            borne_basse,
            borne_haute,
        });
    }
    Ok(valeur)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le HD 650 d'oratory1990, tel que publié par AutoEq. Le fichier complet
    /// est en fixture (`tests/fixtures/autoeq/`) ; ces deux lignes suffisent au
    /// contrat de traduction.
    const DEUX_LIGNES: &str = "Preamp: -6.1 dB\n\
                               Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n\
                               Filter 2: ON PK Fc 8800 Hz Gain 5.1 dB Q 1.42\n";

    #[test]
    fn une_ligne_autoeq_devient_la_bande_equivalente() {
        let profil = analyser(DEUX_LIGNES).expect("profil AutoEq valide");
        assert_eq!(profil.preamp_db, -6.1);
        assert_eq!(profil.bandes.len(), 2);

        let plateau = &profil.bandes[0];
        assert_eq!(plateau.band_type, "low_shelf");
        assert_eq!(plateau.freq, 105.0);
        assert_eq!(plateau.gain, 6.4);
        assert_eq!(plateau.q, 0.70);
        // Une correction de casque vaut pour les deux oreilles.
        assert_eq!(plateau.channel, None);

        let cloche = &profil.bandes[1];
        assert_eq!(cloche.band_type, "peak");
        assert_eq!(cloche.freq, 8800.0);
        assert_eq!(cloche.gain, 5.1);
        assert_eq!(cloche.q, 1.42);
    }

    #[test]
    fn les_trois_familles_de_types_sont_traduites() {
        let texte = "Filter 1: ON PK Fc 1000 Hz Gain 1 dB Q 1\n\
                     Filter 2: ON LSC Fc 100 Hz Gain 1 dB Q 0.7\n\
                     Filter 3: ON HSC Fc 10000 Hz Gain 1 dB Q 0.7\n\
                     Filter 4: ON LSQ Fc 100 Hz Gain 1 dB Q 0.7\n\
                     Filter 5: ON HSQ Fc 10000 Hz Gain 1 dB Q 0.7\n";
        let profil = analyser(texte).unwrap();
        let types: Vec<&str> = profil.bandes.iter().map(|b| b.band_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["peak", "low_shelf", "high_shelf", "low_shelf", "high_shelf"]
        );
    }

    #[test]
    fn un_filtre_desactive_est_ecarte_sans_erreur() {
        let texte = "Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n\
                     Filter 2: OFF PK Fc 0 Hz Gain 0 dB Q 1\n";
        let profil = analyser(texte).unwrap();
        assert_eq!(profil.bandes.len(), 1);
        assert_eq!(profil.bandes[0].freq, 1000.0);
    }

    /// Écarter n'est pas taire : les `OFF` sont comptés pour le compte rendu.
    ///
    /// Sans ce compte, un fichier de dix lignes dont trois désactivées rendrait
    /// « 7 bandes » sans que rien n'explique l'écart, et l'utilisateur croirait
    /// à une troncature.
    #[test]
    fn les_filtres_desactives_sont_comptes_dans_le_compte_rendu() {
        let texte = "Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n\
                     Filter 2: OFF PK Fc 200 Hz Gain 2 dB Q 1\n\
                     Filter 3: off PK Fc 300 Hz Gain 2 dB Q 1\n\
                     Filter 4: ON PK Fc 4000 Hz Gain -2 dB Q 1\n";
        let profil = analyser(texte).unwrap();
        assert_eq!(profil.bandes.len(), 2);
        assert_eq!(profil.filtres_ignores, 2);
    }

    #[test]
    fn un_profil_sans_filtre_desactive_nen_compte_aucun() {
        let profil = analyser(DEUX_LIGNES).unwrap();
        assert_eq!(profil.filtres_ignores, 0);
    }

    #[test]
    fn un_plateau_sans_q_prend_la_pente_des_plateaux_de_tune() {
        // Equalizer APO écrit « LS » sans Q : la pente est fixe.
        let profil = analyser("Filter 1: ON LS Fc 100 Hz Gain 3 dB\n").unwrap();
        assert!((profil.bandes[0].q - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
    }

    #[test]
    fn les_lignes_vides_et_les_commentaires_sont_ignores() {
        let texte = "# AutoEq — Sennheiser HD 650\n\
                     \n\
                     Preamp: -6.1 dB\r\n\
                     \n\
                     Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\r\n";
        let profil = analyser(texte).unwrap();
        assert_eq!(profil.preamp_db, -6.1);
        assert_eq!(profil.bandes.len(), 1);
    }

    #[test]
    fn un_fichier_sans_preamp_reste_lisible() {
        let profil = analyser("Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n").unwrap();
        assert_eq!(profil.preamp_db, 0.0);
    }

    // --- Ce qui doit être REFUSÉ, pas rogné ---

    #[test]
    fn un_texte_quelconque_est_refuse() {
        // Le cas de l'utilisateur qui colle autre chose que son profil.
        assert_eq!(
            analyser("bonjour").unwrap_err(),
            ErreurAutoEq::LigneIncomprise { ligne: 1 }
        );
        assert_eq!(analyser("").unwrap_err(), ErreurAutoEq::AucuneBande);
        assert_eq!(
            analyser("Preamp: -6.1 dB\n").unwrap_err(),
            ErreurAutoEq::AucuneBande
        );
    }

    #[test]
    fn un_type_de_filtre_inconnu_est_refuse() {
        // `BP` existe dans Equalizer APO ; `audio::eq` ne le construit pas.
        assert_eq!(
            analyser("Filter 1: ON BP Fc 1000 Hz Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::TypeInconnu {
                ligne: 1,
                type_filtre: "BP".into(),
            }
        );
    }

    #[test]
    fn une_frequence_a_zero_est_refusee_et_non_remontee_a_10_hz() {
        // Sans ce refus, `EqBandSpec::coeffs` remonterait 0 Hz à 10 Hz et
        // fabriquerait une bande que personne n'a demandée.
        assert_eq!(
            analyser("Filter 1: ON PK Fc 0 Hz Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::ValeurHorsDomaine {
                ligne: 1,
                champ: "Fc",
                valeur: 0.0,
                borne_basse: 1.0,
                borne_haute: FREQ_MAX_HZ,
            }
        );
    }

    #[test]
    fn un_gain_absurde_est_refuse_et_non_rogne_a_24_db() {
        assert_eq!(
            analyser("Filter 1: ON PK Fc 1000 Hz Gain 80 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::ValeurHorsDomaine {
                ligne: 1,
                champ: "Gain",
                valeur: 80.0,
                borne_basse: -GAIN_MAX_DB,
                borne_haute: GAIN_MAX_DB,
            }
        );
    }

    #[test]
    fn un_q_hors_domaine_est_refuse() {
        assert!(matches!(
            analyser("Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 0\n").unwrap_err(),
            ErreurAutoEq::ValeurHorsDomaine { champ: "Q", .. }
        ));
    }

    #[test]
    fn un_champ_obligatoire_absent_est_nomme() {
        assert_eq!(
            analyser("Filter 1: ON PK Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::ChampManquant {
                ligne: 1,
                champ: "Fc",
            }
        );
        assert_eq!(
            analyser("Filter 1: ON PK Fc 1000 Hz Q 1\n").unwrap_err(),
            ErreurAutoEq::ChampManquant {
                ligne: 1,
                champ: "Gain",
            }
        );
    }

    #[test]
    fn une_valeur_non_numerique_est_nommee_avec_sa_ligne() {
        assert_eq!(
            analyser("Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\nFilter 2: ON PK Fc abc Hz Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::NombreInvalide {
                ligne: 2,
                champ: "Fc",
                valeur: "abc".into(),
            }
        );
    }

    #[test]
    fn une_ligne_filter_sans_on_ni_off_est_refusee() {
        assert_eq!(
            analyser("Filter 1: PK Fc 1000 Hz Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::EtatManquant { ligne: 1 }
        );
    }

    #[test]
    fn un_preamp_positif_est_refuse() {
        // Le format ne l'admet pas : il n'y aurait rien à compenser.
        assert!(matches!(
            analyser("Preamp: 3 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\n").unwrap_err(),
            ErreurAutoEq::ValeurHorsDomaine {
                champ: "Preamp",
                ..
            }
        ));
    }

    // --- La marge de gain ---

    #[test]
    fn la_marge_reservee_est_la_somme_des_gains_positifs() {
        let profil = analyser(DEUX_LIGNES).unwrap();
        // 6.4 + 5.1
        assert!((profil.marge_reservee_db() - (-11.5)).abs() < 1e-9);
    }

    #[test]
    fn la_marge_de_tune_couvre_le_preamp_demande() {
        let profil = analyser(DEUX_LIGNES).unwrap();
        assert!(profil.marge_reservee_db() <= profil.preamp_db);
        assert!(profil.marge_de_tune_couvre_le_preamp());
    }

    #[test]
    fn un_profil_uniquement_attenuateur_ne_reserve_aucune_marge() {
        // Aucun gain positif : rien à réserver, et le Preamp d'AutoEq vaut 0.
        let profil = analyser("Filter 1: ON PK Fc 1000 Hz Gain -3 dB Q 1\n").unwrap();
        assert_eq!(profil.marge_reservee_db(), 0.0);
        assert!(profil.marge_de_tune_couvre_le_preamp());
    }
}
