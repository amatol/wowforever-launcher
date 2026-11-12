/* The verified 12 x 40-byte store used by the Windows patcher. No guessed layouts. */
#ifndef FOREVER_KEYSTORE_H
#define FOREVER_KEYSTORE_H
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#define KEY_COUNT 12
#define KEY_ENTRY 40
#define KEY_BLOCK (KEY_COUNT * KEY_ENTRY)

static const char *const known_keys[KEY_COUNT] = {
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
    "769C824A1DADD19AEEFA420414D1732DAF44537F98AA229FC5477BCA55F9D59A"
};

static inline int hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static inline int decode_key(const char *s, uint8_t key[32]) {
    if (!s || strlen(s) != 64) return 0;
    for (size_t i = 0; i < 32; ++i) {
        int hi = hex_digit(s[i * 2]), lo = hex_digit(s[i * 2 + 1]);
        if (hi < 0 || lo < 0) return 0;
        key[i] = (uint8_t)((hi << 4) | lo);
    }
    return 1;
}

/* 0 invalid, 1 original, 2 already patched. The selected key is the only mutable field. */
static inline int keystore_state(const uint8_t *raw, size_t len, const uint8_t keys[KEY_COUNT][32],
                          const uint8_t replacement[32], unsigned group) {
    static const uint8_t flags[KEY_COUNT] = {1,1,1,1,1,1,1,1,0,1,1,1};
    if (len < KEY_BLOCK || group < 2 || group > 12 || group == 9) return 0;
    for (unsigned e = 0; e < KEY_COUNT; ++e) {
        const uint8_t *p = raw + e * KEY_ENTRY;
        if (p[0] != e + 1 || p[1] || p[2] || p[3]) return 0;
        if (memcmp(p + 4, keys[e], 32) &&
            (e != group - 1 || memcmp(p + 4, replacement, 32))) return 0;
        /* Byte 37 is uninitialised padding in the existing Windows client. */
        if (p[36] != flags[e] || p[38] || p[39]) return 0;
    }
    return memcmp(raw + (group - 1) * KEY_ENTRY + 4, replacement, 32) ? 1 : 2;
}

/* Diagnostic only. An unknown-key structure must never authorise a write. */
static inline int keystore_shape(const uint8_t *raw, size_t len) {
    if (len < KEY_BLOCK) return 0;
    for (unsigned e = 0; e < KEY_COUNT; ++e) {
        const uint8_t *p = raw + e * KEY_ENTRY;
        if (p[0] != e + 1 || p[1] || p[2] || p[3]) return 0;
    }
    return 1;
}
#endif
