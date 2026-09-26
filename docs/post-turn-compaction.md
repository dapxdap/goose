# Post-turn compaction

Branch: `feature/post-turn-compaction`

Three commits on top of `aaif-goose/goose@04ed836`:

| Commit | Change |
|---|---|
| `07ad22e` | post-turn compaction after final model response |
| `dd244ba` | emit a usage event after post-turn compaction so the context meter updates |
| `9bed00c` | raise the older-half summarization budget to 50k, gate the pre-inference pass on the threshold |

All three flags default to **off**. With no environment variables set, behaviour is
unchanged from upstream, including the existing tests and callers of `compact_messages`.

## What changes

### 1. Compaction moves from before the reply to after it

Upstream compacts at the start of a turn: the history is summarized *before* the model
sees the new message, so the turn that triggers compaction is generated from already
reduced context.

With `GOOSE_POST_TURN_COMPACTION=1`, compaction runs after the turn's final assistant
message has been yielded and the agent is idle waiting for the user:

- the model generates the response from the full conversation;
- the summarization request overlaps with the time the user spends reading the answer,
  instead of delaying the next reply;
- the next request starts from a conversation that already fits.

The trigger is `check_if_post_turn_compaction_needed` in
`crates/goose/src/context_mgmt/mod.rs`: it fires once the conversation exceeds **1/10 of
the context window**, independent of `GOOSE_COMPACTION_THRESHOLD`. That floor only keeps
one-message exchanges from being compacted. Setting the flag also disables the pre-turn
auto-compaction pass in the legacy `reply()` loop, so the two do not both fire.

Implemented in both agent loops, per the migration rule:

- legacy loop — end of `reply_internal` in `crates/goose/src/agents/agent.rs`
- state machine — `CompactionOperation` in
  `crates/goose/src/agents/state_machine/ops_compaction.rs`

The reactive recovery compaction (`ContextLengthExceeded` handling in `agent.rs`) is
untouched: it already runs after the error and is a separate, error-scoped path.

### 2. Only the older half gets summarized

`crates/goose/src/context_mgmt/half_history.rs` (new) splits the history into a
summarized block and a block carried through verbatim.

Summarizing the whole history makes the retained context as large as the summary the
model feels like writing. Summarizing the oldest `OLDER_HALF_TOKEN_BUDGET` tokens (50k)
bounds the result by something already known. The budget is fixed rather than
proportional because a summarizer with a small context window cannot take a proportional
slice of a very long history.

`retained_split` walks the boundary back until it no longer separates a tool request from
its response — a response left in the retained half without its request is a message the
provider cannot render — and only lands on agent-visible messages. At least two messages
are always kept verbatim, so a compaction can never reduce a conversation to a summary
alone.

Ordering in the compacted conversation: summarized messages (agent-invisible), then the
retained verbatim half, then the summary and the agent-visible continuation, with the
preserved user prompt and the carried turn-context event last. The continuation stays
last of the new messages because `Conversation` validation drops a trailing assistant
message. `created` timestamps are reassigned monotonically, since storage reloads order by
`created_timestamp` and the retained copies must not resurface at their original time.

Each decision is logged for verification:

```
Compaction split decision  split_index=Some(41) total_messages=87
```

### 3. Compaction can run on a separate model

`GOOSE_COMPACT_MODEL` (with optional `GOOSE_COMPACT_PROVIDER`) points the summarization
request at a cheaper or local model, keeping the large history out of the primary model's
context. Resolution lives in `Agent::compaction_model_config` /
`Agent::compaction_provider`, and applies to all three call sites: post-turn, the manual
`/compact` command (`execute_commands.rs`), and recovery compaction. Without the override
both fall back to the session model and provider.

### 4. Context meter refresh after compaction

`dd244ba` adds `yield AgentEvent::Usage(compaction.usage.clone())` after
`HistoryReplaced`. Without it the UI context indicator keeps showing the pre-compaction
token count.

## Configuration

| Variable | Default | Effect |
|---|---|---|
| `GOOSE_POST_TURN_COMPACTION` | off | Compact after the turn instead of before the next reply |
| `GOOSE_HALF_HISTORY_COMPACTION` | off | Summarize only the older half, keep the recent half verbatim |
| `GOOSE_COMPACT_MODEL` | session model | Model used to write summaries |
| `GOOSE_COMPACT_PROVIDER` | session provider | Provider for the summarization request |

Accepted truthy spellings are `1`, `true`, `TRUE`, `yes`. `get_param::<bool>` deserializes
strictly, so `1` would silently disable a boolean-typed flag — these are read as raw
strings and matched explicitly, the same way `GOOSE_STATE_MACHINE` is handled.

```bash
export GOOSE_POST_TURN_COMPACTION=1
export GOOSE_HALF_HISTORY_COMPACTION=1
export GOOSE_COMPACT_MODEL=compactor:latest
export GOOSE_COMPACT_PROVIDER=custom_ollama_nb01
export GOOSE_STATE_MACHINE=1   # optional, exercises the state-machine path
```

Environment is read at process start. For the Electron app, the variables have to be in
the environment of the launched process — a desktop launcher wrapper script that exports
them before `exec` is the reliable place; `/etc/environment` and shell rc files are not
inherited by a GUI-launched session.

## Verification

```bash
cargo test -p goose half_history          # split unit tests
grep 'Compaction split decision' ~/.local/state/goose/logs/cli/$(date +%F)/cli.log
```

`half_history.rs` covers: short history is not split, the split keeps the recent half, the
boundary never orphans a tool response, and the boundary is always agent-visible.

To confirm the timing manually: run several turns until the conversation crosses 1/10 of
the context window. The "goose is compacting the conversation…" progress message and
"Compaction complete" should appear **after** the assistant's answer to the current
prompt, and `split_index` in the log should be `Some(...)` rather than `None`.

## Status

- `cargo check`, `cargo fmt`, `cargo clippy -p goose --lib` clean.
- `cargo clippy --all-targets` reports 15 `E0425`/`E0432`/`E0433` errors in
  `crate::scheduler` / `ScheduleTool`. These reproduce on the base commit without these
  changes and are not addressed here.
- The 50k budget and the pre-inference threshold gate in `9bed00c` are tuned for a local
  summarizer with a 32k context window. For upstream they likely need justification or to
  become configurable; the first two commits are the general mechanism.
