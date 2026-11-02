kind: fixed

- **The test guard no longer flags removing a duplicate test.** Deleting one of two identical copies of a test (a merge often leaves one) read as "tests removed". A copy that stays still runs, so nothing is taken away. Removing the last copy is still flagged.
