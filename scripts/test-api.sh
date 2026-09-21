#!/usr/bin/env bash
# Rejoue contre un device réel les campagnes de test de l'API v1alpha1
# (contrat §4/§4a/§6) faites à la main pendant le dev -- pour éviter les
# régressions au lieu de retaper les mêmes curl à chaque fois.
#
# Usage :
#   scripts/test-api.sh <url> <token> [safe]        # défaut : tests non-destructifs, rejouables à volonté
#   scripts/test-api.sh <url> <token> reboot         # POST /reboot puis attend que le device revienne
#   scripts/test-api.sh <url> <token> rotate-token   # POST /token -- change l'auth, affiche le nouveau token
#   scripts/test-api.sh <url> <token> ota-activate   # active le slot déjà stagé par `safe` -- coupe le device en l'air, voir l'avertissement dans la fonction
#   scripts/test-api.sh <url> <token> push-cert <cert.pem> <key.pem>  # POST /tls/cert -- bascule le serveur admin en HTTPS:443
#   scripts/test-api.sh <url> <token> push-ca <ca.pem>                 # POST /tls/ca -- CA à vérifier pour heartbeat/logs sortants
#
# `safe` ne laisse aucun effet de bord dangereux : il stage un faux binaire
# de test sur le slot inactif (visible dans `GET /info`'s `staged` jusqu'au
# prochain vrai cycle OTA), mais ne touche jamais au boot ni à l'auth. Les
# autres sous-commandes sont volontairement séparées, jamais groupées dans
# un mode "tout lancer" : chacune a un effet de bord réel sur le device
# (`push-cert` en particulier fait basculer le port admin de 80 à 443).
set -euo pipefail

URL="${1:?Usage: $0 <url> <token> [safe|reboot|rotate-token|ota-activate|push-cert|push-ca]}"
TOKEN="${2:?Usage: $0 <url> <token> [safe|reboot|rotate-token|ota-activate|push-cert|push-ca]}"
MODE="${3:-safe}"
URL="${URL%/}"

PASS=0
FAIL=0

# --- petits utilitaires -------------------------------------------------

# jget <json> <clé> : extrait une valeur top-level (ou "a.b" en profondeur)
# d'une réponse JSON. jq n'est pas garanti présent sur toutes les machines
# de dev ESP -- python3, lui, l'est presque toujours (esptool/espflash
# eux-mêmes en dépendent).
jget() {
    python3 -c '
import json, sys
try:
    data = json.loads(sys.argv[1])
    for key in sys.argv[2].split("."):
        data = data[key]
    print(data if not isinstance(data, (dict, list)) else json.dumps(data))
except Exception:
    print("")
' "$1" "$2"
}

check() {
    local desc="$1" got="$2" want="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  OK   $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL $desc (attendu=$want obtenu=$got)"
        FAIL=$((FAIL + 1))
    fi
}

check_ne() {
    local desc="$1" got="$2" not_want="$3"
    if [[ "$got" != "$not_want" ]]; then
        echo "  OK   $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL $desc (n'aurait pas dû valoir '$not_want')"
        FAIL=$((FAIL + 1))
    fi
}

# -k : les certs de test sont auto-signés (safe suite tourne aussi bien en
# clair qu'en HTTPS avec ce flag -- curl l'ignore silencieusement en HTTP).
auth_get() { curl -sk -m 10 -H "Authorization: Bearer $TOKEN" "$URL$1"; }
auth_post() { curl -sk -m 10 -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$2" "$URL$1"; }
http_code() { curl -sk -o /dev/null -w "%{http_code}" -m 10 "$@"; }

# --- campagne "safe" (contrat §4, non-destructif) ------------------------

run_safe() {
    echo "== GET /v1alpha1/info =="
    local info; info=$(auth_get /v1alpha1/info)
    check "node_id non vide" "$([[ -n "$(jget "$info" node_id)" ]] && echo yes)" "yes"
    check "api_versions contient v1alpha1" "$(jget "$info" api_versions)" '["v1alpha1"]'
    check "partition_layout" "$(jget "$info" partition_layout)" "embewi-ab-v1"
    check "state == running" "$(jget "$info" state)" "running"
    check_ne "active_slot non vide (MMU, pas otadata)" "$(jget "$info" active_slot)" ""
    check_ne "firmware.name non vide" "$(jget "$info" firmware.name)" ""
    check_ne "ram_size non vide" "$(jget "$info" ram_size)" ""

    echo "== GET /v1alpha1/health =="
    local health; health=$(auth_get /v1alpha1/health)
    check "status == ok" "$(jget "$health" status)" "ok"
    check "checks.storage == ok" "$(jget "$health" checks.storage)" "ok"

    echo "== GET /v1alpha1/config =="
    local config; config=$(auth_get /v1alpha1/config)
    check_ne "generation présent" "$(jget "$config" generation)" ""

    echo "== Auth =="
    check "sans token -> 401" "$(http_code "$URL/v1alpha1/info")" "401"
    check "mauvais token -> 401" "$(http_code -H 'Authorization: Bearer wrong' "$URL/v1alpha1/info")" "401"

    echo "== POST /v1alpha1/ota/prepare (compat) =="
    local chip; chip=$(jget "$info" chip)
    local bad_chip; bad_chip=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"bidon\",\"partition_layout\":\"embewi-ab-v1\"}")
    check "chip_mismatch" "$(jget "$bad_chip" reason)" "chip_mismatch"
    local bad_layout; bad_layout=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"$chip\",\"partition_layout\":\"bidon\"}")
    check "layout_mismatch" "$(jget "$bad_layout" reason)" "layout_mismatch"
    local ok_prepare; ok_prepare=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test-api-sh\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"$chip\",\"partition_layout\":\"embewi-ab-v1\"}")
    check "accepted" "$(jget "$ok_prepare" accepted)" "True"
    check_ne "target_slot renvoyé" "$(jget "$ok_prepare" target_slot)" ""

    echo "== PUT /v1alpha1/ota/write (écriture monolithique) =="
    local tmp; tmp=$(mktemp)
    printf 'embewi-test-api-sh-payload' > "$tmp"
    local expected; expected="sha256:$(sha256sum "$tmp" | cut -d' ' -f1)"
    local write; write=$(curl -s -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh" -H "X-Embewi-Digest: $expected" \
        --data-binary @"$tmp" "$URL/v1alpha1/ota/write")
    check "status == written" "$(jget "$write" status)" "written"
    check "digest calculé == attendu" "$(jget "$write" digest)" "$expected"

    echo "== PUT /v1alpha1/ota/write (digest volontairement faux) =="
    local bad; bad=$(curl -s -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-bad" -H "X-Embewi-Digest: sha256:0000000000000000000000000000000000000000000000000000000000000000" \
        --data-binary @"$tmp" "$URL/v1alpha1/ota/write")
    check "digest_mismatch" "$(jget "$bad" status)" "digest_mismatch"
    local after; after=$(auth_get /v1alpha1/info)
    check "idempotence : staged garde l'écriture précédente, pas celle en échec" \
        "$(jget "$after" staged.deployment_id)" "test-api-sh"

    echo "== PUT /v1alpha1/ota/write (reprise Content-Range) =="
    printf 'embewi-content-range-resume-test-payload-0123456789' > "$tmp"
    local total; total=$(wc -c < "$tmp")
    local half=$((total / 2))
    head -c "$half" "$tmp" > "$tmp.part1"
    tail -c +"$((half + 1))" "$tmp" > "$tmp.part2"
    local expected_cr; expected_cr="sha256:$(sha256sum "$tmp" | cut -d' ' -f1)"
    local part1; part1=$(curl -s -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-cr" -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes 0-$((half - 1))/$total" \
        --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "1er chunk -> partial" "$(jget "$part1" status)" "partial"
    check "written == taille du 1er chunk" "$(jget "$part1" written)" "$half"
    local part2; part2=$(curl -s -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-cr" -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes $half-$((total - 1))/$total" \
        --data-binary @"$tmp.part2" "$URL/v1alpha1/ota/write")
    check "2e chunk -> written" "$(jget "$part2" status)" "written"
    check "digest final correct après reprise" "$(jget "$part2" digest)" "$expected_cr"

    echo "== PUT /v1alpha1/ota/write (resync sur mauvais offset) =="
    local resync; resync=$(curl -s -m 15 -o /tmp/resync_body.json -w "%{http_code}" -X PUT \
        -H "Authorization: Bearer $TOKEN" -H "X-Embewi-Deployment-Id: test-api-sh-resync" \
        -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes 999-$((999 + half - 1))/2000" --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "offset erroné -> 416" "$resync" "416"
    check "erreur == range_mismatch" "$(jget "$(cat /tmp/resync_body.json)" error)" "range_mismatch"

    echo "== PUT /v1alpha1/ota/write (Content-Range malformé) =="
    local malformed; malformed=$(curl -s -o /tmp/malformed_body.json -w "%{http_code}" -m 10 -X PUT \
        -H "Authorization: Bearer $TOKEN" -H "X-Embewi-Deployment-Id: test-api-sh-malformed" \
        -H "X-Embewi-Digest: $expected_cr" -H "Content-Range: n'importe-quoi" \
        --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "header invalide -> 400" "$malformed" "400"
    check "erreur == bad_content_range" "$(jget "$(cat /tmp/malformed_body.json)" error)" "bad_content_range"

    # Protocole durci : chaque refus se fait avant de toucher la session.
    put_write() { # put_write <fichier> <dep-id> <digest> [Content-Range] -> "<code> <corps>"
        local file="$1" dep="$2" dig="$3" range="${4:-}"
        local args=(-s -m 15 -o /tmp/put_body.json -w "%{http_code}" -X PUT -H "Authorization: Bearer $TOKEN")
        [[ -n "$dep" ]] && args+=(-H "X-Embewi-Deployment-Id: $dep")
        [[ -n "$dig" ]] && args+=(-H "X-Embewi-Digest: $dig")
        [[ -n "$range" ]] && args+=(-H "Content-Range: $range")
        local code; code=$(curl "${args[@]}" --data-binary @"$file" "$URL/v1alpha1/ota/write")
        echo "$code $(jget "$(cat /tmp/put_body.json)" error)"
    }
    echo "== PUT /v1alpha1/ota/write (validation des en-têtes) =="
    check "sans deployment_id -> 400" "$(put_write "$tmp.part1" "" "$expected_cr")" "400 missing_deployment_id"
    check "sans digest -> 400" "$(put_write "$tmp.part1" dep "")" "400 bad_digest"
    check "digest mal formé -> 400" "$(put_write "$tmp.part1" dep "sha256:abc")" "400 bad_digest"
    check "Content-Length != plage -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-1/$total")" "400 content_length_mismatch"
    check "plage inversée -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 10-5/$total")" "400 bad_content_range"
    check "fin >= total -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-$((half - 1))/$((half - 1))")" "400 bad_content_range"
    check "total = 0 -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-$((half - 1))/0")" "400 bad_content_range"

    echo "== PUT /v1alpha1/ota/write (session figée : deployment_id / digest / total) =="
    check "1er chunk -> 200" "$(put_write "$tmp.part1" test-api-sh-sm "$expected_cr" "bytes 0-$((half - 1))/$total" | cut -d' ' -f1)" "200"
    check "autre deployment_id -> 409" \
        "$(put_write "$tmp.part2" other-dep "$expected_cr" "bytes $half-$((total - 1))/$total")" "409 session_mismatch"
    check "autre digest -> 409" \
        "$(put_write "$tmp.part2" test-api-sh-sm "sha256:$(printf '1%.0s' $(seq 64))" "bytes $half-$((total - 1))/$total")" "409 session_mismatch"
    check "autre total -> 409" \
        "$(put_write "$tmp.part2" test-api-sh-sm "$expected_cr" "bytes $half-$((total - 1))/$((total + 1))")" "409 session_mismatch"
    check "chunk final avec les bons paramètres -> 200" \
        "$(put_write "$tmp.part2" test-api-sh-sm "$expected_cr" "bytes $half-$((total - 1))/$total" | cut -d' ' -f1)" "200"

    echo "== POST /v1alpha1/ota/activate (deployment_id différent) =="
    local act; act=$(curl -s -m 10 -o /tmp/act_body.json -w "%{http_code}" -X POST -H "Authorization: Bearer $TOKEN" \
        -H "Content-Type: application/json" -d '{"deployment_id":"pas-le-bon"}' "$URL/v1alpha1/ota/activate")
    check "activate d'un autre deployment -> 409 deployment_mismatch" \
        "$act $(jget "$(cat /tmp/act_body.json)" error)" "409 deployment_mismatch"
    check "staged.deployment_id inchangé" "$(jget "$(auth_get /v1alpha1/info)" staged.deployment_id)" "test-api-sh-sm"

    rm -f /tmp/put_body.json /tmp/act_body.json "$tmp" "$tmp.part1" "$tmp.part2" /tmp/resync_body.json /tmp/malformed_body.json

    echo
    echo "-- $PASS OK / $FAIL FAIL --"
    echo "Note : GET /info->staged pointe maintenant vers le faux binaire de ce" \
         "test (deployment_id=test-api-sh-sm) jusqu'au prochain vrai cycle OTA" \
         "(prepare+write+activate) -- ce n'est pas dangereux, juste cosmétique."
    [[ $FAIL -eq 0 ]]
}

# --- sous-commandes destructives, une par une -----------------------------

run_reboot() {
    echo "POST /v1alpha1/reboot -- le device va couper la connexion."
    auth_post /v1alpha1/reboot '' ; echo
    echo "Attente du retour (jusqu'à 30s)..."
    for _ in $(seq 1 10); do
        sleep 3
        if [[ "$(http_code -H "Authorization: Bearer $TOKEN" "$URL/v1alpha1/info")" == "200" ]]; then
            echo "OK, le device répond à nouveau."
            return 0
        fi
    done
    echo "FAIL : pas de réponse après 30s."
    return 1
}

run_rotate_token() {
    echo "POST /v1alpha1/token -- invalide $TOKEN pour le reste de cette session."
    local new_token; new_token=$(python3 -c "import secrets; print(secrets.token_hex(16))")
    local resp; resp=$(auth_post /v1alpha1/token "{\"token\":\"$new_token\"}")
    check "status == rotated" "$(jget "$resp" status)" "rotated"
    echo "Nouveau token (à conserver) : $new_token"
}

run_ota_activate() {
    cat <<'EOF'
ATTENTION : /ota/activate fait rebooter le device sur le slot déjà stagé
(voir `safe`'s staged.deployment_id via GET /info). Si ce slot ne contient
pas un vrai firmware bootable, le bootloader doit récupérer tout seul sur
l'autre slot -- déjà vérifié une fois à la main, mais chaque nouveau test
répète ce risque. Ctrl-C dans les 5s pour annuler.
EOF
    sleep 5
    local dep_id; dep_id=$(jget "$(auth_get /v1alpha1/info)" staged.deployment_id)
    echo "Activation de deployment_id=$dep_id..."
    auth_post /v1alpha1/ota/activate "{\"deployment_id\":\"$dep_id\",\"reboot\":true}"; echo
    run_reboot
}

run_push_cert() {
    local cert_file="${1:?Usage: $0 <url> <token> push-cert <cert.pem> <key.pem>}"
    local key_file="${2:?Usage: $0 <url> <token> push-cert <cert.pem> <key.pem>}"
    local payload; payload=$(python3 -c "
import json, sys
print(json.dumps({'cert_pem': open(sys.argv[1]).read(), 'key_pem': open(sys.argv[2]).read()}))
" "$cert_file" "$key_file")
    local resp; resp=$(auth_post /v1alpha1/tls/cert "$payload")
    check "status == saved" "$(jget "$resp" status)" "saved"
    echo "Le serveur admin bascule en HTTPS:443 dès la prochaine connexion (port 80 s'arrête)."
}

run_push_ca() {
    local ca_file="${1:?Usage: $0 <url> <token> push-ca <ca.pem>}"
    local payload; payload=$(python3 -c "
import json, sys
print(json.dumps({'ca_pem': open(sys.argv[1]).read()}))
" "$ca_file")
    local resp; resp=$(auth_post /v1alpha1/tls/ca "$payload")
    check "status == saved" "$(jget "$resp" status)" "saved"
    echo "CA enregistré : heartbeat/logs sortants (contrat §5) vérifieront désormais les certs contre ce CA."
}

case "$MODE" in
    safe) run_safe ;;
    reboot) run_reboot ;;
    rotate-token) run_rotate_token ;;
    ota-activate) run_ota_activate ;;
    push-cert) run_push_cert "${4:-}" "${5:-}" ;;
    push-ca) run_push_ca "${4:-}" ;;
    *) echo "Mode inconnu: $MODE (safe|reboot|rotate-token|ota-activate|push-cert|push-ca)" >&2; exit 1 ;;
esac
