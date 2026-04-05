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

    /* === CALL APPLY with SLE callback === */
    printf("\n=== Calling xrpl::apply() via shim ===\n");

    /* Pre-fetched SLEs from mainnet ledger 103354510 */
    /* sender keylet = CED60F22A245F8DE393F2351C5097A81836153584DC3C24B803FA1B9906A506A */
    uint8_t sender_key[32];
    uint8_t sender_sle[256];
    size_t sender_sle_len = decode_hex(
        "CED60F22A245F8DE393F2351C5097A81836153584DC3C24B803FA1B9906A506A",
        sender_key, 32);
    (void)sender_sle_len;
    /* Note: SLE state at ledger 103354510. Sender sequence bumped from 6D to 6E
     * because an earlier tx in ledger 103354511 touched this account first.
     * (tx 39077702 expects sequence 6E) */
    sender_sle_len = decode_hex(
        "11006122000000002404E59B6E25062910882D0000000055B8E0C782ADB99C79445A6C72A3553C6FFD6E7BC529A906DF6FEEE09F439EADFA62400000000776FE378114E5A5902FEBDA49C3BDE5B7E4522C62F3D49E4666",
        sender_sle, sizeof(sender_sle));

    /* dest keylet = 5180078F1F6E062E4F01B17D6D05E734DF5976F0EDB187ED9EBF652A6F47D28D */
    uint8_t dest_key[32];
    uint8_t dest_sle[256];
    decode_hex("5180078F1F6E062E4F01B17D6D05E734DF5976F0EDB187ED9EBF652A6F47D28D",
        dest_key, 32);
    size_t dest_sle_len = decode_hex(
        "1100612200000000240432660C2506290E532D00000012553059070AA6AB0E4DEAC9CEE66914270E0FAD957168D465B657C359132498A64462400000000C234F9F811492BD9E89265D9F5853BF1AAB400766CDDBDAEC3C",
        dest_sle, sizeof(dest_sle));

    printf("sender SLE: %zu bytes, dest SLE: %zu bytes\n", sender_sle_len, dest_sle_len);

    /* Callback context */
    struct SleDb {
        const uint8_t *sk; const uint8_t *sv; size_t slen;
        const uint8_t *dk; const uint8_t *dv; size_t dlen;
    } db = {sender_key, sender_sle, sender_sle_len, dest_key, dest_sle, dest_sle_len};

    /* SLE lookup callback */
    extern bool sle_lookup(void *user_data, const uint8_t key[32],
                           const uint8_t **out_data, size_t *out_len);

    uint8_t parent_hash[32] = {0};  /* dummy — not critical for Payment apply */
    bool applied = false;
    int32_t apply_ter = xrpl_apply(
        tx_bytes, tx_len,
        NULL, 0,                       /* empty rules */
        103354511,                     /* ledger seq */
        797193960,                     /* parent_close_time (approx, NetClock) */
        99985687626634189ULL,          /* total_drops */
        parent_hash,
        10,                            /* base_fee */
        10000000,                      /* reserve (10 XRP) */
        2000000,                       /* increment (2 XRP) */
        0,                             /* apply_flags */
        0,                             /* network_id */
        sle_lookup, &db,
        ter_name, sizeof(ter_name),
        &applied);
    printf("apply result: TER=%d (%s) applied=%s\n", apply_ter, ter_name,
           applied ? "true" : "false");

    /* === APPLY WITH MUTATIONS === */
    printf("\n=== Calling xrpl_apply_with_mutations() ===\n");
    XrplApplyResult *result = xrpl_apply_with_mutations(
        tx_bytes, tx_len,
        NULL, 0,
        103354511, 797193960, 99985687626634189ULL,
        parent_hash, 10, 10000000, 2000000,
        0, 0,
        sle_lookup, &db);

    printf("result: TER=%d (%s) applied=%s drops_destroyed=%lld\n",
           xrpl_result_ter(result),
           xrpl_result_ter_name(result),
           xrpl_result_applied(result) ? "true" : "false",
           (long long)xrpl_result_drops_destroyed(result));

    size_t n_mutations = xrpl_result_mutation_count(result);
    printf("\nSLE mutations: %zu\n", n_mutations);
    const char *kind_names[] = {"Created", "Modified", "Deleted"};
    for (size_t i = 0; i < n_mutations; i++) {
        uint8_t key[32]; uint8_t kind;
        const uint8_t *data = NULL; size_t data_len = 0;
        if (xrpl_result_mutation_at(result, i, key, &kind, &data, &data_len)) {
            printf("  [%zu] %s  key=", i, kind_names[kind]);
            for (int j = 0; j < 8; j++) printf("%02X", key[j]);
            printf("...  %zu bytes\n", data_len);
            /* Scan for Balance field (0x62) and print drops */
            for (size_t j = 7; j + 9 < data_len; j++) {
                if (data[j] == 0x62) {
                    uint64_t raw = 0;
                    for (int k = 0; k < 8; k++) raw = (raw << 8) | data[j+1+k];
                    if ((raw & 0x8000000000000000ULL) == 0 && (raw & 0x4000000000000000ULL) != 0) {
                        uint64_t drops = raw & 0x3FFFFFFFFFFFFFFFULL;
                        if (drops <= 100000000000000000ULL) {
                            printf("         Balance: %lu drops\n", drops);
                            break;
                        }
                    }
                }
            }
        }
    }

    xrpl_result_destroy(result);

    return 0;
}

#include <string.h>
bool sle_lookup(void *user_data, const uint8_t key[32],
                const uint8_t **out_data, size_t *out_len) {
    struct SleDb {
        const uint8_t *sk; const uint8_t *sv; size_t slen;
        const uint8_t *dk; const uint8_t *dv; size_t dlen;
    } *db = (struct SleDb*)user_data;
    if (memcmp(key, db->sk, 32) == 0) { *out_data = db->sv; *out_len = db->slen; return true; }
    if (memcmp(key, db->dk, 32) == 0) { *out_data = db->dv; *out_len = db->dlen; return true; }
    return false;
}
