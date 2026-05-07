---
name: pr-comments
description: Procedural reference for addressing GitHub PR review comments end-to-end — discover open review threads, triage each one, implement accepted changes, push, reply in-thread with the required AI-agent disclaimer, and resolve only the threads where code actually changed. Invoke when the user asks to "address PR comments", "respond to review", "handle PR feedback", or names a specific PR/comment to act on. Covers exact `gh api` / `gh api graphql` invocations for listing threads, posting replies, and resolving threads, plus the rules for what *not* to auto-resolve. The load-bearing *principles* (always disclose AI authorship, never resolve threads the agent didn't act on, never push to `main`, never force-push a branch with commits from multiple contributors, never `--no-verify`) are restated here so the skill is self-contained.
license: MIT
---

# Address PR review comments — procedural reference

This skill walks the agent through addressing review feedback on a GitHub pull request: discovering open threads, deciding what to do with each, making code changes for the ones it accepts, replying in-thread with the required AI-agent disclaimer, and resolving only the threads where code actually changed.

It is intentionally self-contained — every command, query, and rule the agent needs is in this file. There is no separate "principles" doc.

## 0. When to use this skill

Invoke when the user asks to:

- "Address the PR comments" / "handle PR review feedback" / "respond to review on PR #N"
- "Resolve the comments you can" / "reply to the unresolved threads"
- Anything that names a PR (`#N`, a URL, or "this branch's PR") and implies acting on review threads

Do **not** use this skill for:

- Drafting a *new* PR body or description (just use `gh pr create`)
- Posting a single ad-hoc comment unrelated to a review thread
- Resolving merge conflicts (different workflow)

## 1. Toolchain

Prefer `gh` CLI for everything — it's available in both Copilot CLI and Claude environments and authenticates with the user's existing token. The GitHub MCP server, when available, is acceptable for *read* operations (listing threads, fetching diffs); for *write* operations (replies, resolves) prefer `gh` so behavior is identical across agents.

```sh
gh --version                                  # confirm installed
gh auth status                                # confirm authenticated
gh api user --jq .login                       # current login (used in disclaimer)
```

If `gh auth status` shows logged out, stop and ask the user to run `gh auth login` — do not try to authenticate on their behalf.

### Guardrails by category

- Branch safety: never push to `main`; never force-push a branch with commits from multiple contributors.
- Quality gate: never bypass hooks with `--no-verify`; run the required lint/test/format checks before finalizing.
- Thread hygiene: always reply in-thread with the required disclaimer; resolve only `accept` threads where code changed.

> **Shell note**: examples below use POSIX shell syntax (`VAR=$(...)`, single-quoted multi-line `-f query='...'` blocks). On Windows PowerShell, adapt variable assignment (`$VAR = gh ...`) and quoting (here-strings `@'...'@` for the GraphQL bodies). The `gh` arguments themselves are identical across shells.

## 2. Identify the PR

If the user named a PR number or URL, use it. Otherwise resolve from the current branch:

```sh
gh pr view --json number,headRefName,baseRefName,url,state \
  --jq '{number, head: .headRefName, base: .baseRefName, url, state}'
```

If `state != "OPEN"`, stop and confirm with the user before proceeding — addressing comments on a closed/merged PR is almost always a mistake.

Capture into shell variables you'll reuse:

```sh
PR=$(gh pr view --json number --jq .number)
OWNER=$(gh repo view --json owner --jq .owner.login)
REPO=$(gh repo view --json name --jq .name)
ME=$(gh api user --jq .login)
```

## 3. Discover open review threads

Review threads — the line-anchored conversations attached to a review — are the unit of work. PR-level conversation comments (`gh pr view --comments`) are out of scope unless the user explicitly asks.

Fetch all unresolved threads with their full comment history via GraphQL. **Always** request `pageInfo` on both `reviewThreads` and per-thread `comments` so the agent can detect truncation rather than silently miss data. Also fetch each comment's parent-review state so pending (unsubmitted) comments can be filtered out (§10):

```sh
gh api graphql -F owner="$OWNER" -F repo="$REPO" -F number="$PR" -f query='
  query($owner: String!, $repo: String!, $number: Int!) {
    repository(owner: $owner, name: $repo) {
      pullRequest(number: $number) {
        reviewThreads(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes {
            id
            isResolved
            isOutdated
            isCollapsed
            path
            line
            originalLine
            comments(first: 50) {
              pageInfo { hasNextPage endCursor }
              nodes {
                id
                databaseId
                body
                diffHunk
                author { login }
                createdAt
                pullRequestReview { state }
              }
            }
          }
        }
      }
    }
  }'
```

Filter the result locally to threads where `isResolved == false`. Drop any comment whose `pullRequestReview.state == "PENDING"` — those belong to an unsubmitted review and shouldn't be replied to. Keep `isOutdated` threads in scope but flag them — the line they reference may no longer exist, which changes how you respond.

**Pagination.** Two connections in this query can paginate independently — handle each separately, since GraphQL cursors are per-connection (you cannot reuse a thread cursor for comments or vice versa):

1. **`reviewThreads.pageInfo.hasNextPage == true`**: re-issue the query above with an added `$cursor: String` variable and `reviewThreads(first: 100, after: $cursor)`, threading the returned `endCursor` through until `hasNextPage` is false.
2. **Any thread's `comments.pageInfo.hasNextPage == true`**: that thread has a long conversation. Issue a *separate* GraphQL query per such thread to walk its comments connection — e.g. `node(id: $threadId) { ... on PullRequestReviewThread { comments(first: 50, after: $commentCursor) { pageInfo {...} nodes {...} } } }` — until `hasNextPage` is false. Don't try to page nested comments by adding `after` to the outer query; the cursor types don't match.

Don't proceed on a partial dataset — silently skipping threads (or silently truncating a long thread's history) is worse than asking the user to confirm the scope.

## 4. Triage each thread

For each unresolved thread, classify it before touching code:

| Class | Meaning | Action |
| --- | --- | --- |
| **accept** | Concrete, actionable, agent agrees | Implement → push → reply → **resolve** |
| **decline** | Agent disagrees on technical grounds | Reply with reasoning → **do not resolve** |
| **question** | Reviewer asked for clarification | Reply with answer → **do not resolve** |
| **defer** | Valid but out of scope for this PR | Reply acknowledging + linking follow-up issue → **do not resolve** |
| **already-done** | Code already does what's asked (e.g. on a different line) | Reply pointing to current code → **do not resolve** |
| **outdated** | Thread was *already* `isOutdated: true` at triage time and the concern no longer applies | Reply explaining what changed → **do not resolve** (always leave for the human; the agent has no reliable way to attest *why* the line went stale before the agent ran) |

Decision flow (use this order each time):

1. If you agree and can implement now, choose `accept`.
2. Otherwise choose one reply-only class:
  `decline`, `question`, `defer`, `already-done`, or `outdated`.
3. Apply the action rule:
  `accept` => implement + reply + resolve.
  reply-only class => reply only, leave open.

**Do not silently change a classification within the same run.** If you start implementing an `accept` and discover the suggestion is wrong, stop, revert, and explicitly reclassify as `decline` with a reply explaining what you found.

### Timing rule: classifications are sticky from triage (single run)

Classify each thread **once**, at triage time (step §4), using the discovery snapshot from §3. Carry that classification — and the captured `thread.id` — through implement → push → reply → resolve. **Do not re-fetch and re-classify** between push and resolve in the same run.

This matters because GitHub flips `isOutdated` to `true` as soon as your push moves the line a thread anchors to — which is exactly what happens to *every* thread you just successfully addressed. If the agent re-queries threads after pushing and re-classifies them, every `accept` would suddenly look like `outdated` and never get resolved.

The `outdated` triage class is therefore narrow on purpose: it only applies to threads that were *already* `isOutdated: true` in the §3 snapshot, before the agent did anything. Threads that flip to `isOutdated: true` *because of* the agent's push remain classified `accept` and get resolved. The thread ID stays valid and `resolveReviewThread` does not care whether the thread is currently outdated.

## 5. Implement accepted changes

- One logical change per commit when practical; group only when changes are genuinely interdependent.
- Run the local quality gate **before pushing** (the recipe in §8 has it as step 3, after the commit, so the staged-file lint that runs in the pre-commit hook can do its job first). The exact set lives in the `quality-workflow-gate` skill, but for self-containment the required commands are:

  ```sh
  npm run lint
  npm test -- --run
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --features test-helpers -- -D warnings
  cargo test --workspace --features test-helpers
  ```

  If any of these fail, fix in a follow-up commit on the same branch (or amend if the broken commit hasn't been pushed yet) before pushing — **never** bypass hooks with `--no-verify` (see `.github/copilot-instructions.md` "Shift-left quality (principles)").
- Commit messages should reference the reviewer when natural, e.g. `fix(session): handle null worktree path (review feedback)`.
- Include the standard Copilot trailer if the agent posting is Copilot CLI; Claude has its own trailer convention.

Push to the PR branch (never to `main`, never force-push a branch with commits from multiple contributors):

```sh
git push origin "$(git branch --show-current)"
```

Capture the new HEAD SHA — you'll cite it in replies:

```sh
HEAD_SHA=$(git rev-parse HEAD)
SHORT_SHA=$(git rev-parse --short HEAD)
```

## 6. Reply in-thread (mandatory AI disclaimer)

Every reply the agent posts on behalf of the user **must** start with the disclaimer prefix below — no exceptions, including for one-line "done" replies. The disclaimer makes it unambiguous to other reviewers that an AI agent typed the body, even though the comment is attributed to the user's GitHub account.

### Disclaimer format (exact)

```md
🤖 AI agent reply (acting for @<gh-user>):

<body>
```

`<gh-user>` is a placeholder — at posting time, replace it literally with the output of `gh api user --jq .login` (the same value captured as `$ME` in §2). For example, if `$ME` is `mcaden`, the prefix is `🤖 AI agent reply (acting for @mcaden):`. Keep the emoji, the parenthetical, the colon, and the blank line. The body follows on the next line.

### Body conventions

- Address the reviewer's point directly. Don't restate their comment.
- For `accept`: state what changed and link the commit:
  `Done in <SHORT_SHA>. <one-line summary of the change>.`
- For `decline`: lead with the reason, not the disagreement. Cite spec/design docs when relevant (`per DESIGN.md §5.4`).
- For `question`: answer plainly. If you don't know, say so — don't guess.
- For `defer`: name the follow-up (`tracked in #NNN`) or say a follow-up will be filed.
- Never add filler ("Great catch!", "Thanks for the review!"). The reviewer's time is the scarce resource.

### Posting the reply

Reply **inside the existing thread** (not as a new top-level review comment) so the conversation stays threaded:

```sh
# $TOP_COMMENT_ID is the databaseId of the *first* comment in the thread
gh api -X POST \
  "repos/$OWNER/$REPO/pulls/$PR/comments/$TOP_COMMENT_ID/replies" \
  -f body="$REPLY_BODY"
```

The REST `replies` endpoint requires the integer `databaseId` of the top-level comment, not the GraphQL node ID. Get it from the GraphQL response in step 3 (`comments.nodes[0].databaseId`).

If posting via GraphQL is preferred (e.g. when batching), use `addPullRequestReviewThreadReply` with the thread's GraphQL node ID:

```sh
gh api graphql -F threadId="$THREAD_ID" -F body="$REPLY_BODY" -f query='
  mutation($threadId: ID!, $body: String!) {
    addPullRequestReviewThreadReply(input: {
      pullRequestReviewThreadId: $threadId, body: $body
    }) { comment { id url } }
  }'
```

## 7. Resolve threads — only when code changed

After the reply posts successfully, resolve **only** threads classified as `accept`. For everything else — `decline`, `question`, `defer`, `already-done`, `outdated` — leave the thread open. The human author/reviewer decides when those are settled.

```sh
gh api graphql -F threadId="$THREAD_ID" -f query='
  mutation($threadId: ID!) {
    resolveReviewThread(input: {threadId: $threadId}) {
      thread { isResolved }
    }
  }'
```

If the mutation returns `isResolved: false`, the thread was *not* resolved (commonly because the actor lacks write permission or the thread was already resolved). Don't retry; surface the failure in the final summary.

To unresolve (rare — only if the agent mistakenly resolved):

```sh
gh api graphql -F threadId="$THREAD_ID" -f query='
  mutation($threadId: ID!) {
    unresolveReviewThread(input: {threadId: $threadId}) {
      thread { isResolved }
    }
  }'
```

## 8. Order of operations (the recipe)

For each unresolved thread, in this order:

1. **Triage** (§4) — pick a class.
2. **Implement** (§5) — only if class is `accept`. Stage + commit.
3. **Quality gate** — lint/test/clippy locally; fix or revert before pushing.
4. **Push** — once per batch, after all `accept` changes for the PR are
   committed (avoids one push per thread).
5. **Reply** (§6) — disclaimer + body + commit ref.
6. **Resolve** (§7) — only `accept`.

Doing all the implementation work *before* any replies means: (a) the reply can cite a real, pushed SHA; (b) if the quality gate fails, no replies have been posted yet, so nothing to retract.

### Partial-failure recovery (post-push)

Once push succeeds, treat each thread's reply (and its conditional resolve) as an **independent** unit. If any single reply API call fails:

1. **Stop** further reply/resolve mutations for the rest of the batch.
2. Record which threads were already replied/resolved and which weren't.
3. Surface the failure in the §9 summary with the unprocessed thread IDs explicitly listed so the user can finish manually or rerun.

On a **rerun** of this skill against the same PR, before posting any new reply: re-fetch threads (§3) and check the **last comment** of each target thread for an existing AI reply that already cites the current `$SHORT_SHA`. If found, skip — replying again would duplicate. The check is identical to the self-loop guard in §10 (author + prefix match) but additionally requires the SHA in the body to match.

Note: on a rerun, threads the agent already addressed in a prior run will show as `isOutdated: true`. **Do not re-classify them as `outdated` from the §4 table** — the rerun's job is to finish posting replies/resolves for work already done, using the original `accept` classification (which you can recover from the existing AI reply's body referencing a prior `$SHORT_SHA`).

## 9. Final summary to the user

When the batch is done, print a compact table to the user:

```md
PR #N — addressed M of K open threads

  ✓ thread 1 (src/foo.ts:42)  accept    fixed in abc1234, resolved
  ✓ thread 2 (src/bar.rs:88)  decline   replied (left open for author)
  ✓ thread 3 (src/baz.tsx:5)  question  replied (awaiting reviewer)
  ✗ thread 4 (src/qux.rs:12)  defer     replied, follow-up #45
  ⚠ thread 5 (src/zap.ts:99)  outdated  replied (already stale before this run — left for human)
```

Always tell the user which threads were *not* resolved and why, so they can do the final pass.

## 10. Edge cases

- **No PR for current branch**: `gh pr view` exits non-zero. Ask the user which PR to address — don't open a new one.
- **Pending/draft reviews**: comments inside a `PENDING` review aren't visible to others yet. Skip them; they'll show up once submitted.
- **Agent's own previous AI replies (self-loop guard)**: when deciding whether to reply again, look at the thread's **last** comment and require **both** of:
  - `comments.nodes[-1].author.login == $ME` (the comment was posted by the same gh account the agent is using), **and**
  - `comments.nodes[-1].body` starts with the literal disclaimer prefix `🤖 AI agent reply (acting for @` (the username and closing `):` vary per agent, so match the fixed leading substring — looser matches like just `🤖 AI agent reply` will catch unrelated quotes).

  If both hold and **no human has replied since** that AI reply, treat the thread as already addressed and skip — never reply to your own reply in a loop. Author-only matching is not enough either: a human-attributed comment from the same gh account that doesn't carry the disclaimer should still be treated as a real reply.
- **Suggestion blocks** (` ```suggestion ` ): there is no REST/GraphQL endpoint for applying a suggestion programmatically — that's a GitHub UI feature only. If accepting, read the suggestion from the comment body, edit the file to match, and commit normally. The reply (per §6) should still cite the resulting `$SHORT_SHA`.
- **Force-push needed** (e.g. amending): only if the PR is the agent's own and not yet under review by humans, and only with the user's explicit OK. Default is to add a new commit.
- **Rate limits**: `gh api` will surface a 403 with `X-RateLimit-*` headers. Stop and report — don't sleep-and-retry blindly.
- **Permission errors on resolve**: external contributors' agents can reply but not resolve threads they don't own. Surface this as a warning in the final summary; don't treat it as a failure.

## 11. What this skill does *not* do

- Open follow-up issues (use `gh issue create` separately if needed).
- Re-request review (`gh pr review --request` is a separate decision).
- Merge the PR.
- Touch GitHub Actions runs, branch protections, or labels.
