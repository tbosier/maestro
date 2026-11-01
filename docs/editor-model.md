# Editor model

## Positions

Internal positions are char indices into the rope (`usize`). Conversions to other coordinate systems live in `maestro_core::pos`:

| Type | Meaning | Used by |
|---|---|---|
| `usize` | char index | everything inside the editor |
| `ByteOffset` | UTF-8 byte offset | file IO, regex matches |
| `LspPosition` | line plus UTF-16 code unit | language servers |
| `LineCol` | line plus display column (tabs expanded, wide chars count 2) | rendering, vertical motion |

Cursor movement steps over extended grapheme clusters, so a CRLF pair, an emoji ZWJ sequence or a letter with combining marks moves as one unit.

## Selections

A selection is one or more ranges, sorted and non-overlapping, with one primary. Each range has an anchor and a head.

In normal and select mode a range is inclusive: it covers every grapheme from the smaller end through the grapheme that starts at the larger end. A collapsed range (anchor equals head) covers the character under the block cursor. So `d` on a fresh cursor deletes one character, and `wd` deletes a word plus its trailing space.

In insert mode ranges are collapsed insertion points.

After every normal-mode command the editor:

1. commits the undo group,
2. clamps selections so no cursor sits past the last character,
3. scrolls the view to keep the primary cursor visible with `scrolloff` lines of margin.

Ranges that overlap after an edit merge. The merged range stays primary if either part was.

## Modes

- Normal: motions replace the selection (`w` selects the next word).
- Select (`v`): motions extend from the existing anchor.
- Insert: typing inserts at every cursor. The whole session, from entering insert to leaving it, is one undo step and one unit for `.`.
- Prompts (`:`, `/`, `?`, `s`, `S`, `K`) are overlays. Regex prompts preview their effect as you type; Escape restores the selection.

## Profiles

`maestro` (default) is the selection-first model described here.

`vim` adds a small set of Vim bindings on top: `dd`, `yy`, `cc`, `x`, `X`, `D`, `V`. It does not implement Vim's operator-pending grammar (`dw`, `ci(`, `3dd` with counts on operators beyond `dd`), visual block mode, or ex ranges other than `%`. Those keys report "no binding" instead of doing something unexpected.

There is no separate Helix profile yet. The Maestro profile already follows Helix for most keys; the differences are documented in `crates/maestro-editor/src/keymap.rs`.

## Undo history

`u` and `U` walk the current branch of the revision tree. Editing after an undo starts a new branch; the old one stays in memory. `g-` and `g+` (also `earlier` and `later` in the palette) step through every state in the order it was created, across branches, so an abandoned branch is always reachable.

## Repeat, macros, registers

- `.` replays the keys of the last change. A change is any command that modified the document, or an insert session from the key that entered insert mode through Escape.
- `Q` starts and stops recording into register `@` (or `"xQ` for register `x`). `q` replays; a count repeats it.
- Registers hold one string per selection. Yanking three selections and pasting into three selections puts each piece in its own place. `+` and `*` are the system clipboard. `_` discards.
- Text ending in a newline pastes linewise: below the line with `p`, above with `P`.

## Structural pipelines

These `:` commands select syntax nodes with tree-sitter queries, filter and grow those selections, and add a line above each one. They work on the multiple selections like any other command, so after a pipeline `c`, `i`, `a` or `d` act on every selection it left. The ones that read the tree need a recognized file type and a finished parse; otherwise the status line says why ("no syntax tree: file type not recognized", "rust syntax tree is still being built").

| Command | Does |
|---|---|
| `:select-nodes <query>` | Replaces the selections with every node the query matches in the whole buffer. |
| `:select-inside <query>` | Replaces each selection with the matching nodes that lie entirely inside it. |
| `:keep-nodes <query>`, `:remove-nodes <query>` | Keep, or remove, the selections that are exactly a node the query matches (same start and end). |
| `:keep-containing <query>`, `:remove-containing <query>` | Keep, or remove, the selections that contain a matching node anywhere inside them (the selection itself counts). |
| `:keep-matching <regex>`, `:remove-matching <regex>` | Keep, or remove, the selections whose text matches a regex, with smart case like `K` and `A-K`. |
| `:expand-to <kind>...` | Grows each selection to the smallest node of one of the given kinds that contains it (a selection that already is such a node stays). Selections inside no such node are dropped. Kinds are grammar names such as `function_item`. |
| `:insert-above <text>` | Inserts `<text>` on a new line above the first line of each selection, with that line's indentation and the document's line ending. One transaction, one undo step. Several selections on one line get one new line. If a selection starts after other text on its line (`impl X { pub fn a() {} }`), the line above would belong to other code, so nothing is inserted and the status line names the line. |

`keep-nodes` looks at the selection's own node and `keep-containing` at anything inside it. On a function, `keep-nodes returns-result` asks about that function's return type, while `keep-containing returns-result` is also true when a function nested in its body returns `Result`.

### Queries and shortcuts

A query argument is either a shortcut name or raw tree-sitter query syntax, which starts with `(`, `[`, `"` or `_`. A raw query selects the nodes of its capture named `@select`; without one, of its only capture; and a single pattern with no capture selects the node the whole pattern matches. The predicates `#eq?`, `#match?`, `#any-of?` and their `#not-` forms work. An invalid query changes nothing and reports where it went wrong: `select-nodes: invalid query at column 17: unknown node kind "nope"`.

| Language | Shortcuts |
|---|---|
| Rust | `fn` (functions and methods with a body), `pub-fn` (declared `pub`; `pub(crate)` and the like do not count), `async-fn`, `pub-async-fn`, `struct`, `enum`, `impl`, `trait`, `mod`, `call` (function and method calls, not macros), `string` (raw strings included), `returns-result` (functions whose return type is `Result` or a path ending in `Result`, such as `io::Result<()>`), `instrumented` (functions with `#[instrument]` or `#[tracing::instrument]`, with or without arguments, among the attributes and comments right above them) |
| Python | `fn`, `async-fn`, `class`, `call`, `string` |
| JavaScript | `fn` (declarations, expressions, arrow functions, methods), `class`, `call`, `string` (template literals included) |
| TypeScript, TSX | the JavaScript ones plus `interface`; `class` includes abstract classes |
| C | `fn`, `struct` (with a body), `call`, `string` |

TOML, JSON and Markdown have no shortcuts; raw queries work there.

In Rust, attributes and doc comments are sibling nodes in front of an item, not part of it. A function's selection starts at `pub` or `fn`, so `keep-matching instrument` does not see `#[instrument]`; the `instrumented` shortcut looks at the siblings instead. `insert-above` puts its line between the existing attributes and the function. Code in doc comments, strings and macro bodies is not parsed as items, so it never matches.

Selections are sorted and never overlap, so a match inside another match (a function nested in a function) merges into the outer one. The status line counts the merges: `select-nodes 14 (1 nested or overlapping merged)`. The primary selection becomes the first one that ends after the old cursor.

### Pipelines

Steps chain with `|` on one command line. The spec's example, "select every public async function that returns `Result` and has no tracing instrumentation", then adding the attribute:

```
:select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented | insert-above #[tracing::instrument]
```

The status line shows the selection count after each step: `select-nodes 9 | keep-nodes 8 | remove-nodes 5 | insert-above 5: inserted 5 lines (u undoes)`. One `u` removes all five lines. The same pipeline with raw queries:

```
:select-nodes (function_item (visibility_modifier) @v (#eq? @v "pub") (function_modifiers "async")) @select | keep-nodes (function_item return_type: [(generic_type type: (type_identifier) @t) (generic_type type: (scoped_type_identifier name: (type_identifier) @t))] (#eq? @t "Result")) @select | remove-nodes ((attribute_item (attribute [(identifier) (scoped_identifier)] @a (#match? @a "^(tracing::)?instrument$"))) . [(attribute_item) (line_comment) (block_comment)]* . (function_item) @select) | insert-above #[tracing::instrument]
```

Rules:

- A `|` inside double quotes, inside `()`, `[]` or `{}`, or after a backslash does not split. A regex alternation therefore goes in parentheses: `keep-matching (foo|bar)`.
- The whole line is checked before anything runs: step names, queries, regexes and node kinds. Only the commands in the table above can be steps, and `insert-above` only as the last one.
- If a step fails or leaves no selections, nothing changes (selections and text stay as they were) and the status line names the step: `step 3 (remove-nodes) left no selections (after select-nodes 9 | keep-nodes 8); nothing changed`.
- A pipeline is one `:` command, so its edit is one undo step.

Through the automation API, `editor.execute` runs a pipeline line and returns the status message; a failing step returns error `-32016` with the same message. `command.run` with a step's name runs it without an argument, which only reports what argument it needs.

Cost: each query step runs its query over the whole tree. On a generated 10,003-line, 165 KB Rust file with 527 matching functions, the spec's pipeline took a median of 40 ms for the three selection steps and 46 ms with the insertion (21 runs each, release build, timed from Enter to return in `measure_spec_pipeline_on_10k_lines` in `crates/maestro-editor/tests/pipelines.rs`). About 2 to 4 ms of each query step is compiling the query, which is not cached.

Not built: a filter on the text of an enclosing node (use `expand-to` and then `keep-matching`, which changes the selections), other text-changing steps (edit the selections afterward with the usual commands), and key bindings for these commands.

## Invariants the tests hold

- A change set refuses to apply to a document of a different length.
- Applying a change set and then its inverse restores the original text (property test).
- Mapped positions stay inside the new document (property test).
- Selections stay inside the document after arbitrary key sequences (`ranges_stay_valid_after_random_edits`).
- Saving never leaves a partial file: the new text is written and synced to a temp file, then renamed over the target.
- A buffer whose file changed on disk refuses `:w` until the user chooses `:w!` or `:reload`. The check covers metadata, file identity and a hash of the bytes, so a same-size rewrite with a restored timestamp is caught.
- A self-invoking macro stops after 16 levels of nesting with an error.
- Text pasted while a prompt or picker is open goes into the prompt or picker, never into the document.

## Completion

Completion needs a language server that offers it. It works in insert mode only.

A request goes out at the primary cursor in three cases:

- at once, when the typed character is one of the server's trigger characters (rust-analyzer uses `:`, `.`, `'` and `(`);
- 150 ms after a letter, digit or `_`, if no other key came in between. A fast typist's keys arrive 80 to 120 ms apart, so a burst sends one request when it ends;
- at once on `C-x`.

The popup opens under the word being completed, or above it when there is no room below. Each row shows the label, the kind as a word (`function`, `module`, `struct`) and the server's detail. Rows are filtered and ranked by the identifier typed before the cursor (letters, digits and `_`): the same fuzzy match as the pickers, case-insensitive unless the prefix has an uppercase letter, then the server's `sortText`. Typing more narrows the list. If the server marked the list incomplete, typing also sends a new request after the same 150 ms.

| Key | In the popup |
|---|---|
| `C-n`, Down / `C-p`, Up | next / previous item, wrapping |
| Tab, Enter | accept |
| Escape | close the popup and stay in insert mode |
| `C-x` | ask again |

Escape closes only the popup, the way it closes a picker or a prompt and does nothing else; a second Escape leaves insert mode. Any key that moves a cursor (primary or not) without editing closes the popup, and so does typing a character that ends the identifier.

Accepting applies the item as one transaction:

- the item's `textEdit` (for an `InsertReplaceEdit`, its insert range), or else its `insertText`, or else its label, replacing the identifier before the cursor. Ranges come from the server in UTF-16 and are converted against the text the request was made for, then follow every edit typed since, so the replaced range grows with the prefix;
- its `additionalTextEdits` (an import, say), once;
- line breaks in the new text become the document's line ending.

Snippets (`insertTextFormat` 2) are inserted as plain text. Tab stops disappear, placeholders keep their default text, a choice keeps its first option, and variables become their default or nothing. The cursor goes to `$0`, or to the end when there is no `$0`. There is no jumping between tab stops.

With several cursors, the item's edit goes in at the primary cursor. Every other cursor gets the same edit only if the text around it is the same as the text the edit replaces at the primary (the same characters from the edit's start up to the cursor, and from the cursor to the edit's end). The others are left alone and the status line says "completed at N of M cursors". An item whose additional edits overlap its main edit is refused and nothing changes.

A reply is dropped, and nothing is shown, unless on arrival the document version, every cursor, the focused view and its buffer are the same as when the request was sent, the editor is in insert mode, and no prompt or picker is open. The version only grows, so any edit in between drops the reply; a cursor that moved and came back does not. Leaving insert mode or closing the popup also cancels the request. The 150 ms timer checks the same things before it sends anything, so a mouse click into another view or another spot cancels it.

An item whose additional edits sit where you have typed or deleted since the reply is dropped from the popup, since where they belong is no longer known.

In a view smaller than 8 columns or 8 rows the popup is not shown and its keys do their usual work.

`completionItem/resolve` is never sent. The editor does not tell servers it can resolve properties later, so servers send `additionalTextEdits` with the list. Item documentation is not shown.

Repeat and macros: the popup's own keys are not recorded. An accepted item is recorded as the keys that type the same edit (backspaces over the replaced text, the new text, Left to the cursor stop), so `.` and macros repeat it without a server. When typing cannot reproduce the edit (it has additional edits, it skipped a cursor, its new text has line breaks or tabs, or it replaces text after the cursor or text with whitespace in it), `.` repeats nothing for that insert session and a macro being recorded stops with a warning.

## The sign column

The first gutter cell holds one sign. An error or warning diagnostic letter (`E`, `W`) takes it first; otherwise a Git change (`+`, `~`, `-`); otherwise an info or hint letter (`I`, `H`). The diagnostic message for the cursor line is in the status line either way.

## Known limitations

- A reload (automatic or `:reload`) replaces only the region between the old and new text's common prefix and suffix. Cursors, marks and jump entries outside that region keep their place; ones inside it move to its start.

- Bracketed paste is applied directly and is not recorded by macros or `.`.
- `:open` and `:w` take one path; it may contain spaces.
