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

These `:` commands select syntax nodes with tree-sitter queries, filter and grow those selections, and change the text at each one. They work on the multiple selections like any other command, so after a pipeline `c`, `i`, `a` or `d` act on every selection it left. The ones that read the tree need a recognized file type and a finished parse; otherwise the status line says why ("no syntax tree: file type not recognized", "rust syntax tree is still being built").

| Command | Does |
|---|---|
| `:select-nodes <query>` | Replaces the selections with every node the query matches in the whole buffer. |
| `:select-inside <query>` | Replaces each selection with the matching nodes that lie entirely inside it. |
| `:keep-nodes <query>`, `:remove-nodes <query>` | Keep, or remove, the selections that are exactly a node the query matches (same start and end). |
| `:keep-containing <query>`, `:remove-containing <query>` | Keep, or remove, the selections that contain a matching node anywhere inside them (the selection itself counts). |
| `:keep-matching <regex>`, `:remove-matching <regex>` | Keep, or remove, the selections whose text matches a regex, with smart case like `K` and `A-K`. An empty match counts (`^` matches every selection). |
| `:expand-to <kind>...` | Grows each selection to the smallest node of one of the given kinds that contains it (a selection that already is such a node stays). Selections inside no such node are dropped. Kinds are grammar names such as `function_item`. |
| `:insert-above <text>` | Inserts `<text>` (one line, no line breaks) on a new line above the line where each selection starts, with that line's indentation and the document's line ending. Lines end at `\n` here, as they do for compilers, not at the other characters the editor also breaks lines at (a lone CR, form feed, U+2028), so the new line cannot end a `//` comment early. One transaction, one undo step, also when run in the middle of an insert session. Several selections on one line get one new line. If a selection starts after other text on its line (`impl X { pub fn a() {} }`), the line above would belong to other code, so nothing is inserted and the status line names the line. |
| `:insert-below <text>` | The same, on a new line below the line where each selection ends, indented like the line where it starts (so a function's closing `}` line gets a line under it at the function's indentation). A selection that ends before other text on its line is refused the same way. At the end of a file without a final line break, the new line gets its line break in front. |
| `:replace-text <regex> <replacement>` | Replaces every match of the regex inside each selection, never outside it. Smart case as for `keep-matching`, and `^` and `$` match at line starts and ends inside the selection. `$1`, `${1}` and `${name}` put in a capture group (write `${1}x` when a letter or digit follows, since `$1x` names a group `1x`), and `$$` is a literal `$`. A replacement that names a group the regex does not have is refused. An empty match inserts: `replace-text ^ "// "` comments out every line of each selection. |
| `:delete` | Deletes the selected text exactly, and nothing around it (the line break and indentation stay). |
| `:wrap <before> <after>` | Puts `<before>` in front of each selection and `<after>` after it: `wrap "Some(" )`. Either may be `""`, not both. |
| `:prepend <text>`, `:append <text>` | Inserts `<text>` at the start, or the end, of each selection. The text may have line breaks (`"\n"` in quotes). |

The last seven change text. `insert-above` and `insert-below` work line by line on every selection the steps left, nested ones too; the other five work on the selections after nested ones merged, which never overlap. A text step that would leave the text as it was (`replace-text` with no match) is an error and changes nothing. After the edit the selections still cover the same text: what `wrap` and `prepend` put in front of a selection lands outside it.

`keep-nodes` looks at the selection's own node and `keep-containing` at anything inside it. On a function, `keep-nodes returns-result` asks about that function's return type, while `keep-containing returns-result` is also true when a function nested in its body returns `Result`.

### Queries and shortcuts

A query argument is either a shortcut name or raw tree-sitter query syntax, which starts with `(`, `[`, `"` or `_`. A raw query selects the nodes of its capture named `@select`; without one, of its only capture; and a single pattern with no capture selects the node the whole pattern matches. The predicates `#eq?`, `#match?`, `#any-of?` and their `#not-` and `#any-` forms work. Any other predicate (a misspelled `#equal?`, or `#is?`) is refused, because tree-sitter would ignore it and the query would match more than it says. An invalid query changes nothing and reports where it went wrong, as a column on the command line: `:select-nodes (function_item (nope))` says `select-nodes, column 30: invalid query: unknown node kind "nope"`.

| Language | Shortcuts |
|---|---|
| Rust | `fn` (functions and methods with a body), `pub-fn` (declared `pub`; `pub(crate)` and the like do not count), `async-fn`, `pub-async-fn`, `struct`, `enum`, `impl`, `trait`, `mod`, `call` (function and method calls, not macros), `string` (raw strings included), `returns-result` (functions whose return type is `Result` or a path ending in `Result`, such as `io::Result<()>`), `instrumented` (functions with `#[instrument]` or `#[tracing::instrument]`, also written `::tracing::instrument`, with spaces or comments in the path, or with raw identifiers, with or without arguments, among the attributes and comments right above them; matched by name, so a `use` that renames the macro is not seen) |
| Python | `fn`, `async-fn`, `class`, `call`, `string` |
| JavaScript | `fn` (declarations, expressions, generators, arrow functions, methods), `class`, `call`, `string` (template literals included) |
| TypeScript, TSX | the JavaScript ones plus `interface`; `class` includes abstract classes |
| C | `fn`, `struct` (with a body), `call`, `string` |

TOML, JSON and Markdown have no shortcuts; raw queries work there.

In Rust, attributes and doc comments are sibling nodes in front of an item, not part of it. A function's selection starts at `pub` or `fn`, so `keep-matching instrument` does not see `#[instrument]`; the `instrumented` shortcut looks at the siblings instead. `insert-above` puts its line between the existing attributes and the function. Code in doc comments, strings and macro bodies is not parsed as items, so it never matches.

Selections are sorted and never overlap, so a match inside another match (a function nested in a function) merges into the outer one. In a pipeline that happens only at the end: every step sees nested matches separately, so a filter that drops an outer function keeps a matching one inside it, and `insert-above` adds a line above both when both are left. The status line counts the merges: `select-nodes 15; 14 selections after merging 1 nested in others`. The primary selection becomes the first one that ends after the old cursor.

### Pipelines

Steps chain with `|` on one command line. The spec's example, "select every public async function that returns `Result` and has no tracing instrumentation", then adding the attribute:

```
:select-nodes pub-async-fn | keep-nodes returns-result | remove-nodes instrumented | insert-above #[tracing::instrument]
```

The status line shows the selection count after each step: `select-nodes 9 | keep-nodes 8 | remove-nodes 5 | insert-above 5: inserted 5 lines (u undoes)`. One `u` removes all five lines. Shortcuts and raw queries mix; this is the same pipeline with the first two shortcuts written out as the queries they stand for:

```
:select-nodes (function_item (visibility_modifier) @v (#eq? @v "pub") (function_modifiers "async")) @select | keep-nodes (function_item return_type: [(type_identifier) @t (scoped_type_identifier name: (type_identifier) @t) (generic_type type: (type_identifier) @t) (generic_type type: (scoped_type_identifier name: (type_identifier) @t))] (#eq? @t "Result")) @select | remove-nodes instrumented | insert-above #[tracing::instrument]
```

Rules:

- In a selection step, a `|` splits steps unless it is inside parentheses, inside a `[...]` bracket (a regex character class such as `[)|]`, or a query alternation), inside a double-quoted string within parentheses (a predicate's argument), or after a backslash. A regex alternation therefore goes in parentheses: `keep-matching (foo|bar)`.
- In a text step, only double quotes and a backslash keep a `|` from splitting. Parentheses and brackets are plain characters there, so an unbalanced `(` in a text cannot swallow the rest of the line. Write a text with a `|` in quotes: `append " | x"`.
- The whole line is checked before anything runs: step names, queries, regexes, node kinds, quotes and capture groups. Only the commands in the table above can be steps, and at most one changes text, as the last step.
- An error names the step and the column on the line where it is (one-based, in characters): `step 3 (remove-nodes), column 77: invalid query: unknown node kind "bogus"`. A pipeline of one step says just its name: `:wrap x` says `wrap, column 7: needs two arguments: <before> <after>`.
- If a step fails or leaves no selections, nothing changes (selections and text stay as they were) and the status line names the step: `step 3 (remove-nodes) left no selections (after select-nodes 9 | keep-nodes 8); nothing changed`.
- A pipeline is one `:` command, so its edit is one undo step.

#### Text arguments

The text steps take their arguments in one of two forms.

- Bare: `insert-above`, `insert-below`, `prepend` and `append` take the rest of the step, trimmed, exactly as written, backslashes included. `insert-above #[derive(Debug)]` inserts `#[derive(Debug)]`. `wrap` and `replace-text` take two bare words, split at spaces.
- Quoted: an argument that starts with `"` runs to the next `"` that is not escaped. Inside, `\"` is a quote, `\\` a backslash, `\n` a line break and `\t` a tab. Any other backslash stays as written, so a regex keeps its escapes: `replace-text "(\w+)_old" "${1}_new"`. Use quotes for leading or trailing spaces (`prepend "pub "`), for spaces inside a `wrap` or `replace-text` argument, for a `|`, and for an empty argument (`""`).

`insert-above` and `insert-below` refuse a text with a line break. A quote that is never closed, a quoted argument followed by more text, and a wrong number of arguments are errors with their column.

Through the automation API, `editor.execute` runs a pipeline line and returns the status message; a failing step returns error `-32016` with the same message. `command.run` with a step's name runs it without an argument, which only reports what argument it needs. An agent can also send a pipeline as an intent edit (`proposal.create` with `pipeline`, or `document.apply_pipeline`), which runs on the whole document and is run again when the document moved on; see [Intent edits](automation.md#intent-edits).

Cost: each query step runs its query over the whole tree. On a generated 10,003-line, 165 KB Rust file with 527 matching functions, the spec's pipeline took a median of 44 ms for the three selection steps and 50 ms with the insertion (21 runs each, release build, timed from Enter to return in `measure_spec_pipeline_on_10k_lines` in `crates/maestro-editor/tests/pipelines.rs`). About 2 to 4 ms of each query step is compiling the query, which is not cached.

Not built: a filter on the text of an enclosing node (use `expand-to` and then `keep-matching`, which changes the selections), more than one text step in a pipeline, and key bindings for these commands.

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

## Signature help

In insert mode, typing one of the server's signature trigger characters (rust-analyzer uses `(`, `,` and `<`) or pressing `C-k` asks for the signature of the call around the cursor. It shows in a one-line box above the cursor line (below it when there is no room), so it does not cover the completion popup. The parameter the cursor is in is bold and underlined as well as colored. When the server offers several signatures, the title says which one is shown (`signature 1/2`); there is no key to switch.

While the box is showing or a request is out, every key that edits or moves a cursor asks again, so the marked parameter follows typing. A change without a key (a mouse click, a paste) hides the box until the next key asks again. Each request cancels the one before it. A reply is dropped unless the buffer version, every cursor, the focused view and insert mode are what they were when it was sent. An empty answer closes the box, and so does leaving insert mode. `C-k` outside a call says "no signature help here".

## Rename

`space n` (or `:rename`) renames the symbol under the cursor. When the server supports `textDocument/prepareRename`, it is asked first: an answer of nothing or an error is shown in the status line and no prompt opens. Otherwise a `rename to:` prompt opens with the current name filled in. `:rename <name>` skips the prompt.

The server's answer is a workspace edit, applied as described under [Edits from language servers](#edits-from-language-servers). The status line says how many edits went into how many files, that they are not saved, and how many files were opened for the edit. Each buffer's change is one undo step of its own.

## Formatting

`space =` or `:format` formats the buffer; `:format-selection` sends only the primary selection, for servers that support range formatting. The indent settings (`indent`, `indent-width`) go along as the formatting options. The result is one undo step. An edit that replaces whole runs of lines (a formatter that answers with the full text, say) is split into the lines that really differ, so cursors on unchanged lines stay put, and each view keeps the cursor's line on the same screen row. There is no format on save.

## Code actions

`space c` (or `:code-action`) asks for the code actions at the cursor, or for the primary selection when it covers more than one character. The diagnostics that overlap that range are sent along, as the server published them with their ranges moved to where the text is now. The actions open in a picker: each row has the title, the kind (`quickfix`, `refactor.extract`), `preferred` when the server marks one, and the reason when an action is unavailable.

Choosing an action applies its edit, then runs its command with `workspace/executeCommand`. An action that comes without either is first completed with `codeAction/resolve` when the server supports it. A command's own changes come back as a `workspace/applyEdit` request. If any buffer changed, opened or closed between listing the actions and choosing one, nothing is applied and the status line says to ask again.

## Edits from language servers

Rename, code actions, formatting and the server's `workspace/applyEdit` requests all go through one path, and an edit is applied whole or not at all. Before any buffer changes:

- an edit that names the document version it was made for must match the open buffer's version;
- the edit may not create, rename or delete files, and may not name the same file twice;
- every range must lie inside the text (a character past the end of a line counts as the line end, as the protocol says; a line past the end of the file is refused), and no two edits in a file may overlap. Edits that only insert at the same spot go in in the order the server gave;
- no file may be read-only;
- no file may contain a form feed, vertical tab, NEL (U+0085) or Unicode line or paragraph separator. The editor counts those as line breaks and the protocol does not, so the server's line numbers would point at the wrong lines.

A file the edit touches that is not open is read into a new buffer, without a view, and edited there. Nothing is written to disk: the changed buffers are marked modified and `:wa` saves them. A file that is not open must be a regular file of at most 16 MiB. An edit that does not name versions (the protocol's `changes` form) cannot be checked against a version. For answers to Maestro's own requests, the answer is dropped if any buffer changed, opened or closed while waiting, and a `workspace/applyEdit` that arrives while a command Maestro sent to that server is still running is refused under the same rule. An edit a server sends on its own, unversioned, is applied to the text as it is.

The server is answered `applied: true`, or `applied: false` with the reason, and the status line shows the same reason.

Rename, formatting and code actions are not repeated by `.`; after a code action, `.` still repeats the change before it. Asking for code actions or opening the rename prompt with `space n` stops a macro being recorded, with a warning, since the picker or prompt only opens once the server answers and a replay would send the keys meant for it to the buffer instead.

## The sign column

The first gutter cell holds one sign. An error or warning diagnostic letter (`E`, `W`) takes it first; otherwise a Git change (`+`, `~`, `-`); otherwise an info or hint letter (`I`, `H`). The diagnostic message for the cursor line is in the status line either way.

## Known limitations

- A reload (automatic or `:reload`) replaces only the region between the old and new text's common prefix and suffix. Cursors, marks and jump entries outside that region keep their place; ones inside it move to its start.

- Bracketed paste is applied directly and is not recorded by macros or `.`.
- `:open` and `:w` take one path; it may contain spaces.
