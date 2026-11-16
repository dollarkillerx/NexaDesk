#!/bin/sh
# Creates "NexaDesk Local Signing", a self-signed code-signing certificate in your login
# keychain, once. scripts/package-macos.sh signs with it whenever it is there.
#
#   scripts/make-signing-identity.sh
#
# Why: an ad-hoc signature is the build's content hash, so every rebuild is a new app to
# macOS. Screen Recording, Accessibility and Keychain "Always Allow" are tied to that
# identity, and are asked for again after each rebuild. Signed with one certificate, every
# build is the same app ("identifier app.nexadesk.NexaDesk and certificate leaf = H…"),
# and what you allowed once stays allowed.
#
# The private key stays in your login keychain, usable by codesign only. Anyone who can
# use it could sign a program macOS would treat as NexaDesk, with its permissions: keep
# it on this Mac. For builds others run, use a Developer ID instead (NEXA_SIGN_IDENTITY).
set -eu

NAME="NexaDesk Local Signing"
KEYCHAIN="${NEXA_SIGN_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

if security find-certificate -c "$NAME" "$KEYCHAIN" >/dev/null 2>&1; then
  echo "\"$NAME\" is already in $KEYCHAIN"
  exit 0
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/cert.cnf" <<CNF
[req]
distinguished_name = dn
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -config "$WORK/cert.cnf" -extensions ext 2>/dev/null
# A throwaway passphrase: the PKCS#12 file only carries the key into the keychain.
PASS=$(openssl rand -hex 16)
openssl pkcs12 -export -legacy -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -name "$NAME" -out "$WORK/id.p12" -passout "pass:$PASS"
security import "$WORK/id.p12" -k "$KEYCHAIN" -P "$PASS" -T /usr/bin/codesign >/dev/null
echo "created \"$NAME\" in $KEYCHAIN"
echo "Package again (scripts/package-macos.sh), then give NexaDesk its permissions one last time."
