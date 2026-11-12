// Loaded with LD_PRELOAD into the Wine process that runs Wow.exe. It writes the server's Ed25519
// key into the client's key store from inside the process, so it does not need ptrace access to
// /proc/<pid>/mem (which SteamOS restricts under pressure-vessel / Yama ptrace_scope=1).
//
// Environment: FOREVER_SERVER_KEY (64 hex chars), FOREVER_REGION_GROUP (default 8),
// FOREVER_PATCH_LOG (log file, default /tmp/forever-keypatch.log).

#define _GNU_SOURCE
#include <ctype.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <time.h>
#include <unistd.h>

#define ENTRY 40
#define COUNT 12
#define BLOCK (ENTRY * COUNT)
#define CHUNK (8 * 1024 * 1024)

static const char *KNOWN_KEYS[COUNT] = {
    "9B0671C815DFF513BFD4A2B26AE1F84EC9106841B2FB620DB65F6ADE7C21AD06",
    "112451B9843C2799E4750AA3D9B9AFABF536A645B863C4ADB2078B932B354E04",
    "A6B858485748BF38BE193517AB90F8DE169EFF0989EA9360DB346A378B0FFE15",
    "50FFDEB807F8FF73A299A16000AA6575C5945BE875ADFC8795374ED3415249AC",
    "9F842D078755647C008E5FE3E12B839A981DE02A1F520CB2545CC04CD2323E0E",
    "B8CC75864D8EF461D8DD6AA7425E2C63C24D3982066B8D773A1549BFE24E05EE",
    "76BB7BBDD9F34E124A4573C3AA227E3C87CC603506697D054F6FDEC342E56EBF",
    "1FD6DD8FA0EC30D39E3F72E755B8A045BDE0F70449DC71008B767C2EAA89FB9F",
    "15D618BD7DB577BD9A8D45769C59E4FC631633BF447398A4B489B4C26FBC03AD",
    "B34EC592D2AC4993F5FFC85B15C3DA9078694051CB224345592AF7136D796C99",
    "9E91586DD4B115AB0568B21959E181F6545910BC5E2F30B8975CA09D7BC3EFCE",
    "769C824A1DADD19AEEFA420414D1732DAF44537F98AA229FC5477BCA55F9D59A",
};

static uint8_t keys[COUNT][32];
static uint8_t server_key[32];
static int group = 8;
static char log_path[512] = "/tmp/forever-keypatch.log";
static int finished;

static void say(const char *fmt, ...) {
    FILE *f = fopen(log_path, "a");
    if (!f) return;
    time_t t = time(NULL);
    char ts[32];
    strftime(ts, sizeof ts, "%H:%M:%S", localtime(&t));
    fprintf(f, "%s [forever] ", ts);
    va_list ap;
    va_start(ap, fmt);
    vfprintf(f, fmt, ap);
    va_end(ap);
    fputc('\n', f);
    fclose(f);
}

static int hex32(const char *s, uint8_t out[32]) {
    if (!s || strlen(s) != 64) return 0;
    for (int i = 0; i < 32; i++) {
        unsigned v;
        if (sscanf(s + 2 * i, "%2x", &v) != 1) return 0;
        out[i] = (uint8_t)v;
    }
    return 1;
}

static int is_game_process(void) {
    char buf[8192];
    int fd = open("/proc/self/cmdline", O_RDONLY);
    if (fd < 0) return 0;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return 0;
    buf[n] = 0;
    // only argv[0]: start.exe also has Wow.exe in its command line, but it is not the game process
    for (ssize_t i = 0; i < n; i++) {
        if (buf[i] == 0) { buf[i] = 0; break; }
        buf[i] = (char)tolower((unsigned char)buf[i]);
    }
    return strstr(buf, "wow.exe") != NULL || strstr(buf, "wowb.exe") != NULL;
}

// key[i] is at offset 4 in each entry; ids are 1..12, entries are `stride` bytes apart
static int store_state(int mem, unsigned long long addr, int stride, int *patched) {
    if (stride < 36 || stride > 128) return 0;
    uint8_t raw[COUNT * 128];
    (void)mem;
    memcpy(raw, (void *)(uintptr_t)addr, (size_t)COUNT * stride);
    for (int e = 0; e < COUNT; e++) {
        int o = e * stride;
        uint32_t id;
        memcpy(&id, raw + o, 4);
        if (id != (uint32_t)e + 1) return 0;
        uint8_t *key = raw + o + 4;
        int nonzero = 0;
        for (int i = 0; i < 32; i++) nonzero |= key[i];
        if (!nonzero) return 0;
        for (int i = 0; i + 4 <= 32; i++) {
            if (!key[i] && !key[i + 1] && !key[i + 2] && !key[i + 3]) return 0;
        }
        if (stride >= 40 && raw[o + 36] > 1) return 0;
    }
    *patched = memcmp(raw + (group - 1) * stride + 4, server_key, 32) == 0;
    return 1;
}

static int patch_at(int mem, unsigned long long addr, int stride) {
    uint8_t cur[32];
    (void)mem;
    off_t at = (off_t)(addr + (unsigned long)(group - 1) * stride + 4);
    memcpy(cur, (void *)(uintptr_t)at, 32);
    if (memcmp(cur, server_key, 32) == 0) return 1;
    // the region is writable (checked in scan_maps); write from inside the process, no ptrace
    memcpy((void *)(uintptr_t)at, server_key, 32);
    int patched = 0;
    return store_state(mem, addr, stride, &patched) && patched;
}

static int scan_maps(void) {
    FILE *maps = fopen("/proc/self/maps", "r");
    if (!maps) { say("cannot open /proc/self/maps"); return 0; }
    int mem = open("/proc/self/mem", O_RDONLY);
    if (mem < 0) { say("cannot open /proc/self/mem: %d", errno); fclose(maps); return 0; }
    static uint8_t buf[CHUNK + 64];
    char line[1024];
    int found = 0;
    while (!found && fgets(line, sizeof line, maps)) {
        unsigned long long lo, hi;
        char perms[8] = {0};
        if (sscanf(line, "%llx-%llx %7s", &lo, &hi, perms) != 3) continue;
        if (perms[0] != 'r' || perms[1] != 'w') continue;
        char *path = strchr(line, '/');
        if (path && strstr(path, "[stack]")) continue;
        if (hi <= lo || hi - lo > (1ULL << 31)) continue;
        unsigned long long off = lo;
        while (!found && off < hi) {
            size_t take = (size_t)((hi - off < CHUNK) ? (hi - off) : CHUNK);
            ssize_t n = pread(mem, buf, take, (off_t)off);
            if (n <= 0) break;
            // 1) old known keys: find the first key of the store
            for (size_t i = 0; !found && i + 32 <= (size_t)n; i++) {
                if (memcmp(buf + i, keys[0], 32) != 0) continue;
                if (i < 4) continue;
                unsigned long long addr = off + i - 4;
                int patched = 0;
                if (store_state(0, addr, ENTRY, &patched)) {
                    if (patch_at(0, addr, ENTRY)) { found = 1; }
                }
            }
            // 2) structural: ids 1..12; try common entry sizes
            for (size_t i = 0; !found && i + 4 <= (size_t)n; i += 4) {
                uint32_t id;
                memcpy(&id, buf + i, 4);
                if (id != 1) continue;
                for (int stride = 40; stride <= 64 && !found; stride += 8) {
                    int patched = 0;
                    if (store_state(mem, off + i, stride, &patched)) {
                        if (patch_at(0, off + i, stride)) found = 1;
                    }
                }
            }
            if ((size_t)n < take || off + take >= hi) break;
            off += (unsigned long long)n - 64;
        }
    }
    close(mem);
    fclose(maps);
    return found;
}

static void *worker(void *unused) {
    (void)unused;
    for (int i = 0; i < 240 && !finished; i++) {
        if (scan_maps()) {
            say("server key written (group %d)", group);
            finished = 1;
            return NULL;
        }
        usleep(500000);
    }
    say("key store not found");
    return NULL;
}

__attribute__((constructor)) static void init(void) {
    const char *s = getenv("FOREVER_SERVER_KEY");
    const char *g = getenv("FOREVER_REGION_GROUP");
    const char *l = getenv("FOREVER_PATCH_LOG");
    if (l && *l) snprintf(log_path, sizeof log_path, "%s", l);
    if (!is_game_process()) return;
    for (int i = 0; i < COUNT; i++) hex32(KNOWN_KEYS[i], keys[i]);
    if (!hex32(s, server_key)) {
        say("no valid FOREVER_SERVER_KEY");
        return;
    }
    if (g) group = atoi(g);
    if (group < 2 || group > 12 || group == 9) group = 8;
    say("loaded into the game process (pid %d), group %d", (int)getpid(), group);
    pthread_t th;
    if (pthread_create(&th, NULL, worker, NULL) == 0) pthread_detach(th);
}
