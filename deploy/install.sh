#!/bin/sh
# One-command installer for nexa-signal on a Linux server (docs/DEPLOY.md §0).
#
#   curl -fsSL https://raw.githubusercontent.com/dollarkillerx/NexaDesk/main/deploy/install.sh | sudo sh
#
# Options (after `sh -s --` when piped):
#   --host <ip|domain>   what clients will type; default: this server's public IP
#   --key <key>          the signaling key (>= 10 chars); default: a random one, kept on re-runs
#   --web [password]     also serve the browser page; the password is optional (none = open)
#   --binary <file>      install this nexa-signal instead of downloading one
#   --port <n>           signaling TCP port, default 443
#   --show               print the address and key of the installed server, and nothing else
#   --uninstall          stop and remove the service (keeps /etc/nexa-signal unless --purge)
#   --purge              with --uninstall: also delete the configuration, key and certificates
#
# Running it again is safe: the key, certificates and settings already there are kept, and the
# program and unit are brought up to date. Needs root, systemd, curl or wget, and openssl.
#
# NEXA_SIGNAL_URL    where to download the program from (default: the latest GitHub release)
# NEXA_ROOT          install under this directory instead of / (for testing; skips systemd,
#                    users and the firewall)
set -eu

REPO="dollarkillerx/NexaDesk"
URL="${NEXA_SIGNAL_URL:-https://github.com/$REPO/releases/latest/download/nexa-signal-linux-x86_64}"
ROOT="${NEXA_ROOT:-}"
CONF="$ROOT/etc/nexa-signal"
BIN="$ROOT/usr/local/bin/nexa-signal"
UNIT="$ROOT/etc/systemd/system/nexa-signal.service"
TOML="$CONF/nexa-signal.toml"
INFO="$CONF/INFO"
TLS="$CONF/tls"

host="" key="" web=0 web_password="" binary="" port="" action=install purge=0

say() { printf '%s\n' "$*"; }
die() { printf 'nexa-signal installer: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --host) [ $# -ge 2 ] || die "--host needs a value"; host="$2"; shift 2 ;;
        --key) [ $# -ge 2 ] || die "--key needs a value"; key="$2"; shift 2 ;;
        --binary) [ $# -ge 2 ] || die "--binary needs a file"; binary="$2"; shift 2 ;;
        --port) [ $# -ge 2 ] || die "--port needs a number"; port="$2"; shift 2 ;;
        --web)
            web=1
            if [ $# -ge 2 ] && [ "${2#--}" = "$2" ]; then web_password="$2"; shift; fi
            shift ;;
        --show) action=show; shift ;;
        --uninstall) action=uninstall; shift ;;
        --purge) purge=1; shift ;;
        -h|--help) say "usage: install.sh [--host <ip|domain>] [--key <key>] [--web [password]] [--binary <file>] [--port <n>] | --show | --uninstall [--purge]"; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
done

if [ -z "$ROOT" ]; then
    [ "$(id -u)" = 0 ] || die "run as root (put sudo before sh)"
    [ "$(uname -s)" = Linux ] || die "this is for Linux servers"
    command -v systemctl >/dev/null 2>&1 || die "systemd is required (or use Docker: docs/DEPLOY.md §4)"
fi

if [ "$action" = show ]; then
    [ -r "$INFO" ] || die "not installed here (no $INFO)"
    cat "$INFO"
    exit 0
fi

if [ "$action" = uninstall ]; then
    if [ -z "$ROOT" ]; then
        systemctl disable --now nexa-signal >/dev/null 2>&1 || true
        rm -f "$UNIT"
        systemctl daemon-reload || true
    fi
    rm -f "$BIN" "${UNIT:-}"
    if [ "$purge" = 1 ]; then
        rm -rf "$CONF"
        [ -z "$ROOT" ] && { userdel nexa-signal >/dev/null 2>&1 || true; }
        say "Removed the service and $CONF."
    else
        say "Removed the service. Kept $CONF (key and certificates); add --purge to delete it too."
    fi
    say "Firewall rules, if any were opened, are left as they are."
    exit 0
fi

# ---- checks --------------------------------------------------------------------------------

# The key may come from the environment instead of the command line, where `ps` and the shell
# history would show it: sudo env NEXA_SIGNAL_KEY=... sh install.sh
[ -n "$key" ] || key="${NEXA_SIGNAL_KEY:-}"
# Whether this run asks for anything that is in the configuration file.
asked=0
if [ -n "$host$key$port" ] || [ "$web" = 1 ]; then asked=1; fi
command -v openssl >/dev/null 2>&1 || die "openssl is needed (apt install openssl / dnf install openssl)"
if [ -z "$binary" ]; then
    if command -v curl >/dev/null 2>&1; then
        fetch() { curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -q -T 15 -t 3 -O "$2" "$1"; }
    else
        die "curl or wget is needed to download the program (or pass --binary)"
    fi
    if [ -z "$ROOT" ]; then
        case "$(uname -m)" in x86_64|amd64) ;; *) die "the download is x86_64 only; build for this machine and pass --binary" ;; esac
    fi
fi
[ -z "$binary" ] || [ -f "$binary" ] || die "$binary is not a file"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

random_key() {
    # 24 characters of [A-Za-z0-9], no look-alikes needed: they are pasted, not read out.
    LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 24
}

# The address of this server as the internet sees it.
guess_ip() {
    for u in https://api.ipify.org https://ifconfig.me/ip https://icanhazip.com; do
        if command -v curl >/dev/null 2>&1; then g=$(curl -fsS --max-time 6 "$u" 2>/dev/null || true)
        elif command -v wget >/dev/null 2>&1; then g=$(wget -qO- -T 6 "$u" 2>/dev/null || true)
        else g=""; fi
        g=$(printf '%s' "$g" | tr -d ' \r\n')
        case "$g" in ''|*[!0-9.]*) ;; *.*.*.*) printf '%s' "$g"; return 0 ;; esac
    done
    return 1
}

# What an earlier run decided stays unless this run says otherwise: the address, the port, the key
# and the browser page. A configuration that points at certificates this script did not make
# (a public CA's, say) is the administrator's, and is left alone.
exists=0 hand=0 rewrite=1
old_key="" old_port="" old_host=""
if [ -r "$TOML" ]; then
    exists=1
    # The first key of the list (during a key change there are two).
    old_key=$(sed -n 's/^access_secrets = \["\([^"]*\)".*$/\1/p' "$TOML" | head -n1)
    old_port=$(sed -n 's/^listen = "[^"]*:\([0-9][0-9]*\)"$/\1/p' "$TOML" | head -n1)
    old_host=$(sed -n 's/^server_names = \["\([^"]*\)".*$/\1/p' "$TOML" | head -n1)
    grep -q "^tls_cert = \"$TLS/fullchain.pem\"" "$TOML" || hand=1
    if [ "$hand" = 1 ] && [ "$asked" = 1 ]; then
        die "$TOML points at certificates this script did not make, so it is managed by hand: change it there (or remove the file to let this script write it)"
    fi
    if [ "$hand" = 1 ] || [ "$asked" = 0 ]; then rewrite=0; fi
    if [ "$hand" = 0 ] && [ -r "$TLS/host" ]; then old_host=$(cat "$TLS/host"); fi
    [ -n "$host" ] || host="$old_host"
    [ -n "$port" ] || port="$old_port"
    [ -n "$key" ] || key="$old_key"
    # The browser page stays as it is, unless asked for again.
    if [ "$web" = 0 ] && grep -q '^\[web\]' "$TOML"; then
        web=2
        web_password=$(awk '/^\[/{p=($0=="[web]")} p' "$TOML" | sed -n 's/^password = "\(.*\)"$/\1/p' | head -n1)
    fi
fi
[ -n "$port" ] || port=443
case "$port" in ''|*[!0-9]*) die "--port must be a number" ;; esac
[ "$port" -ge 1 ] && [ "$port" -le 65535 ] || die "--port must be 1-65535"

if [ "$hand" = 0 ]; then
    # What clients should type: a domain or an IPv4 address.
    if [ -z "$host" ]; then
        host=$(guess_ip) || die "could not find this server's public address; pass --host <ip or domain>"
    fi
    case "$host" in *[!A-Za-z0-9.-]*|'') die "--host must be an IPv4 address or a domain name" ;; esac
    is_ip=0
    case "$host" in *[!0-9.]*) ;; *) is_ip=1 ;; esac
    # The relay and STUN are reached by address.
    if [ "$is_ip" = 1 ]; then
        ip="$host"
    else
        ip=$(getent ahostsv4 "$host" 2>/dev/null | awk 'NR==1{print $1}')
        [ -n "$ip" ] || ip=$(guess_ip) || die "$host does not resolve to an address yet; point the domain at this server first"
    fi
fi
if [ "$rewrite" = 1 ]; then
    [ -n "$key" ] || key=$(random_key)
    [ "${#key}" -ge 10 ] || die "the key must be at least 10 characters"
    case "$key" in *[!A-Za-z0-9._~+=@%:-]*) die "the key may use letters, digits and . _ ~ + = @ % : - only" ;; esac
    if [ "$web" = 1 ] && [ -n "$web_password" ]; then
        case "$web_password" in *[!A-Za-z0-9._~+=@%:-]*) die "the web password may use letters, digits and . _ ~ + = @ % : - only" ;; esac
    fi
fi

# ---- the program ---------------------------------------------------------------------------

say "==> Installing the program"
if [ -n "$binary" ]; then
    cp "$binary" "$tmp/nexa-signal"
else
    fetch "$URL" "$tmp/nexa-signal" || die "download failed: $URL"
    if fetch "$URL.sha256" "$tmp/nexa-signal.sha256" 2>/dev/null; then
        want=$(cut -d' ' -f1 <"$tmp/nexa-signal.sha256")
        have=$( (sha256sum "$tmp/nexa-signal" 2>/dev/null || shasum -a 256 "$tmp/nexa-signal") | cut -d' ' -f1)
        [ "$want" = "$have" ] || die "the download does not match its checksum; not installing it"
    else
        die "no checksum next to the download ($URL.sha256); not installing an unchecked program"
    fi
fi
chmod 0755 "$tmp/nexa-signal"
"$tmp/nexa-signal" --example-config >/dev/null 2>&1 || die "the program does not run on this machine"

if [ -z "$ROOT" ]; then
    id nexa-signal >/dev/null 2>&1 ||
        useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin nexa-signal ||
        die "could not create the nexa-signal user"
fi
mkdir -p "$ROOT/usr/local/bin" "$CONF" "$TLS" "$(dirname "$UNIT")"
install -m 0755 "$tmp/nexa-signal" "$BIN"

# ---- the certificate -----------------------------------------------------------------------
# A private CA signs the server's certificate (a certificate that is its own CA is refused by
# TLS clients). The CA is what apps ask people to trust, shown as a fingerprint.

make_cert=1
[ "$hand" = 0 ] || make_cert=0
if [ "$hand" = 0 ] && [ -r "$TLS/fullchain.pem" ] && [ -r "$TLS/ca.pem" ] && [ -r "$TLS/host" ] && [ "$(cat "$TLS/host")" = "$host" ]; then
    openssl x509 -checkend 2592000 -noout -in "$TLS/fullchain.pem" >/dev/null 2>&1 && make_cert=0
fi
if [ "$make_cert" = 1 ]; then
    say "==> Making a certificate for $host"
    if [ "$is_ip" = 1 ]; then san="IP:$host"; else san="DNS:$host"; fi
    # A CA kept from before keeps the fingerprint people already trusted.
    if [ ! -r "$TLS/ca.pem" ] || [ ! -r "$TLS/ca.key" ] ||
        ! openssl x509 -checkend 31536000 -noout -in "$TLS/ca.pem" >/dev/null 2>&1; then
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
            -subj "/CN=NexaDesk signal CA" \
            -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
            -addext "keyUsage=critical,keyCertSign,cRLSign" \
            -keyout "$tmp/ca.key" -out "$tmp/ca.pem" >/dev/null 2>&1 || die "openssl could not make the CA"
        install -m 0600 "$tmp/ca.key" "$TLS/ca.key"
        install -m 0644 "$tmp/ca.pem" "$TLS/ca.pem"
    fi
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "/CN=$host" -keyout "$tmp/privkey.pem" -out "$tmp/leaf.csr" >/dev/null 2>&1 ||
        die "openssl could not make the key"
    printf 'subjectAltName=%s\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' "$san" >"$tmp/ext"
    # 398 days is the longest browsers accept for a server certificate; renew by running this again.
    openssl x509 -req -in "$tmp/leaf.csr" -CA "$TLS/ca.pem" -CAkey "$TLS/ca.key" -CAcreateserial \
        -CAserial "$tmp/ca.srl" -days 398 -extfile "$tmp/ext" -out "$tmp/leaf.pem" >/dev/null 2>&1 ||
        die "openssl could not sign the certificate"
    cat "$tmp/leaf.pem" "$TLS/ca.pem" >"$tmp/fullchain.pem"
    install -m 0444 "$tmp/fullchain.pem" "$TLS/fullchain.pem"
    install -m 0400 "$tmp/privkey.pem" "$TLS/privkey.pem"
    printf '%s' "$host" >"$TLS/host"
fi
if [ -z "$ROOT" ] && [ "$hand" = 0 ]; then
    chown -R 10001:10001 "$TLS"
    chmod 0750 "$TLS"
    # The CA's key signs certificates; the service never needs it.
    chown root:root "$TLS/ca.key"
    chmod 0600 "$TLS/ca.key"
fi
fingerprint=""
if [ "$hand" = 0 ]; then fingerprint=$(openssl x509 -noout -fingerprint -sha256 -in "$TLS/ca.pem" | sed 's/^.*=//'); fi

# ---- the configuration ---------------------------------------------------------------------

if [ "$rewrite" = 1 ]; then
say "==> Writing the configuration"
{
    cat <<EOF
# Written by deploy/install.sh. Edit it, then: systemctl restart nexa-signal
# Look at the key and address any time with: sh install.sh --show

[signal]
listen = "0.0.0.0:$port"
tls_cert = "$TLS/fullchain.pem"
tls_key = "$TLS/privkey.pem"
server_names = ["$host"]
access_secrets = ["$key"]

[stun]
listen = ["0.0.0.0:3478"]
advertise = ["$ip:3478"]

[relay]
listen = "0.0.0.0:3480"
advertise = "$ip:3480"
bandwidth_bps = 50000000
max_sessions = 1000

[limits]
max_connections = 10000
max_connections_per_ip = 32
EOF
    if [ "$web" = 1 ]; then
        printf '\n[web]\nmax_allocations = 200\nmax_per_ip = 8\n'
        [ -z "$web_password" ] || printf 'password = "%s"\n' "$web_password"
    elif [ "$web" = 2 ]; then
        # Already serving the browser page: keep its settings.
        printf '\n'
        awk '/^\[/{p=($0=="[web]")} p' "$TOML"
    fi
} >"$tmp/nexa-signal.toml"
install -m 0600 "$tmp/nexa-signal.toml" "$TOML"
if [ -z "$ROOT" ]; then chown 10001:10001 "$TOML"; fi
fi

# Check it as the service will.
"$BIN" --check-config "$TOML" >"$tmp/check.txt" 2>&1 || { cat "$tmp/check.txt" >&2; die "the configuration does not pass --check-config"; }

# ---- the service ---------------------------------------------------------------------------

if [ -z "$ROOT" ]; then
    say "==> Starting the service"
    cat >"$UNIT" <<'EOF'
[Unit]
Description=NexaDesk signaling, STUN and relay server
Wants=network-online.target
After=network-online.target

[Service]
User=nexa-signal
Group=nexa-signal
ExecStartPre=/usr/local/bin/nexa-signal --check-config /etc/nexa-signal/nexa-signal.toml
ExecStart=/usr/local/bin/nexa-signal --config /etc/nexa-signal/nexa-signal.toml
Restart=on-failure
RestartSec=2s
KillSignal=SIGTERM
TimeoutStopSec=10s
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
ProcSubset=pid
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged
UMask=0077
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
    chmod 0644 "$UNIT"
    systemctl daemon-reload
    systemctl enable nexa-signal >/dev/null 2>&1
    systemctl restart nexa-signal
    ok=0
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        if systemctl is-active --quiet nexa-signal; then ok=1; break; fi
    done
    if [ "$ok" != 1 ]; then
        journalctl -u nexa-signal -n 20 --no-pager >&2 || true
        die "the service did not start (log above; common cause: port $port is used by another program)"
    fi

    # ---- the firewall: only the ones that are on --------------------------------------------
    opened=""
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then
        ufw allow "$port/tcp" >/dev/null && ufw allow 3478/udp >/dev/null && ufw allow 3480/udp >/dev/null && opened="ufw"
    fi
    if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        firewall-cmd --permanent --add-port="$port/tcp" --add-port=3478/udp --add-port=3480/udp >/dev/null &&
            firewall-cmd --reload >/dev/null && opened="$opened firewalld"
    fi
fi

# ---- the answer ----------------------------------------------------------------------------

if [ "$port" = 443 ]; then addr="wss://$host"; page="https://$host/"; else addr="wss://$host:$port"; page="https://$host:$port/"; fi
[ -n "$host" ] || die "no server name in $TOML; pass --host <ip or domain>"
if [ "$web" != 0 ]; then
    if [ -n "$web_password" ]; then webline="Browser page:   $page  (password: $web_password)"; else webline="Browser page:   $page  (no password: anyone with a device ID and its code can try)"; fi
else
    webline="Browser page:   off (run again with --web to turn it on)"
fi
{
    say "NexaDesk signaling server"
    say ""
    say "  Server address: $addr"
    say "  Signaling key:  $key"
    say "  $webline"
    if [ -n "$fingerprint" ]; then
        say "  Certificate:    private; apps ask to trust it once. Check that the fingerprint is"
        say "                  $fingerprint"
    else
        say "  Certificate:    the one named in $TOML (not made by this script)"
    fi
    say ""
    say "  Put the address and the key in each app's \"Signaling server\" and \"Access password\"."
    say "  To offer it to everyone, add this to servers.json in the project:"
    say "    { \"endpoint\": \"$addr\", \"key\": \"$key\" }"
    say ""
    say "  Look at this again:  sh install.sh --show      Logs:  journalctl -u nexa-signal -f"
} >"$tmp/info.txt"
install -m 0600 "$tmp/info.txt" "$INFO"

say ""
cat "$tmp/info.txt"
if [ -z "$ROOT" ]; then
    say ""
    if [ -n "$opened" ]; then say "Opened the ports ($port/tcp, 3478/udp, 3480/udp) in:$opened."; else
        say "No active ufw or firewalld found, so no port was opened by this script."; fi
    say "If this server is in a cloud (Alibaba, AWS, Tencent, ...), also allow TCP $port and UDP 3478, 3480"
    say "in the cloud console's security group. Then, from another machine:  nc -z $host $port"
fi
