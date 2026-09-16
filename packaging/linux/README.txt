Tune Server — archive Linux / Linux archive
===========================================

FRANÇAIS
--------

Cette archive est une distribution autonome du serveur Rust. Elle ne contient
pas install.sh et n'installe pas de service systemd. L'ancien install.sh du
serveur Python n'est pas l'installateur de cette édition.

Lancement manuel
1. Extrayez l'archive dans un dossier où votre utilisateur peut écrire.
2. Ouvrez un terminal DANS ce dossier, puis lancez sans sudo :
     ./tune-server
3. Ouvrez http://localhost:8888 dans votre navigateur.
   Arrêtez le serveur avec Ctrl+C dans le terminal.

Gardez web/, plugins/ et les autres fichiers livrés à côté du binaire.
Par défaut, la base tune.db est créée dans le dossier courant. Gardez les
données et votre configuration lors des mises à jour ; n'effacez pas le
dossier d'une installation existante pour remplacer le programme.
Le port peut être choisi au lancement, par exemple :
  TUNE_PORT=8889 ./tune-server
Documentation des réglages :
https://github.com/renesenses/tune-server-rust#configuration

Installation comme service sur Debian / Ubuntu et dérivés compatibles
Utilisez le paquet .deb de la MÊME release, disponible dans ses « Assets » :
https://github.com/renesenses/tune-server-rust/releases
Debian 12+, Ubuntu 22.04+ et dérivés de ces bases sont pris en charge.
Pour connaître l'architecture du paquet sur votre système :
  dpkg --print-architecture
Choisissez le fichier tune-server_<version>_amd64.deb ou _arm64.deb
correspondant, puis utilisez son nom réel :
  sudo apt install ./tune-server_<version>_<arch>.deb
Le paquet crée l'utilisateur tune, installe sous /opt/tune et démarre
tune-server.service. Réglages : /etc/default/tune-server.
Après modification des réglages :
  sudo systemctl restart tune-server
Diagnostic :
  systemctl status tune-server
  journalctl -u tune-server -n 100 --no-pager

Si vous aviez l'ancien serveur Python ou une unité créée à la main,
inspectez AVANT l'installation :
  systemctl cat tune-server
Une unité dans /etc/systemd/system ou un override peut primer sur celle
livrée par le paquet. Si ExecStart pointe vers l'ancienne installation,
faites adapter cette unité et sauvegardez ses réglages et données avant
de changer d'installation. Le .deb n'importe pas les données Python.

Sur une autre distribution, apt et le .deb ne s'appliquent pas : utilisez
le lancement manuel ci-dessus. Cette archive ne fournit pas d'unité
systemd générique ; demandez une procédure adaptée en précisant votre
distribution, sa version et l'architecture :
https://mozaiklabs.fr/forum

ENGLISH
-------

This is a standalone archive of the Rust server. It has no install.sh and
does not install a systemd service. The old Python server's install.sh is
not an installer for this edition.

Manual start
1. Extract the archive into a directory writable by your user.
2. Open a terminal IN that directory and run without sudo:
     ./tune-server
3. Open http://localhost:8888 in your browser.
   Press Ctrl+C in the terminal to stop the server.

Keep web/, plugins/ and the other bundled files beside the executable.
By default, tune.db is created in the current directory. Preserve your data
and configuration when upgrading; do not delete an existing installation
directory to replace the program.
To choose another port, for example:
  TUNE_PORT=8889 ./tune-server
Configuration reference:
https://github.com/renesenses/tune-server-rust#configuration

Service installation on Debian / Ubuntu and compatible derivatives
Download the .deb from the SAME release's Assets:
https://github.com/renesenses/tune-server-rust/releases
Debian 12+, Ubuntu 22.04+ and derivatives of those bases are supported.
Find your package architecture with:
  dpkg --print-architecture
Choose the matching tune-server_<version>_amd64.deb or _arm64.deb,
then substitute its actual filename:
  sudo apt install ./tune-server_<version>_<arch>.deb
The package creates the tune user, installs under /opt/tune and starts
tune-server.service. Configuration: /etc/default/tune-server.
After editing the configuration:
  sudo systemctl restart tune-server
Diagnostics:
  systemctl status tune-server
  journalctl -u tune-server -n 100 --no-pager

If you previously used the Python server or a custom service, inspect
the existing unit BEFORE installing:
  systemctl cat tune-server
A unit under /etc/systemd/system or an override may take precedence over
the packaged unit. If ExecStart points to the old installation, have that
unit adapted and back up its configuration and data before changing
installations. The .deb does not import Python server data.

On other distributions, apt and the .deb do not apply: use the manual
start above. This archive does not provide a generic systemd unit.
Request instructions for your distribution, version and architecture:
https://mozaiklabs.fr/forum
