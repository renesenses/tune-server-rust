# librespot fourni avec Tune (#6872)

Le programme indépendant librespot 0.8.0 est compilé depuis l'archive crates.io
dont le SHA-256 est épinglé dans `scripts/build-librespot.py`. Son Cargo.lock
est conservé (`--locked`). Les fonctionnalités sont identiques pour toutes les
cibles : TLS rustls avec racines webpki et découverte libmdns ; le backend pipe
utilisé par Tune est disponible sans backend audio système.

Le helper construit un binaire pour chaque cible de release (Linux x86_64,
Linux ARM64 GNU/musl, macOS Intel/Apple Silicon, Windows x86_64). Les cibles
Linux ARM64 utilisent `cross` ; aucun binaire étranger n'est exécuté sur l'hôte.
La CRT Windows est statique. Un échec de construction ou de génération des
mentions arrête le paquetage, sans conserver d'ancien binaire.

Chaque paquet emporte LIBRESPOT-LICENSE.txt et LIBRESPOT-THIRD-PARTY.txt.
Les mentions proviennent de cargo-about 0.9.3, sur le même Cargo.lock, la même
cible et les mêmes fonctionnalités que le binaire. Une mise à jour de librespot
demande de changer version et empreinte ensemble, puis de vérifier construction,
mentions et paquetage pour chaque cible. Les conteneurs rangent ces textes dans
/usr/share/doc/librespot/.

macOS : le helper est signé et horodaté avant l'archive. Le .app reprend ce
binaire signé avant sa propre signature et sa notarisation.
Windows : il rejoint la liste Authenticode existante lorsque le certificat est
disponible, puis le zip et NSIS copient ce même fichier.

Témoin ciblé hors réseau et sans son :
`python3 scripts/test-librespot-6872.py`.
Les constructions réelles et signatures de chaque runner restent nécessaires :
les témoins de configuration ne prouvent ni notarisation ni lecture Spotify.
