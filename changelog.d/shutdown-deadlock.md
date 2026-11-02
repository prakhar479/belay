kind: fixed

- **A session could hang on exit and lose its handoff.** When belay wrote the session card at the same moment the session ended (you quit Claude Code, the agent stopped, or an idle session timed out), the two could wait on each other forever. The next session then saw the old one as still running and got no handoff. Every exit path now takes the locks in one order.
