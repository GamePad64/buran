# ADR 0007: Session transcript storage — append-only JSONL is the canon; SQLite only ever a derived index

- Status: accepted
- Date: 2026-11-12
- Deciders: buran owner (Alexander), via design discussion

## Context

Spec open question 4 (session transcript format) deferred storage wiring to
v1.5 while `session.rs` already pins the shape: one `TranscriptRecord` per
line, canonical JSONL. Before v1.5 work starts (resume/fork, compaction
entries per ADR 0006), the storage engine question needed settling:
SQLite was the considered alternative, and the ASR open format
([agentsessionrecord/asr](https://github.com/agentsessionrecord/asr),
format 0.1) the considered interop target.

## Decision

- **Canonical storage**: one UTF-8 JSONL file per session, one
  `TranscriptRecord` per line, strictly append-only. Nothing rewrites
  earlier lines — compaction appends `CompactionPoint` records (ADR 0006),
  forks append entries. Append-only is session *semantics*, not a format
  workaround: it is what makes crash tolerance free (a torn final line
  costs that line, not the file) and standard-tool inspection free
  (`tail`, `jq`, `grep` work without buran's code).
- **SQLite policy**: never the canon. A derived SQLite index for the
  session *catalog* (list/resume/fork/search across many sessions) is
  permitted later, but it must be rebuildable from the JSONL files at any
  time — deleting it must lose nothing. The agent's access pattern
  (append an event, replay in order) is a log-file profile; SQL's
  concurrency and in-place updates buy nothing here and cost WAL
  artifacts, migrations, and opacity.
- **ASR stance**: watched, not adopted. ASR positions itself as an export
  format, not a native one — which matches this ADR exactly. Buran's
  native log stays `TranscriptRecord` JSONL; an ASR exporter, if that
  format matures, is external tooling, never harness code.

## Consequences

- The v1.5 sessions store starts file-first: a sessions directory with one
  `.jsonl` per session; any catalog/index is an optimization added only
  when a real query need appears.
- Cross-tool interop stays a converter problem (field mapping), which the
  one-record-type-per-line shape keeps cheap.
- No schema migrations: `TranscriptRecord` evolves additively (serde
  internally-tagged enum); readers should skip unrecognized record types
  for forward compatibility.
