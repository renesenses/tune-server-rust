#!/bin/bash
# Sonde licence Tune — diagnostic à envoyer au support.
# Usage (sur la machine qui fait tourner Tune Server) :
#   curl -sL https://raw.githubusercontent.com/renesenses/tune-server-rust/main/scripts/tune-license-probe.sh | bash
# Ne révèle pas la clé complète (4 derniers caractères seulement).
set -u
PORT="${TUNE_PORT:-8888}"
BASE="http://127.0.0.1:${PORT}/api/v1"

j() { curl -s -m 6 "$BASE$1" 2>/dev/null; }

echo "=== SONDE LICENCE TUNE ==="
date -u +"Horodatage:            %Y-%m-%d %H:%M UTC"
VERSION=$(j /system/version)
if [ -z "$VERSION" ]; then
    echo "Serveur:               INJOIGNABLE sur ${BASE} (service arrêté ?)"
    echo "=== FIN ==="
    exit 1
fi

SSO=$(j /cloud/sso/status)
LIC=$(j /cloud/license/status)
CFG=$(j /system/config)

python3 - "$VERSION" "$SSO" "$LIC" "$CFG" <<'PY'
import json, sys

def load(s):
    try:
        return json.loads(s)
    except Exception:
        return {}

ver, sso, lic, cfg = (load(x) for x in sys.argv[1:5])

def say(k, v):
    print(f"{k:<23}{v}")

say("Serveur:", f"v{ver.get('version', '?')} ({ver.get('engine', '?')})")
say("Mode appliance:", cfg.get("appliance", "non exposé (serveur < 0.9.10)"))
connected = sso.get("connected", sso.get("authenticated", False))
say("Compte SSO connecté:", connected)
user = sso.get("user") or {}
say("Email du compte:", user.get("email") or "(aucun)")
say("Compte premium:", user.get("premium", "?"))
key = lic.get("license_key") or ""
say("Clé licence:", ("****" + key[-4:]) if key else "(aucune clé enregistrée)")
say("Tier:", lic.get("tier", "?"))
say("Expire:", lic.get("expires_at") or "-")
say("Dernière validation:", lic.get("last_validated") or "jamais")
say("Fingerprint machine:", lic.get("hardware_fingerprint") or "?")
feats = lic.get("features") or {}
on = sum(1 for f in feats.values() if isinstance(f, dict) and f.get("enabled"))
say("Features premium:", f"{on}/{len(feats)} actives")
say("premium_tier (web):", cfg.get("premium_tier", "?"))
say("zone_limit:", cfg.get("zone_limit", "?"))
PY

CLOUD=$(curl -s -o /dev/null -w "%{http_code}" -m 8 https://mozaiklabs.fr 2>/dev/null)
printf "%-23s%s\n" "Cloud mozaiklabs.fr:" "HTTP ${CLOUD:-injoignable}"
echo "=== FIN — copie ou photographie ce bloc et envoie-le au support ==="
