# convertx-rust – working rules

Project overview, architecture, build commands and conventions: see [STATUS.md](STATUS.md)
(especially §7). Answer the user in German; code, comments and STATUS.md are English.

## Keep STATUS.md current (always)

`STATUS.md` is the hand-over document for other contributors. After **every** change to code,
tests, tools, settings, frontend or build setup, update it in the same piece of work, before
replying to the user:

- §3 module tables: line count, status (✅ verified / 🟡 implemented but untested / ⛔ missing),
  completed features and "Pending" of every touched file; add rows for new files.
- §5.1: test count and result after every test run (exact numbers from the run).
- §5.2/§5.3: new manual verifications, or things that are still untested.
- §6: roadmap items done, new ones found, new limitations/caveats.
- §2/§4 when architecture, tools or crates change.
- "State as of" date at the top.
- §8 Changelog: one new entry at the top – date, what changed, files, how it was verified.

Only mark something ✅ if it was actually run or tested. Pure questions/explanations without a
change need no update.
