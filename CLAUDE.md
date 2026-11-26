# WinRust (plugin-launcher)

Установщик `plugin-launcher.exe` ставит и удаляет плагины, которые заменяют части Windows. Для пользователя: `README.md`, планы: `ROADMAP.md`.

## ⚠ Главная ловушка: MSIX и HKCU

Приложение Claude упаковано в MSIX. Всё, что запущено из его оболочки (даже через UAC), пишет HKCU в личную копию реестра пакета, которую настоящий сеанс не видит (так терялись Run-ключи, `ReplaceExplorer`, привязка папок). HKLM пишется по-настоящему.
- Установщик (`install`, `update`) запускай **только через explorer.exe** (ярлык или `explorer.exe <bat>`). `update-plugin.ps1` уже делает это сам.
- `reg query HKCU\...` из своей оболочки показывает копию пакета, а не правду. Настоящие значения смотри батником, запущенным через `explorer.exe <bat>`.

## Где что лежит

| Путь | Что |
|---|---|
| `src/main.rs` | таблица `PLUGINS` и общее состояние |
| `src/install/` | что каждый плагин меняет в системе и как откатывается (файл на плагин; `mod.rs`: общие хелперы, payload, `install_plugin`/`rollback`) |
| `src/cli.rs`, `src/ui.rs` | командная строка и окно |
| `common/` | `winrust-common`: стили, разбор вывода reg.exe и `mft` (чтение MFT) для всех программ |
| `plugins/<имя>/`, `ReShell/` | исходники плагинов (это и есть оригиналы: копий на рабочем столе больше нет, правь здесь) |
| `payload.txt` | единственный список вшиваемых exe: из него читают `build.rs` и `scripts\build-plugins.ps1` |

Один cargo workspace, все exe в `target\release`. `plugins/rust-search` и `plugins/chrome-rs` вне workspace (им нужен `panic = "unwind"`), у них свой `target\`.

## Сборка и проверка

- Полная сборка: `powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1` (плагины → `payload\` → установщик).
- Только плагины: `scripts\build-plugins.ps1`. Только установщик: `cargo build --release -p plugin-launcher` (нужен заполненный `payload\`).
- Тесты: `cargo test --workspace` и `cargo test` в `plugins\rust-search`. У самого установщика тестового exe нет (`test = false`: манифест требует админа).
- Интерфейс всех программ только на английском: тексты пиши прямо строками (макросов `t!`/`tf!`/`en!` и переключателя языка больше нет).

## Обновить установленный плагин после правки (без пересборки установщика)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\update-plugin.ps1 <rust-search|rtaskmgr-win32|topbar|rust-explorer|wiztree|photoview|rust-settings|debloat>
```

Скрипт пересобирает плагины, обновляет `payload\` и запускает `plugin-launcher update <плагин>` через explorer.exe. Появится окно UAC: **«Да» нажимает пользователь**. Не обходи UAC и не меняй его настройки.

После запуска:
1. Скрипт печатает хвост `%ProgramData%\plugin-launcher\install.log` (UTF-8, читай с `-Encoding UTF8`). Успех: `<плагин>: updated` (`обновлён`), предупреждения после `|`. Ошибка: `ERROR ...` (`ОШИБКА ...`).
2. Проверь, что процесс запущен (`Get-Process topbar,reshell` и т. п.).
3. Нет `updated`/`обновлён` или есть ошибка: покажи пользователю текст ошибки, сам ничего не удаляй и не чини реестр.

## Правила

- `update` только для уже установленного плагина. `install <имя> --yes`, `uninstall`, `rollback` **только по прямой просьбе пользователя** (rust-search отключает поиск Windows, topbar прячет панель задач, rust-explorer отключает Проводник).
- Код установки (`src/install/`) ни разу не прогонялся целиком на живой системе: тестируй осторожно, по одному плагину.
- Новый плагин: crate в `plugins/` + `members` в `Cargo.toml`, строка в `payload.txt`, файл в `src/install/`, запись в `PLUGINS`. Перед изменением системы сохраняй исходные значения, чтобы uninstall их вернул.

## Публикация на GitHub (наш форк `selim-durmus/WinRust`, ветка `main`)

Это наш собственный проект на форке. `origin` = `https://github.com/selim-durmus/WinRust.git`, аккаунт gh `selim-durmus`: **всё** (коммиты, ветки, теги, релизы) идёт только туда.
`upstream` = `Lemxy/WinRust`, исходный репозиторий, из которого сделан форк. Он не наш: туда ничего не отправлять (ни push, ни PR, ни релизы, ни issues); пуш в `upstream` выключен (`git remote set-url --push upstream no_push`).

**Как публиковать.** Готовые изменения сразу коммитить и пушить в `main` одним шагом: `scripts\publish.ps1 -Message "..."`. Значит, **код должен собираться и проходить тесты к концу каждого ответа**. Скрипт сам отказывается пушить (и снимает индекс), если `origin` не наш форк, если в изменениях `.exe/.dll/.pdb/.log/.reg/ключи`, `target/`, файлы больше 5 МБ или похожее на токен. CI (`.github/workflows/build.yml`) собирает и тестирует каждый пуш.

**Коммиты.**
- Никаких `Co-Authored-By` и упоминаний Claude в коммитах, PR и релизах (как во всей истории форка). Это важнее общих шаблонов атрибуции.
- Автор: `selim-durmus` с адресом `noreply` GitHub (уже в git config). Настоящую почту владельца в репозиторий не писать.
- `git push --force`, удаление репозитория/тегов/релизов и смена видимости только по прямой просьбе.
- Не коммитить `payload\*.exe`, `target\`, логи (они в `.gitignore`).

**Документация.**
- `README.md` (англ.) и `README.ru.md` (рус.) править одинаково. Так же `ROADMAP.md` / `ROADMAP.ru.md`.
- Бенчмарк в README обновлять только после реального нового замера (`scripts\bench.ps1`), цифры не придумывать.
- Оговорку про неподписанный exe и SmartScreen не убирать, пока нет подписи.
- Описание, темы и главная страница репозитория (`gh repo edit`) только по просьбе владельца.

**Релиз.** `scripts\release.ps1 v0.2.0 [-Notes "..."]`: `build-all.ps1`, пуш, тег, GitHub Release с `WinRust.exe`. Только когда владелец просит. Перед релизом проверить, что README и ROADMAP не устарели.

**Если пуш не прошёл:** показать ошибку владельцу, не чинить принудительным пушем.
