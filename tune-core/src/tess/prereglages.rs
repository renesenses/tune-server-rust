//! Préréglages de chaîne et mode d'application (maquette A2, lot T17).
//!
//! « A preset keeps every plugin, their order and all their settings. » Un
//! préréglage est donc une [`ChaineTess`] complète, nommée. Les préréglages
//! sont GLOBAUX (une liste pour le serveur) ; chaque zone garde sa propre
//! chaîne. Les préréglages propres à un greffon (`eq_presets`,
//! `crossfeed_presets`…) restent dans ses réglages.
//!
//! « Pure — no processing » est un préréglage fourni par le serveur
//! ([`PreReglage::pure`]) : chaîne vide et `bypass_all`, soit le bit-perfect
//! quand le volume est à 0 dB. Charger un préréglage REMPLACE la chaîne de la
//! zone : l'écran (W4) propose d'enregistrer la chaîne courante avant.
//!
//! Le préréglage actif est propre à chaque ZONE (maquette A1 : sélecteurs
//! Zone et Preset) : il est publié dans la réponse de la zone
//! (`active_preset`), pas dans cette liste globale.

use serde::{Deserialize, Serialize};

use super::chaine::ChaineTess;
use super::greffon::id_valide;

/// Id réservé du préréglage « Pure — no processing ».
pub const ID_PREREGLAGE_PURE: &str = "pure";

/// Quand un changement de chaîne prend effet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModeApplication {
    /// La lecture se met en pause puis reprend avec les nouveaux réglages
    /// (relance à la position, #8422).
    #[default]
    Immediately,
    /// Le changement attend la prochaine lecture.
    AtNextPlayback,
    /// Valeur de repli à la lecture : traitée comme `at_next_playback`
    /// (jamais d'interruption non voulue).
    #[serde(other)]
    Unknown,
}

/// Un préréglage de chaîne.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreReglage {
    /// Identifiant stable, minuscules ASCII, chiffres et tirets.
    pub id: String,
    /// Nom choisi par l'utilisateur (« Name the current chain »).
    pub name: String,
    /// Fourni par le serveur (« Pure ») plutôt que créé par l'utilisateur.
    #[serde(default)]
    pub builtin: bool,
    pub chain: ChaineTess,
}

impl PreReglage {
    /// « Pure — no processing ».
    pub fn pure() -> Self {
        Self {
            id: ID_PREREGLAGE_PURE.into(),
            name: "Pure — no processing".into(),
            builtin: true,
            chain: ChaineTess {
                bypass_all: true,
                ..ChaineTess::default()
            },
        }
    }
}

/// La liste des préréglages et le mode d'application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreReglages {
    #[serde(default)]
    pub apply_mode: ModeApplication,
    pub presets: Vec<PreReglage>,
}

impl Default for PreReglages {
    fn default() -> Self {
        Self {
            apply_mode: ModeApplication::default(),
            presets: vec![PreReglage::pure()],
        }
    }
}

/// Pourquoi une liste de préréglages est refusée (409).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurPreReglage {
    #[error("identifiant de préréglage invalide : {0:?}")]
    IdInvalide(String),
    #[error("préréglage {0} présent deux fois")]
    IdEnDouble(String),
    #[error("le préréglage {0} n'a pas de nom")]
    NomVide(String),
    #[error("le nom du préréglage {0} dépasse 80 caractères ou contient un caractère de contrôle")]
    NomInvalide(String),
    #[error("le préréglage Pure doit rester sans traitement")]
    PureModifie,
    #[error("plus de {max} préréglages ({0})", max = PREREGLAGES_MAX)]
    TropNombreux(usize),
}

impl PreReglages {
    pub fn get(&self, id: &str) -> Option<&PreReglage> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// Cohérence de la liste. La validité de chaque chaîne contre le
    /// catalogue et les droits se vérifie au CHARGEMENT
    /// ([`ChaineTess::valider`]) : un préréglage Premium reste enregistré sur
    /// un compte redevenu gratuit.
    pub fn verifier(&self) -> Result<(), ErreurPreReglage> {
        if self.presets.len() > PREREGLAGES_MAX {
            return Err(ErreurPreReglage::TropNombreux(self.presets.len()));
        }
        for (i, p) in self.presets.iter().enumerate() {
            if !id_valide(&p.id) {
                return Err(ErreurPreReglage::IdInvalide(p.id.clone()));
            }
            if self.presets[..i].iter().any(|q| q.id == p.id) {
                return Err(ErreurPreReglage::IdEnDouble(p.id.clone()));
            }
            if p.name.trim().is_empty() {
                return Err(ErreurPreReglage::NomVide(p.id.clone()));
            }
            // Même règle que la route (un fichier importé ne la contourne pas).
            if nom_valide(&p.name).is_none() {
                return Err(ErreurPreReglage::NomInvalide(p.id.clone()));
            }
            if p.id == ID_PREREGLAGE_PURE && p.chain != PreReglage::pure().chain {
                return Err(ErreurPreReglage::PureModifie);
            }
        }
        Ok(())
    }
}

impl ModeApplication {
    /// Le changement interrompt-il la lecture en cours (pause, puis reprise
    /// à la position, #8422) ? Une valeur inconnue ne l'interrompt jamais.
    pub fn immediat(self) -> bool {
        self == Self::Immediately
    }
}

// ── Stockage (lot T17) ──────────────────────────────────────────────

/// La clé de réglage GLOBALE qui porte la liste des préréglages et le mode
/// d'application. Absente : [`PreReglages::default`] (Pure seul, Immédiat).
/// Elle part dans l'export de configuration comme les autres réglages
/// globaux ; l'import la relit par [`lire_texte`] et l'écarte si elle ne
/// passe pas [`PreReglages::verifier`].
pub const CLE_PREREGLAGES: &str = "tess_presets";

/// Longueur maximale d'un nom de préréglage, en caractères.
pub const NOM_MAX: usize = 80;

/// Nombre maximal de préréglages (Pure compris), à l'enregistrement comme à
/// l'import.
pub const PREREGLAGES_MAX: usize = 200;

/// Lit une liste de préréglages écrite (base ou fichier d'export) :
/// tolérante aux champs et valeurs inconnus (comme toute réponse du
/// contrat), mais cohérente ([`PreReglages::verifier`]).
pub fn lire_texte(texte: &str) -> Result<PreReglages, String> {
    let l: PreReglages = serde_json::from_str(texte).map_err(|e| e.to_string())?;
    l.verifier().map_err(|e| e.to_string())?;
    Ok(l)
}

/// La liste enregistrée sur ce serveur. Absente : la liste par défaut.
/// Illisible (JSON) : une erreur (jamais une liste vide qui écraserait
/// l'ancienne). Lisible mais incohérente (écrite par une version aux règles
/// plus lâches) : les entrées fautives sont ÉCARTÉES avec un avertissement,
/// pour qu'une seule entrée ne fasse pas tomber toutes les routes.
pub fn lire(settings: &crate::db::settings_repo::SettingsRepo) -> Result<PreReglages, String> {
    let texte = match settings.get(CLE_PREREGLAGES)? {
        None => return Ok(PreReglages::default()),
        Some(t) if t.trim().is_empty() => return Ok(PreReglages::default()),
        Some(t) => t,
    };
    let mut l: PreReglages = serde_json::from_str(&texte).map_err(|e| e.to_string())?;
    for avis in l.assainir() {
        tracing::warn!(avis = %avis, "tess_prereglage_ecarte");
    }
    Ok(l)
}

impl PreReglages {
    /// Écarte les entrées qui ne passent pas [`Self::verifier`] (id invalide
    /// ou en double, nom invalide, Pure modifié, au-delà du plafond) et rend
    /// un avertissement par entrée écartée. Ensuite `verifier` passe.
    pub fn assainir(&mut self) -> Vec<String> {
        let mut avis = Vec::new();
        let mut gardes: Vec<PreReglage> = Vec::new();
        for p in std::mem::take(&mut self.presets) {
            let motif = if !id_valide(&p.id) {
                Some("invalid id")
            } else if gardes.iter().any(|q| q.id == p.id) {
                Some("duplicate id")
            } else if nom_valide(&p.name).is_none() {
                Some("invalid name")
            } else if p.id == ID_PREREGLAGE_PURE && p.chain != PreReglage::pure().chain {
                Some("Pure preset modified")
            } else if gardes.len() >= PREREGLAGES_MAX {
                Some("over the preset limit")
            } else {
                None
            };
            match motif {
                Some(m) => avis.push(format!("TESS preset {:?} skipped: {m}", p.id)),
                None => gardes.push(p),
            }
        }
        self.presets = gardes;
        avis
    }
}

/// Écrit la liste, après [`PreReglages::verifier`]. Une valeur de mode
/// inconnue est enregistrée comme `at_next_playback` (ce qu'elle vaut).
pub fn ecrire(
    settings: &crate::db::settings_repo::SettingsRepo,
    liste: &PreReglages,
) -> Result<(), String> {
    liste.verifier().map_err(|e| e.to_string())?;
    let mut liste = liste.clone();
    if liste.apply_mode == ModeApplication::Unknown {
        liste.apply_mode = ModeApplication::AtNextPlayback;
    }
    let texte = serde_json::to_string(&liste).map_err(|e| e.to_string())?;
    settings.set(CLE_PREREGLAGES, &texte)
}

/// Le nom choisi, nettoyé : espaces de bord retirés, ni vide ni trop long.
pub fn nom_valide(nom: &str) -> Option<String> {
    let n = nom.trim();
    (!n.is_empty()
        && n.chars().count() <= NOM_MAX
        && !n.chars().any(|c| c.is_control() || caractere_de_format(c)))
    .then(|| n.to_owned())
}

/// Caractère invisible de la catégorie Unicode Cf (format) : espaces de
/// largeur nulle, marques et contrôles de direction (qui retournent un nom à
/// l'affichage), BOM, tirets conditionnels, étiquettes.
pub fn caractere_de_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}'
        | '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// Un identifiant stable tiré du nom (« Evening listening » →
/// `evening-listening`), unique dans la liste. `pure` reste réservé au
/// préréglage fourni, même supprimé : un préréglage d'utilisateur ne peut
/// pas le reprendre (il devrait rester sans traitement).
pub fn id_pour_nom(nom: &str, liste: &PreReglages) -> String {
    let mut base = String::new();
    for c in nom.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => 'a',
            'ç' => 'c',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' | 'ø' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ñ' => 'n',
            'ÿ' | 'ý' => 'y',
            c => c,
        };
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    base.truncate(56);
    let base = base.trim_matches('-').to_owned();
    let base = if base.is_empty() {
        "preset".to_owned()
    } else {
        base
    };
    let pris = |id: &str| id == ID_PREREGLAGE_PURE || liste.get(id).is_some();
    if !pris(&base) {
        return base;
    }
    let libre = (2..).map(|n| format!("{base}-{n}")).find(|id| !pris(id));
    libre.unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::MaillonGreffon;
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::resolution::{
        Badge, ContexteExecution, ControleVolume, SourceFlux, VolumeZone,
    };
    use crate::tess::{ContexteValidation, TransportZone};
    use serde_json::{Value, json};

    fn soiree() -> PreReglage {
        PreReglage {
            id: "evening-listening".into(),
            name: "Evening listening".into(),
            builtin: false,
            chain: ChaineTess {
                plugins: vec![MaillonGreffon {
                    id: "equalizer-graphic".into(),
                    enabled: true,
                    settings: Value::Null,
                }],
                ..ChaineTess::default()
            },
        }
    }

    #[test]
    fn les_prereglages_font_l_aller_retour_json() {
        let mut l = PreReglages::default();
        l.presets.push(soiree());
        l.apply_mode = ModeApplication::AtNextPlayback;
        l.verifier().unwrap();
        let texte = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<PreReglages>(&texte).unwrap(), l);
        assert_eq!(
            serde_json::to_value(l.apply_mode).unwrap(),
            json!("at_next_playback")
        );
        assert_eq!(
            serde_json::to_value(ModeApplication::Immediately).unwrap(),
            json!("immediately")
        );
    }

    fn tout(_: &str) -> bool {
        true
    }

    #[test]
    fn pure_est_bit_perfect_meme_sans_droit() {
        let p = PreReglage::pure();
        p.chain
            .valider(
                &catalogue_de_reference(),
                ContexteValidation { premium: false },
            )
            .unwrap();
        let ctx = ContexteExecution {
            transport: TransportZone::Local,
            source: Some(SourceFlux::Pcm {
                sample_rate_hz: 44_100,
                bits: 16,
                channels: 2,
            }),
            premium: false,
            greffon_disponible: &tout,
            pure: false,
            volume: VolumeZone {
                control: ControleVolume::Software,
                db: 0.0,
            },
        };
        let r = p.chain.resoudre(&catalogue_de_reference(), &ctx);
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(r.headroom_db, 0.0);
        // Contre-épreuve : le préréglage « soirée » traite.
        let r = soiree().chain.resoudre(&catalogue_de_reference(), &ctx);
        assert_eq!(r.badge, Badge::ProcessedByTess);
    }

    #[test]
    fn une_liste_de_prereglages_incoherente_est_refusee() {
        let base = || {
            let mut l = PreReglages::default();
            l.presets.push(soiree());
            l
        };
        base().verifier().unwrap();

        let mut l = base();
        l.presets.push(soiree());
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::IdEnDouble("evening-listening".into()))
        );

        let mut l = base();
        l.presets[1].name = "  ".into();
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::NomVide("evening-listening".into()))
        );

        // Même règle d'identifiant que les greffons (et que le schéma).
        for mauvais in ["Evening listening", "-x", "x-", ""] {
            let mut l = base();
            l.presets[1].id = mauvais.into();
            assert_eq!(
                l.verifier(),
                Err(ErreurPreReglage::IdInvalide(mauvais.into()))
            );
        }

        let mut l = base();
        l.presets[0].chain.bypass_all = false;
        l.presets[0].chain.plugins = soiree().chain.plugins;
        assert_eq!(l.verifier(), Err(ErreurPreReglage::PureModifie));

        // Pure peut être supprimé (la croix de la maquette A2).
        let mut l = base();
        l.presets.remove(0);
        l.verifier().unwrap();
        assert!(l.get(ID_PREREGLAGE_PURE).is_none());
    }

    #[test]
    fn un_client_v1_lit_une_liste_plus_recente() {
        let texte = r#"{"apply_mode":"crossfade","shared":true,
            "presets":[{"id":"x","name":"X","chain":{"version":1},"color":"blue"}]}"#;
        let l: PreReglages = serde_json::from_str(texte).unwrap();
        assert_eq!(l.apply_mode, ModeApplication::Unknown);
        assert!(!l.presets[0].builtin);
        let l: PreReglages = serde_json::from_str(r#"{"presets":[]}"#).unwrap();
        assert_eq!(l.apply_mode, ModeApplication::Immediately);
    }

    // ── Stockage (T17) ──────────────────────────────────────────────

    fn reglages() -> crate::db::settings_repo::SettingsRepo {
        use crate::db::backend::DbBackend;
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
        crate::db::settings_repo::SettingsRepo::with_backend(backend)
    }

    #[test]
    fn la_liste_absente_vaut_pure_seul_et_s_enregistre_t17() {
        let s = reglages();
        let l = lire(&s).unwrap();
        assert_eq!(l, PreReglages::default());
        assert_eq!(l.presets[0].id, ID_PREREGLAGE_PURE);
        assert!(l.apply_mode.immediat());

        let mut l = l;
        l.presets.push(soiree());
        l.apply_mode = ModeApplication::AtNextPlayback;
        ecrire(&s, &l).unwrap();
        assert_eq!(lire(&s).unwrap(), l);
        assert!(!lire(&s).unwrap().apply_mode.immediat());
    }

    #[test]
    fn une_liste_incoherente_n_est_ni_ecrite_ni_lue_t17() {
        let s = reglages();
        let mut l = PreReglages::default();
        l.presets.push(soiree());
        l.presets.push(soiree());
        assert!(ecrire(&s, &l).is_err());
        // Rien n'a été écrit : la liste par défaut reste.
        assert_eq!(lire(&s).unwrap(), PreReglages::default());
        // Une base (ou un fichier) illisible est une erreur, jamais une
        // liste vide qui écraserait l'ancienne.
        s.set(CLE_PREREGLAGES, "{pas du json").unwrap();
        assert!(lire(&s).is_err());
        // Lisible mais incohérente (base écrite par une version plus lâche) :
        // le doublon est écarté, le reste se lit (aucune route ne tombe).
        s.set(CLE_PREREGLAGES, &serde_json::to_string(&l).unwrap())
            .unwrap();
        l.presets.pop();
        assert_eq!(lire(&s).unwrap(), l);
        // Contre-épreuve : l'import (lecture stricte) refuse le doublon.
        let mut double = l.clone();
        double.presets.push(soiree());
        assert!(
            lire_texte(&serde_json::to_string(&double).unwrap())
                .unwrap_err()
                .contains("deux fois")
        );
    }

    #[test]
    fn un_mode_inconnu_est_enregistre_comme_au_prochain_demarrage_t17() {
        let s = reglages();
        let l = PreReglages {
            apply_mode: ModeApplication::Unknown,
            ..PreReglages::default()
        };
        assert!(!l.apply_mode.immediat());
        ecrire(&s, &l).unwrap();
        assert_eq!(
            lire(&s).unwrap().apply_mode,
            ModeApplication::AtNextPlayback
        );
    }

    #[test]
    fn l_identifiant_vient_du_nom_et_reste_unique_t17() {
        let mut l = PreReglages::default();
        assert_eq!(id_pour_nom("Evening listening", &l), "evening-listening");
        assert_eq!(id_pour_nom("  Écoute du soir !  ", &l), "ecoute-du-soir");
        assert_eq!(id_pour_nom("耳机", &l), "preset");
        l.presets.push(soiree());
        assert_eq!(id_pour_nom("Evening listening", &l), "evening-listening-2");
        // `pure` reste réservé, même une fois le préréglage fourni supprimé.
        assert_eq!(id_pour_nom("Pure", &l), "pure-2");
        l.presets.remove(0);
        assert_eq!(id_pour_nom("Pure", &l), "pure-2");
        let long = "x".repeat(200);
        let id = id_pour_nom(&long, &l);
        assert!(
            crate::tess::greffon::id_valide(&id) && id.len() <= 64,
            "{id}"
        );
        // Chaque identifiant produit passe la règle des préréglages.
        for nom in ["--a--", "A b", "Ünïcödé", "9", "évier---"] {
            let id = id_pour_nom(nom, &l);
            assert!(crate::tess::greffon::id_valide(&id), "{nom} → {id}");
        }
    }

    #[test]
    fn le_nom_est_nettoye_et_borne_t17() {
        assert_eq!(nom_valide("  Casque  ").as_deref(), Some("Casque"));
        assert_eq!(nom_valide("   "), None);
        assert_eq!(nom_valide("a\nb"), None);
        assert!(nom_valide(&"é".repeat(NOM_MAX)).is_some());
        assert_eq!(nom_valide(&"é".repeat(NOM_MAX + 1)), None);
    }

    #[test]
    fn verifier_borne_le_nom_comme_la_route_suite_t17() {
        let base = || {
            let mut l = PreReglages::default();
            l.presets.push(soiree());
            l
        };
        for mauvais in [
            "é".repeat(NOM_MAX + 1),
            "a\u{7}b".into(),
            "Soir\nMatin".into(),
        ] {
            let mut l = base();
            l.presets[1].name = mauvais.clone();
            assert_eq!(
                l.verifier(),
                Err(ErreurPreReglage::NomInvalide("evening-listening".into())),
                "{mauvais:?}"
            );
            // L'import (même lecture) l'écarte aussi.
            assert!(lire_texte(&serde_json::to_string(&l).unwrap()).is_err());
        }
        // Contre-épreuve : 80 caractères, espaces de bord compris, passent.
        let mut l = base();
        l.presets[1].name = format!(" {} ", "é".repeat(NOM_MAX));
        l.verifier().unwrap();
        lire_texte(&serde_json::to_string(&l).unwrap()).unwrap();
    }

    #[test]
    fn les_caracteres_invisibles_de_format_sont_refuses_suite_t17() {
        for c in [
            '\u{200B}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}',
            '\u{FEFF}', '\u{00AD}',
        ] {
            let nom = format!("Soi{c}rée");
            assert_eq!(nom_valide(&nom), None, "U+{:04X}", c as u32);
        }
        // Contre-épreuves : accents, idéogrammes, emoji passent.
        for nom in ["Café crème", "耳机", "Écoute 🎧"] {
            assert!(nom_valide(nom).is_some(), "{nom}");
        }
    }

    #[test]
    fn une_entree_stockee_fautive_est_ecartee_sans_faire_tomber_la_liste_suite_t17() {
        let s = reglages();
        let mut l = PreReglages::default();
        l.presets.push(soiree());
        let mut long = soiree();
        long.id = "trop-long".into();
        long.name = "x".repeat(NOM_MAX + 1);
        l.presets.push(long);
        s.set(CLE_PREREGLAGES, &serde_json::to_string(&l).unwrap())
            .unwrap();
        let lue = lire(&s).unwrap();
        assert_eq!(
            lue.presets
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec![ID_PREREGLAGE_PURE, "evening-listening"]
        );
        lue.verifier().unwrap();
        // L'avertissement nomme l'entrée écartée.
        let mut l2 = l.clone();
        assert_eq!(
            l2.assainir(),
            vec!["TESS preset \"trop-long\" skipped: invalid name".to_owned()]
        );
        // Contre-épreuve : sans l'entrée fautive, rien n'est écarté.
        l.presets.pop();
        assert!(l.clone().assainir().is_empty());
    }

    #[test]
    fn deux_cents_preregages_au_plus_suite_t17() {
        let mut l = PreReglages::default();
        for i in 1..PREREGLAGES_MAX {
            let mut p = soiree();
            p.id = format!("p{i}");
            l.presets.push(p);
        }
        assert_eq!(l.presets.len(), PREREGLAGES_MAX);
        l.verifier().unwrap();
        let mut p = soiree();
        p.id = "un-de-trop".into();
        l.presets.push(p);
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::TropNombreux(PREREGLAGES_MAX + 1))
        );
        // L'import refuse ; la base, elle, garde les 200 premiers.
        assert!(lire_texte(&serde_json::to_string(&l).unwrap()).is_err());
        let avis = l.assainir();
        assert_eq!(l.presets.len(), PREREGLAGES_MAX);
        assert_eq!(avis.len(), 1, "{avis:?}");
    }
}
