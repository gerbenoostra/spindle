# Colours

The dashboard uses a small, fixed palette: a hue names a *kind* of thing,
never decoration for its own sake. Everything not listed here is plain
default-foreground text.

## Hues

| Hue | Meaning |
| --- | --- |
| yellow | waiting - a row blocked on you (`!`, `waiting`, the `Needs you` section header, detail attention lines) |
| red | error - `✗`, and bold red for `error`/`blocked` words, `checks failed` and `remote gone` |
| green | done or good - `✓`, `done`, `checks ok`, upstream `tracked`, forge `open`, verdict `safe` |
| blue | working - `●`, `working` |
| cyan | references and keys - the focused pane's border and title (bold cyan), footer and help keys; branch names once identity hues land |
| magenta | identifiers - conversation ids, commit hashes, PR/MR labels once identity hues land |
| dim gray | chrome only - unfocused borders and the separators between fields (`·`, `/`, `|`) |

All hues are the terminal's normal ANSI colours (1-6), plus ANSI 8 for
borders and separators - nothing else uses the dim gray or a bright
variant, so text stays readable on dark themes. Light themes are best
effort: nothing is invisible, but no contrast level is promised.

## The cursor bar

The focused list marks its cursor item - the row plus, under the global
scope, a conversation's context line - as one reverse-video bar across the
pane's inner width. In the lists that are not focused, the cursor item's
text is underlined instead and keeps its hues, so the row that set the
panel's scope stays findable.

Identity hues are not applied yet: labels, summaries and detail values
render in the default foreground. In `[4]`, headings are bold and attention
lines remain yellow, but values carry no identity hues.

## Decisions

- D1: hue means state or identity; bold carries hierarchy, ANSI 8 is chrome only.
- D2: normal ANSI 1-6 hues; red-bold for `error`/`blocked` words.
- D3: light themes are best effort, no contrast target.
- D4: focus stays cyan.
- D5: one reverse-video cursor bar in the focused list; unfocused lists underline the cursor item's text.
- D6: spans take a role; only `src/tui/palette.rs` names a colour.
- D9: earlier-life rows render plain - default foreground, no bold, no glyph.
