# The C++20 SDK: header-only library, its conformance agent, and its tests (sdks/cpp20).
# build.sh and test.sh drive this; OUT is target/sdk/cpp20 inside the dev container.
OUT ?= ../../target/sdk/cpp20
CXX ?= g++
STD := -std=c++20 -pthread -I include
WARN := -Wall -Wextra -Werror -Wpedantic
OPT := -O2 -g
HEADERS := $(wildcard include/scz/*.hpp include/scz/detail/*.hpp)

.PHONY: all agent tests sanitize
all: agent

agent: $(OUT)/scz-agent-cpp20 $(OUT)/crash/scz-agent-cpp20 $(OUT)/header-check.ok

# The agent arms the UNWIND test hook, so it is built with SCZ_TEST_HOOKS (sdk-cpp20 README).
$(OUT)/scz-agent-cpp20: agent/agent.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -DSCZ_TEST_HOOKS -o $@ agent/agent.cpp

# The crash-point variant (testing §2.4): kills itself at ring.enqueue.{claimed,written}.
$(OUT)/crash/scz-agent-cpp20: agent/agent.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -DSCZ_TEST_HOOKS -DSCZ_CRASHPOINTS -o $@ agent/agent.cpp

# The library alone, as an application includes it: no test hooks, with and without
# exceptions and RTTI, and with -Wconversion.
$(OUT)/header-check.ok: tests/header_check.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) -Wconversion -Wsign-conversion $(OPT) -fsyntax-only tests/header_check.cpp
	$(CXX) $(STD) $(WARN) -Wconversion -Wsign-conversion $(OPT) -fno-exceptions -fno-rtti -fsyntax-only tests/header_check.cpp
	@touch $@

TESTS := $(OUT)/tests/unit $(OUT)/tests/alloc_gate $(OUT)/tests/stress
tests: $(TESTS)

$(OUT)/tests/unit: tests/unit.cpp tests/local_arena.hpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -DSCZ_TEST_HOOKS -o $@ tests/unit.cpp

$(OUT)/tests/unit-nx: tests/unit.cpp tests/local_arena.hpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -fno-exceptions -fno-rtti -o $@ tests/unit.cpp

$(OUT)/tests/alloc_gate: tests/alloc_gate.cpp tests/local_arena.hpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -o $@ tests/alloc_gate.cpp

$(OUT)/tests/stress: tests/stress.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) $(OPT) -DSCZ_TEST_HOOKS -o $@ tests/stress.cpp

sanitize: $(OUT)/tests/unit-tsan $(OUT)/tests/unit-asan $(OUT)/tests/unit-nx $(OUT)/tests/stress-tsan $(OUT)/tests/stress-asan

$(OUT)/tests/unit-tsan: tests/unit.cpp tests/local_arena.hpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) -O1 -g -fsanitize=thread -Wno-tsan -DSCZ_TEST_HOOKS -o $@ tests/unit.cpp

$(OUT)/tests/unit-asan: tests/unit.cpp tests/local_arena.hpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all -DSCZ_TEST_HOOKS -o $@ tests/unit.cpp

$(OUT)/tests/stress-tsan: tests/stress.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) -O1 -g -fsanitize=thread -Wno-tsan -DSCZ_TEST_HOOKS -o $@ tests/stress.cpp

$(OUT)/tests/stress-asan: tests/stress.cpp $(HEADERS)
	@mkdir -p $(@D)
	$(CXX) $(STD) $(WARN) -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all -DSCZ_TEST_HOOKS -o $@ tests/stress.cpp
