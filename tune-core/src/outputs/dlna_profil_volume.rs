//! #5793 — le volume que le renderer DIT accepter, lu dans son SCPD.
//!
//! Sevy Tabroc (1.0.0-rc2, darTZeel LHC-208 en DLNA, fil 2147) : le curseur
//! bouge, le serveur envoie `0,18 → 0,12 → 0`, aucun refus n'est journalisé,
//! et le volume de l'appareil ne change pas. Sa télécommande et son
//! application, elles, le changent.
//!
//! Tune ne lisait jamais le SCPD de `RenderingControl`. Il supposait deux
//! choses que le contrat UPnP ne garantit pas :
//!
//! - **l'unité** : `DesiredVolume` partait sur 0–100. La plage est celle de la
//!   variable d'état `Volume` (`allowedValueRange`), propre à chaque
//!   appareil : 0–255, 0–60, 0–31 existent. Hors de cette plage, ou sur une
//!   plage plus large, l'ordre est acquitté sans effet audible ;
//! - **le canal** : `Channel` partait toujours à `Master`. La liste permise est
//!   `A_ARG_TYPE_Channel` (`allowedValueList`). Un appareil qui n'annonce que
//!   `LF`/`RF` peut répondre sans erreur et ne rien appliquer.
//!
//! Ce module ne fait que LIRE et CONVERTIR, sans réseau : c'est
//! [`super::dlna::DlnaOutput`] qui va chercher le SCPD une fois et applique le
//! profil. Un SCPD absent ou illisible rend le profil standard (0–100,
//! `Master`), c'est-à-dire exactement la conduite d'avant.

/// Bornes et canaux de `RenderingControl` pour un appareil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilVolume {
    /// `allowedValueRange/minimum` de la variable `Volume`.
    pub min: u32,
    /// `allowedValueRange/maximum` de la variable `Volume`.
    pub max: u32,
    /// `allowedValueList` de `A_ARG_TYPE_Channel`, dans l'ordre du SCPD. Vide
    /// quand l'appareil ne la publie pas.
    pub canaux: Vec<String>,
}

impl Default for ProfilVolume {
    /// La supposition d'avant #5793 : 0–100 sur `Master`.
    fn default() -> Self {
        Self {
            min: 0,
            max: 100,
            canaux: Vec::new(),
        }
    }
}

impl ProfilVolume {
    /// Lit le SCPD de `RenderingControl`. Chaque partie absente ou illisible
    /// garde sa valeur standard : un SCPD partiel ne doit pas faire pire que
    /// pas de SCPD du tout.
    pub fn depuis_scpd(xml: &str) -> Self {
        let mut profil = Self::default();
        for bloc in blocs_de_variables(xml) {
            match balise(bloc, "name").map(str::trim) {
                Some("Volume") => {
                    let min = balise(bloc, "minimum").and_then(|v| v.trim().parse::<u32>().ok());
                    let max = balise(bloc, "maximum").and_then(|v| v.trim().parse::<u32>().ok());
                    if let (Some(min), Some(max)) = (min, max)
                        && max > min
                    {
                        profil.min = min;
                        profil.max = max;
                    }
                }
                Some("A_ARG_TYPE_Channel") => {
                    profil.canaux = valeurs_permises(bloc);
                }
                _ => {}
            }
        }
        profil
    }

    /// Vrai pour le profil supposé avant #5793 (0–100 sur `Master`).
    pub fn est_standard(&self) -> bool {
        self.min == 0 && self.max == 100 && self.canaux_de_commande() == ["Master"]
    }

    /// La consigne Tune (0,0–1,0) dans l'unité de l'appareil.
    pub fn niveau(&self, volume: f64) -> u32 {
        let v = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let etendue = f64::from(self.max - self.min);
        self.min + (v * etendue).round() as u32
    }

    /// Un niveau rapporté par l'appareil, ramené à 0,0–1,0.
    pub fn fraction(&self, niveau: f64) -> f64 {
        let etendue = f64::from(self.max.saturating_sub(self.min));
        if etendue <= 0.0 || !niveau.is_finite() {
            return 0.5;
        }
        ((niveau - f64::from(self.min)) / etendue).clamp(0.0, 1.0)
    }

    /// Les canaux auxquels envoyer `SetVolume`.
    ///
    /// `Master` quand l'appareil le permet ou ne dit rien (conduite d'avant).
    /// Sinon les deux voies avant `LF` et `RF` présentes, et à défaut le
    /// premier canal annoncé.
    pub fn canaux_de_commande(&self) -> Vec<&str> {
        if self.canaux.is_empty() || self.canaux.iter().any(|c| c == "Master") {
            return vec!["Master"];
        }
        let avant: Vec<&str> = self
            .canaux
            .iter()
            .map(String::as_str)
            .filter(|c| *c == "LF" || *c == "RF")
            .collect();
        if !avant.is_empty() {
            return avant;
        }
        vec![self.canaux[0].as_str()]
    }

    /// Le canal à interroger pour `GetVolume` : le premier de la commande.
    pub fn canal_de_lecture(&self) -> &str {
        self.canaux_de_commande()
            .first()
            .copied()
            .unwrap_or("Master")
    }
}

/// Les corps de chaque `<stateVariable …>…</stateVariable>`.
fn blocs_de_variables(xml: &str) -> Vec<&str> {
    let mut blocs = Vec::new();
    let mut reste = xml;
    while let Some(debut) = reste.find("<stateVariable") {
        let apres = &reste[debut..];
        let Some(fin) = apres.find("</stateVariable>") else {
            break;
        };
        blocs.push(&apres[..fin]);
        reste = &apres[fin + "</stateVariable>".len()..];
    }
    blocs
}

/// Le texte de la première `<tag>…</tag>` du bloc.
fn balise<'a>(bloc: &'a str, tag: &str) -> Option<&'a str> {
    let ouvre = format!("<{tag}>");
    let ferme = format!("</{tag}>");
    let debut = bloc.find(&ouvre)? + ouvre.len();
    let fin = bloc[debut..].find(&ferme)? + debut;
    Some(&bloc[debut..fin])
}

/// Toutes les `<allowedValue>` du bloc, rognées, sans les vides.
fn valeurs_permises(bloc: &str) -> Vec<String> {
    let mut valeurs = Vec::new();
    let mut reste = bloc;
    while let Some(v) = balise(reste, "allowedValue") {
        let v = v.trim();
        if !v.is_empty() {
            valeurs.push(v.to_string());
        }
        let Some(pos) = reste.find("</allowedValue>") else {
            break;
        };
        reste = &reste[pos + "</allowedValue>".len()..];
    }
    valeurs
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCPD_STANDARD: &str = r#"<scpd><serviceStateTable>
      <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType>
        <allowedValueList><allowedValue>Master</allowedValue></allowedValueList></stateVariable>
      <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType>
        <allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>
    </serviceStateTable></scpd>"#;

    const SCPD_255_LF_RF: &str = r#"<scpd><serviceStateTable>
      <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType>
        <allowedValueRange><minimum> 0 </minimum><maximum>255</maximum></allowedValueRange></stateVariable>
      <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType>
        <allowedValueList><allowedValue>LF</allowedValue><allowedValue>RF</allowedValue></allowedValueList></stateVariable>
    </serviceStateTable></scpd>"#;

    #[test]
    fn un_scpd_standard_garde_la_conduite_d_avant() {
        let p = ProfilVolume::depuis_scpd(SCPD_STANDARD);
        assert!(p.est_standard());
        assert_eq!(p.niveau(0.18), 18);
        assert_eq!(p.canaux_de_commande(), ["Master"]);
    }

    #[test]
    fn la_plage_et_les_canaux_viennent_du_scpd() {
        let p = ProfilVolume::depuis_scpd(SCPD_255_LF_RF);
        assert_eq!((p.min, p.max), (0, 255));
        assert!(!p.est_standard());
        assert_eq!(p.niveau(0.5), 128);
        assert_eq!(p.niveau(1.0), 255);
        assert_eq!(p.niveau(0.0), 0);
        assert_eq!(p.canaux_de_commande(), ["LF", "RF"]);
        assert_eq!(p.canal_de_lecture(), "LF");
        assert!((p.fraction(51.0) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn un_minimum_non_nul_est_respecte() {
        let p = ProfilVolume {
            min: 10,
            max: 60,
            canaux: vec![],
        };
        assert_eq!(p.niveau(0.0), 10);
        assert_eq!(p.niveau(1.0), 60);
        assert!((p.fraction(35.0) - 0.5).abs() < 1e-9);
        assert_eq!(p.fraction(0.0), 0.0);
    }

    #[test]
    fn un_scpd_illisible_ou_incoherent_rend_le_profil_standard() {
        assert!(ProfilVolume::depuis_scpd("<html>404</html>").est_standard());
        let inverse = r#"<stateVariable><name>Volume</name>
          <allowedValueRange><minimum>100</minimum><maximum>0</maximum></allowedValueRange></stateVariable>"#;
        assert!(ProfilVolume::depuis_scpd(inverse).est_standard());
    }

    #[test]
    fn un_canal_exotique_seul_est_pris_tel_quel() {
        let p = ProfilVolume {
            min: 0,
            max: 100,
            canaux: vec!["CF".into()],
        };
        assert_eq!(p.canaux_de_commande(), ["CF"]);
    }
}
