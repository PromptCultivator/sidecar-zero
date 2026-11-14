------------------------------- MODULE SlotsRing -------------------------------
(******************************************************************************)
(* Spec A of testing §2.1: slots, ring, and sweep.                            *)
(*                                                                            *)
(* Two client threads of one host process, one ring, a few slots, one ring    *)
(* consumer at a time (a successor takes over after a crash), the SDK's       *)
(* background thread, and the supervisor with its gate, sweep, ring repair,   *)
(* and session reset. It assumes a ring has at most one consumer; Spec B      *)
(* (Consumers.tla) proves that assumption.                                    *)
(*                                                                            *)
(* Failures, injected by the environment process at any step:                 *)
(*   - the active worker crashes (all its threads stop at once);              *)
(*   - the host process dies, taking both threads with it; a reset follows;   *)
(*   - one client thread stops for ever (a killed thread);                    *)
(*   - the supervisor asks for a session reset while the host is alive.       *)
(*                                                                            *)
(* Granularity: every load, store, and compare-and-swap on shared memory is   *)
(* one step. A CAS loop (shm-abi §6.1) is one atomic read-modify-write: a CAS *)
(* succeeds only against the value it read, so the two are equivalent.       *)
(* Memory is sequentially consistent; loom (§2.2) covers reordering.          *)
(*                                                                            *)
(* `Bug` selects one seeded mistake (see spec/README.md); "none" is the       *)
(* design as written. Each seeded mistake must make TLC report a violation.   *)
(******************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Threads,            \* client threads of the one host process (model values)
    NCalls,             \* calls each thread makes
    NSlots,             \* slots in the one slab class
    CAP,                \* ring capacity
    SpinIters,          \* gate checks a waiting client makes before it parks
    MaxWorkerCrashes,   \* crashes of the active worker; each hands the ring to a successor
    HostMayDie,         \* whether the host process may die
    MaxKills,           \* client threads that may stop for ever
    MaxRequestedResets, \* resets the supervisor may ask for with the host alive
    Unwind,             \* whether a producer may unwind between claiming and publishing a cell
    Bug                 \* "none", or one seeded mistake

BGT == 10                                   \* the SDK's background thread
GENS == 1..(MaxWorkerCrashes + 1)           \* worker generations
Workers == {20 + g : g \in GENS}
SUP == 30
ENV == 40
W(g) == 20 + g
GenOf(w) == w - 20
OwnerOf(g) == ((g - 1) % 2) + 1             \* the beat slot + 1 that generation g writes to OWNER
Slots == 1..NSlots
MaxEpoch == Cardinality(Threads) * NCalls + 1
Perms == Permutations(Threads)                \* symmetry, for safety-only configurations

Free(e) == [e |-> e, st |-> "FREE", w |-> FALSE, o |-> 0]
Low(x) == <<x.st, x.w, x.o>>                \* the low 32 bits of the slot word: what a futex compares
Fresh == [i \in 0..(CAP - 1) |-> [seq |-> i, s |-> 0, e |-> 0, void |-> FALSE]]

(* --algorithm SlotsRing {
variables
    \* ── shared memory ──
    slot = [x \in Slots |-> Free(0)],                 \* the slot word: epoch, state, WAITER, OWNER
    cells = Fresh,                                    \* descriptor cells: seq, slot, epoch, VOID
    tail = 0,
    head = 0,                                         \* the header's mirror of the consumer's head
    cwait = "AWAKE",                                  \* consumer_wait
    gate = [st |-> "HEALTHY", gen |-> 1],
    ctlDrain = FALSE,                                 \* ctl_req's DRAIN bit
    drainAck = [h \in GENS |-> FALSE],                \* each worker's ctl_ack for DRAIN
    \* ── futex wait queues ──
    csleep = FALSE,                                   \* the consumer sleeps on consumer_wait
    sleepers = [x \in Slots |-> {}],                  \* client threads sleeping on each slot word
    \* ── processes and the session ──
    holds = {},                                       \* reasons the gate is held closed (supervisor §5.7)
    activeGen = 1,
    wAlive = [h \in GENS |-> TRUE],
    wActive = [h \in GENS |-> h = 1],                 \* reported ACTIVE
    hostAlive = TRUE,
    tAlive = [u \in Threads |-> TRUE],
    sock = "open",                                    \* open | half (supervisor shut its side) | closed | none
    snapshot = 1,                                     \* the SDK's current arena; 0 = none mapped
    nextSnap = 2,
    marker = [u \in Threads |-> 0],                   \* in-call markers (sdk-contract §6)
    supPhase = "idle",                                \* idle | await_close | draining
    resetWanted = FALSE,
    \* ── counters for the bounds ──
    crashes = 0, kills = 0, requested = 0,
    \* ── ghosts: never read by the protocol ──
    holders = [x \in Slots |-> {}],                   \* who may touch the slot's data regions now
    reqEpoch = [x \in Slots |-> 0],                   \* the call whose request is in the slot
    respEpoch = [x \in Slots |-> 0],                  \* the call the response in the slot answers
    fates = [key \in Slots \X (0..MaxEpoch) |-> {}],    \* how each (slot, epoch) ended
    cellBy = [j \in 0..(CAP - 1) |-> 0],              \* the thread that claimed each cell this lap
    handled = [h \in GENS |-> FALSE],                 \* the death of g is fully handled
    readySwept = [h \in GENS |-> FALSE],              \* the READY part of g's sweep is done
    repaired = [h \in GENS |-> FALSE],                \* the ring was repaired after g's death
    sweptClosed = {};                                 \* closures whose READY sweep is done: "DEAD", "RESET"

define {
    Live(t) == hostAlive /\ tAlive[t]
    \* A reset can happen: the host may die, the supervisor may ask, or a killed producer may
    \* leave a head cell that only a reset can clear.
    Resets == HostMayDie \/ MaxRequestedResets > 0 \/ MaxKills > 0
    WLive(w) == wAlive[GenOf(w)] /\ activeGen = GenOf(w)
    GateOf(hs, g) ==
        IF hs = {} THEN [st |-> "HEALTHY", gen |-> g]
        ELSE IF "DEAD" \in hs THEN [st |-> "DEAD", gen |-> g]
        ELSE [st |-> "RELOADING", gen |-> g]
    \* A head cell claimed by a thread that has stopped for ever (shm-abi §4.5)
    HeadStuck == /\ tail > head
                 /\ cells[head % CAP].seq = head
                 /\ cellBy[head % CAP] \in Threads
                 /\ ~tAlive[cellBy[head % CAP]]
}

\* ── client threads: sdk-contract §4 ─────────────────────────────────────────────────
\* Steps are merged where nothing can observe the state between them, so that larger
\* configurations stay checkable: admission is merged into the slot claim, the request write
\* into the READY store, the gate re-check load into its cancel, the WAITER announce into the
\* load before it. Two splits exist only in configurations that can use them: the marker
\* re-check needs resets, and the gap between WAKING and the futex wake needs killed threads.
fair process (C \in Threads)
variables s = 0, e = 0, pos = 0, m = 0, k = 0, spins = 0,
          wv = Free(0), cw = "AWAKE", result = "", voided = FALSE;
{
c_top:
    while (k < NCalls) {
        await Live(self);
        \* Enter: claim a caller entry naming the snapshot, then re-check it (sdk-rust §3.1).
        if (Resets) {
            if (snapshot = 0) { result := "NOT_CONNECTED"; goto c_exit; }
            else { marker[self] := snapshot; m := snapshot; };
c_enter2:
            await Live(self);
            if (snapshot # m) { result := "NOT_CONNECTED"; goto c_exit; };
        };
        \* Admission, then claim a slot: CAS (e, FREE) -> (e + 1, CLAIMED).
c_claim:
        await Live(self);
        if (gate.st # "HEALTHY") { result := gate.st; goto c_exit; }
        else if (\E x \in Slots : slot[x].st = "FREE") {
            with (x \in {y \in Slots : slot[y].st = "FREE"}) {
                s := x;
                e := slot[x].e + 1;
                slot[x] := [e |-> slot[x].e + 1, st |-> "CLAIMED", w |-> FALSE, o |-> 0];
                holders[x] := holders[x] \cup {self};
            };
        } else { result := "NO_SLOT"; goto c_exit; };
        \* Encode, hand the slot over with a plain store of READY, and load tail.
c_ready:
        await Live(self);
        reqEpoch[s] := e;
        slot[s] := [e |-> e, st |-> "READY", w |-> FALSE, o |-> 0];
        holders[s] := holders[s] \ {self};
        pos := tail;
        \* Enqueue (shm-abi §4.3).
c_enq:
        await Live(self);
        if (cells[pos % CAP].seq = pos) {
            if (tail = pos) { tail := pos + 1; cellBy[pos % CAP] := IF MaxKills > 0 THEN self ELSE 0; }
            else { pos := tail; goto c_enq; };
        } else if (cells[pos % CAP].seq < pos) {
            goto c_full;
        } else { pos := tail; goto c_enq; };
c_fill:
        await Live(self);
        either {
            cells[pos % CAP] := [cells[pos % CAP] EXCEPT !.s = s, !.e = e, !.void = FALSE];
        } or {
            \* The call unwinds here; its scope guard publishes the cell as void (shm-abi §4.3),
            \* and, like any publish, applies the waker rule.
            await Unwind;
            cells[pos % CAP] := [seq |-> pos + 1, s |-> s, e |-> e, void |-> TRUE];
            voided := TRUE;
            if (Bug = "void_no_wake") { goto c_unwound; } else { goto c_wake1; };
        };
c_publish:
        await Live(self);
        cells[pos % CAP].seq := pos + 1;
        \* The waker rule (signaling §3.1).
c_wake1:
        await Live(self);
        cw := cwait;
        if (cw = "AWAKE") { if (voided) { goto c_unwound; } else { goto c_recheck; }; };
c_wake2:
        await Live(self);
        if (cw = "PARKED" /\ cwait = "PARKED") { cwait := "WAKING"; };
        if (MaxKills = 0) { csleep := FALSE; if (voided) { goto c_unwound; } else { goto c_recheck; }; };
c_wake3:
        await Live(self);
        csleep := FALSE;
        if (voided) { goto c_unwound; };
        \* The gate re-check after publishing (shm-abi §6.3): load the gate, and cancel the
        \* slot if it is still READY.
c_recheck:
        await Live(self);
        if (Bug # "no_recheck" /\ gate.st # "HEALTHY" /\ slot[s].st = "READY" /\ slot[s].e = e) {
            slot[s] := Free(e);
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"cancelled"};
            result := gate.st;
            goto c_exit;
        };
        \* Spin: watch the slot; cancel a still-queued request if the gate closes.
c_spin:
        await Live(self);
        assert slot[s].e = e;
        if (slot[s].st = "DONE") { goto c_read; }
        else if (slot[s].st = "FAILED") { goto c_failed; }
        else if (spins >= SpinIters) { goto c_park; }
        else if (slot[s].st = "READY") { either { goto c_spin_gate; } or { goto c_abandon; }; }
        else { spins := spins + 1; either { goto c_spin; } or { goto c_abandon; }; };
c_spin_gate:
        await Live(self);
        spins := spins + 1;
        if (gate.st # "HEALTHY" /\ slot[s].st = "READY" /\ slot[s].e = e) {
            slot[s] := Free(e);
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"cancelled"};
            result := gate.st;
            goto c_exit;
        } else { goto c_spin; };
        \* Park: announce WAITER (a CAS over the word just loaded), then futex-wait on the low
        \* half (signaling §3.2). The deadline may pass at any moment.
c_park:
        await Live(self);
        if (slot[s].st = "DONE") { goto c_read; }
        else if (slot[s].st = "FAILED") { goto c_failed; }
        else {
            either { goto c_abandon; }
            or {
                wv := [slot[s] EXCEPT !.w = TRUE];
                slot[s].w := TRUE;
            };
        };
c_fwait:
        await Live(self);
        if (Low(slot[s]) = Low(wv)) { sleepers[s] := sleepers[s] \cup {self}; }
        else { goto c_park; };
c_asleep:
        await Live(self);
        either { await self \notin sleepers[s]; goto c_park; }
        or { sleepers[s] := sleepers[s] \ {self}; goto c_abandon; };     \* the deadline
        \* Abandon (sdk-contract §4): one CAS loop over the word.
c_abandon:
        await Live(self);
        wv := slot[s];
        assert wv.e = e;
        if (wv.st = "READY") {
            slot[s] := Free(e);
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"cancelled"};
            result := "DEADLINE";
            goto c_exit;
        } else if (wv.st = "PROCESSING") {
            slot[s] := [wv EXCEPT !.st = "ABANDONED", !.w = FALSE,
                                  !.o = IF Bug = "abandon_drops_owner" THEN 0 ELSE wv.o];
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"abandoned"};
            result := "DEADLINE";
            goto c_exit;
        } else if (wv.st = "DONE") { goto c_read; }
        else if (wv.st = "FAILED") { goto c_failed; }
        else { assert FALSE; };
        \* Resolve.
c_read:
        await Live(self);
        assert respEpoch[s] = e;                       \* the answer is to this call's request
        assert "done" \in fates[<<s, e>>];
        holders[s] := holders[s] \cup {self};
c_release:
        await Live(self);
        slot[s] := Free(e);
        holders[s] := holders[s] \ {self};
        result := "MODEL";
        goto c_exit;
c_failed:
        await Live(self);
        slot[s] := Free(e);
        result := "WORKER_FAILED";
        goto c_exit;
c_full:
        await Live(self);
        if (slot[s].st = "READY" /\ slot[s].e = e) {
            slot[s] := Free(e);
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"cancelled"};
        } else if (slot[s].st = "FAILED") { slot[s] := Free(e); };
        result := "RING_FULL";
        goto c_exit;
c_unwound:
        await Live(self);
        if (slot[s].st = "READY" /\ slot[s].e = e) {
            slot[s] := Free(e);
            fates[<<s, e>>] := fates[<<s, e>>] \cup {"cancelled"};
        } else if (slot[s].st = "FAILED") { slot[s] := Free(e); };
        result := "CODEC_ERROR";
c_exit:
        await Live(self);
        marker[self] := 0;
        k := k + 1;
        s := 0; e := 0; pos := 0; m := 0; spins := 0;
        wv := Free(0); cw := "AWAKE"; result := ""; voided := FALSE;
    };
}

\* ── the SDK's background thread: hold, retire, reconnect (sdk-contract §6) ──────────
fair process (B = BGT)
variables old = 0;
{
b_hold:
    while (TRUE) {
        await hostAlive /\ sock = "half";              \* end of stream
        old := snapshot;
        snapshot := 0;                                 \* new calls fall back from here
b_retire:
        await hostAlive /\ (Bug = "ignore_markers" \/ \A t \in Threads : marker[t] # old \/ ~tAlive[t]);
        sock := "closed";                              \* unmapped first, then the socket closed
        old := 0;
b_connect:
        await hostAlive /\ sock = "none";              \* BUSY until the reset is done
        snapshot := nextSnap;
        nextSnap := nextSnap + 1;
        sock := "open";
    };
}

\* ── the ring consumer, one process per worker generation (worker §4) ─────────────────
fair process (Wk \in Workers)
variables hl = 0, d = [s |-> 0, e |-> 0, void |-> FALSE], wb = FALSE;
{
w_start:
    await activeGen = GenOf(self) /\ wAlive[GenOf(self)];
    hl := head;                                        \* reads head from the header
    wActive[GenOf(self)] := TRUE;                      \* reports ACTIVE
w_loop:
    while (TRUE) {
        await WLive(self);
        if (ctlDrain /\ ~drainAck[GenOf(self)]) {      \* a pause, at the loop boundary
            drainAck[GenOf(self)] := TRUE;
            d := [s |-> 0, e |-> 0, void |-> FALSE];
            goto w_paused;
        } else if (cells[hl % CAP].seq = hl + 1) {
            d := [s |-> cells[hl % CAP].s, e |-> cells[hl % CAP].e, void |-> cells[hl % CAP].void];
        } else {
            d := [s |-> 0, e |-> 0, void |-> FALSE];
            goto w_park;
        };
w_stepA:
        await WLive(self);
        cells[hl % CAP].seq := hl + CAP;
        if (MaxWorkerCrashes = 0) {                    \* no crash can fall between A and B
            head := hl + 1;
            hl := hl + 1;
            if (d.void) { goto w_loop; } else { goto w_claim; };
        };
w_stepB:
        await WLive(self);
        head := hl + 1;
        hl := hl + 1;
        if (d.void) { goto w_loop; };
        \* Claim: CAS (e, READY) -> (e, PROCESSING), keeping WAITER and setting OWNER.
w_claim:
        await WLive(self);
        if (slot[d.s].st = "READY" /\ slot[d.s].e = d.e) {
            assert reqEpoch[d.s] = d.e;                \* the request is the descriptor's
            slot[d.s] := [slot[d.s] EXCEPT !.st = "PROCESSING", !.o = OwnerOf(GenOf(self))];
            holders[d.s] := holders[d.s] \cup {self};
        } else { goto w_loop; };
        \* Infer, write the response, and complete: PROCESSING -> DONE, or ABANDONED -> FREE.
w_complete:
        await WLive(self);
        respEpoch[d.s] := d.e;
        wb := slot[d.s].w;
        holders[d.s] := holders[d.s] \ {self};
        if (slot[d.s].e = d.e /\ slot[d.s].st = "ABANDONED") {
            slot[d.s] := Free(d.e);
            goto w_loop;
        } else if (slot[d.s].e = d.e /\ slot[d.s].st = "PROCESSING") {
            slot[d.s] := [e |-> d.e, st |-> "DONE", w |-> FALSE, o |-> 0];
            fates[<<d.s, d.e>>] := fates[<<d.s, d.e>>] \cup {"done"};
            if (~wb \/ Bug = "no_complete_wake") { goto w_loop; };
        } else { goto w_loop; };
w_wake:
        await WLive(self);
        sleepers[d.s] := {};
        wb := FALSE;
        goto w_loop;
        \* Park (signaling §3.1): announce, re-check, sleep.
w_park:
        await WLive(self);
        cwait := "PARKED";
w_recheck:
        await WLive(self);
        if (cells[hl % CAP].seq = hl + 1 \/ (ctlDrain /\ ~drainAck[GenOf(self)])) {
            cwait := "AWAKE";
            goto w_loop;
        };
w_fwait:
        await WLive(self);
        if (cwait = "PARKED") { csleep := TRUE; } else { cwait := "AWAKE"; goto w_loop; };
w_asleep:
        await WLive(self) /\ ~csleep;
        cwait := "AWAKE";
        goto w_loop;
        \* Paused by DRAIN: wait on standby_wait until the bit is cleared, then re-read head.
w_paused:
        await WLive(self) /\ ~ctlDrain;
        drainAck[GenOf(self)] := FALSE;
        hl := head;
    };
}

\* ── the supervisor: one loop, one event at a time (supervisor §2) ────────────────────
fair process (S = SUP)
variables g = 0, i = 1;
{
s_loop:
    while (TRUE) {
        either {
            \* The active worker has died (supervisor §5.3).
            await ~wAlive[activeGen] /\ ~handled[activeGen];
            g := activeGen;
            if (Bug # "sweep_before_gate") {
                holds := holds \cup {"DEAD"};
                gate := GateOf(holds \cup {"DEAD"}, gate.gen);
            };
            i := 1;
s_ready:    \* fail every READY slot, waking its waiter; and wake every DONE slot, whose
            \* worker may have died between completing it and waking its waiter
            while (i <= NSlots) {
                if (slot[i].st = "READY") {
                    fates[<<i, slot[i].e>>] := fates[<<i, slot[i].e>>] \cup {"failed"};
                    slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0];
                    sleepers[i] := {};
                } else if (slot[i].st = "DONE" /\ Bug # "no_done_wake") {
                    sleepers[i] := {};
                };
                i := i + 1;
            };
s_gate_late:
            readySwept[g] := TRUE;
            sweptClosed := sweptClosed \cup {"DEAD"};
            if (Bug = "sweep_before_gate") {
                holds := holds \cup {"DEAD"};
                gate := GateOf(holds \cup {"DEAD"}, gate.gen);
            };
s_repair:   \* the repair rule (shm-abi §4.5)
            if (IF Bug = "repair_equality" THEN cells[head % CAP].seq = head + CAP
                ELSE cells[head % CAP].seq >= head + CAP) {
                head := head + 1;
            };
            repaired[g] := TRUE;
            i := 1;
s_owned:    \* the slots it held, by OWNER
            while (i <= NSlots) {
                if (slot[i].o = OwnerOf(g) /\ slot[i].st = "PROCESSING") {
                    fates[<<i, slot[i].e>>] := fates[<<i, slot[i].e>>] \cup {"failed"};
                    slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0];
                    sleepers[i] := {};
                } else if (slot[i].o = OwnerOf(g) /\ slot[i].st = "ABANDONED") {
                    slot[i] := Free(slot[i].e);
                };
                i := i + 1;
            };
s_successor:
            handled[g] := TRUE;
            if (g + 1 \in GENS) {
                activeGen := g + 1;
                gate := GateOf(holds, g + 1);
            };
            g := 0;
            i := 1;
        } or {
            \* The successor reported ACTIVE: drop the death's hold.
            await "DEAD" \in holds /\ wActive[activeGen] /\ wAlive[activeGen];
            sweptClosed := sweptClosed \ {"DEAD"};
            holds := holds \ {"DEAD"};
            gate := GateOf(holds \ {"DEAD"}, gate.gen);
        } or {
            \* The client's end of the session closed: reset (supervisor §4).
            await sock = "closed" /\ supPhase \in {"idle", "await_close"};
            holds := holds \cup {"RESET"};
            gate := GateOf(holds \cup {"RESET"}, gate.gen);
            ctlDrain := TRUE;
            supPhase := "draining";
s_ctl_wake: \* the waker rule after the store to ctl_req
            if (cwait = "PARKED") { cwait := "WAKING"; };
            csleep := FALSE;
        } or {
            \* The worker has paused, or is gone: re-initialize the arena.
            await supPhase = "draining" /\ (drainAck[activeGen] \/ ~wAlive[activeGen]);
s_reinit:
            cells := Fresh;
            head := 0;
            tail := 0;
            cwait := "AWAKE";
            slot := [x \in Slots |-> Free(slot[x].e)];
            sleepers := [x \in Slots |-> {}];
s_reset_done:
            sweptClosed := sweptClosed \ {"RESET"};
            ctlDrain := FALSE;
            holds := holds \ {"RESET"};
            gate := GateOf(holds \ {"RESET"}, gate.gen);
            supPhase := "idle";
            sock := "none";
        } or {
            \* A reset requested with the host attached (supervisor §4): close the gate, fail
            \* what is queued, and shut the sending side so the client leaves.
            await supPhase = "idle" /\ sock = "open"
                  /\ (resetWanted \/ (HeadStuck /\ Bug # "no_stuck_reset"));
            holds := holds \cup {"RESET"};
            gate := GateOf(holds \cup {"RESET"}, gate.gen);
            i := 1;
s_rq_fail:
            while (i <= NSlots) {
                if (slot[i].st = "READY") {
                    fates[<<i, slot[i].e>>] := fates[<<i, slot[i].e>>] \cup {"failed"};
                    slot[i] := [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0];
                    sleepers[i] := {};
                };
                i := i + 1;
            };
s_rq_shut:
            i := 1;
            sweptClosed := sweptClosed \cup {"RESET"};
            sock := "half";
            supPhase := "await_close";
            resetWanted := FALSE;
        } or {
            \* Backstop: the worker's heartbeat thread applies the waker rule to a ring with
            \* work (signaling §3.1). Modelled here; the effect is the same.
            await wAlive[activeGen] /\ csleep /\ cwait # "AWAKE"
                  /\ (cells[head % CAP].seq = head + 1 \/ (ctlDrain /\ ~drainAck[activeGen]));
            if (cwait = "PARKED") { cwait := "WAKING"; };
            csleep := FALSE;
        };
    };
}

\* ── failures: not fair, so none is ever forced ────────────────────────────────────────
process (E = ENV)
{
e_loop:
    while (TRUE) {
        either {
            await crashes < MaxWorkerCrashes /\ wAlive[activeGen] /\ wActive[activeGen];
            wAlive[activeGen] := FALSE;
            crashes := crashes + 1;
            csleep := FALSE;
            holders := [x \in Slots |-> holders[x] \ {W(activeGen)}];
        } or {
            await HostMayDie /\ hostAlive;
            hostAlive := FALSE;
            if (sock \in {"open", "half"}) { sock := "closed"; };
            holders := [x \in Slots |-> holders[x] \ Threads];
            sleepers := [x \in Slots |-> {}];
        } or {
            await kills < MaxKills /\ hostAlive;
            with (t \in {u \in Threads : tAlive[u]}) {
                tAlive[t] := FALSE;
                holders := [x \in Slots |-> holders[x] \ {t}];
                sleepers := [x \in Slots |-> sleepers[x] \ {t}];
            };
            kills := kills + 1;
        } or {
            await requested < MaxRequestedResets /\ hostAlive /\ sock = "open" /\ ~resetWanted;
            resetWanted := TRUE;
            requested := requested + 1;
        };
    };
}
} *)
\* BEGIN TRANSLATION
VARIABLES slot, cells, tail, head, cwait, gate, ctlDrain, drainAck, csleep, 
          sleepers, holds, activeGen, wAlive, wActive, hostAlive, tAlive, 
          sock, snapshot, nextSnap, marker, supPhase, resetWanted, crashes, 
          kills, requested, holders, reqEpoch, respEpoch, fates, cellBy, 
          handled, readySwept, repaired, sweptClosed, pc

(* define statement *)
Live(t) == hostAlive /\ tAlive[t]


Resets == HostMayDie \/ MaxRequestedResets > 0 \/ MaxKills > 0
WLive(w) == wAlive[GenOf(w)] /\ activeGen = GenOf(w)
GateOf(hs, g) ==
    IF hs = {} THEN [st |-> "HEALTHY", gen |-> g]
    ELSE IF "DEAD" \in hs THEN [st |-> "DEAD", gen |-> g]
    ELSE [st |-> "RELOADING", gen |-> g]

HeadStuck == /\ tail > head
             /\ cells[head % CAP].seq = head
             /\ cellBy[head % CAP] \in Threads
             /\ ~tAlive[cellBy[head % CAP]]

VARIABLES s, e, pos, m, k, spins, wv, cw, result, voided, old, hl, d, wb, g, 
          i

vars == << slot, cells, tail, head, cwait, gate, ctlDrain, drainAck, csleep, 
           sleepers, holds, activeGen, wAlive, wActive, hostAlive, tAlive, 
           sock, snapshot, nextSnap, marker, supPhase, resetWanted, crashes, 
           kills, requested, holders, reqEpoch, respEpoch, fates, cellBy, 
           handled, readySwept, repaired, sweptClosed, pc, s, e, pos, m, k, 
           spins, wv, cw, result, voided, old, hl, d, wb, g, i >>

ProcSet == (Threads) \cup {BGT} \cup (Workers) \cup {SUP} \cup {ENV}

Init == (* Global variables *)
        /\ slot = [x \in Slots |-> Free(0)]
        /\ cells = Fresh
        /\ tail = 0
        /\ head = 0
        /\ cwait = "AWAKE"
        /\ gate = [st |-> "HEALTHY", gen |-> 1]
        /\ ctlDrain = FALSE
        /\ drainAck = [h \in GENS |-> FALSE]
        /\ csleep = FALSE
        /\ sleepers = [x \in Slots |-> {}]
        /\ holds = {}
        /\ activeGen = 1
        /\ wAlive = [h \in GENS |-> TRUE]
        /\ wActive = [h \in GENS |-> h = 1]
        /\ hostAlive = TRUE
        /\ tAlive = [u \in Threads |-> TRUE]
        /\ sock = "open"
        /\ snapshot = 1
        /\ nextSnap = 2
        /\ marker = [u \in Threads |-> 0]
        /\ supPhase = "idle"
        /\ resetWanted = FALSE
        /\ crashes = 0
        /\ kills = 0
        /\ requested = 0
        /\ holders = [x \in Slots |-> {}]
        /\ reqEpoch = [x \in Slots |-> 0]
        /\ respEpoch = [x \in Slots |-> 0]
        /\ fates = [key \in Slots \X (0..MaxEpoch) |-> {}]
        /\ cellBy = [j \in 0..(CAP - 1) |-> 0]
        /\ handled = [h \in GENS |-> FALSE]
        /\ readySwept = [h \in GENS |-> FALSE]
        /\ repaired = [h \in GENS |-> FALSE]
        /\ sweptClosed = {}
        (* Process C *)
        /\ s = [self \in Threads |-> 0]
        /\ e = [self \in Threads |-> 0]
        /\ pos = [self \in Threads |-> 0]
        /\ m = [self \in Threads |-> 0]
        /\ k = [self \in Threads |-> 0]
        /\ spins = [self \in Threads |-> 0]
        /\ wv = [self \in Threads |-> Free(0)]
        /\ cw = [self \in Threads |-> "AWAKE"]
        /\ result = [self \in Threads |-> ""]
        /\ voided = [self \in Threads |-> FALSE]
        (* Process B *)
        /\ old = 0
        (* Process Wk *)
        /\ hl = [self \in Workers |-> 0]
        /\ d = [self \in Workers |-> [s |-> 0, e |-> 0, void |-> FALSE]]
        /\ wb = [self \in Workers |-> FALSE]
        (* Process S *)
        /\ g = 0
        /\ i = 1
        /\ pc = [self \in ProcSet |-> CASE self \in Threads -> "c_top"
                                        [] self = BGT -> "b_hold"
                                        [] self \in Workers -> "w_start"
                                        [] self = SUP -> "s_loop"
                                        [] self = ENV -> "e_loop"]

c_top(self) == /\ pc[self] = "c_top"
               /\ IF k[self] < NCalls
                     THEN /\ Live(self)
                          /\ IF Resets
                                THEN /\ IF snapshot = 0
                                           THEN /\ result' = [result EXCEPT ![self] = "NOT_CONNECTED"]
                                                /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                                                /\ UNCHANGED << marker, m >>
                                           ELSE /\ marker' = [marker EXCEPT ![self] = snapshot]
                                                /\ m' = [m EXCEPT ![self] = snapshot]
                                                /\ pc' = [pc EXCEPT ![self] = "c_enter2"]
                                                /\ UNCHANGED result
                                ELSE /\ pc' = [pc EXCEPT ![self] = "c_claim"]
                                     /\ UNCHANGED << marker, m, result >>
                     ELSE /\ pc' = [pc EXCEPT ![self] = "Done"]
                          /\ UNCHANGED << marker, m, result >>
               /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                               drainAck, csleep, sleepers, holds, activeGen, 
                               wAlive, wActive, hostAlive, tAlive, sock, 
                               snapshot, nextSnap, supPhase, resetWanted, 
                               crashes, kills, requested, holders, reqEpoch, 
                               respEpoch, fates, cellBy, handled, readySwept, 
                               repaired, sweptClosed, s, e, pos, k, spins, wv, 
                               cw, voided, old, hl, d, wb, g, i >>

c_claim(self) == /\ pc[self] = "c_claim"
                 /\ Live(self)
                 /\ IF gate.st # "HEALTHY"
                       THEN /\ result' = [result EXCEPT ![self] = gate.st]
                            /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                            /\ UNCHANGED << slot, holders, s, e >>
                       ELSE /\ IF \E x \in Slots : slot[x].st = "FREE"
                                  THEN /\ \E x \in {y \in Slots : slot[y].st = "FREE"}:
                                            /\ s' = [s EXCEPT ![self] = x]
                                            /\ e' = [e EXCEPT ![self] = slot[x].e + 1]
                                            /\ slot' = [slot EXCEPT ![x] = [e |-> slot[x].e + 1, st |-> "CLAIMED", w |-> FALSE, o |-> 0]]
                                            /\ holders' = [holders EXCEPT ![x] = holders[x] \cup {self}]
                                       /\ pc' = [pc EXCEPT ![self] = "c_ready"]
                                       /\ UNCHANGED result
                                  ELSE /\ result' = [result EXCEPT ![self] = "NO_SLOT"]
                                       /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                                       /\ UNCHANGED << slot, holders, s, e >>
                 /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                 drainAck, csleep, sleepers, holds, activeGen, 
                                 wAlive, wActive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 reqEpoch, respEpoch, fates, cellBy, handled, 
                                 readySwept, repaired, sweptClosed, pos, m, k, 
                                 spins, wv, cw, voided, old, hl, d, wb, g, i >>

c_ready(self) == /\ pc[self] = "c_ready"
                 /\ Live(self)
                 /\ reqEpoch' = [reqEpoch EXCEPT ![s[self]] = e[self]]
                 /\ slot' = [slot EXCEPT ![s[self]] = [e |-> e[self], st |-> "READY", w |-> FALSE, o |-> 0]]
                 /\ holders' = [holders EXCEPT ![s[self]] = holders[s[self]] \ {self}]
                 /\ pos' = [pos EXCEPT ![self] = tail]
                 /\ pc' = [pc EXCEPT ![self] = "c_enq"]
                 /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                 drainAck, csleep, sleepers, holds, activeGen, 
                                 wAlive, wActive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 respEpoch, fates, cellBy, handled, readySwept, 
                                 repaired, sweptClosed, s, e, m, k, spins, wv, 
                                 cw, result, voided, old, hl, d, wb, g, i >>

c_enq(self) == /\ pc[self] = "c_enq"
               /\ Live(self)
               /\ IF cells[pos[self] % CAP].seq = pos[self]
                     THEN /\ IF tail = pos[self]
                                THEN /\ tail' = pos[self] + 1
                                     /\ cellBy' = [cellBy EXCEPT ![pos[self] % CAP] = IF MaxKills > 0 THEN self ELSE 0]
                                     /\ pc' = [pc EXCEPT ![self] = "c_fill"]
                                     /\ pos' = pos
                                ELSE /\ pos' = [pos EXCEPT ![self] = tail]
                                     /\ pc' = [pc EXCEPT ![self] = "c_enq"]
                                     /\ UNCHANGED << tail, cellBy >>
                     ELSE /\ IF cells[pos[self] % CAP].seq < pos[self]
                                THEN /\ pc' = [pc EXCEPT ![self] = "c_full"]
                                     /\ pos' = pos
                                ELSE /\ pos' = [pos EXCEPT ![self] = tail]
                                     /\ pc' = [pc EXCEPT ![self] = "c_enq"]
                          /\ UNCHANGED << tail, cellBy >>
               /\ UNCHANGED << slot, cells, head, cwait, gate, ctlDrain, 
                               drainAck, csleep, sleepers, holds, activeGen, 
                               wAlive, wActive, hostAlive, tAlive, sock, 
                               snapshot, nextSnap, marker, supPhase, 
                               resetWanted, crashes, kills, requested, holders, 
                               reqEpoch, respEpoch, fates, handled, readySwept, 
                               repaired, sweptClosed, s, e, m, k, spins, wv, 
                               cw, result, voided, old, hl, d, wb, g, i >>

c_fill(self) == /\ pc[self] = "c_fill"
                /\ Live(self)
                /\ \/ /\ cells' = [cells EXCEPT ![pos[self] % CAP] = [cells[pos[self] % CAP] EXCEPT !.s = s[self], !.e = e[self], !.void = FALSE]]
                      /\ pc' = [pc EXCEPT ![self] = "c_publish"]
                      /\ UNCHANGED voided
                   \/ /\ Unwind
                      /\ cells' = [cells EXCEPT ![pos[self] % CAP] = [seq |-> pos[self] + 1, s |-> s[self], e |-> e[self], void |-> TRUE]]
                      /\ voided' = [voided EXCEPT ![self] = TRUE]
                      /\ IF Bug = "void_no_wake"
                            THEN /\ pc' = [pc EXCEPT ![self] = "c_unwound"]
                            ELSE /\ pc' = [pc EXCEPT ![self] = "c_wake1"]
                /\ UNCHANGED << slot, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, fates, cellBy, 
                                handled, readySwept, repaired, sweptClosed, s, 
                                e, pos, m, k, spins, wv, cw, result, old, hl, 
                                d, wb, g, i >>

c_publish(self) == /\ pc[self] = "c_publish"
                   /\ Live(self)
                   /\ cells' = [cells EXCEPT ![pos[self] % CAP].seq = pos[self] + 1]
                   /\ pc' = [pc EXCEPT ![self] = "c_wake1"]
                   /\ UNCHANGED << slot, tail, head, cwait, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, holders, reqEpoch, respEpoch, 
                                   fates, cellBy, handled, readySwept, 
                                   repaired, sweptClosed, s, e, pos, m, k, 
                                   spins, wv, cw, result, voided, old, hl, d, 
                                   wb, g, i >>

c_wake1(self) == /\ pc[self] = "c_wake1"
                 /\ Live(self)
                 /\ cw' = [cw EXCEPT ![self] = cwait]
                 /\ IF cw'[self] = "AWAKE"
                       THEN /\ IF voided[self]
                                  THEN /\ pc' = [pc EXCEPT ![self] = "c_unwound"]
                                  ELSE /\ pc' = [pc EXCEPT ![self] = "c_recheck"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "c_wake2"]
                 /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                 ctlDrain, drainAck, csleep, sleepers, holds, 
                                 activeGen, wAlive, wActive, hostAlive, tAlive, 
                                 sock, snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 holders, reqEpoch, respEpoch, fates, cellBy, 
                                 handled, readySwept, repaired, sweptClosed, s, 
                                 e, pos, m, k, spins, wv, result, voided, old, 
                                 hl, d, wb, g, i >>

c_wake2(self) == /\ pc[self] = "c_wake2"
                 /\ Live(self)
                 /\ IF cw[self] = "PARKED" /\ cwait = "PARKED"
                       THEN /\ cwait' = "WAKING"
                       ELSE /\ TRUE
                            /\ cwait' = cwait
                 /\ IF MaxKills = 0
                       THEN /\ csleep' = FALSE
                            /\ IF voided[self]
                                  THEN /\ pc' = [pc EXCEPT ![self] = "c_unwound"]
                                  ELSE /\ pc' = [pc EXCEPT ![self] = "c_recheck"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "c_wake3"]
                            /\ UNCHANGED csleep
                 /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                                 drainAck, sleepers, holds, activeGen, wAlive, 
                                 wActive, hostAlive, tAlive, sock, snapshot, 
                                 nextSnap, marker, supPhase, resetWanted, 
                                 crashes, kills, requested, holders, reqEpoch, 
                                 respEpoch, fates, cellBy, handled, readySwept, 
                                 repaired, sweptClosed, s, e, pos, m, k, spins, 
                                 wv, cw, result, voided, old, hl, d, wb, g, i >>

c_wake3(self) == /\ pc[self] = "c_wake3"
                 /\ Live(self)
                 /\ csleep' = FALSE
                 /\ IF voided[self]
                       THEN /\ pc' = [pc EXCEPT ![self] = "c_unwound"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "c_recheck"]
                 /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                 ctlDrain, drainAck, sleepers, holds, 
                                 activeGen, wAlive, wActive, hostAlive, tAlive, 
                                 sock, snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 holders, reqEpoch, respEpoch, fates, cellBy, 
                                 handled, readySwept, repaired, sweptClosed, s, 
                                 e, pos, m, k, spins, wv, cw, result, voided, 
                                 old, hl, d, wb, g, i >>

c_recheck(self) == /\ pc[self] = "c_recheck"
                   /\ Live(self)
                   /\ IF Bug # "no_recheck" /\ gate.st # "HEALTHY" /\ slot[s[self]].st = "READY" /\ slot[s[self]].e = e[self]
                         THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                              /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"cancelled"}]
                              /\ result' = [result EXCEPT ![self] = gate.st]
                              /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                         ELSE /\ pc' = [pc EXCEPT ![self] = "c_spin"]
                              /\ UNCHANGED << slot, fates, result >>
                   /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, holders, reqEpoch, respEpoch, 
                                   cellBy, handled, readySwept, repaired, 
                                   sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                                   voided, old, hl, d, wb, g, i >>

c_spin(self) == /\ pc[self] = "c_spin"
                /\ Live(self)
                /\ Assert(slot[s[self]].e = e[self], 
                          "Failure of assertion at line 203, column 9.")
                /\ IF slot[s[self]].st = "DONE"
                      THEN /\ pc' = [pc EXCEPT ![self] = "c_read"]
                           /\ spins' = spins
                      ELSE /\ IF slot[s[self]].st = "FAILED"
                                 THEN /\ pc' = [pc EXCEPT ![self] = "c_failed"]
                                      /\ spins' = spins
                                 ELSE /\ IF spins[self] >= SpinIters
                                            THEN /\ pc' = [pc EXCEPT ![self] = "c_park"]
                                                 /\ spins' = spins
                                            ELSE /\ IF slot[s[self]].st = "READY"
                                                       THEN /\ \/ /\ pc' = [pc EXCEPT ![self] = "c_spin_gate"]
                                                               \/ /\ pc' = [pc EXCEPT ![self] = "c_abandon"]
                                                            /\ spins' = spins
                                                       ELSE /\ spins' = [spins EXCEPT ![self] = spins[self] + 1]
                                                            /\ \/ /\ pc' = [pc EXCEPT ![self] = "c_spin"]
                                                               \/ /\ pc' = [pc EXCEPT ![self] = "c_abandon"]
                /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, fates, cellBy, 
                                handled, readySwept, repaired, sweptClosed, s, 
                                e, pos, m, k, wv, cw, result, voided, old, hl, 
                                d, wb, g, i >>

c_spin_gate(self) == /\ pc[self] = "c_spin_gate"
                     /\ Live(self)
                     /\ spins' = [spins EXCEPT ![self] = spins[self] + 1]
                     /\ IF gate.st # "HEALTHY" /\ slot[s[self]].st = "READY" /\ slot[s[self]].e = e[self]
                           THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                                /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"cancelled"}]
                                /\ result' = [result EXCEPT ![self] = gate.st]
                                /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                           ELSE /\ pc' = [pc EXCEPT ![self] = "c_spin"]
                                /\ UNCHANGED << slot, fates, result >>
                     /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                     drainAck, csleep, sleepers, holds, 
                                     activeGen, wAlive, wActive, hostAlive, 
                                     tAlive, sock, snapshot, nextSnap, marker, 
                                     supPhase, resetWanted, crashes, kills, 
                                     requested, holders, reqEpoch, respEpoch, 
                                     cellBy, handled, readySwept, repaired, 
                                     sweptClosed, s, e, pos, m, k, wv, cw, 
                                     voided, old, hl, d, wb, g, i >>

c_park(self) == /\ pc[self] = "c_park"
                /\ Live(self)
                /\ IF slot[s[self]].st = "DONE"
                      THEN /\ pc' = [pc EXCEPT ![self] = "c_read"]
                           /\ UNCHANGED << slot, wv >>
                      ELSE /\ IF slot[s[self]].st = "FAILED"
                                 THEN /\ pc' = [pc EXCEPT ![self] = "c_failed"]
                                      /\ UNCHANGED << slot, wv >>
                                 ELSE /\ \/ /\ pc' = [pc EXCEPT ![self] = "c_abandon"]
                                            /\ UNCHANGED <<slot, wv>>
                                         \/ /\ wv' = [wv EXCEPT ![self] = [slot[s[self]] EXCEPT !.w = TRUE]]
                                            /\ slot' = [slot EXCEPT ![s[self]].w = TRUE]
                                            /\ pc' = [pc EXCEPT ![self] = "c_fwait"]
                /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, fates, cellBy, 
                                handled, readySwept, repaired, sweptClosed, s, 
                                e, pos, m, k, spins, cw, result, voided, old, 
                                hl, d, wb, g, i >>

c_fwait(self) == /\ pc[self] = "c_fwait"
                 /\ Live(self)
                 /\ IF Low(slot[s[self]]) = Low(wv[self])
                       THEN /\ sleepers' = [sleepers EXCEPT ![s[self]] = sleepers[s[self]] \cup {self}]
                            /\ pc' = [pc EXCEPT ![self] = "c_asleep"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "c_park"]
                            /\ UNCHANGED sleepers
                 /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                 ctlDrain, drainAck, csleep, holds, activeGen, 
                                 wAlive, wActive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 holders, reqEpoch, respEpoch, fates, cellBy, 
                                 handled, readySwept, repaired, sweptClosed, s, 
                                 e, pos, m, k, spins, wv, cw, result, voided, 
                                 old, hl, d, wb, g, i >>

c_asleep(self) == /\ pc[self] = "c_asleep"
                  /\ Live(self)
                  /\ \/ /\ self \notin sleepers[s[self]]
                        /\ pc' = [pc EXCEPT ![self] = "c_park"]
                        /\ UNCHANGED sleepers
                     \/ /\ sleepers' = [sleepers EXCEPT ![s[self]] = sleepers[s[self]] \ {self}]
                        /\ pc' = [pc EXCEPT ![self] = "c_abandon"]
                  /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                  ctlDrain, drainAck, csleep, holds, activeGen, 
                                  wAlive, wActive, hostAlive, tAlive, sock, 
                                  snapshot, nextSnap, marker, supPhase, 
                                  resetWanted, crashes, kills, requested, 
                                  holders, reqEpoch, respEpoch, fates, cellBy, 
                                  handled, readySwept, repaired, sweptClosed, 
                                  s, e, pos, m, k, spins, wv, cw, result, 
                                  voided, old, hl, d, wb, g, i >>

c_abandon(self) == /\ pc[self] = "c_abandon"
                   /\ Live(self)
                   /\ wv' = [wv EXCEPT ![self] = slot[s[self]]]
                   /\ Assert(wv'[self].e = e[self], 
                             "Failure of assertion at line 243, column 9.")
                   /\ IF wv'[self].st = "READY"
                         THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                              /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"cancelled"}]
                              /\ result' = [result EXCEPT ![self] = "DEADLINE"]
                              /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                         ELSE /\ IF wv'[self].st = "PROCESSING"
                                    THEN /\ slot' = [slot EXCEPT ![s[self]] = [wv'[self] EXCEPT !.st = "ABANDONED", !.w = FALSE,
                                                                                                !.o = IF Bug = "abandon_drops_owner" THEN 0 ELSE wv'[self].o]]
                                         /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"abandoned"}]
                                         /\ result' = [result EXCEPT ![self] = "DEADLINE"]
                                         /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                                    ELSE /\ IF wv'[self].st = "DONE"
                                               THEN /\ pc' = [pc EXCEPT ![self] = "c_read"]
                                               ELSE /\ IF wv'[self].st = "FAILED"
                                                          THEN /\ pc' = [pc EXCEPT ![self] = "c_failed"]
                                                          ELSE /\ Assert(FALSE, 
                                                                         "Failure of assertion at line 257, column 16.")
                                                               /\ pc' = [pc EXCEPT ![self] = "c_read"]
                                         /\ UNCHANGED << slot, fates, result >>
                   /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, holders, reqEpoch, respEpoch, 
                                   cellBy, handled, readySwept, repaired, 
                                   sweptClosed, s, e, pos, m, k, spins, cw, 
                                   voided, old, hl, d, wb, g, i >>

c_read(self) == /\ pc[self] = "c_read"
                /\ Live(self)
                /\ Assert(respEpoch[s[self]] = e[self], 
                          "Failure of assertion at line 261, column 9.")
                /\ Assert("done" \in fates[<<s[self], e[self]>>], 
                          "Failure of assertion at line 262, column 9.")
                /\ holders' = [holders EXCEPT ![s[self]] = holders[s[self]] \cup {self}]
                /\ pc' = [pc EXCEPT ![self] = "c_release"]
                /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                reqEpoch, respEpoch, fates, cellBy, handled, 
                                readySwept, repaired, sweptClosed, s, e, pos, 
                                m, k, spins, wv, cw, result, voided, old, hl, 
                                d, wb, g, i >>

c_release(self) == /\ pc[self] = "c_release"
                   /\ Live(self)
                   /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                   /\ holders' = [holders EXCEPT ![s[self]] = holders[s[self]] \ {self}]
                   /\ result' = [result EXCEPT ![self] = "MODEL"]
                   /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                   /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, reqEpoch, respEpoch, fates, 
                                   cellBy, handled, readySwept, repaired, 
                                   sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                                   voided, old, hl, d, wb, g, i >>

c_failed(self) == /\ pc[self] = "c_failed"
                  /\ Live(self)
                  /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                  /\ result' = [result EXCEPT ![self] = "WORKER_FAILED"]
                  /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                  /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                  drainAck, csleep, sleepers, holds, activeGen, 
                                  wAlive, wActive, hostAlive, tAlive, sock, 
                                  snapshot, nextSnap, marker, supPhase, 
                                  resetWanted, crashes, kills, requested, 
                                  holders, reqEpoch, respEpoch, fates, cellBy, 
                                  handled, readySwept, repaired, sweptClosed, 
                                  s, e, pos, m, k, spins, wv, cw, voided, old, 
                                  hl, d, wb, g, i >>

c_full(self) == /\ pc[self] = "c_full"
                /\ Live(self)
                /\ IF slot[s[self]].st = "READY" /\ slot[s[self]].e = e[self]
                      THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                           /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"cancelled"}]
                      ELSE /\ IF slot[s[self]].st = "FAILED"
                                 THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                                 ELSE /\ TRUE
                                      /\ slot' = slot
                           /\ fates' = fates
                /\ result' = [result EXCEPT ![self] = "RING_FULL"]
                /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, cellBy, handled, 
                                readySwept, repaired, sweptClosed, s, e, pos, 
                                m, k, spins, wv, cw, voided, old, hl, d, wb, g, 
                                i >>

c_unwound(self) == /\ pc[self] = "c_unwound"
                   /\ Live(self)
                   /\ IF slot[s[self]].st = "READY" /\ slot[s[self]].e = e[self]
                         THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                              /\ fates' = [fates EXCEPT ![<<s[self], e[self]>>] = fates[<<s[self], e[self]>>] \cup {"cancelled"}]
                         ELSE /\ IF slot[s[self]].st = "FAILED"
                                    THEN /\ slot' = [slot EXCEPT ![s[self]] = Free(e[self])]
                                    ELSE /\ TRUE
                                         /\ slot' = slot
                              /\ fates' = fates
                   /\ result' = [result EXCEPT ![self] = "CODEC_ERROR"]
                   /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                   /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, holders, reqEpoch, respEpoch, 
                                   cellBy, handled, readySwept, repaired, 
                                   sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                                   voided, old, hl, d, wb, g, i >>

c_exit(self) == /\ pc[self] = "c_exit"
                /\ Live(self)
                /\ marker' = [marker EXCEPT ![self] = 0]
                /\ k' = [k EXCEPT ![self] = k[self] + 1]
                /\ s' = [s EXCEPT ![self] = 0]
                /\ e' = [e EXCEPT ![self] = 0]
                /\ pos' = [pos EXCEPT ![self] = 0]
                /\ m' = [m EXCEPT ![self] = 0]
                /\ spins' = [spins EXCEPT ![self] = 0]
                /\ wv' = [wv EXCEPT ![self] = Free(0)]
                /\ cw' = [cw EXCEPT ![self] = "AWAKE"]
                /\ result' = [result EXCEPT ![self] = ""]
                /\ voided' = [voided EXCEPT ![self] = FALSE]
                /\ pc' = [pc EXCEPT ![self] = "c_top"]
                /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, supPhase, resetWanted, 
                                crashes, kills, requested, holders, reqEpoch, 
                                respEpoch, fates, cellBy, handled, readySwept, 
                                repaired, sweptClosed, old, hl, d, wb, g, i >>

c_enter2(self) == /\ pc[self] = "c_enter2"
                  /\ Live(self)
                  /\ IF snapshot # m[self]
                        THEN /\ result' = [result EXCEPT ![self] = "NOT_CONNECTED"]
                             /\ pc' = [pc EXCEPT ![self] = "c_exit"]
                        ELSE /\ pc' = [pc EXCEPT ![self] = "c_claim"]
                             /\ UNCHANGED result
                  /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                  ctlDrain, drainAck, csleep, sleepers, holds, 
                                  activeGen, wAlive, wActive, hostAlive, 
                                  tAlive, sock, snapshot, nextSnap, marker, 
                                  supPhase, resetWanted, crashes, kills, 
                                  requested, holders, reqEpoch, respEpoch, 
                                  fates, cellBy, handled, readySwept, repaired, 
                                  sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                                  voided, old, hl, d, wb, g, i >>

C(self) == c_top(self) \/ c_claim(self) \/ c_ready(self) \/ c_enq(self)
              \/ c_fill(self) \/ c_publish(self) \/ c_wake1(self)
              \/ c_wake2(self) \/ c_wake3(self) \/ c_recheck(self)
              \/ c_spin(self) \/ c_spin_gate(self) \/ c_park(self)
              \/ c_fwait(self) \/ c_asleep(self) \/ c_abandon(self)
              \/ c_read(self) \/ c_release(self) \/ c_failed(self)
              \/ c_full(self) \/ c_unwound(self) \/ c_exit(self)
              \/ c_enter2(self)

b_hold == /\ pc[BGT] = "b_hold"
          /\ hostAlive /\ sock = "half"
          /\ old' = snapshot
          /\ snapshot' = 0
          /\ pc' = [pc EXCEPT ![BGT] = "b_retire"]
          /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                          drainAck, csleep, sleepers, holds, activeGen, wAlive, 
                          wActive, hostAlive, tAlive, sock, nextSnap, marker, 
                          supPhase, resetWanted, crashes, kills, requested, 
                          holders, reqEpoch, respEpoch, fates, cellBy, handled, 
                          readySwept, repaired, sweptClosed, s, e, pos, m, k, 
                          spins, wv, cw, result, voided, hl, d, wb, g, i >>

b_retire == /\ pc[BGT] = "b_retire"
            /\ hostAlive /\ (Bug = "ignore_markers" \/ \A t \in Threads : marker[t] # old \/ ~tAlive[t])
            /\ sock' = "closed"
            /\ old' = 0
            /\ pc' = [pc EXCEPT ![BGT] = "b_connect"]
            /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                            drainAck, csleep, sleepers, holds, activeGen, 
                            wAlive, wActive, hostAlive, tAlive, snapshot, 
                            nextSnap, marker, supPhase, resetWanted, crashes, 
                            kills, requested, holders, reqEpoch, respEpoch, 
                            fates, cellBy, handled, readySwept, repaired, 
                            sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                            result, voided, hl, d, wb, g, i >>

b_connect == /\ pc[BGT] = "b_connect"
             /\ hostAlive /\ sock = "none"
             /\ snapshot' = nextSnap
             /\ nextSnap' = nextSnap + 1
             /\ sock' = "open"
             /\ pc' = [pc EXCEPT ![BGT] = "b_hold"]
             /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                             drainAck, csleep, sleepers, holds, activeGen, 
                             wAlive, wActive, hostAlive, tAlive, marker, 
                             supPhase, resetWanted, crashes, kills, requested, 
                             holders, reqEpoch, respEpoch, fates, cellBy, 
                             handled, readySwept, repaired, sweptClosed, s, e, 
                             pos, m, k, spins, wv, cw, result, voided, old, hl, 
                             d, wb, g, i >>

B == b_hold \/ b_retire \/ b_connect

w_start(self) == /\ pc[self] = "w_start"
                 /\ activeGen = GenOf(self) /\ wAlive[GenOf(self)]
                 /\ hl' = [hl EXCEPT ![self] = head]
                 /\ wActive' = [wActive EXCEPT ![GenOf(self)] = TRUE]
                 /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                 /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                 ctlDrain, drainAck, csleep, sleepers, holds, 
                                 activeGen, wAlive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 holders, reqEpoch, respEpoch, fates, cellBy, 
                                 handled, readySwept, repaired, sweptClosed, s, 
                                 e, pos, m, k, spins, wv, cw, result, voided, 
                                 old, d, wb, g, i >>

w_loop(self) == /\ pc[self] = "w_loop"
                /\ WLive(self)
                /\ IF ctlDrain /\ ~drainAck[GenOf(self)]
                      THEN /\ drainAck' = [drainAck EXCEPT ![GenOf(self)] = TRUE]
                           /\ d' = [d EXCEPT ![self] = [s |-> 0, e |-> 0, void |-> FALSE]]
                           /\ pc' = [pc EXCEPT ![self] = "w_paused"]
                      ELSE /\ IF cells[hl[self] % CAP].seq = hl[self] + 1
                                 THEN /\ d' = [d EXCEPT ![self] = [s |-> cells[hl[self] % CAP].s, e |-> cells[hl[self] % CAP].e, void |-> cells[hl[self] % CAP].void]]
                                      /\ pc' = [pc EXCEPT ![self] = "w_stepA"]
                                 ELSE /\ d' = [d EXCEPT ![self] = [s |-> 0, e |-> 0, void |-> FALSE]]
                                      /\ pc' = [pc EXCEPT ![self] = "w_park"]
                           /\ UNCHANGED drainAck
                /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                                csleep, sleepers, holds, activeGen, wAlive, 
                                wActive, hostAlive, tAlive, sock, snapshot, 
                                nextSnap, marker, supPhase, resetWanted, 
                                crashes, kills, requested, holders, reqEpoch, 
                                respEpoch, fates, cellBy, handled, readySwept, 
                                repaired, sweptClosed, s, e, pos, m, k, spins, 
                                wv, cw, result, voided, old, hl, wb, g, i >>

w_stepA(self) == /\ pc[self] = "w_stepA"
                 /\ WLive(self)
                 /\ cells' = [cells EXCEPT ![hl[self] % CAP].seq = hl[self] + CAP]
                 /\ IF MaxWorkerCrashes = 0
                       THEN /\ head' = hl[self] + 1
                            /\ hl' = [hl EXCEPT ![self] = hl[self] + 1]
                            /\ IF d[self].void
                                  THEN /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                                  ELSE /\ pc' = [pc EXCEPT ![self] = "w_claim"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "w_stepB"]
                            /\ UNCHANGED << head, hl >>
                 /\ UNCHANGED << slot, tail, cwait, gate, ctlDrain, drainAck, 
                                 csleep, sleepers, holds, activeGen, wAlive, 
                                 wActive, hostAlive, tAlive, sock, snapshot, 
                                 nextSnap, marker, supPhase, resetWanted, 
                                 crashes, kills, requested, holders, reqEpoch, 
                                 respEpoch, fates, cellBy, handled, readySwept, 
                                 repaired, sweptClosed, s, e, pos, m, k, spins, 
                                 wv, cw, result, voided, old, d, wb, g, i >>

w_stepB(self) == /\ pc[self] = "w_stepB"
                 /\ WLive(self)
                 /\ head' = hl[self] + 1
                 /\ hl' = [hl EXCEPT ![self] = hl[self] + 1]
                 /\ IF d[self].void
                       THEN /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "w_claim"]
                 /\ UNCHANGED << slot, cells, tail, cwait, gate, ctlDrain, 
                                 drainAck, csleep, sleepers, holds, activeGen, 
                                 wAlive, wActive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 holders, reqEpoch, respEpoch, fates, cellBy, 
                                 handled, readySwept, repaired, sweptClosed, s, 
                                 e, pos, m, k, spins, wv, cw, result, voided, 
                                 old, d, wb, g, i >>

w_claim(self) == /\ pc[self] = "w_claim"
                 /\ WLive(self)
                 /\ IF slot[d[self].s].st = "READY" /\ slot[d[self].s].e = d[self].e
                       THEN /\ Assert(reqEpoch[d[self].s] = d[self].e, 
                                      "Failure of assertion at line 358, column 13.")
                            /\ slot' = [slot EXCEPT ![d[self].s] = [slot[d[self].s] EXCEPT !.st = "PROCESSING", !.o = OwnerOf(GenOf(self))]]
                            /\ holders' = [holders EXCEPT ![d[self].s] = holders[d[self].s] \cup {self}]
                            /\ pc' = [pc EXCEPT ![self] = "w_complete"]
                       ELSE /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                            /\ UNCHANGED << slot, holders >>
                 /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                 drainAck, csleep, sleepers, holds, activeGen, 
                                 wAlive, wActive, hostAlive, tAlive, sock, 
                                 snapshot, nextSnap, marker, supPhase, 
                                 resetWanted, crashes, kills, requested, 
                                 reqEpoch, respEpoch, fates, cellBy, handled, 
                                 readySwept, repaired, sweptClosed, s, e, pos, 
                                 m, k, spins, wv, cw, result, voided, old, hl, 
                                 d, wb, g, i >>

w_complete(self) == /\ pc[self] = "w_complete"
                    /\ WLive(self)
                    /\ respEpoch' = [respEpoch EXCEPT ![d[self].s] = d[self].e]
                    /\ wb' = [wb EXCEPT ![self] = slot[d[self].s].w]
                    /\ holders' = [holders EXCEPT ![d[self].s] = holders[d[self].s] \ {self}]
                    /\ IF slot[d[self].s].e = d[self].e /\ slot[d[self].s].st = "ABANDONED"
                          THEN /\ slot' = [slot EXCEPT ![d[self].s] = Free(d[self].e)]
                               /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                               /\ fates' = fates
                          ELSE /\ IF slot[d[self].s].e = d[self].e /\ slot[d[self].s].st = "PROCESSING"
                                     THEN /\ slot' = [slot EXCEPT ![d[self].s] = [e |-> d[self].e, st |-> "DONE", w |-> FALSE, o |-> 0]]
                                          /\ fates' = [fates EXCEPT ![<<d[self].s, d[self].e>>] = fates[<<d[self].s, d[self].e>>] \cup {"done"}]
                                          /\ IF ~wb'[self] \/ Bug = "no_complete_wake"
                                                THEN /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                                                ELSE /\ pc' = [pc EXCEPT ![self] = "w_wake"]
                                     ELSE /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                                          /\ UNCHANGED << slot, fates >>
                    /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                                    drainAck, csleep, sleepers, holds, 
                                    activeGen, wAlive, wActive, hostAlive, 
                                    tAlive, sock, snapshot, nextSnap, marker, 
                                    supPhase, resetWanted, crashes, kills, 
                                    requested, reqEpoch, cellBy, handled, 
                                    readySwept, repaired, sweptClosed, s, e, 
                                    pos, m, k, spins, wv, cw, result, voided, 
                                    old, hl, d, g, i >>

w_wake(self) == /\ pc[self] = "w_wake"
                /\ WLive(self)
                /\ sleepers' = [sleepers EXCEPT ![d[self].s] = {}]
                /\ wb' = [wb EXCEPT ![self] = FALSE]
                /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                                drainAck, csleep, holds, activeGen, wAlive, 
                                wActive, hostAlive, tAlive, sock, snapshot, 
                                nextSnap, marker, supPhase, resetWanted, 
                                crashes, kills, requested, holders, reqEpoch, 
                                respEpoch, fates, cellBy, handled, readySwept, 
                                repaired, sweptClosed, s, e, pos, m, k, spins, 
                                wv, cw, result, voided, old, hl, d, g, i >>

w_park(self) == /\ pc[self] = "w_park"
                /\ WLive(self)
                /\ cwait' = "PARKED"
                /\ pc' = [pc EXCEPT ![self] = "w_recheck"]
                /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                                drainAck, csleep, sleepers, holds, activeGen, 
                                wAlive, wActive, hostAlive, tAlive, sock, 
                                snapshot, nextSnap, marker, supPhase, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, fates, cellBy, 
                                handled, readySwept, repaired, sweptClosed, s, 
                                e, pos, m, k, spins, wv, cw, result, voided, 
                                old, hl, d, wb, g, i >>

w_recheck(self) == /\ pc[self] = "w_recheck"
                   /\ WLive(self)
                   /\ IF cells[hl[self] % CAP].seq = hl[self] + 1 \/ (ctlDrain /\ ~drainAck[GenOf(self)])
                         THEN /\ cwait' = "AWAKE"
                              /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                         ELSE /\ pc' = [pc EXCEPT ![self] = "w_fwait"]
                              /\ cwait' = cwait
                   /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                                   drainAck, csleep, sleepers, holds, 
                                   activeGen, wAlive, wActive, hostAlive, 
                                   tAlive, sock, snapshot, nextSnap, marker, 
                                   supPhase, resetWanted, crashes, kills, 
                                   requested, holders, reqEpoch, respEpoch, 
                                   fates, cellBy, handled, readySwept, 
                                   repaired, sweptClosed, s, e, pos, m, k, 
                                   spins, wv, cw, result, voided, old, hl, d, 
                                   wb, g, i >>

w_fwait(self) == /\ pc[self] = "w_fwait"
                 /\ WLive(self)
                 /\ IF cwait = "PARKED"
                       THEN /\ csleep' = TRUE
                            /\ pc' = [pc EXCEPT ![self] = "w_asleep"]
                            /\ cwait' = cwait
                       ELSE /\ cwait' = "AWAKE"
                            /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                            /\ UNCHANGED csleep
                 /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                                 drainAck, sleepers, holds, activeGen, wAlive, 
                                 wActive, hostAlive, tAlive, sock, snapshot, 
                                 nextSnap, marker, supPhase, resetWanted, 
                                 crashes, kills, requested, holders, reqEpoch, 
                                 respEpoch, fates, cellBy, handled, readySwept, 
                                 repaired, sweptClosed, s, e, pos, m, k, spins, 
                                 wv, cw, result, voided, old, hl, d, wb, g, i >>

w_asleep(self) == /\ pc[self] = "w_asleep"
                  /\ WLive(self) /\ ~csleep
                  /\ cwait' = "AWAKE"
                  /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                  /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                                  drainAck, csleep, sleepers, holds, activeGen, 
                                  wAlive, wActive, hostAlive, tAlive, sock, 
                                  snapshot, nextSnap, marker, supPhase, 
                                  resetWanted, crashes, kills, requested, 
                                  holders, reqEpoch, respEpoch, fates, cellBy, 
                                  handled, readySwept, repaired, sweptClosed, 
                                  s, e, pos, m, k, spins, wv, cw, result, 
                                  voided, old, hl, d, wb, g, i >>

w_paused(self) == /\ pc[self] = "w_paused"
                  /\ WLive(self) /\ ~ctlDrain
                  /\ drainAck' = [drainAck EXCEPT ![GenOf(self)] = FALSE]
                  /\ hl' = [hl EXCEPT ![self] = head]
                  /\ pc' = [pc EXCEPT ![self] = "w_loop"]
                  /\ UNCHANGED << slot, cells, tail, head, cwait, gate, 
                                  ctlDrain, csleep, sleepers, holds, activeGen, 
                                  wAlive, wActive, hostAlive, tAlive, sock, 
                                  snapshot, nextSnap, marker, supPhase, 
                                  resetWanted, crashes, kills, requested, 
                                  holders, reqEpoch, respEpoch, fates, cellBy, 
                                  handled, readySwept, repaired, sweptClosed, 
                                  s, e, pos, m, k, spins, wv, cw, result, 
                                  voided, old, d, wb, g, i >>

Wk(self) == w_start(self) \/ w_loop(self) \/ w_stepA(self) \/ w_stepB(self)
               \/ w_claim(self) \/ w_complete(self) \/ w_wake(self)
               \/ w_park(self) \/ w_recheck(self) \/ w_fwait(self)
               \/ w_asleep(self) \/ w_paused(self)

s_loop == /\ pc[SUP] = "s_loop"
          /\ \/ /\ ~wAlive[activeGen] /\ ~handled[activeGen]
                /\ g' = activeGen
                /\ IF Bug # "sweep_before_gate"
                      THEN /\ holds' = (holds \cup {"DEAD"})
                           /\ gate' = GateOf(holds' \cup {"DEAD"}, gate.gen)
                      ELSE /\ TRUE
                           /\ UNCHANGED << gate, holds >>
                /\ i' = 1
                /\ pc' = [pc EXCEPT ![SUP] = "s_ready"]
                /\ UNCHANGED <<cwait, ctlDrain, csleep, supPhase, sweptClosed>>
             \/ /\ "DEAD" \in holds /\ wActive[activeGen] /\ wAlive[activeGen]
                /\ sweptClosed' = sweptClosed \ {"DEAD"}
                /\ holds' = holds \ {"DEAD"}
                /\ gate' = GateOf(holds' \ {"DEAD"}, gate.gen)
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<cwait, ctlDrain, csleep, supPhase, g, i>>
             \/ /\ sock = "closed" /\ supPhase \in {"idle", "await_close"}
                /\ holds' = (holds \cup {"RESET"})
                /\ gate' = GateOf(holds' \cup {"RESET"}, gate.gen)
                /\ ctlDrain' = TRUE
                /\ supPhase' = "draining"
                /\ pc' = [pc EXCEPT ![SUP] = "s_ctl_wake"]
                /\ UNCHANGED <<cwait, csleep, sweptClosed, g, i>>
             \/ /\ supPhase = "draining" /\ (drainAck[activeGen] \/ ~wAlive[activeGen])
                /\ pc' = [pc EXCEPT ![SUP] = "s_reinit"]
                /\ UNCHANGED <<cwait, gate, ctlDrain, csleep, holds, supPhase, sweptClosed, g, i>>
             \/ /\ supPhase = "idle" /\ sock = "open"
                   /\ (resetWanted \/ (HeadStuck /\ Bug # "no_stuck_reset"))
                /\ holds' = (holds \cup {"RESET"})
                /\ gate' = GateOf(holds' \cup {"RESET"}, gate.gen)
                /\ i' = 1
                /\ pc' = [pc EXCEPT ![SUP] = "s_rq_fail"]
                /\ UNCHANGED <<cwait, ctlDrain, csleep, supPhase, sweptClosed, g>>
             \/ /\ wAlive[activeGen] /\ csleep /\ cwait # "AWAKE"
                   /\ (cells[head % CAP].seq = head + 1 \/ (ctlDrain /\ ~drainAck[activeGen]))
                /\ IF cwait = "PARKED"
                      THEN /\ cwait' = "WAKING"
                      ELSE /\ TRUE
                           /\ cwait' = cwait
                /\ csleep' = FALSE
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED <<gate, ctlDrain, holds, supPhase, sweptClosed, g, i>>
          /\ UNCHANGED << slot, cells, tail, head, drainAck, sleepers, 
                          activeGen, wAlive, wActive, hostAlive, tAlive, sock, 
                          snapshot, nextSnap, marker, resetWanted, crashes, 
                          kills, requested, holders, reqEpoch, respEpoch, 
                          fates, cellBy, handled, readySwept, repaired, s, e, 
                          pos, m, k, spins, wv, cw, result, voided, old, hl, d, 
                          wb >>

s_ready == /\ pc[SUP] = "s_ready"
           /\ IF i <= NSlots
                 THEN /\ IF slot[i].st = "READY"
                            THEN /\ fates' = [fates EXCEPT ![<<i, slot[i].e>>] = fates[<<i, slot[i].e>>] \cup {"failed"}]
                                 /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0]]
                                 /\ sleepers' = [sleepers EXCEPT ![i] = {}]
                            ELSE /\ IF slot[i].st = "DONE" /\ Bug # "no_done_wake"
                                       THEN /\ sleepers' = [sleepers EXCEPT ![i] = {}]
                                       ELSE /\ TRUE
                                            /\ UNCHANGED sleepers
                                 /\ UNCHANGED << slot, fates >>
                      /\ i' = i + 1
                      /\ pc' = [pc EXCEPT ![SUP] = "s_ready"]
                 ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_gate_late"]
                      /\ UNCHANGED << slot, sleepers, fates, i >>
           /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, drainAck, 
                           csleep, holds, activeGen, wAlive, wActive, 
                           hostAlive, tAlive, sock, snapshot, nextSnap, marker, 
                           supPhase, resetWanted, crashes, kills, requested, 
                           holders, reqEpoch, respEpoch, cellBy, handled, 
                           readySwept, repaired, sweptClosed, s, e, pos, m, k, 
                           spins, wv, cw, result, voided, old, hl, d, wb, g >>

s_gate_late == /\ pc[SUP] = "s_gate_late"
               /\ readySwept' = [readySwept EXCEPT ![g] = TRUE]
               /\ sweptClosed' = (sweptClosed \cup {"DEAD"})
               /\ IF Bug = "sweep_before_gate"
                     THEN /\ holds' = (holds \cup {"DEAD"})
                          /\ gate' = GateOf(holds' \cup {"DEAD"}, gate.gen)
                     ELSE /\ TRUE
                          /\ UNCHANGED << gate, holds >>
               /\ pc' = [pc EXCEPT ![SUP] = "s_repair"]
               /\ UNCHANGED << slot, cells, tail, head, cwait, ctlDrain, 
                               drainAck, csleep, sleepers, activeGen, wAlive, 
                               wActive, hostAlive, tAlive, sock, snapshot, 
                               nextSnap, marker, supPhase, resetWanted, 
                               crashes, kills, requested, holders, reqEpoch, 
                               respEpoch, fates, cellBy, handled, repaired, s, 
                               e, pos, m, k, spins, wv, cw, result, voided, 
                               old, hl, d, wb, g, i >>

s_repair == /\ pc[SUP] = "s_repair"
            /\ IF IF Bug = "repair_equality" THEN cells[head % CAP].seq = head + CAP
                  ELSE cells[head % CAP].seq >= head + CAP
                  THEN /\ head' = head + 1
                  ELSE /\ TRUE
                       /\ head' = head
            /\ repaired' = [repaired EXCEPT ![g] = TRUE]
            /\ i' = 1
            /\ pc' = [pc EXCEPT ![SUP] = "s_owned"]
            /\ UNCHANGED << slot, cells, tail, cwait, gate, ctlDrain, drainAck, 
                            csleep, sleepers, holds, activeGen, wAlive, 
                            wActive, hostAlive, tAlive, sock, snapshot, 
                            nextSnap, marker, supPhase, resetWanted, crashes, 
                            kills, requested, holders, reqEpoch, respEpoch, 
                            fates, cellBy, handled, readySwept, sweptClosed, s, 
                            e, pos, m, k, spins, wv, cw, result, voided, old, 
                            hl, d, wb, g >>

s_owned == /\ pc[SUP] = "s_owned"
           /\ IF i <= NSlots
                 THEN /\ IF slot[i].o = OwnerOf(g) /\ slot[i].st = "PROCESSING"
                            THEN /\ fates' = [fates EXCEPT ![<<i, slot[i].e>>] = fates[<<i, slot[i].e>>] \cup {"failed"}]
                                 /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0]]
                                 /\ sleepers' = [sleepers EXCEPT ![i] = {}]
                            ELSE /\ IF slot[i].o = OwnerOf(g) /\ slot[i].st = "ABANDONED"
                                       THEN /\ slot' = [slot EXCEPT ![i] = Free(slot[i].e)]
                                       ELSE /\ TRUE
                                            /\ slot' = slot
                                 /\ UNCHANGED << sleepers, fates >>
                      /\ i' = i + 1
                      /\ pc' = [pc EXCEPT ![SUP] = "s_owned"]
                 ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_successor"]
                      /\ UNCHANGED << slot, sleepers, fates, i >>
           /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, drainAck, 
                           csleep, holds, activeGen, wAlive, wActive, 
                           hostAlive, tAlive, sock, snapshot, nextSnap, marker, 
                           supPhase, resetWanted, crashes, kills, requested, 
                           holders, reqEpoch, respEpoch, cellBy, handled, 
                           readySwept, repaired, sweptClosed, s, e, pos, m, k, 
                           spins, wv, cw, result, voided, old, hl, d, wb, g >>

s_successor == /\ pc[SUP] = "s_successor"
               /\ handled' = [handled EXCEPT ![g] = TRUE]
               /\ IF g + 1 \in GENS
                     THEN /\ activeGen' = g + 1
                          /\ gate' = GateOf(holds, g + 1)
                     ELSE /\ TRUE
                          /\ UNCHANGED << gate, activeGen >>
               /\ g' = 0
               /\ i' = 1
               /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
               /\ UNCHANGED << slot, cells, tail, head, cwait, ctlDrain, 
                               drainAck, csleep, sleepers, holds, wAlive, 
                               wActive, hostAlive, tAlive, sock, snapshot, 
                               nextSnap, marker, supPhase, resetWanted, 
                               crashes, kills, requested, holders, reqEpoch, 
                               respEpoch, fates, cellBy, readySwept, repaired, 
                               sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                               result, voided, old, hl, d, wb >>

s_ctl_wake == /\ pc[SUP] = "s_ctl_wake"
              /\ IF cwait = "PARKED"
                    THEN /\ cwait' = "WAKING"
                    ELSE /\ TRUE
                         /\ cwait' = cwait
              /\ csleep' = FALSE
              /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
              /\ UNCHANGED << slot, cells, tail, head, gate, ctlDrain, 
                              drainAck, sleepers, holds, activeGen, wAlive, 
                              wActive, hostAlive, tAlive, sock, snapshot, 
                              nextSnap, marker, supPhase, resetWanted, crashes, 
                              kills, requested, holders, reqEpoch, respEpoch, 
                              fates, cellBy, handled, readySwept, repaired, 
                              sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                              result, voided, old, hl, d, wb, g, i >>

s_reinit == /\ pc[SUP] = "s_reinit"
            /\ cells' = Fresh
            /\ head' = 0
            /\ tail' = 0
            /\ cwait' = "AWAKE"
            /\ slot' = [x \in Slots |-> Free(slot[x].e)]
            /\ sleepers' = [x \in Slots |-> {}]
            /\ pc' = [pc EXCEPT ![SUP] = "s_reset_done"]
            /\ UNCHANGED << gate, ctlDrain, drainAck, csleep, holds, activeGen, 
                            wAlive, wActive, hostAlive, tAlive, sock, snapshot, 
                            nextSnap, marker, supPhase, resetWanted, crashes, 
                            kills, requested, holders, reqEpoch, respEpoch, 
                            fates, cellBy, handled, readySwept, repaired, 
                            sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                            result, voided, old, hl, d, wb, g, i >>

s_reset_done == /\ pc[SUP] = "s_reset_done"
                /\ sweptClosed' = sweptClosed \ {"RESET"}
                /\ ctlDrain' = FALSE
                /\ holds' = holds \ {"RESET"}
                /\ gate' = GateOf(holds' \ {"RESET"}, gate.gen)
                /\ supPhase' = "idle"
                /\ sock' = "none"
                /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
                /\ UNCHANGED << slot, cells, tail, head, cwait, drainAck, 
                                csleep, sleepers, activeGen, wAlive, wActive, 
                                hostAlive, tAlive, snapshot, nextSnap, marker, 
                                resetWanted, crashes, kills, requested, 
                                holders, reqEpoch, respEpoch, fates, cellBy, 
                                handled, readySwept, repaired, s, e, pos, m, k, 
                                spins, wv, cw, result, voided, old, hl, d, wb, 
                                g, i >>

s_rq_fail == /\ pc[SUP] = "s_rq_fail"
             /\ IF i <= NSlots
                   THEN /\ IF slot[i].st = "READY"
                              THEN /\ fates' = [fates EXCEPT ![<<i, slot[i].e>>] = fates[<<i, slot[i].e>>] \cup {"failed"}]
                                   /\ slot' = [slot EXCEPT ![i] = [slot[i] EXCEPT !.st = "FAILED", !.w = FALSE, !.o = 0]]
                                   /\ sleepers' = [sleepers EXCEPT ![i] = {}]
                              ELSE /\ TRUE
                                   /\ UNCHANGED << slot, sleepers, fates >>
                        /\ i' = i + 1
                        /\ pc' = [pc EXCEPT ![SUP] = "s_rq_fail"]
                   ELSE /\ pc' = [pc EXCEPT ![SUP] = "s_rq_shut"]
                        /\ UNCHANGED << slot, sleepers, fates, i >>
             /\ UNCHANGED << cells, tail, head, cwait, gate, ctlDrain, 
                             drainAck, csleep, holds, activeGen, wAlive, 
                             wActive, hostAlive, tAlive, sock, snapshot, 
                             nextSnap, marker, supPhase, resetWanted, crashes, 
                             kills, requested, holders, reqEpoch, respEpoch, 
                             cellBy, handled, readySwept, repaired, 
                             sweptClosed, s, e, pos, m, k, spins, wv, cw, 
                             result, voided, old, hl, d, wb, g >>

s_rq_shut == /\ pc[SUP] = "s_rq_shut"
             /\ i' = 1
             /\ sweptClosed' = (sweptClosed \cup {"RESET"})
             /\ sock' = "half"
             /\ supPhase' = "await_close"
             /\ resetWanted' = FALSE
             /\ pc' = [pc EXCEPT ![SUP] = "s_loop"]
             /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                             drainAck, csleep, sleepers, holds, activeGen, 
                             wAlive, wActive, hostAlive, tAlive, snapshot, 
                             nextSnap, marker, crashes, kills, requested, 
                             holders, reqEpoch, respEpoch, fates, cellBy, 
                             handled, readySwept, repaired, s, e, pos, m, k, 
                             spins, wv, cw, result, voided, old, hl, d, wb, g >>

S == s_loop \/ s_ready \/ s_gate_late \/ s_repair \/ s_owned \/ s_successor
        \/ s_ctl_wake \/ s_reinit \/ s_reset_done \/ s_rq_fail \/ s_rq_shut

e_loop == /\ pc[ENV] = "e_loop"
          /\ \/ /\ crashes < MaxWorkerCrashes /\ wAlive[activeGen] /\ wActive[activeGen]
                /\ wAlive' = [wAlive EXCEPT ![activeGen] = FALSE]
                /\ crashes' = crashes + 1
                /\ csleep' = FALSE
                /\ holders' = [x \in Slots |-> holders[x] \ {W(activeGen)}]
                /\ UNCHANGED <<sleepers, hostAlive, tAlive, sock, resetWanted, kills, requested>>
             \/ /\ HostMayDie /\ hostAlive
                /\ hostAlive' = FALSE
                /\ IF sock \in {"open", "half"}
                      THEN /\ sock' = "closed"
                      ELSE /\ TRUE
                           /\ sock' = sock
                /\ holders' = [x \in Slots |-> holders[x] \ Threads]
                /\ sleepers' = [x \in Slots |-> {}]
                /\ UNCHANGED <<csleep, wAlive, tAlive, resetWanted, crashes, kills, requested>>
             \/ /\ kills < MaxKills /\ hostAlive
                /\ \E t \in {u \in Threads : tAlive[u]}:
                     /\ tAlive' = [tAlive EXCEPT ![t] = FALSE]
                     /\ holders' = [x \in Slots |-> holders[x] \ {t}]
                     /\ sleepers' = [x \in Slots |-> sleepers[x] \ {t}]
                /\ kills' = kills + 1
                /\ UNCHANGED <<csleep, wAlive, hostAlive, sock, resetWanted, crashes, requested>>
             \/ /\ requested < MaxRequestedResets /\ hostAlive /\ sock = "open" /\ ~resetWanted
                /\ resetWanted' = TRUE
                /\ requested' = requested + 1
                /\ UNCHANGED <<csleep, sleepers, wAlive, hostAlive, tAlive, sock, crashes, kills, holders>>
          /\ pc' = [pc EXCEPT ![ENV] = "e_loop"]
          /\ UNCHANGED << slot, cells, tail, head, cwait, gate, ctlDrain, 
                          drainAck, holds, activeGen, wActive, snapshot, 
                          nextSnap, marker, supPhase, reqEpoch, respEpoch, 
                          fates, cellBy, handled, readySwept, repaired, 
                          sweptClosed, s, e, pos, m, k, spins, wv, cw, result, 
                          voided, old, hl, d, wb, g, i >>

E == e_loop

Next == B \/ S \/ E
           \/ (\E self \in Threads: C(self))
           \/ (\E self \in Workers: Wk(self))

Spec == /\ Init /\ [][Next]_vars
        /\ \A self \in Threads : WF_vars(C(self))
        /\ WF_vars(B)
        /\ \A self \in Workers : WF_vars(Wk(self))
        /\ WF_vars(S)

\* END TRANSLATION

-------------------------------------------------------------------------------
(* Properties of testing §2.1, Spec A. *)

Waiting(t) == pc[t] \in {"c_spin", "c_spin_gate", "c_park", "c_fwait", "c_asleep"}
InArena(t) == Live(t) /\ pc[t] \in {"c_claim", "c_ready", "c_enq", "c_fill", "c_publish", "c_wake1",
                  "c_wake2", "c_wake3", "c_recheck", "c_spin", "c_spin_gate", "c_park", "c_fwait",
                  "c_asleep", "c_abandon", "c_read", "c_release", "c_failed", "c_full", "c_unwound"}
Terminal == {"done", "failed", "cancelled", "abandoned"}

\* A slot's data regions have at most one owner at any time.
OneOwner == \A x \in Slots : Cardinality(holders[x]) <= 1

\* OWNER is 1 or 2 in PROCESSING and ABANDONED, and 0 elsewhere (shm-abi §6.1).
OwnerMeaningful == \A x \in Slots :
    IF slot[x].st \in {"PROCESSING", "ABANDONED"} THEN slot[x].o \in {1, 2} ELSE slot[x].o = 0

\* No call resolves to more than one outcome: each (slot, epoch) ends one way.
OneOutcome == \A key \in DOMAIN fates : Cardinality(fates[key] \cap Terminal) <= 1

\* The gate word is what the holds say (supervisor §5.7).
GateFollowsHolds == gate = GateOf(holds, gate.gen)

\* No sleeper is stranded: a sleeper whose condition already holds has a wake-up pending.
\* Client: its slot is resolved and the resolver has yet to issue the wake. Consumer: work
\* is published, or a pause is requested, and a waker is between its publish and its wake
\* (a thread stopped there is covered by the heartbeat backstop, checked by liveness).
ConsumerNotStranded ==
    (hostAlive /\ csleep /\ wAlive[activeGen]
       /\ (cells[hl[W(activeGen)] % CAP].seq = hl[W(activeGen)] + 1 \/ (ctlDrain /\ ~drainAck[activeGen])))
    => \/ \E t \in Threads : pc[t] \in {"c_wake1", "c_wake2", "c_wake3"}
       \/ pc[SUP] = "s_ctl_wake"

\* After a worker crash, once the ring is repaired, it is consistent: every position in the
\* window is claimed or published, every other cell is free for its position. In particular
\* it cannot read as empty to its consumer and full to its producers.
RingConsistent ==
    /\ head <= tail /\ tail - head <= CAP
    /\ \A p \in head..(head + CAP - 1) :
         IF p < tail THEN cells[p % CAP].seq \in {p, p + 1} ELSE cells[p % CAP].seq = p
Unrepaired == ~wAlive[activeGen] /\ ~repaired[activeGen]
MidDequeue == wAlive[activeGen] /\ pc[W(activeGen)] = "w_stepB"
RingUsable == (~Unrepaired /\ ~MidDequeue) => RingConsistent

\* After a worker's exit and its sweep, it holds nothing.
OwnedSwept == \A gg \in GENS : handled[gg] =>
    \A x \in Slots : ~(slot[x].st \in {"PROCESSING", "ABANDONED"} /\ slot[x].o = OwnerOf(gg)
                       /\ ~(\E h \in GENS : h # gg /\ OwnerOf(h) = OwnerOf(gg) /\ wAlive[h] /\ activeGen = h))

\* A request published around a worker death is resolved by the client's gate re-check or by
\* the sweep. Stated as: a slot that is READY, whose client has passed its re-check and is
\* waiting, can still be reached by a consumer, by the sweep, or by a requested reset.
InRing(x, ep) == \E p \in head..(tail - 1) :
    cells[p % CAP].seq = p + 1 /\ cells[p % CAP].s = x /\ cells[p % CAP].e = ep /\ ~cells[p % CAP].void
FutureConsumer == wAlive[activeGen] \/ (activeGen + 1 \in GENS)
HeldByConsumer(x, ep) == /\ wAlive[activeGen]
                         /\ pc[W(activeGen)] \in {"w_stepA", "w_stepB", "w_claim"}
                         /\ d[W(activeGen)].s = x /\ d[W(activeGen)].e = ep
SweepPending(x) == \/ ~wAlive[activeGen] /\ ~readySwept[activeGen] /\ ~(pc[SUP] = "s_ready" /\ i > x)
                   \/ pc[SUP] = "s_rq_fail" /\ i <= x
Reachable(x, ep) == (InRing(x, ep) /\ FutureConsumer) \/ HeldByConsumer(x, ep) \/ SweepPending(x)
NoOrphan == \A t \in Threads :
    (Live(t) /\ Waiting(t) /\ slot[s[t]].st = "READY" /\ slot[s[t]].e = e[t]) => Reachable(s[t], e[t])

\* The stronger promise: once the gate is closed and its sweep has run, no live client is still
\* waiting on a READY slot. A request published around the closure was cancelled by the
\* client's re-check or failed by the sweep, and does not wait for whatever comes next (for a
\* cold restart, longer than its deadline). shm-abi §6.3: "every call in flight at a worker
\* death resolves within microseconds of the gate flipping".
NoLateQueued == sweptClosed # {} =>
    \A t \in Threads : ~(Live(t) /\ Waiting(t) /\ slot[s[t]].st = "READY" /\ slot[s[t]].e = e[t])

\* (placed here because it uses SweepPending)
ClientNotStranded == \A x \in Slots : \A t \in sleepers[x] :
    slot[x].st \in {"DONE", "FAILED"} =>
        \/ \E w \in Workers : WLive(w) /\ pc[w] = "w_wake" /\ d[w].s = x
        \/ SweepPending(x)                              \* the sweep has yet to reach it

\* A session reset never re-initializes the arena while a client thread is inside it.
NoResetUnderClient == pc[SUP] = "s_reinit" => \A t \in Threads : ~InArena(t)

\* After a reset, every slot is FREE and the ring is empty and consistent.
ResetClean == pc[SUP] = "s_reset_done" =>
    /\ \A x \in Slots : slot[x].st = "FREE" /\ slot[x].o = 0 /\ ~slot[x].w
    /\ head = 0 /\ tail = 0 /\ cells = Fresh /\ cwait = "AWAKE"

\* ── liveness ──
\* Every call resolves, provided its own thread keeps running.
CallsResolve == \A t \in Threads : (pc[t] = "c_claim") ~> (pc[t] = "c_exit" \/ ~Live(t))
\* A producer that stops for ever after claiming a cell does not stall its ring for ever.
StuckHeadClears == HeadStuck ~> ~HeadStuck
\* Every abandoned slot returns to FREE (the worker frees it, or the sweep does).
AbandonedFreed == \A x \in Slots : (slot[x].st = "ABANDONED") ~> (slot[x].st # "ABANDONED")
\* A sleeping consumer with work is woken (the backstop covers a waker that stopped).
ConsumerWoken == (csleep /\ wAlive[activeGen] /\ cells[head % CAP].seq = head + 1) ~> (~csleep \/ ~wAlive[activeGen])

\* ── state fingerprint for safety runs ──
\* A worker that has died never runs again, nor does any thread of a host that has died, and
\* no property reads their program counters or locals (each one is guarded by liveness). So
\* their frozen state is put in one canonical form: otherwise every crash point would leave a
\* distinct dead state behind. Likewise how an old epoch of a slot ended can no longer change,
\* and the claimer of a cell outside the ring's window is never read.
DeadP(p) == \/ p \in Workers /\ ~wAlive[GenOf(p)]
            \/ p \in Threads \cup {BGT} /\ ~hostAlive
View == <<slot, cells, tail, head, cwait, gate, ctlDrain, drainAck, csleep, sleepers, holds,
          activeGen, wAlive, wActive, hostAlive, tAlive, sock, snapshot, nextSnap, marker,
          supPhase, resetWanted, crashes, kills, requested, holders, reqEpoch, respEpoch,
          [key \in DOMAIN fates |-> IF key[2] < slot[key[1]].e THEN {} ELSE fates[key]],
          [j \in DOMAIN cellBy |-> IF \E p \in head..(tail - 1) : p % CAP = j THEN cellBy[j] ELSE 0],
          handled, readySwept, repaired, sweptClosed,
          [p \in DOMAIN pc |-> IF DeadP(p) THEN "dead" ELSE pc[p]],
          [t \in Threads |-> IF DeadP(t) THEN <<>> ELSE <<s[t], e[t], pos[t], m[t], k[t], spins[t], wv[t], cw[t], voided[t]>>],
          IF ~hostAlive THEN 0 ELSE old,
          [w \in Workers |-> IF DeadP(w) THEN <<>> ELSE <<hl[w], d[w], wb[w]>>],
          g, i>>
===============================================================================
