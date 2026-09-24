#!/usr/bin/env bash
# Generates a fresh CA + server leaf pair for mock_core.py, both EC P-256
# (embewi-agent-esp's MbedTLS build excludes RSA on purpose -- see this
# repo's root Cargo.toml -- so an RSA cert here would just fail to verify
# once pushed to the device with `scripts/test-api.sh push-ca`).
#
# Usage:
#   tooling/mock-core/generate-certs.sh [ip-or-host]   # default: 127.0.0.1
#
# Writes server.pem/server.key (what mock_core.py loads by default) and
# ca.pem (what to push to the device: `scripts/test-api.sh <url> <token>
# push-ca tooling/mock-core/ca.pem`) next to this script. None of the three
# are committed (*.pem/*.key are gitignored) -- regenerate them here any
# time, don't go hunting for a copy that might be stale or, worse, a leaf
# cert mistaken for a CA (see the incident this script exists to prevent
# below).
#
# Past incident this fixes: a leaf cert once got committed-in-spirit (kept
# around in a now-removed tmp/ copy) whose `issuer` didn't match its own
# `subject` -- i.e. it was signed by a CA that was never saved anywhere.
# Pushing that leaf as if it were the CA produced a self-referential trust
# anchor: `MbedtlsError(-9984 / 0x2700)` (X509_CERT_VERIFY_FAILED) on every
# connection attempt from the device. This script always generates a
# matching CA+leaf pair together, so that specific mistake can't recur.
set -euo pipefail
cd "$(dirname "$0")"

HOST="${1:-127.0.0.1}"

openssl ecparam -name prime256v1 -genkey -noout -out ca-key.pem
openssl req -x509 -new -key ca-key.pem -sha256 -days 3650 -out ca.pem \
    -subj "/CN=embewi-core-mock-ca" \
    -addext "basicConstraints=critical,CA:true" \
    -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl ecparam -name prime256v1 -genkey -noout -out server.key
openssl req -new -key server.key -out server.csr -subj "/CN=$HOST"
SAN="DNS:$HOST"
[[ "$HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] && SAN="IP:$HOST"
cat > leaf.ext <<EOF
subjectAltName=$SAN
basicConstraints=CA:false
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
    -out server.pem -days 3650 -sha256 -extfile leaf.ext

rm -f server.csr leaf.ext ca.srl

echo "== CA (push with: scripts/test-api.sh <url> <token> push-ca tooling/mock-core/ca.pem) =="
openssl x509 -in ca.pem -noout -subject -issuer -ext basicConstraints
echo "== server leaf (used by mock_core.py) =="
openssl x509 -in server.pem -noout -subject -issuer -ext subjectAltName
echo "== chain verify =="
openssl verify -CAfile ca.pem server.pem
