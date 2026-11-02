kind: added

- **belay sees reads.** The plugin and `belay claude` also hook Read, Grep, and Glob after they run, and `SubagentStop`. Each read is recorded as an output (`belay show o-N`), never changed. `belay report` adds `Reads: N (R repeated)`: a repeat is the same content of the same file read again with no compaction in between. Read outputs no longer count as Bash output. `belay retro` sees reads in transcripts too, so its "Tool calls" count goes up. New ledger event `subagent_ended` (additive; older belay versions skip it).
