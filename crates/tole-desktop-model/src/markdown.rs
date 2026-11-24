//! The Markdown subset of an answer, as plain data the window draws (#1031).
//!
//! Tole's answers are Markdown, written by a model, so they are drawn by this
//! small parser and not by anything that could do more than draw:
//!
//! - Blocks: paragraphs, headings (`#` to `######`), bullet and numbered lists
//!   (nesting by indent), fenced code, quotes and a horizontal rule. A table,
//!   an image or raw HTML is not understood and stays as the text it is.
//! - Inline: `code`, `**bold**`, `*italic*` and `_italic_`, and `[label](url)`.
//! - A link is never opened and never drawn as one: it reads "label (url)".
//!   An answer cannot make the window go anywhere or fetch anything.
//! - Nothing is lost: text this parser does not understand is kept as text,
//!   an unclosed marker is its own characters, and the input is the answer as
//!   the transcript made it printable (`plain::printable_text`).

/// How a run of text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    /// Inline code (a path, a command).
    pub code: bool,
}

/// A run of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One item of a list; `depth` is 0 for the outer list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub depth: u8,
    /// The number shown (`1.`), or `None` for a bullet.
    pub number: Option<u32>,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    Heading { level: u8, spans: Vec<Span> },
    List(Vec<ListItem>),
    Code(String),
    Quote(Vec<Span>),
    Rule,
}

/// Deepest nesting a list keeps; deeper items are drawn at this depth.
pub const MAX_DEPTH: u8 = 4;

/// Parse `source` into blocks. Never fails: every line ends up in a block.
pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut list: Vec<ListItem> = Vec::new();
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush(&mut blocks, &mut paragraph, &mut list);
            let mut code = Vec::new();
            for inner in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
                code.push(inner);
            }
            blocks.push(Block::Code(code.join("\n")));
        } else if trimmed.is_empty() {
            flush(&mut blocks, &mut paragraph, &mut list);
        } else if let Some(item) = list_item(line) {
            if !paragraph.is_empty() {
                flush(&mut blocks, &mut paragraph, &mut list);
            }
            list.push(item);
        } else if !list.is_empty() && line.starts_with(' ') && paragraph.is_empty() {
            // A wrapped line of the last item.
            let last = list.last_mut().expect("a list has an item");
            last.spans.push(Span {
                text: " ".into(),
                style: Style::default(),
            });
            last.spans.extend(inline(trimmed));
        } else if let Some((level, text)) = heading(trimmed) {
            flush(&mut blocks, &mut paragraph, &mut list);
            blocks.push(Block::Heading {
                level,
                spans: inline(text),
            });
        } else if is_rule(trimmed) {
            flush(&mut blocks, &mut paragraph, &mut list);
            blocks.push(Block::Rule);
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            flush(&mut blocks, &mut paragraph, &mut list);
            blocks.push(Block::Quote(inline(quote.trim_start())));
        } else {
            if !list.is_empty() {
                flush(&mut blocks, &mut paragraph, &mut list);
            }
            paragraph.push(trimmed);
        }
    }
    flush(&mut blocks, &mut paragraph, &mut list);
    blocks
}

fn flush(blocks: &mut Vec<Block>, paragraph: &mut Vec<&str>, list: &mut Vec<ListItem>) {
    if !paragraph.is_empty() {
        blocks.push(Block::Paragraph(inline(&paragraph.join(" "))));
        paragraph.clear();
    }
    if !list.is_empty() {
        blocks.push(Block::List(std::mem::take(list)));
    }
}

/// `# Title`: one to six marks, a space, the text.
fn heading(line: &str) -> Option<(u8, &str)> {
    let marks = line.chars().take_while(|c| *c == '#').count();
    let rest = line.get(marks..)?;
    ((1..=6).contains(&marks) && rest.starts_with(' ')).then(|| (marks as u8, rest.trim()))
}

fn is_rule(line: &str) -> bool {
    let mut chars = line.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_')
        && line.matches(first).count() >= 3
        && chars.all(|c| c == first)
}

/// `- item`, `* item`, `+ item` or `1. item`, indented by two spaces a level.
fn list_item(line: &str) -> Option<ListItem> {
    let body = line.trim_start();
    let indent = line.len() - body.len();
    let depth = u8::try_from(indent / 2).map_or(MAX_DEPTH, |d| d.min(MAX_DEPTH));
    if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|m| body.strip_prefix(m)) {
        if is_rule(body) {
            return None;
        }
        return Some(ListItem {
            depth,
            number: None,
            spans: inline(rest.trim()),
        });
    }
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let (number, rest) = body.split_at(digits);
    let rest = rest
        .strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))?;
    Some(ListItem {
        depth,
        number: Some(number.parse().ok().filter(|_| digits <= 9)?),
        spans: inline(rest.trim()),
    })
}

/// Inline styles of one line of text.
pub fn inline(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let push = |out: &mut Vec<Span>, text: String, style: Style| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&text),
            _ => out.push(Span { text, style }),
        }
    };
    while i < chars.len() {
        let rest = &chars[i..];
        let found = match rest[0] {
            '`' => close(rest, 1, &['`']).map(|n| {
                (
                    n,
                    1,
                    Style {
                        code: true,
                        ..Style::default()
                    },
                )
            }),
            '*' if rest.get(1) == Some(&'*') => close(rest, 2, &['*', '*']).map(|n| {
                (
                    n,
                    2,
                    Style {
                        bold: true,
                        ..Style::default()
                    },
                )
            }),
            '*' | '_' => close(rest, 1, &[rest[0]])
                .filter(|_| word_edge(&chars, i, rest[0]))
                .map(|n| {
                    (
                        n,
                        1,
                        Style {
                            italic: true,
                            ..Style::default()
                        },
                    )
                }),
            _ => None,
        };
        if let Some((end, open, style)) = found {
            let inner: String = rest[open..end].iter().collect();
            push(&mut out, std::mem::take(&mut plain), Style::default());
            if style.code {
                push(&mut out, inner, style);
            } else {
                // The inside is read again: it may hold code, a link or a second style.
                for span in inline(&inner) {
                    push(
                        &mut out,
                        span.text,
                        Style {
                            bold: style.bold,
                            italic: style.italic,
                            ..span.style
                        },
                    );
                }
            }
            i += end + open;
        } else if rest[0] == '[' {
            if let Some((label, url, used)) = link(rest) {
                plain.push_str(&label);
                if url != label {
                    plain.push_str(&format!(" ({url})"));
                }
                i += used;
            } else {
                plain.push('[');
                i += 1;
            }
        } else {
            plain.push(rest[0]);
            i += 1;
        }
    }
    push(&mut out, plain, Style::default());
    out
}

/// Where the closing `marker` of an opener `open` characters long is, as an
/// index into `rest`; the text between must not be empty.
fn close(rest: &[char], open: usize, marker: &[char]) -> Option<usize> {
    let body = rest.get(open..)?;
    let at = body
        .windows(marker.len())
        .position(|w| w == marker)
        .filter(|at| *at > 0)?;
    Some(open + at)
}

/// An emphasis mark opens at a word edge and does not sit in the middle of a
/// word (`snake_case_name` stays as it is) or next to a space.
fn word_edge(chars: &[char], at: usize, mark: char) -> bool {
    let before = at.checked_sub(1).map(|b| chars[b]);
    let after = chars.get(at + 1).copied();
    let glued = before.is_some_and(char::is_alphanumeric) && mark == '_';
    !glued && after.is_some_and(|c| !c.is_whitespace())
}

/// Longest label (characters) and longest address a link is looked for in;
/// anything longer stays as the text it is.
pub const MAX_LABEL: usize = 200;
pub const MAX_URL: usize = 2_048;

/// `[label](url)` at the start of `rest`: the label, the url and the
/// characters used.
fn link(rest: &[char]) -> Option<(String, String, usize)> {
    // Both scans are bounded: a model that writes 200,000 `[` must not make
    // every one of them read to the end of the answer.
    let label_end = rest.iter().take(MAX_LABEL + 2).position(|c| *c == ']')?;
    if rest.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let after = &rest[label_end + 2..];
    let url_end = after.iter().take(MAX_URL + 1).position(|c| *c == ')')? + label_end + 2;
    let label: String = rest[1..label_end].iter().collect();
    let url: String = rest[label_end + 2..url_end].iter().collect();
    (!label.is_empty() && !url.contains(' ')).then_some((label, url, url_end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn the_board_answer_is_a_paragraph_a_list_and_a_closing_paragraph() {
        let blocks = parse(
            "I found 36 invoices. My plan:\n\n- Make 6 folders\n- Copy each invoice\n  into its month\n\nCopying is done by a program.",
        );
        assert_eq!(blocks.len(), 3);
        let Block::List(items) = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(text(&items[1].spans), "Copy each invoice into its month");
        assert!(matches!(&blocks[2], Block::Paragraph(p) if text(p).starts_with("Copying")));
    }

    #[test]
    fn inline_styles_nest_and_an_unclosed_marker_is_its_own_text() {
        let spans = inline("a **bold `x` word** and *it* and `code`");
        assert_eq!(text(&spans), "a bold x word and it and code");
        assert!(spans
            .iter()
            .any(|s| s.style.bold && s.style.code && s.text == "x"));
        assert!(spans.iter().any(|s| s.style.italic && s.text == "it"));
        assert_eq!(text(&inline("2 * 3 and **open")), "2 * 3 and **open");
        assert_eq!(text(&inline("snake_case_name")), "snake_case_name");
        assert_eq!(text(&inline("`` and []( x")), "`` and []( x");
    }

    #[test]
    fn a_link_is_looked_for_only_within_its_limits() {
        let near = |n: usize| "x".repeat(n);
        let link_of = |label: usize, url: usize| {
            let src = format!("[{}]({})", near(label), near(url));
            let chars: Vec<char> = src.chars().collect();
            link(&chars).map(|(_, _, used)| used == chars.len())
        };
        assert_eq!(
            link_of(MAX_LABEL, MAX_URL),
            Some(true),
            "the longest that links"
        );
        assert_eq!(link_of(MAX_LABEL + 1, 5), None, "a label too long is text");
        assert_eq!(link_of(5, MAX_URL + 1), None, "an address too long is text");
        // The closing marks are there, just too far: the answer keeps every character.
        let far = format!("[{}](u)", near(MAX_LABEL + 50));
        assert_eq!(text(&inline(&far)), far);
    }

    #[test]
    fn hostile_answers_are_kept_whole_and_their_scans_are_bounded() {
        // What made the link scan read to the end of the answer from every `[`.
        let n = 20_000;
        let hostile = [
            "[".repeat(n),
            "[a](b".repeat(n / 5),
            "\u{e9}[".repeat(n / 2),
            "[a]".repeat(n / 3),
        ];
        for answer in hostile {
            assert_eq!(text(&inline(&answer)), answer, "nothing lost");
            assert_eq!(parse(&answer).len(), 1);
        }
    }

    #[test]
    fn a_link_is_text_with_its_address_and_never_a_link() {
        assert_eq!(
            text(&inline("see [docs](https://a.example/x)")),
            "see docs (https://a.example/x)"
        );
        assert_eq!(
            text(&inline("[https://a.example](https://a.example)")),
            "https://a.example"
        );
        // Not a link: a space in the address.
        assert_eq!(text(&inline("[a](b c)")), "[a](b c)");
        let all = parse("[x](javascript:alert(1))");
        assert!(matches!(&all[0], Block::Paragraph(p) if text(p).contains("javascript:")));
    }

    #[test]
    fn code_blocks_keep_every_line_and_an_unclosed_fence_takes_the_rest() {
        let blocks = parse("run:\n```sh\nls -la\n\n  cd x\n```\ndone\n```\nnever closed");
        assert_eq!(blocks[1], Block::Code("ls -la\n\n  cd x".into()));
        assert!(matches!(&blocks[2], Block::Paragraph(p) if text(p) == "done"));
        assert_eq!(blocks[3], Block::Code("never closed".into()));
    }

    #[test]
    fn headings_numbered_lists_quotes_and_rules() {
        let blocks = parse("## Plan\n1. one\n2) two\n   - nested\n> quoted *text*\n---\n#nospace");
        assert!(matches!(&blocks[0], Block::Heading { level: 2, spans } if text(spans) == "Plan"));
        let Block::List(items) = &blocks[1] else {
            panic!("{blocks:?}")
        };
        assert_eq!(
            items.iter().map(|i| i.number).collect::<Vec<_>>(),
            [Some(1), Some(2), None]
        );
        assert_eq!(items[2].depth, 1);
        assert!(matches!(&blocks[2], Block::Quote(q) if text(q) == "quoted text"));
        assert_eq!(blocks[3], Block::Rule);
        assert!(matches!(&blocks[4], Block::Paragraph(p) if text(p) == "#nospace"));
    }

    #[test]
    fn html_and_tables_stay_as_the_text_they_are_and_nothing_is_lost() {
        let blocks = parse("<script>alert(1)</script>\n| a | b |\n|---|---|");
        let Block::Paragraph(p) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(text(p), "<script>alert(1)</script> | a | b | |---|---|");
        assert!(parse("").is_empty() && parse("\n\n  \n").is_empty());
        let deep = format!("{}- x", " ".repeat(40));
        let Block::List(items) = &parse(&deep)[0] else {
            panic!("a list")
        };
        assert_eq!(items[0].depth, MAX_DEPTH);
    }
}
