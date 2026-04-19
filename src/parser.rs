use bear_cli::db::ExportNote;
use bear_cli::frontmatter::parse_front_matter;
use sha2::{Digest, Sha256};

const CALLOUT_TYPES: &[&str] = &["CARD", "TIP", "NOTE", "IMPORTANT", "WARNING"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub kind: CardKind,
    pub deck: String,
    pub fingerprint: String,
    pub callout_type: String, // "card", "tip", "note", "important"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardKind {
    Basic { front: String, back: String },
    Cloze { text: String },
}

pub fn parse_cards(note: &ExportNote) -> Vec<Card> {
    let (frontmatter, body) = parse_front_matter(&note.text);

    let deck_override = frontmatter
        .as_ref()
        .and_then(|fm| fm.get("anki-deck"))
        .map(str::to_owned);

    let mut cards = Vec::new();
    let mut h1 = note.title.trim().to_owned();
    let mut h2: Option<String> = None;
    let mut h3: Option<String> = None;

    let lines: Vec<&str> = body.lines().collect();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];

        if let Some(heading) = line.strip_prefix("### ") {
            h3 = Some(heading.trim().to_owned());
            index += 1;
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            h2 = Some(heading.trim().to_owned());
            h3 = None;
            index += 1;
            continue;
        }
        if let Some(heading) = line.strip_prefix("# ") {
            h1 = heading.trim().to_owned();
            h2 = None;
            h3 = None;
            index += 1;
            continue;
        }

        if let Some((callout_type, rest)) = detect_callout(line) {
            let title = rest.strip_prefix(' ').unwrap_or("").trim().to_owned();
            let title = if title.is_empty() { None } else { Some(title) };

            // Collect body lines
            let mut body_lines: Vec<String> = Vec::new();
            index += 1;
            while index < lines.len() {
                let body_line = lines[index];
                if body_line.is_empty() {
                    break;
                }
                if let Some(content) = body_line.strip_prefix("> ") {
                    body_lines.push(content.to_owned());
                } else if body_line == ">" {
                    body_lines.push(String::new());
                } else {
                    break;
                }
                index += 1;
            }

            let deck = deck_override
                .clone()
                .unwrap_or_else(|| build_deck(&h1, h2.as_deref(), h3.as_deref()));

            if let Some(card) = classify_card(title, body_lines, deck, callout_type) {
                cards.push(card);
            } else {
                eprintln!(
                    "bear-anki: skipping malformed callout in {:?} ({})",
                    note.title, note.identifier
                );
            }
            continue;
        }

        index += 1;
    }

    cards
}

fn detect_callout(line: &str) -> Option<(String, &str)> {
    for &ct in CALLOUT_TYPES {
        let prefix = format!("> [!{ct}]");
        if let Some(rest) = line.strip_prefix(prefix.as_str()) {
            return Some((ct.to_lowercase(), rest));
        }
    }
    None
}

fn classify_card(
    title: Option<String>,
    body_lines: Vec<String>,
    deck: String,
    callout_type: String,
) -> Option<Card> {
    let has_cloze = body_lines.iter().any(|line| line.contains("{{"));

    if has_cloze {
        let raw = body_lines.join("\n");
        let fp = fingerprint(&raw);
        let text = convert_cloze(&raw);
        return Some(Card {
            kind: CardKind::Cloze { text },
            deck,
            fingerprint: fp,
            callout_type,
        });
    }

    if let Some(front) = title {
        let back = body_lines.join("\n").trim().to_owned();
        let fp = fingerprint(&front);
        return Some(Card {
            kind: CardKind::Basic { front, back },
            deck,
            fingerprint: fp,
            callout_type,
        });
    }

    // No title — look for separator
    let sep_index = body_lines.iter().position(|line| line.trim() == "---");
    if let Some(sep) = sep_index {
        let front = body_lines[..sep].join("\n").trim().to_owned();
        let back = body_lines[sep + 1..].join("\n").trim().to_owned();
        if front.is_empty() {
            return None;
        }
        let fp = fingerprint(&front);
        return Some(Card {
            kind: CardKind::Basic { front, back },
            deck,
            fingerprint: fp,
            callout_type,
        });
    }

    None
}

fn build_deck(h1: &str, h2: Option<&str>, h3: Option<&str>) -> String {
    let mut parts = vec![h1];
    if let Some(h) = h2 {
        parts.push(h);
    }
    if let Some(h) = h3 {
        parts.push(h);
    }
    parts.join("::")
}

fn convert_cloze(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut counter = 1usize;
    let mut chars = text.char_indices().peekable();

    while let Some((_i, ch)) = chars.next() {
        if ch == '{' && chars.peek().map(|(_, c)| *c) == Some('{') {
            chars.next(); // consume second '{'
            let mut inner = String::new();
            let mut closed = false;
            while let Some((_, c)) = chars.next() {
                if c == '}' && chars.peek().map(|(_, c)| *c) == Some('}') {
                    chars.next(); // consume second '}'
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            if closed {
                result.push_str(&format!("{{{{c{counter}::{inner}}}}}"));
                counter += 1;
            } else {
                // Unclosed — emit as-is
                result.push_str("{{");
                result.push_str(&inner);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

fn fingerprint(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    hex::encode(&hash[..8])
}

#[cfg(test)]
mod tests {
    use bear_cli::db::ExportNote;

    use super::{CardKind, convert_cloze, parse_cards};

    fn make_note(title: &str, text: &str) -> ExportNote {
        ExportNote {
            identifier: "NOTE-1".into(),
            title: title.into(),
            text: text.into(),
            pinned: false,
            created_at: None,
            modified_at: None,
            tags: vec![],
        }
    }

    #[test]
    fn parses_basic_card_with_title_as_front() {
        let note = make_note(
            "System Security",
            "# System Security\n\n> [!CARD] What is a buffer overflow?\n> Memory beyond buffer bounds is overwritten.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].kind,
            CardKind::Basic {
                front: "What is a buffer overflow?".into(),
                back: "Memory beyond buffer bounds is overwritten.".into(),
            }
        );
        assert_eq!(cards[0].deck, "System Security");
        assert_eq!(cards[0].callout_type, "card");
    }

    #[test]
    fn parses_basic_card_with_separator() {
        let note = make_note(
            "System Security",
            "# System Security\n\n> [!CARD]\n> What is a buffer overflow?\n> ---\n> Memory beyond buffer bounds is overwritten.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].kind,
            CardKind::Basic {
                front: "What is a buffer overflow?".into(),
                back: "Memory beyond buffer bounds is overwritten.".into(),
            }
        );
    }

    #[test]
    fn parses_cloze_card() {
        let note = make_note(
            "Biology",
            "# Biology\n\n> [!CARD]\n> The {{mitochondria}} is the powerhouse of the {{cell}}.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].kind,
            CardKind::Cloze {
                text: "The {{c1::mitochondria}} is the powerhouse of the {{c2::cell}}.".into(),
            }
        );
    }

    #[test]
    fn assigns_deck_from_heading_hierarchy() {
        let note = make_note(
            "System Security",
            "# System Security\n## Chapter 1\n### Topic A\n\n> [!CARD] Question?\n> Answer.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "System Security::Chapter 1::Topic A");
    }

    #[test]
    fn h2_resets_h3() {
        let note = make_note(
            "Deck",
            "# Deck\n## Part 1\n### Section A\n## Part 2\n\n> [!CARD] Q?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::Part 2");
    }

    #[test]
    fn h1_resets_h2_and_h3() {
        let note = make_note(
            "Deck",
            "# Deck\n## Part 1\n### Section A\n# New Top\n\n> [!CARD] Q?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "New Top");
    }

    #[test]
    fn card_before_any_subheading_uses_root_deck() {
        let note = make_note("System Security", "# System Security\n\n> [!CARD] Q?\n> A.");
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "System Security");
    }

    #[test]
    fn frontmatter_deck_overrides_heading_context() {
        let note = make_note(
            "System Security",
            "---\nanki-deck: Custom::Override\n---\n# System Security\n## Chapter 1\n\n> [!CARD] Q?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Custom::Override");
    }

    #[test]
    fn skips_malformed_card_with_no_title_no_separator_no_cloze() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD]\n> Just a body line with no structure.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 0);
    }

    #[test]
    fn parses_multiple_cards() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD] Q1?\n> A1.\n\n> [!CARD] Q2?\n> A2.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 2);
    }

    #[test]
    fn fingerprints_are_stable() {
        let note = make_note("Deck", "# Deck\n\n> [!CARD] Q?\n> A.");
        let cards1 = parse_cards(&note);
        let cards2 = parse_cards(&note);
        assert_eq!(cards1[0].fingerprint, cards2[0].fingerprint);
    }

    #[test]
    fn fingerprints_differ_for_different_fronts() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD] Q1?\n> A.\n\n> [!CARD] Q2?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_ne!(cards[0].fingerprint, cards[1].fingerprint);
    }

    #[test]
    fn converts_multiple_cloze_blanks() {
        assert_eq!(
            convert_cloze("The {{mitochondria}} is the {{powerhouse}}."),
            "The {{c1::mitochondria}} is the {{c2::powerhouse}}."
        );
    }

    #[test]
    fn convert_cloze_handles_unclosed_brace() {
        // Unclosed {{ should be passed through without panic
        let result = convert_cloze("foo {{ bar");
        assert!(result.contains("bar"));
    }

    #[test]
    fn parses_tip_callout() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!TIP] Remember this trick.\n> Use shortcut Cmd+K.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].callout_type, "tip");
        assert_eq!(
            cards[0].kind,
            CardKind::Basic {
                front: "Remember this trick.".into(),
                back: "Use shortcut Cmd+K.".into(),
            }
        );
    }

    #[test]
    fn parses_note_callout() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!NOTE] What is idempotency?\n> An operation that produces the same result when applied multiple times.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].callout_type, "note");
    }

    #[test]
    fn parses_important_callout() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!IMPORTANT] Never store secrets in plain text.\n> Use a secrets manager or OS keychain.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].callout_type, "important");
    }

    #[test]
    fn parses_warning_callout() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!WARNING] Never use textbook RSA.\n> It is malleable and provides no semantic security.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].callout_type, "warning");
    }

    #[test]
    fn parses_mixed_callout_types_in_one_note() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD] Q?\n> A.\n\n> [!TIP] T?\n> T.\n\n> [!IMPORTANT] I?\n> I.\n\n> [!WARNING] W?\n> W.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 4);
        assert_eq!(cards[0].callout_type, "card");
        assert_eq!(cards[1].callout_type, "tip");
        assert_eq!(cards[2].callout_type, "important");
        assert_eq!(cards[3].callout_type, "warning");
    }
}
