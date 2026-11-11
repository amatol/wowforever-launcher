//! Messages shown to the player (steps, errors, journal) in French, English, Spanish and Portuguese (Latin America).
//! The UI sends the language it uses (`set_language`); English is the fallback.

use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};

static LANG: AtomicUsize = AtomicUsize::new(1);

/// ru, en, es, pt (anything else: en).
pub fn set(lang: &str) {
    let i = match lang.get(..2).unwrap_or("").to_ascii_lowercase().as_str() {
        "ru" => 0,
        "es" => 2,
        "pt" => 3,
        _ => 1,
    };
    LANG.store(i, Ordering::Relaxed);
}

/// The message in the current language.
pub fn t(key: &str) -> &'static str {
    let texts = MESSAGES.iter().find(|(k, _)| *k == key).map(|(_, v)| v).unwrap_or(&[""; 4]);
    texts[LANG.load(Ordering::Relaxed)]
}

/// The message with each `{}` replaced by the next argument.
pub fn tf(key: &str, args: &[&dyn Display]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut parts = t(key).split("{}").peekable();
    while let Some(part) = parts.next() {
        out.push_str(part);
        if parts.peek().is_some() {
            if let Some(a) = args.next() {
                out.push_str(&a.to_string());
            }
        }
    }
    out
}

type Texts = [&'static str; 4];

#[rustfmt::skip]
const MESSAGES: &[(&str, Texts)] = &[
    // launch steps
    ("step_connecting", ["Подключение к серверу…", "Connecting to the server…", "Conectando con el servidor…", "Conectando ao servidor…"]),
    ("step_preparing", ["Подготовка игры…", "Preparing the game…", "Preparando el juego…", "Preparando o jogo…"]),
    ("step_launching", ["Запуск World of Warcraft…", "Starting World of Warcraft…", "Iniciando World of Warcraft…", "Iniciando World of Warcraft…"]),
    ("running", ["Игра запущена: войдите в мир", "Game started: log in to the realm", "Juego iniciado: entra al reino", "Jogo iniciado: entre no reino"]),
    ("ready", ["Ключ сервера установлен: входите в мир", "Server key in place: enter the realm", "Clave del servidor lista: entra al reino", "Chave do servidor pronta: entre no reino"]),
    ("closed", ["Игра закрыта", "The game is closed", "El juego se cerró", "O jogo foi fechado"]),
    ("not_started", ["Игра не запустилась (Wow.exe не найден за 3 минуты)", "The game did not start (no Wow.exe found within 3 minutes)", "El juego no arrancó (no se encontró Wow.exe en 3 minutos)", "O jogo não iniciou (nenhum Wow.exe encontrado em 3 minutos)"]),
    ("mac_copying", ["Копирование игры для Дримворлд Навсегда (первый раз или после обновления)…", "Copying the game for Dreamworld Forever (first time or after an update)…", "Copiando el juego para Forever (primera vez o tras una actualización)…", "Copiando o jogo para o Forever (primeira vez ou após uma atualização)…"]),
    ("mac_signing", ["Подпись копии игры…", "Signing the game copy…", "Firmando la copia del juego…", "Assinando a cópia do jogo…"]),
    // launch errors
    ("already_running", ["игра уже запущена", "the game is already running", "el juego ya está abierto", "o jogo já está aberto"]),
    ("game_not_found", ["игра не найдена: выберите папку _forever_ в настройках", "game not found: choose the _forever_ folder in the settings", "juego no encontrado: elige la carpeta _forever_ en los ajustes", "jogo não encontrado: escolha a pasta _forever_ nas configurações"]),
    ("login_first", ["сначала войдите", "log in first", "inicia sesión primero", "entre na sua conta primeiro"]),
    ("session_expired", ["сессия истекла: войдите снова", "session expired: log in again", "sesión caducada: vuelve a iniciar sesión", "sessão expirada: entre novamente"]),
    ("session_save", ["сохранение сессии: {}", "saving the session: {}", "guardando la sesión: {}", "salvando a sessão: {}"]),
    ("launch_failed", ["запуск {}: {}", "starting {}: {}", "iniciando {}: {}", "iniciando {}: {}"]),
    ("wtf_folder", ["папка WTF: {}", "WTF folder: {}", "carpeta WTF: {}", "pasta WTF: {}"]),
    ("registry", ["реестр: {}", "registry: {}", "registro: {}", "registro: {}"]),
    ("mac_failed", ["{} не удалось: {}", "{} failed: {}", "{} falló: {}", "{} falhou: {}"]),
    ("mac_not_started", ["игра не запустилась (open: {})", "the game could not start (open: {})", "el juego no pudo arrancar (open: {})", "o jogo não pôde iniciar (open: {})"]),
    ("mac_module", ["модуль Дримворлд Навсегда не загрузился в игру", "the Dreamworld Forever module was not loaded into the game", "el módulo Forever no se cargó en el juego", "o módulo Forever não foi carregado no jogo"]),
    ("mac_key", ["не удалось получить ключ тикета", "could not derive the ticket key", "no se pudo derivar la clave del ticket", "não foi possível derivar a chave do ticket"]),
    ("mac_encrypt", ["не удалось зашифровать тикет ({})", "could not encrypt the ticket ({})", "no se pudo cifrar el ticket ({})", "não foi possível criptografar o ticket ({})"]),
    ("mac_user", ["неизвестное имя пользователя", "unknown user name", "nombre de usuario desconocido", "nome de usuário desconhecido"]),
    // server API
    ("address_not_found", ["адрес сервера не найден ({})", "server address not found ({})", "dirección del servidor no encontrada ({})", "endereço do servidor não encontrado ({})"]),
    ("unexpected_answer", ["неожиданный ответ сервера", "unexpected answer from the server", "respuesta inesperada del servidor", "resposta inesperada do servidor"]),
    ("authenticator", ["аккаунт использует аутентификатор, лаунчер его не поддерживает", "this account uses an authenticator, which the launcher does not support", "esta cuenta usa un autenticador, que el launcher no admite", "esta conta usa um autenticador, que o launcher não suporta"]),
    ("bad_login", ["неверная почта или пароль", "wrong e-mail or password", "correo o contraseña incorrectos", "e-mail ou senha incorretos"]),
    ("remember_refused", ["сервер отклонил автовход (HTTP {})", "the server refuses automatic login (HTTP {})", "el servidor rechaza el inicio de sesión automático (HTTP {})", "o servidor recusa o login automático (HTTP {})"]),
    ("banned", ["аккаунт заблокирован", "account banned", "cuenta suspendida", "conta banida"]),
    ("server_error", ["ошибка сервера (HTTP {})", "server error (HTTP {})", "error del servidor (HTTP {})", "erro do servidor (HTTP {})"]),
    ("unreachable", ["сервер недоступен", "server unreachable", "servidor inaccesible", "servidor inacessível"]),
    ("network_error", ["сетевая ошибка: {}", "network error: {}", "error de red: {}", "erro de rede: {}"]),
    ("no_news", ["новостей нет (HTTP {})", "no news (HTTP {})", "sin novedades (HTTP {})", "sem novidades (HTTP {})"]),
    ("unexpected_site", ["неожиданный ответ сайта", "unexpected answer from the website", "respuesta inesperada del sitio", "resposta inesperada do site"]),
    // PC checks and setup
    ("wine_system", ["Wine (системный)", "Wine (system)", "Wine (sistema)", "Wine (sistema)"]),
    ("no_resolve", ["{} не разрешается", "{} does not resolve", "{} no se resuelve", "{} não é resolvido"]),
    ("hosts_wrong", ["{} → {} (ожидался {})", "{} → {} (expected {})", "{} → {} (se esperaba {})", "{} → {} (esperado {})"]),
    ("hosts_missing", ["{} отсутствует в файле hosts", "{} is not in the hosts file", "{} no está en el archivo hosts", "{} não está no arquivo hosts"]),
    ("cert_store_unreadable", ["хранилище сертификатов недоступно", "certificate store unreadable", "almacén de certificados ilegible", "repositório de certificados ilegível"]),
    ("cert_ok_windows", ["сертификат сервера доверен Windows", "server certificate trusted by Windows", "certificado del servidor reconocido por Windows", "certificado do servidor reconhecido pelo Windows"]),
    ("cert_missing", ["сертификат сервера не установлен", "server certificate not installed", "certificado del servidor no instalado", "certificado do servidor não instalado"]),
    ("cert_ok_macos", ["сертификат сервера доверен macOS", "server certificate trusted by macOS", "certificado del servidor aprobado por macOS", "certificado do servidor aprovado pelo macOS"]),
    ("cert_not_trusted", ["сертификат сервера не доверен", "server certificate not trusted", "certificado del servidor no aprobado", "certificado do servidor não aprovado"]),
    ("cert_ok_prefix", ["сертификат установлен в префикс Wine", "certificate installed in the Wine prefix", "certificado instalado en el prefijo de Wine", "certificado instalado no prefixo do Wine"]),
    ("prefix_missing", ["префикс Wine ещё не создан", "Wine prefix not created yet", "prefijo de Wine aún no creado", "prefixo do Wine ainda não criado"]),
    ("cert_not_in_prefix", ["сертификата нет в префиксе Wine", "certificate missing from the Wine prefix", "certificado ausente del prefijo de Wine", "certificado ausente do prefixo do Wine"]),
    ("bad_portal", ["неверный портал", "invalid portal", "portal no válido", "portal inválido"]),
    ("admin_refused", ["права администратора отклонены", "administrator permission refused", "permiso de administrador denegado", "permissão de administrador recusada"]),
    ("install_wait", ["не удалось дождаться установки", "could not wait for the installation", "no se pudo esperar a la instalación", "não foi possível aguardar a instalação"]),
    ("install_failed", ["установка не удалась (код {})", "installation failed (code {})", "la instalación falló (código {})", "a instalação falhou (código {})"]),
    ("hosts_prompt_remove", ["Дримворлд Навсегда: удаляем сервер из файла hosts.", "Dreamworld Forever: removing the server from the hosts file.", "Dreamworld Forever: quitando el servidor del archivo hosts.", "Dreamworld Forever: removendo o servidor do arquivo hosts."]),
    ("hosts_prompt", ["Дримворлд Навсегда: добавляем сервер в файл hosts.", "Dreamworld Forever: adding the server to the hosts file.", "Forever Launcher: añadiendo el servidor al archivo hosts.", "Forever Launcher: adicionando o servidor ao arquivo hosts."]),
    ("hosts_refused", ["доступ к файлу hosts отклонён", "permission refused for the hosts file", "permiso denegado para el archivo hosts", "permissão recusada para o arquivo hosts"]),
    ("hosts_write_failed", ["не удалось записать hosts: {}", "could not write the hosts file: {}", "no se pudo escribir el archivo hosts: {}", "não foi possível gravar o arquivo hosts: {}"]),
    ("hosts_write_failed_pkexec", ["не удалось записать hosts (pkexec)", "could not write the hosts file (pkexec)", "no se pudo escribir el archivo hosts (pkexec)", "não foi possível gravar o arquivo hosts (pkexec)"]),
    ("hosts_pkexec_unavailable", ["нет прав администратора для hosts: задайте пароль командой passwd в Konsole и нажмите «Подготовить» в режиме рабочего стола", "no administrator rights for the hosts file: set a password with passwd in Konsole and press Prepare in Desktop Mode", "sin permisos de administrador para hosts: define una contraseña con passwd en Konsole y pulsa Preparar en el modo escritorio", "sem permissão de administrador para o hosts: defina uma senha com passwd no Konsole e clique em Preparar no modo área de trabalho"]),
    ("cert_trust_failed", ["сертификат не доверен: {}", "certificate not trusted: {}", "certificado no aprobado: {}", "certificado não aprovado: {}"]),
    ("prefix_error", ["префикс {}: {}", "prefix {}: {}", "prefijo {}: {}", "prefixo {}: {}"]),
    ("wine_not_found", ["wine не найден", "wine not found", "no se encontró wine", "wine não encontrado"]),
    ("umu_not_found", ["umu-run не найден (пакет umu-launcher)", "umu-run not found (umu-launcher package)", "no se encontró umu-run (paquete umu-launcher)", "umu-run não encontrado (pacote umu-launcher)"]),
    ("no_proton", ["Proton не найден: установите Proton в Steam (Библиотека → Инструменты → Proton)", "Proton not found: install Proton in Steam (Library → Tools → Proton)", "Proton no encontrado: instala Proton en Steam (Biblioteca → Herramientas → Proton)", "Proton não encontrado: instale o Proton no Steam (Biblioteca → Ferramentas → Proton)"]),
    ("proton_not_found", ["Proton не найден: {}", "Proton not found: {}", "Proton no encontrado: {}", "Proton não encontrado: {}"]),
    ("command_failed", ["{} не удалось: {}", "{} failed: {}", "{} falló: {}", "{} falhou: {}"]),
    // key patcher (journal)
    ("bad_public_key", ["неверный публичный ключ сервера", "invalid server public key", "clave pública del servidor no válida", "chave pública do servidor inválida"]),
    ("bad_region_group", ["неверная группа региона (2-12, кроме 9)", "invalid region group (2-12, except 9)", "grupo de región no válido (2-12, excepto 9)", "grupo de região inválido (2-12, exceto 9)"]),
    ("write_refused", ["запись отклонена", "write refused", "escritura rechazada", "gravação recusada"]),
    ("verify_failed", ["проверка не удалась", "check failed", "verificación fallida", "verificação falhou"]),
    ("memory_changed", ["память изменилась во время записи", "memory changed while writing", "la memoria cambió durante la escritura", "a memória mudou durante a gravação"]),
    ("key_restored", ["ключ сервера восстановлен", "server key put back", "clave del servidor restablecida", "chave do servidor restaurada"]),
    ("key_write_error", ["запись ключа: {}", "writing the key: {}", "escribiendo la clave: {}", "gravando a chave: {}"]),
    ("key_store_freed", ["хранилище ключей освобождено, поиск при следующем входе", "key store freed, searching again at the next login", "almacén de claves liberado, nueva búsqueda en el próximo inicio de sesión", "repositório de chaves liberado, nova busca no próximo login"]),
    ("game_process", ["процесс игры: PID {}", "game process: PID {}", "proceso del juego: PID {}", "processo do jogo: PID {}"]),
    ("key_written", ["ключ сервера записан (группа {})", "server key written (group {})", "clave del servidor escrita (grupo {})", "chave do servidor gravada (grupo {})"]),
    ("key_write_retry", ["запись ключа: {}, повтор", "writing the key: {}, retrying", "escribiendo la clave: {}, reintentando", "gravando a chave: {}, tentando de novo"]),
    ("waiting_realm", ["ожидание входа в мир…", "waiting for the realm login…", "esperando la entrada al reino…", "aguardando a entrada no reino…"]),
    ("many_key_stores", ["{} хранилищ ключей одновременно: ждём", "{} key stores at once: waiting", "{} almacenes de claves a la vez: esperando", "{} repositórios de chaves ao mesmo tempo: aguardando"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_has_four_texts_with_the_same_placeholders() {
        for (key, texts) in MESSAGES {
            let n = texts[0].matches("{}").count();
            for text in texts {
                assert!(!text.is_empty(), "{key}");
                assert_eq!(text.matches("{}").count(), n, "{key}: {text}");
            }
        }
    }

    #[test]
    fn formats_in_the_chosen_language() {
        set("es-419");
        assert_eq!(tf("server_error", &[&503]), "error del servidor (HTTP 503)");
        set("pt-BR");
        assert_eq!(t("banned"), "conta banida");
        set("de");
        assert_eq!(t("banned"), "account banned");
        set("ru");
    }
}
