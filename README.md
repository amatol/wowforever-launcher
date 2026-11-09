# Forever AI Launcher

Лаунчер для сервера **Forever AI** (WoW Forever, TrinityCore forever).

Репозиторий основан на открытом launcher из проекта
[`jibay442/forever_docker`](https://github.com/jibay442/forever_docker) (GPL-2.0).
Он настраивает hosts и сертификат, выполняет вход через LoginREST bnetserver,
подставляет ключ сервера и запускает `Wow.exe`.

## Что делает

1. Находит папку `_forever_` с `Wow.exe`.
2. Добавляет `92.42.44.9 trinity.actual.battle.net` в hosts (нужны права
   администратора).
3. Устанавливает публичный CA `forever_root_ca.crt` в доверенные.
4. Логинится на `https://trinity.actual.battle.net:8081` по email и паролю.
5. Запоминает сессию на 30 дней.
6. Подставляет ключ сервера в память клиента и запускает игру.

## Сборка

GitHub Actions, Windows only: `.github/workflows/launcher.yml`.
Результат — NSIS-установщик и `forever-ai-launcher.exe`.

Локально (Windows):

```powershell
cd app
npm install -g @tauri-apps/cli@2
npx @tauri-apps/cli@2 build --bundles nsis
```

## Настройки

По умолчанию:

- адрес: `92.42.44.9`
- портал: `trinity.actual.battle.net`
- сайт/новости: `https://forever.mmolabs.ru/launcher`
- мир: `Дримворлд`

Их можно изменить в меню «Настройки → Сервер».

## Публичный CA

`app/src-tauri/resources/forever_root_ca.crt` — только публичный сертификат.
Приватный ключ CA хранится на сервере в `/root/wowserver/certs/ca/` и никогда
не публикуется.
