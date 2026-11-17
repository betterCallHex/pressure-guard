# pressure-guard

## The idea

As many coding agents as the user wants, in parallel, on one 16 GB MacBook Air: the Mac never freezes, never stalls from heat, and answers every click at once. Unlimited agents, zero freezes, no new hardware. It sounds impossible; every change moves toward it.

## The contract

Every agent runs every command it chooses, as many as it chooses, for as long as it chooses. pressure-guard reshapes *how* the Mac serves that work (priority, cores, whose memory is swapped first), never *whether* it runs. Measure each idea against the contract: an idea that has agents do less, wait, skip or stop, or that adds hardware, breaks it. Keep looking for the version that doesn't.

## The insight

Agents are latency-tolerant: they wait seconds on the model, so a slower command costs them little. The user is not: a 200 ms lag on a click is felt. Spend agent latency to buy user responsiveness. Agent work **yields**:

- always: `nice 10`, inherited by everything an agent starts, detached children included;
- under memory or heat pressure: Darwin background mode, so macOS's background page queue (`vm.vm_page_background_mode`) swaps agent memory before the user's, and agent CPU work moves to the efficiency cores. A calm minute restores full speed.

Find agent work from the live system (agent process trees, plus the agent nice level children inherit), never from saved state, so a crash or restart leaves nothing stuck.

## Verifying

`cargo test`, then watch the real Mac: the log (`/opt/homebrew/var/log/pressure-guard.log`), `vm.vm_page_background_count`, load average. Tests started from an agent session inherit its nice level; write process tests relative to the test's own nice.

## Agent skills

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
