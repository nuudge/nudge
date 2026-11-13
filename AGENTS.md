# Agent Instructions

Instructions for AI agents (and humans) working in this repo.

## Think before coding

- State your assumptions. If a request has several readings, say which you picked, or ask.
- If a simpler approach exists, say so and push back.
- If something is unclear, stop and name what is confusing.

## Simplicity

Write the least code that solves the problem.

- No features, options, or abstractions that weren't asked for. Three similar lines beat a premature helper.
- No error handling for cases that can't happen. Validate at boundaries (user input, network, parsed data) and trust internal code and the type system.
- No backwards-compat shims or dead code kept "just in case". Git history is the record.
- If 200 lines could be 50, rewrite it.

## Surgical changes

- Touch only what the task needs. Every changed line should trace back to the request.
- Don't refactor, reformat, or "improve" adjacent code. Mention unrelated problems instead of fixing them.
- Remove imports, variables, and functions your change made unused. Leave pre-existing dead code unless asked.

## Comments

Default to no comment. Well-named code says what it does.

- Write a comment only for a non-obvious *why*: a hidden constraint, a subtle invariant, a workaround, behavior that would surprise the next reader.
- Keep it to one or two lines. If it needs a paragraph, the code or the design probably needs fixing instead.
- Never restate the code, narrate history ("previously", "now"), or reference the task, issue, or caller that motivated it. That belongs in the commit message.
- Older code in this repo is commented more heavily than this. Don't copy that density.

## Docs

- Write like a human expert. Cut hedging, filler, and prose that restates the code.
- Describe the current state of the system. Leave out how it was built, what was tried, and future work; link an issue instead.

## Verify

Turn the task into a check you can run, then loop until it passes: a bug fix gets a test that reproduces it, a refactor keeps the tests green before and after.

## Commits and PRs

- Prefix the subject: `feat:`, `fix:`, `refactor:`, `doc:`, `chore:`.
- Say what changed and why. Don't report that tests, fmt, or clippy pass, and don't add a test plan made of CI checks. Mention only manual verification CI can't do (e.g. on a real device or network).
