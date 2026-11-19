# buran vs. the field: resource benchmarks

**Maintained, periodically refreshed comparison of buran against TS-based
coding agents (pi, Claude Code, opencode) on memory footprint, binary size,
install footprint, and startup latency.** This document exists because
buran's differentiator is *lightness* — and lightness claims decay fast when
the comparison row goes stale. Update the table when a new buran release
ships or a new competitor version is measured; keep the measurement method
section honest as tools change.

## Method (how the numbers are taken)

All measurements on one development machine (Linux x86_64, wayland, kernel
7.0.0-43), buran column refreshed 2026-11-14 on the fat-LTO + strip
release profile (ADR 0008 budgets gate this column). Where an LLM
roundtrip is involved, all agents talk to the **same LiteLLM-style
gateway** (`http://172.21.2.30:4000`, model `chat-router`), one-shot
prompt "Reply with exactly one word: ping", so latency differences are
agent-side only.

- **RSS** = `VmRSS` from `/proc/<pid>/status`, sampled by polling every
  30–50 ms during the measured window (peak), or read once after settle
  (idle).
- **One-shot wall time** = process spawn → final output printed, warm cache.
- **buran serve** driven over ACP stdio by a minimal JSON-RPC client; a
  session is `initialize` + `session/new` (+ one `session/prompt` where
  noted).
- **pi** = interactive session resident ~90 min (the owner's running
  instance); one-shot = `pi --print --provider litellm --model chat-router`
  with a temporary `PI_HOME`, node child RSS peak-sampled via
  `pgrep -f node_modules/.bin/pi`. First runs include LiteLLM route
  discovery; warm runs do not.
- **Claude Code** = the running v2.1.310 instance on this machine (its
  `bg-spare`/`bg-pty-host`/`daemon` fleet), plus install footprint on disk.
- opencode: not installed on this machine; row kept with "not measured"
  rather than numbers from someone else's blog.

## The table

| Metric | buran 0.1.0 (fat LTO + strip, HTTP listener) | pi 1.5.2 | Claude Code 2.1.310 | opencode |
|---|---|---|---|---|
| Binary / install size | **9.8 MB** buran + **3.9 MB** buran-tui (two stripped ELFs; axum stack for `--listen` inside the buran number, ADR 0010) | 152 MB per release (314 MB with old release kept) | 279 MB install + 71 MB plugins = **350 MB** | not measured |
| Idle process RSS | **~6.2 MB** (serve, waiting) | — | — | not measured |
| One process, one active session (prompt-turn peak) | **9.5–9.6 MB** (was 8.8–9.0 pre-axum; budget 10 MB — headroom ~0.4 MB, watched) | ~104–148 MB (node runtime floor ~45 MB) | ~100–400 MB per process | not measured |
| Session with a live MCP server (harness + tool child) | harness **8.9 MB** peak with 1 server connected, **9.3 MB** with 2; each `mcp-server-filesystem` child ≈ **85 MB** node RSS — the harness stays light, the tools don't (measured 2026-11-18, local npx-cached server, no network) | tools are separate node processes, same ~50–100 MB each | same shape: every tool server is a node child | not measured |
| Subagent delegation (v2) | parent harness unchanged (the subagents server runs in-process); each child ≈ the session row above (~9 MB + inherited tool children) — delegation multiplies the session floor, not a new cost | — | — | — |
| Hooks (v2.1) | zero resident cost: a hook is a process spawned per event (≈1–5 ms `sh -c` each) and killed after; with no `[hooks]` section the runner never runs at all | — | — | — |
| Interactive fleet (this machine, just now) | n/a (one process per session by design; N HTTP sessions share one process) | 148 MB (one TUI, 90 min) | **1 754 MB** total: 11 `bg-spare` processes 77–405 MB each + daemon 99 MB + `agents` 147 MB | not measured |
| Cold start (spawn → accepts first RPC) | **p50 4 ms** (10 runs, min 2, max 7) | ~0.4 s to config-loaded failure; ~1 s+ warm one-shot end-to-end | seconds (node + 350 MB tree) | not measured |
| One-shot prompt roundtrip (same gateway) | **18–25 ms** agent-side overhead (wall 0.4–1.7 s incl. LLM — gateway varies) | 0.5–3.4 s wall (LiteLLM discovery on cold runs; warm ~0.5–1.4 s) | not comparable headless (bg-spare fleet is the resident cost) | not measured |
| Runtime dependencies | **none** (statically linked musl/gnu targets; node not needed) | node 26 (bundled via pi-node), npm tree | node (bundled), npm tree | node/bun |
| Outbound connections by design | **1 — the LLM provider you configured** (nothing else; logs stay local) | none beyond the provider | **4 telemetry channels on by default**: Anthropic events, OpenTelemetry, two Datadog endpoints; each event carries email, device ID, hashed git remote, HEAD SHA; persistent offline queue; background agents (`autoDream` grep past transcripts daily, `awaySummary` while unfocused) — see [habr teardown of v2.1.294](https://habr.com/ru/articles/1109634/); all of it is disable-able via env vars, off-by-default is not the default | provider |
| Uninstall = | `rm` two files | `rm -rf ~/.pi` (314 MB) | `rm -rf` 350 MB + plugin cache | — |

## Honest caveats

- pi's ~104 MB one-shot peak is node's floor (~45 MB bare) plus pi's TS
  agent; its **interactive** instance held 148 MB over 90 minutes. pi is
  the *leanest* of the TS field and buran's honest peer — the comparison
  gap to pi is real but smaller than the gap to Claude Code.
- Claude Code's `bg-spare` fleet (11 processes, 1.75 GB) is this machine's
  specific state (background jobs enabled), not a universal constant; a
  bare interactive Claude Code is nearer 100–200 MB. The install footprint
  (350 MB on disk) is the stable number. The 2026-11-19
  [teardown of v2.1.294](https://habr.com/ru/articles/1109634/) explains
  where the fleet comes from: hidden background agents (`autoDream`
  memory consolidation over past transcripts, `awaySummary` for unfocused
  windows) — a design choice, not machine noise.
- buran's "one process per session" is by design (AGENTS.md FIXED): the
  fleet row compares *equivalent work*, not a bug.
- buran one-shot agent-side overhead (16–34 ms) excludes the LLM; wall
  time for the same prompt on the same gateway was ~1.4 s for both buran
  and pi — the difference is the ~30× smaller agent-side share.
- Session-with-tools numbers measured 2026-11-18 (see table): the
  harness row stays honest — buran itself barely moves (8.9 → 9.3 MB
  adding a second server, catalog bookkeeping only) — but each
  node-based MCP child carries its own ~85 MB. Lightness of the harness
  does not lightness-apply to the tools it spawns.

## What this document is for

Every buran release: re-run the buran column (script below), spot-check one
competitor column if its version moved, update the date. If buran's numbers
regress past the resource budgets (ADR 0008), that's a release blocker —
the whole point of the table is that the differentiator stays true.

Budget line, from ADR 0008: session RSS ≤ 10 MB, cold start ≤ 100 ms,
stripped binary ≤ 15 MB. Current: 9.5–9.6 MB RSS / 4 ms / **9.8 MB binary**
(compaction re-fit LTO sections; binary went 10.3 → 9.8 while gaining the
feature) — green. RSS headroom is still ~0.4 MB (fat LTO took the peak
6.8 → 8.8 MB; the axum stack for `--listen` added ~0.7 more). The next
feature with resident cost amends ADR 0008 first.

## Reproducing

```bash
# buran: cold start p50 (10 spawns)
node cold-start.mjs          # ACP client; see docs/benchmarks method
# buran: one-shot prompt wall + agent-side overhead
BURAN_API_KEY=... node buran-oneshot.mjs
# pi: one-shot against the same gateway (needs http loopback forward if
# the gateway is remote — pi refuses non-loopback plain http)
socat TCP-LISTEN:4000,bind=127.0.0.1,fork,reuseaddr TCP:<gw>:4000 &
HOME=<pi-home> pi --print --provider litellm --model <model> "ping"
# RSS: poll /proc/<pid>/status VmRSS at 30–50 ms; sample the node child
# (pgrep -f 'node_modules/.bin/pi'), not the shell wrapper
```
