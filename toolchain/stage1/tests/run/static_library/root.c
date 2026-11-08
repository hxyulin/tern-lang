#include <math.h>
#include <stdint.h>

// Needs libm on Linux, so the test also links `-l m`.
int64_t lib_root(int64_t x) { return (int64_t)sqrt((double)x); }
