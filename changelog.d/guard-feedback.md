kind: added

- **Guard feedback:** when you `belay allow` blocks of the same guard rule twice in a session, that rule only warns for the rest of the session, and `belay allow` says so. `belay resume guard` makes it deny again. Recorded as `feature_changed` on `guard:<rule>`.
