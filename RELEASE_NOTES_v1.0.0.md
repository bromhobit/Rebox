## Rebox 1.0.0

Финальный релиз **Rebox 1.0.0** с полным переименованием всех внутренних компонентов, идентификаторов, путей и бинарников, а также исправлением жизненного цикла rootless `.deb`-пакета в Sileo/dpkg.

### Что изменилось

- **Полное переименование в Rebox (`com.bromhobit.rebox` / `com.bromhobit.Rebox`):**
  - iOS-приложение: `/var/jb/Applications/Rebox.app/Rebox`, `CFBundleIdentifier = com.bromhobit.Rebox`, `CFBundleExecutable = Rebox`, классы UIKit `ReboxDelegate` и `ReboxController`.
  - Вспомогательные бинарники и крейты: `rebox-ui`, `rebox-controller`, `rebox-apple`, `profilectl`, `rebox` в `/var/jb/usr/libexec/rebox/`.
  - LaunchDaemon: `/var/jb/Library/LaunchDaemons/com.bromhobit.rebox.plist` (`Label = com.bromhobit.rebox`).
  - Каталог данных: `/var/mobile/Library/Rebox/`.
  - Интеграция с Sidebox сохранена без изменений.
- **Автоматическая миграция и замена старого пакета:**
  - Пакет объявляет `Conflicts: org.codex.dopaminerenew` и `Replaces: org.codex.dopaminerenew`.
  - `postinst` выгружает старую службу `org.codex.dopamine-renew`, а `postinst` и `rebox-controller` неразрушающе переносят `config.plist`, `apple-session.plist`, `anisette_state`, `.sidebox-anisette-url` и `.sidebox-session-link` из `/var/mobile/Library/DopamineRenew/` в `/var/mobile/Library/Rebox/`, не перезаписывая существующие файлы.
- **Исправление rootless `.deb` для Sileo / dpkg:**
  - Во всех скриптах (`postinst`, `prerm`, `postrm`) используется rootless-интерпретатор `#!/var/jb/bin/sh`.
  - Убрана жёсткая остановка `set -e`, все вызовы `launchctl` и `uicache` защищены от ложных ошибок и всегда завершаются `exit 0`.
  - Добавлена поддержка Sileo control fd (`SILEO` -> `finish:uicache`), добавлен скрипт `postrm` и контрольные суммы `md5sums` в `control.tar.gz`.
  - В `control` добавлены корректные поля `Maintainer`, `Author`, `Homepage`, `Conflicts`, `Replaces`.

### Файлы релиза

- `Rebox-1.0.0-rootless.deb` — rootless-пакет для iPhone с Dopamine (`iphoneos-arm64`), установка через Sileo / Zebra / `dpkg`.
- `Rebox-1.0.0.ipa` — IPA для установки через TrollStore.
- `SHA256SUMS` — контрольные суммы SHA-256.

> При переходе со старой застрявшей установки `org.codex.dopaminerenew` при необходимости выполните в терминале на устройстве `dpkg --purge --force-remove-reinstreq org.codex.dopaminerenew`. Если Sileo при попытке удалить локально установленный `.deb` в той же сессии пишет `APT was unable to find this package`, перезапустите Sileo (чтобы список прочитался из `status`) или удалите пакет командой `dpkg -r com.bromhobit.rebox`.
