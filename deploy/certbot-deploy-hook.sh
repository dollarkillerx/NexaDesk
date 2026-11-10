#!/bin/sh
# Certbot deploy hook for nexa-signal (docs/DEPLOY.md).
#
#   install -m 0755 deploy/certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/nexa-signal.sh
#
# Let's Encrypt keeps the private key readable by root only; the server runs as
# uid 10001. After each issue or renewal this copies the pair to
# /etc/nexa-signal/tls/ for that user, then restarts the server, which loads
# certificates only at start. Clients reconnect and register again by themselves.
#
# NEXA_SIGNAL_DOMAIN picks the certificate when certbot manages more than one.
set -eu

domain="${NEXA_SIGNAL_DOMAIN:-}"
if [ -z "$domain" ]; then
    # Certbot sets RENEWED_LINEAGE to /etc/letsencrypt/live/<name>.
    lineage="${RENEWED_LINEAGE:?run from certbot, or set NEXA_SIGNAL_DOMAIN}"
else
    lineage="/etc/letsencrypt/live/$domain"
    if [ -n "${RENEWED_LINEAGE:-}" ] && [ "$RENEWED_LINEAGE" != "$lineage" ]; then
        exit 0 # another certificate was renewed
    fi
fi

dest=/etc/nexa-signal/tls
install -d -m 0750 -o 10001 -g 10001 "$dest"
install -m 0444 -o 10001 -g 10001 "$lineage/fullchain.pem" "$dest/fullchain.pem.new"
install -m 0400 -o 10001 -g 10001 "$lineage/privkey.pem" "$dest/privkey.pem.new"
mv -f "$dest/fullchain.pem.new" "$dest/fullchain.pem"
mv -f "$dest/privkey.pem.new" "$dest/privkey.pem"

if command -v systemctl >/dev/null 2>&1 && systemctl is-enabled --quiet nexa-signal 2>/dev/null; then
    systemctl restart nexa-signal
elif command -v docker >/dev/null 2>&1 && docker ps --format '{{.Names}}' | grep -q nexa-signal; then
    docker ps --format '{{.Names}}' | grep nexa-signal | xargs docker restart >/dev/null
fi
