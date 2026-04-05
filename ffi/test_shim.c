/* test_shim.c — minimal C program to verify we can link against xrpl_shim.
 * Should print the shim version and rippled version.
 */

#include "xrpl_shim.h"
#include <stdio.h>

int main(void) {
    printf("xrpl_shim version:    %s\n", xrpl_shim_version());
    printf("xrpl libxrpl version: %s\n", xrpl_rippled_version());
    printf("\n");

    XrplEngine *engine = xrpl_engine_create();
    printf("Created engine: %p\n", (void *)engine);
    xrpl_engine_destroy(engine);
    printf("Destroyed engine successfully.\n");

    return 0;
}
