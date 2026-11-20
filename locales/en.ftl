usage =
    elanfp — open-source driver for the ELAN 04f3:0c4b fingerprint reader

      sudo elanfp enroll <finger>   enroll a finger (for example right-index-finger)
      elanfp verify [finger]        verify a touch
      elanfp list                   enrolled fingers
      sudo elanfp delete <finger>   delete a finger
      sudo elanfp unlock            lift the lockout after failed attempts

    root may name another user: --uid <number>

    exit status: 0 — success, 1 — not recognised, 2 — error, 3 — reader busy
usage-dev =
    elanfp-dev — direct sensor access for development; not installed

      elanfp-dev info                      the device and its endpoints
      elanfp-dev probe                     firmware, size, calibration
      elanfp-dev touch [sensor|frames]     one touch with the chosen way of waiting
      elanfp-dev collect <label> [count]   frames and touch dynamics into captures/dataset/<label>
missing-argument = missing argument: { $what }
arg-finger = finger
arg-label = label
error = error: { $text }
reply-ok = ✓ { $text }
reply-no = ✗ { $text }
error-internal = internal error: { $detail }

ipc-closed = the service closed the connection
ipc-garbled = unexpected reply from the service
ipc-unavailable = elanfpd service is not available ({ $path }): { $detail }
runtime-dir-unset = XDG_RUNTIME_DIR is not set

sensor-not-found = sensor 04f3:0c4b not found
usb-timeout = the sensor did not answer in time (command { $command })
usb-error = USB error (command { $command }): { $detail }
sensor-unsupported = untested sensor variant { $vid }:{ $pid }, firmware { $fw }, generation { $gen }, size { $dim }: the driver has to be adapted
sensor-short-write = the sensor accepted { $got } of { $expected } bytes (command { $command })
sensor-short-reply = the sensor returned { $got } of { $expected } bytes (command { $command })

touch-lift = lift your finger
touch-finger-on-sensor = there is a finger on the sensor — lift it
touch-stuck = something keeps lying on the sensor: lift your finger and try again
reject-partial = the finger did not cover the whole sensor
reject-ridges = no ridges visible (wet, dirty or pressed too hard?)
reject-moving = the finger moved
quality = coverage { $coverage }%, contrast { $contrast }, shift { $motion }% (up to { $limit }% allowed)

region-0 = centre of the pad
region-1 = higher, towards the tip
region-2 = lower, towards the joint
region-3 = left side
region-4 = right side
region-5 = fingertip
region-6 = finger turned left
region-7 = finger turned right
region-8 = however is comfortable, your usual way

finger-name-invalid = finger name: Latin letters, digits, - _ . (up to 64 characters)
finger-exists = finger «{ $finger }» is already enrolled; delete it first to replace it
finger-unknown = finger «{ $finger }» is not enrolled
enroll-intro = enrolling «{ $finger }»: wipe the reader first, then place the same finger, a little differently each time. It ends by itself once at least { $min } touches are confirmed and { $streak } in a row are recognised with confidence — a whole round of the prompts; the limit is { $max } touches
enroll-prompt = {"["}{ $n }] { $region } — place your finger and hold still
touch-timeout-wait = no touch yet, waiting again
touch-missed-retry = no touch ({ $status }), once more
touch-rejected-retry = not accepted: { $reason } ({ $quality }), once more
enroll-same-place = this touch landed almost where many of the earlier ones did: move the finger further. If the finger did move, something is on the reader — wipe it
enroll-accepted = accepted: { $quality }, { $points } points; { $novelty ->
        [known] known area
       *[new] new area
    } (score { $score }); recognised in a row { $streak }/{ $need }, confirmed { $kept } (at least { $min } needed)
enroll-covered = finger covered: { $streak ->
        [one] { $streak } touch in a row recognised
       *[other] { $streak } touches in a row recognised
    }
enroll-consistency = consistency check in { $seconds } s: { $dropped ->
        [0] all touches agree
        [one] removed { $dropped } touch that matches no other (another finger or the edge)
       *[other] removed { $dropped } touches that match no other (another finger or the edge)
    }
enroll-done = finger «{ $finger }» enrolled: { $count ->
        [one] { $count } touch
       *[other] { $count } touches
    }, encrypted with { $algorithm }

no-fingers = no fingers enrolled
verify-prompt = place your finger on the reader (attempt { $attempt }/{ $attempts })
verify-no-touch = no touch
verify-bad-frame = unusable frame: { $reason }
verify-match = recognised finger «{ $finger }»
verify-no-match = not recognised

creds-unavailable = the key is sealed with systemd-creds, which is missing ({ $detail })
creds-failed = systemd-creds { $action }: { $detail }
key-corrupt = the master key is corrupted
home-unset = HOME is not set
key-lost = master key { $path } is gone while encrypted templates remain: they can no longer be read. Delete *.papv and enroll the fingers again
key-file-missing = the template key { $path } is missing: the installer creates it (systemd passes the key through LoadCredentialEncrypted, without systemd it is a file whose path can be set in ELANFP_KEY_FILE)
key-file-exposed = the key file { $path } is accessible to someone besides the service: it needs mode 0400 or 0600 and must belong to the service or root
key-file-unreadable = the key file { $path } cannot be read: { $detail }
daemon-root = the system service does not run as root: start it as the elanfp user (the bundled systemd, OpenRC and runit files do)
process-unprotected = could not forbid memory dumps and privilege gain for this process: { $detail }
sleep-interrupted = the computer slept or froze while this request was waiting for a touch: the request is cancelled — try again
store-foreign-user = this store does not belong to user { $uid }
template-unreadable = the template of finger «{ $finger }» cannot be read: { $detail }

service-own-user-only = this service only works with its own user's data
list-empty = no fingers enrolled
list = { $fingers }
deleted = finger «{ $finger }» deleted
locked = fingerprint temporarily locked after { $failures } failures (~{ $minutes } min left) — use your password
user-no-fingers = the user has no enrolled fingers
verified = finger «{ $finger }» recognised
not-verified = fingerprint not recognised
no-touch = there was no touch
need-sudo = only root may enroll and delete fingers or lift a lockout (run through sudo or doas)
foreign-fingers = no access to another user's fingers
bad-request = malformed request
client-gone = client disconnected

log-started = elanfpd: { $mode ->
        [user] user
       *[system] system
    } mode, socket { $socket }, encryption { $algorithm }
log-sleep-watch = a sleep of the computer during a request is noticed { $by ->
        [counter] by the kernel's suspend counter and by the clocks
       *[clock] by the clocks only: the kernel's suspend counter is not available
    }
log-request-failed = request failed: { $detail }
log-enroll = enroll: uid={ $uid } finger={ $finger } (requested by uid={ $peer })
log-delete = delete: uid={ $uid } finger={ $finger } (requested by uid={ $peer })
log-verify-match = verify: uid={ $uid } (requested by uid={ $peer }) — matched { $finger }
log-verify-no-match = verify: uid={ $uid } (requested by uid={ $peer }) — no match, failures in a row { $count }
fatal = elanfpd: { $text }

info-device = { $vid }:{ $pid } bus { $bus } address { $address } — { $maker } / { $product }, speed { $speed }
info-interface = interface { $number } alt { $alt }: class { $class }
info-endpoint = endpoint { $address } { $direction } { $kind }, packet { $size } bytes
probe-firmware = firmware: { $fw }
probe-generation = generation (40 21 0a): { $gen }
probe-size = size: { $width }×{ $height } (raw bytes { $raw })
probe-dac = DAC (40 21 06): { $dac }
probe-mean = calibration mean (40 24): { $mean }
probe-status = status (40 13): { $status }
collect-prompt = {"["}{ $n }/{ $count }] «{ $label }»: { $region }, hold still
collect-accepted = accepted: { $quality }
collect-rejected = frame not accepted: { $reason } ({ $quality }), once more
collect-done = done: { $count } frames in { $dir }
dataset-listed = { $kept } frames listed, { $rejected } rejected by the quality filter: { $file }
dataset-usage = usage: elanfp-dataset list | replay | stained <share in percent, 1–99> | dynamics
replay-stained = stained reader: { $file } is blended into every frame with weight { $percent }%; foreign touches are every { $every }th frame

busy = the reader is busy with another request — try again in a moment
service-failure = internal service error (details in the journal: journalctl -u elanfpd)
already-running = the service is already running ({ $socket })
daemon-usage = usage: elanfpd [--user]
bad-count = the count must be a number, not «{ $value }»
too-many-fingers = { $limit } fingers are already enrolled — that is the limit; delete one first
enroll-abandoned = enrollment aborted: no touches for too long
enroll-limit = the limit of { $max } touches has been reached
touch-test-prompt = place your finger — waiting up to { $seconds } s ({ $mode ->
        [frames] the finger is found by polling frames
       *[sensor] the sensor itself reports the finger
    })
arg-touch-mode = mode: sensor or frames
transport-without-timeouts = this USB transport cannot interrupt a wait: the finger can only be found by polling frames
scheme-error = scheme { $scheme }: { $detail }
daemon-usage-redox = usage: elanfpd <xhci-scheme> <port> <interface> (started by xhcid from /usr/lib/xhcid.d/elanfp.toml)
log-started-redox = elanfpd: { $path }, controller { $controller }, port { $port }, sensor firmware { $firmware }, encryption { $algorithm }
usage-redox =
    elanfp — open driver for the ELAN 04f3:0c4b fingerprint reader

      elanfp enroll <finger> [--uid N]   enroll a finger (root only)
      elanfp verify [finger] [--uid N]   check a touch
      elanfp list [--uid N]              enrolled fingers
      elanfp delete <finger> [--uid N]   delete a finger (root only)
      elanfp unlock [--uid N]            lift the lockout after failed attempts (root only)

    Finger name: latin letters, digits, "-", "_", "."
arg-uid = user number after --uid
bad-uid = the user number must be a number, not "{ $value }"
unexpected-argument = unexpected argument: { $value }
enroll-unusable = enrollment stopped: { $count } unusable touches without a single good frame — wipe the finger and the sensor and start again
log-template-skipped = template skipped: uid={ $uid } finger={ $finger }: { $detail }
log-adapt-failed = adaptation not saved: uid={ $uid } finger={ $finger }: { $detail }
log-lockout = lockout: uid={ $uid } for { $minutes } min after failed comparisons
log-lockout-unsaved = failure counter not written to disk: uid={ $uid }: { $detail }
usb-transfer-failed = transfer failed: { $cause }
usb-endpoint-unavailable = endpoint unavailable: { $cause }
usb-stalled = the device stalled the endpoint (STALL)
usb-xhcid-failed = xhcid: { $cause }
usb-transfer-unknown = xhcid reported a failed transfer without details
usb-endpoint-missing = endpoint { $endpoint } is not open
usb-transfer-too-large = a transfer of { $bytes } bytes exceeds the limit of { $limit }
sensor-ambiguous = several built-in 04f3:0c4b devices found — the driver does not pick one at random
sensor-not-built-in = a 04f3:0c4b device is attached to an external port: the driver only works with the built-in reader
uid-required = say whose finger this is: --uid <user number> (the command runs as root without sudo)
dataset-set-name = a set must be named owner-finger-condition: { $path }
dataset-no-base = the set has no background frame base-*: { $path }
contended = while waiting for your finger another program asked for the fingerprint too — the request is cancelled; use your password and check what else asked for it
quiet = the fingerprint is switched off for a moment (~{ $seconds } s left): nobody touched the reader on earlier requests, or requests collided — use your password
log-contended = two verify requests at once for uid={ $uid }: the waiting one (from uid={ $peer }) is cancelled, the fingerprint is switched off for a while
dataset-bad-frame = the frame cannot be read or has the wrong size: { $path }
replay-enrolled = { $finger }: enrolled in { $touches } touches, kept { $kept }, dropped { $dropped }; unlinked areas { $groups }, the largest { $largest }; touches showing the same still picture as many earlier ones { $still }
replay-failed = { $finger }: enrollment failed after { $touches } touches — { $reason }
replay-verified = { $finger }: own touches { $probes }, missed on the first try { $missed } ({ $percent }%), learned { $learned }, templates now { $templates }; foreign touches { $impostors }, accepted { $accepted }, highest score { $top } at threshold { $threshold }
fprint-started = elanfp-fprintd: name { $name } on the system bus, service { $socket }
fprint-bus = cannot take the name on D-Bus: { $detail }
fprint-bad-setting = unknown value { $name }={ $value }
fprint-not-claimed = the device is not claimed by this client: call Claim first
fprint-in-use = the reader is already in use by another client or operation
fprint-denied = this operation is not permitted
fprint-internal = internal bridge error
fprint-unknown-user = no such user
fprint-no-prints = the user has no enrolled fingers
fprint-no-action = nothing to stop: no operation is running
fprint-bad-finger = unknown finger name
fprint-not-deleted = some prints could not be deleted
log-touch-dynamics = touch dynamics: uid={ $uid } frames={ $frames } settled in { $settle } ms, held { $held } ms, coverage rise { $rise }, contrast drift { $drift }, motion { $motion }
dynamics-set = { $set }: touches { $touches }; medians — frames { $frames }, settled in { $settle } ms, held { $held } ms, coverage rise { $rise }, contrast drift { $drift }, motion { $motion }
fprint-bus-lost = the connection to the D-Bus bus is lost
sensor-port-unknown = the firmware of this computer does not say that the reader sits on an internal port, so it is not used: an external device with the same ID cannot be told apart
peer-unknown = this account is not known to the system
usb-speed-low = low (1.5 Mbit/s)
usb-speed-full = full (12 Mbit/s)
usb-speed-high = high (480 Mbit/s)
usb-speed-super = SuperSpeed (5 Gbit/s)
usb-speed-super-plus = SuperSpeed+ (10 Gbit/s)
usb-speed-unknown = unknown
usb-direction-in = in
usb-direction-out = out
usb-kind-control = control
usb-kind-isochronous = isochronous
usb-kind-bulk = bulk
usb-kind-interrupt = interrupt
attempt-unrecorded = the fingerprint is unavailable right now: the service cannot record the attempt on disk — use your password
unlocked = the fingerprint lockout is lifted
enroll-too-long = enrollment stopped: it has been running for more than { $minutes } min — start again
log-verify-score = comparison score: uid={ $uid } { $score }
log-verify-idle = verify: uid={ $uid } — a request from root ended without a touch, { $count } in a row
log-refused-locked = verify: uid={ $uid } — refused, locked for another ~{ $minutes } min
log-refused-quiet = verify: uid={ $uid } — refused, paused for another ~{ $seconds } s
log-benched = verify: uid={ $uid } — the request went without a touch while a request for uid={ $by } was waiting for the reader; the next requests for uid={ $uid } wait ~{ $seconds } s
watch-verify = a fingerprint check was requested { $by ->
        [root] by a program running as root
       *[self] from your own account
    }{ $route ->
        [bridge] {" "}through the desktop bridge
       *[direct] {""}
    }{ $pid ->
        [0] {""}
       *[other] , process { $pid }
    }, PAM service: { $program }
watch-enroll = a finger is being enrolled for this account{ $route ->
        [bridge] {" "}through the desktop bridge
       *[direct] {""}
    }{ $pid ->
        [0] {""}
       *[other] , process { $pid }
    }
watch-over = the request has ended
watch-idle = no requests
watch-closed = watching for requests has ended
watch-crowded = too many programs already watch the requests of this account
watch-unsupported = watching for requests is not supported here
notify-app = Fingerprint reader
notify-verify-title = A fingerprint was requested
notify-who = "{ $program }", a program running as root, asks for your fingerprint.
notify-who-unnamed = A program running as root asks for your fingerprint.
notify-how = Started like this (in its own words): "{ $command }"
notify-how-cut = Started like this (in its own words, only the beginning is shown): "{ $command }…"
notify-via-bridge = The request came through the desktop bridge.
notify-advice = If it is not your command, do not touch the reader.
notify-enroll-title = Enrolling a finger
notify-enroll = A finger is being enrolled for your account. If that is not you, check who is at the computer.
notify-name-taken = the name { $name } on the session bus is held by another program; notifications will be shown anyway
notify-no-bus = there is no desktop session bus: { $detail }
notify-bus-lost = the desktop session bus has closed
notify-fatal = elanfp-notify: { $text }
log-delegated = request through the bridge uid={ $delegate } on behalf of uid={ $peer }: { $verb } uid={ $uid }
log-unlock = lockout lifted: uid={ $uid } (requested by uid={ $peer })
fprint-no-service-account = the service account «{ $name }» does not exist on this system
ipc-untrusted = whoever answers at { $path } is neither the elanfp service nor root — the request was not sent
bad-setting = unknown value { $name }={ $value }
sensor-layout-unverified = the 04f3:0c4b device does not have the USB endpoints of the verified reader: the driver does not use it
usb-link-dead = after a USB failure the reader's port no longer answers: on Redox only a reboot brings it back
