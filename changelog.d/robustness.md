kind: fixed
- Notifications to `notify_command` and the desktop no longer carry secrets that appeared in agent text.
- A failed ledger write no longer swallows the next entry, and failed blob writes leave no temp files.
- Without HOME, belay uses a per-user directory in the temp dir, not one shared path.
- A global flag before the verb (`belay --flag allow`) no longer hides user-only commands from the agent denial.
- The install script rejects a BELAY_VERSION with odd characters.
