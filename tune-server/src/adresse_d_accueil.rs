//! L'adresse de première connexion, dite à voix haute au démarrage (#1272).
//!
//! Point de friction récurrent du forum (#41831 p.24-26, *harmonique131*,
//! *kole*, *Papytechnofil*) : on tape l'adresse du serveur **sans `http://`**,
//! ou **sans `:8888`**, et on n'arrive nulle part. Sous Android, `.local` ne
//! se résout pas du tout : l'IP est la seule voie.
//!
//! Le serveur CONNAÎT ces adresses — `server_urls()` les calcule déjà, avec la
//! remarque Android écrite dans son commentaire — mais elles ne sortaient
//! jamais du bloc JSON de `GET /system/config`. Ni console, ni journal, ni
//! installeur : la personne devant la machine (fenêtre de console Windows,
//! `docker logs`, `journalctl`) n'avait rien à dicter à celle qui tient le
//! téléphone, et devait deviner exactement ce que #1272 dit qu'on devine mal.
//!
//! ⚠️ **Ce n'est pas la réponse complète à #1272.** Pour qu'une adresse SANS
//! port fonctionne, il faut que quelque chose réponde sur le **port 80** —
//! seules les images Tune OS le font aujourd'hui (`systemd-socket-proxyd`,
//! `image/build-nuc-image.sh`), et les installations Windows / macOS / tarball
//! / Docker n'ont aucun équivalent. Ce module ne fait que la moitié qui ne
//! demande aucun arbitrage : dire l'adresse COMPLÈTE, à l'endroit où
//! quelqu'un la lit.

/// Les lignes imprimées sur la console juste après que le serveur écoute.
///
/// Séparé de l'impression pour rester testable : c'est le CONTENU qui porte
/// le raisonnement, pas sa mise en forme. Chaque adresse doit être **complète**
/// — schéma ET port — parce que le défaut qu'on corrige est précisément une
/// adresse incomplète recopiée de mémoire.
///
/// `urls` vient de `routes::system::server_urls(port)` : l'IP du réseau local
/// d'abord (ou `TUNE_ADVERTISE_IP`), puis `<hôte>.local` quand il y en a un.
pub(crate) fn lignes_d_accueil(port: u16, urls: &[String]) -> Vec<String> {
    let mut lignes = vec!["Tune is listening. Open it at:".to_string()];

    for url in urls {
        lignes.push(format!("  {url}"));
    }

    // Toujours proposer la boucle locale : c'est la seule adresse qui marche
    // à coup sûr depuis la machine elle-même, y compris quand aucune IP de
    // réseau local n'a pu être déterminée.
    let locale = format!("http://localhost:{port}");
    if !urls.iter().any(|u| *u == locale) {
        lignes.push(format!("  {locale}  (from this machine only)"));
    }

    // La mise en garde ne s'imprime que si une adresse `.local` est proposée :
    // sinon elle enverrait chercher un problème qui ne se pose pas.
    if urls.iter().any(|u| u.contains(".local:")) {
        lignes.push("  Android does not resolve .local — use the IP address above.".to_string());
    }

    // Aucune IP de réseau local trouvée : le dire, plutôt que de laisser
    // quelqu'un chercher pourquoi son téléphone n'arrive nulle part.
    if !urls.iter().any(|u| adresse_ip(u)) {
        lignes.push(
            "  No LAN address detected — other devices cannot reach this server yet.".to_string(),
        );
    }

    lignes
}

/// L'URL porte-t-elle une adresse IP littérale plutôt qu'un nom ?
///
/// Volontairement grossier : on ne valide pas une IP, on distingue « une
/// adresse qu'un téléphone Android peut atteindre » d'un nom `.local` qu'il ne
/// résoudra pas. Un hôte fait de chiffres et de points en est une.
fn adresse_ip(url: &str) -> bool {
    let sans_schema = url.strip_prefix("http://").unwrap_or(url);
    let hote = sans_schema.split(['/', ':']).next().unwrap_or("");
    // IPv6 littéral : `http://[fe80::1]:8888`.
    if hote.starts_with('[') || sans_schema.starts_with('[') {
        return true;
    }
    !hote.is_empty() && hote.chars().all(|c| c.is_ascii_digit() || c == '.') && hote.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corps(lignes: &[String]) -> Vec<&str> {
        lignes.iter().skip(1).map(String::as_str).collect()
    }

    /// Le contrat central : **aucune** adresse proposée ne peut être
    /// incomplète. Une ligne sans schéma ou sans port reproduirait, imprimée
    /// par nous cette fois, exactement l'erreur que #1272 décrit.
    #[test]
    fn chaque_adresse_proposee_porte_le_schema_et_le_port() {
        let urls = vec![
            "http://192.168.1.20:8888".to_string(),
            "http://salon.local:8888".to_string(),
        ];
        let lignes = lignes_d_accueil(8888, &urls);
        let mut adresses = 0;
        for ligne in corps(&lignes) {
            let adresse = ligne.trim();
            if !adresse.starts_with("http") {
                continue; // une mise en garde, pas une adresse
            }
            adresses += 1;
            let adresse = adresse.split_whitespace().next().unwrap();
            assert!(
                adresse.starts_with("http://"),
                "adresse sans schéma : {adresse}"
            );
            assert!(adresse.ends_with(":8888"), "adresse sans port : {adresse}");
        }
        // Sans ce compte, une liste VIDE satisferait la boucle ci-dessus : le
        // test resterait vert alors que le serveur n'annonce plus rien. La
        // contre-épreuve l'a montré — c'est le seul des sept qui survivait à
        // la neutralisation.
        assert!(
            adresses >= 3,
            "IP, .local et boucle locale attendues, {adresses} trouvée(s) : {lignes:?}"
        );
    }

    /// L'IP passe AVANT le `.local` : c'est la seule des deux qu'un téléphone
    /// Android sait joindre, et l'ordre de lecture est l'ordre d'essai.
    #[test]
    fn l_adresse_ip_precede_le_nom_local() {
        let urls = vec![
            "http://192.168.1.20:8888".to_string(),
            "http://salon.local:8888".to_string(),
        ];
        let lignes = lignes_d_accueil(8888, &urls);
        let rang = |motif: &str| {
            lignes
                .iter()
                .position(|l| l.contains(motif))
                .unwrap_or_else(|| panic!("absente : {motif}"))
        };
        assert!(rang("192.168.1.20") < rang("salon.local"));
    }

    /// La mise en garde Android accompagne le `.local`, et seulement lui.
    #[test]
    fn la_mise_en_garde_android_suit_le_point_local_et_rien_d_autre() {
        let avec = lignes_d_accueil(
            8888,
            &[
                "http://192.168.1.20:8888".to_string(),
                "http://salon.local:8888".to_string(),
            ],
        );
        assert!(avec.iter().any(|l| l.contains("Android")));

        // Contre-épreuve : sans `.local` proposé, la mise en garde disparaît.
        let sans = lignes_d_accueil(8888, &["http://192.168.1.20:8888".to_string()]);
        assert!(
            !sans.iter().any(|l| l.contains("Android")),
            "mise en garde imprimée sans qu'aucun .local ne soit proposé : {sans:?}"
        );
    }

    /// Aucune IP trouvée : on le DIT. Sans cette ligne, l'écran affiche une
    /// seule adresse `localhost` et rien n'indique qu'aucun autre appareil ne
    /// peut se connecter — le silence se lit comme « tout va bien ».
    #[test]
    fn l_absence_d_adresse_de_reseau_local_est_annoncee() {
        let lignes = lignes_d_accueil(8888, &[]);
        assert!(lignes.iter().any(|l| l.contains("http://localhost:8888")));
        assert!(lignes.iter().any(|l| l.contains("No LAN address")));

        // Contre-épreuve : dès qu'une IP est là, l'avertissement disparaît.
        let avec = lignes_d_accueil(8888, &["http://192.168.1.20:8888".to_string()]);
        assert!(!avec.iter().any(|l| l.contains("No LAN address")));
    }

    /// Le port n'est pas une constante : un serveur configuré ailleurs qu'en
    /// 8888 doit annoncer SON port, sinon l'aide devient le piège.
    #[test]
    fn le_port_annonce_est_celui_du_serveur() {
        let lignes = lignes_d_accueil(9000, &["http://192.168.1.20:9000".to_string()]);
        assert!(lignes.iter().any(|l| l.contains("http://localhost:9000")));
        assert!(
            !lignes.iter().any(|l| l.contains(":8888")),
            "8888 imprimé sur un serveur qui écoute en 9000 : {lignes:?}"
        );
    }

    /// La boucle locale n'est jamais proposée deux fois.
    #[test]
    fn la_boucle_locale_n_est_pas_dupliquee() {
        let lignes = lignes_d_accueil(8888, &["http://localhost:8888".to_string()]);
        let n = lignes
            .iter()
            .filter(|l| l.contains("http://localhost:8888"))
            .count();
        assert_eq!(n, 1, "{lignes:?}");
    }

    #[test]
    fn une_adresse_ip_se_distingue_d_un_nom() {
        assert!(adresse_ip("http://192.168.1.20:8888"));
        assert!(adresse_ip("http://10.0.0.4:8888"));
        assert!(adresse_ip("http://[fe80::1]:8888"));
        assert!(!adresse_ip("http://salon.local:8888"));
        assert!(!adresse_ip("http://localhost:8888"));
    }
}
