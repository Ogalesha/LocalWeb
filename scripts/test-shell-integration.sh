#!/bin/bash
# Verifies that `php`, `composer` and the Node tools follow the project folder
# once `localweb integration install` puts the shims on PATH, and that
# everything outside a project keeps using the system tools.
#
# The test uses its own HOME and XDG directories, so it neither touches the
# caller's shell configuration nor any LocalWeb data already on the host.
#
# Usage: scripts/test-shell-integration.sh [binary-dir]
set -euo pipefail

binary_dir=${1:-/usr/lib/localweb}
: "${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set, as in a desktop session or CI host}"
work=$(mktemp -d /tmp/localweb-shell.XXXXXX)
daemon_pid=
cleanup() {
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        "$binary_dir/localweb" shutdown > /dev/null 2>&1 || true
        for _ in $(seq 1 40); do
            kill -0 "$daemon_pid" 2>/dev/null || break
            sleep 0.25
        done
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

expect() {
    if [ "$2" != "$1" ]; then
        fail "$3: expected '$1', got '$2'"
    fi
}

home="$work/home"
system_bin="$work/system-bin"
mkdir -p "$home" "$work/config" "$work/data" "$work/state" "$system_bin"
printf '#!/bin/sh\necho "system php $*"\n' > "$system_bin/php"
chmod 755 "$system_bin/php"

# The runtime directory stays shared with the login session: `systemctl --user`
# only reaches the user manager through $XDG_RUNTIME_DIR.
export HOME="$home"
export XDG_CONFIG_HOME="$work/config"
export XDG_DATA_HOME="$work/data"
export XDG_STATE_HOME="$work/state"
export SHELL=/bin/bash
shim_dir="$XDG_DATA_HOME/localweb/bin"
sock="$XDG_RUNTIME_DIR/localweb/localweb.sock"
if [ -e "$sock" ]; then
    fail "a LocalWeb daemon already owns $sock; run this test on a disposable host"
fi
export PATH="$shim_dir:$system_bin:$PATH"

localweb="$binary_dir/localweb"
daemon="$binary_dir/localwebd"

cat > "$work/add-project.py" <<'PY'
"""Registers a project over the daemon protocol without touching /etc/hosts."""
import json
import socket
import sys

sock, name, path, version = sys.argv[1:5]
payload = {
    "type": "project_add",
    "data": {
        "path": path,
        "name": name,
        "hostname": name + ".test",
        "document_root": ".",
        "profile": "genericphp",
        "web_server": "nginx",
        "php": version,
        "composer": "2",
        "node": "24",
        "database": "none",
        "cache": "none",
        "supervisor": {"enabled": False, "command": []},
        "cron": [],
        "cron_preset": False,
        "mail": False,
        "autostart": False,
    },
}
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(60)
    connection.connect(sock)
    connection.sendall(json.dumps({"version": 1, "payload": payload}).encode() + b"\n")
    response = b""
    while not response.endswith(b"\n"):
        chunk = connection.recv(65536)
        assert chunk, "the daemon closed the socket without a response"
        response += chunk
result = json.loads(response)["payload"]
assert result["type"] != "error", result
PY

add_project() {
    python3 "$work/add-project.py" "$sock" "$1" "$2" "$3"
}

mkdir -p "$work/demo/sub" "$work/demo/public"
"$daemon" > "$work/daemon.log" 2>&1 &
daemon_pid=$!
started=
for _ in $(seq 1 240); do
    if "$localweb" status > /dev/null 2>&1; then
        started=1
        break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        cat "$work/daemon.log" >&2
        fail "the daemon exited before answering"
    fi
    sleep 0.25
done
if [ -z "$started" ]; then
    cat "$work/daemon.log" >&2
    fail "the daemon did not answer within 60 seconds"
fi

add_project demo "$work/demo" 8.5
add_project nested "$work/demo/sub" 8.4

"$localweb" integration status | grep -q 'Shims: missing' ||
    fail "status does not report the absent integration"
printf 'export EDITOR=vim\n' > "$home/.bashrc"
"$localweb" integration install > /dev/null
for tool in php composer node npm npx; do
    [ -L "$shim_dir/$tool" ] || fail "install did not create the $tool shim"
done
bash -n "$home/.bashrc" || fail "the managed shell block is not valid bash"
grep -q 'BEGIN LOCALWEB SHELL' "$home/.bashrc" || fail "install did not write the shell block"
"$localweb" integration install > /dev/null
expect 1 "$(grep -c 'BEGIN LOCALWEB SHELL' "$home/.bashrc")" "install is idempotent"
status_output=$("$localweb" integration status)
case "$status_output" in
    *"Shims: installed"*) ;;
    *) fail "status does not report the shims: $status_output" ;;
esac
case "$status_output" in
    *"PATH: active"*) ;;
    *) fail "status does not report the PATH entry: $status_output" ;;
esac
echo "PASS integration install is idempotent and reports its state"

php_version='echo PHP_MAJOR_VERSION.".".PHP_MINOR_VERSION;'
cd "$work/demo"
expect 8.5 "$(php -r "$php_version")" "php uses the project runtimes"
expect 8.4 "$(cd sub && php -r "$php_version")" "php follows the deepest project root"
expect 8.5 "$(cd public && php -r "$php_version")" "php follows subdirectories"
echo "PASS php resolves through the project that owns the folder"

cd "$work/demo/public"
expect "$(pwd -P)" "$(php -r 'echo getcwd();')" "php keeps the working directory"
expect "$(pwd -P)" "$(node -p 'process.cwd()')" "node keeps the working directory"
expect 8.5 "$(LOCALWEB_PROJECT=demo php -r "$php_version")" \
    "LOCALWEB_PROJECT keeps localweb shell working"
expect "$(pwd -P)" "$(LOCALWEB_PROJECT=demo php -r 'echo getcwd();')" \
    "LOCALWEB_PROJECT keeps the working directory"
echo "PASS the shims run in the folder the command was typed in"

cd "$work/demo"
composer_version=$(composer --version 2>&1)
case "$composer_version" in
    "Composer version 2."*) ;;
    *) fail "composer inside the project returned '$composer_version'" ;;
esac
node_version=$(node --version)
case "$node_version" in
    v24.*) ;;
    *) fail "node inside the project returned '$node_version'" ;;
esac
npm_version=$(npm --version)
case "$npm_version" in
    [0-9]*.[0-9]*) ;;
    *) fail "npm inside the project returned '$npm_version'" ;;
esac
echo "PASS composer, node and npm use the project runtimes"

expect PING "$(printf 'ping' | php -r 'echo strtoupper(stream_get_contents(STDIN));')" \
    "php keeps standard input"
status=0
php -r 'exit(3);' || status=$?
expect 3 "$status" "php keeps the exit code"
expect 8.5 "$("$localweb" php demo -- -r "$php_version")" "localweb php still works"
expect 8.5 "$("$localweb" exec demo -- php -r "$php_version")" "localweb exec still works"
shell_output=$(SHELL=/bin/sh "$localweb" shell demo <<'EOF'
echo "$LOCALWEB_PROJECT"
php -r 'echo PHP_MAJOR_VERSION.".".PHP_MINOR_VERSION;'
EOF
)
expect $'demo\n8.5' "$shell_output" "localweb shell still exports the project"
echo "PASS php keeps stdin, exit codes and the explicit commands"

cd "$work"
expect "system php -v" "$(php -v)" "system php runs outside projects"
stderr=$(php -v 2>&1 > /dev/null)
expect "" "$stderr" "the fallback stays silent without a terminal"
cd "$work/demo"
expect "system php -v" "$(LOCALWEB_NO_SHIM=1 php -v)" "LOCALWEB_NO_SHIM restores the system tools"
echo "PASS the system tools stay in charge outside projects"

"$localweb" shutdown
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=
cd "$work/demo"
expect "system php -v" "$(php -v)" "a stopped daemon falls back to the system php"
status=0
LOCALWEB_PROJECT=demo php -v > /dev/null 2>&1 || status=$?
if [ "$status" -eq 0 ]; then
    fail "an explicit project must not fall back silently"
fi
if command -v script > /dev/null 2>&1; then
    tty_output=$(script -qec "php -v" /dev/null 2>&1)
    case "$tty_output" in
        *"LocalWeb:"*"localweb start"*) ;;
        *) fail "the interactive fallback printed no warning: '$tty_output'" ;;
    esac
    echo "PASS a stopped daemon warns in a terminal"
else
    echo "SKIP interactive fallback warning: script is unavailable"
fi

"$localweb" integration remove > /dev/null
for tool in php composer node npm npx; do
    [ ! -e "$shim_dir/$tool" ] || fail "remove left the $tool shim"
done
if [ -d "$shim_dir" ]; then
    fail "remove left the shim directory"
fi
if grep -q 'LOCALWEB SHELL' "$home/.bashrc"; then
    fail "remove left the shell block"
fi
expect 'export EDITOR=vim' "$(cat "$home/.bashrc")" "remove restores the startup file"
"$localweb" integration status | grep -q 'Shims: missing' ||
    fail "status still reports an installed integration"
echo "PASS integration remove restores the shell configuration"

echo "Shell integration passed"
