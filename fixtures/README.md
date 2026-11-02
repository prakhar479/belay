# Fixtures

| Path | Content | Status |
|---|---|---|
| `outputs/pytest_fail.txt` | pytest -v: 300 passed, 2 failed | Captured 2026-10-19 (pytest 8.3) |
| `outputs/cargo_fail.txt` | cargo test: 148 passed, 2 failed (one `assert_eq!`, one `panic!`) | Captured 2026-10-19 (Rust 1.95) |
| `outputs/pytest_cut_by_claude.txt` | A failing pytest run as Claude Code cut it (`... [N characters truncated] ...`) | Captured 2026-10-19 |
| `outputs/go_test_fail.txt`, `go_test_v_fail.txt` | go test, plain and `-v`: 3 failures (one subtest, one panic) | Captured 2026-10-19 (go 1.22) |
| `outputs/jest_fail.txt` | jest 29 `--verbose`: 3 failed, 30 passed | Captured 2026-10-19 |
| `outputs/vitest_fail.txt` | vitest 2.1 `--reporter=verbose`: 3 failed, 30 passed | Captured 2026-10-19 |
| `outputs/repeated_traces.txt` | Python worker: the same traceback 50 times | Captured 2026-10-19 |
| `claude_code/*.json` | Claude Code hook payloads | Captured 2026-10-19, Claude Code 2.1.303 (docs/dev/TASKS.md S1) |
| `claude_code/pre_tool_use_bash_auto.json` | PreToolUse Bash in auto mode (`python3 -m pytest -v tests`) | Captured 2026-10-20, Claude Code 2.1.304 (conformance run) |
| `claude_code/post_tool_use_failure_wrapped.json` | PostToolUseFailure of a test run belay wrapped: the error text is belay's summary | Captured 2026-10-20, Claude Code 2.1.304, when the tool was named tend; the binary path replaced with `/usr/local/bin/belay`, and `tend` in the summary marker changed to `belay` |
| `claude_code/permission_denied_bash.json` | PermissionDenied for a wrapped Bash call | **Synthetic**, from the 2.1.303 hook schema; replace with a capture when a denial happens (R3.7) |
| `claude_code/session_start_compact.json` | SessionStart after a compaction (`source: "compact"`, the same session id) | **Synthetic**, from the hook docs; [Verify] with a capture (U4b) |
| `claude_code/post_tool_use_read.json` | PostToolUse Read: `tool_response` `{type, file: {filePath, content, …}}` | **Synthetic**: the shape of a 2.1.316 transcript's `toolUseResult` for Read; [Verify] with a capture (U5.1, conformance) |
| `claude_code/post_tool_use_grep.json`, `post_tool_use_grep_content.json`, `post_tool_use_glob.json` | PostToolUse Grep (`files_with_matches`, `content`) and Glob | **Synthetic**, from the hook docs; [Verify] with a capture (U5.1) |
| `claude_code/subagent_stop.json` | SubagentStop with `agent_type` | **Synthetic**, from the hook docs; [Verify] with a capture (U5.1) |
| `outputs/pytest_grep_e_lines.txt`, `outputs/pytest_summary_only.txt`, `outputs/go_test_grep_n.txt` | Output as an agent trims it (`grep -E`, `tail -1`, `grep -n`) | **Hand-made** from the captured runs above |
| `ledgers/v1_look_session.jsonl` | A 0.1 ledger (format 1): a look-mode session with a shape, a guard flag, and a mode change | **Hand-made** in the 0.1 format (U2a), to keep old ledgers readable |
| `transcripts/loop_session.jsonl` | Claude Code transcript: 4 failing runs, 3 edits | Captured 2026-10-19 (docs/dev/TASKS.md S5). Only user/assistant lines and the fields belay reads |
| `transcripts/reads_session.jsonl` | Claude Code transcript: the same file read twice, a Grep, a Glob, a WebFetch, with `toolUseResult` | **Hand-made** (U5.1) in the shape of 2.1.316 transcripts |
| `guard/real/*.json` | Guard precision corpus: one case per file (`steps` of edits, shell writes, and runs in one turn; `expect` `ok` or `flag`; `rule`). New-file, outside-project, conditional-skip, justified-tolerance, focus, and move patterns, plus true positives | **Synthetic**, minimal, from the patterns of a real project's false alarms (P0.1, 2026-11-02); no project code. A new case also goes in the list in `controller/guard/tests.rs` |
| `sessions/mcp/` | A session dir: ledger with one test output `o-1` (and its blob) and the shape action `a-1` | **Hand-made** in ledger format 1, for `belay mcp` |
| `keepawake/powercfg_qh_lidaction_wsl.txt`, `powercfg_q_lidaction_hidden.txt` | `powercfg /qh` and `/q SCHEME_CURRENT SUB_BUTTONS LIDACTION` from WSL2 on a laptop: `/q` leaves the hidden lid setting out | Captured 2026-11-02 (Windows, powercfg via WSL interop) |
| `keepawake/systemd_inhibit_no_bus.txt` | `systemd-inhibit` with no logind (WSL without systemd) | Captured 2026-11-02 (systemd 255) |
| `keepawake/systemd_inhibit_polkit_denied.txt`, `pmset_batt_*.txt` | polkit refusing an inhibitor; `pmset -g batt` on battery and on AC | **Hand-made**; [Verify] with captures on a logind desktop and a Mac |

Rules:

- Every parser change needs a fixture here and a test.
- Captured payloads come from `<session>/hooks.jsonl` (run the session with `BELAY_CAPTURE=1`). Make fixtures with `scripts/capture_fixtures.py`; it removes personal paths and ids and refuses output that still has `$HOME` or `$USER`. Read the result before you commit it.
- In captures, paths are `/home/dev/shop`, the session id is `9b1d6f2e-0000-4000-8000-000000000001`, and tool-use ids are `toolu_0N`.
| `skills/<rule>/` | A skill per risk rule (pipe-to-shell, base64-blob, executable, disables-guard, ignore-instructions, hidden-unicode) and a clean one | Written by hand 2026-11-01 (U9) |
