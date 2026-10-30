# 0002: Inclusive, grapheme-aligned selections

Date: October 30, 2026. Status: accepted.

## Decision

Ranges are inclusive in the Kakoune sense: anchor and head are both grapheme starts and the range covers through the grapheme at the larger end. A collapsed range covers one grapheme. In insert mode ranges are treated as insertion points.

## Why

Commands never need a special case for "nothing selected": `d`, `c`, `y`, `r` and `~` all act on the covered text, which is at least the character under the cursor. Helix uses half-open ranges with a one-wide minimum, which needs extra logic to decide where the visible cursor sits. The inclusive model is simpler and matches what the user sees.

## Consequences

- The end of a range needs the text to compute (`Range::to(text)`), because it is one grapheme past the last covered start.
- Normal mode cannot put a cursor at the end-of-text position of a non-empty document; selections are clamped after every command.
- The cursor may rest on a line terminator, so empty lines are reachable and `x` on them works.
