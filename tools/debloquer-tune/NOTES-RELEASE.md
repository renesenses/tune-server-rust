**Outil à part, ce n'est pas une version de Tune.** / **A separate tool, not a Tune release.**

## Français

Votre serveur **Tune OS en rc12** refuse sa mise à jour avec le message « enable authentication and sign in as an administrator » ? Cet outil règle le problème en quelques clics, sans aucune commande.

1. Sur un ordinateur **du même réseau** que votre Tune, téléchargez :
   - Windows : `Debloquer-Tune-windows-x86_64.exe`
   - Mac : `Debloquer-Tune-macos.zip` (double-cliquez pour le décompresser)
2. Double-cliquez sur le fichier. Votre navigateur s'ouvre sur la page « Débloquer Tune ».
   - **Windows** : si un écran bleu « Windows a protégé votre ordinateur » apparaît, cliquez sur **Informations complémentaires › Exécuter quand même**. L'outil n'est pas encore signé, d'où cet avertissement. Si le pare-feu demande une autorisation, acceptez pour les **réseaux privés**.
   - **Mac** : si macOS demande l'accès au réseau local, cliquez sur **Autoriser**.
3. Choisissez votre Tune dans la liste, ou tapez son adresse (par exemple `192.168.1.18`).
4. Choisissez un mot de passe de **8 caractères au moins**, deux fois. Notez-le bien : il vous servira ensuite à vous connecter à Tune avec l'identifiant **`default`**.
5. Cliquez sur **Activer l'authentification et mettre à jour**, puis attendez : l'outil suit le téléchargement, le redémarrage et l'arrivée de la nouvelle version.
6. C'est fait. Connectez-vous à Tune avec l'identifiant `default` et votre mot de passe.

**À savoir**
- L'app iPhone ne se connecte pas tant que l'authentification est active.
- Si votre Tune a déjà la rc13 ou une version plus récente, l'outil ne fait rien et vous renvoie au fil épinglé du forum.
- L'outil ne parle qu'à votre Tune, sur votre réseau local. Il n'enregistre aucun mot de passe et n'envoie rien ailleurs. Il s'arrête tout seul quand vous fermez la page.

## English

Is your **Tune OS server on rc12** refusing to update with "enable authentication and sign in as an administrator"? This tool fixes it in a few clicks, with no commands.

1. On a computer **on the same network** as your Tune, download:
   - Windows: `Debloquer-Tune-windows-x86_64.exe`
   - Mac: `Debloquer-Tune-macos.zip` (double-click to unzip it)
2. Double-click the file. Your browser opens the "Unblock Tune" page.
   - **Windows**: if a blue "Windows protected your PC" screen appears, click **More info › Run anyway**. The tool is not signed yet, which is why you see this warning. If the firewall asks, allow it on **private networks**.
   - **Mac**: if macOS asks for local network access, click **Allow**.
3. Pick your Tune from the list, or type its address (for example `192.168.1.18`).
4. Choose a password of **at least 8 characters** and type it twice. Write it down: you will then sign in to Tune with the username **`default`** and this password.
5. Click **Turn on authentication and update**, then wait: the tool follows the download, the restart and the new version coming up.
6. Done. Sign in to Tune with the username `default` and your password.

**Good to know**
- The iPhone app does not connect while authentication is on.
- If your Tune already runs rc13 or later, the tool does nothing and points you to the pinned forum thread.
- The tool only talks to your Tune, on your local network. It stores no password and sends nothing anywhere else. It stops by itself when you close the page.

`SHA256SUMS-debloquer-tune.txt` : sommes de contrôle / checksums.
