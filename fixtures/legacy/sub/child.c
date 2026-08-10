#include "legacy.h"
#if !LEGACY_MODE
#error "need legacy mode"
#endif
int child_fn(void) { return 2; }
