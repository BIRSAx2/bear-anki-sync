# bear-anki-sync

Sync flashcards from [Bear](https://bear.app) notes to [Anki](https://apps.ankiweb.net/) via [AnkiConnect](https://ankiweb.net/shared/info/2055492159).

Cards are written as callout blocks inside Bear notes. The note's heading hierarchy becomes the Anki deck path. Images, math, cloze deletions, and Bear tags are all handled automatically.

Comes as two binaries:

- **`bear-anki`**: CLI for scripting and manual syncs
- **`bear-anki-app`** : native macOS menu bar app with auto-sync

## Requirements

- macOS with Bear installed
- Anki desktop with the [AnkiConnect](https://ankiweb.net/shared/info/2055492159) plugin (code `2055492159`)
- Rust 1.85+

## Installation

### CLI only

```sh
cargo install bear-anki-sync
```

This installs the `bear-anki` command to `~/.cargo/bin/`.

### Menu bar app

After `cargo install`, run `bear-anki-app --install` to install the menu bar app:

```sh
cargo install bear-anki-sync
bear-anki-app --install                          # installs to ~/Applications
bear-anki-app --install --apps-dir /Applications # installs system-wide
```

Safe to re-run after upgrading.

`bear-anki-sync` reads notes and attachments directly from Bear's local SQLite database via `bear-rs`, so no CloudKit sign-in is needed.

## Card syntax

Cards are Bear callout blocks. Five types are recognised:

| Callout | Default Anki tag | Intended use |
|---|---|---|
| `[!CARD]` | `bear-card` | Generic : use when none of the below fit |
| `[!IMPORTANT]` | `bear-important` | Must-know facts, key definitions |
| `[!NOTE]` | `bear-note` | Standard definitions, reference material |
| `[!TIP]` | `bear-tip` | Mnemonics, practical rules, shortcuts |
| `[!WARNING]` | `bear-warning` | Pitfalls, common mistakes, gotchas |

The callout type is attached as an Anki tag, so you can filter cards by category inside Anki.

### Basic card : title as front

The callout title is the card front; the body is the back.

```markdown
> [!IMPORTANT] STRIDE threat categories
> Spoofing, Tampering, Repudiation, Information disclosure, Denial of service, Elevation of privilege.

> [!WARNING] Textbook RSA is malleable
> Enc(m₁) · Enc(m₂) = Enc(m₁ · m₂). Always use OAEP or PKCS#1 v1.5 padding.
```

### Basic card : body separator

Use `---` inside the body when the front needs more than one line.

```markdown
> [!NOTE]
> What is the difference between TCP and UDP?
> ---
> TCP is connection-oriented and guarantees ordered delivery.
> UDP is connectionless and trades reliability for lower latency.
```

### Cloze card

Any body containing `{{…}}` is treated as a cloze card. Each `{{word}}` is
converted to Anki's `{{c1::word}}`, `{{c2::word}}`, etc.

```markdown
> [!IMPORTANT]
> The {{one-time pad}} provides perfect secrecy because every ciphertext is equally likely for any plaintext.

> [!NOTE]
> TCP's three-way handshake is {{SYN}}, {{SYN-ACK}}, {{ACK}}.
```

Cloze detection takes priority : if the body contains `{{`, the callout title is ignored and the entire body becomes the cloze text.

### Deck from heading hierarchy

The Anki deck path mirrors the heading context at the card's position:

```markdown
# Systems Security

## Cryptography

### Symmetric Encryption

> [!IMPORTANT] AES-GCM
> Authenticated encryption mode : provides both confidentiality and integrity. Nonce must never be reused.
```

This card lands in **Systems Security::1.Cryptography::1.Symmetric Encryption**.

Rules:
- A new `##` clears the active `###`.
- A new `#` clears both `##` and `###`.
- Sibling `##` and `###` decks are prefixed with a source-order number so Anki's deck tree sorts them in Bear order. Only headings with direct or descendant cards consume a number. Padding is only added when that generated sibling deck count needs it, e.g. `1.A`, `2.B` for two generated sibling decks and `01.A`, `02.B`, ... `10.J` for ten.

### Ordering

Within a Bear note, decks and cards carry explicit ordering metadata:

- Heading decks are named with ordered path components such as `Systems::1.Cryptography::2.Hashing`, or `Systems::01.Cryptography` when that level has ten or more siblings.
- Cards are synced in Bear source order.
- Basic cards get a hidden order prefix in the Anki `Front` field, and cloze cards get it in the `Text` field. Sort the Anki browser by **Sort Field** to see cards in Bear order without showing the prefix during review.

Frontmatter `anki-deck` is treated as an exact deck path, so heading-order deck prefixes are not added when it is set.

### Card context

By default, cards under `##` or `###` headings show a small muted context pill above the Anki prompt. The context uses the unnumbered Bear heading path, for example `Cryptography / Symmetric Encryption`, while the normal card title remains unchanged below it.

This uses inline HTML in the standard Anki `Basic` and `Cloze` fields, so no custom Anki note model is required. Disable it with `include_card_context = false` in the config file, or change the separator with `card_context_separator`.

### Frontmatter deck override

To assign all cards in a note to a fixed deck regardless of headings, add `anki-deck` to YAML frontmatter at the very top of the note:

```markdown
---
anki-deck: Inbox::Unsorted
---

# My Note Title
```

### Bear tags

Bear tags on a note are copied to all its Anki cards. The `/` hierarchy separator is converted to `::`:

| Bear tag | Anki tag |
|---|---|
| `cs` | `cs` |
| `cs/networking` | `cs::networking` |

### Images

Images attached to a Bear note are read from Bear's local attachment store, uploaded to Anki's media collection, and embedded as `<img>` tags. Alt text written in the markdown (`![diagram of X](…)`) is preserved in the `alt` attribute.

Each unique image is uploaded at most once per sync run, even if it appears in multiple cards from the same note.

### Math

Bear's `$…$` and `$$…$$` syntax is converted to MathJax delimiters after markdown rendering, so Anki's MathJax renderer picks it up:

| Bear syntax | Rendered as | MathJax type |
|---|---|---|
| `$E = mc^2$` | `\(E = mc^2\)` | Inline |
| `$$E = mc^2$$` (single line) | `\(E = mc^2\)` | Inline |
| `$$…$$` (multi-line block) | `\[…\]` | Display |

Math inside code spans and fenced code blocks is left untouched.

---

## Menu bar app

`bear-anki-app` runs as a macOS menu bar icon. It shows sync status, Bear access state, last sync time, and card count at a glance.

**Menu actions:**

- **Sync Now** : incremental sync (skips unchanged cards)
- **Force Re-sync** : re-syncs all cards regardless of content hash
- **Check Bear Access** : verify Bear's local database is readable

**Auto-sync:** set `sync_interval_minutes` in the config file to sync automatically in the background (see [Configuration](#configuration)).

---

## CLI

### `sync`

```sh
bear-anki sync                          # sync all notes
bear-anki sync --tag anki               # only notes tagged #anki
bear-anki sync --note "Systems"         # only notes whose title contains "Systems"
bear-anki sync --dry-run                # print what would change, touch nothing
bear-anki sync --force                  # re-sync all cards regardless of content hash
bear-anki sync --verbose                # print each card action (add / update / skip)
```

Each sync adds new cards, updates changed cards, and deletes cards whose callout no longer exists. Unchanged cards are skipped via a content hash stored in the state file.

### `list`

Print all cards that would be synced. No Anki connection required.

```sh
bear-anki list
bear-anki list --tag anki
bear-anki list --note "Systems"
```

```
Systems Security (3 card(s))
  [basic] STRIDE threat categories
  [basic] AES-GCM
  [cloze] The {{c1::one-time pad}} provides perfect secrecy...
```

### `status`

Print tracked card counts grouped by note, with an optional Bear title lookup.

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

Clear the sync state. Use when starting fresh or switching Anki profiles.

```sh
bear-anki reset                        # clear local state, leave Anki notes untouched
bear-anki reset --delete-anki-notes    # delete tracked notes from Anki, then clear state
```

### Global flags

| Flag | Env var | Default |
|---|---|---|
| `--anki-url <url>` | `ANKI_URL` | `http://127.0.0.1:8765` |

Flag and env var take precedence over the config file.

---

## Configuration

Config is read from `~/Library/Application Support/bear-anki/config.toml`. The file is optional : all settings have defaults. Unknown keys are ignored.

```toml
# AnkiConnect endpoint. Override if Anki runs on a non-default port or host.
anki_url = "http://127.0.0.1:8765"

# Auto-sync interval for the menu bar app (minutes). 0 or absent = disabled.
sync_interval_minutes = 30

# Show a small heading-context pill above synced card prompts.
include_card_context = true
card_context_separator = " / "

# Map callout type → Anki tag. Unlisted types fall back to "bear-{type}".
[tags]
important = "exam-critical"
warning   = "pitfall"
tip       = "shortcut"
note      = "reference"
card      = "misc"
```

---

## How sync works

### Card identity

Each card is identified by a composite key of its Bear note ID and a fingerprint:

- **Basic card:** fingerprint = SHA-256 of the front text (first 8 bytes, hex-encoded)
- **Cloze card:** fingerprint = SHA-256 of the raw body before cloze conversion

This means:

| Change | Effect |
|---|---|
| Edit the back / cloze text | Card updated in place |
| Edit the front of a basic card | Old card deleted, new card added |
| Move a card to a different heading (deck) | Card moved in Anki |
| Reorder a card within the same heading | Card updated in place |
| Delete the callout from Bear | Card deleted from Anki |

### State file

The sync state lives at `~/Library/Application Support/bear-anki/state.json`. It maps each `(note_id, fingerprint)` pair to an Anki note ID and stores a content hash for change detection. Writes are atomic (written to `.tmp`, then renamed).

### Content hash

A separate SHA-256 hash covers the full card content (deck, source-order key, callout type, front, back). This is what determines whether a card needs to be updated : the fingerprint only determines *identity*, while the hash determines *whether the content changed*.

---

## Notes

- Encrypted and locked Bear notes are not synced.
- `sync` and `reset --delete-anki-notes` require Anki to be open with AnkiConnect active.
- The menu bar app requires macOS (uses the Accessory activation policy : no Dock icon).
- Math conversion runs after markdown rendering so CommonMark never strips the backslashes from `\(` and `\[`.
- If a card was synced before an image was correctly uploaded (e.g. after fixing a filename encoding issue), run `bear-anki sync --force` to re-process it.

## Development

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
```
