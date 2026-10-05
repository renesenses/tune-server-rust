//! Montage CIFS : l'echelle de dialectes, et la question « est-ce monte ? ».
//!
//! Ce module existe parce que le montage se fait a DEUX endroits — la route
//! interactive (`routes/network.rs`) et le remontage au demarrage
//! (`startup.rs`) — et que ces deux endroits doivent imperativement parler le
//! meme dialecte.
//!
//! Ils ne le faisaient pas. La route avait appris a negocier (#1834) ; le
//! remontage, lui, imposait toujours `vers=3.0`. Consequence pour Philippe
//! Landes, dont le streamer Rose ne parle que SMB 1.0 : l'assistant montait son
//! partage, il retrouvait sa musique, et le **premier redemarrage le lui
//! reprenait** — le remontage retentait un dialecte que son materiel refuse.
//! L'interface, elle, continuait d'afficher le partage comme monte (#1916).
//!
//! Le commentaire d'origine assumait la recopie : « la route rend des erreurs
//! HTTP a un humain qui attend, celle-ci journalise et passe au suivant ». Cet
//! argument tient toujours pour la *restitution* — et c'est pourquoi les deux
//! appelants gardent la leur. Il ne tient pas pour la *strategie de montage* :
//! deux echelles de dialectes qui divergent, c'est un partage qui monte a
//! l'ecran et se demonte au redemarrage.

use std::time::Duration;

/// Dialectes essayes, dans l'ordre.
///
/// `None` = aucune option `vers=` : c'est ce qui declenche la negociation du
/// noyau, pas une valeur particuliere. Le module CIFS ne negocie de lui-meme
/// qu'entre 2.1, 3.0 et 3.1.1 — il ne descend jamais jusqu'a SMB 1, sorti de la
/// negociation par `CONFIG_CIFS_ALLOW_INSECURE_LEGACY`. Il faut le lui demander
/// explicitement, d'ou les deux echelons du bas.
pub const DIALECTES: [Option<&str>; 3] = [None, Some("2.0"), Some("1.0")];

/// Delai par essai. Trois essais tiennent alors sous le delai d'attente de 60 s
/// de l'API, la ou 15 s l'auraient frole.
pub const ESSAI_TIMEOUT: Duration = Duration::from_secs(10);

/// Etiquette d'un dialecte pour les journaux et pour la base.
///
/// La negociation libre s'ecrit `negocie` plutot que de laisser un trou : une
/// colonne vide se lit « on ne sait pas », alors qu'ici on sait tres bien.
pub fn etiquette(dialecte: Option<&str>) -> &str {
    dialecte.unwrap_or("negocie")
}

/// L'inverse d'[`etiquette`] : ce que la base a retenu redevient une option.
pub fn depuis_etiquette(etiquette: &str) -> Option<&str> {
    match etiquette.trim() {
        "" | "negocie" => None,
        v => Some(v),
    }
}

/// Options passees a `mount.cifs` pour un essai.
///
/// `iocharset=utf8` est impose : sans lui, le noyau convertit les noms de
/// fichiers avec le jeu de caracteres par defaut du systeme, qui n'est pas
/// toujours UTF-8. Tout caractere qu'il ne sait pas representer — apostrophe
/// typographique ’, n tilde ñ, e accent aigu decompose, œ — devient alors un
/// `?` dans le nom que liste le partage. Le fichier apparait au scan, mais
/// `?` n'est pas son vrai nom : l'ouvrir echoue par `No such file or
/// directory`, et la piste est perdue pour la bibliotheque.
///
/// Les deux appelants (route interactive et remontage au demarrage) passent
/// par ici : un partage monte en UTF-8 depuis l'assistant doit le rester au
/// redemarrage.
///
/// La chaine porte le mot de passe : elle ne doit JAMAIS aller dans une trace.
pub fn options_de_montage(user: &str, pass: &str, dialecte: Option<&str>) -> String {
    let mut opts = format!("username={user},password={pass},iocharset=utf8");
    if let Some(v) = dialecte {
        opts.push_str(&format!(",vers={v}"));
    }
    opts
}

#[cfg(test)]
mod options_de_montage_tests {
    use super::options_de_montage;

    /// Retour de terrain : sans `iocharset=utf8`, les noms avec ’, ñ ou é passaient
    /// en `?` et les fichiers devenaient introuvables.
    #[test]
    fn chaque_essai_monte_en_utf8() {
        for dialecte in super::DIALECTES {
            let opts = options_de_montage("u", "p", dialecte);
            assert!(
                opts.split(',').any(|o| o == "iocharset=utf8"),
                "essai {dialecte:?} sans iocharset=utf8 : {opts}"
            );
        }
    }

    #[test]
    fn le_dialecte_reste_une_option_a_part() {
        assert_eq!(
            options_de_montage("u", "p", Some("1.0")),
            "username=u,password=p,iocharset=utf8,vers=1.0"
        );
        assert!(!options_de_montage("u", "p", None).contains("vers="));
    }
}

/// L'echelle a parcourir, le dialecte connu d'abord.
///
/// Un partage qui a deja monte en SMB 1.0 remonte en SMB 1.0 du premier coup :
/// sans cela, chaque demarrage rejouerait deux essais voues a l'echec, soit
/// vingt secondes de retard par partage avant que la bibliotheque ne soit
/// lisible. Le reste de l'echelle suit quand meme — un NAS mis a jour, ou
/// remplace, ne doit pas rester prisonnier de ce qu'il repondait l'an dernier.
pub fn echelle(connu: Option<&str>) -> Vec<Option<&str>> {
    let mut ordre: Vec<Option<&str>> = Vec::with_capacity(DIALECTES.len());
    if let Some(c) = connu {
        // Seul un dialecte de l'echelle est retenu : une valeur aberrante en
        // base ne doit pas devenir une option passee a `mount.cifs`.
        if let Some(d) = DIALECTES.iter().find(|d| etiquette(**d) == c) {
            ordre.push(*d);
        }
    }
    let deja = ordre.clone();
    ordre.extend(DIALECTES.iter().filter(|d| !deja.contains(d)).copied());
    ordre
}

/// Le message d'erreur de `mount.cifs` traduit-il un refus d'identifiants ?
///
/// La distinction porte une decision : un dialecte inadapte se repare en en
/// essayant un autre, un mot de passe refuse non. Reessayer trois fois ferait
/// patienter l'utilisateur trente secondes pour lui resservir la meme reponse.
pub fn est_refus_d_authentification(stderr: &str) -> bool {
    let bas = stderr.to_lowercase();
    bas.contains("permission denied")
        || bas.contains("access denied")
        || bas.contains("bad user name or password")
}

/// Le message de `mount.cifs` dit-il que le point de montage est deja occupe ?
///
/// `mount error(16): Device or resource busy` (EBUSY) : le noyau refuse un
/// second montage sur un point deja monte. Changer de dialecte n'y fera rien —
/// et c'est pourtant ce que faisait l'echelle : apres l'EBUSY, elle essayait
/// `vers=2.0` puis `vers=1.0`, que le NAS refusait (`error(95)`), et c'est ce
/// DERNIER message que l'utilisateur lisait. Daniel Levy (fil 2145) a ainsi vu
/// « Operation not supported » pour un partage qui etait simplement deja monte.
pub fn est_deja_monte(stderr: &str) -> bool {
    let bas = stderr.to_lowercase();
    bas.contains("error(16)") || bas.contains("device or resource busy")
}

/// Faut-il arreter l'echelle des dialectes sur cet echec ?
///
/// Seuls les echecs qu'un autre dialecte ne reparera pas l'arretent : un refus
/// d'identifiants, et un point de montage deja occupe (fil 2145). Les deux
/// appelants — la route interactive et le remontage au demarrage — passent par
/// ici, pour ne pas diverger (voir l'en-tete du module).
pub fn arrete_l_echelle(stderr: &str) -> bool {
    est_refus_d_authentification(stderr) || est_deja_monte(stderr)
}

/// Defaire les echappements octaux de `/proc/self/mounts` (`\040` pour une
/// espace, `\011`, `\012`, `\134`).
fn desechapper_octal(champ: &str) -> String {
    let octets = champ.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len());
    let mut i = 0;
    while i < octets.len() {
        if octets[i] == b'\\'
            && i + 3 < octets.len()
            && (b'0'..=b'3').contains(&octets[i + 1])
            && octets[i + 2..i + 4]
                .iter()
                .all(|c| (b'0'..=b'7').contains(c))
        {
            let v =
                (octets[i + 1] - b'0') * 64 + (octets[i + 2] - b'0') * 8 + (octets[i + 3] - b'0');
            sortie.push(v);
            i += 4;
        } else {
            sortie.push(octets[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&sortie).into_owned()
}

/// La source montee sur `chemin`, lue dans une table au format
/// `/proc/self/mounts`. Le DERNIER montage l'emporte : c'est lui qui est
/// visible quand plusieurs sont empiles sur le meme point.
pub fn source_dans_la_table(table: &str, chemin: &str) -> Option<String> {
    let chemin = chemin.trim_end_matches('/');
    let chemin = if chemin.is_empty() { "/" } else { chemin };
    table.lines().rev().find_map(|ligne| {
        let mut champs = ligne.split_whitespace();
        let source = champs.next()?;
        let cible = champs.next()?;
        (desechapper_octal(cible) == chemin).then(|| desechapper_octal(source))
    })
}

/// La source effectivement montee sur `chemin` (`//hote/partage` pour un
/// montage CIFS). `None` quand on ne sait pas la lire : hors Linux, ou table
/// des montages illisible.
pub fn source_du_montage(chemin: &std::path::Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let canonique = std::fs::canonicalize(chemin).unwrap_or_else(|_| chemin.to_path_buf());
    source_dans_la_table(&table, &canonique.to_string_lossy())
}

/// `source` (lue dans la table des montages) designe-t-elle `//hote/partage` ?
///
/// La comparaison ignore la casse (SMB l'ignore, pour l'hote comme pour le
/// partage), le sens des barres (`\\hote\partage`), les crochets d'une IPv6
/// litterale et une barre finale.
pub fn meme_source(source: &str, hote: &str, partage: &str) -> bool {
    fn normaliser(s: &str) -> String {
        s.replace('\\', "/")
            .replace(['[', ']'], "")
            .trim_end_matches('/')
            .to_lowercase()
    }
    normaliser(source) == normaliser(&format!("//{hote}/{partage}"))
}

/// Identifiant de serveur SMB2 (`ServerGuid` de la reponse NEGOTIATE).
pub type GuidServeur = [u8; 16];

/// Une requete SMB2 NEGOTIATE minimale, cadree NetBIOS (port 445).
///
/// Les dialectes 2.0.2 a 3.0.2 seulement : 3.1.1 exigerait des contextes de
/// negociation, inutiles pour lire le `ServerGuid`.
pub fn requete_negotiate() -> Vec<u8> {
    const DIALECTES_SMB2: [u16; 4] = [0x0202, 0x0210, 0x0300, 0x0302];
    let mut smb = Vec::with_capacity(64 + 36 + 8);
    // En-tete SMB2 (64 octets).
    smb.extend_from_slice(&[0xFE, b'S', b'M', b'B']);
    smb.extend_from_slice(&64u16.to_le_bytes()); // StructureSize
    smb.extend_from_slice(&0u16.to_le_bytes()); // CreditCharge
    smb.extend_from_slice(&0u32.to_le_bytes()); // Status
    smb.extend_from_slice(&0u16.to_le_bytes()); // Command = NEGOTIATE
    smb.extend_from_slice(&1u16.to_le_bytes()); // CreditRequest
    smb.extend_from_slice(&0u32.to_le_bytes()); // Flags
    smb.extend_from_slice(&0u32.to_le_bytes()); // NextCommand
    smb.extend_from_slice(&0u64.to_le_bytes()); // MessageId
    smb.extend_from_slice(&0u32.to_le_bytes()); // Reserved
    smb.extend_from_slice(&0u32.to_le_bytes()); // TreeId
    smb.extend_from_slice(&0u64.to_le_bytes()); // SessionId
    smb.extend_from_slice(&[0u8; 16]); // Signature
    // Corps NEGOTIATE.
    smb.extend_from_slice(&36u16.to_le_bytes()); // StructureSize
    smb.extend_from_slice(&(DIALECTES_SMB2.len() as u16).to_le_bytes());
    smb.extend_from_slice(&1u16.to_le_bytes()); // SecurityMode : signature possible
    smb.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    smb.extend_from_slice(&0u32.to_le_bytes()); // Capabilities
    smb.extend_from_slice(b"Tune-client-guid"); // ClientGuid (16 octets)
    smb.extend_from_slice(&0u64.to_le_bytes()); // ClientStartTime
    for d in DIALECTES_SMB2 {
        smb.extend_from_slice(&d.to_le_bytes());
    }
    let mut trame = Vec::with_capacity(4 + smb.len());
    trame.push(0);
    let n = smb.len() as u32;
    trame.extend_from_slice(&n.to_be_bytes()[1..]);
    trame.extend_from_slice(&smb);
    trame
}

/// Le `ServerGuid` d'une reponse NEGOTIATE (trame NetBIOS comprise).
///
/// `None` si la reponse n'est pas un NEGOTIATE SMB2 reussi, ou si le GUID est
/// nul : un GUID nul ne distingue personne, le prendre pour une identite
/// ferait confondre deux serveurs.
pub fn guid_de_la_reponse(trame: &[u8]) -> Option<GuidServeur> {
    let smb = trame.get(4..)?;
    if smb.get(0..4)? != [0xFE, b'S', b'M', b'B'] {
        return None;
    }
    let statut = u32::from_le_bytes(smb.get(8..12)?.try_into().ok()?);
    let commande = u16::from_le_bytes(smb.get(12..14)?.try_into().ok()?);
    if statut != 0 || commande != 0 {
        return None;
    }
    let corps = smb.get(64..)?;
    if u16::from_le_bytes(corps.get(0..2)?.try_into().ok()?) != 65 {
        return None;
    }
    let guid: GuidServeur = corps.get(8..24)?.try_into().ok()?;
    (guid != [0u8; 16]).then_some(guid)
}

/// Demander son `ServerGuid` au serveur SMB `hote:port`.
///
/// C'est ce qui reconnait un meme NAS sous deux adresses (fil 2145 : un
/// Synology vu en IPv6 par la decouverte, puis en IPv4 par la saisie). Samba
/// derive ce GUID du nom NetBIOS du serveur (`smbd_server_guid`), Windows le
/// garde en registre : il est donc le meme sur toutes les interfaces d'une
/// machine, quelle que soit l'adresse par laquelle on l'interroge.
///
/// `None` si le serveur ne repond pas, ou seulement en SMB1 : on ne sait
/// alors pas conclure, et l'appelant ne doit rien fusionner.
pub async fn guid_du_serveur(hote: &str, port: u16) -> Option<GuidServeur> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hote = hote.trim_start_matches('[').trim_end_matches(']');
    let essai = async {
        let mut flux = tokio::net::TcpStream::connect((hote, port)).await.ok()?;
        flux.write_all(&requete_negotiate()).await.ok()?;
        let mut entete = [0u8; 4];
        flux.read_exact(&mut entete).await.ok()?;
        let n = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]) as usize;
        if n > 64 * 1024 {
            return None;
        }
        let mut reste = vec![0u8; n];
        flux.read_exact(&mut reste).await.ok()?;
        let mut trame = entete.to_vec();
        trame.extend_from_slice(&reste);
        guid_de_la_reponse(&trame)
    };
    tokio::time::timeout(Duration::from_secs(3), essai)
        .await
        .ok()
        .flatten()
}

/// Le chemin est-il reellement un point de montage ?
///
/// Le garde-fou « deja monte » du remontage testait `read_dir().next().
/// is_some()` : *il y a des fichiers, donc c'est monte*. Un point de montage
/// non monte mais portant des residus — le scan a ecrit dedans pendant que le
/// partage etait tombe, ou un `mount` precedent a laisse des fichiers — faisait
/// donc sauter le remontage **sans un mot**, et l'utilisateur se retrouvait
/// avec une bibliotheque a moitie lisible que rien n'expliquait.
///
/// Le test juste est celui de `mountpoint(1)` : un point de montage ne porte
/// pas le meme peripherique que son parent. Il ne depend d'aucun format de
/// fichier systeme, donc il vaut sur Linux comme sur macOS.
///
/// La racine `/` est son propre parent : elle est donc toujours vue comme
/// montee, ce qui est exact — et de toute facon aucun partage n'y est monte.
#[cfg(unix)]
pub fn est_un_point_de_montage(chemin: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(ici) = std::fs::metadata(chemin) else {
        return false;
    };
    let Some(parent) = chemin.parent() else {
        return true;
    };
    match std::fs::metadata(parent) {
        Ok(dessus) => ici.dev() != dessus.dev(),
        // Parent illisible : on ne peut pas conclure. Repondre « non monte »
        // ferait retenter un montage par-dessus un montage existant.
        Err(_) => true,
    }
}

/// Windows ne monte pas de partage CIFS par cette route (`mount.cifs` n'y
/// existe pas) ; la question ne s'y pose donc jamais.
#[cfg(not(unix))]
pub fn est_un_point_de_montage(_chemin: &std::path::Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_refus_d_identifiants_arrete_les_essais() {
        assert!(est_refus_d_authentification(
            "mount error(13): Permission denied"
        ));
        assert!(est_refus_d_authentification("Access denied"));
        assert!(est_refus_d_authentification(
            "mount error: bad user name or password"
        ));
    }

    /// Le cas de Philippe Landes : `mount error(22): Invalid argument`, obtenu
    /// avec vers=3.0, puis en negociation libre, puis avec vers=2.0 — alors que
    /// `smbclient -L` listait le partage avec les MEMES identifiants. Si ce
    /// message etait pris pour un refus d'authentification, la boucle
    /// s'arreterait au premier essai et n'atteindrait jamais le dialecte qui
    /// marche : le correctif ne corrigerait rien.
    #[test]
    fn un_dialecte_inadapte_laisse_la_boucle_continuer() {
        assert!(!est_refus_d_authentification(
            "mount error(22): Invalid argument"
        ));
        assert!(!est_refus_d_authentification(
            "mount error(112): Host is down"
        ));
        assert!(!est_refus_d_authentification("Device or resource busy"));
        assert!(!est_refus_d_authentification(""));
    }

    /// Fil 2145 (Daniel Levy) : `negocie` rendait `mount error(16): Device or
    /// resource busy`, l'echelle continuait sur 2.0 puis 1.0, et l'utilisateur
    /// lisait l'`error(95)` du dernier essai. L'EBUSY doit arreter l'echelle.
    #[test]
    fn un_point_deja_occupe_arrete_l_echelle() {
        let ebusy = "mount error(16): Device or resource busy\n\
                     Refer to the mount.cifs(8) manual page (e.g. man mount.cifs)";
        assert!(est_deja_monte(ebusy));
        assert!(arrete_l_echelle(ebusy));
        assert!(arrete_l_echelle("mount error(13): Permission denied"));
        // Un dialecte refuse, lui, doit laisser l'echelle continuer.
        assert!(!arrete_l_echelle(
            "mount error(95): Operation not supported"
        ));
        assert!(!arrete_l_echelle("mount error(22): Invalid argument"));
        assert!(!est_deja_monte("mount error(112): Host is down"));
    }

    #[test]
    fn la_source_d_un_point_se_lit_dans_la_table_des_montages() {
        let table = "\
proc /proc proc rw,nosuid 0 0
//192.168.10.69/Music /mnt/192.168.10.69_Music cifs rw,vers=3.1.1 0 0
//fd12::1/Ma\\040Musique /mnt/fd12::1_Ma_Musique cifs rw 0 0
tmpfs /mnt/empile tmpfs rw 0 0
//nas/A /mnt/empile cifs rw 0 0
";
        assert_eq!(
            source_dans_la_table(table, "/mnt/192.168.10.69_Music").as_deref(),
            Some("//192.168.10.69/Music")
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/192.168.10.69_Music/").as_deref(),
            Some("//192.168.10.69/Music"),
            "une barre finale ne change pas le point"
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/fd12::1_Ma_Musique").as_deref(),
            Some("//fd12::1/Ma Musique"),
            "les echappements octaux sont defaits"
        );
        assert_eq!(
            source_dans_la_table(table, "/mnt/empile").as_deref(),
            Some("//nas/A"),
            "le dernier montage empile est celui qu'on voit"
        );
        assert_eq!(source_dans_la_table(table, "/mnt/absent"), None);
    }

    #[test]
    fn la_meme_source_s_ecrit_de_plusieurs_facons() {
        assert!(meme_source(
            "//192.168.10.69/Music",
            "192.168.10.69",
            "Music"
        ));
        assert!(meme_source(
            "//192.168.10.69/music/",
            "192.168.10.69",
            "Music"
        ));
        assert!(meme_source(r"\\NAS\Music", "nas", "music"));
        assert!(meme_source("//fd12::1/Music", "[fd12::1]", "Music"));
        assert!(!meme_source(
            "//192.168.10.69/Video",
            "192.168.10.69",
            "Music"
        ));
        assert!(!meme_source(
            "//192.168.10.70/Music",
            "192.168.10.69",
            "Music"
        ));
        assert!(!meme_source("tmpfs", "192.168.10.69", "Music"));
    }

    /// Une reponse NEGOTIATE SMB2 telle qu'un serveur la rend.
    fn reponse_negotiate(guid: GuidServeur, statut: u32) -> Vec<u8> {
        let mut smb = vec![0u8; 64];
        smb[0..4].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        smb[4..6].copy_from_slice(&64u16.to_le_bytes());
        smb[8..12].copy_from_slice(&statut.to_le_bytes());
        let mut corps = vec![0u8; 64];
        corps[0..2].copy_from_slice(&65u16.to_le_bytes());
        corps[4..6].copy_from_slice(&0x0302u16.to_le_bytes());
        corps[8..24].copy_from_slice(&guid);
        smb.extend_from_slice(&corps);
        let mut trame = vec![0u8];
        trame.extend_from_slice(&(smb.len() as u32).to_be_bytes()[1..]);
        trame.extend_from_slice(&smb);
        trame
    }

    #[test]
    fn la_requete_negotiate_est_bien_cadree() {
        let r = requete_negotiate();
        let n = u32::from_be_bytes([0, r[1], r[2], r[3]]) as usize;
        assert_eq!(r[0], 0);
        assert_eq!(n, r.len() - 4, "longueur NetBIOS");
        assert_eq!(&r[4..8], &[0xFE, b'S', b'M', b'B']);
        assert_eq!(u16::from_le_bytes([r[68], r[69]]), 36, "StructureSize");
        assert_eq!(n, 64 + 36 + 2 * 4);
    }

    #[test]
    fn le_guid_se_lit_dans_la_reponse() {
        let guid = *b"daniel-synology!";
        assert_eq!(guid_de_la_reponse(&reponse_negotiate(guid, 0)), Some(guid));
        // Un refus (STATUS_NOT_SUPPORTED) ou un GUID nul n'identifient rien.
        assert_eq!(
            guid_de_la_reponse(&reponse_negotiate(guid, 0xC00000BB)),
            None
        );
        assert_eq!(guid_de_la_reponse(&reponse_negotiate([0; 16], 0)), None);
        // Une reponse SMB1 (0xFF 'SMB') non plus.
        let mut smb1 = reponse_negotiate(guid, 0);
        smb1[4] = 0xFF;
        assert_eq!(guid_de_la_reponse(&smb1), None);
        assert_eq!(guid_de_la_reponse(&[0, 0]), None);
    }

    /// Un faux serveur SMB2 qui rend `guid` a chaque NEGOTIATE.
    async fn faux_serveur(adresse: &str, guid: GuidServeur) -> Option<u16> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ecoute = tokio::net::TcpListener::bind((adresse, 0)).await.ok()?;
        let port = ecoute.local_addr().ok()?.port();
        tokio::spawn(async move {
            while let Ok((mut flux, _)) = ecoute.accept().await {
                let mut entete = [0u8; 4];
                if flux.read_exact(&mut entete).await.is_err() {
                    continue;
                }
                let n = u32::from_be_bytes([0, entete[1], entete[2], entete[3]]) as usize;
                let mut reste = vec![0u8; n];
                let _ = flux.read_exact(&mut reste).await;
                let _ = flux.write_all(&reponse_negotiate(guid, 0)).await;
            }
        });
        Some(port)
    }

    /// Le cas du fil 2145 rejoue : le meme serveur interroge par son IPv4 et
    /// par son IPv6 rend le meme GUID ; un autre serveur, un autre.
    #[tokio::test]
    async fn le_meme_serveur_se_reconnait_par_ses_deux_adresses() {
        let nas = *b"daniel-synology!";
        let p4 = faux_serveur("127.0.0.1", nas).await.expect("ecoute IPv4");
        let par_ipv4 = guid_du_serveur("127.0.0.1", p4).await;
        assert_eq!(par_ipv4, Some(nas));
        if let Some(p6) = faux_serveur("::1", nas).await {
            assert_eq!(guid_du_serveur("[::1]", p6).await, par_ipv4);
        }
        let autre = faux_serveur("127.0.0.1", *b"un-autre-serveur")
            .await
            .unwrap();
        assert_ne!(guid_du_serveur("127.0.0.1", autre).await, par_ipv4);
    }

    #[tokio::test]
    async fn un_serveur_muet_n_a_pas_d_identite() {
        // Port ferme : on ne sait pas, et on le dit.
        let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ecoute.local_addr().unwrap().port();
        drop(ecoute);
        assert_eq!(guid_du_serveur("127.0.0.1", port).await, None);
    }

    /// La casse de `mount.cifs` varie selon les versions.
    #[test]
    fn la_casse_du_message_ne_change_rien() {
        assert!(est_refus_d_authentification("PERMISSION DENIED"));
        assert!(est_refus_d_authentification(
            "Mount Error(13): Permission Denied"
        ));
    }

    #[test]
    fn sans_dialecte_connu_l_echelle_est_celle_d_origine() {
        assert_eq!(echelle(None), DIALECTES.to_vec());
    }

    /// Le cas de Philippe apres son premier montage reussi : SMB 1.0 est en
    /// base, il doit repasser en premier. Sans cela, chaque demarrage rejoue
    /// deux essais de dix secondes avant d'arriver au seul qui marche.
    #[test]
    fn le_dialecte_connu_passe_en_premier_sans_perdre_les_autres() {
        let ordre = echelle(Some("1.0"));
        assert_eq!(ordre.first(), Some(&Some("1.0")));
        assert_eq!(
            ordre.len(),
            DIALECTES.len(),
            "aucun dialecte perdu : {ordre:?}"
        );
        for d in DIALECTES {
            assert!(ordre.contains(&d), "{d:?} manque dans {ordre:?}");
        }
    }

    /// La negociation libre est un dialecte comme un autre une fois retenue.
    #[test]
    fn la_negociation_libre_se_relit_depuis_la_base() {
        assert_eq!(etiquette(None), "negocie");
        assert_eq!(depuis_etiquette("negocie"), None);
        assert_eq!(depuis_etiquette(""), None);
        assert_eq!(depuis_etiquette("1.0"), Some("1.0"));
        assert_eq!(echelle(Some("negocie")).first(), Some(&None));
    }

    /// Une valeur aberrante en base — colonne editee a la main, migration
    /// bancale — ne doit pas devenir une option `vers=` passee au noyau.
    #[test]
    fn un_dialecte_inconnu_en_base_est_ignore() {
        let ordre = echelle(Some("4.2"));
        assert_eq!(ordre, DIALECTES.to_vec());
    }

    /// `/tmp` n'est pas un point de montage sur toutes les machines, mais un
    /// repertoire quelconque cree dans le repertoire temporaire n'en est
    /// JAMAIS un — c'est exactement le cas que l'ancien garde-fou confondait
    /// des qu'il contenait un fichier.
    #[cfg(unix)]
    #[test]
    fn un_repertoire_avec_des_residus_n_est_pas_un_point_de_montage() {
        let base = tune_core::test_scratch::scratch_dir("tune_smb_test");
        std::fs::write(base.join("residu.flac"), b"x").unwrap();
        assert!(
            !est_un_point_de_montage(&base),
            "un repertoire ordinaire portant des fichiers a ete pris pour un montage"
        );
    }

    #[cfg(unix)]
    #[test]
    fn un_chemin_absent_n_est_pas_un_point_de_montage() {
        assert!(!est_un_point_de_montage(std::path::Path::new(
            "/n/existe/pas/du/tout"
        )));
    }
}
