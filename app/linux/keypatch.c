/* PE64 helper run by the same Wine/Proton as the game. It owns the game launch and
 * uses Wine's Windows APIs, not ptrace, host /proc memory or LD_PRELOAD hooks.
 * Protocol: newline-delimited records flushed to the per-launch log, no tickets. */
#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0601
#include <windows.h>
#include <tlhelp32.h>
#include <psapi.h>
#include <stdio.h>
#include <stdarg.h>
#include <wchar.h>
#include <stdlib.h>
#include "keystore.h"

#define CHUNK (1024 * 1024)
#define MAX_CLIENTS 16
#define MAX_OWNED 256
#define MAX_CANDIDATES 128
#define MAX_SCAN_REGIONS 8192
#define SMALL_HEAP_LIMIT (64u * 1024u * 1024u)

static FILE *log_file;
static uint8_t keys[KEY_COUNT][32], replacement[32];
static unsigned group;
static ULONGLONG last_heartbeat;
static ULONGLONG started_tick;
static DWORD owned[MAX_OWNED];
static size_t owned_count;

static int is_owned(DWORD pid) {
    for (size_t i = 0; i < owned_count; ++i) if (owned[i] == pid) return 1;
    return 0;
}

static void record(const char *kind, const char *fmt, ...) {
    fprintf(log_file, "%s ", kind);
    va_list args;
    va_start(args, fmt);
    vfprintf(log_file, fmt, args);
    va_end(args);
    SYSTEMTIME utc;
    GetSystemTime(&utc);
    fprintf(log_file, " t_ms=%llu utc=%04u-%02u-%02uT%02u:%02u:%02u.%03uZ\n",
            (unsigned long long)(GetTickCount64() - started_tick),
            utc.wYear, utc.wMonth, utc.wDay, utc.wHour, utc.wMinute, utc.wSecond, utc.wMilliseconds);
    fflush(log_file);
}

static void heartbeat(void) {
    ULONGLONG now = GetTickCount64();
    if (now - last_heartbeat >= 2000) {
        record("HEARTBEAT", "%llu", (unsigned long long)now);
        last_heartbeat = now;
    }
}

static int read_exact(HANDLE process, uintptr_t address, void *buffer, size_t len) {
    SIZE_T got = 0;
    return ReadProcessMemory(process, (void *)address, buffer, len, &got) && got == len;
}

static int live(HANDLE process) {
    return WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
}

static int state_at(HANDLE process, uintptr_t address) {
    uint8_t raw[KEY_BLOCK];
    if (!read_exact(process, address, raw, sizeof raw)) return 0;
    return keystore_state(raw, sizeof raw, keys, replacement, group);
}

static int writable(DWORD protect);

static int patch_store(HANDLE process, uintptr_t address) {
    /* Revalidate the entire store immediately before writing. Never change protection. */
    int state = state_at(process, address);
    if (state == 2) return 1;
    if (state != 1) { SetLastError(ERROR_INVALID_DATA); return 0; }
    SIZE_T written = 0;
    uintptr_t target = address + (group - 1) * KEY_ENTRY + 4;
    MEMORY_BASIC_INFORMATION info;
    if (!VirtualQueryEx(process, (void *)target, &info, sizeof info)) return 0;
    if (info.State != MEM_COMMIT || !writable(info.Protect) ||
        target < (uintptr_t)info.BaseAddress ||
        target - (uintptr_t)info.BaseAddress > info.RegionSize ||
        info.RegionSize - (target - (uintptr_t)info.BaseAddress) < 32) {
        SetLastError(ERROR_ACCESS_DENIED);
        return 0;
    }
    if (!WriteProcessMemory(process, (void *)target, replacement, 32, &written) || written != 32) {
        return 0;
    }
    if (state_at(process, address) == 2) return 1;
    SetLastError(ERROR_INVALID_DATA);
    return 0;
}

typedef struct {
    DWORD pid;
    HANDLE handle;
    uintptr_t store;
    ULONGLONG next_scan;
    ULONGLONG first_seen;
    ULONGLONG next_warning;
    ULONGLONG next_patch;
    uintptr_t pending_store;
    ULONGLONG pending_since;
    uintptr_t candidates[MAX_CANDIDATES];
    size_t candidate_count;
    size_t confirmed_candidates;
    int candidate_overflow;
    int ready;
} Client;

static int writable(DWORD protect) {
    if (protect & (PAGE_GUARD | PAGE_NOACCESS)) return 0;
    DWORD base = protect & 0xff;
    return base == PAGE_READWRITE || base == PAGE_WRITECOPY ||
           base == PAGE_EXECUTE_READWRITE || base == PAGE_EXECUTE_WRITECOPY;
}

static void remember_candidate(Client *client, uintptr_t address, const uint8_t *raw) {
    for (size_t i = 0; i < client->candidate_count; ++i)
        if (client->candidates[i] == address) return;
    if (client->candidate_count == MAX_CANDIDATES) {
        if (!client->candidate_overflow)
            record("WARN", "pid=%lu candidate index overflow; no write until a complete reindex", client->pid);
        client->candidate_overflow = 1;
        return;
    }
    client->candidates[client->candidate_count++] = address;
    unsigned mask = 0;
    for (unsigned e = 0; e < KEY_COUNT; ++e)
        if (!memcmp(raw + e * KEY_ENTRY + 4, keys[e], 32)) mask |= 1u << e;
    record("CANDIDATE", "pid=%lu address=%llx known_mask=%03x", client->pid,
           (unsigned long long)address, mask);
}

/* IDs can be present long before the keys are populated. Poll small, previously
 * indexed candidates rather than waiting for another multi-GB sweep. A shape alone
 * never permits a write: every key, flag and padding byte is still verified.
 * Only a completed sweep can make new candidates eligible. Duplicates fail closed. */
static uintptr_t probe_candidates(Client *client) {
    if (!client->confirmed_candidates || client->candidate_overflow) return 0;
    uintptr_t found = 0;
    size_t matches = 0;
    int eligible = 0;
    for (size_t i = 0; i < client->candidate_count; ++i) {
        if (state_at(client->handle, client->candidates[i])) {
            found = client->candidates[i];
            eligible = i < client->confirmed_candidates;
            ++matches;
        }
    }
    if (matches == 1) {
        if (eligible) {
            record("CACHE_HIT", "pid=%lu address=%llx candidates=%zu", client->pid,
                   (unsigned long long)found, client->candidate_count);
            return found;
        }
        /* A store discovered in the middle of a sweep is fully verified, but
         * wait one short settling interval so a duplicate in the next region
         * can still make the choice ambiguous. */
        ULONGLONG now = GetTickCount64();
        if (client->pending_store != found) {
            client->pending_store = found;
            client->pending_since = now;
        } else if (now - client->pending_since >= 100) {
            record("CANDIDATE_SETTLED", "pid=%lu address=%llx candidates=%zu",
                   client->pid, (unsigned long long)found, client->candidate_count);
            return found;
        }
    } else {
        client->pending_store = 0;
        client->pending_since = 0;
    }
    if (matches > 1 && GetTickCount64() >= client->next_warning) {
        record("WARN", "pid=%lu indexed verified stores=%zu; no write", client->pid, matches);
        client->next_warning = GetTickCount64() + 15000;
    }
    return 0;
}

typedef struct {
    uintptr_t lo;
    uintptr_t hi;
    int priority;
} ScanRegion;

static int compare_regions(const void *left, const void *right) {
    const ScanRegion *a = left;
    const ScanRegion *b = right;
    if (a->priority != b->priority) return a->priority - b->priority;
    uintptr_t as = a->hi - a->lo;
    uintptr_t bs = b->hi - b->lo;
    return as < bs ? -1 : as > bs ? 1 : 0;
}

static uintptr_t scan(Client *client, uint8_t *buffer, const wchar_t *game) {
    uintptr_t address = 0;
    size_t matches = 0, shapes = 0, regions = 0, failures = 0;
    size_t module_regions = 0, heap_regions = 0;
    unsigned long long bytes = 0;
    ULONGLONG began = GetTickCount64();
    ULONGLONG last_probe = began;
    if (client->candidate_overflow) {
        client->candidate_count = client->confirmed_candidates = 0;
        client->candidate_overflow = 0;
    }

    /* The client's key store is created in its own image (.data/.bss) or in
     * the process heap. Collect writable regions first, then scan the main
     * module and small private heaps before huge unrelated mappings. */
    static ScanRegion list[MAX_SCAN_REGIONS];
    size_t count = 0;
    MEMORY_BASIC_INFORMATION info;
    while (live(client->handle) && VirtualQueryEx(client->handle, (void *)address, &info, sizeof info)) {
        uintptr_t lo = (uintptr_t)info.BaseAddress;
        if (info.RegionSize > UINTPTR_MAX - lo) break;
        uintptr_t hi = lo + info.RegionSize;
        if (hi <= address) break;
        address = hi;
        if (info.State != MEM_COMMIT || !writable(info.Protect)) continue;
        if (count == MAX_SCAN_REGIONS) break;
        int priority = 3;
        if (info.Type == MEM_IMAGE) {
            wchar_t mapped[32768];
            DWORD len = GetMappedFileNameW(client->handle, (void *)lo, mapped,
                                           (DWORD)(sizeof mapped / sizeof mapped[0]));
            if (len) {
                wchar_t *name = wcsrchr(mapped, L'\\');
                name = name ? name + 1 : mapped;
                wchar_t *slash = wcsrchr(name, L'/');
                if (slash) name = slash + 1;
                const wchar_t *wanted = wcsrchr(game, L'\\');
                wanted = wanted ? wanted + 1 : game;
                wchar_t *wanted_slash = wcsrchr((wchar_t *)wanted, L'/');
                if (wanted_slash) wanted = wanted_slash + 1;
                if (!_wcsicmp(name, wanted)) priority = 0;
            }
        } else if (info.Type == MEM_PRIVATE) {
            priority = info.RegionSize <= SMALL_HEAP_LIMIT ? 1 : 2;
        }
        list[count].lo = lo;
        list[count].hi = hi;
        list[count].priority = priority;
        ++count;
    }
    for (size_t i = 0; i < count; ++i) {
        if (list[i].priority == 0) ++module_regions;
        else if (list[i].priority <= 2) ++heap_regions;
    }
    qsort(list, count, sizeof(list[0]), compare_regions);
    if (count == 0) return 0;

    for (size_t region = 0; region < count && live(client->handle); ++region) {
        uintptr_t lo = list[region].lo;
        uintptr_t hi = list[region].hi;
        ++regions;
        for (uintptr_t off = lo; off < hi && live(client->handle);) {
            size_t take = hi - off < CHUNK ? (size_t)(hi - off) : CHUNK;
            SIZE_T got = 0;
            ReadProcessMemory(client->handle, (void *)off, buffer, take, &got);
            bytes += got;
            /* Scan only bytes copied successfully; no direct pointer dereferences. */
            for (size_t i = 0; i + KEY_BLOCK <= got;) {
                const uint8_t *hit = memchr(buffer + i, 1, got - KEY_BLOCK - i + 1);
                if (!hit) break;
                i = (size_t)(hit - buffer);
                if ((off + i) % 4 == 0 && keystore_shape(hit, got - i)) {
                    ++shapes;
                    remember_candidate(client, off + i, hit);
                    if (keystore_state(hit, got - i, keys, replacement, group)) {
                        ++matches;
                    }
                }
                ++i;
            }
            heartbeat();
            if (GetTickCount64() - last_probe >= 25) {
                last_probe = GetTickCount64();
                uintptr_t cached = probe_candidates(client);
                if (cached) {
                    record("SCAN_PAUSED", "pid=%lu bytes=%llu ms=%llu reason=verified_cached_store",
                           client->pid, bytes, (unsigned long long)(GetTickCount64() - began));
                    return cached;
                }
            }
            /* Bound the work per pass as well as the memory allocation. */
            if (GetTickCount64() - began > 10000) {
                record("WARN", "pid=%lu scan timed out; no write", client->pid);
                return 0;
            }
            if (got != take) ++failures;
            if (off + take == hi) break;
            if (got == take) off += take - (KEY_BLOCK - 1);
            else off += take; /* Failed/partial pages are retried on the next pass. */
            Sleep(1);
        }
    }
    record("SCAN", "pid=%lu regions=%zu module=%zu heap=%zu bytes=%llu read_failures=%zu known=%zu shapes=%zu ms=%llu",
           client->pid, regions, module_regions, heap_regions, bytes, failures, matches, shapes,
           (unsigned long long)(GetTickCount64() - began));
    client->confirmed_candidates = client->candidate_count;
    uintptr_t candidate = probe_candidates(client);
    if (!candidate) {
        if ((matches > 1 || GetTickCount64() - client->first_seen >= 15000) &&
            GetTickCount64() >= client->next_warning) {
            record("WARN", "pid=%lu expected one verified key store, found %zu; no write", client->pid, matches);
            client->next_warning = GetTickCount64() + 15000;
        }
        return 0;
    }
    return candidate;
}

/* A new same-path client in this wineserver can replace a launcher stub. Older
 * already-running games and executables in other directories are never patched. */
static void discover(Client clients[MAX_CLIENTS], const wchar_t *game, ULONGLONG started) {
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) return;
    PROCESSENTRY32W entry = {0};
    entry.dwSize = sizeof entry;
    for (BOOL ok = Process32FirstW(snapshot, &entry); ok; ok = Process32NextW(snapshot, &entry)) {
        /* Remember descendants even if an intermediate launcher exits. Never select an
         * independently started game just because its executable path happens to match. */
        if (!is_owned(entry.th32ProcessID) && is_owned(entry.th32ParentProcessID) && owned_count < MAX_OWNED)
            owned[owned_count++] = entry.th32ProcessID;
        if (!is_owned(entry.th32ProcessID)) continue;
        if (_wcsicmp(entry.szExeFile, L"Wow.exe") && _wcsicmp(entry.szExeFile, L"WowB.exe")) continue;
        int slot = -1, exists = 0;
        for (int i = 0; i < MAX_CLIENTS; ++i) {
            if (clients[i].handle && clients[i].pid == entry.th32ProcessID) exists = 1;
            if (!clients[i].handle && slot < 0) slot = i;
        }
        if (exists || slot < 0) continue;
        HANDLE process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ |
            PROCESS_VM_WRITE | PROCESS_VM_OPERATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
        if (!process) continue;
        wchar_t path[32768];
        DWORD len = (DWORD)(sizeof path / sizeof path[0]);
        FILETIME created, exited, kernel, user;
        ULARGE_INTEGER stamp;
        int valid = live(process) && QueryFullProcessImageNameW(process, 0, path, &len) && !_wcsicmp(path, game) &&
                    GetProcessTimes(process, &created, &exited, &kernel, &user);
        if (valid) {
            stamp.LowPart = created.dwLowDateTime;
            stamp.HighPart = created.dwHighDateTime;
            valid = stamp.QuadPart >= started;
        }
        if (!valid) { CloseHandle(process); continue; }
        clients[slot] = (Client){.pid = entry.th32ProcessID, .handle = process, .first_seen = GetTickCount64()};
        record("PROCESS", "pid=%lu", entry.th32ProcessID);
    }
    CloseHandle(snapshot);
}

int wmain(int argc, wchar_t **argv) {
    /* argv: helper log game key group; no account, password or login ticket. */
    if (argc != 5) return 2;
    log_file = _wfopen(argv[1], L"ab");
    if (!log_file) return 3;
    started_tick = GetTickCount64();
    char key[65];
    size_t key_len = wcslen(argv[3]);
    if (key_len != 64) { record("ERROR", "invalid public key"); return 4; }
    for (size_t i = 0; i <= key_len; ++i) key[i] = argv[3][i] <= 127 ? (char)argv[3][i] : '?';
    wchar_t *end;
    unsigned long parsed = wcstoul(argv[4], &end, 10);
    if (*end || parsed < 2 || parsed > 12 || parsed == 9 || !decode_key(key, replacement)) {
        record("ERROR", "invalid public key or region group"); return 4;
    }
    group = (unsigned)parsed;
    for (unsigned i = 0; i < KEY_COUNT; ++i) decode_key(known_keys[i], keys[i]);
    record("START", "helper=%lu group=%u protocol=1", GetCurrentProcessId(), group);
    wchar_t game[32768];
    DWORD len = GetFullPathNameW(argv[2], (DWORD)(sizeof game / sizeof game[0]), game, NULL);
    if (!len || len >= sizeof game / sizeof game[0] || wcschr(game, L'"')) {
        record("ERROR", "invalid game path"); return 5;
    }
    wchar_t command[32768];
    if (wcslen(game) + 32 >= sizeof command / sizeof command[0]) {
        record("ERROR", "game path too long"); return 5;
    }
    swprintf(command, sizeof command / sizeof command[0], L"\"%ls\" -launcherlogin", game);
    uint8_t *buffer = malloc(CHUNK);
    if (!buffer) { record("ERROR", "out of memory"); return 6; }
    FILETIME now;
    GetSystemTimeAsFileTime(&now);
    ULARGE_INTEGER started = {.LowPart = now.dwLowDateTime, .HighPart = now.dwHighDateTime};
    STARTUPINFOW startup = {0};
    startup.cb = sizeof startup;
    PROCESS_INFORMATION child;
    if (!CreateProcessW(game, command, NULL, NULL, FALSE, 0, NULL, NULL, &startup, &child)) {
        record("ERROR", "CreateProcessW failed winerr=%lu", GetLastError());
        free(buffer); return 7;
    }
    CloseHandle(child.hThread);
    Client clients[MAX_CLIENTS] = {{0}};
    clients[0] = (Client){.pid = child.dwProcessId, .handle = child.hProcess, .first_seen = GetTickCount64()};
    owned[owned_count++] = child.dwProcessId;
    record("PROCESS", "pid=%lu", child.dwProcessId);
    ULONGLONG empty_since = 0;
    ULONGLONG next_discover = 0;
    DWORD exit_code = 0;
    for (;;) {
        heartbeat();
        for (int i = 0; i < MAX_CLIENTS; ++i) {
            Client *c = &clients[i];
            if (!c->handle) continue;
            if (!live(c->handle)) {
                GetExitCodeProcess(c->handle, &exit_code);
                record("EXIT", "pid=%lu code=%lu", c->pid, exit_code);
                CloseHandle(c->handle);
                memset(c, 0, sizeof *c);
            }
        }
        if (GetTickCount64() >= next_discover) {
            discover(clients, game, started.QuadPart);
            next_discover = GetTickCount64() + 500;
        }
        int active = 0;
        for (int i = 0; i < MAX_CLIENTS; ++i) {
            Client *c = &clients[i];
            if (!c->handle) continue;
            ++active;
            int state = c->store ? state_at(c->handle, c->store) : 0;
            if (c->store && !state) {
                record("WARN", "pid=%lu key store changed; rescanning", c->pid);
                c->store = 0;
                c->ready = 0;
            }
            if (!c->store) c->store = probe_candidates(c);
            if (!c->store && GetTickCount64() >= c->next_scan) {
                c->store = scan(c, buffer, game);
                /* The store can be allocated during startup, shortly before autologin.
                 * Retry promptly at first; after READY only 480 bytes are polled. */
                c->next_scan = GetTickCount64() + (GetTickCount64() - c->first_seen < 20000 ?
                    (c->confirmed_candidates ? 2000 : 500) : 5000);
            }
            if (c->store && state != 2) c->ready = 0;
            if (c->store && !c->ready && GetTickCount64() >= c->next_patch) {
                c->next_patch = GetTickCount64() + 250;
                if (patch_store(c->handle, c->store)) {
                    record("READY", "pid=%lu group=%u address=%llx verified=1", c->pid, group,
                           (unsigned long long)c->store);
                    c->ready = 1;
                } else if (GetTickCount64() >= c->next_warning) {
                    record("WARN", "pid=%lu key write/verification failed winerr=%lu", c->pid, GetLastError());
                    c->next_warning = GetTickCount64() + 15000;
                }
            }
        }
        if (active) empty_since = 0;
        else if (!empty_since) empty_since = GetTickCount64();
        else if (GetTickCount64() - empty_since >= 3000) break;
        Sleep(25);
    }
    record("CLOSED", "exit=%lu", exit_code);
    free(buffer);
    fclose(log_file);
    return 0;
}
