#!/usr/bin/awk -f
#
# Combien d'items le panneau « Quoi de neuf » tirera-t-il de cette note ?
#
# Lit un corps de release sur l'entree standard et rend, sur la sortie
# standard, quatre lignes `cle<TAB>valeur` :
#
#   features<TAB>N        items de la rubrique Nouveautes
#   improvements<TAB>N    items de la rubrique Ameliorations
#   fixes<TAB>N           items de la rubrique Corrections
#   langues<TAB>fr,en,de  blocs de langue trouves, dans l'ordre
#
# Ce n'est pas une heuristique de lisibilite : c'est le MEME decoupage que
# `parse_release_body` dans `tune-server/src/routes/system/update.rs`, celui
# qui alimente reellement `GET /system/changelog`. Une note qui rend 0 ici rend
# « Release 0.9.x » a l'ecran — c'est ce qu'ont vu les vingt entrees du panneau
# le 23/09/2026 (#4190).
#
# Les regles reproduites, dans l'ordre ou le Rust les applique :
#   - un titre (`# …`, ou `**Titre**` seul sur sa ligne) CHOISIT la rubrique ;
#   - seules les PUCES (`- `, `* `, `• `) deviennent des items ; la prose et
#     les titres n'en sont jamais ;
#   - une rubrique non reconnue (« Telechargements ») avale ses puces ;
#   - hors de toute rubrique, les mots-cles de la puce decident, Nouveautes
#     par defaut ;
#   - le balisage en ligne (gras, `code`, liens) est retire, et une puce vide
#     apres retrait ne compte pas.
#
# Le decompte porte sur le BLOC FRANCAIS, pas sur le corps entier : les puces
# d'un bloc `<!-- lang:en -->` ne masquent pas un francais en prose.
#
# ⚠️ Duplication assumee, et gardee. Les trois tables de mots-cles ci-dessous
# sont la copie de `TITRES_CORRECTIONS`, `TITRES_AMELIORATIONS` et
# `TITRES_NOUVEAUTES` du Rust. `scripts/test-forme-des-notes.sh` les extrait de
# `update.rs` et refuse la moindre divergence : une table qui derive rendrait
# ce controle vert sur une note que le serveur, lui, ne saurait pas lire.
#
# Limite connue : `tolower()` d'awk ne replie que l'ASCII. Un titre ecrit tout
# en capitales ACCENTUEES (« AMELIORATIONS » avec l'accent) ne serait pas
# reconnu ici alors que le `to_lowercase()` de Rust le reconnaitrait. Nos
# titres sont en casse normale ; le sens de l'ecart est le bon (ce controle
# est plus severe que le serveur, jamais plus laxiste).

function trim(s) {
  sub(/^[ \t\r]+/, "", s)
  sub(/[ \t\r]+$/, "", s)
  return s
}

function section_du_titre(t,   l, i) {
  l = tolower(t)
  for (i = 1; i <= n_corr; i++) if (index(l, corr[i])) return "fixes"
  for (i = 1; i <= n_amel; i++) if (index(l, amel[i])) return "improvements"
  for (i = 1; i <= n_nouv; i++) if (index(l, nouv[i])) return "features"
  return "other"
}

# Retire le balisage Markdown EN LIGNE, caractere par caractere, comme
# `strip_inline_markdown` : `**`/`__` par paires, `*` et `` ` `` isoles,
# `[texte](url)` reduit a son texte. Un `_` isole est conserve (identifiants).
function sans_balises(s,   out, i, n, c, j) {
  out = ""
  i = 1
  n = length(s)
  while (i <= n) {
    c = substr(s, i, 1)
    i++
    if ((c == "*" || c == "_") && i <= n && substr(s, i, 1) == c) { i++; continue }
    if (c == "*" || c == "`") continue
    if (c == "[") {
      j = i
      while (j <= n && substr(s, j, 1) != "]") j++
      out = out substr(s, i, j - i)
      i = (j <= n) ? j + 1 : j
      if (i <= n && substr(s, i, 1) == "(") {
        i++
        while (i <= n && substr(s, i, 1) != ")") i++
        if (i <= n) i++
      }
      continue
    }
    out = out c
  }
  return trim(out)
}

# `<!-- lang:xx -->` seul sur sa ligne — meme tolerance que `marqueur_de_langue`
# (espaces libres, casse libre, region ignoree). Rend "" si ce n'en est pas un.
function marqueur_de_langue(line,   t, inner, tag) {
  t = trim(line)
  if (length(t) < 7) return ""
  if (substr(t, 1, 4) != "<!--") return ""
  if (substr(t, length(t) - 2) != "-->") return ""
  inner = trim(substr(t, 5, length(t) - 7))
  if (substr(inner, 1, 5) != "lang:") return ""
  tag = substr(inner, 6)
  sub(/-.*$/, "", tag)
  tag = tolower(trim(tag))
  if (tag !~ /^[a-z]+$/) return ""
  return tag
}

BEGIN {
  # Copies conformes des tables du Rust — separateur `|`, car plusieurs
  # entrees contiennent une espace (« neue funktion », « nya funktion »).
  CORR = "correct|fix|bug|korrektur|fehler|behoben|correc|correz|corect|remed|rätt|ratt|修复|修正|수정"
  AMEL = "amélio|ameli|improv|verbesser|mejor|miglior|îmbunăt|imbunat|förbättr|forbattr|改进|优化|改善|개선"
  NOUV = "nouveaut|feature|ajout|neuheit|neuerung|neue funktion|noved|nuevas func|novit|nuove|noutăț|noutat|nyhet|nya funktion|新功能|新增|新機能|새로운 기능|신규"
  n_corr = split(CORR, corr, "|")
  n_amel = split(AMEL, amel, "|")
  n_nouv = split(NOUV, nouv, "|")

  # Un corps sans marqueur est entierement francais : le bloc courant s'ouvre
  # en `fr`, exactement comme `blocs_par_langue`.
  n_blocs = 1
  bloc_lang[1] = "fr"
  bloc_txt[1] = ""
  ordre = "fr"
}

{
  tag = marqueur_de_langue($0)
  if (tag != "") {
    n_blocs++
    bloc_lang[n_blocs] = tag
    bloc_txt[n_blocs] = ""
    if (index("," ordre ",", "," tag ",") == 0) ordre = ordre "," tag
    next
  }
  bloc_txt[n_blocs] = bloc_txt[n_blocs] $0 "\n"
}

END {
  # Le bloc a examiner : le PREMIER bloc francais non vide, comme
  # `notes_dans_la_langue`. A defaut (corps sans francais utile), le corps
  # entier, ce que le Rust fait aussi.
  choisi = ""
  for (k = 1; k <= n_blocs; k++) {
    if (bloc_lang[k] == "fr" && trim(bloc_txt[k]) != "") { choisi = bloc_txt[k]; break }
  }
  if (choisi == "") for (k = 1; k <= n_blocs; k++) choisi = choisi bloc_txt[k]

  features = 0; improvements = 0; fixes = 0
  courante = ""   # "" = hors de toute rubrique
  nl = split(choisi, lignes, "\n")
  for (i = 1; i <= nl; i++) {
    line = trim(lignes[i])
    if (line == "") continue

    if (substr(line, 1, 1) == "#") {
      titre = substr(line, 2)
      sub(/^#+/, "", titre)
      courante = section_du_titre(titre)
      continue
    }

    # Un titre en gras seul sur sa ligne tient lieu de titre de section.
    if (length(line) >= 4 && substr(line, 1, 2) == "**" && substr(line, length(line) - 1) == "**") {
      inner = substr(line, 3, length(line) - 4)
      if (index(inner, "**") == 0) { courante = section_du_titre(inner); continue }
    }

    p2 = substr(line, 1, 2)
    if (p2 == "- " || p2 == "* ") item = substr(line, 3)
    else if (substr(line, 1, length("• ")) == "• ") item = substr(line, length("• ") + 1)
    else continue   # prose, tableau, separateur, image : jamais un item.

    item = sans_balises(item)
    if (item == "") continue

    dest = courante
    if (dest == "other") continue
    if (dest == "") {
      dest = section_du_titre(item)
      if (dest == "other") dest = "features"
    }
    if (dest == "features") features++
    else if (dest == "improvements") improvements++
    else if (dest == "fixes") fixes++
  }

  printf "features\t%d\n", features
  printf "improvements\t%d\n", improvements
  printf "fixes\t%d\n", fixes
  printf "langues\t%s\n", ordre
}
