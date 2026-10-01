# Attention parity

The dashboard's unacknowledged-attention projection, kept row-for-row with
the shared attention contract: the detailed state the conversation carries,
the summary it shows until acknowledged, and the glyph a row leads with.

| Event or effective condition           | Detailed state                  | Summary while unseen | Glyph |
| -------------------------------------- | ------------------------------- | -------------------- | ----- |
| Turn active                            | Busy                            | working              | ●     |
| Clean turn end                         | Idle + CompletedUnseen          | done                 | ✓     |
| Blocked on the human                   | Waiting(reason)                 | waiting              | !     |
| Aborted turn                           | Idle or Unknown + retained error | error               | ✗     |
| Acknowledged end, error or wait        | the detailed state, unchanged   | none                 | blank |
| No reliable signal                     | Unknown                         | unknown              | ?     |

## Ordering

- Cross-pane rank, the inbox order: `waiting > error > done > working`.
- Per-pane latch precedence, which unseen event still shows on one row:
  `error > done > waiting > working`.
- The orderings differ on purpose: rank answers which pane needs attention
  most; precedence answers which event you still have not seen.
