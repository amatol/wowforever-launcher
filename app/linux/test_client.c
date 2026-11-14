/* Synthetic Windows client, never connects to a server or uses an account. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>
#include "keystore.h"

static void result(const char *text) {
    FILE *f = fopen("fixture-result.txt", "ab");
    if (f) { fprintf(f, "%s\n", text); fclose(f); }
}

static void fill(uint8_t *raw) {
    memset(raw, 0, KEY_BLOCK);
    for (unsigned e = 0; e < KEY_COUNT; ++e) {
        raw[e * KEY_ENTRY] = (uint8_t)e + 1;
        decode_key(known_keys[e], raw + e * KEY_ENTRY + 4);
        raw[e * KEY_ENTRY + 36] = e == 8 ? 0 : 1;
    }
}

/* Store in the client's own image (.data), not in the heap. */
static uint8_t module_store[KEY_BLOCK];

int main(void) {
    const char *mode = getenv("FOREVER_FIXTURE_MODE");
    if (!mode) return 10;
    if (!strcmp(mode, "delayed")) Sleep(1500);
    if (!strcmp(mode, "respawn")) {
        SetEnvironmentVariableA("FOREVER_FIXTURE_MODE", "valid");
        wchar_t path[32768], command[32768];
        GetModuleFileNameW(NULL, path, 32768);
        swprintf(command, 32768, L"\"%ls\"", path);
        STARTUPINFOW si = {0}; si.cb = sizeof si;
        PROCESS_INFORMATION pi;
        if (!CreateProcessW(path, command, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) return 11;
        CloseHandle(pi.hThread); CloseHandle(pi.hProcess);
        return 0;
    }
    /* Add a false ID candidate at a guard boundary on every test. */
    uint8_t *guard = VirtualAlloc(NULL, 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!guard) return 12;
    guard[4092] = 1;
    DWORD old;
    VirtualProtect(guard + 4096, 4096, PAGE_NOACCESS, &old);
    uint8_t *allocation = VirtualAlloc(NULL, 2 * 1024 * 1024, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!allocation) return 13;
    /* The complete store straddles a scan chunk, so overlap is required. */
    volatile uint8_t *raw = allocation + 1024 * 1024 - 240;
    fill((uint8_t *)raw);
    if (!strcmp(mode, "module")) {
        memset((uint8_t *)raw, 0, KEY_BLOCK);
        fill(module_store);
        raw = module_store;
    }
    int late_keys = !strcmp(mode, "late-keys") || !strcmp(mode, "late-duplicate");
    if (late_keys) {
        for (unsigned e = 0; e < KEY_COUNT; ++e)
            memset((uint8_t *)raw + e * KEY_ENTRY + 4, 0, 32);
    }
    if (!strcmp(mode, "unknown")) raw[4] ^= 1;
    if (!strcmp(mode, "duplicate")) fill(allocation + 128);
    if (!strcmp(mode, "late-duplicate")) {
        fill(allocation + 128);
        for (unsigned e = 0; e < KEY_COUNT; ++e)
            memset(allocation + 128 + e * KEY_ENTRY + 4, 0, 32);
    }
    if (!strcmp(mode, "overflow")) {
        for (unsigned c = 0; c < 128; ++c) {
            uint8_t *p = allocation + 128 + c * 512;
            memset(p, 0, KEY_BLOCK);
            for (unsigned e = 0; e < KEY_COUNT; ++e) p[e * KEY_ENTRY] = (uint8_t)e + 1;
        }
    }
    if (!strcmp(mode, "readonly")) VirtualProtect(allocation, 2 * 1024 * 1024, PAGE_READONLY, &old);
    uint8_t expected[32];
    decode_key("02596F0D0C061A8B30745988FD72C59E29EC367FB0F341F28E0F08D037BAFC69", expected);
    int should_patch = !strcmp(mode, "valid") || !strcmp(mode, "restore") || !strcmp(mode, "delayed") || !strcmp(mode, "late-keys") || !strcmp(mode, "module");
    int seen = 0, restored = 0;
    ULONGLONG start = GetTickCount64();
    ULONGLONG filled_at = 0;
    while (GetTickCount64() - start < (late_keys ? 28000 : 8000)) {
        /* IDs exist early, keys arrive after the scanner's startup/backoff period. */
        if (late_keys && !filled_at && GetTickCount64() - start >= 22000) {
            fill((uint8_t *)raw);
            if (!strcmp(mode, "late-duplicate")) fill(allocation + 128);
            filled_at = GetTickCount64();
        }
        int patched = !memcmp((const void *)(raw + 7 * KEY_ENTRY + 4), expected, 32);
        if (patched) {
            seen = 1;
            if (!should_patch) { result("FIXTURE FAIL unexpected write"); return 14; }
            if (!strcmp(mode, "restore") && !restored) {
                Sleep(400);
                decode_key(known_keys[7], (uint8_t *)raw + 7 * KEY_ENTRY + 4);
                restored = 1;
            } else {
                if (late_keys && GetTickCount64() - filled_at > 500) {
                    result("FIXTURE FAIL late keys were not patched within 500ms"); return 16;
                }
                if (late_keys) {
                    char message[96];
                    snprintf(message, sizeof message, "FIXTURE PASS late_keys_latency_ms=%llu",
                             (unsigned long long)(GetTickCount64() - filled_at));
                    result(message);
                }
                result("FIXTURE PASS patched and verified");
                return 0;
            }
        }
        Sleep(50);
    }
    if (should_patch || seen) { result("FIXTURE FAIL missing patch"); return 15; }
    result("FIXTURE PASS no speculative write");
    return 0;
}
