# Rebox

**Rebox** — инструмент на **Rust** для iPhone с **Dopamine rootless** и **TrollStore**, отслеживающий срок действия provisioning-профилей приложений и автоматически продлевающий их через Apple Developer.

## Что внутри

- **iOS-приложение `Rebox.app`** (`rebox-ui`) — интерфейс статуса профилей, состояния сессии Apple ID, фоновой службы и ручного запуска продления.
- **`rebox-controller`** — контроллер обнаружения установленных профилей, проверки сроков, синхронизации сессии из Sidebox и автоматического продления.
- **`rebox-apple`** — клиент Apple Developer / Anisette (`authorize`, `fetch`, `session-status`).
- **`profilectl`** — чтение, декодирование и установка provisioning-профилей на устройстве.
- **LaunchDaemon `com.bromhobit.rebox`** — фоновая проверка каждые 6 часов в rootless-сборке.

Контроллер также поддерживает одностороннюю безопасную синхронизацию сессии Apple ID и Anisette-состояния из установленного **Sidebox** (если не используется отдельная автономная сессия Rebox).

## Скачать

Готовые сборки находятся в [GitHub Releases](https://github.com/bromhobit/Rebox/releases).

| Устройство / способ | Файл | Назначение |
|---|---|---|
| iPhone с **Dopamine rootless** | `Rebox-1.0.1-rootless.deb` | Установка через Sileo / Zebra; содержит `Rebox.app`, утилиты и LaunchDaemon |
| iPhone с **TrollStore** | `Rebox-1.0.1.ipa` | Установка `Rebox.app` со встроенными утилитами |

Обычная установка IPA через бесплатный Apple ID не даёт необходимых привилегий (в отличие от TrollStore или rootless-джейлбрейка). Это **не Linux-приложение** — AppImage, DEB для Ubuntu и пакеты pacman здесь неприменимы.

## Идентификаторы и пути на iPhone

- **Bundle ID приложения:** `com.bromhobit.Rebox`
- **Пакет Debian (rootless):** `com.bromhobit.rebox` (`Architecture: iphoneos-arm64`)
- **Приложение:** `/var/jb/Applications/Rebox.app` (бинарник `Rebox`)
- **Вспомогательные бинарники:** `/var/jb/usr/libexec/rebox/` (`rebox-controller`, `rebox-apple`, `profilectl`)
- **LaunchDaemon:** `/var/jb/Library/LaunchDaemons/com.bromhobit.rebox.plist` (label `com.bromhobit.rebox`)
- **Данные и конфигурация:** `/var/mobile/Library/Rebox/` (`config.plist`, `apple-session.plist`, `anisette_state`, `daemon.log`)

### Обновление со старого пакета `org.codex.dopaminerenew`

Пакет `com.bromhobit.rebox` объявляет `Conflicts: org.codex.dopaminerenew` и `Replaces: org.codex.dopaminerenew`:

- При установке нового `.deb` старый пакет `org.codex.dopaminerenew` и служба `org.codex.dopamine-renew` автоматически заменяются.
- При первом запуске контроллера и в `postinst` разрешённые файлы конфигурации и сессии (`config.plist`, `apple-session.plist`, `anisette_state`, `.sidebox-anisette-url`, `.sidebox-session-link`) автоматически и неразрушающим образом переносятся из `/var/mobile/Library/DopamineRenew/` в `/var/mobile/Library/Rebox/`, если в новом каталоге их ещё нет.

### Удаление в Sileo и работа с локальным `.deb`

В версии 1.0.0 скрипты `postinst`, `prerm` и `postrm` используют rootless-интерпретатор `#!/var/jb/bin/sh`, корректно выгружают LaunchDaemon, обновляют `uicache` / протокол `SILEO` и не блокируют `dpkg` при отсутствии или повторном удалении, а в пакет включён `md5sums`.

> **Примечание по особенностям Sileo при установке локального `.deb`:** если вы открыли локальный `.deb` через «Поделиться → Sileo» и пытаетесь удалить его в той же сессии Sileo, интерфейс Sileo может выдать `APT was unable to find this package` (кэш APT не индексирует локальные `.deb` после установки). В этом случае достаточно закрыть и заново открыть Sileo (пакет появится во вкладке «Установленные» из `/var/jb/Library/dpkg/status` и удалится штатно) или выполнить в терминале:
>
> ```sh
> dpkg -r com.bromhobit.rebox
> ```
>
> Если на устройстве ранее завис старый пакет `org.codex.dopaminerenew`, очистите его принудительно перед установкой `1.0.0`:
>
> ```sh
> dpkg --purge --force-remove-reinstreq org.codex.dopaminerenew
> ```

## Безопасность

Исходные коды, release-пакеты и диагностические сообщения не содержат Apple-токенов, pairing-записей, паролей или provisioning-профилей пользователя.

## Сборка на Linux

Понадобятся Rust с целью `aarch64-apple-ios`, Clang/LLVM, `ld64.lld` и совместимый iPhoneOS SDK (`IPHONEOS_DEPLOYMENT_TARGET=15.0`, `-Wl,-platform_version,ios,15.0,16.4`). Сначала соберите ARM64 iOS Mach-O для четырёх компонентов: `rebox-ui` (`ios-app/Cargo.toml`), `rebox-controller` (`controller/Cargo.toml`), `profilectl` (`ondevice/Cargo.toml`), `rebox-apple` (`apple/Cargo.toml`).

После компиляции:

```sh
python3 packaging/build_release.py \
  --binary-dir /absolute/path/to/ios-binaries \
  --output dist --version 1.0.1 --force
(cd dist && sha256sum -c SHA256SUMS)
```

Проверяйте `LC_BUILD_VERSION` (`platform ios`, `minos 15.0`) через `llvm-objdump --macho --private-headers`.

## Проверки

```sh
cargo test --locked --manifest-path controller/Cargo.toml
cargo test --locked --manifest-path apple/Cargo.toml
cargo test --locked --manifest-path ondevice/Cargo.toml
cargo test --locked --manifest-path ios-app/Cargo.toml
cargo test --locked --manifest-path Cargo.toml
python3 package/codesign.py --self-test
python3 -m unittest discover -s packaging/tests -v
bash -n package/setup-device.sh
```
