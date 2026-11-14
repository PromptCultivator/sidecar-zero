#include "textflag.h"

// func CPURelax()
// ISB rather than YIELD, which is a no-op on most cores (sdk-go §5).
TEXT ·CPURelax(SB),NOSPLIT,$0-0
	ISB	$15
	RET
