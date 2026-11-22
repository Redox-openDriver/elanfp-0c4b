usage =
    elanfp — открытый драйвер сканера отпечатков ELAN 04f3:0c4b

      sudo elanfp enroll <палец>   зарегистрировать палец (например right-index-finger)
      elanfp verify [палец]        проверить касание
      elanfp list                  зарегистрированные пальцы
      sudo elanfp delete <палец>   удалить палец
      sudo elanfp unlock           снять блокировку после неудачных попыток

    root может указать другого пользователя: --uid <номер>

    код возврата: 0 — успех, 1 — не узнан, 2 — ошибка, 3 — сканер занят
usage-dev =
    elanfp-dev — прямой доступ к сенсору для разработки; не устанавливается

      elanfp-dev info                           устройство и его конечные точки
      elanfp-dev probe                          прошивка, размер, калибровка
      elanfp-dev touch [sensor|frames]          одно касание выбранным способом ожидания
      elanfp-dev collect <метка> [количество]   кадры и динамика касаний в captures/dataset/<метка>
missing-argument = не хватает аргумента: { $what }
arg-finger = палец
arg-label = метка
error = ошибка: { $text }
reply-ok = ✓ { $text }
reply-no = ✗ { $text }
error-internal = внутренняя ошибка: { $detail }

ipc-closed = служба закрыла соединение
ipc-garbled = непонятный ответ службы
ipc-unavailable = служба elanfpd недоступна ({ $path }): { $detail }
runtime-dir-unset = XDG_RUNTIME_DIR не задан

sensor-not-found = сенсор 04f3:0c4b не найден
usb-timeout = сенсор не ответил вовремя (команда { $command })
usb-error = ошибка USB (команда { $command }): { $detail }
sensor-unsupported = непроверенный вариант сенсора { $vid }:{ $pid }, прошивка { $fw }, поколение { $gen }, размер { $dim }: драйвер нужно адаптировать
sensor-short-write = сенсор принял { $got } из { $expected } байт (команда { $command })
sensor-short-reply = сенсор ответил { $got } из { $expected } байт (команда { $command })

touch-lift = убери палец
touch-finger-on-sensor = на сенсоре палец — убери его
touch-stuck = на сенсоре всё время что-то лежит: убери палец и попробуй снова
reject-partial = палец лёг не на весь сенсор
reject-ridges = не видно линий (мокрый, грязный или слишком сильно прижат?)
reject-moving = палец двигался
quality = покрытие { $coverage }%, контраст { $contrast }, сдвиг { $motion }% (допустимо до { $limit }%)

region-0 = центр подушечки
region-1 = выше, ближе к кончику
region-2 = ниже, ближе к сгибу
region-3 = левая сторона
region-4 = правая сторона
region-5 = кончик пальца
region-6 = повернув палец влево
region-7 = повернув палец вправо
region-8 = как удобно, своим обычным движением

finger-name-invalid = имя пальца: латиница, цифры, - _ . (до 64 символов)
finger-exists = палец «{ $finger }» уже зарегистрирован; чтобы заменить, сначала удали его
finger-unknown = палец «{ $finger }» не зарегистрирован
enroll-intro = регистрация «{ $finger }»: сначала протри сканер, потом прикладывай один и тот же палец, каждый раз немного в другом месте. Закончится сама, когда подтвердится не меньше { $min } касаний и { $streak } подряд будут уверенно узнаны — то есть целый круг подсказок; предел — { $max } касаний
enroll-prompt = {"["}{ $n }] { $region } — приложи и держи неподвижно
touch-timeout-wait = не дождался касания, жду снова
touch-missed-retry = нет касания ({ $status }), ещё раз
touch-rejected-retry = не принято: { $reason } ({ $quality }), ещё раз
enroll-same-place = это касание легло почти туда же, что и многие прежние: сдвинь палец заметнее. Если палец уже двигался, значит, на сканере что-то есть — протри его
enroll-accepted = принято: { $quality }, точек { $points }; { $novelty ->
        [known] уже знакомый участок
       *[new] новый участок
    } (оценка { $score }); узнано подряд { $streak }/{ $need }, подтверждено { $kept } (нужно от { $min })
enroll-covered = палец покрыт: { $streak ->
        [one] { $streak } касание подряд узнано
        [few] { $streak } касания подряд узнаны
       *[many] { $streak } касаний подряд узнаны
    }
enroll-consistency = проверка согласованности за { $seconds } с: { $dropped ->
        [0] все касания согласованы
        [one] убрано { $dropped } касание, не похожее ни на одно другое (другой палец или край пальца)
        [few] убрано { $dropped } касания, не похожих ни на одно другое (другой палец или край пальца)
       *[many] убрано { $dropped } касаний, не похожих ни на одно другое (другой палец или край пальца)
    }
enroll-done = палец «{ $finger }» зарегистрирован: { $count ->
        [one] { $count } касание
        [few] { $count } касания
       *[many] { $count } касаний
    }, зашифровано { $algorithm }

no-fingers = нет зарегистрированных пальцев
verify-prompt = приложи палец к сканеру (попытка { $attempt }/{ $attempts })
verify-no-touch = не дождался касания
verify-bad-frame = кадр не годится: { $reason }
verify-match = узнан палец «{ $finger }»
verify-no-match = не узнан

creds-unavailable = ключ запечатан systemd-creds, а его нет ({ $detail })
creds-failed = systemd-creds { $action }: { $detail }
key-corrupt = главный ключ повреждён
home-unset = HOME не задан
key-lost = главный ключ { $path } пропал, а зашифрованные шаблоны остались: их больше нельзя прочитать. Удали *.papv и зарегистрируй пальцы заново
key-file-missing = нет ключа шаблонов { $path }: его создаёт установщик (systemd передаёт ключ через LoadCredentialEncrypted, без systemd это файл, путь можно задать в ELANFP_KEY_FILE)
key-file-exposed = файл ключа { $path } доступен кому-то кроме службы: нужны права 0400 или 0600 и владелец — служба или root
key-file-unreadable = файл ключа { $path } не читается: { $detail }
daemon-root = системная служба не работает от root: запусти её от пользователя elanfp (так делают готовые файлы для systemd, OpenRC и runit)
process-unprotected = не удалось запретить процессу дампы памяти и повышение прав: { $detail }
sleep-interrupted = компьютер засыпал или замирал, пока запрос ждал касания: запрос отменён — повтори
store-foreign-user = это хранилище не принадлежит пользователю { $uid }
template-unreadable = шаблон пальца «{ $finger }» не читается: { $detail }

service-own-user-only = эта служба работает только с данными своего пользователя
list-empty = зарегистрированных пальцев нет
list = { $fingers }
deleted = палец «{ $finger }» удалён
locked = отпечаток временно заблокирован после { $failures } неудач (ещё ~{ $minutes } мин) — используй пароль
user-no-fingers = у пользователя нет зарегистрированных пальцев
verified = палец «{ $finger }» узнан
not-verified = отпечаток не узнан
no-touch = касания не было
need-sudo = регистрировать и удалять пальцы и снимать блокировку может только root (запусти через sudo или doas)
foreign-fingers = нет доступа к пальцам другого пользователя
bad-request = непонятный запрос
client-gone = клиент отключился

log-started = elanfpd: { $mode ->
        [user] пользовательский
       *[system] системный
    } режим, сокет { $socket }, шифрование { $algorithm }
log-sleep-watch = сон компьютера во время запроса замечаем { $by ->
        [counter] по счётчику засыпаний ядра и по часам
       *[clock] только по часам: счётчик засыпаний ядра недоступен
    }
log-request-failed = запрос не обработан: { $detail }
log-enroll = регистрация: uid={ $uid } палец={ $finger } (запросил uid={ $peer })
log-delete = удаление: uid={ $uid } палец={ $finger } (запросил uid={ $peer })
log-verify-match = проверка: uid={ $uid } (запросил uid={ $peer }) — узнан { $finger }
log-verify-no-match = проверка: uid={ $uid } (запросил uid={ $peer }) — не узнан, неудач подряд { $count }
fatal = elanfpd: { $text }

info-device = { $vid }:{ $pid } шина { $bus } адрес { $address } — { $maker } / { $product }, скорость { $speed }
info-interface = интерфейс { $number } alt { $alt }: класс { $class }
info-endpoint = конечная точка { $address } { $direction } { $kind }, пакет { $size } байт
probe-firmware = прошивка: { $fw }
probe-generation = поколение (40 21 0a): { $gen }
probe-size = размер: { $width }×{ $height } (сырые байты { $raw })
probe-dac = DAC (40 21 06): { $dac }
probe-mean = среднее калибровки (40 24): { $mean }
probe-status = статус (40 13): { $status }
collect-prompt = {"["}{ $n }/{ $count }] «{ $label }»: { $region }, держи неподвижно
collect-accepted = принят: { $quality }
collect-rejected = кадр не принят: { $reason } ({ $quality }), ещё раз
collect-done = готово: { $count } кадров в { $dir }
dataset-listed = в списке { $kept } кадров, отброшено фильтром качества { $rejected }: { $file }
dataset-usage = использование: elanfp-dataset list | replay | stained <доля в процентах, 1–99> | dynamics
replay-stained = загрязнённый сканер: в каждый кадр подмешан { $file } с весом { $percent }%; чужие касания — каждый { $every }-й кадр

busy = сканер занят другим запросом — повтори чуть позже
service-failure = внутренняя ошибка службы (подробности в журнале: journalctl -u elanfpd)
already-running = служба уже запущена ({ $socket })
daemon-usage = использование: elanfpd [--user]
bad-count = количество должно быть числом, а не «{ $value }»
too-many-fingers = уже зарегистрировано { $limit } пальцев — это предел; сначала удали лишний
enroll-abandoned = регистрация прервана: слишком долго не было касаний
enroll-limit = достигнут предел в { $max } касаний
touch-test-prompt = приложи палец — жду до { $seconds } с ({ $mode ->
        [frames] палец ищу опросом кадров
       *[sensor] палец сообщает сам сенсор
    })
arg-touch-mode = способ: sensor или frames
transport-without-timeouts = этот USB-транспорт не умеет прерывать ожидание: палец можно искать только опросом кадров
scheme-error = схема { $scheme }: { $detail }
daemon-usage-redox = использование: elanfpd <схема-xhci> <порт> <интерфейс> (запускает xhcid по /usr/lib/xhcid.d/elanfp.toml)
log-started-redox = elanfpd: { $path }, контроллер { $controller }, порт { $port }, прошивка сенсора { $firmware }, шифрование { $algorithm }
usage-redox =
    elanfp — открытый драйвер сканера отпечатков ELAN 04f3:0c4b

      elanfp enroll <палец> [--uid N]   зарегистрировать палец (только root)
      elanfp verify [палец] [--uid N]   проверить касание
      elanfp list [--uid N]             зарегистрированные пальцы
      elanfp delete <палец> [--uid N]   удалить палец (только root)
      elanfp unlock [--uid N]           снять блокировку после неудачных попыток (только root)

    Имя пальца: латинские буквы, цифры, «-», «_», «.»
arg-uid = номер пользователя после --uid
bad-uid = номер пользователя должен быть числом, а не «{ $value }»
unexpected-argument = лишний аргумент: { $value }
enroll-unusable = регистрация прервана: { $count } негодных касаний подряд не дали ни одного хорошего кадра — вытри палец и сенсор и начни заново
log-template-skipped = шаблон пропущен: uid={ $uid } палец={ $finger }: { $detail }
log-adapt-failed = дообучение не сохранено: uid={ $uid } палец={ $finger }: { $detail }
log-lockout = блокировка: uid={ $uid } на { $minutes } мин после неудачных сравнений
log-lockout-unsaved = счётчик неудач не записан на диск: uid={ $uid }: { $detail }
usb-transfer-failed = передача не удалась: { $cause }
usb-endpoint-unavailable = конечная точка недоступна: { $cause }
usb-stalled = конечная точка остановлена устройством (STALL)
usb-xhcid-failed = xhcid: { $cause }
usb-transfer-unknown = xhcid сообщил о сбое передачи без подробностей
usb-endpoint-missing = конечная точка { $endpoint } не открыта
usb-transfer-too-large = передача { $bytes } байт больше предела { $limit }
sensor-ambiguous = найдено несколько встроенных устройств 04f3:0c4b — драйвер не выбирает наугад
sensor-not-built-in = устройство 04f3:0c4b подключено к внешнему порту: драйвер работает только со встроенным сканером
uid-required = укажи, чей это палец: --uid <номер пользователя> (команда запущена от root, но не прямо из sudo или doas)
acting-for = для учётной записи { $name } (uid { $uid })
dataset-set-name = имя набора должно быть вида владелец-палец-условие: { $path }
dataset-no-base = в наборе нет кадра фона base-*: { $path }
contended = пока ждали твой палец, отпечаток запросила ещё одна программа — запрос отменён; используй пароль и проверь, что ещё просило отпечаток
quiet = отпечаток временно выключен (ещё ~{ $seconds } с): на прошлые запросы никто не приложил палец или они мешали друг другу — используй пароль
log-contended = два запроса проверки одновременно для uid={ $uid }: ожидавший запрос (от uid={ $peer }) отменён, отпечаток на время выключен
dataset-bad-frame = кадр не читается или имеет не тот размер: { $path }
replay-enrolled = { $finger }: регистрация за { $touches } касаний, принято { $kept }, отброшено { $dropped }; несвязанных областей { $groups }, крупнейшая { $largest }; касаний с тем же неподвижным рисунком, что у многих прежних, { $still }
replay-failed = { $finger }: регистрация не удалась за { $touches } касаний — { $reason }
replay-verified = { $finger }: свои касания { $probes }, не узнано с первого раза { $missed } ({ $percent }%), дообучено { $learned }, шаблонов стало { $templates }; чужие касания { $impostors }, принято { $accepted }, наибольшая оценка { $top } при пороге { $threshold }
fprint-started = elanfp-fprintd: имя { $name } на системной шине, служба { $socket }
fprint-bus = не удалось занять имя на шине D-Bus: { $detail }
fprint-bad-setting = непонятное значение { $name }={ $value }
fprint-not-claimed = устройство не занято этим клиентом: сначала Claim
fprint-in-use = сканер уже занят другим клиентом или другой операцией
fprint-denied = нет прав на эту операцию
fprint-internal = внутренняя ошибка моста
fprint-unknown-user = нет такого пользователя
fprint-no-prints = у пользователя нет зарегистрированных пальцев
fprint-no-action = нечего останавливать: операция не запущена
fprint-bad-finger = непонятное имя пальца
fprint-not-deleted = не все отпечатки удалось удалить
fprint-audit-enroll = uid { $caller } записывает { $fingers } для uid { $target }
fprint-audit-delete = uid { $caller } удаляет { $fingers } у uid { $target }
log-touch-dynamics = динамика касания: uid={ $uid } кадров={ $frames } до устойчивого кадра { $settle } мс, удержание { $held } мс, рост площади { $rise }, изменение контраста { $drift }, движение { $motion }
dynamics-set = { $set }: касаний { $touches }; медианы — кадров { $frames }, до устойчивого кадра { $settle } мс, удержание { $held } мс, рост площади { $rise }, изменение контраста { $drift }, движение { $motion }
fprint-bus-lost = соединение с шиной D-Bus потеряно
sensor-port-unknown = прошивка этого компьютера не сообщает, что сканер стоит на внутреннем порту, поэтому он не используется: внешнее устройство с тем же ID от него не отличить
peer-unknown = эта учётная запись системе неизвестна
usb-speed-low = низкая (1,5 Мбит/с)
usb-speed-full = полная (12 Мбит/с)
usb-speed-high = высокая (480 Мбит/с)
usb-speed-super = SuperSpeed (5 Гбит/с)
usb-speed-super-plus = SuperSpeed+ (10 Гбит/с)
usb-speed-unknown = неизвестна
usb-direction-in = вход
usb-direction-out = выход
usb-kind-control = управляющая
usb-kind-isochronous = изохронная
usb-kind-bulk = потоковая
usb-kind-interrupt = прерывания
attempt-unrecorded = отпечаток сейчас недоступен: служба не может записать попытку на диск — используй пароль
unlocked = блокировка отпечатка снята
enroll-too-long = регистрация остановлена: она длится дольше { $minutes } мин — начни заново
log-verify-score = оценка сравнения: uid={ $uid } { $score }
log-verify-idle = проверка: uid={ $uid } — запрос от root остался без касания, таких подряд { $count }
log-refused-locked = проверка: uid={ $uid } — отказано, блокировка ещё ~{ $minutes } мин
log-refused-quiet = проверка: uid={ $uid } — отказано, пауза ещё ~{ $seconds } с
log-benched = проверка: uid={ $uid } — запрос остался без касания, пока сканера ждал запрос для uid={ $by }; следующие запросы для uid={ $uid } подождут ~{ $seconds } с
watch-verify = запрошена проверка отпечатка { $by ->
        [root] программой с правами root
       *[self] от твоей учётной записи
    }{ $route ->
        [bridge] {" "}через мост рабочего стола
       *[direct] {""}
    }{ $pid ->
        [0] {""}
       *[other] , процесс { $pid }
    }, служба PAM: { $program }
watch-enroll = идёт регистрация пальца для этой учётной записи{ $route ->
        [bridge] {" "}через мост рабочего стола
       *[direct] {""}
    }{ $pid ->
        [0] {""}
       *[other] , процесс { $pid }
    }
watch-over = запрос закончился
watch-idle = запросов нет
watch-closed = наблюдение за запросами окончено
watch-crowded = за запросами этой учётной записи уже следит слишком много программ
watch-unsupported = наблюдение за запросами здесь не поддерживается
notify-app = Сканер отпечатков
notify-verify-title = Запрошен отпечаток
notify-who = Отпечаток просит «{ $program }» — программа с правами root.
notify-who-unnamed = Отпечаток просит программа с правами root.
notify-how = Запущена так (это её собственные слова): «{ $command }»
notify-how-cut = Запущена так (это её собственные слова, показано только начало): «{ $command }…»
notify-via-bridge = Запрос пришёл через мост рабочего стола.
notify-advice = Если это не твоя команда — не касайся сканера.
notify-enroll-title = Регистрация пальца
notify-enroll = Для твоей учётной записи регистрируют палец. Если это делаешь не ты — проверь, кто работает за компьютером.
notify-name-taken = имя { $name } на шине сеанса занято другой программой; уведомления всё равно будут показаны
notify-no-bus = нет шины сеанса рабочего стола: { $detail }
notify-bus-lost = шина сеанса рабочего стола закрылась
notify-fatal = elanfp-notify: { $text }
log-delegated = запрос через мост uid={ $delegate } от имени uid={ $peer }: { $verb } uid={ $uid }
log-unlock = блокировка снята: uid={ $uid } (запросил uid={ $peer })
fprint-no-service-account = учётной записи службы «{ $name }» нет в системе
ipc-untrusted = на { $path } отвечает не служба elanfp и не root — запрос не отправлен
bad-setting = неизвестное значение { $name }={ $value }
sensor-layout-unverified = у устройства 04f3:0c4b не те конечные точки USB, что у проверенного сканера: драйвер с ним не работает
usb-link-dead = после сбоя USB порт сканера больше не отвечает: в Redox его оживляет только перезагрузка
