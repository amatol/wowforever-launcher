# Сборка лаунчера

## GitHub Actions

Workflow: `.github/workflows/launcher.yml` (Windows only).

1. Запушьте репозиторий в `https://github.com/amatol/wowforever-launcher`.
2. Откройте **Actions → Launcher → Run workflow**.
3. Укажите версию, например `1.0.0`, и запустите.
4. Через ~10–15 минут появится релиз `v1.0.0` с NSIS-установщиком.

Публичный CA уже лежит в `app/src-tauri/resources/forever_root_ca.crt`.
Переменные репозитория не обязательны: адрес, мир и сайт заданы по умолчанию
в `app/src-tauri/src/config.rs`.

## Локальная сборка (Windows)

```powershell
cd app
npx --yes @tauri-apps/cli@2 build --bundles nsis
```

Требуется Rust stable, Node.js 22 и WebView2 (в Windows 10/11 есть).

## Проверка

- `cargo test --lib` в `app/src-tauri`.
- Установить лаунчер на чистую Windows, выбрать `_forever_` с `Wow.exe`.
- Нажать «Подготовить ПК», войти аккаунтом из бота, нажать «Играть».
- Проверить `hosts`, «Доверенные корневые сертификаты», вход в мир.
