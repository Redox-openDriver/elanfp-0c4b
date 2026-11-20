#!/bin/sh
set -eu

step() { printf '\n=== %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAILED: %s\n' "$*"; failures=$((failures + 1)); }
expect() {
  label=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$label"; else bad "$label"; fi
}
failures=0
repo=/src/elanfp-0c4b
dist=$repo/linux/dist

step "environment"
printf 'alpine %s, %s, libc: %s\n' "$(cat /etc/alpine-release)" "$(rustc --version)" "$(ldd --version 2>&1 | head -n 1)"

step "tests on musl"
cd "$repo"
cargo test --release --locked --no-fail-fast 2>&1 | grep -E '^test result|FAILED|panicked|^error' | sort | uniq -c
cargo test --release --locked --no-fail-fast >/dev/null 2>&1 && ok "all test suites pass on musl" || bad "test suites on musl"
bus_tests=$(cargo test --release --locked -p elanfp-fprintd --test bus --no-run --message-format=json 2>/dev/null | python3 -I -c '
import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable") and message.get("target", {}).get("name") == "bus":
        print(message["executable"])
' | tail -n 1)
if [ -n "$bus_tests" ] && su -s /bin/sh nobody -c "'$bus_tests'" > /tmp/bus-unprivileged.log 2>&1; then
  ok "bridge rules hold for an unprivileged caller"
else
  tail -n 30 /tmp/bus-unprivileged.log
  bad "bridge tests as an unprivileged user"
fi

install_with() {
  ELANFP_SERVICE_MANAGER=$1 sh "$dist/install.sh" > /tmp/install.log 2>&1 || { cat /tmp/install.log; return 1; }
  grep -E '^(service manager|template key|disk encryption|device access|service):' /tmp/install.log
}
purge() {
  if ! sh "$dist/uninstall.sh" --purge > /tmp/uninstall.log 2>&1; then
    cat /tmp/uninstall.log
    bad "uninstall --purge stopped half way"
  fi
  for left in /var/lib/elanfp /etc/elanfp /usr/local/bin/elanfpd /usr/local/bin/elanfp /usr/local/bin/elanfp-fprintd /usr/local/lib/security/pam_elanfp.so \
    /usr/local/bin/elanfp-notify /etc/xdg/autostart/elanfp-notify.desktop \
    /etc/dbus-1/system.d/elanfp-fprint.conf /etc/udev/rules.d/70-elanfp.rules; do
    [ ! -e "$left" ] || bad "uninstall --purge left $left"
  done
  ! grep -q '^elanfp' /etc/passwd || bad "uninstall --purge left the accounts"
  rm -rf /run/elanfp
}
answers() {
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if /usr/local/bin/elanfp list --uid 0 >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}
service_user() {
  pid=$(pgrep -x elanfpd | head -n 1)
  [ -n "$pid" ] && [ "$(stat -c %U "/proc/$pid")" = elanfp ]
}

bridge_answers() {
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if dbus-send --system --print-reply --dest=net.reactivated.Fprint /net/reactivated/Fprint/Manager net.reactivated.Fprint.Manager.GetDefaultDevice 2>/dev/null | grep -q '/net/reactivated/Fprint/Device/0'; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}
bridge_user() {
  pid=$(pgrep -x elanfp-fprintd | head -n 1)
  [ -n "$pid" ] && [ "$(stat -c %U "/proc/$pid")" = elanfp-fprint ]
}
bridge_lists_nothing() {
  dbus-send --system --print-reply --dest=net.reactivated.Fprint /net/reactivated/Fprint/Device/0 net.reactivated.Fprint.Device.ListEnrolledFingers string:root 2>&1 | grep -q NoEnrolledPrints
}
root_requests_the_name() {
  dbus-send --system --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.RequestName string:net.reactivated.Fprint uint32:4 2>&1
}
stock_policy_lets_root_own() {
  root_requests_the_name | grep -q 'uint32 1'
}
bridge_policy_stops_root() {
  root_requests_the_name | grep -q 'AccessDenied'
}
watcher_hears_a_request() {
  python3 -I - <<'PY'
import socket, subprocess, sys, time
watcher = socket.socket(socket.AF_UNIX)
watcher.settimeout(10)
watcher.connect("/run/elanfp/socket")
watcher.sendall(b"WATCH 0 fmt=keys\n")
time.sleep(0.5)
subprocess.run(["/usr/local/bin/elanfp", "enroll", "left-thumb", "--uid", "0"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
heard = b""
while b"watch-over" not in heard:
    piece = watcher.recv(4096)
    if not piece:
        break
    heard += piece
sys.exit(0 if b"MSG watch-enroll route=direct pid=" in heard and b"MSG watch-over" in heard else 1)
PY
}
bridge_refuses_verify_without_a_console() {
  dbus-send --system --print-reply --dest=net.reactivated.Fprint /net/reactivated/Fprint/Device/0 net.reactivated.Fprint.Device.VerifyStart string:any 2>&1 | grep -q 'ClaimDevice'
}

step "OpenRC"
mkdir -p /run/openrc
touch /run/openrc/softlevel
rc-service dbus start >/dev/null 2>&1
cat > /usr/share/dbus-1/system.d/net.reactivated.Fprint.conf <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <policy user="root">
    <allow own="net.reactivated.Fprint"/>
  </policy>
</busconfig>
XML
dbus-send --system --type=method_call --dest=org.freedesktop.DBus / org.freedesktop.DBus.ReloadConfig
expect "the stock fprintd policy lets root own the name" stock_policy_lets_root_own
install_with ""
expect "detected as openrc" grep -q '^service manager:  openrc' /tmp/install.log
expect "service answers" answers
expect "runs as elanfp" service_user
expect "supervised" rc-service elanfpd status
expect "a watcher of the same account hears a request begin and end" watcher_hears_a_request
expect "the session agent and its autostart entry are installed" sh -c '[ -x /usr/local/bin/elanfp-notify ] && [ -f /etc/xdg/autostart/elanfp-notify.desktop ]'
expect "bridge reported as running" grep -q '^desktop bridge:   running' /tmp/install.log
expect "bridge owns net.reactivated.Fprint" bridge_answers
expect "bridge runs as elanfp-fprint" bridge_user
expect "bridge reaches the service with delegation" bridge_lists_nothing
expect "bridge keeps the claim rule" bridge_refuses_verify_without_a_console
expect "with the bridge installed root cannot take its name" bridge_policy_stops_root
rc-service elanfp-fprintd stop >/dev/null 2>&1 || true
expect "a stopped bridge does not hand the name back to root" bridge_policy_stops_root
rm -f /usr/local/bin/elanfp-fprintd
ELANFP_FPRINTD_BRIDGE=no ELANFP_NOTIFY=no sh "$dist/install.sh" > /tmp/install.log 2>&1 || cat /tmp/install.log
expect "installing again without the bridge removes what is left of it, even with its program already gone" sh -c '[ ! -e /usr/local/bin/elanfp-fprintd ] && [ ! -e /etc/dbus-1/system.d/elanfp-fprint.conf ] && [ ! -e /etc/init.d/elanfp-fprintd ]'
expect "the summary does not warn about the name when the bridge is off" sh -c '! grep -q "net.reactivated.Fprint" /tmp/install.log'
expect "installing again without the session agent removes it" sh -c '[ ! -e /usr/local/bin/elanfp-notify ] && [ ! -e /etc/xdg/autostart/elanfp-notify.desktop ]'
expect "the service itself stays" answers
rc-service elanfpd stop >/dev/null 2>&1 || true
purge
expect "after removal the stock policy applies again" stock_policy_lets_root_own
rm -f /run/openrc/softlevel

step "service manager unknown"
mkdir -p /tmp/pretender/scan
printf '#!/bin/sh\nsleep 600\n' > /tmp/pretender/runsvdir
chmod 755 /tmp/pretender/runsvdir
chmod 777 /tmp/pretender/scan
su -s /bin/sh nobody -c '/tmp/pretender/runsvdir /tmp/pretender/scan' >/dev/null 2>&1 &
sleep 0.5
expect "a pretender process is there" pgrep -x runsvdir
install_with ""
expect "a supervisor that an unprivileged user pretends to run is not believed" grep -q '^service manager:  none' /tmp/install.log
expect "nothing was linked into the pretender's directory" sh -c '[ -z "$(ls -A /tmp/pretender/scan)" ]'
pkill -x runsvdir || true
sleep 0.2
mkdir -p /tmp/pretender/suid /tmp/pretender/rootscan
chmod 755 /tmp/pretender/rootscan
printf 'fn main() {\n    std::thread::sleep(std::time::Duration::from_secs(600));\n}\n' > /tmp/pretender/idle.rs
rustc -O -o /tmp/pretender/suid/runsvdir /tmp/pretender/idle.rs
chown root:root /tmp/pretender/suid/runsvdir
chmod 4755 /tmp/pretender/suid/runsvdir
su -s /bin/sh nobody -c '/tmp/pretender/suid/runsvdir /tmp/pretender/rootscan' >/dev/null 2>&1 &
sleep 0.5
expect "a set-uid pretender runs with root's effective id and an ordinary real one" sh -c '[ "$(awk "/^Uid:/ { print \$2 \"-\" \$3 }" "/proc/$(pgrep -x runsvdir | head -n 1)/status")" = "$(id -u nobody)-0" ]'
install_with ""
expect "a set-uid program started by an ordinary user is not believed either" grep -q '^service manager:  none' /tmp/install.log
expect "nothing was linked into the directory it names" sh -c '[ -z "$(ls -A /tmp/pretender/rootscan)" ]'
key_before=$(sha256sum /etc/elanfp/master.key)
expect "key file is private to the service" sh -c '[ "$(stat -c "%a %U" /etc/elanfp/master.key)" = "400 elanfp" ]'
chown root:root /etc/elanfp/master.key
chmod 0600 /etc/elanfp/master.key
install_with "" >/dev/null
expect "a key file left with the wrong owner is put right on the next run" sh -c '[ "$(stat -c "%a %U" /etc/elanfp/master.key)" = "400 elanfp" ]'
expect "installing again keeps the key that was there" sh -c "[ \"\$(sha256sum /etc/elanfp/master.key)\" = \"$key_before\" ]"
expect "the key creation script refuses to replace a key" sh -c "! sh '$dist/newkey.sh' file /etc/elanfp/master.key elanfp:elanfp 2>/dev/null && [ \"\$(sha256sum /etc/elanfp/master.key)\" = \"$key_before\" ] && [ ! -e /etc/elanfp/master.key.new ]"
expect "refuses to run as root" sh -c '! /usr/local/bin/elanfpd'
install -d -o elanfp -g elanfp -m 0755 /run/elanfp
install -d -o elanfp -g elanfp -m 0700 /var/lib/elanfp
su -s /bin/sh elanfp -c '/usr/local/bin/elanfpd' > /tmp/manual.log 2>&1 &
expect "manual start answers" answers
expect "runs as elanfp" service_user
expect "enroll by a non-root user is refused" sh -c 'su -s /bin/sh nobody -c "/usr/local/bin/elanfp enroll x" 2>&1 | grep -q root'
su -s /bin/sh elanfp-fprint -c '/usr/local/bin/elanfp-fprintd' > /tmp/bridge.log 2>&1 &
expect "bridge started by hand owns the name" bridge_answers
pkill -x elanfp-fprintd || true
pkill -x elanfpd || true
sleep 0.5
mkdir -p /tmp/stubs /etc/sv/elanfpd
printf '#!/bin/sh\nexit 8\n' > /tmp/stubs/userdel
chmod 755 /tmp/stubs/userdel
ln -s /etc/sv/elanfpd /tmp/pretender/rootscan/elanfpd
if PATH=/tmp/stubs:$PATH sh "$dist/uninstall.sh" --purge > /tmp/uninstall.log 2>&1; then
  bad "a purge that could not delete the accounts reported success"
else
  ok "a purge that cannot delete the accounts says so with its exit status"
fi
expect "it names the accounts that are left" grep -q 'elanfp-fprint elanfp' /tmp/uninstall.log
expect "and has removed everything else by then" sh -c '[ ! -e /var/lib/elanfp ] && [ ! -e /etc/elanfp ] && [ ! -e /usr/local/bin/elanfpd ] && [ ! -e /etc/udev/rules.d/70-elanfp.rules ]'
expect "the uninstaller leaves alone the directory named by the set-uid pretender" sh -c '[ -L /tmp/pretender/rootscan/elanfpd ]'
pkill -x runsvdir || true
rm -rf /tmp/pretender /tmp/stubs
purge

step "runit"
mkdir -p /etc/service
runsvdir /etc/service >/dev/null 2>&1 &
sleep 0.5
install_with ""
expect "detected as runit" grep -q '^service manager:  runit' /tmp/install.log
expect "service answers" answers
expect "runs as elanfp" service_user
expect "bridge owns net.reactivated.Fprint" bridge_answers
expect "bridge runs as elanfp-fprint" bridge_user
sv down elanfp-fprintd >/dev/null 2>&1 || true
sv down elanfpd >/dev/null 2>&1 || true
purge
pkill -x runsvdir || true
pkill -x runsv || true
sleep 0.5

step "dinit"
mkdir -p /etc/dinit.d/boot.d
printf 'type = internal\nwaits-for.d = boot.d\n' > /etc/dinit.d/boot
dinit --system --services-dir /etc/dinit.d --socket-path /run/dinitctl --quiet >/dev/null 2>&1 &
sleep 0.5
install_with ""
expect "detected as dinit" grep -q '^service manager:  dinit' /tmp/install.log
expect "service answers" answers
expect "runs as elanfp" service_user
expect "bridge owns net.reactivated.Fprint" bridge_answers
expect "bridge runs as elanfp-fprint" bridge_user
dinitctl stop elanfp-fprintd >/dev/null 2>&1 || true
dinitctl stop elanfpd >/dev/null 2>&1 || true
purge
pkill -x dinit || true
sleep 0.5

step "s6"
mkdir -p /run/service
s6-svscan /run/service >/dev/null 2>&1 &
sleep 0.5
install_with ""
expect "detected as s6" grep -q '^service manager:  s6' /tmp/install.log
ln -s /etc/elanfp/s6/elanfpd /run/service/elanfpd
ln -s /etc/elanfp/s6/elanfp-fprintd /run/service/elanfp-fprintd
s6-svscanctl -a /run/service
expect "service answers" answers
expect "runs as elanfp" service_user
expect "bridge owns net.reactivated.Fprint" bridge_answers
s6-svc -d /run/service/elanfp-fprintd >/dev/null 2>&1 || true

step "PAM on musl"
cat > /tmp/pamtest.py <<'PY'
import ctypes, sys
pam = ctypes.CDLL("libpam.so.0")
class Msg(ctypes.Structure): _fields_ = [("style", ctypes.c_int), ("msg", ctypes.c_char_p)]
class Resp(ctypes.Structure): _fields_ = [("resp", ctypes.c_char_p), ("code", ctypes.c_int)]
CONV = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_int, ctypes.POINTER(ctypes.POINTER(Msg)), ctypes.POINTER(ctypes.POINTER(Resp)), ctypes.c_void_p)
@CONV
def conv(n, msgs, resp, data):
    resp[0] = ctypes.cast(ctypes.c_void_p(0), ctypes.POINTER(Resp))
    return 0
class Conv(ctypes.Structure): _fields_ = [("conv", CONV), ("data", ctypes.c_void_p)]
h = ctypes.c_void_p()
c = Conv(conv, None)
assert pam.pam_start_confdir(sys.argv[1].encode(), b"root", ctypes.byref(c), b"/tmp/pam.d", ctypes.byref(h)) == 0
print(pam.pam_authenticate(h, 0))
PY
cat > /tmp/yes.py <<'PY'
import os, socket, sys
path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
listener = socket.socket(socket.AF_UNIX)
listener.bind(path)
listener.listen(4)
while True:
    client, _ = listener.accept()
    client.recv(256)
    client.sendall(b"OK yes\n")
    client.close()
PY
python3 /tmp/yes.py /tmp/yes.socket &
yes_service=$!
mkdir -p /tmp/pam.d
module=/usr/local/lib/security/pam_elanfp.so
printf 'auth required %s timeout=1\n' "$module" > /tmp/pam.d/closed
printf 'auth required %s timeout=1 console=any\n' "$module" > /tmp/pam.d/any
printf 'auth required %s timeout=1 socket=/tmp/yes.socket service_user=root\n' "$module" > /tmp/pam.d/yes
printf 'auth required %s timeout=1 socket=/tmp/yes.socket service_user=root console=any\n' "$module" > /tmp/pam.d/yes-any
for library in /usr/lib/libelogind.so.0 /lib/libelogind.so.0; do
  [ ! -e "$library" ] || mv "$library" "$library.hidden"
done
expect "module loads and stays closed without logind (9 = unavailable)" sh -c '[ "$(python3 /tmp/pamtest.py closed)" = 9 ]'
expect "without logind even a service that says yes is not asked" sh -c '[ "$(python3 /tmp/pamtest.py yes)" = 9 ]'
expect "console=any lets a yes through (0 = success)" sh -c '[ "$(python3 /tmp/pamtest.py yes-any)" = 0 ]'
expect "console=any reaches the real service: no fingers means unavailable, not a failure" sh -c '[ "$(python3 /tmp/pamtest.py any)" = 9 ]'
for library in /usr/lib/libelogind.so.0 /lib/libelogind.so.0; do
  [ ! -e "$library.hidden" ] || mv "$library.hidden" "$library"
done
expect "with elogind but nobody at the console it stays closed" sh -c '[ "$(python3 /tmp/pamtest.py yes)" = 9 ]'
kill "$yes_service" 2>/dev/null || true

if [ -n "${READER_NODE:-}" ]; then
  step "the real reader on musl without systemd"
  chown root:elanfp "$READER_NODE"
  chmod 0660 "$READER_NODE"
  timeout 8 /usr/local/bin/elanfp enroll smoke --uid 0 > /tmp/reader.log 2>&1 || true
  cat /tmp/reader.log
  expect "the service opened the reader and asked for a touch" grep -q '\[1\]' /tmp/reader.log
fi

s6-svc -d /run/service/elanfpd >/dev/null 2>&1 || true
pkill -x s6-svscan || true

step "result"
printf 'failures: %s\n' "$failures"
[ "$failures" -eq 0 ]
