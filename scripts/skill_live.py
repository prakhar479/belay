#!/usr/bin/env python3
"""Live check: does an agent use belay on its own, without being asked? (agent_skill)

    scripts/skill_live.py                       # target/debug/belay, Haiku, two sessions
    scripts/skill_live.py --model sonnet --keep

Two plain `claude` sessions in a throwaway repo, through belay's plugin (the installed
one, else `--plugin-dir`), with belay's permission rules allowed. The prompts never
mention belay. Session 1 fixes a bug; session 2 continues the work. For each session
it scores the routine from the agent's tool calls:

    start:   read the last handoff (handoff_get, or `belay handoff` with no text)
    card:    said what it works on (`belay card --task`)
    brief:   ran `brief` before it finished
    handoff: left a handoff (`belay handoff "…"`) before it stopped

Session 2 also scores whether its first steps used session 1's handoff. Costs two short
sessions (about $0.05 on Haiku). Exit code 0 when every session ran; the score is a
report, not a gate, because a model may skip a step on a task this small.
"""

import argparse
import re
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from conformance import jsonl, plugin_installed, tool_results  # noqa: E402

TASK_1 = "test_calc.py fails. Fix it."
TASK_2 = "Carry on with the calculator work in this repo: add a `mul(a, b)` function with a test."

# belay's PERMISSION_RULES (crates/belay-cli/src/install.rs), plus what the task needs.
ALLOWED = ",".join([
    "Bash(belay handoff:*)", "Bash(belay memory search:*)", "Bash(belay memory list:*)",
    "Bash(belay memory add:*)", "Bash(belay ask:*)", "Bash(belay share:*)",
    "Bash(belay card:*)", "Bash(belay sessions:*)", "Bash(belay msg:*)", "Bash(belay inbox:*)",
    "Bash(belay brief:*)", "Bash(belay show:*)", "Bash(belay why:*)",
    "mcp__plugin_belay_belay__memory_search", "mcp__plugin_belay_belay__memory_add",
    "mcp__plugin_belay_belay__handoff_get", "mcp__plugin_belay_belay__output_show",
    "mcp__plugin_belay_belay__brief",
    "Edit", "Write", "Read", "Bash(cd:*)", "Bash(ls:*)", "Bash(cat:*)",
    "Bash(python3 -m pytest:*)", "Bash(python -m pytest:*)", "Bash(pytest:*)", "Bash(git status:*)", "Bash(git diff:*)",
])


def repo(d: Path) -> None:
    d.mkdir(parents=True)
    (d / "calc.py").write_text("def add(a, b):\n    return a - b\n")
    (d / "test_calc.py").write_text("from calc import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n")
    (d / ".gitignore").write_text("__pycache__/\n.pytest_cache/\n")
    git = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]
    subprocess.run(["git", "init", "-q"], cwd=d, check=True)
    subprocess.run(["git", "add", "."], cwd=d, check=True)
    subprocess.run([*git, "commit", "-qm", "init"], cwd=d, check=True)


def calls(transcript: list) -> list:
    """(name, command or '', result) for each tool call, in order."""
    seen = tool_results(transcript)
    out = []
    for d in transcript:
        for b in (d.get("message") or {}).get("content") or []:
            if isinstance(b, dict) and b.get("type") == "tool_use":
                cmd = (b.get("input") or {}).get("command", "")
                out.append((b.get("name", ""), cmd, seen.get(b.get("id"), "")))
    return out


def score(steps: list) -> dict:
    def shell(prefix, text_arg=None):
        for name, cmd, _ in steps:
            # Each command of a line (`a && b; c | d`), without redirections like `2>&1`.
            for part in re.split(r"&&|\|\||;|\|", cmd):
                words = [w for w in part.strip().split() if not re.match(r"^\d*[<>]", w)]
                if name == "Bash" and " ".join(words[:len(prefix)]) == " ".join(prefix):
                    if text_arg is None or (len(words) > len(prefix)) == text_arg:
                        return True
        return False

    def tool(t):
        return any(name == f"mcp__plugin_belay_belay__{t}" for name, _, _ in steps)

    return {
        "start": tool("handoff_get") or shell(["belay", "handoff"], text_arg=False),
        "card": shell(["belay", "card"]),
        "brief": tool("brief") or shell(["belay", "brief"]),
        "handoff": shell(["belay", "handoff"], text_arg=True),
    }


def session(proj: Path, home: Path, prompt: str, args, env: dict) -> list:
    cmd = [args.claude, "-p", prompt, "--model", args.model, "--allowedTools", ALLOWED]
    if not plugin_installed(args.claude):
        cmd += ["--plugin-dir", str(ROOT / "plugin")]
    before = set((home / "sessions").glob("*")) if (home / "sessions").exists() else set()
    subprocess.run(cmd, cwd=proj, env=env, capture_output=True, text=True, timeout=args.timeout)
    new = sorted(set((home / "sessions").glob("*")) - before)
    if not new:
        return []
    ledger = [e["body"] for e in jsonl(new[-1] / "ledger.jsonl") if "body" in e]
    path = next((e["event"].get("transcript_path") for e in ledger
                 if e.get("kind") == "event" and e["event"].get("type") == "session_started"), None)
    return calls(jsonl(Path(path))) if path else []


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--belay", default=str(ROOT / "target/debug/belay"))
    ap.add_argument("--claude", default="claude")
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--timeout", type=int, default=400)
    ap.add_argument("--keep", action="store_true", help="keep the work dir")
    args = ap.parse_args()

    work = Path(tempfile.mkdtemp(prefix="belay-skill-live-"))
    proj, home = work / "calc", work / "home"
    repo(proj)
    (work / "config.toml").write_text("notify = off\n")
    env = dict(os.environ, BELAY_HOME=str(home), BELAY_CONFIG=str(work / "config.toml"))
    env["PATH"] = str(Path(args.belay).resolve().parent) + os.pathsep + env.get("PATH", "")
    for k in ("BELAY_SOCKET", "BELAY_SESSION_DIR", "BELAY_PLUGIN"):
        env.pop(k, None)

    rows, ran = [], True
    for n, prompt in ((1, TASK_1), (2, TASK_2)):
        steps = session(proj, home, prompt, args, env)
        ran = ran and bool(steps)
        s = score(steps)
        if n == 2:
            # It read session 1's handoff, the agent's own or the one belay writes from
            # the session's facts: either names the file session 1 changed.
            s["saw handoff"] = any(
                (name.endswith("handoff_get") or "belay handoff" in cmd)
                and "calc.py" in result
                for name, cmd, result in steps)
        rows.append((n, s, [f"{name} {cmd}".strip()[:70] for name, cmd, _ in steps]))

    for n, s, steps in rows:
        got = sum(s.values())
        print(f"session {n}: {got}/{len(s)}  " + "  ".join(f"{k} {'✓' if v else '✗'}" for k, v in s.items()))
        for line in steps:
            print(f"    {line}")
    if args.keep or not ran:
        print(f"work dir: {work}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    return 0 if ran else 1


if __name__ == "__main__":
    sys.exit(main())
