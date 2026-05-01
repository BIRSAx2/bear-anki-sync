use std::collections::{BTreeSet, HashMap};

use bear_rs::frontmatter::parse_front_matter;
use sha2::{Digest, Sha256};

const CALLOUT_PREFIXES: &[(&str, &str)] = &[
    ("> [!CARD]", "card"),
    ("> [!TIP]", "tip"),
    ("> [!NOTE]", "note"),
    ("> [!IMPORTANT]", "important"),
    ("> [!WARNING]", "warning"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub kind: CardKind,
    pub deck: String,
    pub fingerprint: String,
    pub callout_type: String, // "card", "tip", "note", "important"
    pub sort_key: String,
    pub context: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardKind {
    Basic { front: String, back: String },
    Cloze { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearNote {
    pub identifier: String,
    pub title: String,
    pub text: String,
    pub pinned: bool,
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadingLevel {
    title: String,
    ordinal: usize,
    width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadingOrder {
    ordinal: usize,
    width: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct HeadingOrders {
    h2: HashMap<(usize, usize), HeadingOrder>,
    h3: HashMap<(usize, usize, usize), HeadingOrder>,
}

impl HeadingOrders {
    fn h2_order(&self, h1_section: usize, raw_h2_ordinal: usize) -> HeadingOrder {
        self.h2
            .get(&(h1_section, raw_h2_ordinal))
            .copied()
            .unwrap_or_else(|| HeadingOrder {
                ordinal: raw_h2_ordinal,
                width: order_width_for_count(raw_h2_ordinal),
            })
    }

    fn h3_order(
        &self,
        h1_section: usize,
        raw_h2_ordinal: usize,
        raw_h3_ordinal: usize,
    ) -> HeadingOrder {
        self.h3
            .get(&(h1_section, raw_h2_ordinal, raw_h3_ordinal))
            .copied()
            .unwrap_or_else(|| HeadingOrder {
                ordinal: raw_h3_ordinal,
                width: order_width_for_count(raw_h3_ordinal),
            })
    }
}

pub fn parse_cards(note: &BearNote) -> Vec<Card> {
    let (frontmatter, body) = parse_front_matter(&note.text);
    let body = strip_html_comments(&body);

    let deck_override = frontmatter
        .as_ref()
        .and_then(|fm| fm.get("anki-deck"))
        .map(str::to_owned);

    let mut cards = Vec::new();
    let mut h1 = note.title.trim().to_owned();
    let mut h2: Option<HeadingLevel> = None;
    let mut h3: Option<HeadingLevel> = None;
    let mut h1_section = 0usize;
    let mut h2_ordinal = 0usize;
    let mut h3_ordinal = 0usize;
    let mut card_ordinal = 0usize;

    let lines: Vec<&str> = body.lines().collect();
    let heading_orders = compute_heading_orders(&lines);
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];

        if let Some(heading) = line.strip_prefix("### ") {
            h3_ordinal += 1;
            let order = heading_orders.h3_order(h1_section, h2_ordinal, h3_ordinal);
            h3 = Some(HeadingLevel {
                title: heading.trim().to_owned(),
                ordinal: order.ordinal,
                width: order.width,
            });
            index += 1;
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            h2_ordinal += 1;
            let order = heading_orders.h2_order(h1_section, h2_ordinal);
            h2 = Some(HeadingLevel {
                title: heading.trim().to_owned(),
                ordinal: order.ordinal,
                width: order.width,
            });
            h3 = None;
            h3_ordinal = 0;
            index += 1;
            continue;
        }
        if let Some(heading) = line.strip_prefix("# ") {
            h1 = heading.trim().to_owned();
            h2 = None;
            h3 = None;
            h1_section += 1;
            h2_ordinal = 0;
            h3_ordinal = 0;
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
                .unwrap_or_else(|| build_deck(&h1, h2.as_ref(), h3.as_ref()));
            let context = build_context(h2.as_ref(), h3.as_ref());

            let sort_key = (card_ordinal + 1).to_string();
            if let Some(card) =
                classify_card(title, body_lines, deck, callout_type, sort_key, context)
            {
                card_ordinal += 1;
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

    let card_width = order_width_for_count(cards.len());
    for (index, card) in cards.iter_mut().enumerate() {
        card.sort_key = format_order_key(index + 1, card_width);
    }

    cards
}

fn compute_heading_orders(lines: &[&str]) -> HeadingOrders {
    let mut used_h2: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut used_h3: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
    let mut h1_section = 0usize;
    let mut h2_ordinal = 0usize;
    let mut h3_ordinal = 0usize;
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];

        if line.strip_prefix("### ").is_some() {
            h3_ordinal += 1;
            index += 1;
            continue;
        }
        if line.strip_prefix("## ").is_some() {
            h2_ordinal += 1;
            h3_ordinal = 0;
            index += 1;
            continue;
        }
        if line.strip_prefix("# ").is_some() {
            h1_section += 1;
            h2_ordinal = 0;
            h3_ordinal = 0;
            index += 1;
            continue;
        }

        if let Some((_callout_type, rest)) = detect_callout(line) {
            let title = rest.strip_prefix(' ').unwrap_or("").trim().to_owned();
            let title = if title.is_empty() { None } else { Some(title) };

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

            if is_valid_card(title.as_deref(), &body_lines) {
                if h2_ordinal > 0 {
                    used_h2.insert((h1_section, h2_ordinal));
                }
                if h3_ordinal > 0 {
                    used_h3.insert((h1_section, h2_ordinal, h3_ordinal));
                }
            }
            continue;
        }

        index += 1;
    }

    build_heading_orders(used_h2, used_h3)
}

fn build_heading_orders(
    used_h2: BTreeSet<(usize, usize)>,
    used_h3: BTreeSet<(usize, usize, usize)>,
) -> HeadingOrders {
    let mut h2_by_h1: HashMap<usize, Vec<usize>> = HashMap::new();
    for (h1_section, h2_ordinal) in used_h2 {
        h2_by_h1.entry(h1_section).or_default().push(h2_ordinal);
    }

    let mut h3_by_parent: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (h1_section, h2_ordinal, h3_ordinal) in used_h3 {
        h3_by_parent
            .entry((h1_section, h2_ordinal))
            .or_default()
            .push(h3_ordinal);
    }

    let mut orders = HeadingOrders::default();
    for (h1_section, h2_ordinals) in h2_by_h1 {
        let width = order_width_for_count(h2_ordinals.len());
        for (index, h2_ordinal) in h2_ordinals.into_iter().enumerate() {
            orders.h2.insert(
                (h1_section, h2_ordinal),
                HeadingOrder {
                    ordinal: index + 1,
                    width,
                },
            );
        }
    }
    for ((h1_section, h2_ordinal), h3_ordinals) in h3_by_parent {
        let width = order_width_for_count(h3_ordinals.len());
        for (index, h3_ordinal) in h3_ordinals.into_iter().enumerate() {
            orders.h3.insert(
                (h1_section, h2_ordinal, h3_ordinal),
                HeadingOrder {
                    ordinal: index + 1,
                    width,
                },
            );
        }
    }

    orders
}

fn strip_html_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 4..];
        if let Some(end) = after_start.find("-->") {
            rest = &after_start[end + 3..];
        } else {
            rest = "";
            break;
        }
    }

    out.push_str(rest);
    out
}

fn detect_callout(line: &str) -> Option<(String, &str)> {
    for &(prefix, callout_type) in CALLOUT_PREFIXES {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((callout_type.to_owned(), rest));
        }
    }
    None
}

fn classify_card(
    title: Option<String>,
    body_lines: Vec<String>,
    deck: String,
    callout_type: String,
    sort_key: String,
    context: Vec<String>,
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
            sort_key,
            context,
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
            sort_key,
            context,
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
            sort_key,
            context,
        });
    }

    None
}

fn is_valid_card(title: Option<&str>, body_lines: &[String]) -> bool {
    if body_lines.iter().any(|line| line.contains("{{")) {
        return true;
    }
    if title.is_some() {
        return true;
    }
    if let Some(sep) = body_lines.iter().position(|line| line.trim() == "---") {
        return !body_lines[..sep].join("\n").trim().is_empty();
    }
    false
}

fn build_deck(h1: &str, h2: Option<&HeadingLevel>, h3: Option<&HeadingLevel>) -> String {
    let mut parts = vec![h1.to_owned()];
    if let Some(h) = h2 {
        parts.push(ordered_heading(h));
    }
    if let Some(h) = h3 {
        parts.push(ordered_heading(h));
    }
    parts.join("::")
}

fn build_context(h2: Option<&HeadingLevel>, h3: Option<&HeadingLevel>) -> Vec<String> {
    let mut context = Vec::new();
    if let Some(h) = h2 {
        context.push(h.title.clone());
    }
    if let Some(h) = h3 {
        context.push(h.title.clone());
    }
    context
}

fn ordered_heading(heading: &HeadingLevel) -> String {
    format!(
        "{}.{}",
        format_order_key(heading.ordinal, heading.width),
        heading.title
    )
}

fn order_width_for_count(count: usize) -> usize {
    count.max(1).to_string().len()
}

fn format_order_key(ordinal: usize, width: usize) -> String {
    format!("{ordinal:0width$}")
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
    use super::{convert_cloze, parse_cards, strip_html_comments, BearNote, CardKind};

    fn make_note(title: &str, text: &str) -> BearNote {
        BearNote {
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
    fn strips_fold_comments_from_headings() {
        let note = make_note(
            "Systems Security",
            "# Systems Security\n\n## Access Control and Authentication<!-- {\"fold\":true} -->\n\n> [!CARD] What is auth?\n> Identity verification.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].deck,
            "Systems Security::1.Access Control and Authentication"
        );
    }

    #[test]
    fn strips_html_comments_from_card_body() {
        let note = make_note(
            "Systems Security",
            "# Systems Security\n\n> [!CARD] Front\n> Bear metadata<!-- {\"preview\":\"true\"} --> stays hidden.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].kind,
            CardKind::Basic {
                front: "Front".into(),
                back: "Bear metadata stays hidden.".into(),
            }
        );
    }

    #[test]
    fn strip_html_comments_handles_unclosed_comment() {
        assert_eq!(strip_html_comments("hello <!-- broken"), "hello ");
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
        assert_eq!(cards[0].deck, "System Security::1.Chapter 1::1.Topic A");
        assert_eq!(cards[0].context, vec!["Chapter 1", "Topic A"]);
    }

    #[test]
    fn h2_resets_h3() {
        let note = make_note(
            "Deck",
            "# Deck\n## Part 1\n### Section A\n## Part 2\n\n> [!CARD] Q?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::1.Part 2");
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
        assert!(cards[0].context.is_empty());
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
    fn assigns_order_keys_from_card_source_order() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD] Q1?\n> A1.\n\n> [!CARD] Q2?\n> A2.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].sort_key, "1");
        assert_eq!(cards[1].sort_key, "2");
    }

    #[test]
    fn prefixes_sibling_decks_from_heading_source_order() {
        let note = make_note(
            "Deck",
            "# Deck\n## A1\n\n> [!CARD] Q1?\n> A1.\n## A2\n\n> [!CARD] Q2?\n> A2.\n## A10\n\n> [!CARD] Q3?\n> A3.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::1.A1");
        assert_eq!(cards[1].deck, "Deck::2.A2");
        assert_eq!(cards[2].deck, "Deck::3.A10");
    }

    #[test]
    fn pads_sibling_decks_only_to_the_width_needed() {
        let mut text = String::from("# Deck\n");
        for n in 1..=10 {
            text.push_str(&format!("## A{n}\n\n> [!CARD] Q{n}?\n> A.\n\n"));
        }
        let note = make_note("Deck", &text);
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::01.A1");
        assert_eq!(cards[9].deck, "Deck::10.A10");
    }

    #[test]
    fn unused_headings_do_not_consume_deck_order_numbers() {
        let note = make_note(
            "Deck",
            "# Deck\n## A1\n\n> [!CARD] Q1?\n> A.\n\n## A2\n## A3\n## A4\n## A5\n## A6\n## A7\n## A8\n## A9\n## A10\n\n> [!CARD] Q10?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::1.A1");
        assert_eq!(cards[1].deck, "Deck::2.A10");
    }

    #[test]
    fn parent_heading_numbering_uses_descendant_cards() {
        let note = make_note(
            "Deck",
            "# Deck\n## Empty\n### Also Empty\n## Used Parent\n### Used Topic\n\n> [!CARD] Q?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::1.Used Parent::1.Used Topic");
    }

    #[test]
    fn unused_h3_headings_do_not_consume_deck_order_numbers() {
        let note = make_note(
            "Deck",
            "# Deck\n## Chapter\n### Empty\n### Topic A\n\n> [!CARD] Q1?\n> A.\n\n### Topic B\n\n> [!CARD] Q2?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].deck, "Deck::1.Chapter::1.Topic A");
        assert_eq!(cards[1].deck, "Deck::1.Chapter::2.Topic B");
    }

    #[test]
    fn pads_card_order_keys_only_to_the_width_needed() {
        let note = make_note(
            "Deck",
            "# Deck\n\n> [!CARD] Q1?\n> A.\n\n> [!CARD] Q2?\n> A.\n\n> [!CARD] Q3?\n> A.\n\n> [!CARD] Q4?\n> A.\n\n> [!CARD] Q5?\n> A.\n\n> [!CARD] Q6?\n> A.\n\n> [!CARD] Q7?\n> A.\n\n> [!CARD] Q8?\n> A.\n\n> [!CARD] Q9?\n> A.\n\n> [!CARD] Q10?\n> A.",
        );
        let cards = parse_cards(&note);
        assert_eq!(cards[0].sort_key, "01");
        assert_eq!(cards[8].sort_key, "09");
        assert_eq!(cards[9].sort_key, "10");
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
