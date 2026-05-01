use std::collections::HashMap;

use anyhow::Result;
use base64::Engine as _;
use pulldown_cmark::{html, Options, Parser};
use sha2::{Digest, Sha256};

use crate::anki::AnkiClient;

#[derive(Debug, Clone)]
pub struct NoteImage {
    pub filename: String,
    pub upload_key: String,
    pub content_hash: String,
    pub data: Vec<u8>,
}

impl NoteImage {
    pub fn new(filename: String, upload_key: String, data: Vec<u8>) -> Self {
        let content_hash = short_hash(&data);
        Self {
            filename,
            upload_key,
            content_hash,
            data,
        }
    }

    pub fn anki_filename(&self) -> String {
        let suffix = short_hash(self.upload_key.as_bytes());
        format!("bear_{suffix}_{}", self.filename)
    }
}

/// Render card text for Anki:
/// 1. Upload image attachments and replace `![alt](filename)` with `<img>` tags
/// 2. Render markdown to HTML
/// 3. Convert Bear math syntax (`$...$`, `$$...$$`) to MathJax delimiters
///    (runs on the HTML output so `\(` is never processed as a markdown escape)
///
/// `image_files`   — images attached to this note.
/// `upload_cache`  — deduplicate uploads across multiple `render_for_anki` calls for the
///                   same note (keyed on attachment identity -> Anki filename).
pub fn render_for_anki(
    text: &str,
    image_files: &[NoteImage],
    client: &AnkiClient,
    upload_cache: &mut HashMap<String, String>,
) -> Result<String> {
    let text = process_images(text, image_files, client, upload_cache)?;
    let text = markdown_to_html(&text);
    let text = convert_math_html(&text);
    Ok(text)
}

// ── Markdown rendering ────────────────────────────────────────────────────────

fn markdown_to_html(text: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(text, opts);
    let mut output = String::with_capacity(text.len() * 2);
    html::push_html(&mut output, parser);
    output
}

// ── Math conversion ──────────────────────────────────────────────────────────

/// Convert math in an HTML string, skipping `<code>` and `<pre>` blocks so that
/// inline code examples (e.g. `` `$x$` ``) are left untouched.
fn convert_math_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;

    while !rest.is_empty() {
        // Find the nearest code/pre block
        let code_pos = rest.find("<code");
        let pre_pos = rest.find("<pre");
        let skip_pos = match (code_pos, pre_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };

        match skip_pos {
            None => {
                result.push_str(&convert_math(rest));
                break;
            }
            Some(pos) => {
                // Convert math in text that precedes this code/pre block
                result.push_str(&convert_math(&rest[..pos]));
                rest = &rest[pos..];

                let close = if rest.starts_with("<pre") {
                    "</pre>"
                } else {
                    "</code>"
                };
                // Skip past the opening tag, then find the closing tag
                let after_open = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
                match rest[after_open..].find(close) {
                    None => {
                        result.push_str(rest);
                        break;
                    }
                    Some(rel) => {
                        let end = after_open + rel + close.len();
                        result.push_str(&rest[..end]);
                        rest = &rest[end..];
                    }
                }
            }
        }
    }

    result
}

/// Convert Bear math delimiters to Anki/MathJax delimiters.
/// $$...$$ (multiline)  → \[...\]   (display math)
/// $$...$$  (single line) → \(...\)  (inline math)
/// $...$                → \(...\)   (inline math)
fn convert_math(text: &str) -> String {
    // Pass 1: replace $$...$$ — must run before single-$ pass to avoid double-processing
    let text = replace_math_delimiters(text, "$$", "$$", |content| {
        if content.contains('\n') {
            format!("\\[{content}\\]")
        } else {
            format!("\\({content}\\)")
        }
    });

    // Pass 2: replace $...$ (single dollar, inline only)
    replace_math_delimiters(&text, "$", "$", |content| format!("\\({content}\\)"))
}

/// Generic delimiter replacer. Finds `open`...`close` pairs and calls `render` on the content.
/// Unclosed delimiters are passed through as-is.
fn replace_math_delimiters(
    text: &str,
    open: &str,
    close: &str,
    render: impl Fn(&str) -> String,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(start) = remaining.find(open) {
        result.push_str(&remaining[..start]);
        remaining = &remaining[start + open.len()..];

        if let Some(end) = remaining.find(close) {
            let content = &remaining[..end];
            result.push_str(&render(content));
            remaining = &remaining[end + close.len()..];
        } else {
            // Unclosed — emit the delimiter and continue
            result.push_str(open);
        }
    }
    result.push_str(remaining);
    result
}

// ── Image processing ─────────────────────────────────────────────────────────

/// Uploads images referenced in `text` to Anki and replaces `![alt](filename)` with
/// `<img src="bear_filename" alt="alt">` tags.
///
/// Already-uploaded images are served from `upload_cache` to avoid re-uploading
/// the same file for multiple cards in one sync.
fn process_images(
    text: &str,
    image_files: &[NoteImage],
    client: &AnkiClient,
    upload_cache: &mut HashMap<String, String>,
) -> Result<String> {
    if image_files.is_empty() {
        return Ok(text.to_owned());
    }

    let mut result = text.to_owned();
    for image in image_files {
        let filename = image.filename.as_str();
        let Some(md_pattern) = markdown_image_pattern(&result, filename) else {
            continue;
        };

        let anki_filename = if let Some(cached) = upload_cache.get(image.upload_key.as_str()) {
            cached.clone()
        } else {
            match upload_image(image, client) {
                Ok(name) => {
                    upload_cache.insert(image.upload_key.clone(), name.clone());
                    name
                }
                Err(err) => {
                    eprintln!("bear-anki: failed to upload image {filename}: {err}");
                    continue;
                }
            }
        };

        result = replace_md_image_with_pattern(&result, &md_pattern, &anki_filename);
    }

    Ok(result)
}

pub fn referenced_images<'a>(text: &str, image_files: &'a [NoteImage]) -> Vec<&'a NoteImage> {
    image_files
        .iter()
        .filter(|image| markdown_image_pattern(text, &image.filename).is_some())
        .collect()
}

fn markdown_image_pattern(text: &str, filename: &str) -> Option<String> {
    // Bear percent-encodes filenames in Markdown links (e.g. "my image.png" -> "my%20image.png").
    // Build the encoded pattern first; fall back to the raw filename if unencoded.
    let encoded_pat = format!("]({})", percent_encode_filename(filename));
    if text.contains(&encoded_pat) {
        return Some(encoded_pat);
    }

    let raw_pat = format!("]({filename})");
    text.contains(&raw_pat).then_some(raw_pat)
}

/// Replace every `![alt](…pattern…)` in `text` with an `<img>` tag.
/// The alt text between `![` and `](` is preserved as the `alt` attribute.
/// Falls back gracefully when no matching `![` precedes the pattern.
fn replace_md_image_with_pattern(text: &str, close: &str, anki_src: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(close_pos) = remaining.find(close) {
        let before = &remaining[..close_pos];
        if let Some(open_pos) = before.rfind("![") {
            let alt = &before[open_pos + 2..];
            result.push_str(&remaining[..open_pos]);
            if alt.is_empty() {
                result.push_str(&format!("<img src=\"{anki_src}\">"));
            } else {
                result.push_str(&format!(
                    "<img src=\"{anki_src}\" alt=\"{}\">",
                    escape_html_attr(alt)
                ));
            }
        } else {
            // No matching ![ — leave the ](filename) in place
            result.push_str(&remaining[..close_pos + close.len()]);
        }
        remaining = &remaining[close_pos + close.len()..];
    }
    result.push_str(remaining);
    result
}

/// Escape characters that are unsafe inside an HTML attribute value.
fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode characters that Bear encodes when embedding filenames in Markdown links.
fn percent_encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(c),
        }
    }
    out
}

fn upload_image(image: &NoteImage, client: &AnkiClient) -> Result<String> {
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&image.data);
    let filename = image.anki_filename();
    client.store_media_file(&filename, &base64_data)?;
    Ok(filename)
}

fn short_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(&digest[..8])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        convert_math, convert_math_html, escape_html_attr, percent_encode_filename,
        referenced_images, replace_md_image_with_pattern, NoteImage,
    };

    // Thin wrapper used only in tests — mirrors what process_images does per image.
    fn replace_md_image(text: &str, filename: &str, anki_src: &str) -> String {
        let close = format!("]({})", filename);
        replace_md_image_with_pattern(text, &close, anki_src)
    }

    #[test]
    fn converts_display_math() {
        let input = "Before\n$$\nE = mc^2\n$$\nAfter";
        let output = convert_math(input);
        assert!(output.contains("\\["), "should have display math open");
        assert!(output.contains("\\]"), "should have display math close");
        assert!(!output.contains("$$"));
    }

    #[test]
    fn converts_inline_double_dollar() {
        let output = convert_math("The formula $$E=mc^2$$ is famous.");
        assert_eq!(output, "The formula \\(E=mc^2\\) is famous.");
    }

    #[test]
    fn converts_inline_single_dollar() {
        let output = convert_math("Let $x = 5$ be given.");
        assert_eq!(output, "Let \\(x = 5\\) be given.");
    }

    #[test]
    fn double_dollar_takes_priority_over_single() {
        // $$...$$ should not be split into two $...$ matches
        let output = convert_math("$$a + b$$");
        assert_eq!(output, "\\(a + b\\)");
    }

    #[test]
    fn unclosed_delimiter_passes_through() {
        let output = convert_math("unclosed $expression");
        assert!(output.contains('$'));
    }

    #[test]
    fn no_math_passes_through_unchanged() {
        let input = "No math here, just text with a price tag $5.";
        // Single $ with no closing $ — passes through
        let output = convert_math(input);
        assert!(output.contains('$'));
    }

    #[test]
    fn convert_math_html_skips_code_spans() {
        let html = "<p>See <code>$x$</code> and $y$</p>";
        let out = convert_math_html(html);
        assert!(
            out.contains("<code>$x$</code>"),
            "code span should be unchanged"
        );
        assert!(
            out.contains("\\(y\\)"),
            "math outside code should be converted"
        );
    }

    #[test]
    fn convert_math_html_skips_pre_blocks() {
        let html = "<pre><code>$$block$$</code></pre><p>$x$</p>";
        let out = convert_math_html(html);
        assert!(out.contains("$$block$$"), "pre block should be unchanged");
        assert!(
            out.contains("\\(x\\)"),
            "math outside pre should be converted"
        );
    }

    #[test]
    fn replaces_markdown_image_no_alt() {
        let out = replace_md_image("See ![](photo.png) here", "photo.png", "bear_photo.png");
        assert_eq!(out, "See <img src=\"bear_photo.png\"> here");
    }

    #[test]
    fn replaces_markdown_image_preserves_alt() {
        let out = replace_md_image(
            "See ![a fluffy cat](photo.png) here",
            "photo.png",
            "bear_photo.png",
        );
        assert_eq!(
            out,
            "See <img src=\"bear_photo.png\" alt=\"a fluffy cat\"> here"
        );
    }

    #[test]
    fn alt_text_special_chars_are_escaped() {
        let out = replace_md_image(r#"![<diagram> "A&B"](img.png)"#, "img.png", "bear_img.png");
        assert_eq!(
            out,
            "<img src=\"bear_img.png\" alt=\"&lt;diagram&gt; &quot;A&amp;B&quot;\">"
        );
    }

    #[test]
    fn leaves_unrelated_text_unchanged() {
        let out = replace_md_image("no image here", "photo.png", "bear_photo.png");
        assert_eq!(out, "no image here");
    }

    #[test]
    fn replaces_multiple_occurrences() {
        let out = replace_md_image("![](a.png) and ![](a.png)", "a.png", "bear_a.png");
        assert_eq!(out, "<img src=\"bear_a.png\"> and <img src=\"bear_a.png\">");
    }

    #[test]
    fn percent_encode_encodes_space() {
        assert_eq!(percent_encode_filename("my image.png"), "my%20image.png");
    }

    #[test]
    fn percent_encode_encodes_special_chars() {
        assert_eq!(percent_encode_filename("a%b#c?.png"), "a%25b%23c%3F.png");
    }

    #[test]
    fn percent_encode_leaves_plain_names_unchanged() {
        assert_eq!(percent_encode_filename("photo.png"), "photo.png");
    }

    #[test]
    fn replace_md_image_handles_percent_encoded_filename() {
        // Bear writes "![](my%20image.png)" in markdown for a file named "my image.png"
        let out = replace_md_image(
            "See ![](my%20image.png) here",
            "my%20image.png",
            "bear_my image.png",
        );
        assert_eq!(out, "See <img src=\"bear_my image.png\"> here");
    }

    #[test]
    fn referenced_images_finds_raw_and_percent_encoded_filenames() {
        let images = vec![
            NoteImage::new(
                "my image.png".into(),
                "note:image-1".into(),
                b"one".to_vec(),
            ),
            NoteImage::new("other.png".into(), "note:image-2".into(), b"two".to_vec()),
        ];

        let refs = referenced_images("See ![](my%20image.png)", &images);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "my image.png");
    }

    #[test]
    fn anki_filenames_include_attachment_identity_hash() {
        let a = NoteImage::new("image.png".into(), "note-a:image".into(), b"same".to_vec());
        let b = NoteImage::new("image.png".into(), "note-b:image".into(), b"same".to_vec());
        assert_ne!(a.anki_filename(), b.anki_filename());
        assert!(a.anki_filename().starts_with("bear_"));
        assert!(a.anki_filename().ends_with("_image.png"));
    }

    #[test]
    fn escape_html_attr_handles_all_special_chars() {
        assert_eq!(
            escape_html_attr("\"A\" & <B>"),
            "&quot;A&quot; &amp; &lt;B&gt;"
        );
    }

    #[test]
    fn escape_html_attr_leaves_plain_text_unchanged() {
        assert_eq!(escape_html_attr("plain text"), "plain text");
    }
}
