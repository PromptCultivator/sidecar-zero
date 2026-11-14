#include "textflag.h"

// func CPURelax()
TEXT ·CPURelax(SB),NOSPLIT,$0-0
	PAUSE
	RET
