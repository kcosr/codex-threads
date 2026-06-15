use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use crate::tui::state::{MessageColor, MessageSpan};

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

pub(crate) fn highlight_code_lines(
    language: Option<&str>,
    code: &str,
) -> Option<Vec<Vec<MessageSpan>>> {
    if code.len() > MAX_HIGHLIGHT_BYTES || code.lines().count() > MAX_HIGHLIGHT_LINES {
        return None;
    }
    let syntax_set = syntax_set();
    let syntax = syntax_for_language(syntax_set, language);
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = highlighter.highlight_line(line, syntax_set).ok()?;
        let spans = ranges
            .into_iter()
            .map(|(style, text)| MessageSpan {
                text: text.trim_end_matches(['\n', '\r']).to_string(),
                color: message_color(style.foreground),
                bold: style.font_style.contains(FontStyle::BOLD),
                italic: style.font_style.contains(FontStyle::ITALIC),
            })
            .filter(|span| !span.text.is_empty())
            .collect::<Vec<_>>();
        lines.push(spans);
    }
    Some(lines)
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(EmbeddedThemeName::CatppuccinMocha)
            .clone()
    })
}

fn syntax_for_language<'a>(
    syntax_set: &'a SyntaxSet,
    language: Option<&str>,
) -> &'a SyntaxReference {
    language
        .and_then(normalize_language)
        .and_then(|language| {
            syntax_set
                .find_syntax_by_token(language)
                .or_else(|| syntax_set.find_syntax_by_name(language))
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn normalize_language(language: &str) -> Option<&str> {
    let language = language.split_whitespace().next()?.trim();
    if language.is_empty() {
        return None;
    }
    Some(match language {
        "js" => "javascript",
        "rs" => "rust",
        "sh" | "shell" => "bash",
        "ts" => "typescript",
        "py" => "python",
        other => other,
    })
}

fn message_color(color: SyntectColor) -> Option<MessageColor> {
    // syntect uses alpha 0 as the "use the default foreground" sentinel; any
    // other alpha is an explicit theme color (terminals ignore alpha, so the
    // RGB triple is what matters).
    if color.a == 0x00 {
        return None;
    }
    Some(MessageColor::Rgb(color.r, color.g, color.b))
}
