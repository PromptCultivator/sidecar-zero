------------------------------- MODULE Consumers -------------------------------
(******************************************************************************)
(* Spec B of testing §2.1: changing consumers.                                *)
(*                                                                            *)
(* The old worker, its successor, fresh replacements (one per failure),      *)
(* and the supervisor with the gate, ctl_req's HANDOVER bit, the status-pipe  *)
(* reports (WARM, ACTIVE, RELEASED), each consume thread's death lock, and    *)
(* the exit notification (pidfd). One ring. Producers are reduced to "a       *)
(* request may be published at any moment"; clients may cancel, abandon, and  *)
(* release at any moment.                                                     *)
(*                                                                            *)
(* Scenario "crash": the active worker may fail and the warm standby is       *)
(* promoted (supervisor §5.3). "same": a reload with the same schema, handed  *)
(* over with the gate healthy (§5.6). "changed": a reload with a changed      *)
(* schema, handed over behind the gate. In every scenario any worker may die  *)
(* or stall at any step, up to MaxFailures times, and a stall may end.        *)
(*                                                                            *)
(* A worker's consume thread dying is what its death lock reports at once. The*)
(* process exits later: until then its runtime threads may still write a slot *)
(* it was processing, so it keeps its claim on that slot until the exit.      *)
(*                                                                            *)
(* Spec A (SlotsRing.tla) checks the waits, the ring's words, and the client; *)
(* here those are abstracted and the subject is who may consume.              *)
(******************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Scenario,        \* "crash" | "same" | "changed"
    MaxFailures,     \* worker deaths and stalls, in all
    MaxRequests,     \* requests the producers publish, in all
    NSlots,
    CAP,
    Pressure,        \* whether the memory monitor may hold the gate closed
    ClientsCancel,   \* whether clients may cancel and abandon (off: every request waits for an answer)
    Bug              \* "none", or one seeded mistake

\* 1 is active, 2 the warm standby or the new model's worker, and one fresh spawn for each
\* failure: the real supervisor keeps respawning (§5.5), so with failures bounded some worker
\* always survives them.
GENS == 1..(MaxFailures + 2)
CT(g) == 10 + g                 \* consume thread of generation g
CTs == {CT(h) : h \in GENS}
SUP == 30
ENV == 40
Slots == 1..NSlots
MaxEpoch == MaxRequests + 1
Free(e) == [e |-> e, st |-> "FREE", o |-> 0]
Fresh == [i \in 0..(CAP - 1) |-> [seq |-> i, s |-> 0, e |-> 0]]
NoDesc == [s |-> 0, e |-> 0]
CLIENT == 0                     \* the client, as a holder of a slot's data

(* --algorithm Consumers {
variables
    slot = [x \in Slots |-> Free(0)],
    cells = Fresh,
    tail = 0,
    head = 0,
    gate = [st |-> "HEALTHY", gen |-> 1],
    holds = {},                                    \* "DEAD", "RELOAD", "PSI"
    ctlHandover = FALSE,
    \* each generation, as the supervisor and the kernel see it
    spawned = [h \in GENS |-> h \in {1, 2}],
    beat = [h \in GENS |-> IF h <= 2 THEN h ELSE 0],  \* the beat slot + 1 it writes to OWNER
    ctAlive = [h \in GENS |-> h \in {1, 2}],       \* its consume thread runs (death lock held)
    procAlive = [h \in GENS |-> h \in {1, 2}],     \* not yet exited (pidfd not readable)
    stalled = [h \in GENS |-> FALSE],
    warm = [h \in GENS |-> h \in {1, 2}],          \* reported WARM
    active = [h \in GENS |-> h = 1],               \* reported ACTIVE
    released = [h \in GENS |-> FALSE],             \* reported RELEASED
    consumer = 1,                                  \* the generation the supervisor treats as the ring consumer
    phase = "steady",                              \* steady | handover | await_active | await_warm | down
    reloadStarted = FALSE,
    deathSeen = [h \in GENS |-> FALSE],
    reaped = [h \in GENS |-> FALSE],
    pressure = FALSE,
    failures = 0,
    published = 0,
    \* ghosts
    holders = [x \in Slots |-> {}],                \* who may write the slot's data now
    served = [key \in Slots \X (0..MaxEpoch) |-> 0],
    needClosed = {};                               \* paths that need the gate closed, by the design's own account

define {
    Run(g) == ctAlive[g] /\ ~stalled[g]
    GateOf(hs, g) ==
        IF hs = {} THEN [st |-> "HEALTHY", gen |-> g]
        ELSE IF "DEAD" \in hs THEN [st |-> "DEAD", gen |-> g]
        ELSE IF "RELOAD" \in hs THEN [st |-> "RELOADING", gen |-> g]
        ELSE [st |-> "PSI_SHEDDING", gen |-> g]
    Published(p) == cells[p % CAP].seq = p + 1
    \* A warm worker that has never been active, alive, and not stalled for good
    Successor == {h \in GENS : spawned[h] /\ warm[h] /\ ~active[h] /\ ctAlive[h] /\ h # consumer}
    FreeBeat == {b \in {1, 2} : \A h \in GENS : spawned[h] /\ beat[h] = b => reaped[h]}
}

\* ── consume threads, one per generation (worker §3–4, shm-abi §4.6) ─────────────────
fair process (C \in CTs)
variables hl = 0, d = NoDesc;
{
c_load:   \* load and warm up, then report WARM (fresh spawns only: 1 and 2 start warm)
    await spawned[self - 10] /\ Run(self - 10);
    warm[self - 10] := TRUE;
c_standby:   \* park on standby_wait until the gate carries this generation (signaling §3.3)
    await Run(self - 10) /\ gate.gen = self - 10;
    hl := head;
    active[self - 10] := TRUE;                      \* reports ACTIVE
c_loop:
    while (TRUE) {
        await Run(self - 10) /\ (gate.gen # self - 10 \/ ctlHandover \/ Published(hl));
        if (gate.gen # self - 10) {
            goto c_exit;                           \* defensive: no longer the authoritative generation
        } else if (ctlHandover) {
            released[self - 10] := TRUE;           \* stops at the loop boundary: RELEASED
            goto c_exit;
        } else {
            d := [s |-> cells[hl % CAP].s, e |-> cells[hl % CAP].e];
        };
c_stepA:
        await Run(self - 10);
        cells[hl % CAP].seq := hl + CAP;
c_stepB:
        await Run(self - 10);
        head := hl + 1;
        hl := hl + 1;
c_claim:
        await Run(self - 10);
        if (slot[d.s].st = "READY" /\ slot[d.s].e = d.e) {
            slot[d.s] := [e |-> d.e, st |-> "PROCESSING", o |-> beat[self - 10]];
            holders[d.s] := holders[d.s] \cup {self - 10};
        } else { d := NoDesc; goto c_loop; };
c_complete:   \* inference is done and the response written; complete the slot
        await Run(self - 10);
        if (slot[d.s].e = d.e /\ slot[d.s].st = "PROCESSING") {
            slot[d.s] := [e |-> d.e, st |-> "DONE", o |-> 0];
            served[<<d.s, d.e>>] := served[<<d.s, d.e>>] + 1;
        } else if (slot[d.s].e = d.e /\ slot[d.s].st = "ABANDONED") {
            slot[d.s] := Free(d.e);
        };
        holders[d.s] := holders[d.s] \ {self - 10};
        d := NoDesc;
    };
c_exit:
    skip;
}

\* ── the supervisor (supervisor §5.3–5.7): one loop, one event at a time ───────────────
fair process (S = SUP)
variables g = 0, i = 1;
{
s_loop:
    while (TRUE) {
        either {
            \* Start the reload, if the scenario is one.
            await Scenario \in {"same", "changed"} /\ ~reloadStarted /\ phase = "steady"
                  /\ 2 \in Successor;
            reloadStarted := TRUE;
            if (Scenario = "changed") {
                holds := holds \cup {"RELOAD"};
                gate := GateOf(holds \cup {"RELOAD"}, gate.gen);
                needClosed := needClosed \cup {"reload"};
            };
            ctlHandover := TRUE;                   \* and wake the consumer's parked threads
            phase := "handover";
s_start2:
            if (Bug = "handover_no_release") {     \* seeded: switch generations without waiting
                consumer := 2;
                gate := GateOf(holds, 2);
                phase := "await_active";
            };
        } or {
            \* RELEASED from the consumer during a handover.
            await phase = "handover" /\ released[consumer];
            ctlHandover := FALSE;
            if (Scenario = "changed" \/ Successor = {}) {
                \* queued requests were encoded for the old model, or nobody is left to serve them
                if (Successor = {}) {
                    holds := holds \cup {"RELOAD"};
                    gate := GateOf(holds \cup {"RELOAD"}, gate.gen);
                    needClosed := needClosed \cup {"reload"};
                };
                i := 1;
s_rel_fail:
                while (i <= NSlots) {
                    if (slot[i].st = "READY") { slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.o = 0]; };
                    i := i + 1;
                };
            };
s_rel_activate:
            if (Successor # {}) {
                with (h \in Successor) {
                    consumer := h;
                    gate := GateOf(holds, h);      \* one store: the generation changes, the state does not
                };
                phase := "await_active";
            } else { phase := "await_warm"; };
        } or {
            \* ACTIVE from the new consumer: drop this path's holds.
            await phase = "await_active" /\ active[consumer];
            if (Bug = "store_healthy") {
                gate := [st |-> "HEALTHY", gen |-> gate.gen];
                holds := holds \ {"DEAD", "RELOAD"};
            } else {
                holds := holds \ {"DEAD", "RELOAD"};
                gate := GateOf(holds \ {"DEAD", "RELOAD"}, gate.gen);
            };
            needClosed := needClosed \ {"death", "reload"};
            phase := "steady";
        } or {
            \* The consumer's death lock reported (or its pidfd, if it died before taking it):
            \* the worker died as the ring consumer, meaning it had been activated (whether or
            \* not it had reported ACTIVE yet) and had not released.
            await ~ctAlive[consumer] /\ ~released[consumer] /\ ~deathSeen[consumer]
                  /\ phase \in {"steady", "handover", "await_active"};
            g := consumer;
            deathSeen[g] := TRUE;
            holds := holds \cup {"DEAD"};
            gate := GateOf(holds \cup {"DEAD"}, gate.gen);
            needClosed := needClosed \cup {"death"};
            ctlHandover := FALSE;                  \* a handover in progress ends on the crash path
            i := 1;
s_d_fail:   \* fail every READY slot (and, seeded, the slots it was processing: too early)
            while (i <= NSlots) {
                if (slot[i].st = "READY") { slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.o = 0]; }
                else if (Bug = "fail_owned_on_lock" /\ slot[i].st = "PROCESSING" /\ slot[i].o = beat[g]) {
                    slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.o = 0];
                };
                i := i + 1;
            };
s_d_repair:   \* repair the ring, once its own consume thread is gone (the lock says so)
            assert ~ctAlive[g];
            if (cells[head % CAP].seq >= head + CAP) { head := head + 1; };
s_d_next:
            if (Successor # {}) {
                with (h \in Successor) { consumer := h; gate := GateOf(holds, h); };
                phase := "await_active";
            } else { phase := "await_warm"; };
            g := 0;
        } or {
            \* A worker's pidfd: fail or free the slots it held, by OWNER.
            await \E h \in GENS : spawned[h] /\ ~procAlive[h] /\ ~reaped[h];
            with (h \in {x \in GENS : spawned[x] /\ ~procAlive[x] /\ ~reaped[x]}) { g := h; };
            i := 1;
s_p_owned:
            while (i <= NSlots) {
                if (slot[i].o = beat[g] /\ slot[i].st = "PROCESSING") {
                    slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.o = 0];
                } else if (slot[i].o = beat[g] /\ slot[i].st = "ABANDONED") {
                    slot[i] := Free(slot[i].e);
                };
                i := i + 1;
            };
s_p_done:
            reaped[g] := TRUE;
            g := 0;
        } or {
            \* No worker can serve: spawn a fresh one into a free beat slot (§5.5), one at a time.
            await phase = "await_warm" /\ Successor = {} /\ FreeBeat # {}
                  /\ (\E h \in GENS : ~spawned[h])
                  /\ ~(\E h \in GENS : spawned[h] /\ ctAlive[h] /\ ~warm[h]);
            with (h \in {x \in GENS : ~spawned[x] /\ \A y \in GENS : y < x => spawned[y]}) {
                with (b \in FreeBeat) { beat[h] := b; };
                spawned[h] := TRUE;
                ctAlive[h] := TRUE;
                procAlive[h] := TRUE;
            };
        } or {
            \* The fresh worker is warm: activate it.
            await phase = "await_warm" /\ Successor # {};
            with (h \in Successor) { consumer := h; gate := GateOf(holds, h); };
            phase := "await_active";
        } or {
            \* Timeouts (§5.2): no RELEASED within the handover timeout, no ACTIVE after
            \* activation, a hung consumer, or a released worker that has not exited. Each one
            \* condemns the worker; the timeout can fire only while the worker is stalled.
            await \E h \in GENS : spawned[h] /\ ctAlive[h] /\ stalled[h];
            with (h \in {x \in GENS : spawned[x] /\ ctAlive[x] /\ stalled[x]}) {
                if (Bug = "promote_on_kill" /\ h = consumer /\ active[h]) {
                    \* activate the standby on sending SIGKILL, without waiting for the death lock
                    with (n \in Successor) { consumer := n; gate := GateOf(holds, n); };
                    phase := "await_active";
                } else {
                    ctAlive[h] := FALSE;               \* SIGKILL: the lock reports at once
                };
            };
        } or {
            \* A released worker exits in its own time; a dead one's process exits.
            await \E h \in GENS : procAlive[h] /\ (~ctAlive[h] \/ released[h]);
            with (h \in {x \in GENS : procAlive[x] /\ (~ctAlive[x] \/ released[x])}) {
                ctAlive[h] := FALSE;
                procAlive[h] := FALSE;
                holders := [x \in Slots |-> holders[x] \ {h}];
            };
        } or {
            \* The memory monitor (§6): it holds the gate while there is pressure.
            await Pressure /\ (pressure # ("PSI" \in holds));
            if (pressure) {
                holds := holds \cup {"PSI"};
                gate := GateOf(holds \cup {"PSI"}, gate.gen);
                needClosed := needClosed \cup {"psi"};
            } else {
                holds := holds \ {"PSI"};
                gate := GateOf(holds \ {"PSI"}, gate.gen);
                needClosed := needClosed \ {"psi"};
            };
        };
    };
}

\* ── producers and clients, abstracted ─────────────────────────────────────────────────
process (P = ENV)
variables px = 0;
{
p_loop:
    while (TRUE) {
        either {
            \* publish a request: claim a slot, write it, READY, enqueue (shm-abi §4.3, §5.3)
            await published < MaxRequests /\ gate.st = "HEALTHY" /\ \E y \in Slots : slot[y].st = "FREE";
            with (y \in {z \in Slots : slot[z].st = "FREE"}) {
                px := y;
                slot[y] := [e |-> slot[y].e + 1, st |-> "CLAIMED", o |-> 0];
                holders[y] := holders[y] \cup {CLIENT};
            };
            published := published + 1;
p_ready:
            holders[px] := holders[px] \ {CLIENT};
            if (cells[tail % CAP].seq = tail) {
                slot[px] := [slot[px] EXCEPT !.st = "READY"];
                cells[tail % CAP] := [seq |-> tail + 1, s |-> px, e |-> slot[px].e];
                tail := tail + 1;
            } else {
                slot[px] := Free(slot[px].e);            \* the ring is full: fall back
            };
            px := 0;
        } or {
            \* a client reads its answer, or its failure, and releases the slot
            await \E y \in Slots : slot[y].st \in {"DONE", "FAILED"};
            with (y \in {z \in Slots : slot[z].st \in {"DONE", "FAILED"}}) { slot[y] := Free(slot[y].e); };
        } or {
            \* a client cancels a queued request, or abandons one being processed
            await ClientsCancel /\ \E y \in Slots : slot[y].st \in {"READY", "PROCESSING"};
            with (y \in {z \in Slots : slot[z].st \in {"READY", "PROCESSING"}}) {
                if (slot[y].st = "READY") { slot[y] := Free(slot[y].e); }
                else { slot[y] := [slot[y] EXCEPT !.st = "ABANDONED"]; };
            };
        } or {
            \* a worker dies (its consume thread first; the process exits later) ...
            await failures < MaxFailures /\ \E h \in GENS : ctAlive[h];
            with (h \in {z \in GENS : ctAlive[z]}) { ctAlive[h] := FALSE; };
            failures := failures + 1;
        } or {
            \* ... or stalls, for as long as it likes
            await failures < MaxFailures /\ \E h \in GENS : ctAlive[h] /\ ~stalled[h];
            with (h \in {z \in GENS : ctAlive[z] /\ ~stalled[z]}) { stalled[h] := TRUE; };
            failures := failures + 1;
        } or {
            await \E h \in GENS : stalled[h];
            with (h \in {z \in GENS : stalled[z]}) { stalled[h] := FALSE; };
        } or {
            await Pressure;
            pressure := ~pressure;
        };
    };
}
} *)
\* BEGIN TRANSLATION
VARIABLES slot, cells, tail, head, gate, holds, ctlHandover, spawned, beat, 
          ctAlive, procAlive, stalled, warm, active, released, consumer, 
          phase, reloadStarted, deathSeen, reaped, pressure, failures, 
          published, holders, served, needClosed, pc

(* define statement *)
Run(g) == ctAlive[g] /\ ~stalled[g]
GateOf(hs, g) ==
    IF hs = {} THEN [st |-> "HEALTHY", gen |-> g]
    ELSE IF "DEAD" \in hs THEN [st |-> "DEAD", gen |-> g]
    ELSE IF "RELOAD" \in hs THEN [st |-> "RELOADING", gen |-> g]
    ELSE [st |-> "PSI_SHEDDING", gen |-> g]
Published(p) == cells[p % CAP].seq = p + 1

Successor == {h \in GENS : spawned[h] /\ warm[h] /\ ~active[h] /\ ctAlive[h] /\ h # consumer}
FreeBeat == {b \in {1, 2} : \A h \in GENS : spawned[h] /\ beat[h] = b => reaped[h]}

VARIABLES hl, d, g, i, px

vars == << slot, cells, tail, head, gate, holds, ctlHandover, spawned, beat, 
           ctAlive, procAlive, stalled, warm, active, released, consumer, 
           phase, reloadStarted, deathSeen, reaped, pressure, failures, 
           published, holders, served, needClosed, pc, hl, d, g, i, px >>

ProcSet == (CTs) \cup {SUP} \cup {ENV}

Init == (* Global variables *)
        /\ slot = [x \in Slots |-> Free(0)]
        /\ cells = Fresh
        /\ tail = 0
        /\ head = 0
        /\ gate = [st |-> "HEALTHY", gen |-> 1]
        /\ holds = {}
        /\ ctlHandover = FALSE
        /\ spawned = [h \in GENS |-> h \in {1, 2}]
        /\ beat = [h \in GENS |-> IF h <= 2 THEN h ELSE 0]
        /\ ctAlive = [h \in GENS |-> h \in {1, 2}]
        /\ procAlive = [h \in GENS |-> h \in {1, 2}]
        /\ stalled = [h \in GENS |-> FALSE]
        /\ warm = [h \in GENS |-> h \in {1, 2}]
        /\ active = [h \in GENS |-> h = 1]
        /\ released = [h \in GENS |-> FALSE]
        /\ consumer = 1
        /\ phase = "steady"
        /\ reloadStarted = FALSE
        /\ deathSeen = [h \in GENS |-> FALSE]
        /\ reaped = [h \in GENS |-> FALSE]
        /\ pressure = FALSE
        /\ failures = 0
        /\ published = 0
        /\ holders = [x \in Slots |-> {}]
        /\ served = [key \in Slots \X (0..MaxEpoch) |-> 0]
        /\ needClosed = {}
        (* Process C *)
        /\ hl = [self \in CTs |-> 0]
        /\ d = [self \in CTs |-> NoDesc]
        (* Process S *)
        /\ g = 0
        /\ i = 1
        (* Process P *)
        /\ px = 0
        /\ pc = [self \in ProcSet |-> CASE self \in CTs -> "c_load"
                                        [] self = SUP -> "s_loop"
                                        [] self = ENV -> "p_loop"]

c_load(self) == /\ pc[self] = "c_load"
                /\ spawned[self - 10] /\ Run(self - 10)
                /\ warm' = [warm EXCEPT ![self - 10] = TRUE]
                /\ pc' = [pc EXCEPT ![self] = "c_standby"]
                /\ UNCHANGED << slot, cells, tail, head, gate, holds, 
                                ctlHandover, spawned, beat, ctAlive, procAlive, 
                                stalled, active, released, consumer, phase, 
                                reloadStarted, deathSeen, reaped, pressure, 
                                failures, published, holders, served, 
                                needClosed, hl, d, g, i, px >>

c_standby(self) == /\ pc[self] = "c_standby"
                   /\ Run(self - 10) /\ gate.gen = self - 10
                   /\ hl' = [hl EXCEPT ![self] = head]
                   /\ active' = [active EXCEPT ![self - 10] = TRUE]
                   /\ pc' = [pc EXCEPT ![self] = "c_loop"]
                   /\ UNCHANGED << slot, cells, tail, head, gate, holds, 
                                   ctlHandover, spawned, beat, ctAlive, 
                                   procAlive, stalled, warm, released, 
                                   consumer, phase, reloadStarted, deathSeen, 
                                   reaped, pressure, failures, published, 
                                   holders, served, needClosed, d, g, i, px >>

c_loop(self) == /\ pc[self] = "c_loop"
                /\ Run(self - 10) /\ (gate.gen # self - 10 \/ ctlHandover \/ Published(hl[self]))
                /\ IF gate.gen # self - 10
                      THEN /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                           /\ UNCHANGED << released, d >>
                      ELSE /\ IF ctlHandover
                                 THEN /\ released' = [released EXCEPT ![self - 10] = TRUE]
                                      /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                                      /\ d' = d
                                 ELSE /\ d' = [d EXCEPT ![self] = [s |-> cells[hl[self] % CAP].s, e |-> cells[hl[self] % CAP].e]]
                                      /\ pc' = [pc EXCEPT ![self] = "c_stepA"]
                                      /\ UNCHANGED released
                /\ UNCHANGED << slot, cells, tail, head, gate, holds, 
                                ctlHandover, spawned, beat, ctAlive, procAlive, 
                                stalled, warm, active, consumer, phase, 
                                reloadStarted, deathSeen, reaped, pressure, 
                                failures, published, holders, served, 
                                needClosed, hl, g, i, px >>

c_stepA(self) == /\ pc[self] = "c_stepA"
                 /\ Run(self - 10)
                 /\ cells' = [cells EXCEPT ![hl[self] % CAP].seq = hl[self] + CAP]
                 /\ pc' = [pc EXCEPT ![self] = "c_stepB"]
                 /\ UNCHANGED << slot, tail, head, gate, holds, ctlHandover, 
                                 spawned, beat, ctAlive, procAlive, stalled, 
                                 warm, active, released, consumer, phase, 
                                 reloadStarted, deathSeen, reaped, pressure, 
                                 failures, published, holders, served, 
                                 needClosed, hl, d, g, i, px >>

c_stepB(self) == /\ pc[self] = "c_stepB"
                 /\ Run(self - 10)
                 /\ head' = hl[self] + 1
                 /\ hl' = [hl EXCEPT ![self] = hl[self] + 1]
                 /\ pc' = [pc EXCEPT ![self] = "c_claim"]
                 /\ UNCHANGED << slot, cells, tail, gate, holds, ctlHandover, 
                                 spawned, beat, ctAlive, procAlive, stalled, 
                                 warm, active, released, consumer, phase, 
                                 reloadStarted, deathSeen, reaped, pressure, 
                                 failures, published, holders, served, 
                                 needClosed, d, g, i, px >>

c_claim(self) == /\ pc[self] = "c_claim"
                 /\ Run(self - 10)
                 /\ IF slot[d[self].s].st = "READY" /\ slot[d[self].s].e = d[self].e
                       THEN /\ slot' = [slot EXCEPT ![d[self].s] = [e |-> d[self].e, st |-> "PROCESSING", o |-> beat[self - 10]]]
                            /\ holders' = [holders EXCEPT ![d[self].s] = holders[d[self].s] \cup {self - 10}]
                            /\ pc' = [pc EXCEPT ![self] = "c_complete"]
                            /\ d' = d
                       ELSE /\ d' = [d EXCEPT ![self] = NoDesc]
                            /\ pc' = [pc EXCEPT ![self] = "c_loop"]
                            /\ UNCHANGED << slot, holders >>
                 /\ UNCHANGED << cells, tail, head, gate, holds, ctlHandover, 
                                 spawned, beat, ctAlive, procAlive, stalled, 
                                 warm, active, released, consumer, phase, 
                                 reloadStarted, deathSeen, reaped, pressure, 
                                 failures, published, served, needClosed, hl, 
                                 g, i, px >>

c_complete(self) == /\ pc[self] = "c_complete"
                    /\ Run(self - 10)
                    /\ IF slot[d[self].s].e = d[self].e /\ slot[d[self].s].st = "PROCESSING"
                          THEN /\ slot' = [slot EXCEPT ![d[self].s] = [e |-> d[self].e, st |-> "DONE", o |-> 0]]
                               /\ served' = [served EXCEPT ![<<d[self].s, d[self].e>>] = served[<<d[self].s, d[self].e>>] + 1]
                          ELSE /\ IF slot[d[self].s].e = d[self].e /\ slot[d[self].s].st = "ABANDONED"
                                     THEN /\ slot' = [slot EXCEPT ![d[self].s] = Free(d[self].e)]
                                     ELSE /\ TRUE
                                          /\ slot' = slot
                               /\ UNCHANGED served
                    /\ holders' = [holders EXCEPT ![d[self].s] = holders[d[self].s] \ {self - 10}]
                    /\ d' = [d EXCEPT ![self] = NoDesc]
                    /\ pc' = [pc EXCEPT ![self] = "c_loop"]
                    /\ UNCHANGED << cells, tail, head, gate, holds, 
                                    ctlHandover, spawned, beat, ctAlive, 
                                    procAlive, stalled, warm, active, released, 
                                    consumer, phase, reloadStarted, deathSeen, 
                                    reaped, pressure, failures, published, 
                                    needClosed, hl, g, i, px >>

c_exit(self) == /\ pc[self] = "c_exit"
                /\ TRUE
                /\ pc' = [pc EXCEPT ![self] = "Done"]
                /\ UNCHANGED << slot, cells, tail, head, gate, holds, 
                                ctlHandover, spawned, beat, ctAlive, procAlive, 
                                stalled, warm, active, released, consumer, 
                                phase, reloadStarted, deathSeen, reaped, 
                                pressure, failures, published, holders, served, 
                                needClosed, hl, d, g, i, px >>

C(self) == c_load(self) \/ c_standby(self) \/ c_loop(self) \/ c_stepA(self)
              \/ c_stepB(self) \/ c_claim(self) \/ c_complete(self)
              \/ c_exit(self)

s_loop == /\ pc[SUP] = "s_loop"
          /\ \/ /\ Scenario \in {"same", "changed"} /\ ~reloadStarted /\ phase = "steady"
                   /\ 2 \in Successor
                /\ reloadStarted' = TRUE
                /\ IF Scenario = "changed"
                      THEN /\ holds' = (holds \cup {"RELOAD"})
                           /\ gate' = GateOf(holds' \cup {"RELOAD"}, gate.gen)
                           /\ needClosed' = (needClosed \cup {"reload"})
                      ELSE /\ TRUE
                           /\ UNCHANGED << gate, holds, needClosed >>
                /\ ctlHandover' = TRUE
                /\ phase' = "handover"
                /\ pc' = [pc EXCEPT ![SUP] = "s_start2"]
                /\ UNCHANGED <<spawned, beat, ctAlive, procAlive, consumer, deathSeen, holders, g, i>>
             \/ /\ phase = "handover" /\ released[consumer]
                /\ ctlHandover' = FALSE
                /\ IF Scenario = "changed" \/ Successor = {}
                      THEN /\ IF Successor = {}
                                 THEN /\ holds' = (holds \cup {"RELOAD"})
                                      /\ gate' = GateOf(holds' \cup {"RELOAD"}, gate.gen)
                                      /\ needClosed' = (needClosed \cup {"reload"})
                                 ELSE /\ TRUE
                                      /\ UNCHANGED << gate, holds, needClosed >>
                           /\ i' = 1
                           /\ pc' = [pc EXCEPT ![SUP] = "s_rel_fail"]
                      ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_rel_activate"]
                           /\ UNCHANGED << gate, holds, needClosed, i >>
                /\ UNCHANGED <<spawned, beat, ctAlive, procAlive, consumer, phase, reloadStarted, deathSeen, holders, g>>
             \/ /\ phase = "await_active" /\ active[consumer]
                /\ IF Bug = "store_healthy"
                      THEN /\ gate' = [st |-> "HEALTHY", gen |-> gate.gen]
                           /\ holds' = holds \ {"DEAD", "RELOAD"}
                      ELSE /\ holds' = holds \ {"DEAD", "RELOAD"}
                           /\ gate' = GateOf(holds' \ {"DEAD", "RELOAD"}, gate.gen)
                /\ needClosed' = needClosed \ {"death", "reload"}
                /\ phase' = "steady"
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<ctlHandover, spawned, beat, ctAlive, procAlive, consumer, reloadStarted, deathSeen, holders, g, i>>
             \/ /\ ~ctAlive[consumer] /\ ~released[consumer] /\ ~deathSeen[consumer]
                   /\ phase \in {"steady", "handover", "await_active"}
                /\ g' = consumer
                /\ deathSeen' = [deathSeen EXCEPT ![g'] = TRUE]
                /\ holds' = (holds \cup {"DEAD"})
                /\ gate' = GateOf(holds' \cup {"DEAD"}, gate.gen)
                /\ needClosed' = (needClosed \cup {"death"})
                /\ ctlHandover' = FALSE
                /\ i' = 1
                /\ pc' = [pc EXCEPT ![SUP] = "s_d_fail"]
                /\ UNCHANGED <<spawned, beat, ctAlive, procAlive, consumer, phase, reloadStarted, holders>>
             \/ /\ \E h \in GENS : spawned[h] /\ ~procAlive[h] /\ ~reaped[h]
                /\ \E h \in {x \in GENS : spawned[x] /\ ~procAlive[x] /\ ~reaped[x]}:
                     g' = h
                /\ i' = 1
                /\ pc' = [pc EXCEPT ![SUP] = "s_p_owned"]
                /\ UNCHANGED <<gate, holds, ctlHandover, spawned, beat, ctAlive, procAlive, consumer, phase, reloadStarted, deathSeen, holders, needClosed>>
             \/ /\ phase = "await_warm" /\ Successor = {} /\ FreeBeat # {}
                   /\ (\E h \in GENS : ~spawned[h])
                   /\ ~(\E h \in GENS : spawned[h] /\ ctAlive[h] /\ ~warm[h])
                /\ \E h \in {x \in GENS : ~spawned[x] /\ \A y \in GENS : y < x => spawned[y]}:
                     /\ \E b \in FreeBeat:
                          beat' = [beat EXCEPT ![h] = b]
                     /\ spawned' = [spawned EXCEPT ![h] = TRUE]
                     /\ ctAlive' = [ctAlive EXCEPT ![h] = TRUE]
                     /\ procAlive' = [procAlive EXCEPT ![h] = TRUE]
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<gate, holds, ctlHandover, consumer, phase, reloadStarted, deathSeen, holders, needClosed, g, i>>
             \/ /\ phase = "await_warm" /\ Successor # {}
                /\ \E h \in Successor:
                     /\ consumer' = h
                     /\ gate' = GateOf(holds, h)
                /\ phase' = "await_active"
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<holds, ctlHandover, spawned, beat, ctAlive, procAlive, reloadStarted, deathSeen, holders, needClosed, g, i>>
             \/ /\ \E h \in GENS : spawned[h] /\ ctAlive[h] /\ stalled[h]
                /\ \E h \in {x \in GENS : spawned[x] /\ ctAlive[x] /\ stalled[x]}:
                     IF Bug = "promote_on_kill" /\ h = consumer /\ active[h]
                        THEN /\ \E n \in Successor:
                                  /\ consumer' = n
                                  /\ gate' = GateOf(holds, n)
                             /\ phase' = "await_active"
                             /\ UNCHANGED ctAlive
                        ELSE /\ ctAlive' = [ctAlive EXCEPT ![h] = FALSE]
                             /\ UNCHANGED << gate, consumer, phase >>
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<holds, ctlHandover, spawned, beat, procAlive, reloadStarted, deathSeen, holders, needClosed, g, i>>
             \/ /\ \E h \in GENS : procAlive[h] /\ (~ctAlive[h] \/ released[h])
                /\ \E h \in {x \in GENS : procAlive[x] /\ (~ctAlive[x] \/ released[x])}:
                     /\ ctAlive' = [ctAlive EXCEPT ![h] = FALSE]
                     /\ procAlive' = [procAlive EXCEPT ![h] = FALSE]
                     /\ holders' = [x \in Slots |-> holders[x] \ {h}]
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<gate, holds, ctlHandover, spawned, beat, consumer, phase, reloadStarted, deathSeen, needClosed, g, i>>
             \/ /\ Pressure /\ (pressure # ("PSI" \in holds))
                /\ IF pressure
                      THEN /\ holds' = (holds \cup {"PSI"})
                           /\ gate' = GateOf(holds' \cup {"PSI"}, gate.gen)
                           /\ needClosed' = (needClosed \cup {"psi"})
                      ELSE /\ holds' = holds \ {"PSI"}
                           /\ gate' = GateOf(holds' \ {"PSI"}, gate.gen)
                           /\ needClosed' = needClosed \ {"psi"}
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<ctlHandover, spawned, beat, ctAlive, procAlive, consumer, phase, reloadStarted, deathSeen, holders, g, i>>
          /\ UNCHANGED << slot, cells, tail, head, stalled, warm, active, 
                          released, reaped, pressure, failures, published, 
                          served, hl, d, px >>

s_start2 == /\ pc[SUP] = "s_start2"
            /\ IF Bug = "handover_no_release"
                  THEN /\ consumer' = 2
                       /\ gate' = GateOf(holds, 2)
                       /\ phase' = "await_active"
                  ELSE /\ TRUE
                       /\ UNCHANGED << gate, consumer, phase >>
            /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
            /\ UNCHANGED << slot, cells, tail, head, holds, ctlHandover, 
                            spawned, beat, ctAlive, procAlive, stalled, warm, 
                            active, released, reloadStarted, deathSeen, reaped, 
                            pressure, failures, published, holders, served, 
                            needClosed, hl, d, g, i, px >>

s_rel_fail == /\ pc[SUP] = "s_rel_fail"
              /\ IF i <= NSlots
                    THEN /\ IF slot[i].st = "READY"
                               THEN /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.o = 0]]
                               ELSE /\ TRUE
                                    /\ slot' = slot
                         /\ i' = i + 1
                         /\ pc' = [pc EXCEPT ![SUP] = "s_rel_fail"]
                    ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_rel_activate"]
                         /\ UNCHANGED << slot, i >>
              /\ UNCHANGED << cells, tail, head, gate, holds, ctlHandover, 
                              spawned, beat, ctAlive, procAlive, stalled, warm, 
                              active, released, consumer, phase, reloadStarted, 
                              deathSeen, reaped, pressure, failures, published, 
                              holders, served, needClosed, hl, d, g, px >>

s_rel_activate == /\ pc[SUP] = "s_rel_activate"
                  /\ IF Successor # {}
                        THEN /\ \E h \in Successor:
                                  /\ consumer' = h
                                  /\ gate' = GateOf(holds, h)
                             /\ phase' = "await_active"
                        ELSE /\ phase' = "await_warm"
                             /\ UNCHANGED << gate, consumer >>
                  /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                  /\ UNCHANGED << slot, cells, tail, head, holds, ctlHandover, 
                                  spawned, beat, ctAlive, procAlive, stalled, 
                                  warm, active, released, reloadStarted, 
                                  deathSeen, reaped, pressure, failures, 
                                  published, holders, served, needClosed, hl, 
                                  d, g, i, px >>

s_d_fail == /\ pc[SUP] = "s_d_fail"
            /\ IF i <= NSlots
                  THEN /\ IF slot[i].st = "READY"
                             THEN /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.o = 0]]
                             ELSE /\ IF Bug = "fail_owned_on_lock" /\ slot[i].st = "PROCESSING" /\ slot[i].o = beat[g]
                                        THEN /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.o = 0]]
                                        ELSE /\ TRUE
                                             /\ slot' = slot
                       /\ i' = i + 1
                       /\ pc' = [pc EXCEPT ![SUP] = "s_d_fail"]
                  ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_d_repair"]
                       /\ UNCHANGED << slot, i >>
            /\ UNCHANGED << cells, tail, head, gate, holds, ctlHandover, 
                            spawned, beat, ctAlive, procAlive, stalled, warm, 
                            active, released, consumer, phase, reloadStarted, 
                            deathSeen, reaped, pressure, failures, published, 
                            holders, served, needClosed, hl, d, g, px >>

s_d_repair == /\ pc[SUP] = "s_d_repair"
              /\ Assert(~ctAlive[g], 
                        "Failure of assertion at line 230, column 13.")
              /\ IF cells[head % CAP].seq >= head + CAP
                    THEN /\ head' = head + 1
                    ELSE /\ TRUE
                         /\ head' = head
              /\ pc' = [pc EXCEPT ![SUP] = "s_d_next"]
              /\ UNCHANGED << slot, cells, tail, gate, holds, ctlHandover, 
                              spawned, beat, ctAlive, procAlive, stalled, warm, 
                              active, released, consumer, phase, reloadStarted, 
                              deathSeen, reaped, pressure, failures, published, 
                              holders, served, needClosed, hl, d, g, i, px >>

s_d_next == /\ pc[SUP] = "s_d_next"
            /\ IF Successor # {}
                  THEN /\ \E h \in Successor:
                            /\ consumer' = h
                            /\ gate' = GateOf(holds, h)
                       /\ phase' = "await_active"
                  ELSE /\ phase' = "await_warm"
                       /\ UNCHANGED << gate, consumer >>
            /\ g' = 0
            /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
            /\ UNCHANGED << slot, cells, tail, head, holds, ctlHandover, 
                            spawned, beat, ctAlive, procAlive, stalled, warm, 
                            active, released, reloadStarted, deathSeen, reaped, 
                            pressure, failures, published, holders, served, 
                            needClosed, hl, d, i, px >>

s_p_owned == /\ pc[SUP] = "s_p_owned"
             /\ IF i <= NSlots
                   THEN /\ IF slot[i].o = beat[g] /\ slot[i].st = "PROCESSING"
                              THEN /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.o = 0]]
                              ELSE /\ IF slot[i].o = beat[g] /\ slot[i].st = "ABANDONED"
                                         THEN /\ slot' = [slot EXCEPT ![i] = Free(slot[i].e)]
                                         ELSE /\ TRUE
                                              /\ slot' = slot
                        /\ i' = i + 1
                        /\ pc' = [pc EXCEPT ![SUP] = "s_p_owned"]
                   ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_p_done"]
                        /\ UNCHANGED << slot, i >>
             /\ UNCHANGED << cells, tail, head, gate, holds, ctlHandover, 
                             spawned, beat, ctAlive, procAlive, stalled, warm, 
                             active, released, consumer, phase, reloadStarted, 
                             deathSeen, reaped, pressure, failures, published, 
                             holders, served, needClosed, hl, d, g, px >>

s_p_done == /\ pc[SUP] = "s_p_done"
            /\ reaped' = [reaped EXCEPT ![g] = TRUE]
            /\ g' = 0
            /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
            /\ UNCHANGED << slot, cells, tail, head, gate, holds, ctlHandover, 
                            spawned, beat, ctAlive, procAlive, stalled, warm, 
                            active, released, consumer, phase, reloadStarted, 
                            deathSeen, pressure, failures, published, holders, 
                            served, needClosed, hl, d, i, px >>

S == s_loop \/ s_start2 \/ s_rel_fail \/ s_rel_activate \/ s_d_fail
        \/ s_d_repair \/ s_d_next \/ s_p_owned \/ s_p_done

p_loop == /\ pc[ENV] = "p_loop"
          /\ \/ /\ published < MaxRequests /\ gate.st = "HEALTHY" /\ \E y \in Slots : slot[y].st = "FREE"
                /\ \E y \in {z \in Slots : slot[z].st = "FREE"}:
                     /\ px' = y
                     /\ slot' = [slot EXCEPT ![y] = [e |-> slot[y].e + 1, st |-> "CLAIMED", o |-> 0]]
                     /\ holders' = [holders EXCEPT ![y] = holders[y] \cup {CLIENT}]
                /\ published' = published + 1
                /\ pc' = [pc EXCEPT ![ENV] = "p_ready"]
                /\ UNCHANGED <<ctAlive, stalled, pressure, failures>>
             \/ /\ \E y \in Slots : slot[y].st \in {"DONE", "FAILED"}
                /\ \E y \in {z \in Slots : slot[z].st \in {"DONE", "FAILED"}}:
                     slot' = [slot EXCEPT ![y] = Free(slot[y].e)]
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<ctAlive, stalled, pressure, failures, published, holders, px>>
             \/ /\ ClientsCancel /\ \E y \in Slots : slot[y].st \in {"READY", "PROCESSING"}
                /\ \E y \in {z \in Slots : slot[z].st \in {"READY", "PROCESSING"}}:
                     IF slot[y].st = "READY"
                        THEN /\ slot' = [slot EXCEPT ![y] = Free(slot[y].e)]
                        ELSE /\ slot' = [slot EXCEPT ![y] = [slot[y] EXCEPT !.st = "ABANDONED"]]
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<ctAlive, stalled, pressure, failures, published, holders, px>>
             \/ /\ failures < MaxFailures /\ \E h \in GENS : ctAlive[h]
                /\ \E h \in {z \in GENS : ctAlive[z]}:
                     ctAlive' = [ctAlive EXCEPT ![h] = FALSE]
                /\ failures' = failures + 1
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<slot, stalled, pressure, published, holders, px>>
             \/ /\ failures < MaxFailures /\ \E h \in GENS : ctAlive[h] /\ ~stalled[h]
                /\ \E h \in {z \in GENS : ctAlive[z] /\ ~stalled[z]}:
                     stalled' = [stalled EXCEPT ![h] = TRUE]
                /\ failures' = failures + 1
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<slot, ctAlive, pressure, published, holders, px>>
             \/ /\ \E h \in GENS : stalled[h]
                /\ \E h \in {z \in GENS : stalled[z]}:
                     stalled' = [stalled EXCEPT ![h] = FALSE]
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<slot, ctAlive, pressure, failures, published, holders, px>>
             \/ /\ Pressure
                /\ pressure' = ~pressure
                /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
                /\ UNCHANGED <<slot, ctAlive, stalled, failures, published, holders, px>>
          /\ UNCHANGED << cells, tail, head, gate, holds, ctlHandover, spawned, 
                          beat, procAlive, warm, active, released, consumer, 
                          phase, reloadStarted, deathSeen, reaped, served, 
                          needClosed, hl, d, g, i >>

p_ready == /\ pc[ENV] = "p_ready"
           /\ holders' = [holders EXCEPT ![px] = holders[px] \ {CLIENT}]
           /\ IF cells[tail % CAP].seq = tail
                 THEN /\ slot' = [slot EXCEPT ![px] = [slot[px] EXCEPT !.st = "READY"]]
                      /\ cells' = [cells EXCEPT ![tail % CAP] = [seq |-> tail + 1, s |-> px, e |-> slot'[px].e]]
                      /\ tail' = tail + 1
                 ELSE /\ slot' = [slot EXCEPT ![px] = Free(slot[px].e)]
                      /\ UNCHANGED << cells, tail >>
           /\ px' = 0
           /\ pc' = [pc EXCEPT ![ENV] = "p_loop"]
           /\ UNCHANGED << head, gate, holds, ctlHandover, spawned, beat, 
                           ctAlive, procAlive, stalled, warm, active, released, 
                           consumer, phase, reloadStarted, deathSeen, reaped, 
                           pressure, failures, published, served, needClosed, 
                           hl, d, g, i >>

P == p_loop \/ p_ready

Next == S \/ P
           \/ (\E self \in CTs: C(self))

Spec == /\ Init /\ [][Next]_vars
        /\ \A self \in CTs : WF_vars(C(self))
        /\ WF_vars(S)

\* END TRANSLATION

-------------------------------------------------------------------------------
(* Properties of testing §2.1, Spec B. *)

\* A consume thread that can touch the ring: running or merely stalled, past activation,
\* and not yet stopped.
InRing(h) == ctAlive[h] /\ pc[CT(h)] \in {"c_loop", "c_stepA", "c_stepB", "c_claim", "c_complete"}

\* At most one worker consumes from the ring, across crash, promotion, handover, and reload.
OneConsumer == Cardinality({h \in GENS : InRing(h)}) <= 1

\* ... and no slot can be written by two parties. A worker keeps its claim on a slot it was
\* processing until its process has exited.
OneWriter == \A y \in Slots : Cardinality(holders[y]) <= 1

\* No two workers that have not been reaped share a beat slot, so OWNER names one worker.
BeatsDistinct == \A h1, h2 \in GENS :
    (h1 # h2 /\ spawned[h1] /\ spawned[h2] /\ ~reaped[h1] /\ ~reaped[h2]) => beat[h1] # beat[h2]

\* A request is served at most once.
ServedOnce == \A key \in DOMAIN served : served[key] <= 1

\* The gate reads HEALTHY only when no path holds it closed, whatever order they finish in.
HealthyOnlyWhenOpen == gate.st = "HEALTHY" => needClosed = {}

\* The gate word is what the holds say.
GateFollowsHolds == gate = GateOf(holds, gate.gen)

\* ── liveness ──
\* A clean handover loses no queued request: every request that is queued is answered.
QueuedServed == \A y \in Slots : (slot[y].st = "READY") ~> (slot[y].st = "DONE")
\* The handover ends, completed or on the crash path, and the gate does not stay closed for ever.
GateReopens == <>[](gate.st = "HEALTHY")
HandoverEnds == (phase = "handover") ~> (phase # "handover")
===============================================================================
