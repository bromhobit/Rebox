## Rebox 1.0.1

Обновление совместимости с **Sidebox 1.0.0** (`com.bromhobit.Sidebox`).

### Что изменилось
- **Поддержка нового и старого Bundle ID Sidebox (`sidebox_bridge`):**
  - Мост синхронизации сессии Apple ID и Anisette теперь автоматически распознаёт контейнеры нового семейства `com.bromhobit.Sidebox` / `com.bromhobit.Sidebox.<TEAM_ID>` и сохраняет полную обратную совместимость с прежним `org.codex.SideBox` / `org.codex.SideBox.<TEAM_ID>`.
  - При одновременной установке обеих версий приоритет отдаётся `com.bromhobit.Sidebox*` (с плавным fallback на legacy-контейнер, пока пользователь не вошёл в новом Sidebox).
  - Контейнеры расширений (например, `com.bromhobit.Sidebox.<TEAM>.Widget`) надёжно игнорируются.
- **Сборка и пакеты 1.0.1:**
  - Все бинарники (`rebox-ui`, `rebox-controller`, `rebox-apple`, `profilectl`), `.deb` и `.ipa` собраны с версией `1.0.1`.

### Артефакты и SHA-256
- `Rebox-1.0.1-rootless.deb` — rootless-пакет для iPhone с Dopamine (`iphoneos-arm64`), установка через Sileo / Zebra / `dpkg`.
- `Rebox-1.0.1.ipa` — IPA для установки через TrollStore.
- `SHA256SUMS` — контрольные суммы SHA-256.
