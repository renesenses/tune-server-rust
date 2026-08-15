; Crochets NSIS du mini-lecteur Tune — voir #1704.
;
; Tauri insère ce fichier dans son modèle d'installateur (`bundle > windows >
; nsis > installerHooks`). Les macros non définies ici sont simplement ignorées.
;
; CONTEXTE. L'interface du mini-lecteur est une page rendue par WebView2, qui
; garde une copie sur disque de ce qu'il a chargé. Aucun `dataDirectory` n'étant
; déclaré dans tauri.conf.json, WebView2 retombe sur son emplacement par défaut,
; documenté par Microsoft comme « le chemin de l'exécutable suivi de .WebView2 » :
; le cache vit donc À L'INTÉRIEUR du dossier d'installation, sous
; `$INSTDIR\tune-widget.exe.WebView2\EBWebView\` (le binaire garde le nom du
; paquet Cargo, aucun `mainBinaryName` n'étant déclaré). Or le désinstalleur de Tauri
; termine par un `RMDir "$INSTDIR"` SANS /r, qui échoue en silence sur un dossier
; non vide : le cache survit à la désinstallation, puis à la réinstallation.
;
; Sandro l'a démontré lui-même le 14 août 2026 : dans un bac à sable Windows
; vierge, le bouton d'agrandissement du mini-lecteur apparaît ; sur sa machine,
; non — même après avoir désinstallé puis réinstallé. Toute correction
; d'interface pouvait ainsi rester invisible, sans que rien ne distingue « le
; correctif n'est pas livré » de « le correctif est masqué par un cache ».

!macro NSIS_HOOK_POSTUNINSTALL
  ; 1. Le cache HTTP, dans TOUS les cas — mise à jour comprise, puisque c'est
  ;    précisément la mise à jour qui doit repartir d'une page fraîche.
  ;
  ;    On ne vide QUE les deux dossiers de cache. Leur voisin `Local Storage`
  ;    contient l'adresse du serveur saisie par l'utilisateur (clé `tune-server`
  ;    posée par app.js) : l'effacer ici renverrait le widget sur l'adresse en
  ;    dur du code, c'est-à-dire le réseau de quelqu'un d'autre.
  RMDir /r "$INSTDIR\${MAINBINARYNAME}.exe.WebView2\EBWebView\Default\Cache"
  RMDir /r "$INSTDIR\${MAINBINARYNAME}.exe.WebView2\EBWebView\Default\Code Cache"

  ; 2. Le reste seulement si l'utilisateur a coché « supprimer les données de
  ;    l'application » sur la page de confirmation, et qu'il ne s'agit pas d'une
  ;    mise à jour. La case existe déjà dans le modèle Tauri, mais elle ne balaie
  ;    que `$APPDATA\<identifiant>` et `$LOCALAPPDATA\<identifiant>` — deux
  ;    dossiers que ce widget n'utilise pas. Ce qu'il écrit vraiment, c'est le
  ;    profil WebView2 ci-dessus et `$APPDATA\tune-widget` (config.json et le
  ;    journal, cf. `dirs::config_dir()` dans main.rs).
  ${If} $UpdateMode <> 1
  ${AndIf} $DeleteAppDataCheckboxState = 1
    SetShellVarContext current
    RMDir /r "$INSTDIR\${MAINBINARYNAME}.exe.WebView2"
    RMDir /r "$APPDATA\tune-widget"
  ${EndIf}

  ; 3. Débarrassé de son cache, le dossier d'installation peut enfin disparaître.
  ;    `RMDir` sans /r reste volontaire : s'il reste autre chose, on préfère le
  ;    laisser que supprimer à l'aveugle un dossier choisi par l'utilisateur.
  RMDir "$INSTDIR"
!macroend
