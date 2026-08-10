#include "legacy.h"
#if !LEGACY_MODE
#error "need legacy mode"
#endif
int core_fn(void) { return 1; }
