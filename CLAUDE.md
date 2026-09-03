# Development Guide
Browse the codebase except to figure out what is going on.
Respect `.gitignore` and do not observe files mentioned inside.


## Coding Principles
- CRITICAL: Code goes into financial system with multi-million turnover. Error = huge losses. Consider all edge cases, race conditions, security. Write production-grade code.
- Always strive for concise, simple solution.
- If a problem can be solved in a simpler way, propose it.
- If asked to do too much work at once, stop and state that clearly.
- If you suggest removing/avoiding something, always provide an alternative.
- Follow existing conventions in the codebase instead of inventing new patterns.
- Before implementing, state your assumptions and ask if unclear.
- If you don't know something, say so. Don't guess or hallucinate.
- Prefer editing existing code over creating new files.
- Don't refactor code that isn't related to the current task.
- If a task requires changes in 5+ files, confirm scope first.