#!/usr/bin/env bash
# Spike S5 - runs inside the scz-spike-s5 container. Writes raw output to /out (= results/ on the host).
# Every results file is rewritten from scratch by its redirect.
set -u
M=/work/models
S5=/work/s5
LIB=/opt/ort/lib/libonnxruntime.so.${ORT_VERSION}
INC=/opt/ort/include
QUICK=${S5_QUICK:-0}
CALLS=100000; [ "$QUICK" = 1 ] && CALLS=2000
JEMALLOC=$(dpkg -L libjemalloc2 | grep 'libjemalloc.so.2$')
say() { printf '\n######## %s\n' "$*"; }
T0=$(date +%s)
step() { echo "[$(( $(date +%s) - T0 ))s] $*"; }

# ---------------------------------------------------------------- 00 environment
step "00 environment"
{
  echo "date (UTC): $(date -u +%FT%TZ)"
  echo "uname -a  : $(uname -a)"
  echo "nproc     : $(nproc)"
  echo "page size : $(getconf PAGESIZE)"
  grep -E 'MemTotal|MemAvailable' /proc/meminfo
  echo "cgroup memory.max: $(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo n/a)"
  echo "glibc     : $(ldd --version | head -1)"
  echo "compiler  : $(g++ --version | head -1)"
  echo "jemalloc  : $(dpkg-query -W -f='${Package} ${Version}' libjemalloc2) ($JEMALLOC)"
  echo "ONNX Runtime C library: $LIB"
  echo "  VERSION_NUMBER: $(cat /opt/ort/VERSION_NUMBER)   GIT_COMMIT_ID: $(cat /opt/ort/GIT_COMMIT_ID)"
  echo "  sha256 of the .so: $(sha256sum "$LIB" | cut -d' ' -f1)"
  echo "python packages: $(pip list 2>/dev/null | grep -Ei '^(onnx|onnxruntime|numpy) ' | tr -s ' ' | tr '\n' ';')"
  echo "CPU (first core in /proc/cpuinfo):"; grep -m3 -E 'CPU implementer|CPU part|Features' /proc/cpuinfo | sed 's/^/  /'
  echo "S5_QUICK=$QUICK (latency calls per run: $CALLS)"
  echo; echo "models (generated at image build):"; ls -l $M | sed 's/^/  /'
  echo; echo "model generation log:"; sed 's/^/  /' $M/gen.log
} > /out/00-environment.txt 2>&1

# ---------------------------------------------------------------- 01 option names (e)
step "01 option names"
{
  say "session option keys, as spelled in include/onnxruntime_session_options_config_keys.h"
  grep -n -E '"session\.use_ort_model_bytes_directly"|"session\.use_ort_model_bytes_for_initializers"|"session\.use_memory_mapped_ort_model"|"session\.use_env_allocators"|"session\.(intra|inter)_op\.allow_spinning"|"session\.enable_cpu_mem_arena"|"session\.enable_mem_pattern"' \
      -B1 $INC/onnxruntime_session_options_config_keys.h
  say "run option key, as spelled in include/onnxruntime_run_options_config_keys.h"
  grep -n -B8 '"memory.enable_memory_arena_shrinkage"' $INC/onnxruntime_run_options_config_keys.h
  say "the same strings inside the shipped binary (strings -a libonnxruntime.so)"
  strings -a "$LIB" | grep -n -E '^session\.use_ort_model_bytes|^session\.use_memory_mapped_ort_model|^session\.use_env_allocators$|^memory\.enable_memory_arena_shrinkage$'
  say "arena configuration keys accepted by OrtApi::CreateArenaCfgV2 (include/onnxruntime_c_api.h)"
  grep -n -E '^\s+\* "(max_mem|arena_extend_strategy|initial_chunk_size_bytes|max_dead_bytes_per_chunk|initial_growth_chunk_size_bytes|max_power_of_two_extend_bytes)"' $INC/onnxruntime_c_api.h
  say "s5 options"
  $S5 options
} > /out/01-option-names.txt 2>&1

# ---------------------------------------------------------------- 02 zero copy (a)
step "02 zero copy"
{
  for model in tree.ort mlp.ort; do
    for rows in 1 32; do
      LD_PRELOAD=/work/libmhook.so $S5 zerocopy $M/$model $rows A
    done
  done
  # Two more row counts for the tree model, to see which allocations grow with the request.
  for rows in 2 64; do
    LD_PRELOAD=/work/libmhook.so $S5 zerocopy $M/tree.ort $rows A
  done
  # Profile B: allocations come from the arena, so the allocation trace says little; pointers and first touch still apply.
  for model in tree.ort mlp.ort; do
    LD_PRELOAD=/work/libmhook.so $S5 zerocopy $M/$model 32 B
  done
} > /out/02-zero-copy.txt 2>&1
# Disassemble around every distinct faulting instruction, so the finding can say what kind of code touched the slot.
{
  echo "Instructions at the program counters recorded in 02-zero-copy.txt ('=>' marks the faulting one)."
  echo "The library is stripped: there are no function names, the instruction itself is the evidence."
  grep -o 'module=[^ ]* offset=0x[0-9a-f]*' /out/02-zero-copy.txt | sort | uniq -c | sort -rn | while read -r count mod off; do
    mod=${mod#module=}; off=${off#offset=}
    echo; echo "---- $mod $off  (seen $count times)"
    start=$(( off - 24 )); stop=$(( off + 16 ))
    objdump -d --no-show-raw-insn --start-address=$start --stop-address=$stop "$mod" 2>/dev/null \
      | grep -E '^\s+[0-9a-f]+:' \
      | awk -v pc="$(printf '%x' $(( off )))" '{ a=$1; sub(":","",a); printf "%s %s\n", (a==pc ? "=>" : "  "), $0 }'
  done
} > /out/02-zero-copy-disasm.txt 2>&1

# ---------------------------------------------------------------- 03 fixed cost per call (b)
step "03 fixed cost per call"
{
  echo "Laptop VM numbers (Docker Desktop's Linux VM on Apple silicon). Order of magnitude only."
  echo "One thread: intra_op=1, inter_op=1, sequential, spinning off. Each call uses a different request."
  echo "variant bound-once    = RunWithBinding only; the binding was made once"
  echo "variant bind-per-call = 2x CreateTensorWithDataAsOrtValue over the request's slot + BindInput + BindOutput + RunWithBinding + release"
  echo "variant run-per-call  = 2x CreateTensorWithDataAsOrtValue + Run() with a pre-allocated output + release (no IoBinding)"
  echo "tree1.ort is one tree of depth 1: its Run is the runtime's fixed cost with almost no arithmetic."
  echo
  for model in tree1.ort tree.ort; do
    for v in bound-once bind-per-call run-per-call; do $S5 latency $M/$model $v $CALLS 1 A; done
    $S5 latency $M/$model bind-per-call $CALLS 1 B
  done
  $S5 latency $M/tree.ort bind-per-call $(( CALLS / 5 )) 32 A
  echo
  for v in bound-once bind-per-call; do $S5 latency $M/mlp.ort $v $CALLS 1 A; done
  $S5 latency $M/mlp.ort bind-per-call $CALLS 1 B
} > /out/03-fixed-cost.txt 2>&1

# ---------------------------------------------------------------- 04 memory profiles (c)
step "04 memory profiles"
{
  echo "RSS from /proc/self/statm (resident, shared) at each phase boundary; every sample is in the CSV named after the run."
  echo "'rss-shared' is resident minus file and shmem pages, so the slot (a 5 MB memfd) and the mapped model are not in it."
  echo "Peak RSS inside a phase is VmHWM from /proc/self/status, reset at each boundary."
  echo
  $S5 mem $M/mlp.ort  A         /out/04-mem-mlp-A-glibc.csv
  LD_PRELOAD=$JEMALLOC $S5 mem $M/mlp.ort A /out/04-mem-mlp-A-jemalloc.csv
  $S5 mem $M/mlp.ort  B         /out/04-mem-mlp-B-capped-glibc.csv
  LD_PRELOAD=$JEMALLOC $S5 mem $M/mlp.ort B /out/04-mem-mlp-B-capped-jemalloc.csv
  $S5 mem $M/mlp.ort  B-default /out/04-mem-mlp-B-default-glibc.csv
  $S5 mem $M/tree.ort A         /out/04-mem-tree-A-glibc.csv
  $S5 mem $M/tree.ort B         /out/04-mem-tree-B-capped-glibc.csv
} > /out/04-memory-profiles.txt 2>&1

# ---------------------------------------------------------------- 05 model from a mapped descriptor (d)
step "05 model load"
{
  echo "All values in kB. 'model-file mapping' sums the /proc/self/smaps entries backed by the model file."
  echo "Session options otherwise as profile A. Variants:"
  echo "  bytes+init = mmap read-only, CreateSessionFromArray, session.use_ort_model_bytes_directly=1 and"
  echo "               session.use_ort_model_bytes_for_initializers=1 (what the design names)"
  echo "  bytes      = the same with only session.use_ort_model_bytes_directly=1"
  echo "  buffer     = mmap read-only, CreateSessionFromArray, neither option"
  echo "  path       = CreateSession by file name, neither option"
  echo "  path-mmap  = CreateSession by file name, session.use_memory_mapped_ort_model=1 and"
  echo "               session.use_ort_model_bytes_for_initializers=1"
  say "one process"
  for v in bytes+init bytes buffer path path-mmap; do $S5 load $M/mlp.ort $v; done
  for v in buffer path; do $S5 load $M/mlp.onnx $v; done
  for v in bytes+init buffer; do $S5 load $M/tree.ort $v; done
  $S5 load $M/tree.onnx buffer
  say "two processes alive at once"
  python /work/share.py $M/mlp.ort bytes+init
  python /work/share.py $M/mlp.onnx buffer
  python /work/share.py $M/mlp.ort path-mmap
  python /work/share.py $M/tree.ort bytes+init
} > /out/05-model-load.txt 2>&1

# ---------------------------------------------------------------- 06 RLIMIT_DATA backstop (extra)
step "06 RLIMIT_DATA"
{
  $S5 rlimit $M/mlp.ort; echo "exit code: $?"; echo
  LD_PRELOAD=$JEMALLOC $S5 rlimit $M/mlp.ort; echo "exit code: $?"
} > /out/06-rlimit-data.txt 2>&1

step "done"
ls -l /out
grep -h '^RESULT' /out/02-zero-copy.txt
