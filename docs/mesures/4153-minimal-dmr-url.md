# #4153 — URL de contrôle absolues dans la découverte DLNA de secours

JP Robbe / OpenAI Codex / jp-robbe-20260917-4153-dmr-url

## Défaut et périmètre

`minimal_dmr::try_xml_description` préfixait systématiquement le serveur
devant `controlURL`, y compris lorsqu'elle contenait déjà une URL absolue.
Une description UPnP valide produisait ainsi
`http://h:p/http://h:p/upnp/renderer/5/AVTransport/control`.

La découverte de secours conserve désormais les URL commençant par
`http://` ou `https://`, pour AVTransport et RenderingControl. Les chemins
relatifs gardent leur comportement existant. Le chemin de découverte
principal, déjà corrigé par #707, n'est pas modifié ; la logique n'est pas
centralisée dans ce correctif limité. Aucune migration ni version changée.

## Base et exécution

- Base main : `73707a08c1658289913058a9843250622be08521` (.153).
- Lot : `batch/jp-p2-discovery-20260917` à cette même base.
- Branche : `fix/jp-robbe-20260917-4153-dmr-url`.
- Worktree Shrek : `/srv/builds/worktrees/jp-4153-20260917`.
- Target distinct : `/srv/cache/tune/targets/jp-4153-20260917`.
- Preuves : `/srv/builds/jp-evidence/jp-4153-20260917`.
- Verrou acquis atomiquement avant modification ; conservé pendant revue.

```sh
export TUNE_TARGET_KEY=jp-4153-20260917 CARGO_BUILD_JOBS=6
. /srv/cache/tune/env.sh
cargo test -p tune-core --lib --no-default-features --features oaat \
  discovery::minimal_dmr -- --nocapture
cargo fmt --all -- --check
cargo clippy -p tune-core --lib --no-default-features --features oaat \
  -- -D clippy::correctness
git diff --check
```

## Tests et contre-épreuve

**6 tests réussis**, dont 4 nouveaux. Formatage, Clippy avec
-D clippy::correctness et git diff --check réussis ; les avertissements
hors du fichier modifié restent visibles dans clippy.log. Les quatre nouveaux tests servent une
description par HTTP sur un port loopback alloué par le système et appellent
le vrai `probe_minimal_dmr` :

- HTTP absolu : même hôte pour AVTransport, autre hôte/port pour RenderingControl ;
- HTTPS absolu : schéma, port et échappement du chemin conservés ;
- chemins relatifs, avec et sans slash initial ;
- XML réellement produit par `upnp_renderer::renderer_description_xml`.

Le serveur factice est arrêté après chaque test ; le test est borné à cinq
secondes. Les URL de contrôle sont inspectées, pas contactées. Le test HTTPS
ne prétend donc pas vérifier une négociation TLS ni une commande SOAP.

**Contre-épreuve** : garde HTTP/HTTPS désactivée dans la production, bloc
`#[cfg(test)]` vérifié inchangé, même commande Cargo. Le binaire compile et
les **3 tests d'URL absolue échouent / 3 autres restent verts**. Le témoin
`tune_renderer_description_keeps_its_absolute_service_urls` dit :
« #4153 : le descripteur Tune ne doit pas produire http://h:p/http://h:p/... »
et compare bien l'adresse doublée à l'adresse absolue attendue.
Les témoins HTTP et HTTPS échouent également sur leur URL, pas sur le réseau.

Restauration par `cp`, empreinte vérifiée, puis **6/6 verts** :

`ad64e271c9c8aa4cea96be64188b04e7089a13bc06f7d40f68bbc82d316b6c63`
(`tune-core/src/discovery/minimal_dmr.rs`, tests inclus).

Journaux : `green.log`, `counter.log`, `counter.exit`, `restored.sha256`,
`restored-green.log`, `fmt.log`, `clippy.log`.
Un premier passage avait une assertion de fixture erronée sur le nom
`Salon (Tune)` : le générateur rend `Salon`. Cette assertion a été corrigée ;
ce premier échec est conservé dans `fixture-name-mismatch.log` et n'est pas
présenté comme une contre-épreuve.

## Limites

L'impact chez le testeur de #4153 n'est toujours pas établi : le journal
concernait une zone fantôme ensuite masquée. Aucun renderer Denon ou
Frontier Silicon réel n'a été validé. La création de zones fantômes (#3688),
les URL relatives au document/URLBase, les schémas autres que HTTP/HTTPS et
la refonte du parseur XML restent hors périmètre.

Ces tests font partie de la bibliothèque tune-core sélectionnée par le job
Test habituel. Leur réussite locale ne remplace ni la CI de PR, ni les tests
multiplateformes du lot/RC. Aucun merge, tag ou déploiement.
