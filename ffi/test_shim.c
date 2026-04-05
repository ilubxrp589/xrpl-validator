/* test_shim.c — parses a real mainnet Payment tx via the libxrpl shim. */

#include "xrpl_shim.h"
#include <stdio.h>
#include <stdint.h>
#include <string.h>

/* Real mainnet Payment tx fetched from localai rippled.
 * Hash: 39077702C3FCE0DDC5693065FC0DA35576E4D0112FDEA08D6CAD099074033ABA
 * Type: Payment (referral reward)
 */
static const char *MAINNET_PAYMENT_HEX =
    "12000022800000002404E59B6E61400000000000000A68400000000000000F"
    "732103ECD7DE6564273713EE4EA28A1F4522B3B480F66DC903EB4E5309D32F"
    "633A6DAC74463044022074B318BE47C213C5B9A57341A454033D3CDF97FBB6"
    "998FDA654B4F879A9C1C6502204F520B978C98C8F857B8111FD5E00E94EE16"
    "A2C503EF522FDFA7B0131201051A8114E5A5902FEBDA49C3BDE5B7E4522C62"
    "F3D49E4666831492BD9E89265D9F5853BF1AAB400766CDDBDAEC3CF9EA7D1D"
    "526566657272616C207265776172642066726F6D204D61676E65746963E1F1";

static int hex2byte(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

static size_t decode_hex(const char *hex, uint8_t *out, size_t out_max) {
    size_t hex_len = strlen(hex);
    if (hex_len % 2 || hex_len / 2 > out_max) return 0;
    for (size_t i = 0; i < hex_len; i += 2) {
        int hi = hex2byte(hex[i]);
        int lo = hex2byte(hex[i + 1]);
        if (hi < 0 || lo < 0) return 0;
        out[i / 2] = (uint8_t)((hi << 4) | lo);
    }
    return hex_len / 2;
}

/* boost_test_exec_monitor provides main() which calls test_main().
 * We override test_main() here instead of providing our own main(). */
int test_main(int argc, char **argv) {
    (void)argc; (void)argv;
    printf("=== xrpl_shim integration test ===\n\n");
    printf("xrpl_shim version:    %s\n", xrpl_shim_version());
    printf("xrpl libxrpl version: %s\n\n", xrpl_rippled_version());

    /* Decode the mainnet tx */
    uint8_t tx_bytes[1024];
    size_t tx_len = decode_hex(MAINNET_PAYMENT_HEX, tx_bytes, sizeof(tx_bytes));
    if (tx_len == 0) {
        printf("hex decode failed\n");
        return 1;
    }
    printf("Decoded tx: %zu bytes\n", tx_len);

    /* Parse via shim */
    uint8_t tx_hash[32];
    char type_name[64];
    if (!xrpl_tx_parse(tx_bytes, tx_len, tx_hash, type_name, sizeof(type_name))) {
        printf("xrpl_tx_parse FAILED\n");
        return 1;
    }

    printf("\n=== Parsed via libxrpl ===\n");
    printf("Tx type: %s\n", type_name);
    printf("Tx hash: ");
    for (int i = 0; i < 32; i++) printf("%02X", tx_hash[i]);
    printf("\n");
    printf("Expected: 39077702C3FCE0DDC5693065FC0DA35576E4D0112FDEA08D6CAD099074033ABA\n\n");

    /* Test engine create/destroy */
    XrplEngine *engine = xrpl_engine_create();
    printf("Created engine: %p\n", (void *)engine);
    xrpl_engine_destroy(engine);
    printf("Destroyed engine.\n");

    /* === CALL PREFLIGHT with real mainnet tx === */
    printf("\n=== Calling xrpl::preflight() via shim ===\n");
    char ter_name[128];
    int32_t ter = xrpl_preflight(
        tx_bytes, tx_len,
        NULL, 0,           /* no amendments (empty Rules) */
        0,                 /* ApplyFlags = tapNONE */
        0,                 /* network_id = mainnet */
        ter_name, sizeof(ter_name));
    printf("preflight result: TER=%d (%s)\n", ter, ter_name);

    return 0;
}
