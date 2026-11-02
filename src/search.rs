//! Text matching shared by the editor and viewer: plain, whole word, fuzzy,
//! regular expression and style-preserving search. Each line is searched on
//! its own, and all positions are character indexes.

use fancy_regex::Regex;
use std::collections::HashMap;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// The options in the search and replace dialogs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub text: String,
    pub replace: String,
    pub case_sensitive: bool,
    pub whole_words: bool,
    pub fuzzy: bool,
    pub regex: bool,
    pub preserve_style: bool,
}

/// A match within one line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hit {
    pub start: usize,
    pub end: usize,
    groups: Vec<Option<String>>,
    names: HashMap<String, String>,
    tokens: Vec<String>,
    delims: Vec<String>,
}

enum Kind {
    Plain { needle: Vec<char> },
    Regex(Box<Regex>),
    Style { tokens: Vec<String> },
}

/// A compiled search.
pub struct Matcher {
    kind: Kind,
    opts: SearchOptions,
}

/// True for characters that make up a word.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Splits a pattern in Far's `/regexp/options` form, if it uses that form.
fn split_slashes(pattern: &str) -> Option<(&str, &str)> {
    let rest = pattern.strip_prefix('/')?;
    let end = rest.rfind('/')?;
    let (inner, options) = (&rest[..end], &rest[end + 1..]);
    let escaped = inner.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1;
    let valid = options.chars().all(|c| "ismx".contains(c));
    (!escaped && valid).then_some((inner, options))
}

/// A group name usable by the regex engine. Far also allows spaces.
fn group_name(name: &str) -> String {
    name.chars()
        .map(|c| if is_word_char(c) { c } else { '_' })
        .collect()
}

/// Translates Far's regular expression syntax to the engine's syntax.
pub fn translate_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            i += 2;
            match next {
                'i' => out.push_str(r"\p{L}"),
                'I' => out.push_str(r"\P{L}"),
                'l' => out.push_str(r"\p{Ll}"),
                'L' => out.push_str(r"\P{Ll}"),
                'u' => out.push_str(r"\p{Lu}"),
                'U' => out.push_str(r"\P{Lu}"),
                'Z' => out.push_str(r"\z"),
                // The no-return point only speeds up matching.
                'O' => {}
                'Q' => {
                    while i < chars.len() {
                        if chars[i] == '\\' && chars.get(i + 1) == Some(&'E') {
                            i += 2;
                            break;
                        }
                        let literal = chars[i];
                        if !literal.is_alphanumeric() && !literal.is_whitespace() {
                            out.push('\\');
                        }
                        out.push(literal);
                        i += 1;
                    }
                }
                'x' => {
                    let digits: String = chars[i..]
                        .iter()
                        .take(4)
                        .take_while(|d| d.is_ascii_hexdigit())
                        .collect();
                    i += digits.len();
                    out.push_str(&format!(r"\x{{{digits}}}"));
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            continue;
        }
        if in_class {
            in_class = c != ']';
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_class = true;
                out.push(c);
                // A leading `]` or `^]` is a literal, not the end of the class.
                if chars.get(i + 1) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                if chars.get(i + 1) == Some(&']') {
                    out.push_str(r"\]");
                    i += 1;
                }
            }
            '(' if chars.get(i + 1) == Some(&'?') && chars.get(i + 2) == Some(&'{') => {
                let name: String = chars[i + 3..].iter().take_while(|n| **n != '}').collect();
                out.push_str(&format!("(?P<{}>", group_name(&name)));
                i += 3 + name.chars().count();
            }
            '{' if chars.get(i + 1) == Some(&',') => out.push_str("{0"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

fn fold_char(c: char, opts: &SearchOptions, out: &mut Vec<char>) {
    let mut push = |c: char| {
        if opts.case_sensitive {
            out.push(c);
        } else {
            out.extend(c.to_lowercase());
        }
    };
    if !opts.fuzzy {
        push(c);
        return;
    }
    // Letters that compatibility decomposition leaves alone.
    let special = match c {
        'Æ' => Some("AE"),
        'æ' => Some("ae"),
        'Œ' => Some("OE"),
        'œ' => Some("oe"),
        'Ø' => Some("O"),
        'ø' => Some("o"),
        'ß' => Some("ss"),
        'Đ' => Some("D"),
        'đ' => Some("d"),
        'Ł' => Some("L"),
        'ł' => Some("l"),
        _ => None,
    };
    if let Some(text) = special {
        text.chars().for_each(&mut push);
        return;
    }
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf)
        .nfkd()
        .filter(|d| !is_combining_mark(*d))
        .for_each(&mut push);
}

/// Folds text, recording which original character each result came from.
fn fold(text: &str, opts: &SearchOptions) -> (Vec<char>, Vec<usize>) {
    let mut folded = Vec::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (index, c) in text.chars().enumerate() {
        let before = folded.len();
        fold_char(c, opts, &mut folded);
        origin.extend(std::iter::repeat_n(index, folded.len() - before));
    }
    (folded, origin)
}

/// Splits an identifier at delimiters and at camelCase boundaries.
pub fn split_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            previous = None;
            continue;
        }
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_numeric()) {
            tokens.push(std::mem::take(&mut current));
        }
        current.push(c);
        previous = Some(c);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Writes `word` in the style of `model`: UPPER, Capitalized or lower.
fn apply_style(word: &str, model: &str) -> String {
    let letters: Vec<char> = model.chars().filter(|c| c.is_alphabetic()).collect();
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    if letters.len() > 1 && upper == letters.len() {
        return word.to_uppercase();
    }
    let lower = word.to_lowercase();
    if letters.first().is_some_and(|c| c.is_uppercase()) {
        let mut chars = lower.chars();
        return match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => lower,
        };
    }
    lower
}

impl Matcher {
    /// Compiles the options, reporting an invalid regular expression.
    pub fn new(opts: &SearchOptions) -> Result<Self, String> {
        let kind = if opts.regex {
            let (pattern, flags) = split_slashes(&opts.text).unwrap_or((&opts.text, ""));
            let mut flags = flags.to_string();
            if !opts.case_sensitive && !flags.contains('i') {
                flags.push('i');
            }
            let prefix = if flags.is_empty() {
                String::new()
            } else {
                format!("(?{flags})")
            };
            let source = format!("{prefix}{}", translate_regex(pattern));
            let regex = Regex::new(&source).map_err(|e| format!("Bad regular expression: {e}"))?;
            Kind::Regex(Box::new(regex))
        } else if opts.preserve_style {
            Kind::Style {
                tokens: split_tokens(&opts.text)
                    .iter()
                    .map(|t| t.to_lowercase())
                    .collect(),
            }
        } else {
            Kind::Plain {
                needle: fold(&opts.text, opts).0,
            }
        };
        Ok(Matcher {
            kind,
            opts: opts.clone(),
        })
    }

    /// Finds the first match starting at or after `from`, or, going backward,
    /// the last match ending at or before `from`.
    pub fn find(&self, line: &str, from: usize, backward: bool) -> Option<Hit> {
        let mut found = None;
        let mut at = if backward { 0 } else { from };
        while let Some(hit) = self.find_forward(line, at) {
            if backward && hit.end > from {
                break;
            }
            at = if hit.end > hit.start {
                hit.end
            } else {
                hit.end + 1
            };
            if !backward {
                return Some(hit);
            }
            // Overlapping matches count going backward too.
            at = at.min(hit.start + 1);
            found = Some(hit);
        }
        found
    }

    fn whole_word_ok(&self, chars: &[char], start: usize, end: usize) -> bool {
        if !self.opts.whole_words {
            return true;
        }
        let before = start > 0 && is_word_char(chars[start - 1]);
        let after = end < chars.len() && is_word_char(chars[end]);
        !before && !after
    }

    fn find_forward(&self, line: &str, from: usize) -> Option<Hit> {
        match &self.kind {
            Kind::Plain { needle } => self.find_plain(line, from, needle),
            Kind::Regex(regex) => Self::find_regex(regex, line, from),
            Kind::Style { tokens } => self.find_style(line, from, tokens),
        }
    }

    fn find_plain(&self, line: &str, from: usize, needle: &[char]) -> Option<Hit> {
        if needle.is_empty() {
            return None;
        }
        let chars: Vec<char> = line.chars().collect();
        let (folded, origin) = fold(line, &self.opts);
        if folded.len() < needle.len() {
            return None;
        }
        for at in 0..=folded.len() - needle.len() {
            if origin[at] < from || (at > 0 && origin[at - 1] == origin[at]) {
                continue;
            }
            if folded[at..at + needle.len()] != *needle {
                continue;
            }
            let (start, end) = (origin[at], origin[at + needle.len() - 1] + 1);
            // The match must cover all of the last original character.
            let next = origin.get(at + needle.len());
            if next == Some(&(end - 1)) {
                continue;
            }
            if self.whole_word_ok(&chars, start, end) {
                return Some(Hit {
                    start,
                    end,
                    ..Default::default()
                });
            }
        }
        None
    }

    fn find_regex(regex: &Regex, line: &str, from: usize) -> Option<Hit> {
        let byte_from = line
            .char_indices()
            .nth(from)
            .map(|(b, _)| b)
            .or_else(|| (from == line.chars().count()).then_some(line.len()))?;
        let captures = regex.captures_from_pos(line, byte_from).ok()??;
        let whole = captures.get(0)?;
        let start = line[..whole.start()].chars().count();
        let end = start + whole.as_str().chars().count();
        let groups = (0..captures.len())
            .map(|i| captures.get(i).map(|m| m.as_str().to_string()))
            .collect();
        let names = regex
            .capture_names()
            .flatten()
            .filter_map(|name| {
                let text = captures.name(name)?.as_str().to_string();
                Some((name.to_string(), text))
            })
            .collect();
        Some(Hit {
            start,
            end,
            groups,
            names,
            ..Default::default()
        })
    }

    fn find_style(&self, line: &str, from: usize, tokens: &[String]) -> Option<Hit> {
        if tokens.is_empty() {
            return None;
        }
        let chars: Vec<char> = line.chars().collect();
        for start in from..chars.len() {
            if let Some(hit) = Self::match_style(&chars, start, tokens) {
                if self.whole_word_ok(&chars, hit.start, hit.end) {
                    return Some(hit);
                }
            }
        }
        None
    }

    fn match_style(chars: &[char], start: usize, tokens: &[String]) -> Option<Hit> {
        let mut at = start;
        let mut found = Vec::new();
        let mut delims = Vec::new();
        for (i, token) in tokens.iter().enumerate() {
            if i > 0 {
                let delimiter = chars.get(at).filter(|c| "_-. ".contains(**c));
                delims.push(delimiter.map_or(String::new(), |c| c.to_string()));
                at += usize::from(delimiter.is_some());
            }
            let length = token.chars().count();
            let piece: String = chars.get(at..at + length)?.iter().collect();
            if piece.to_lowercase() != *token {
                return None;
            }
            found.push(piece);
            at += length;
        }
        Some(Hit {
            start,
            end: at,
            tokens: found,
            delims,
            ..Default::default()
        })
    }

    /// The text that replaces a match.
    pub fn replacement(&self, hit: &Hit) -> String {
        match &self.kind {
            Kind::Plain { .. } => self.opts.replace.clone(),
            Kind::Regex(_) => expand_template(&self.opts.replace, hit),
            Kind::Style { .. } => self.style_replacement(hit),
        }
    }

    fn style_replacement(&self, hit: &Hit) -> String {
        let words = split_tokens(&self.opts.replace);
        let mut out = String::new();
        for (i, word) in words.iter().enumerate() {
            if i > 0 {
                let delimiter = hit.delims.get(i - 1).or(hit.delims.last());
                out.push_str(delimiter.map_or("", String::as_str));
            }
            let model = hit.tokens.get(i).or(hit.tokens.last());
            out.push_str(&apply_style(word, model.map_or("", String::as_str)));
        }
        out
    }
}

/// Expands `$0`..`$N` and `${name}` in a replacement.
fn expand_template(template: &str, hit: &Hit) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'{') {
            let name: String = chars[i + 2..].iter().take_while(|c| **c != '}').collect();
            let closed = chars.get(i + 2 + name.chars().count()) == Some(&'}');
            if closed {
                if let Some(text) = hit.names.get(&group_name(&name)) {
                    out.push_str(text);
                }
                i += 3 + name.chars().count();
                continue;
            }
        }
        let digits: String = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            out.push('$');
            i += 1;
            continue;
        }
        if let Some(Some(text)) = digits.parse::<usize>().ok().and_then(|n| hit.groups.get(n)) {
            out.push_str(text);
        }
        i += 1 + digits.len();
    }
    out
}

/// Parses hexadecimal search text such as `6C 6F` or `6c6f`.
pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.is_empty() || digits.len() % 2 != 0 {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let high = pair[0].to_digit(16)?;
            let low = pair[1].to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(text: &str) -> SearchOptions {
        SearchOptions {
            text: text.into(),
            ..Default::default()
        }
    }

    fn span(m: &Matcher, line: &str, from: usize, backward: bool) -> Option<(usize, usize)> {
        m.find(line, from, backward).map(|h| (h.start, h.end))
    }

    #[test]
    fn case_sensitivity_follows_the_option() {
        let line = "Foo foo";
        let any = Matcher::new(&opts("foo")).unwrap();
        assert_eq!(span(&any, line, 0, false), Some((0, 3)));
        let exact = Matcher::new(&SearchOptions {
            case_sensitive: true,
            ..opts("foo")
        })
        .unwrap();
        assert_eq!(span(&exact, line, 0, false), Some((4, 7)));
    }

    #[test]
    fn whole_words_reject_parts_of_words() {
        let m = Matcher::new(&SearchOptions {
            whole_words: true,
            ..opts("cat")
        })
        .unwrap();
        assert_eq!(span(&m, "concat cat_x cat", 0, false), Some((13, 16)));
        assert_eq!(span(&m, "concat", 0, false), None);
    }

    #[test]
    fn backward_search_finds_the_last_match_before_the_position() {
        let m = Matcher::new(&opts("ab")).unwrap();
        assert_eq!(span(&m, "ab ab ab", 8, true), Some((6, 8)));
        assert_eq!(span(&m, "ab ab ab", 7, true), Some((3, 5)));
        assert_eq!(span(&m, "ab ab ab", 1, true), None);
    }

    #[test]
    fn multibyte_positions_are_characters() {
        let m = Matcher::new(&opts("ЖЁ")).unwrap();
        assert_eq!(span(&m, "ааж ёжё", 0, false), Some((5, 7)));
    }

    #[test]
    fn fuzzy_search_matches_the_documented_cases() {
        let fuzzy = |text: &str| {
            Matcher::new(&SearchOptions {
                fuzzy: true,
                ..opts(text)
            })
            .unwrap()
        };
        assert_eq!(span(&fuzzy("deja"), "déjà vu", 0, false), Some((0, 4)));
        assert_eq!(span(&fuzzy("fluffy"), "ﬂuﬀy", 0, false), Some((0, 4)));
        assert_eq!(span(&fuzzy("42"), "④②", 0, false), Some((0, 2)));
        let exact = |text: &str| {
            Matcher::new(&SearchOptions {
                fuzzy: true,
                case_sensitive: true,
                ..opts(text)
            })
            .unwrap()
        };
        assert_eq!(span(&exact("Uber"), "Überwald", 0, false), Some((0, 4)));
        assert_eq!(span(&exact("Uber"), "überwald", 0, false), None);
        assert_eq!(span(&exact("Æther"), "AEther", 0, false), Some((0, 6)));
        assert_eq!(span(&exact("Æther"), "Aether", 0, false), None);
    }

    fn regex(text: &str) -> Matcher {
        Matcher::new(&SearchOptions {
            regex: true,
            case_sensitive: true,
            ..opts(text)
        })
        .unwrap()
    }

    #[test]
    fn regular_expressions_support_perl_features() {
        assert_eq!(span(&regex(r"\d+"), "ab 123", 0, false), Some((3, 6)));
        assert_eq!(
            span(&regex(r"foo(?!bar)"), "foobar foo", 0, false),
            Some((7, 10))
        );
        assert_eq!(span(&regex(r"(?<=x)y"), "y xy", 0, false), Some((3, 4)));
        assert_eq!(span(&regex(r"(a)\1"), "ab aa", 0, false), Some((3, 5)));
        assert_eq!(span(&regex(r"a.*?z"), "a--z--z", 0, false), Some((0, 4)));
    }

    #[test]
    fn far_syntax_is_translated() {
        assert_eq!(span(&regex("/ABC/i"), "xabc", 0, false), Some((1, 4)));
        assert_eq!(span(&regex(r"/a\/b/"), "a/b", 0, false), Some((0, 3)));
        assert_eq!(span(&regex(r"\u\l+"), "abc Def", 0, false), Some((4, 7)));
        assert_eq!(span(&regex(r"\i+"), "12 ab", 0, false), Some((3, 5)));
        assert_eq!(span(&regex(r"\Qa.b\E"), "axb a.b", 0, false), Some((4, 7)));
        assert_eq!(span(&regex(r"\x0041"), "xA", 0, false), Some((1, 2)));
        assert_eq!(span(&regex(r"a{,2}b"), "aab", 0, false), Some((0, 3)));
        assert_eq!(span(&regex(r"a\Ob"), "ab", 0, false), Some((0, 2)));
        assert_eq!(span(&regex(r"b\Z"), "b b", 0, false), Some((2, 3)));
    }

    #[test]
    fn regex_replacement_expands_groups_and_names() {
        let m = Matcher::new(&SearchOptions {
            regex: true,
            replace: "$2-$1 $0 ${word} $9 $x".into(),
            ..opts(r"(\d)(?{word}\d)")
        })
        .unwrap();
        let hit = m.find("12", 0, false).unwrap();
        assert_eq!(m.replacement(&hit), "2-1 12 2  $x");
    }

    #[test]
    fn group_names_may_contain_spaces() {
        let m = Matcher::new(&SearchOptions {
            regex: true,
            replace: "${my name}".into(),
            ..opts(r"(?{my name}\d)")
        })
        .unwrap();
        let hit = m.find("7", 0, false).unwrap();
        assert_eq!(m.replacement(&hit), "7");
    }

    #[test]
    fn invalid_regex_is_reported_not_a_panic() {
        let bad = SearchOptions {
            regex: true,
            ..opts("(unclosed")
        };
        assert!(Matcher::new(&bad).is_err());
    }

    #[test]
    fn empty_matches_do_not_stall_the_search() {
        let m = regex("^");
        assert_eq!(span(&m, "abc", 0, false), Some((0, 0)));
        assert_eq!(span(&m, "abc", 1, false), None);
        assert_eq!(span(&regex("x*"), "abc", 3, false), Some((3, 3)));
        assert_eq!(span(&regex("x*"), "abc", 4, false), None);
        assert_eq!(span(&regex("x*"), "abc", 3, true), Some((3, 3)));
    }

    fn styled(find: &str, with: &str, line: &str) -> String {
        let m = Matcher::new(&SearchOptions {
            preserve_style: true,
            replace: with.into(),
            ..opts(find)
        })
        .unwrap();
        let hit = m.find(line, 0, false).unwrap();
        let chars: Vec<char> = line.chars().collect();
        let head: String = chars[..hit.start].iter().collect();
        let tail: String = chars[hit.end..].iter().collect();
        format!("{head}{}{tail}", m.replacement(&hit))
    }

    #[test]
    fn preserve_style_matches_the_documented_examples() {
        assert_eq!(styled("tu", "to", "Tu be"), "To be");
        assert_eq!(styled("tu", "to", "not tu be"), "not to be");
        let (find, with) = ("UserName", "PersonLogin");
        assert_eq!(styled(find, with, "writerUserName"), "writerPersonLogin");
        assert_eq!(styled(find, with, "user.NAME"), "person.LOGIN");
        assert_eq!(
            styled(find, with, "DEFAULT_USER_NAME"),
            "DEFAULT_PERSON_LOGIN"
        );
        assert_eq!(
            styled(find, with, "default-User-name"),
            "default-Person-login"
        );
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("6C 6f"), Some(vec![0x6c, 0x6f]));
        assert_eq!(parse_hex("6c6f"), Some(vec![0x6c, 0x6f]));
        assert_eq!(parse_hex("6"), None);
        assert_eq!(parse_hex("zz"), None);
        assert_eq!(parse_hex(""), None);
    }
}
