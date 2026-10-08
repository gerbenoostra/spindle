# Attention

The `[3]` conversation list and the `Needs you` section of `[2]` are an
inbox: they list what is asking for you, and they empty only when you have
seen it. This page covers what puts a row there, what clears it, and how
agent hooks feed it.

## Glyphs and sections

| Glyph | Attention | Means |
| --- | --- | --- |
| `!` | waiting | the agent is blocked on you: a question, a permission prompt, a plan to approve |
| `✗` | error | a turn aborted and you have not seen it |
| `✓` | done | a turn ended cleanly and you have not seen it |
| `●` | working | the agent is busy and nothing above is pending |
| `?` | unknown | a live process with no applicable evidence, or seen-state that cannot be read |
| blank | none | nothing pending |

A conversation can show a retained `✗` or `✓` while it is busy again. The row
then reads `error · working` or `done · working`, so the unseen event is not
hidden by the new turn. A `✗` or `✓` also survives the process exiting: an
unseen event does not go away because the agent quit.

On a work row the glyph is the most urgent attention among its bound
conversations, ranked `waiting > error > done > working`. The row sits under
`Needs you` for waiting, error or done, and under `Active` for working.
Everything else stays in the flat list below them.

## What clears attention

Attention is acknowledged, and so cleared, by exactly four things:

- **Focus.** A refresh sees the agent's pane active, in its session's
  current window, with a client attached. The dashboard's own pane never
  counts, including the pane a `display-popup` covers.
- **`space`** on a row that carries attention. On a work row it
  acknowledges every bound conversation.
- **A new prompt** (`start`) on the conversation.
- **`enter`** on a live conversation, the deliberate jump: a successful
  pane selection acknowledges it. Resuming a stopped conversation
  acknowledges just before the provider process launches - see
  [the caveat](architecture/work-lifecycle.md#decisions).

Moving the cursor, opening the detail pane and starting the dashboard never
acknowledge anything.

A wait is acknowledged as an episode. Once you have seen a wait it stays
blank until the agent starts a new wait, even if it is still waiting.

## Marking a stuck turn not busy

A turn interrupted without an event (a killed tool, a lost hook) can leave a
conversation `●` busy forever while its process lives. `space` on a busy row
with nothing to acknowledge records a not-busy mark. The row leaves `Active`,
and any newer event or provider update for that conversation overrides the
mark. A process that has exited is never busy, and needs no mark.

When the store refuses a `space` write, the footer says `space: not saved -`
and the reason until the next key.

## Hooks

Agents report lifecycle events by running:

```sh
agent-sessions hook <provider> <event>
```

`provider` is `claude`, `vibe` or `devin`. `event` is the agent's own event
name, and the hook payload arrives on stdin. The command never writes stdout.
It exits zero on any failure to record, so it cannot break the agent, and
reports the failure on stderr. Only an unknown provider is a usage error.
Event names the mapping does not know are recorded as diagnostic activity.

Claude publishes waiting and busy states in its own session files, which the
dashboard reads directly. Its `Stop` and `StopFailure` hooks are the only
source of `✓` and `✗`.

## Decisions

- Acknowledging a wait clears it until a newer wait begins - because the
  shared attention projection renders an acknowledged wait blank, and an
  inbox you have looked at must be able to empty; not keeping `!` for as long
  as the agent waits (the row would never leave `Needs you` while you decide).
- Cursor movement, the detail pane and startup do not acknowledge - because
  none of them proves you saw the agent, and an inbox must not empty as you
  scroll; not acknowledging on selection.
- A new prompt acknowledges everything retained, while tool activity does
  not - because answering implies the attention was seen; not clearing on
  any later event.
- An acknowledged error shows blank, not `?` - because `?` means no evidence
  at all, and an acknowledged error is evidence; not `?` for every live row
  without a current execution claim.
