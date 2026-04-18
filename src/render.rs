use std::path::PathBuf;

use anyhow::Result;
use bear_cli::config::encode_file;
use pulldown_cmark::{Options, Parser, html};

use crate::anki::AnkiClient;

/// Render card text for Anki:
/// 1. Upload image attachments and replace `![](filename)` with `<img>` tags
/// 2. Render markdown to HTML
/// 3. Convert Bear math syntax (`$...$`, `$$...$$`) to MathJax delimiters
///    (runs on the HTML output so `\(` is never processed as a markdown escape)
///
/// `image_files` — list of `(filename, path)` for images attached to this note,
/// obtained via `BearDb::note_files`.
pub fn render_for_anki(
    text: &str,
    image_files: &[(String, PathBuf)],
    client: &AnkiClient,
) -> Result<String> {
    let text = process_images(text, image_files, client)?;
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

/// Uploads each image in `image_files` to Anki and replaces references in `text`
/// (`![...](filename)` or bare filename) with `<img src="bear_{filename}">` tags.
fn process_images(
    text: &str,
    image_files: &[(String, PathBuf)],
    client: &AnkiClient,
) -> Result<String> {
    if image_files.is_empty() {
        return Ok(text.to_owned());
    }

    let mut result = text.to_owned();
    for (filename, path) in image_files {
        let md_pattern = format!("]({})", filename);
        if !result.contains(md_pattern.as_str()) {
            continue;
        }

        let anki_filename = match upload_image(path, client) {
            Ok(name) => name,
            Err(err) => {
                eprintln!("bear-anki: failed to upload image {filename}: {err}");
                continue;
            }
        };

        let img_tag = format!("<img src=\"{anki_filename}\">");
        result = replace_md_image(&result, filename, &img_tag);
    }

    Ok(result)
}

/// Replace `![alt](filename)` (any alt text) with `img_tag`.
/// Falls back gracefully if no matching `![` is found before `](filename)`.
fn replace_md_image(text: &str, filename: &str, img_tag: &str) -> String {
    let close = format!("]({})", filename);
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(close_pos) = remaining.find(close.as_str()) {
        let before = &remaining[..close_pos];
        if let Some(open_pos) = before.rfind("![") {
            result.push_str(&remaining[..open_pos]);
            result.push_str(img_tag);
        } else {
            // No matching ![  — leave the ](filename) in place
            result.push_str(&remaining[..close_pos + close.len()]);
        }
        remaining = &remaining[close_pos + close.len()..];
    }
    result.push_str(remaining);
    result
}

fn upload_image(path: &std::path::Path, client: &AnkiClient) -> Result<String> {
    let base64_data = encode_file(path)?;
    let filename = format!("bear_{}", path.file_name().unwrap().to_string_lossy());
    client.store_media_file(&filename, &base64_data)?;
    Ok(filename)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{convert_math, convert_math_html};

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

    use super::replace_md_image;

    #[test]
    fn replaces_markdown_image_no_alt() {
        let out = replace_md_image("See ![](photo.png) here", "photo.png", "<img src=\"x\">");
        assert_eq!(out, "See <img src=\"x\"> here");
    }

    #[test]
    fn replaces_markdown_image_with_alt() {
        let out = replace_md_image(
            "See ![a cat](photo.png) here",
            "photo.png",
            "<img src=\"x\">",
        );
        assert_eq!(out, "See <img src=\"x\"> here");
    }

    #[test]
    fn leaves_unrelated_text_unchanged() {
        let out = replace_md_image("no image here", "photo.png", "<img src=\"x\">");
        assert_eq!(out, "no image here");
    }

    #[test]
    fn replaces_multiple_occurrences() {
        let out = replace_md_image("![](a.png) and ![](a.png)", "a.png", "<img>");
        assert_eq!(out, "<img> and <img>");
    }
}
