kind: changed
- Verify before done also covers a session that changed source files and never ran a test or check. belay asks the agent once per session to run the project's tests, or to say why they can't run. Before, it stayed quiet when it knew no test command.
- A new session no longer takes the name of the session that ended last in its repo. The handoff it reads at start names that session, so the two read as one.
