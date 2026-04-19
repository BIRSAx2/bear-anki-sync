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

Cards are callout blocks. Five types are recognised:

| Callout | Anki tag | Intended use |
|---|---|---|
| `[!IMPORTANT]` | `bear-important` | Must-know facts, key definitions |
| `[!NOTE]` | `bear-note` | Standard definitions, reference material |
| `[!TIP]` | `bear-tip` | Mnemonics, practical rules, shortcuts |
| `[!WARNING]` | `bear-warning` | Pitfalls, common mistakes, gotchas |
| `[!CARD]` | `bear-card` | Generic — use when none of the above fit |

Use the callout type that best reflects the nature of the content. The callout type is attached as an Anki tag so cards can be filtered in Anki by category.

### Basic card — title as front

```markdown
> [!IMPORTANT] STRIDE
> Spoofing, Tampering, Repudiation, Information disclosure, Denial of service, Elevation of privileges.

> [!NOTE] Cryptographic hash function
> A function mapping arbitrary-length input to a fixed-length digest. Must satisfy preimage resistance, second preimage resistance, and collision resistance.

> [!WARNING] Textbook RSA is malleable
> Enc(m₁) · Enc(m₂) = Enc(m₁ · m₂). Never use RSA without proper padding (OAEP or PKCS#1 v1.5).
```

The callout title is the card front; the body is the back.

### Basic card — body separator

```markdown
> [!NOTE]
> What is the difference between TCP and UDP?
> ---
> TCP is connection-oriented and guarantees delivery.
> UDP is connectionless and trades reliability for speed.
```

`---` inside the body splits front from back when the front requires multiple lines.

### Cloze card

```markdown
> [!IMPORTANT]
> The {{one-time pad}} provides perfect secrecy because every ciphertext is equally likely for any plaintext.
```

`{{word}}` converts to Anki's `{{c1::word}}`, `{{c2::word}}`, etc. Cloze detection takes priority over the title/separator formats.

### Deck from heading hierarchy

The Anki deck is derived from the headings at the position of the card:

```markdown
# Systems Security
## Cryptography
### Symmetric Encryption

> [!IMPORTANT] AES modes
> ECB is insecure (same block → same ciphertext). Use CBC, CTR, or GCM.
```

This card is placed in `Systems Security::Cryptography::Symmetric Encryption`. A `##` heading clears `###`; a `#` heading clears both.

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
  [basic] STRIDE
  [basic] AES modes
  [cloze] The {{c1::one-time pad}} provides perfect secrecy...
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
