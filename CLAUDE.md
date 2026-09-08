# ankimarkable — repository rules

## Commit messages (STRICT)

Commits in this repository are authored by the human maintainer only. **Never**
add any of the following to a commit message, tag message, PR description, or
release note:

- `Co-Authored-By:` trailers of any kind
- `Claude-Session:` / session-URL trailers
- "Generated with Claude Code", "🤖", or any other reference to Claude,
  Anthropic, or an AI assistant

This is enforced three ways:

1. `.claude/settings.json` disables Claude Code's commit/PR attribution.
2. `.githooks/commit-msg` rejects any offending message. Enable it once per
   clone with `git config core.hooksPath .githooks`.
3. This file. If a tool insists on adding attribution, strip it before
   committing — do not commit and amend later.

Author and committer must be the maintainer's own name and email.
