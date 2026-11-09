# Roadmap лаунчера

## Текущий этап

- Windows-лаунчер Forever AI (Tauri, NSIS).
- hosts + публичный CA + LoginREST + автовход + ключ сервера.
- Новости из `https://forever.mmolabs.ru/launcher/news.json`.

## Следующий этап: SteamOS / Steam Deck

Отдельная упрощённая сборка **AppImage** для SteamOS.

Требования:

- запускается как стороннее приложение в Steam Game Mode;
- фиксированный сервер `forever.mmolabs.ru`, без лишних настроек;
- при старте молча проверяет и обновляет клиент;
- запускает обычную Windows-версию `Wow.exe` через Proton;
- сам устанавливает CA и hosts/portal в Proton-префикс;
- автовход через сохранённый токен или логин при первом запуске;
- крупные кнопки/шрифт для геймпада и телевизора;
- не требует установки Node.js, Rust, Wine или Python на Steam Deck.

Основа: Linux-часть текущего Tauri-лаунчера (`game.rs`, `system.rs`,
`dpapi::wine`, `patcher/lin.rs`), но с урезанным UI и без выбора Proton —
используется Proton из Steam.

Статус: **отложено до запуска основного сервера**.
