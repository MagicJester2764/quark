/* The environment, from C.
 *
 * Two libraries have to answer this: Quark's own, and musl through the
 * translation layer. This is built against the first, and the second is
 * checked by building the same source with x86_64-quark-musl-gcc.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int passed, failed;

static void check(const char *what, int ok) {
    if (ok) {
        passed++;
        printf("  ok    %s\n", what);
    } else {
        failed++;
        printf("  FAIL  %s\n", what);
    }
}

int main(void) {
    printf("environment (C):\n");
    check("HOME is set", getenv("HOME") != 0);
    check("HOME is a path", getenv("HOME") && getenv("HOME")[0] == '/');
    check("PATH is set", getenv("PATH") != 0);
    check("an unset name is null", getenv("NOPE") == 0);
    /* Without checking the '=', HOM matches HOME=/home/root. */
    check("a prefix does not match", getenv("HOM") == 0);

    check("setenv a new name", setenv("QUARK_T", "1", 1) == 0);
    check("and read it back", getenv("QUARK_T") && !strcmp(getenv("QUARK_T"), "1"));
    check("setenv without overwrite keeps the old",
          setenv("QUARK_T", "2", 0) == 0 && !strcmp(getenv("QUARK_T"), "1"));
    check("setenv with overwrite replaces it",
          setenv("QUARK_T", "2", 1) == 0 && !strcmp(getenv("QUARK_T"), "2"));
    check("unsetenv removes it", unsetenv("QUARK_T") == 0 && getenv("QUARK_T") == 0);
    /* What was there before must survive all of that. */
    check("HOME survived", getenv("HOME") != 0);

    printf("[envtest] %d passed, %d failed\n", passed, failed);
    return failed == 0 ? 0 : 1;
}
