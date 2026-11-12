#include <assert.h>
#include <stdio.h>
#include "keystore.h"

int main(void) {
    uint8_t keys[KEY_COUNT][32], server[32], raw[KEY_BLOCK];
    assert(decode_key("02596F0D0C061A8B30745988FD72C59E29EC367FB0F341F28E0F08D037BAFC69", server));
    for (unsigned e = 0; e < KEY_COUNT; ++e) {
        assert(decode_key(known_keys[e], keys[e]));
        uint8_t *p = raw + e * KEY_ENTRY;
        memset(p, 0, KEY_ENTRY);
        p[0] = (uint8_t)e + 1;
        memcpy(p + 4, keys[e], 32);
        p[36] = e == 8 ? 0 : 1;
        p[37] = 0x7f;
    }
    assert(keystore_state(raw, sizeof raw, keys, server, 8) == 1);
    assert(keystore_shape(raw, sizeof raw));
    for (size_t len = 0; len < KEY_BLOCK; ++len) {
        assert(!keystore_state(raw, len, keys, server, 8));
        assert(!keystore_shape(raw, len));
    }
    memcpy(raw + 7 * KEY_ENTRY + 4, server, 32);
    assert(keystore_state(raw, sizeof raw, keys, server, 8) == 2);
    raw[36] = 0;
    assert(!keystore_state(raw, sizeof raw, keys, server, 8));
    raw[36] = 1;
    raw[38] = 1;
    assert(!keystore_state(raw, sizeof raw, keys, server, 8));
    raw[38] = 0;
    raw[4] ^= 1; /* Unknown keys may be diagnosed, never patched. */
    assert(!keystore_state(raw, sizeof raw, keys, server, 8));
    assert(keystore_shape(raw, sizeof raw));
    assert(!keystore_state(raw, sizeof raw, keys, server, 1));
    assert(!keystore_state(raw, sizeof raw, keys, server, 9));
    assert(!keystore_state(raw, sizeof raw, keys, server, 13));
    char bad[65];
    memset(bad, '1', 64); bad[64] = 0; bad[1] = 'Z';
    assert(!decode_key(bad, server));
    assert(!decode_key("00", server));
    puts("keystore tests passed (bounds, unknown keys, flags, hex, region group)");
    return 0;
}
