# Colours

The dashboard uses a small, fixed palette: a hue names a *kind* of thing,
never decoration for its own sake. Everything not listed here is plain
default-foreground text.

## Hues

| Hue | Meaning |
| --- | --- |
| yellow | waiting - a row blocked on you (`!`, `waiting`, the `Needs you` section header, detail attention lines) |
| red | error - `✗`, and bold red for `error`/`blocked` words, `checks failed` and `remote gone` |
| green | done or good - `✓`, `done`, `checks ok` |
| blue | working - `●`, `working` |
| cyan | references and keys - branch names, the focused pane's border and title (bold cyan), footer and help keys |
| magenta | identifiers - conversation ids and PR/MR labels in the lists (detail identifiers stay plain for now) |
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

## Where hues apply

In the lists these all apply: repo names are bold, branches cyan,
conversation ids and PR/MR labels magenta, state words take their glyph's
hue, and a summary's verdicts (`checks ok`, `blocked`, `remote gone`) go
green or bold red. In `[4]`, headings are bold and attention lines remain
yellow, but detail values still render plain - the identity hues land
there with the detail-pane work.

## Decisions

- A hue means state (waiting, error, done, working) or identity (references,
  identifiers), and bold carries hierarchy - because a hue that only
  decorates teaches nothing; not a neutral gray ladder (labels, summaries and
  ages flatten into one weight), not lazygit's roles (status hues double as
  type hues).
- Hues are the normal ANSI 1-6, with bold red for `error`/`blocked` words -
  because, compared side by side, they read better on Catppuccin and degrade
  less on light themes; not the bright 9-14 set, which wins on Kanagawa and
  Tango. Accepted: red, and on Tango blue and magenta, stay below 4.5:1
  contrast.
- ANSI 8 is for borders and separators only - because it falls below 4.5:1
  on common dark themes, and ANSI 7 is near-invisible on light ones.
- Light themes are best effort: nothing is invisible - because both hue sets
  mostly fail contrast on a light background; not a light-theme contrast
  target.
- Focus stays cyan - because green already means done; not lazygit's green
  focus border.
- One cursor bar, in the focused list only; other lists underline their
  cursor item - because a bar in every list reads as three cursors, and the
  unfocused cursor still sets the next pane's scope; not bold (labels that
  are already bold barely change), not an invisible unfocused cursor.
- A span takes a role, never a colour, and only `src/tui/palette.rs` builds
  a style - because the next palette change then audits one file; not
  inline styles, not a configurable theme.
- Earlier-life rows render plain - default foreground, no bold, no glyph -
  because an earlier life must not look as current as the live one; not the
  live rows' accents.
- A work summary is a typed structure - reason parts, fact parts, the
  parked flag - whose flat text is computed once from those parts at
  construction - because the renderer hues the parts and `list --json`
  keeps emitting the same string; not re-parsing a flattened display
  string into pieces (it breaks silently when wording changes), not
  serializing the parts (it widens the JSON contract for a rendering
  concern).
