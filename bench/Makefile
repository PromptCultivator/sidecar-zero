# The chaos benchmark (bench §6). From the repository root:
#
#   make -C bench chaos-bench          the full run: every system, scenario, and host language,
#                                      five repetitions, 1,000 kills; both modes. Hours.
#   make -C bench chaos-bench-quick    laptop scale (bench §6 "Hardware expectations"): one
#                                      repetition, 10 kills, the spinning regime at 30,000/s
#   make -C bench chaos-bench-docker   in-process mode only, without Kind
#   make -C bench report RESULTS=bench/results/<timestamp>    regenerate a report
#
# Variables: SYSTEMS (A,B,C,D,E), SCENARIOS (steady,kill,freeze,reload,overload,idle), REPEATS,
# KILLS, HOST_LANG (rust,java), MODES. Results: bench/results/<timestamp>/report.md.

ROOT := $(abspath ..)
SYSTEMS ?= A,B,C,D,E
SCENARIOS ?= steady,kill,freeze,reload,overload,idle
HOST_LANG ?= rust,java
MODES ?= inproc,service
OPTS = --systems=$(SYSTEMS) --scenarios=$(SCENARIOS) --hosts=$(HOST_LANG) $(if $(REPEATS),--repeats=$(REPEATS)) $(if $(KILLS),--kills=$(KILLS))

.PHONY: chaos-bench chaos-bench-quick chaos-bench-docker report

chaos-bench:
	$(ROOT)/scripts/bench --modes=$(MODES) $(OPTS)

chaos-bench-quick:
	$(ROOT)/scripts/bench --quick --modes=$(MODES) $(OPTS)

chaos-bench-docker:
	$(ROOT)/scripts/bench --quick --modes=inproc $(OPTS)

report:
	$(ROOT)/scripts/dev python3 bench/report/report.py $(RESULTS)
