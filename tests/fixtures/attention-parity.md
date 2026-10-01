# Attention parity

The dashboard's unacknowledged-attention projection, kept row-for-row with
the shared attention contract: the detailed state the conversation carries,
the summary it shows until acknowledged, and the glyph a row leads with.

The `Events` column is what the test drives through the journal reduction
and arbitration on a live process: normalized events in commit order,
`ack` for an acknowledgement of everything pending at that point, and `-`
for no events at all. A cell may list several sequences, comma-separated;
each must produce the row.

| Event or effective condition    | Events                                                  | Detailed state                   | Summary while unseen | Glyph |
| ------------------------------- | ------------------------------------------------------- | -------------------------------- | -------------------- | ----- |
| Turn active                     | `start`, `start activity`                               | Busy                             | working              | ●     |
| Clean turn end                  | `start end`                                             | Idle + CompletedUnseen           | done                 | ✓     |
| Blocked on the human            | `start awaiting`                                        | Waiting(reason)                  | waiting              | !     |
| Aborted turn                    | `start error`                                           | Idle or Unknown + retained error | error                | ✗     |
| Acknowledged end, error or wait | `start end ack`, `start error ack`, `start awaiting ack` | the detailed state, unchanged    | none                 | blank |
| No reliable signal              | `-`                                                     | Unknown                          | unknown              | ?     |

## Ordering

- Cross-pane rank, the inbox order: `waiting > error > done > working`.
- Per-pane latch precedence, which unseen event still shows on one row:
  `error > done > waiting > working`.
- The orderings differ on purpose: rank answers which pane needs attention
  most; precedence answers which event you still have not seen.
