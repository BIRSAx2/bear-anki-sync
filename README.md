# bear-anki-sync

Syncs flashcards from [Bear.app](https://bear.app/) to [Anki](https://apps.ankiweb.net/) via [AnkiConnect](https://ankiweb.net/shared/info/2055492159).

Cards are written as callout blocks inside Bear notes. The note's heading hierarchy determines the Anki deck.

## Requirements

- macOS with Bear.app installed
- Anki desktop with the AnkiConnect plugin (plugin code `2055492159`)
- Rust toolchain

## Installation

```sh
git clone https://github.com/BIRSAx2/bear-anki-sync
cd bear-anki-sync
cargo install --path .
```

The binary is named `bear-anki`.

## Card syntax

Cards are callout blocks. Four types are recognised: `CARD`, `TIP`, `NOTE`, `IMPORTANT`. The type becomes an Anki tag (`bear-card`, `bear-tip`, `bear-note`, `bear-important`).

### Basic card — title as front

```markdown
> [!CARD] What is a buffer overflow?
> Memory written beyond the bounds of an allocated buffer,
> potentially overwriting adjacent memory.
```

The callout title is the front, the body is the back.

### Basic card — body separator

```markdown
> [!CARD]
> What is the difference between TCP and UDP?
> ---
> TCP is connection-oriented and guarantees delivery.
> UDP is connectionless and trades reliability for speed.
```

`---` inside the body splits front from back when multi-line front text is needed.

### Cloze card

```markdown
> [!CARD]
> The {{mitochondria}} is the powerhouse of the {{cell}}.
```

`{{word}}` converts to Anki's `{{c1::word}}`, `{{c2::word}}`, etc.

### Deck from heading hierarchy

The Anki deck is derived from the headings at the position of the card:

```markdown
# Systems Security
## Chapter 3
### Memory Safety

> [!CARD] What causes a use-after-free?
> Accessing memory after it has been freed.
```

This card is placed in `Systems Security::Chapter 3::Memory Safety`. A `##` heading clears `###`; a `#` heading clears both.

### Frontmatter deck override

To assign all cards in a note to a fixed deck, add `anki-deck` to the note's YAML frontmatter:

```markdown
---
anki-deck: Inbox::Unsorted
---
```

### Bear tags

Bear tags on a note are added to all its Anki cards. The `/` separator is converted to `::`:

| Bear tag        | Anki tag         |
|-----------------|------------------|
| `cs`            | `cs`             |
| `cs/networking` | `cs::networking` |

### Images and math

Images attached to a Bear note are uploaded to Anki's media collection and embedded as `<img>` tags.

Math uses Bear's `$...$` and `$$...$$` syntax:

- `$...$` → `\(...\)` (inline MathJax)
- `$$...$$` on one line → `\(...\)` (inline MathJax)
- `$$...$$` spanning multiple lines → `\[...\]` (display MathJax)

## Commands

### `sync`

```sh
bear-anki sync
bear-anki sync --tag anki           # notes tagged #anki only
bear-anki sync --note "Systems"     # notes whose title contains "Systems"
bear-anki sync --dry-run            # print what would change, touch nothing
bear-anki sync --force              # re-sync all cards regardless of content hash
bear-anki sync --verbose            # print each card action
```

Each sync adds new cards, updates changed cards, and deletes cards whose callout block no longer exists. Unchanged cards are skipped via a content hash.

### `list`

Prints all cards Bear would sync. Does not connect to Anki.

```sh
bear-anki list
bear-anki list --tag anki
bear-anki list --note "Systems"
```

Example output:

```
Systems Security (3 cards)
  [basic] What is a buffer overflow?
  [basic] What causes a use-after-free?
  [cloze] The {{c1::stack}} grows downward on x86.
```

### `status`

Prints the number of tracked cards grouped by note.

```sh
bear-anki status
```

```
12 card(s) tracked across 3 note(s):
    7 card(s)  Systems Security
    3 card(s)  Biology
    2 card(s)  Networking Fundamentals

State file: ~/Library/Application Support/bear-anki/state.json
```

### `reset`

Clears sync state. Use when starting fresh or switching Anki profiles.

```sh
bear-anki reset                       # clears local state, leaves Anki untouched
bear-anki reset --delete-anki-notes   # deletes tracked notes from Anki, then clears state
```

## Global flags

| Flag | Env var | Default |
|------|---------|---------|
| `--database <path>` | `BEAR_DATABASE` | auto-discovered |
| `--anki-url <url>` | `ANKI_URL` | `http://127.0.0.1:8765` |

## State file

Sync state is stored at `~/Library/Application Support/bear-anki/state.json`. It maps each card's identity (Bear note ID + fingerprint) to its Anki note ID and records a content hash for change detection. Writes are atomic: the file is written to a `.tmp` path and then renamed.

## Card identity

Each card's fingerprint is a hash of its front text (basic) or raw body before cloze conversion (cloze). This determines what counts as the same card across syncs:

- Editing the front of a basic card: old card deleted, new card added.
- Editing the back: existing card updated in place.
- Moving a card to a different deck: existing card updated in place.

## Notes

- Encrypted and locked Bear notes are not synced.
- `sync` and `reset --delete-anki-notes` require Anki to be running with AnkiConnect active.
- Math conversion runs after markdown rendering to avoid CommonMark stripping backslashes from `\(` and `\[`.

## Development

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
```
