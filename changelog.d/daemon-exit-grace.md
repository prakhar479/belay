kind: fixed
- A belay daemon that decides to exit (its agent is gone, or it was idle too long) now exits within 10 seconds even if a thread is stuck, so no daemon outlives its session.
