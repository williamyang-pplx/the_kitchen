#!/usr/bin/env bash
# One-time: create a self-signed code-signing identity, "Kitchen Local Signing",
# in the login keychain and trust it for code signing (macOS asks for your
# password once). install.sh signs The Kitchen.app with it, so the app keeps
# the same identity across rebuilds and macOS remembers privacy grants
# (Documents folder etc.) instead of asking again after every build.
set -euo pipefail
name="Kitchen Local Signing"
keychain="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "\"$name\""; then
  echo "\"$name\" already exists and is trusted"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/cfg" <<CFG
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=$name
[ext]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
CFG
# /usr/bin/openssl (LibreSSL) writes a PKCS#12 that `security import` accepts.
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -days 3650 -config "$tmp/cfg" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" -passout pass:kitchen -name "$name"
security import "$tmp/id.p12" -k "$keychain" -P kitchen -T /usr/bin/codesign >/dev/null
echo "trusting the certificate for code signing (macOS will ask for your password)…"
security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$tmp/cert.pem"
security find-identity -v -p codesigning | grep "\"$name\""
echo "done — now run scripts/install.sh"
